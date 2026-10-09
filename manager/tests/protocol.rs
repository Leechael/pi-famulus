#![cfg(unix)]
//! Black-box integration tests for `pi-famulus`.
//!
//! Contract source: docs/design.md §3 (protocol messages, lifecycle, state
//! machine, CLI). Written against the *contract*, not the implementation:
//! each test spawns the compiled binary and speaks the wire protocol
//! (u32 BE length + UTF-8 JSON) by hand over the unix socket. `serde_json`
//! parses response ids exactly; payload assertions use substring matching
//! (whitespace-tolerant where it matters via `compact()`). libc probes the
//! daemon lifetime lock.
//!
//! Isolation: every test uses its own PI_FAMULUS_HOME under temp_dir()
//! (pi-famulus-test-<pid>-<testname>) and kills its daemon on drop.

use serde_json::Value;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pi-famulus");
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_TIMEOUT: Duration = Duration::from_secs(6);
// Mirrors the production cap; the debug hook records one byte per WouldBlock.
#[cfg(debug_assertions)]
const DAEMON_LOCK_RETRY_ATTEMPTS: usize = 100;

// ---------------------------------------------------------------------------
// test scaffolding
// ---------------------------------------------------------------------------

fn sock_path(home: &Path) -> PathBuf {
    home.join("manager.sock")
}

fn test_home(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("pi-famulus-test-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test home");
    dir
}

fn spawn_daemon(home: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .env("PI_FAMULUS_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pi-famulus daemon")
}

fn spawn_daemon_with_agent_budget_pause(
    home: &Path,
    marker: &Path,
    release_marker: &Path,
) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .env("PI_FAMULUS_HOME", home)
        .env("PI_FAMULUS_TEST_AGENT_BUDGET_PAUSE_CHILD", "ch-old")
        .env("PI_FAMULUS_TEST_AGENT_BUDGET_MARKER", marker)
        .env("PI_FAMULUS_TEST_AGENT_BUDGET_RELEASE_MARKER", release_marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pi-famulus daemon with budget pause")
}

fn spawn_daemon_with_writer_queue_marker(home: &Path, marker: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .env("PI_FAMULUS_HOME", home)
        .env("PI_FAMULUS_TEST_WRITER_QUEUE_FULL_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pi-famulus daemon with writer queue marker")
}

fn wait_for_socket(home: &Path, timeout: Duration) {
    // §3.1 step 3: clients poll for socket readiness with a 2s timeout.
    let deadline = Instant::now() + timeout;
    while !sock_path(home).exists() {
        assert!(
            Instant::now() < deadline,
            "manager.sock did not appear within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn connect(home: &Path, timeout: Duration) -> Conn {
    let sock = sock_path(home);
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(&sock) {
            Ok(stream) => {
                return Conn {
                    stream,
                    buf: Vec::new(),
                    history: Vec::new(),
                }
            }
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "connect {sock:?} failed: {e}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// RAII guard: kill the daemon and wipe PI_FAMULUS_HOME when the test ends.
/// (If the test already killed/reaped the child, the extra kill is a no-op.)
struct Daemon {
    child: Child,
    home: PathBuf,
}

impl Daemon {
    fn start(test: &str) -> Daemon {
        let home = test_home(test);
        let child = spawn_daemon(&home);
        wait_for_socket(&home, Duration::from_secs(2));
        Daemon { child, home }
    }

    fn connect(&self) -> Conn {
        connect(&self.home, CONNECT_TIMEOUT)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

// ---------------------------------------------------------------------------
// wire protocol helpers (§3.3: u32 BE length + JSON)
// ---------------------------------------------------------------------------

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
    /// Every frame ever read — lets event assertions win races against
    /// request/response interleaving (events may arrive while we await a
    /// response and would otherwise be consumed and lost).
    history: Vec<String>,
}

fn has_response_id(frame: &str, id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(frame) else {
        return false;
    };
    value.get("id").and_then(serde_json::Value::as_str) == Some(id)
}

impl Conn {
    fn send(&mut self, json: &str) {
        let body = json.as_bytes();
        let len = (body.len() as u32).to_be_bytes();
        self.stream.write_all(&len).expect("write frame header");
        self.stream.write_all(body).expect("write frame body");
        self.stream.flush().expect("flush");
    }

    /// Read the next frame arriving before `deadline`; None on timeout.
    fn read_frame(&mut self, deadline: Instant) -> Option<String> {
        loop {
            if self.buf.len() >= 4 {
                let n =
                    u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                        as usize;
                if self.buf.len() >= 4 + n {
                    let body: Vec<u8> = self.buf.drain(..4 + n).skip(4).collect();
                    let frame = String::from_utf8_lossy(&body).into_owned();
                    self.history.push(frame.clone());
                    return Some(frame);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = deadline - now;
            self.stream
                .set_read_timeout(Some(remaining.min(Duration::from_millis(100))))
                .ok();
            let mut chunk = [0u8; 16384];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("daemon closed the connection unexpectedly"),
                Ok(k) => self.buf.extend_from_slice(&chunk[..k]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("socket read error: {e}"),
            }
        }
    }

    /// Send a request carrying `id` and wait for the response echoing that id
    /// (§3.3: responses carry the request id; interleaved events are skipped
    /// but retained in `history`).
    fn request(&mut self, json: &str, id: &str) -> String {
        self.send(json);
        self.read_until(id, RESPONSE_TIMEOUT)
            .unwrap_or_else(|| panic!("no response echoing id {id:?} within {RESPONSE_TIMEOUT:?}"))
    }

    fn read_until(&mut self, id: &str, timeout: Duration) -> Option<String> {
        if let Some(frame) = self.history.iter().find(|frame| has_response_id(frame, id)) {
            return Some(frame.clone());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let frame = self.read_frame(deadline)?;
            if has_response_id(&frame, id) {
                return Some(frame);
            }
        }
    }

    /// Wait for a server-pushed event frame (§3.3: events carry no id).
    /// Matches on whitespace-normalized JSON so either compact or spaced
    /// serialization satisfies the contract.
    fn read_until_event(&mut self, event: &str, timeout: Duration) -> Option<String> {
        let needle = format!("\"event\":\"{event}\"");
        if let Some(f) = self.history.iter().find(|f| compact(f).contains(&needle)) {
            return Some(f.clone());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let frame = self.read_frame(deadline)?;
            if compact(&frame).contains(&needle) {
                return Some(frame);
            }
        }
    }
}

#[test]
fn p0_conn_read_until_matches_json_id_exactly() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut conn = Conn {
        stream,
        buf: Vec::new(),
        history: vec![r#"{"v":1,"id":"","version":"0.1.2+5ec12f60e7"}"#.to_owned()],
    };

    assert_eq!(conn.read_until("c1", Duration::ZERO), None);
}

// ---------------------------------------------------------------------------
// JSON helpers (exact id parsing, substring assertions for payloads)
// ---------------------------------------------------------------------------

/// Whitespace-stripped copy for structural assertions (`"ok":true` matches
/// both `{"ok":true}` and `{ "ok": true }`). Never use for payload content.
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Extract a string field value (tolerates `"k":"v"` and `"k": "v"`).
fn extract_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    for pat in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        if let Some(i) = json.find(&pat) {
            let rest = &json[i + pat.len()..];
            if let Some(end) = rest.find('"') {
                return Some(&rest[..end]);
            }
        }
    }
    None
}

/// Extract an unsigned integer field value.
fn extract_num(json: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let i = json.find(&pat)? + pat.len();
    let rest = json[i..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ---------------------------------------------------------------------------
// request builders (§3.3 message definitions)
// ---------------------------------------------------------------------------

/// §3.3 hello example carries no "v"/"id" — sent exactly as specified.
/// NOTE(design ambiguity): §3.3 framing says requests are `{"v":1,"id":...}`,
/// but the hello examples omit both. We follow the example for hello and the
/// framing section for everything else.
fn hello_ext(conn: &mut Conn, session_id: &str) -> String {
    conn.send(&format!(
        r#"{{"type":"hello","client_kind":"extension","session_id":"{session_id}","pi_pid":{}}}"#,
        std::process::id()
    ));
    conn.read_frame(Instant::now() + Duration::from_secs(5))
        .expect("hello response within 5s")
}

fn hello_ext_protocol(conn: &mut Conn, session_id: &str, protocol: u32) -> String {
    conn.send(&format!(
        r#"{{"type":"hello","client_kind":"extension","session_id":"{session_id}","pi_pid":{},"protocol":{protocol}}}"#,
        std::process::id()
    ));
    conn.read_frame(Instant::now() + Duration::from_secs(5))
        .expect("hello response within 5s")
}

fn start_req(id: &str, command: &str, background: bool) -> String {
    // env is the child's COMPLETE environment per §3.3 — pass a minimal one.
    // kind:"shell" is assumed to run via `sh -c` (tests use `;` compounds).
    format!(
        r#"{{"v":1,"id":"{id}","type":"start","kind":"shell","command":"{command}","cwd":"/tmp","env":{{"PATH":"/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"}},"run_in_background":{background},"timeout_ms":null}}"#
    )
}

fn wait_req(id: &str, task_id: &str, budget_ms: u64) -> String {
    format!(
        r#"{{"v":1,"id":"{id}","type":"wait","task_id":"{task_id}","budget_ms":{budget_ms}}}"#
    )
}

fn output_req(id: &str, task_id: &str, cursor: u64) -> String {
    format!(
        r#"{{"v":1,"id":"{id}","type":"output","task_id":"{task_id}","cursor":{cursor},"max_bytes":65536}}"#
    )
}

fn stop_req(id: &str, task_id: &str) -> String {
    format!(r#"{{"v":1,"id":"{id}","type":"stop","task_id":"{task_id}"}}"#)
}

fn list_req(id: &str) -> String {
    format!(r#"{{"v":1,"id":"{id}","type":"list","all":false}}"#)
}

fn watch_req(id: &str, task_id: &str) -> String {
    format!(r#"{{"v":1,"id":"{id}","type":"watch","task_id":"{task_id}"}}"#)
}

// ---------------------------------------------------------------------------
// process helpers
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
fn blocked_lock_attempts(path: &Path) -> usize {
    fs::read(path).map(|attempts| attempts.len()).unwrap_or(0)
}

/// Probe the real daemon lifetime lock without relying on pid-file contents.
#[cfg(debug_assertions)]
fn lifetime_lock_held(home: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("manager.lock"))
        .expect("open daemon lifetime lock");
    // SAFETY: a successful probe is released when `lock` drops.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        false
    } else {
        assert_eq!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::WouldBlock);
        true
    }
}

fn wait_child_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait: {e}"),
        }
    }
}

/// std-only liveness check via the system kill(1) binary (kill -0).
fn pid_alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Like `pid_alive` but a zombie (exited, not yet reaped by its parent)
/// counts as dead: after the daemon's SIGKILL the task processes re-parent
/// to init, and a non-reaping PID 1 would otherwise keep `kill -0`
/// succeeding until the deadline.
fn pid_running(pid: u64) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            !s.is_empty() && !s.starts_with('Z')
        }
        Err(_) => true,
    }
}

/// Run a CLI invocation against the daemon at `home`, capturing output.
fn run_cli(home: &Path, args: &[&str], timeout: Duration) -> (ExitStatus, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .env("PI_FAMULUS_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    match wait_child_timeout(&mut child, timeout) {
        Some(status) => {
            let out = child.wait_with_output().expect("collect cli output");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            (status, text)
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cli {args:?} timed out after {timeout:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// §3.3 hello: extension handshake returns ok/version/pid; cli kind needs no
/// session. Also pins that `pid` is the daemon's real process id.
#[test]
fn t01_hello_handshake() {
    let d = Daemon::start("hello");
    let mut c = d.connect();
    let resp = hello_ext(&mut c, "sess-hello");
    let cmp = compact(&resp);
    assert!(cmp.contains("\"ok\":true"), "hello resp: {resp}");
    assert!(cmp.contains("\"version\""), "hello resp missing version: {resp}");
    assert!(cmp.contains("\"pid\""), "hello resp missing pid: {resp}");
    let pid = extract_num(&resp, "pid").expect("numeric pid in hello response");
    assert_eq!(
        pid,
        d.child.id() as u64,
        "hello pid should be the daemon process id"
    );

    // cli-flavoured hello on a second connection (§3.3: no session fields)
    let mut cli = d.connect();
    cli.send(r#"{"type":"hello","client_kind":"cli"}"#);
    let resp = cli
        .read_frame(Instant::now() + Duration::from_secs(5))
        .expect("cli hello response");
    assert!(
        compact(&resp).contains("\"ok\":true"),
        "cli hello resp: {resp}"
    );
}

/// §3.3 start/wait/output happy path; §3.3 task_id format `<prefix>_<8hex>`.
#[test]
fn t02_start_wait_echo() {
    let d = Daemon::start("echo");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-echo");

    let resp = c.request(&start_req("r2-start", "echo hello", false), "r2-start");
    let cr = compact(&resp);
    assert!(cr.contains("\"ok\":true"), "start: {resp}");
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();
    assert!(
        task_id.starts_with("sh_"),
        "shell task_id must use sh_ prefix (§3.3): {task_id}"
    );
    extract_num(&resp, "pid").expect("pid in start response");

    let w = c.request(&wait_req("r2-wait", &task_id, 5000), "r2-wait");
    let cw = compact(&w);
    assert!(cw.contains("\"done\":true"), "wait done: {w}");
    assert!(cw.contains("\"exit_code\":0"), "wait exit_code: {w}");

    let o = c.request(&output_req("r2-out", &task_id, 0), "r2-out");
    assert!(
        o.contains("hello"),
        "output chunk should contain command output: {o}"
    );
}

/// §3.3 wait: budget expiry returns done:false and the task keeps running;
/// stop afterwards; §3.3 events: task_exited pushed to the owning session.
#[test]
fn t03_wait_budget_expires_task_keeps_running() {
    let d = Daemon::start("budget");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-budget");

    let resp = c.request(&start_req("r3-start", "sleep 5", false), "r3-start");
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();

    let w = c.request(&wait_req("r3-wait", &task_id, 200), "r3-wait");
    let cw = compact(&w);
    assert!(
        cw.contains("\"ok\":true") && cw.contains("\"done\":false"),
        "budget expiry must return done:false: {w}"
    );

    let s = c.request(&stop_req("r3-stop", &task_id), "r3-stop");
    assert!(compact(&s).contains("\"ok\":true"), "stop: {s}");

    let ev = c
        .read_until_event("task_exited", EVENT_TIMEOUT)
        .expect("task_exited event after stop");
    assert!(ev.contains(&task_id), "event carries task_id: {ev}");
}

/// §3.3: run_in_background is a semantic marker only; task_exited is always
/// pushed to the owning session's long-lived connection.
#[test]
fn t04_background_task_exited_event_pushed() {
    let d = Daemon::start("bgevent");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-bg");

    let resp = c.request(&start_req("r4-start", "echo bg-done", true), "r4-start");
    assert!(compact(&resp).contains("\"ok\":true"), "start: {resp}");
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();

    let ev = c
        .read_until_event("task_exited", EVENT_TIMEOUT)
        .expect("task_exited event within timeout");
    assert!(ev.contains(&task_id), "event carries task_id: {ev}");
}

/// §3.3 output: cursor is a byte offset; next_cursor feeds the next
/// incremental read; incremental chunks must not overlap consumed content.
#[test]
fn t05_output_cursor_incremental_read() {
    let d = Daemon::start("cursor");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-cursor");

    let resp = c.request(
        &start_req("r5-start", "echo first; sleep 2; echo second", false),
        "r5-start",
    );
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();

    // Poll cursor=0 until the first line appears (well within the 2s sleep).
    let mut first_chunk = String::new();
    let mut cursor = 0u64;
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut i = 0;
    while Instant::now() < deadline {
        let id = format!("r5-out1-{i}");
        let o = c.request(&output_req(&id, &task_id, 0), &id);
        cursor = extract_num(&o, "next_cursor").expect("next_cursor");
        first_chunk = extract_str(&o, "chunk").unwrap_or("").to_string();
        if !first_chunk.is_empty() {
            break;
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        first_chunk.contains("first"),
        "first chunk should contain 'first': {first_chunk:?}"
    );
    assert!(
        !first_chunk.contains("second"),
        "'second' must not exist before the sleep ends: {first_chunk:?}"
    );

    let w = c.request(&wait_req("r5-wait", &task_id, 5000), "r5-wait");
    assert!(compact(&w).contains("\"done\":true"), "wait: {w}");

    let o2 = c.request(&output_req("r5-out2", &task_id, cursor), "r5-out2");
    let second_chunk = extract_str(&o2, "chunk").unwrap_or("").to_string();
    assert!(
        second_chunk.contains("second"),
        "incremental chunk should contain 'second': {second_chunk:?}"
    );
    assert!(
        !second_chunk.contains("first"),
        "incremental read must not re-deliver consumed bytes: {second_chunk:?}"
    );
}

/// §3.3 list: extension connections see only their own session's tasks.
#[test]
fn t06_list_is_scoped_to_session() {
    let d = Daemon::start("isolation");
    let mut a = d.connect();
    let mut b = d.connect();
    hello_ext(&mut a, "sess-A");
    hello_ext(&mut b, "sess-B");

    let ra = a.request(&start_req("r6-start-a", "echo aaa", true), "r6-start-a");
    let task_a = extract_str(&ra, "task_id").expect("task_a").to_string();
    let rb = b.request(&start_req("r6-start-b", "echo bbb", true), "r6-start-b");
    let task_b = extract_str(&rb, "task_id").expect("task_b").to_string();

    let la = a.request(&list_req("r6-list-a"), "r6-list-a");
    assert!(la.contains(&task_a), "A sees its own task: {la}");
    assert!(!la.contains(&task_b), "A must not see B's task: {la}");

    let lb = b.request(&list_req("r6-list-b"), "r6-list-b");
    assert!(lb.contains(&task_b), "B sees its own task: {lb}");
    assert!(!lb.contains(&task_a), "B must not see A's task: {lb}");
}

/// §3.3 watch: after watch, the server pushes {"event":"output", ...} frames
/// carrying task_id/chunk/next_cursor.
#[test]
fn t07_watch_streams_output_events() {
    let d = Daemon::start("watch");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-watch");

    let resp = c.request(
        &start_req("r7-start", "echo one; sleep 1; echo two", true),
        "r7-start",
    );
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();

    let w = c.request(&watch_req("r7-watch", &task_id), "r7-watch");
    assert!(compact(&w).contains("\"ok\":true"), "watch: {w}");

    let ev = c
        .read_until_event("output", EVENT_TIMEOUT)
        .expect("output event after watch");
    assert!(ev.contains(&task_id), "output event carries task_id: {ev}");
    assert!(
        ev.contains("one") || ev.contains("two"),
        "output event carries an output chunk: {ev}"
    );
}

/// A monitor streams to the connection that started it from spawn on. With a
/// separate `watch` round trip, a fast command (`echo noop`) printed and
/// exited before the watch landed: its lines were lost and the extension
/// never saw the monitor end (manual testing, 2026-09-24).
#[test]
fn t07b_monitor_streams_from_spawn_without_watch() {
    let d = Daemon::start("monitor-autowatch");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-mon");

    let req = start_req("r7b-start", "echo early-line", true).replace(r#""kind":"shell""#, r#""kind":"monitor""#);
    let resp = c.request(&req, "r7b-start");
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();

    let ev = c
        .read_until_event("output", EVENT_TIMEOUT)
        .expect("output event without a watch request");
    assert!(ev.contains(&task_id) && ev.contains("early-line"), "first line reaches the starter: {ev}");
    let exit = c.read_until_event("task_exited", EVENT_TIMEOUT).expect("task_exited");
    assert!(exit.contains(&task_id), "exit event: {exit}");
}

/// §3.4 state machine: running --stop--> killed (terminal).
/// NOTE: whether the stop response is acked before/after the state flips is
/// unspecified — we poll list until the terminal state is observable.
#[test]
fn t08_stop_marks_task_killed() {
    let d = Daemon::start("stop");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-stop");

    let resp = c.request(&start_req("r8-start", "sleep 30", false), "r8-start");
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();

    let s = c.request(&stop_req("r8-stop", &task_id), "r8-stop");
    assert!(compact(&s).contains("\"ok\":true"), "stop: {s}");

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut i = 0;
    loop {
        let id = format!("r8-list-{i}");
        let l = c.request(&list_req(&id), &id);
        if l.contains(&task_id) && compact(&l).contains("\"status\":\"killed\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "task must reach terminal 'killed' after stop (§3.4); last list: {l}"
        );
        i += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// §3.2 lifecycle: active connections at zero for 5s → graceful shutdown:
/// running tasks get SIGTERM (group) → 2s grace → SIGKILL, then the daemon
/// exits. Asserts: self-exit within the contract window, not before the 5s
/// dwell, and the running task's pid is gone afterwards.
#[test]
fn t09_zero_connections_shutdown_kills_tasks() {
    let home = test_home("lifecycle");
    let mut child = spawn_daemon(&home);
    wait_for_socket(&home, Duration::from_secs(2));

    let mut c = connect(&home, CONNECT_TIMEOUT);
    hello_ext(&mut c, "sess-life");
    let resp = c.request(&start_req("r9-start", "sleep 30", true), "r9-start");
    let task_pid = extract_num(&resp, "pid").expect("child pid in start response");
    assert!(pid_alive(task_pid), "task should be running");

    drop(c); // last (only) connection gone
    let t = Instant::now();
    let status = wait_child_timeout(&mut child, Duration::from_secs(12)).unwrap_or_else(|| {
        panic!(
            "daemon must self-exit after connections hit zero (§3.2: 5s dwell + up to 2s kill grace)"
        )
    });
    let elapsed = t.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4),
        "daemon exited too early ({elapsed:?}); §3.2 requires a 5s zero-connection dwell"
    );
    // exit code on graceful shutdown is unspecified — recorded, not asserted.
    let _ = status;

    // The running task must have been reaped (SIGTERM group → SIGKILL).
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(task_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pid_alive(task_pid),
        "task pid {task_pid} must be killed on manager shutdown (§3.2)"
    );
    // Socket + pid files must be cleaned (§3.2 step 4) — no dangling IPC.
    assert!(
        !sock_path(&home).exists(),
        "manager.sock must be removed after graceful shutdown"
    );
    assert!(
        !home.join("manager.pid").exists(),
        "manager.pid must be removed after graceful shutdown"
    );
    let _ = fs::remove_dir_all(&home);
}

/// §3.1: a live pid in manager.pid makes a second daemon refuse to start —
/// prints "already running" and exits with code 0.
// The marker hook used to prove all retries is intentionally debug-only.
#[cfg(debug_assertions)]
#[test]
fn t10_second_daemon_refused() {
    use std::os::unix::fs::MetadataExt;

    let d = Daemon::start("singleton");
    let mut owner_conn = d.connect();
    let owner_hello = hello_ext(&mut owner_conn, "sess-singleton-owner");
    let owner_pid = extract_num(&owner_hello, "pid").expect("owner pid");
    let socket = sock_path(&d.home);
    let owner_socket_inode = fs::metadata(&socket).unwrap().ino();
    let blocked = d.home.join("blocked-lock-attempts");
    assert!(lifetime_lock_held(&d.home), "owner must hold manager.lock before refusal probe");

    let mut duplicate = Command::new(BIN)
        .arg("daemon")
        .env("PI_FAMULUS_HOME", &d.home)
        .env("PI_FAMULUS_TEST_DAEMON_LOCK_BLOCKED", &blocked)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn competing daemon");

    // Retry count is the synchronization condition; this wall deadline is
    // only a deadlock guard and does not replace the bounded-policy evidence.
    let deadline = Instant::now() + Duration::from_secs(15);
    while blocked_lock_attempts(&blocked) < DAEMON_LOCK_RETRY_ATTEMPTS && Instant::now() < deadline {
        assert!(lifetime_lock_held(&d.home), "owner released manager.lock during contention");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(blocked_lock_attempts(&blocked), DAEMON_LOCK_RETRY_ATTEMPTS,
        "contender must observe the retry cap while the real owner lock is held");
    assert!(lifetime_lock_held(&d.home), "owner must hold manager.lock at refusal");
    let remaining = deadline.saturating_duration_since(Instant::now());
    let status = wait_child_timeout(&mut duplicate, remaining)
        .expect("contending daemon did not exit after the bounded retry policy");
    let out = duplicate.wait_with_output().expect("collect refusal output");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(status.success(), "second daemon must exit 0 (§3.1), got {status:?}; output: {text}");
    assert!(text.to_lowercase().contains("already running"),
        "second daemon should print an 'already running' message, got: {text:?}");

    // Strict refusal: no new pid, endpoint inode, or service replaces owner.
    assert_eq!(extract_num(&fs::read_to_string(d.home.join("manager.pid")).unwrap(), "pid"), Some(owner_pid));
    assert_eq!(fs::metadata(&socket).unwrap().ino(), owner_socket_inode);
    assert!(lifetime_lock_held(&d.home), "refusal must leave owner's lock held");
    let mut still_owner = d.connect();
    still_owner.send(r#"{"type":"hello","client_kind":"cli"}"#);
    let current = still_owner.read_frame(Instant::now() + Duration::from_secs(5))
        .expect("owner still serves original socket");
    assert_eq!(extract_num(&current, "pid"), Some(owner_pid), "no second daemon may serve");
}

/// §3.5 CLI smoke: status/sessions/list/doctor against a running daemon.
/// NOTE: CLI output format is unspecified — we assert exit codes only
/// (plus non-empty output for `status`, and doctor's protocol verdict).
#[test]
fn t11_cli_smoke() {
    let d = Daemon::start("cli");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-cli");
    let resp = c.request(&start_req("r11-start", "echo cli-task", false), "r11-start");
    assert!(compact(&resp).contains("\"ok\":true"), "start: {resp}");

    for args in [
        &["status"][..],
        &["sessions"][..],
        &["list"][..],
        &["ls", "--json"][..],
    ] {
        let (status, text) = run_cli(&d.home, args, Duration::from_secs(5));
        assert!(
            status.success(),
            "cli {args:?} failed: {status:?}; output: {text}"
        );
    }

    let (_, out) = run_cli(&d.home, &["status"], Duration::from_secs(5));
    assert!(
        !out.trim().is_empty(),
        "status should print something about the running daemon"
    );

    // This session's hello follows the original §3.3 example, which has no
    // `protocol` field: doctor must flag it as an older extension and exit 1.
    let (status, text) = run_cli(&d.home, &["doctor"], Duration::from_secs(5));
    assert_eq!(status.code(), Some(1), "doctor: {text}");
    assert!(
        text.contains("FAIL  protocol: session sess-cli did not announce a protocol"),
        "doctor must name the session without a protocol: {text}"
    );
}

/// §3.1 step 5 + §3.2 lifeline: SIGKILL the daemon (stale socket/pid files
/// remain). Its task dies with it: the runner sees the lifeline break. A
/// restart with the same PI_FAMULUS_HOME cleans the stale files, takes over, and
/// lists the task as orphaned (end_reason manager-crash) without
/// re-adopting anything.
#[test]
fn t12_restart_after_crash_orphans_the_task() {
    let home = test_home("crash");
    let mut d1 = spawn_daemon(&home);
    wait_for_socket(&home, Duration::from_secs(2));

    let mut c1 = connect(&home, CONNECT_TIMEOUT);
    hello_ext(&mut c1, "sess-crash");
    // The command reports its own pid: the task's pid is its runner's, and
    // the command must die too, not just the runner.
    let resp = c1.request(&start_req("r12-start", "echo $$; exec sleep 30", true), "r12-start");
    let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();
    let task_pid = extract_num(&resp, "pid").expect("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut i = 0;
    let cmd_pid: u64 = loop {
        i += 1;
        let id = format!("r12-out-{i:04}"); // fixed width: no id is a prefix of another
        let out = c1.request(&output_req(&id, &task_id, 0), &id);
        let digits: String = extract_str(&out, "chunk").unwrap_or("").chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = digits.parse() {
            break p;
        }
        assert!(Instant::now() < deadline, "command did not print its pid: {out}");
        std::thread::sleep(Duration::from_millis(50));
    };
    drop(c1);

    d1.kill().expect("SIGKILL daemon");
    d1.wait().expect("reap daemon");
    let deadline = Instant::now() + Duration::from_secs(4);
    while pid_running(task_pid) || pid_running(cmd_pid) {
        assert!(Instant::now() < deadline, "task (runner {task_pid}, command {cmd_pid}) survived the daemon's SIGKILL");
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut d2 = spawn_daemon(&home);
    // Stale socket file still exists; retry connect while the new daemon
    // clears it and rebinds (§3.1 zombie-socket path).
    let mut c2 = connect(&home, Duration::from_secs(5));
    hello_ext(&mut c2, "sess-crash");
    let last = c2.request(&list_req("r12-list"), "r12-list");
    assert!(last.contains(&task_id), "restarted daemon lists the task: {last}");
    let flat = compact(&last);
    assert!(
        flat.contains("\"status\":\"orphaned\"") && flat.contains("\"end_reason\":\"manager-crash\""),
        "crashed task is orphaned, not re-adopted: {last}"
    );

    drop(c2);
    let _ = d2.kill();
    let _ = d2.wait();
    let _ = fs::remove_dir_all(&home);
}

/// Memory/lifecycle stress: N short-lived start→wait cycles must not panic
/// the daemon. Uses `wait` (not list polling) so Conn history substring
/// matching cannot confuse ids like `r13-list-1` vs `r13-list-10`.
#[test]
fn p5_machine_agent_admission_rejects_releases_and_reaps_disconnects() {
    let home = test_home("p5-agent-capacity");
    fs::write(home.join("config.json"), r#"{"maxAgents":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut a = connect(&home, CONNECT_TIMEOUT);
    let mut b = connect(&home, CONNECT_TIMEOUT);
    let mut c = connect(&home, CONNECT_TIMEOUT);
    hello_ext(&mut a, "session-a");
    hello_ext(&mut b, "session-b");
    hello_ext(&mut c, "session-c");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let ba = barrier.clone();
    let ta = std::thread::spawn(move || {
        ba.wait();
        let response = a.request(r#"{"id":"a1","type":"acquire_agent","child_id":"ch_shared"}"#, "a1");
        (response, a)
    });
    let bb = barrier.clone();
    let tb = std::thread::spawn(move || {
        bb.wait();
        let response = b.request(r#"{"id":"b1","type":"acquire_agent","child_id":"ch_shared"}"#, "b1");
        (response, b)
    });
    barrier.wait();
    let (ra, mut a) = ta.join().unwrap();
    let (rb, mut b) = tb.join().unwrap();
    assert_ne!(compact(&ra).contains("\"granted\":true"), compact(&rb).contains("\"granted\":true"), "one machine slot admits exactly one session");
    let (winner_a, loser_b) = (compact(&ra).contains("\"granted\":true"), compact(&rb).contains("\"granted\":true"));
    assert!(winner_a || loser_b);
    if winner_a {
        assert!(compact(&rb).contains("\"rejection\":\"global_capacity\""), "{rb}");
    } else {
        assert!(compact(&ra).contains("\"rejection\":\"global_capacity\""), "{ra}");
    }
    fs::write(home.join("config.json"), r#"{"maxAgents":2}"#).unwrap();
    let admitted = if winner_a {
        b.request(
            r#"{"id":"p5-scale-up-b","type":"acquire_agent","child_id":"ch_shared"}"#,
            "p5-scale-up-b",
        )
    } else {
        a.request(r#"{"id":"a2","type":"acquire_agent","child_id":"ch_shared"}"#, "a2")
    };
    assert!(compact(&admitted).contains("\"granted\":true"), "scale up did not admit: {admitted}");
    if winner_a {
        a.request(r#"{"id":"release1","type":"release_agent","child_id":"ch_shared"}"#, "release1");
        a.request(r#"{"id":"release2","type":"release_agent","child_id":"ch_shared"}"#, "release2");
    } else {
        b.request(r#"{"id":"release1","type":"release_agent","child_id":"ch_shared"}"#, "release1");
        b.request(r#"{"id":"release2","type":"release_agent","child_id":"ch_shared"}"#, "release2");
    }
    let after_release = c.request(r#"{"id":"c1","type":"acquire_agent","child_id":"ch_c"}"#, "c1");
    assert!(compact(&after_release).contains("\"granted\":true"), "winner release did not free a permit: {after_release}");
    c.request(r#"{"id":"c2","type":"release_agent","child_id":"ch_c"}"#, "c2");

    fs::write(home.join("config.json"), r#"{"maxAgents":1}"#).unwrap();
    let denied = c.request(r#"{"id":"c3","type":"acquire_agent","child_id":"ch_c"}"#, "c3");
    assert!(compact(&denied).contains("\"rejection\":\"global_capacity\""), "scale down did not limit new admissions: {denied}");
    if winner_a { drop(b); } else { drop(a); }
    std::thread::sleep(Duration::from_millis(50));
    let granted = c.request(r#"{"id":"c4","type":"acquire_agent","child_id":"ch_c"}"#, "c4");
    assert!(compact(&granted).contains("\"granted\":true"), "stale session permit was not reaped: {granted}");
    c.request(r#"{"id":"c5","type":"release_agent","child_id":"ch_c"}"#, "c5");
}

#[test]
fn p7_stale_capacity_read_cannot_overtake_new_admission() {
    let home = test_home("p7");
    fs::write(home.join("config.json"), r#"{"maxAgents":3,"maxTest":3}"#).unwrap();
    let marker = home.join("old-budget-read");
    let release_marker = home.join("release-old-budget-read");
    let mut daemon = spawn_daemon_with_agent_budget_pause(&home, &marker, &release_marker);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut existing = connect(&home, CONNECT_TIMEOUT);
    let mut old = connect(&home, CONNECT_TIMEOUT);
    let mut fresh = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut existing, "budget-existing", 5);
    hello_ext_protocol(&mut old, "budget-old", 5);
    hello_ext_protocol(&mut fresh, "budget-fresh", 5);
    let held = existing.request(
        r#"{"id":"held","type":"acquire_agent","child_id":"ch-held","work_kind":"test"}"#,
        "held",
    );
    assert!(compact(&held).contains("\"granted\":true"), "{held}");

    let old_request = std::thread::spawn(move || {
        let response = old.request(
            r#"{"id":"old","type":"acquire_agent","child_id":"ch-old","work_kind":"test"}"#,
            "old",
        );
        (response, old)
    });
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "acquire did not reach its old-budget read"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // `ch-old` has cached maxAgents=3 but is paused before its state admission.
    // A newer request sees maxAgents=2 and must not be overtaken when the older
    // request resumes with its stale budget.
    fs::write(home.join("config.json"), r#"{"maxAgents":2,"maxTest":3}"#).unwrap();
    let refreshed = existing.request(r#"{"id":"refresh","type":"status"}"#, "refresh");
    let refreshed: Value = serde_json::from_str(&refreshed).unwrap();
    assert_eq!(refreshed["agent_capacity"]["used"], 1, "{refreshed}");
    assert_eq!(refreshed["agent_capacity"]["total"], 2, "{refreshed}");
    assert_eq!(refreshed["agent_capacity"]["by_kind"]["test"]["used"], 1, "{refreshed}");
    assert_eq!(refreshed["agent_capacity"]["by_kind"]["test"]["total"], 3, "{refreshed}");
    fs::write(&release_marker, b"release").unwrap();
    let fresh_response = fresh.request(
        r#"{"id":"fresh","type":"acquire_agent","child_id":"ch-fresh","work_kind":"test"}"#,
        "fresh",
    );
    let (old_response, _old) = old_request.join().unwrap();
    let fresh_granted = compact(&fresh_response).contains("\"granted\":true");
    let old_granted = compact(&old_response).contains("\"granted\":true");
    assert_ne!(
        fresh_granted, old_granted,
        "stale admission overtook the refreshed budget: {fresh_response}; {old_response}"
    );

    let status = existing.request(r#"{"id":"p7status","type":"status"}"#, "p7status");
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["agent_capacity"]["used"], 2, "{status}");
    assert_eq!(status["agent_capacity"]["total"], 2, "{status}");
    assert_eq!(status["agent_capacity"]["by_kind"]["test"]["used"], 2, "{status}");
    assert_eq!(status["agent_capacity"]["by_kind"]["test"]["total"], 3, "{status}");
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn p6_wrong_shape_capacity_config_refuses_reads_updates_and_admission() {
    let home = test_home("p6-config-shapes");
    fs::write(home.join("config.json"), r#"{"maxAgents":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut c = connect(&home, CONNECT_TIMEOUT);
    hello_ext(&mut c, "session-config-shapes");
    for (index, corrupt) in ["[]", "null", "42"].into_iter().enumerate() {
        fs::write(home.join("config.json"), corrupt).unwrap();
        let id = format!("bad{index}");
        let request = format!(r#"{{"id":"{id}","type":"acquire_agent","child_id":"ch_bad{index}"}}"#);
        let response = c.request(&request, &id);
        assert!(compact(&response).contains("\"ok\":false"), "admitted with {corrupt}: {response}");
        assert!(response.contains("JSON object"), "missing loud config error: {response}");
        let get = Command::new(BIN).arg("--home").arg(&home).args(["config", "get", "max-agents"]).output().unwrap();
        assert!(!get.status.success(), "config get accepted {corrupt}");
        let set = Command::new(BIN).arg("--home").arg(&home).args(["config", "set", "max-agents", "12"]).output().unwrap();
        assert!(!set.status.success(), "config set accepted {corrupt}");
        assert_eq!(fs::read_to_string(home.join("config.json")).unwrap(), corrupt);
    }
}

#[test]
fn p7_per_kind_budget_independence_and_queued_wakeup() {
    let home = test_home("p7-workkind-wakeup");
    fs::write(
        home.join("config.json"),
        r#"{"maxAgents":4,"maxTest":1,"maxBuild":1,"maxTestSuite":1}"#,
    )
    .unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut holder = connect(&home, CONNECT_TIMEOUT);
    let mut waiting = connect(&home, CONNECT_TIMEOUT);
    let mut builder = connect(&home, CONNECT_TIMEOUT);
    let mut suite = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut holder, "session-p7-holder", 5);
    hello_ext_protocol(&mut waiting, "session-p7-waiting", 5);
    hello_ext_protocol(&mut builder, "session-p7-builder", 5);
    hello_ext_protocol(&mut suite, "session-p7-suite", 5);

    let granted = holder.request(
        r#"{"id":"p7-test-a","type":"acquire_agent","child_id":"ch_test_a","work_kind":"test"}"#,
        "p7-test-a",
    );
    assert!(compact(&granted).contains("\"granted\":true"), "{granted}");

    waiting.send(
        r#"{"id":"p7-test-b","type":"acquire_agent","child_id":"ch_test_b","work_kind":"test"}"#,
    );
    assert!(waiting.read_until("p7-test-b", Duration::from_millis(150)).is_none());

    let build = builder.request(
        r#"{"id":"p7-build","type":"acquire_agent","child_id":"ch_build","work_kind":"build"}"#,
        "p7-build",
    );
    assert!(compact(&build).contains("\"granted\":true"), "different work kind blocked: {build}");
    let suite_grant = suite.request(
        r#"{"id":"p7-suite","type":"acquire_agent","child_id":"ch_suite","work_kind":"test-suite"}"#,
        "p7-suite",
    );
    assert!(compact(&suite_grant).contains("\"granted\":true"), "test-suite has an independent budget: {suite_grant}");

    holder.request(
        r#"{"id":"p7-release-test","type":"release_agent","child_id":"ch_test_a"}"#,
        "p7-release-test",
    );
    let awakened = waiting
        .read_until("p7-test-b", Duration::from_secs(3))
        .expect("FIFO waiter wakes when a same-kind permit is released");
    assert!(compact(&awakened).contains("\"granted\":true"), "{awakened}");

    let status = builder.request(r#"{"id":"p7-status","type":"status"}"#, "p7-status");
    let flat = compact(&status);
    assert!(flat.contains("\"test\":{\"used\":1,\"total\":1}"), "per-kind status missing: {status}");
    assert!(flat.contains("\"build\":{\"used\":1,\"total\":1}"), "independent status missing: {status}");
    let cli_status = Command::new(BIN)
        .args(["--home", home.to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert!(cli_status.status.success());
    let cli_status = String::from_utf8_lossy(&cli_status.stdout);
    assert!(cli_status.contains("agent slots: 3/4 used"), "{cli_status}");
    assert!(cli_status.contains("test: 1/1 used"), "{cli_status}");
    assert!(cli_status.contains("build: 1/1 used"), "{cli_status}");

    waiting.request(
        r#"{"id":"p7-release-test-b","type":"release_agent","child_id":"ch_test_b"}"#,
        "p7-release-test-b",
    );
    builder.request(
        r#"{"id":"p7-release-build","type":"release_agent","child_id":"ch_build"}"#,
        "p7-release-build",
    );
    suite.request(
        r#"{"id":"p7-release-suite","type":"release_agent","child_id":"ch_suite"}"#,
        "p7-release-suite",
    );
}

#[test]
fn p7b_status_escapes_unknown_work_kind_control_characters() {
    let home = test_home("p7b-escape");
    fs::write(home.join("config.json"), r#"{"maxAgents":4}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut client = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut client, "session-p7b", 5);

    let kind = "vendor\u{1b}[31m\nforged: 9000/9000 used";
    let request = format!(
        r#"{{"id":"p7b-acquire","type":"acquire_agent","child_id":"ch_p7b","work_kind":{}}}"#,
        serde_json::to_string(kind).unwrap(),
    );
    let response = client.request(&request, "p7b-acquire");
    assert!(
        compact(&response).contains("\"granted\":true"),
        "{response}"
    );

    let status = Command::new(BIN)
        .args(["--home", home.to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    let escaped: String = kind.chars().flat_map(char::escape_default).collect();
    assert!(
        stdout.contains(&format!("  {escaped}: 1/4 used")),
        "{stdout}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "raw terminal escape in status: {stdout:?}"
    );
    assert!(
        !stdout.contains("\nforged:"),
        "forged terminal line in status: {stdout:?}"
    );
}

#[test]
fn p8_shrinking_kind_budget_keeps_queued_fifo_and_existing_permits() {
    let home = test_home("p8-workkind-shrink");
    fs::write(home.join("config.json"), r#"{"maxAgents":4,"maxTest":2}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut first = connect(&home, CONNECT_TIMEOUT);
    let mut second = connect(&home, CONNECT_TIMEOUT);
    let mut queued_first = connect(&home, CONNECT_TIMEOUT);
    let mut queued_second = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut first, "session-p8-first", 5);
    hello_ext_protocol(&mut second, "session-p8-second", 5);
    hello_ext_protocol(&mut queued_first, "session-p8-q1", 5);
    hello_ext_protocol(&mut queued_second, "session-p8-q2", 5);
    for (conn, id, child) in [(&mut first, "p8-a", "ch_a"), (&mut second, "p8-b", "ch_b")] {
        let response = conn.request(
            &format!(r#"{{"id":"{id}","type":"acquire_agent","child_id":"{child}","work_kind":"test"}}"#),
            id,
        );
        assert!(compact(&response).contains("\"granted\":true"), "{response}");
    }
    queued_first.send(
        r#"{"id":"p8-q1","type":"acquire_agent","child_id":"ch_q1","work_kind":"test"}"#,
    );
    assert!(queued_first.read_until("p8-q1", Duration::from_millis(100)).is_none());
    queued_second.send(
        r#"{"id":"p8-q2","type":"acquire_agent","child_id":"ch_q2","work_kind":"test"}"#,
    );
    assert!(queued_second.read_until("p8-q2", Duration::from_millis(100)).is_none());

    let set = Command::new(BIN)
        .args(["--home", home.to_str().unwrap(), "config", "set", "max-test", "1"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
    let capacity_events = fs::read_to_string(home.join("events.jsonl")).unwrap();
    assert!(capacity_events.contains(r#""type":"capacity.changed""#), "capacity.changed event missing: {capacity_events}");
    assert!(capacity_events.contains(r#""budget":"max-test""#), "capacity budget missing: {capacity_events}");
    assert!(capacity_events.contains(r#""previous":2,"#), "previous budget missing: {capacity_events}");
    assert!(capacity_events.contains(r#""total":1,"#), "new budget missing: {capacity_events}");
    let status = first.request(r#"{"id":"p8-status","type":"status"}"#, "p8-status");
    assert!(compact(&status).contains("\"test\":{\"used\":2,\"total\":1}"), "shrink revoked a granted permit: {status}");

    first.request(
        r#"{"id":"p8-release-a","type":"release_agent","child_id":"ch_a"}"#,
        "p8-release-a",
    );
    assert!(queued_first.read_until("p8-q1", Duration::from_millis(150)).is_none());
    assert!(queued_second.read_until("p8-q2", Duration::from_millis(100)).is_none());
    second.request(
        r#"{"id":"p8-release-b","type":"release_agent","child_id":"ch_b"}"#,
        "p8-release-b",
    );
    let first_grant = queued_first
        .read_until("p8-q1", Duration::from_secs(3))
        .expect("oldest FIFO request granted first");
    assert!(compact(&first_grant).contains("\"granted\":true"), "{first_grant}");
    assert!(queued_second.read_until("p8-q2", Duration::from_millis(150)).is_none());
    queued_first.request(
        r#"{"id":"p8-release-q1","type":"release_agent","child_id":"ch_q1"}"#,
        "p8-release-q1",
    );
    let second_grant = queued_second
        .read_until("p8-q2", Duration::from_secs(3))
        .expect("second FIFO request advances after first release");
    assert!(compact(&second_grant).contains("\"granted\":true"), "{second_grant}");
}

#[test]
fn p9_protocol_v4_uses_global_only_immediate_admission() {
    let home = test_home("p9-v4-admission");
    fs::write(home.join("config.json"), r#"{"maxAgents":2,"maxTest":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut first = connect(&home, CONNECT_TIMEOUT);
    let mut second = connect(&home, CONNECT_TIMEOUT);
    let mut third = connect(&home, CONNECT_TIMEOUT);
    let hello = hello_ext_protocol(&mut first, "session-p9-first", 4);
    assert!(compact(&hello).contains("\"protocol\":5"), "daemon advertises its max: {hello}");
    hello_ext_protocol(&mut second, "session-p9-second", 4);
    hello_ext_protocol(&mut third, "session-p9-third", 4);
    for (conn, id, child) in [(&mut first, "p9-a", "ch_a"), (&mut second, "p9-b", "ch_b")] {
        let response = conn.request(
            &format!(r#"{{"id":"{id}","type":"acquire_agent","child_id":"{child}","work_kind":"test"}}"#),
            id,
        );
        assert!(compact(&response).contains("\"granted\":true"), "v4 must ignore per-kind cap: {response}");
    }
    let denied = third.request(
        r#"{"id":"p9-c","type":"acquire_agent","child_id":"ch_c","work_kind":"test"}"#,
        "p9-c",
    );
    assert!(compact(&denied).contains("\"rejection\":\"global_capacity\""), "{denied}");
}

#[test]
fn p17_status_survives_malformed_kind_budget() {
    let home = test_home("p17-bad-kind");
    fs::write(home.join("config.json"), r#"{"maxAgents":2,"maxTest":"2"}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut client = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut client, "session-p17-status", 5);

    let response = client.request(r#"{"id":"p17-status","type":"status"}"#, "p17-status");
    let value: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["agent_capacity"]["total"], 2, "{value}");
    assert!(value["agent_capacity"]["by_kind"]["test"].is_null(), "{value}");
    assert_eq!(value["agent_capacity"]["by_kind"]["build"]["total"], 2, "{value}");
}

#[test]
fn p11_capacity_increase_notifies_daemon_and_wakes_kind_queue() {
    let home = test_home("p11-budget-wake");
    fs::write(home.join("config.json"), r#"{"maxAgents":4,"maxTest":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut holder = connect(&home, CONNECT_TIMEOUT);
    let mut waiting = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut holder, "session-p11-holder", 5);
    hello_ext_protocol(&mut waiting, "session-p11-waiting", 5);
    holder.request(
        r#"{"id":"p11-hold","type":"acquire_agent","child_id":"ch_hold","work_kind":"test"}"#,
        "p11-hold",
    );
    waiting.send(
        r#"{"id":"p11-wait","type":"acquire_agent","child_id":"ch_wait","work_kind":"test"}"#,
    );
    assert!(waiting.read_until("p11-wait", Duration::from_millis(100)).is_none());

    let set = Command::new(BIN)
        .args(["--home", home.to_str().unwrap(), "config", "set", "max-test", "2"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
    let wake = waiting
        .read_until("p11-wait", Duration::from_secs(3))
        .expect("config set notifies daemon and grants newly eligible waiter");
    assert!(compact(&wake).contains("\"granted\":true"), "{wake}");
    let status = holder.request(r#"{"id":"p11-status","type":"status"}"#, "p11-status");
    assert!(compact(&status).contains("\"test\":{\"used\":2,\"total\":2}"), "{status}");

    waiting.request(
        r#"{"id":"p11-release-wait","type":"release_agent","child_id":"ch_wait"}"#,
        "p11-release-wait",
    );
    holder.request(
        r#"{"id":"p11-release-hold","type":"release_agent","child_id":"ch_hold"}"#,
        "p11-release-hold",
    );
}

#[test]
fn p10_cancel_and_disconnect_remove_queued_acquires() {
    let home = test_home("p10-cancel-queue");
    fs::write(home.join("config.json"), r#"{"maxAgents":3,"maxTest":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut holder = connect(&home, CONNECT_TIMEOUT);
    let mut cancelled = connect(&home, CONNECT_TIMEOUT);
    let mut disconnected = connect(&home, CONNECT_TIMEOUT);
    let mut contender = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut holder, "session-p10-holder", 5);
    hello_ext_protocol(&mut cancelled, "session-p10-cancel", 5);
    hello_ext_protocol(&mut disconnected, "session-p10-drop", 5);
    hello_ext_protocol(&mut contender, "session-p10-contender", 5);
    holder.request(
        r#"{"id":"p10-hold","type":"acquire_agent","child_id":"ch_hold","work_kind":"test"}"#,
        "p10-hold",
    );
    cancelled.send(
        r#"{"id":"p10-cancelled-acquire","type":"acquire_agent","child_id":"ch_cancel","work_kind":"test"}"#,
    );
    disconnected.send(
        r#"{"id":"p10-disconnected-acquire","type":"acquire_agent","child_id":"ch_drop","work_kind":"test"}"#,
    );
    assert!(cancelled.read_until("p10-cancelled-acquire", Duration::from_millis(100)).is_none());
    assert!(disconnected.read_until("p10-disconnected-acquire", Duration::from_millis(100)).is_none());
    let cancel_ack = cancelled.request(
        r#"{"id":"p10-cancel","type":"cancel_acquire_agent","request_id":"p10-cancelled-acquire","child_id":"ch_cancel"}"#,
        "p10-cancel",
    );
    assert!(compact(&cancel_ack).contains("\"ok\":true"), "{cancel_ack}");
    drop(disconnected);
    let disconnect_deadline = Instant::now() + RESPONSE_TIMEOUT;
    let mut attempt = 0;
    loop {
        let id = format!("p10-disconnect-status-{attempt}");
        let status = contender.request(&format!(r#"{{"id":"{id}","type":"status"}}"#), &id);
        let value: Value = serde_json::from_str(&status).unwrap();
        let observed = value["sessions"].as_array().is_some_and(|sessions| {
            sessions.iter().any(|session| {
                session["session_id"] == "session-p10-drop" && session["connected"] == false
            })
        });
        if observed {
            break;
        }
        assert!(Instant::now() < disconnect_deadline, "daemon did not observe disconnected session");
        attempt += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    holder.request(
        r#"{"id":"p10-release","type":"release_agent","child_id":"ch_hold"}"#,
        "p10-release",
    );
    let result = contender.request(
        r#"{"id":"p10-contender","type":"acquire_agent","child_id":"ch_contender","work_kind":"test"}"#,
        "p10-contender",
    );
    assert!(compact(&result).contains("\"granted\":true"), "stale pending request held the kind slot: {result}");
}

#[test]
fn p16_queued_grant_survives_writer_backpressure_and_session_activity() {
    let home = test_home("p16-grant-retry");
    fs::write(home.join("config.json"), r#"{"maxAgents":3,"maxTest":1}"#).unwrap();
    let marker = home.join("p16-writer-queue-full");
    let child = spawn_daemon_with_writer_queue_marker(&home, &marker);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let _daemon = Daemon { child, home: home.clone() };
    let mut holder = connect(&home, CONNECT_TIMEOUT);
    let mut waiter = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut holder, "session-p16-holder", 5);
    hello_ext_protocol(&mut waiter, "session-p16-waiter", 5);
    holder.request(
        r#"{"id":"p16-hold","type":"acquire_agent","child_id":"ch_hold","work_kind":"test"}"#,
        "p16-hold",
    );
    let acquire = r#"{"id":"p16-wait","type":"acquire_agent","child_id":"ch_wait","work_kind":"test"}"#;
    waiter.send(acquire);
    assert!(waiter.read_until("p16-wait", Duration::from_millis(100)).is_none());
    for n in 0..2048 {
        waiter.send(&format!(r#"{{"id":"p16-status-{n:04}","type":"status"}}"#));
    }
    let queue_deadline = Instant::now() + RESPONSE_TIMEOUT;
    while !marker.exists() {
        assert!(
            Instant::now() < queue_deadline,
            "writer response queue never reached capacity"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    holder.request(
        r#"{"id":"p16-release","type":"release_agent","child_id":"ch_hold"}"#,
        "p16-release",
    );

    // The marker observes transient fullness, not a barrier at release.
    // A pipelined status request may re-serve the grant while we drain.
    // Exact full-channel retention and same-id retry are covered by the
    // deterministic dispatch tests, independent of socket-buffer scheduling.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut status_frames = waiter
        .history
        .iter()
        .filter(|frame| frame.contains("p16-status-"))
        .count();
    while status_frames < 2048 {
        let frame = waiter.read_frame((Instant::now() + Duration::from_millis(100)).min(deadline));
        let Some(frame) = frame else {
            assert!(Instant::now() < deadline, "flooded status responses did not drain");
            continue;
        };
        let value: Value = serde_json::from_str(&frame).unwrap();
        if value["id"] == "p16-wait" {
            assert_eq!(value["granted"], true, "{value}");
        }
        if value["id"].as_str().is_some_and(|id| id.starts_with("p16-status-")) {
            status_frames += 1;
        }
    }
    // If no flooded request re-served it, this ordinary request must do so.
    // request() retains interleaved grant frames in history.
    let status = waiter.request(
        r#"{"id":"p16-status-after-drain","type":"status"}"#,
        "p16-status-after-drain",
    );
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["agent_capacity"]["by_kind"]["test"]["used"], 1, "{status}");
    let quiet_deadline = Instant::now() + Duration::from_secs(2);
    while waiter.read_frame(quiet_deadline).is_some() {}
    let grants: Vec<Value> = waiter
        .history
        .iter()
        .map(|frame| serde_json::from_str::<Value>(frame).unwrap())
        .filter(|value| value["id"] == "p16-wait")
        .collect();
    assert_eq!(grants.len(), 1, "grant was lost or delivered more than once: {grants:?}");
    assert_eq!(grants[0]["granted"], true, "{:?}", grants[0]);
}

#[test]
fn p12_wake_scans_for_eligible_kind_and_keeps_same_kind_fifo() {
    let home = test_home("p12-kind-scan");
    fs::write(home.join("config.json"), r#"{"maxAgents":4,"maxTest":1,"maxBuild":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut test_holder = connect(&home, CONNECT_TIMEOUT);
    let mut build_holder = connect(&home, CONNECT_TIMEOUT);
    let mut test_waiter = connect(&home, CONNECT_TIMEOUT);
    let mut build_waiter = connect(&home, CONNECT_TIMEOUT);
    let mut build_fresh = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut test_holder, "session-p12-th", 5);
    hello_ext_protocol(&mut build_holder, "session-p12-bh", 5);
    hello_ext_protocol(&mut test_waiter, "session-p12-tw", 5);
    hello_ext_protocol(&mut build_waiter, "session-p12-bw", 5);
    hello_ext_protocol(&mut build_fresh, "session-p12-bf", 5);
    test_holder.request(
        r#"{"id":"p12-hold-test","type":"acquire_agent","child_id":"ch_th","work_kind":"test"}"#,
        "p12-hold-test",
    );
    build_holder.request(
        r#"{"id":"p12-hold-build","type":"acquire_agent","child_id":"ch_bh","work_kind":"build"}"#,
        "p12-hold-build",
    );
    test_waiter.send(
        r#"{"id":"p12-test-wait","type":"acquire_agent","child_id":"ch_tw","work_kind":"test"}"#,
    );
    assert!(test_waiter.read_until("p12-test-wait", Duration::from_millis(100)).is_none());
    test_waiter.send(
        r#"{"id":"p12-test-alias","type":"acquire_agent","child_id":"ch_tw","work_kind":"test"}"#,
    );
    assert!(test_waiter.read_until("p12-test-alias", Duration::from_millis(100)).is_none());
    build_waiter.send(
        r#"{"id":"p12-build-wait","type":"acquire_agent","child_id":"ch_bw","work_kind":"build"}"#,
    );
    assert!(build_waiter.read_until("p12-build-wait", Duration::from_millis(100)).is_none());
    build_fresh.send(
        r#"{"id":"p12-build-fresh","type":"acquire_agent","child_id":"ch_bf","work_kind":"build"}"#,
    );
    assert!(build_fresh.read_until("p12-build-fresh", Duration::from_millis(100)).is_none());

    build_holder.request(
        r#"{"id":"p12-release-build","type":"release_agent","child_id":"ch_bh"}"#,
        "p12-release-build",
    );
    let build_grant = build_waiter.read_until("p12-build-wait", Duration::from_secs(3)).unwrap();
    assert!(compact(&build_grant).contains("\"granted\":true"), "{build_grant}");
    assert!(build_fresh.read_until("p12-build-fresh", Duration::from_millis(100)).is_none());
    assert!(test_waiter.read_until("p12-test-wait", Duration::from_millis(100)).is_none());

    build_waiter.request(
        r#"{"id":"p12-release-build-wait","type":"release_agent","child_id":"ch_bw"}"#,
        "p12-release-build-wait",
    );
    let fresh_grant = build_fresh.read_until("p12-build-fresh", Duration::from_secs(3)).unwrap();
    assert!(compact(&fresh_grant).contains("\"granted\":true"), "{fresh_grant}");
    test_holder.request(
        r#"{"id":"p12-release-test","type":"release_agent","child_id":"ch_th"}"#,
        "p12-release-test",
    );
    for id in ["p12-test-wait", "p12-test-alias"] {
        let grant = test_waiter.read_until(id, Duration::from_secs(3)).unwrap();
        assert!(compact(&grant).contains("\"granted\":true"), "{grant}");
    }
}

#[test]
fn p13_pending_queue_enforces_per_session_cap_and_child_deduplication() {
    let home = test_home("p13-sess-cap");
    fs::write(home.join("config.json"), r#"{"maxAgents":1000,"maxTest":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut holder = connect(&home, CONNECT_TIMEOUT);
    let mut waiter = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut holder, "session-p13-holder", 5);
    hello_ext_protocol(&mut waiter, "session-p13-waiter", 5);
    holder.request(
        r#"{"id":"p13-hold","type":"acquire_agent","child_id":"ch_hold","work_kind":"test"}"#,
        "p13-hold",
    );
    for n in 0..64 {
        waiter.send(&format!(
            r#"{{"id":"p13-q{n:02}","type":"acquire_agent","child_id":"ch_{n:02}","work_kind":"test"}}"#
        ));
    }
    std::thread::sleep(Duration::from_millis(200));
    let overflow = waiter.request(
        r#"{"id":"p13-overflow","type":"acquire_agent","child_id":"ch_overflow","work_kind":"test"}"#,
        "p13-overflow",
    );
    assert!(compact(&overflow).contains("\"rejection\":\"capacity_queue_full\""), "{overflow}");

    // A distinct request id for an already queued child aliases the same
    // bounded waiter and receives its grant under both ids.
    waiter.send(
        r#"{"id":"p13-alias","type":"acquire_agent","child_id":"ch_00","work_kind":"test"}"#,
    );
    assert!(waiter.read_until("p13-alias", Duration::from_millis(100)).is_none());
    let mut alias_ids = vec!["p13-q00".to_string(), "p13-alias".to_string()];
    for n in 1..7 {
        let id = format!("p13-alias-{n}");
        waiter.send(&format!(
            r#"{{"id":"{id}","type":"acquire_agent","child_id":"ch_00","work_kind":"test"}}"#
        ));
        alias_ids.push(id);
    }
    let alias_overflow = waiter.request(
        r#"{"id":"p13-alias-overflow","type":"acquire_agent","child_id":"ch_00","work_kind":"test"}"#,
        "p13-alias-overflow",
    );
    assert!(
        compact(&alias_overflow).contains("\"rejection\":\"capacity_queue_full\""),
        "{alias_overflow}"
    );
    holder.request(
        r#"{"id":"p13-release","type":"release_agent","child_id":"ch_hold"}"#,
        "p13-release",
    );
    for id in alias_ids {
        let grant = waiter.read_until(&id, Duration::from_secs(3)).unwrap_or_else(|| {
            panic!("missing grant for {id}; received frames: {:?}", waiter.history)
        });
        assert!(compact(&grant).contains("\"granted\":true"), "{grant}");
    }
}

#[test]
fn p14_pending_queue_enforces_daemon_global_cap() {
    let home = test_home("p14-glob-cap");
    fs::write(home.join("config.json"), r#"{"maxAgents":1000,"maxTest":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut holder = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut holder, "session-p14-holder", 5);
    holder.request(
        r#"{"id":"p14-hold","type":"acquire_agent","child_id":"ch_hold","work_kind":"test"}"#,
        "p14-hold",
    );
    let mut sessions = Vec::new();
    for session in 0..5 {
        let mut conn = connect(&home, CONNECT_TIMEOUT);
        hello_ext_protocol(&mut conn, &format!("session-p14-{session}"), 5);
        sessions.push(conn);
    }
    for (session, conn) in sessions.iter_mut().take(4).enumerate() {
        for n in 0..64 {
            conn.send(&format!(
                r#"{{"id":"p14-{session}-{n:02}","type":"acquire_agent","child_id":"ch_{session}_{n:02}","work_kind":"test"}}"#
            ));
        }
        // This connection processes acquires in wire order; its response is
        // a barrier proving all 64 children have reached the daemon queue.
        conn.request(
            &format!(r#"{{"id":"p14-barrier-{session}","type":"status"}}"#),
            &format!("p14-barrier-{session}"),
        );
    }
    let overflow = sessions[4].request(
        r#"{"id":"p14-overflow","type":"acquire_agent","child_id":"ch_overflow","work_kind":"test"}"#,
        "p14-overflow",
    );
    assert!(compact(&overflow).contains("\"rejection\":\"capacity_queue_full\""), "{overflow}");
}

#[test]
fn p15_cancel_only_releases_its_owned_permit_and_same_id_retry_is_new() {
    let home = test_home("p15-cancel-own");
    fs::write(home.join("config.json"), r#"{"maxAgents":3,"maxTest":1}"#).unwrap();
    let _daemon = spawn_daemon(&home);
    wait_for_socket(&home, CONNECT_TIMEOUT);
    let mut conn = connect(&home, CONNECT_TIMEOUT);
    hello_ext_protocol(&mut conn, "session-p15", 5);
    let first = conn.request(
        r#"{"id":"p15-old","type":"acquire_agent","child_id":"ch_shared","work_kind":"test"}"#,
        "p15-old",
    );
    assert!(compact(&first).contains("\"granted\":true"), "{first}");
    conn.request(
        r#"{"id":"p15-cancel-old","type":"cancel_acquire_agent","request_id":"p15-old","child_id":"ch_shared"}"#,
        "p15-cancel-old",
    );
    let second = conn.request(
        r#"{"id":"p15-new","type":"acquire_agent","child_id":"ch_shared","work_kind":"test"}"#,
        "p15-new",
    );
    assert!(compact(&second).contains("\"granted\":true"), "{second}");
    conn.request(
        r#"{"id":"p15-stale-cancel","type":"cancel_acquire_agent","request_id":"p15-old","child_id":"ch_shared"}"#,
        "p15-stale-cancel",
    );
    let status = conn.request(r#"{"id":"p15-status","type":"status"}"#, "p15-status");
    assert!(compact(&status).contains("\"test\":{\"used\":1,\"total\":1}"), "stale cancel dropped the new permit: {status}");

    conn.send(
        r#"{"id":"p15-race-cancel","type":"cancel_acquire_agent","request_id":"p15-race","child_id":"ch_race"}"#,
    );
    conn.read_until("p15-race-cancel", Duration::from_secs(3)).unwrap();
    conn.send(
        r#"{"id":"p15-race","type":"acquire_agent","child_id":"ch_race","work_kind":"build"}"#,
    );
    assert!(conn.read_until("p15-race", Duration::from_millis(100)).is_none());
    conn.send(
        r#"{"id":"p15-race","type":"acquire_agent","child_id":"ch_race","work_kind":"build"}"#,
    );
    let retry = conn.read_until("p15-race", Duration::from_secs(3)).unwrap();
    assert!(compact(&retry).contains("\"granted\":true"), "retry after consumed cancel tombstone: {retry}");
}

#[test]
fn t13_repeated_start_exit_stress() {
    let d = Daemon::start("stress");
    let mut c = d.connect();
    hello_ext(&mut c, "sess-stress");
    const N: usize = 50;
    for i in 0..N {
        let sid = format!("r13s{i:03}");
        let resp = c.request(&start_req(&sid, "printf x", false), &sid);
        assert!(
            compact(&resp).contains("\"ok\":true"),
            "start {i} failed: {resp}"
        );
        let task_id = extract_str(&resp, "task_id").expect("task_id").to_string();
        let wid = format!("r13w{i:03}");
        let w = c.request(&wait_req(&wid, &task_id, 5000), &wid);
        assert!(
            compact(&w).contains("\"done\":true"),
            "wait {i} for {task_id}: {w}"
        );
    }
    let list = c.request(&list_req("r13final"), "r13final");
    let n_status = list.matches("\"status\":").count();
    assert!(
        n_status >= N,
        "expected ≥{N} terminal records after stress; list={list}"
    );
}
