//! Black-box integration tests for `pi-famulus`.
//!
//! Contract source: docs/design.md §3 (protocol messages, lifecycle, state
//! machine, CLI). Written against the *contract*, not the implementation:
//! each test spawns the compiled binary and speaks the wire protocol
//! (u32 BE length + UTF-8 JSON) by hand over the unix socket. std only —
//! no dev-dependencies, no serde; JSON is asserted via substring matching
//! (whitespace-tolerant where it matters via `compact()`).
//!
//! Isolation: every test uses its own PI_FAMULUS_HOME under temp_dir()
//! (pi-famulus-test-<pid>-<testname>) and kills its daemon on drop.

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

    fn read_until(&mut self, needle: &str, timeout: Duration) -> Option<String> {
        if let Some(f) = self.history.iter().find(|f| f.contains(needle)) {
            return Some(f.clone());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let frame = self.read_frame(deadline)?;
            if frame.contains(needle) {
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

// ---------------------------------------------------------------------------
// JSON helpers (std-only, substring based)
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

fn blocked_lock_attempts(path: &Path) -> usize {
    fs::read(path).map(|attempts| attempts.len()).unwrap_or(0)
}

/// Probe the real daemon lifetime lock without relying on pid-file contents.
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
        assert!(duplicate.try_wait().unwrap().is_none(), "second daemon exited before its retry cap");
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
