//! Shared black-box scaffolding for the adversarial lifecycle suites.
//!
//! Everything here talks to the compiled `pi-famulus` binary over its local
//! socket (u32 BE length + JSON, design doc §3.3): a unix socket, or on
//! Windows the named pipe derived from the home path. Nothing links against
//! the crate's internals; `serde_json`, `libc`, `tokio` and `windows-sys` are
//! already regular dependencies of the package, so integration tests can use
//! them without new dev-dependencies.
//!
//! The Unix-only suites use all of it. `tests/platform.rs` and
//! `tests/resource_bench.rs` use the portable part (marked below), which is
//! what runs on Windows.
//!
//! Determinism rules used throughout:
//! - every wait is a poll against a deadline (`poll_until`), never a bare sleep
//!   that the assertion depends on;
//! - the daemon's own timers (5s idle grace, 2s kill grace, the fallback
//!   leftover-group poll) are stepped with `Home::advance*`: on the manual
//!   clock under `--features test-clock`, as real sleeps otherwise (a task
//!   runner's lifeline grace is real time in the runner process);
//! - the remaining fixed sleeps are short "let the effect happen" pauses.

#![allow(dead_code)]

pub mod kit;

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_pi-famulus");
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const PATH_ENV: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin";

#[cfg(unix)]
pub const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
pub const SIGKILL: i32 = libc::SIGKILL;
/// Windows has no signals; the harness maps both to TerminateProcess.
#[cfg(windows)]
pub const SIGTERM: i32 = 15;
#[cfg(windows)]
pub const SIGKILL: i32 = 9;

/// Working directory for test tasks: `/tmp` on Unix, the temp dir on Windows
/// (a bare `/tmp` there means `<current drive>:\tmp`, which need not exist).
pub fn task_cwd() -> String {
    if cfg!(windows) {
        std::env::temp_dir().to_string_lossy().into_owned()
    } else {
        "/tmp".to_string()
    }
}

/// The complete task environment (§3.3: the client builds it). Unix: a fixed
/// PATH. Windows: the test's own environment, as the extension sends
/// `process.env`; Windows programs misbehave without `SystemRoot` & co.
pub fn task_env() -> Value {
    if cfg!(windows) {
        let mut m = serde_json::Map::new();
        for (k, v) in std::env::vars_os() {
            if let (Some(k), Some(v)) = (k.to_str(), v.to_str()) {
                m.insert(k.to_string(), json!(v));
            }
        }
        Value::Object(m)
    } else {
        json!({"PATH": PATH_ENV})
    }
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

/// Poll `f` every 25ms until it returns Some or `timeout` elapses.
pub fn poll_until<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A short real pause after a manual-clock step: timers that fired have
/// their effects (signals, exits, records) under way by the time it returns,
/// so a negative assertion that follows is meaningful.
pub fn settle() {
    std::thread::sleep(Duration::from_millis(150));
}

pub fn poll_true(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    poll_until(timeout, || if f() { Some(()) } else { None }).is_some()
}

// ---------------------------------------------------------------------------
// Isolated home
// ---------------------------------------------------------------------------

/// Isolated `--home`. Short path on purpose: unix socket paths are limited to
/// ~104 bytes on Darwin. On drop: SIGKILL every process group the daemon ever
/// recorded, SIGKILL the daemon from manager.pid, remove the directory.
pub struct Home {
    pub path: PathBuf,
    /// Extra pids (e.g. grandchildren) the test learned about.
    pub extra_pids: Vec<u32>,
    /// Daemons of this home run on the manual clock (see `advance`).
    pub manual: bool,
}

/// True when the suite runs with `--features test-clock`: daemons started
/// through `Home` then use the manual clock, and `Home::advance` steps it
/// instead of sleeping. Without the feature every wait is real time.
pub const MANUAL_CLOCK: bool = cfg!(feature = "test-clock");

/// `PI_FAMULUS_TEST_OWNER` for every daemon a test starts: this test process.
/// Test-clock daemons exit once it is gone (a killed test binary runs no
/// `Drop`), since their manual timers would never idle them out.
pub fn test_owner() -> String {
    std::process::id().to_string()
}

impl Home {
    pub fn new(name: &str) -> Home {
        let path = std::env::temp_dir().join(format!("pi-famulus-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test home");
        Home {
            path,
            extra_pids: Vec::new(),
            manual: MANUAL_CLOCK,
        }
    }
    /// A home whose daemons always use real time (timing canaries).
    pub fn new_real(name: &str) -> Home {
        let mut h = Home::new(name);
        h.manual = false;
        h
    }
    fn clock_env(&self) -> &'static str {
        if self.manual {
            "manual"
        } else {
            ""
        }
    }
    /// Advance the daemon's `label` timer by `ms`. Manual clock: wait until
    /// a timer with that label is armed (so the step cannot race ahead of
    /// the daemon), then advance. Real clock: just sleep `ms`.
    pub fn advance(&self, label: &str, ms: u64) {
        if !self.manual {
            std::thread::sleep(Duration::from_millis(ms));
            return;
        }
        self.wait_armed(label, ms);
        let r = clock_request(&self.path, json!({"type":"clock_advance","ms":ms}));
        assert_eq!(r["ok"], true, "clock_advance: {r}");
    }
    /// Wait for a `label` timer with at least `min_due_ms` left.
    ///
    /// "Any timer with that label" is not enough. A cancelled timer (e.g. the
    /// idle countdown a new hello aborted) is dropped asynchronously, so it
    /// can still be listed while the daemon has not yet armed the timer the
    /// test means. Under load, `d6b` once stepped the clock against such a
    /// leftover. A leftover never has as much time left as a fresh timer,
    /// because the clock has moved since it was armed.
    fn wait_armed(&self, label: &str, min_due_ms: u64) {
        let armed = poll_true(Duration::from_secs(10), || {
            clock_request(&self.path, json!({"type":"clock_status"}))["pending"]
                .as_array()
                .is_some_and(|p| {
                    p.iter()
                        .any(|t| t["label"] == label && t["due_in_ms"].as_u64().unwrap_or(0) >= min_due_ms)
                })
        });
        assert!(
            armed,
            "no {label:?} timer with >= {min_due_ms} ms left; pending: {}",
            clock_request(&self.path, json!({"type":"clock_status"}))
        );
    }
    /// Advance by `ms` once a `label` timer is armed with any time left (a
    /// timer re-armed with what remained of it, e.g. after an in-place
    /// upgrade). Real clock: sleep `ms`.
    pub fn advance_partial(&self, label: &str, ms: u64) {
        if !self.manual {
            std::thread::sleep(Duration::from_millis(ms));
            return;
        }
        self.wait_armed(label, 1);
        let r = clock_request(&self.path, json!({"type":"clock_advance","ms":ms}));
        assert_eq!(r["ok"], true, "clock_advance: {r}");
    }
    /// Advance time without waiting for a particular timer (e.g. to show a
    /// cancelled countdown does not fire). Real clock: sleep `ms`.
    pub fn advance_now(&self, ms: u64) {
        if !self.manual {
            std::thread::sleep(Duration::from_millis(ms));
            return;
        }
        let r = clock_request(&self.path, json!({"type":"clock_advance","ms":ms}));
        assert_eq!(r["ok"], true, "clock_advance: {r}");
    }
    /// Bring the armed `label` timer of `total_ms` to just short of its
    /// deadline, so the caller can assert nothing has fired yet.
    /// Manual clock: exactly 1 ms short; the timer must still be pending
    /// with 1 ms left (a shorter constant fails here, deterministically),
    /// then a short real pause so a wrongly early effect would show. Real
    /// clock: 60% of `total_ms`, leaving margin for scheduling (the boundary
    /// itself is the timing canaries' job).
    pub fn advance_almost(&self, label: &str, total_ms: u64) {
        if self.manual {
            // A freshly armed timer: exactly `total_ms` left.
            self.wait_armed(label, total_ms);
            self.advance(label, total_ms - 1);
            let st = clock_request(&self.path, json!({"type":"clock_status"}));
            let armed = st["pending"]
                .as_array()
                .is_some_and(|p| p.iter().any(|t| t["label"] == label && t["due_in_ms"] == 1));
            assert!(armed, "{label:?} timer is not {total_ms} ms long: {st}");
            settle();
        } else {
            std::thread::sleep(Duration::from_millis(total_ms * 6 / 10));
        }
    }
    /// Take a timer brought to `advance_almost` over its deadline. Manual
    /// clock: the last millisecond. Real clock: nothing; the caller then
    /// waits for the effect with a real timeout.
    pub fn advance_past(&self) {
        if self.manual {
            self.advance_now(1);
        }
    }
    #[cfg(unix)]
    pub fn sock(&self) -> PathBuf {
        self.path.join("manager.sock")
    }
    /// A daemon accepts connections on this home's socket / named pipe.
    pub fn reachable(&self) -> bool {
        Transport::connect(&self.path).is_ok()
    }
    pub fn pidfile(&self) -> PathBuf {
        self.path.join("manager.pid")
    }
    pub fn pidfile_pid(&self) -> Option<u32> {
        let v: Value = serde_json::from_slice(&fs::read(self.pidfile()).ok()?).ok()?;
        v.get("pid")?.as_u64().map(|p| p as u32)
    }
    /// Every TaskRecord on disk (any session).
    pub fn records(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(sessions) = fs::read_dir(self.path.join("sessions")) else {
            return out;
        };
        for s in sessions.flatten() {
            let Ok(tasks) = fs::read_dir(s.path().join("tasks")) else {
                continue;
            };
            for t in tasks.flatten() {
                let p = t.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Ok(v) = serde_json::from_slice::<Value>(&fs::read(&p).unwrap_or_default()) {
                        out.push(v);
                    }
                }
            }
        }
        out
    }
    pub fn record(&self, task_id: &str) -> Option<Value> {
        self.records()
            .into_iter()
            .find(|r| r["task_id"].as_str() == Some(task_id))
    }
    /// The daemon's stderr (panics land here), appended to `daemon.stderr`
    /// in the home so a failed test can keep it (see `Drop`).
    fn daemon_stderr(&self) -> Stdio {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.join("daemon.stderr"))
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null())
    }
    /// Spawn `pi-famulus --home H daemon` as a direct child of the test.
    pub fn spawn_daemon(&self) -> Child {
        let child = Command::new(BIN)
            .arg("--home")
            .arg(&self.path)
            .env("PI_FAMULUS_TEST_CLOCK", self.clock_env())
            .env("PI_FAMULUS_TEST_OWNER", test_owner())
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(self.daemon_stderr())
            .spawn()
            .expect("spawn daemon");
        // Bind later Drop/kill_pid probes to this process handle (Windows PID reuse).
        track(child.id());
        child
    }
    /// A private copy of the binary under this home, so a test can replace
    /// it (in-place upgrade) without touching the one other tests use.
    pub fn install_copy(&self) -> PathBuf {
        let dir = self.path.join("bin");
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("pi-famulus");
        replace_binary(&dst, Path::new(BIN));
        dst
    }
    /// Start a daemon from `bin` with extra environment; waits until it
    /// accepts connections.
    pub fn start_daemon_from(&self, bin: &Path, env: &[(&str, &str)]) -> Child {
        let mut cmd = Command::new(bin);
        cmd.arg("--home")
            .arg(&self.path)
            .env("PI_FAMULUS_TEST_CLOCK", self.clock_env())
            .env("PI_FAMULUS_TEST_OWNER", test_owner())
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(self.daemon_stderr());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn daemon");
        track(child.id());
        // The first run of a freshly copied binary is slow on macOS (code
        // signature assessment), hence the longer wait.
        assert!(
            poll_true(Duration::from_secs(20), || self.reachable() && self.pidfile_pid().is_some()),
            "daemon did not start listening within 20s"
        );
        child
    }
    /// Spawn a daemon and wait until it accepts connections.
    pub fn start_daemon(&self) -> Child {
        let child = self.spawn_daemon();
        assert!(
            // The daemon binds, then writes manager.pid: ready means both.
            poll_true(Duration::from_secs(3), || self.reachable() && self.pidfile_pid().is_some()),
            "daemon did not start listening within 3s"
        );
        child
    }
    pub fn connect(&self) -> Conn {
        let s = poll_until(Duration::from_secs(3), || Transport::connect(&self.path).ok())
            .expect("connect to the manager socket");
        Conn::new(s)
    }
    /// Run a CLI subcommand with a hard deadline.
    /// Run a CLI subcommand; a daemon it auto-spawns uses this home's clock.
    pub fn cli(&self, args: &[&str], timeout: Duration) -> CliOut {
        run_cli_env(&self.path, args, timeout, &[("PI_FAMULUS_TEST_CLOCK", self.clock_env())])
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if std::thread::panicking() && std::env::var_os("PI_FAMULUS_TEST_ARTIFACTS").is_some() {
            self.dump_processes();
        }
        // Windows reuses a finished task's pid within seconds, so killing
        // recorded pids there can hit another test's live process. Killing
        // the daemon below is enough: its kill-on-close jobs take every task
        // tree with it.
        #[cfg(unix)]
        for r in self.records() {
            if let Some(pid) = r["pid"].as_u64() {
                kill_group(pid as u32, SIGKILL);
            }
        }
        for p in &self.extra_pids {
            kill_pid(*p, SIGKILL);
        }
        // Windows reuses PIDs quickly: never TerminateProcess a bare pidfile
        // value unless we already hold a handle from spawn (`track`) or can
        // still prove the process is this home's daemon via its command line.
        #[cfg(windows)]
        {
            let pidfile = self.pidfile_pid();
            let tracked_ok = pidfile.is_some_and(|pid| win::kill_if_tracked(pid));
            if !tracked_ok {
                for pid in daemon_pids_for(&self.path) {
                    kill_pid(pid, SIGKILL);
                }
            }
        }
        #[cfg(unix)]
        {
            if let Some(pid) = self.pidfile_pid() {
                kill_pid(pid, SIGKILL);
            }
            for pid in daemon_pids_for(&self.path) {
                kill_pid(pid, SIGKILL);
            }
        }
        if std::thread::panicking() {
            keep_failed_home(&self.path);
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Home {
    /// Before a failed test's processes are killed: what each task and the
    /// daemon are doing (`ps` state and wait channel, stdio descriptors),
    /// to `processes.txt` in the home. A task that stopped making progress
    /// shows here whether it is blocked writing to a pipe, sleeping, or gone.
    #[cfg(windows)]
    fn dump_processes(&self) {
        let list = Command::new("tasklist")
            .args(["/v", "/fo", "csv"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_else(|e| format!("tasklist: {e}"));
        let groups: Vec<String> = self.records().iter().filter_map(|r| r["pid"].as_u64().map(|p| p.to_string())).collect();
        let _ = fs::write(self.path.join("processes.txt"), format!("# task runners: {}\n{list}", groups.join(" ")));
    }
    #[cfg(unix)]
    fn dump_processes(&self) {
        let mut groups: Vec<String> = self
            .records()
            .iter()
            .filter_map(|r| r["pid"].as_u64().map(|p| p.to_string()))
            .collect();
        let daemon = self.pidfile_pid().map(|p| p.to_string());
        let ps = Command::new("ps")
            .args(["-axo", "pid,ppid,pgid,stat,wchan,etime,command"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let mut report = String::new();
        let mut pids = Vec::new();
        for (i, line) in ps.lines().enumerate() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let hit = cols.len() > 2 && (groups.contains(&cols[2].to_string()) || daemon.as_deref() == Some(cols[0]));
            if i == 0 || hit {
                report.push_str(line);
                report.push('\n');
                if hit {
                    pids.push(cols[0].to_string());
                }
            }
        }
        for pid in &pids {
            let lsof = Command::new("lsof")
                .args(["-a", "-p", pid, "-d", "0-2"])
                .output()
                .map(|o| format!("{}\n{}{}", o.status, String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
                .unwrap_or_else(|e| format!("lsof: {e}"));
            report.push_str(&format!("\n# lsof -p {pid} -d 0-2\n{lsof}"));
        }
        groups.sort();
        let _ = fs::write(self.path.join("processes.txt"), format!("# task groups: {}\n{report}", groups.join(" ")));
    }
}

/// A failed test's home is otherwise deleted with everything the daemon
/// said. With `PI_FAMULUS_TEST_ARTIFACTS` set (CI uploads it), copy it there:
/// logs, per-session events and task records; files over 1 MiB (large task
/// output) and sockets are skipped.
fn keep_failed_home(home: &Path) {
    let Some(dir) = std::env::var_os("PI_FAMULUS_TEST_ARTIFACTS") else { return };
    let Some(name) = home.file_name() else { return };
    copy_small_files(home, &Path::new(&dir).join(name));
}

fn copy_small_files(src: &Path, dst: &Path) {
    let Ok(entries) = fs::read_dir(src) else { return };
    let _ = fs::create_dir_all(dst);
    for e in entries.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let (from, to) = (e.path(), dst.join(e.file_name()));
        if t.is_dir() {
            copy_small_files(&from, &to);
        } else if t.is_file() && e.metadata().map(|m| m.len() <= 1 << 20).unwrap_or(false) {
            let _ = fs::copy(&from, &to);
        }
    }
}

pub struct CliOut {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Atomically put a copy of `src` at `dst` (copy beside it, then rename),
/// the way `install` does: a new inode, never a half-written file.
pub fn replace_binary(dst: &Path, src: &Path) {
    let tmp = dst.with_extension("new");
    fs::copy(src, &tmp).expect("copy binary");
    // Run it once before it takes its final name. The first run of a new
    // binary file is slow on macOS (signature assessment): take it here, not
    // inside a timed step of the test. And on Linux, a child another test
    // thread forked while `fs::copy` had the file open for writing holds that
    // descriptor until it execs; until then, exec of this file fails with
    // ETXTBSY (CI, 2026-09-29: "spawn daemon: Text file busy"). Our own
    // descriptor is closed, so no later fork can inherit it: once one exec
    // succeeds, the file stays executable, for the test and for a daemon
    // that execs it in an upgrade.
    #[cfg(unix)]
    const ETXTBSY: i32 = libc::ETXTBSY;
    #[cfg(windows)]
    const ETXTBSY: i32 = -1;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match Command::new(&tmp).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                panic!("{}: still ETXTBSY after 10 s of retries (a writer never exec'd?)", tmp.display())
            }
            r => {
                r.unwrap_or_else(|e| panic!("run the copied binary {}: {e}", tmp.display()));
                break;
            }
        }
    }
    fs::rename(&tmp, dst).expect("rename binary");
}

pub fn run_cli(home: &Path, args: &[&str], timeout: Duration) -> CliOut {
    run_cli_env(home, args, timeout, &[])
}

pub fn run_cli_env(home: &Path, args: &[&str], timeout: Duration, env: &[(&str, &str)]) -> CliOut {
    let mut child = Command::new(BIN)
        .arg("--home")
        .arg(home)
        .args(args)
        .env("PI_FAMULUS_TEST_OWNER", test_owner())
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    // Drain both pipes while the child runs: output over the pipe buffer
    // (64 KiB) would otherwise block the child forever.
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        so.read_to_end(&mut b).ok();
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        se.read_to_end(&mut b).ok();
        b
    });
    let status = wait_child(&mut child, timeout);
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("cli {args:?} did not finish within {timeout:?}");
    };
    CliOut {
        status,
        stdout: String::from_utf8_lossy(&t_out.join().unwrap()).into_owned(),
        stderr: String::from_utf8_lossy(&t_err.join().unwrap()).into_owned(),
    }
}

pub fn wait_child(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    poll_until(timeout, || child.try_wait().ok().flatten())
}

/// Leave a socket inode at `path` that nobody listens on (what a SIGKILLed
/// daemon leaves behind).
///
/// `bind` + `drop` alone is not enough in a multi-threaded test binary. On
/// macOS, std sets FD_CLOEXEC only after `socket()` returns, so a child that
/// another test thread spawns in that window inherits the socket. Once the
/// socket is bound and listening, that copy keeps it accepting after we drop
/// ours: a client then connects, gets no hello answer and waits out its
/// timeout. A standalone repro leaked 35% of listeners under heavy
/// concurrent spawning. So check that a connect is refused, and if not,
/// unlink the path and try a fresh inode (the leaked copy then listens on a
/// name nobody can reach).
#[cfg(unix)]
pub fn dead_socket(path: &Path) {
    for _ in 0..50 {
        let _ = fs::remove_file(path);
        drop(std::os::unix::net::UnixListener::bind(path).expect("bind socket"));
        if UnixStream::connect(path).is_err() {
            return;
        }
    }
    panic!("could not create a dead socket at {}", path.display());
}

/// One manual-clock debug request, sent as the first frame of a fresh
/// connection (no hello), so it never counts as an active client.
pub fn clock_request(home: &Path, req: Value) -> Value {
    let s = poll_until(Duration::from_secs(3), || Transport::connect(home).ok()).expect("connect for clock request");
    let mut c = Conn::new(s);
    c.request(req)
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

#[cfg(unix)]
/// kill(pid, 0); EPERM counts as alive. Zombies count as alive too, so callers
/// that own the child must reap it (test-owned daemons are reaped via
/// `wait_child`; task processes are children of the daemon, not of the test).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
/// Like `pid_alive` but a zombie (exited, not yet reaped by its parent)
/// counts as dead. Grandchildren re-parented to init get reaped promptly, but
/// on macOS `ps` is the only portable way to see the Z state.
pub fn pid_running(pid: u32) -> bool {
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

#[cfg(unix)]
pub fn kill_pid(pid: u32, sig: i32) {
    if pid > 1 {
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

#[cfg(unix)]
pub fn kill_group(pid: u32, sig: i32) {
    if pid > 1 {
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
    }
}

#[cfg(unix)]
/// Resident set size of `pid` in bytes (via `ps -o rss=`, KiB on Darwin+Linux).
pub fn rss_bytes(pid: u32) -> Option<u64> {
    let o = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&o.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

#[cfg(unix)]
/// Pids of `pi-famulus ... daemon` processes whose command line names `home`
/// (i.e. daemons auto-spawned by a CLI client with `--home <home>`).
pub fn daemon_pids_for(home: &Path) -> Vec<u32> {
    let Ok(o) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };
    let home_s = home.to_string_lossy().to_string();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let (pid, cmd) = l.split_once(' ')?;
            let cmd = cmd.trim();
            // Exact token match: "pi-famulus-test-1-d1" must not match "pi-famulus-test-1-d15".
            let words: Vec<&str> = cmd.split_whitespace().collect();
            if cmd.contains("pi-famulus")
                && words.iter().any(|w| *w == home_s)
                && words.iter().any(|w| *w == "daemon")
            {
                let pid: u32 = pid.parse().ok()?;
                if pid_running(pid) {
                    return Some(pid);
                }
            }
            None
        })
        .collect()
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessHandleCount, GetProcessTimes, OpenProcess, TerminateProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    const STILL_ACTIVE: u32 = 259;

    struct Proc(HANDLE);
    impl Proc {
        fn open(pid: u32, access: u32) -> Option<Proc> {
            if pid == 0 {
                return None;
            }
            // SAFETY: plain handle query; closed in Drop.
            let h = unsafe { OpenProcess(access, 0, pid) };
            (!h.is_null()).then_some(Proc(h))
        }
    }
    impl Drop for Proc {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: a process handle is a process-wide kernel object.
    unsafe impl Send for Proc {}

    /// Handles to processes the tests were told about, by pid. Windows gives
    /// a freed pid to the next new process within seconds, so a probe by pid
    /// alone can find a stranger alive in place of a task that was killed
    /// (or kill that stranger). A handle stays bound to the original process.
    fn tracked() -> &'static Mutex<HashMap<(usize, u32), Proc>> {
        static T: OnceLock<Mutex<HashMap<(usize, u32), Proc>>> = OnceLock::new();
        T.get_or_init(Default::default)
    }

    pub fn track(scope: usize, pid: u32) {
        if let Some(p) = Proc::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE) {
            tracked().lock().unwrap().insert((scope, pid), p);
        }
    }

    pub fn forget(scope: usize) {
        tracked().lock().unwrap().retain(|(s, _), _| *s != scope);
    }

    fn with_proc<R>(pid: u32, access: u32, f: impl FnOnce(&Proc) -> R) -> Option<R> {
        if let Some(p) = tracked().lock().unwrap().get(&(super::test_scope(), pid)) {
            return Some(f(p));
        }
        Proc::open(pid, access).map(|p| f(&p))
    }

    /// The process exists and has not exited. A process that exited but
    /// still has open handles reports its exit code, so it counts as dead.
    pub fn pid_alive(pid: u32) -> bool {
        with_proc(pid, PROCESS_QUERY_LIMITED_INFORMATION, |p| {
            let mut code = 0u32;
            // SAFETY: valid handle, out pointer to a local.
            unsafe { GetExitCodeProcess(p.0, &mut code) != 0 && code == STILL_ACTIVE }
        })
        .unwrap_or(false)
    }

    pub fn kill_pid(pid: u32) {
        // SAFETY: valid handle with terminate access.
        with_proc(pid, PROCESS_TERMINATE, |p| unsafe { TerminateProcess(p.0, 137) });
    }

    /// Terminate only through a handle opened while this test still knew the
    /// pid belonged to its process. Returns false when nothing was tracked
    /// or termination failed (caller must fall back to command-line matching).
    pub fn kill_if_tracked(pid: u32) -> bool {
        let map = tracked().lock().unwrap();
        let Some(p) = map.get(&(super::test_scope(), pid)) else {
            return false;
        };
        // SAFETY: handle opened with PROCESS_TERMINATE in `track`.
        unsafe { TerminateProcess(p.0, 137) != 0 }
    }

    #[test]
    fn tracked_termination_failure_requests_fallback() {
        let pid = std::process::id();
        let key = (super::test_scope(), pid);
        // Query-only access deliberately makes TerminateProcess fail; this
        // handle cannot terminate the test process.
        let process = Proc::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        tracked().lock().unwrap().insert(key, process);
        let killed = kill_if_tracked(pid);
        tracked().lock().unwrap().remove(&key);
        assert!(!killed, "failed TerminateProcess must not suppress fallback cleanup");
        assert!(!kill_if_tracked(pid), "an untracked pid must request fallback too");
    }

    pub fn working_set_bytes(pid: u32) -> Option<u64> {
        let p = Proc::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        // SAFETY: zeroed POD out-struct with its size set.
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = unsafe { K32GetProcessMemoryInfo(p.0, &mut c, c.cb) };
        (ok != 0).then_some(c.WorkingSetSize as u64)
    }

    pub fn handle_count(pid: u32) -> Option<u64> {
        let p = Proc::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let mut n = 0u32;
        // SAFETY: valid handle, out pointer to a local.
        let ok = unsafe { GetProcessHandleCount(p.0, &mut n) };
        (ok != 0).then_some(n as u64)
    }

    /// User + kernel CPU time in milliseconds.
    pub fn cpu_ms(pid: u32) -> Option<u64> {
        let p = Proc::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z, z, z, z);
        // SAFETY: valid handle, out pointers to locals.
        let ok = unsafe { GetProcessTimes(p.0, &mut c, &mut e, &mut k, &mut u) };
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) / 10_000;
        (ok != 0).then(|| t(k) + t(u))
    }

    /// `pi-famulus ... daemon` processes whose command line names `home`.
    pub fn daemon_pids_for(home: &Path) -> Vec<u32> {
        let script = "Get-CimInstance Win32_Process -Filter \"Name='pi-famulus.exe'\" | \
                      ForEach-Object { \"$($_.ProcessId)`t$($_.CommandLine)\" }";
        let Ok(o) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
        else {
            return Vec::new();
        };
        let home_s = home.to_string_lossy().to_string();
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let (pid, cmd) = l.split_once('\t')?;
                // Homes with spaces are quoted on the command line; strip
                // quotes and match the path as one contiguous substring so
                // split_whitespace cannot break it apart.
                let flat: String = cmd.chars().filter(|&c| c != '"').collect();
                let home_flat: String = home_s.chars().filter(|&c| c != '"').collect();
                let has_daemon = flat.split_whitespace().any(|w| w == "daemon");
                if flat.contains(&home_flat) && has_daemon {
                    let pid: u32 = pid.trim().parse().ok()?;
                    pid_alive(pid).then_some(pid)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Bind this test's later probes of `pid` (`pid_alive`, `pid_running`,
/// `kill_pid`) to the process that has it now; see `win::track`. Unix keeps
/// a killed child's pid until its parent reaps it, and allocates pids in
/// order.
pub fn track(pid: u32) -> u32 {
    #[cfg(windows)]
    win::track(test_scope(), pid);
    pid
}

thread_local! {
    static TEST_SCOPE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The test this thread works for. Two tests running at once can each be
/// told about a process with the same (reused) pid; tracking is per test so
/// each probes its own. A thread a test spawns must `enter_test_scope` too.
pub fn test_scope() -> usize {
    TEST_SCOPE.with(|s| s.get())
}

pub fn enter_test_scope(scope: usize) {
    TEST_SCOPE.with(|s| s.set(scope));
}

/// Drop this thread's test scope and the processes it tracked.
pub fn end_test_scope() {
    #[cfg(windows)]
    win::forget(test_scope());
    enter_test_scope(0);
}
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    win::pid_alive(pid)
}
#[cfg(windows)]
pub fn pid_running(pid: u32) -> bool {
    win::pid_alive(pid)
}
#[cfg(windows)]
pub fn kill_pid(pid: u32, _sig: i32) {
    win::kill_pid(pid)
}
#[cfg(windows)]
pub fn rss_bytes(pid: u32) -> Option<u64> {
    win::working_set_bytes(pid)
}
#[cfg(windows)]
pub fn daemon_pids_for(home: &Path) -> Vec<u32> {
    win::daemon_pids_for(home)
}

/// Open handles (Windows) or descriptors (Unix) of `pid`.
pub fn open_handles(pid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        win::handle_count(pid)
    }
    #[cfg(target_os = "linux")]
    {
        Some(fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count() as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let o = Command::new("lsof").args(["-n", "-P", "-p", &pid.to_string()]).output().ok()?;
        let n = String::from_utf8_lossy(&o.stdout)
            .lines()
            .skip(1)
            .filter(|l| l.split_whitespace().nth(3).is_some_and(|fd| fd.starts_with(|c: char| c.is_ascii_digit())))
            .count();
        Some(n as u64)
    }
}

/// CPU time (user + system) `pid` has used so far, in milliseconds.
pub fn cpu_time_ms(pid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        win::cpu_ms(pid)
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesised comm; utime and stime are 14 and 15.
        let rest = &stat[stat.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        Some(ticks * 1000 / hz)
    }
    #[cfg(target_os = "macos")]
    {
        // `ps -o time=`: [[dd-]hh:]mm:ss.cc
        let o = Command::new("ps").args(["-o", "time=", "-p", &pid.to_string()]).output().ok()?;
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        let (rest, frac) = s.split_once('.').unwrap_or((&s, "0"));
        let mut secs = 0u64;
        for part in rest.split(':') {
            secs = secs * 60 + part.parse::<u64>().ok()?;
        }
        Some(secs * 1000 + frac.parse::<u64>().ok()? * 10)
    }
}

// ---------------------------------------------------------------------------
// Wire connection
// ---------------------------------------------------------------------------

/// Named-pipe identity of a home, as the manager derives it: FNV-1a 64 of
/// the absolute home path with `\` separators, no trailing separator, in
/// lower case.
pub fn pipe_name(home: &Path) -> String {
    let home = std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf());
    let mut s = home.to_string_lossy().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.to_lowercase().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!(r"\\.\pipe\pi-famulus-{h:x}")
}

/// The client end of the manager's local socket.
///
/// Unix: a blocking `UnixStream` with read timeouts. Windows: a tokio named
/// pipe client driven by a private current-thread runtime, because a
/// synchronous pipe handle serialises a blocked read against writes. Either
/// way bytes are only read when the test asks for them, so a test that
/// stops reading really leaves frames in the daemon's queue.
pub struct Transport {
    #[cfg(unix)]
    stream: UnixStream,
    #[cfg(windows)]
    rt: tokio::runtime::Runtime,
    #[cfg(windows)]
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
}

#[cfg(unix)]
impl From<UnixStream> for Transport {
    fn from(stream: UnixStream) -> Transport {
        Transport { stream }
    }
}

impl Transport {
    #[cfg(unix)]
    pub fn connect(home: &Path) -> std::io::Result<Transport> {
        Ok(Transport { stream: UnixStream::connect(home.join("manager.sock"))? })
    }
    #[cfg(windows)]
    pub fn connect(home: &Path) -> std::io::Result<Transport> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let name = pipe_name(home);
        let pipe = {
            let _g = rt.enter();
            tokio::net::windows::named_pipe::ClientOptions::new().open(&name)?
        };
        Ok(Transport { rt, pipe })
    }
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            self.stream.write_all(bytes)?;
            self.stream.flush()
        }
        #[cfg(windows)]
        {
            use tokio::io::AsyncWriteExt;
            let pipe = &mut self.pipe;
            self.rt.block_on(async { pipe.write_all(bytes).await?; pipe.flush().await })
        }
    }
    /// One read of at most `buf.len()` bytes, waiting up to `d`. Times out
    /// with `ErrorKind::TimedOut`; `Ok(0)` is EOF.
    pub fn read_timeout(&mut self, buf: &mut [u8], d: Duration) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            self.stream.set_read_timeout(Some(d.max(Duration::from_millis(1)))).ok();
            self.stream.read(buf)
        }
        #[cfg(windows)]
        {
            use tokio::io::AsyncReadExt;
            let pipe = &mut self.pipe;
            match self.rt.block_on(async { tokio::time::timeout(d, pipe.read(buf)).await }) {
                Ok(Ok(n)) => Ok(n),
                // The server closed its end: EOF, like a unix socket.
                Ok(Err(e)) if e.raw_os_error() == Some(109) => Ok(0),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
            }
        }
    }
}

pub enum Recv {
    Frame(Value),
    Timeout,
    Closed,
}

pub struct Conn {
    pub stream: Transport,
    buf: Vec<u8>,
    /// Frames read while waiting for something else (events, other ids).
    pub pending: VecDeque<Value>,
    /// Every event ever seen on this connection.
    pub events: Vec<Value>,
    next_id: u64,
    pub closed: bool,
    /// What made `closed` true: EOF, or the read error.
    pub close_reason: String,
}

impl Conn {
    pub fn new(stream: impl Into<Transport>) -> Conn {
        Conn {
            stream: stream.into(),
            buf: Vec::new(),
            pending: VecDeque::new(),
            events: Vec::new(),
            next_id: 0,
            closed: false,
            close_reason: String::new(),
        }
    }

    pub fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes)
    }

    pub fn send(&mut self, v: &Value) {
        let body = serde_json::to_vec(v).unwrap();
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);
        self.send_raw(&frame).expect("write frame");
    }

    /// Read one frame from the socket before `deadline`.
    pub fn recv(&mut self, deadline: Instant) -> Recv {
        loop {
            if self.buf.len() >= 4 {
                let n = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                    as usize;
                if self.buf.len() >= 4 + n {
                    let body: Vec<u8> = self.buf.drain(..4 + n).skip(4).collect();
                    let v: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
                        panic!("daemon sent non-JSON frame ({e}): {:?}", String::from_utf8_lossy(&body[..body.len().min(200)]))
                    });
                    if v["type"] == "event" {
                        self.events.push(v.clone());
                    }
                    return Recv::Frame(v);
                }
            }
            if self.closed {
                return Recv::Closed;
            }
            let now = Instant::now();
            if now >= deadline {
                return Recv::Timeout;
            }
            let mut chunk = vec![0u8; 64 * 1024];
            match self.stream.read_timeout(&mut chunk, (deadline - now).min(Duration::from_millis(100))) {
                Ok(0) => {
                    self.closed = true;
                    self.close_reason = format!("EOF from the daemon ({} bytes of a frame buffered)", self.buf.len());
                }
                Ok(k) => self.buf.extend_from_slice(&chunk[..k]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.closed = true;
                    self.close_reason = format!("read error: {e} ({:?})", e.kind());
                }
            }
        }
    }

    /// Send `req` with a fresh id; return the response echoing it (or None on
    /// timeout / close). Other frames are kept in `pending`.
    pub fn try_request(&mut self, mut req: Value, timeout: Duration) -> Option<Value> {
        self.next_id += 1;
        let id = format!("q{}", self.next_id);
        req["v"] = json!(1);
        req["id"] = json!(id);
        self.send(&req);
        if let Some(pos) = self.pending.iter().position(|f| f["id"] == json!(id)) {
            return self.pending.remove(pos);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.recv(deadline) {
                Recv::Frame(f) => {
                    if f["id"] == json!(id) {
                        return Some(f);
                    }
                    self.pending.push_back(f);
                }
                Recv::Timeout | Recv::Closed => return None,
            }
        }
    }

    /// Wait for the response with a caller-chosen id (for raw-sent frames).
    /// Responses to pipelined requests may arrive in any order.
    pub fn wait_id(&mut self, id: &str, timeout: Duration) -> Option<Value> {
        if let Some(pos) = self.pending.iter().position(|f| f["id"] == json!(id)) {
            return self.pending.remove(pos);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.recv(deadline) {
                Recv::Frame(f) if f["id"] == json!(id) => return Some(f),
                Recv::Frame(f) => self.pending.push_back(f),
                Recv::Timeout | Recv::Closed => return None,
            }
        }
    }

    /// Send and wait for the answer: 10 s, or a request's own `budget_ms`
    /// plus 5 s, so a `wait` that legitimately runs out its budget answers
    /// `done:false` instead of looking like a daemon that stopped replying.
    pub fn request(&mut self, req: Value) -> Value {
        let budget = req["budget_ms"].as_u64().map(|ms| Duration::from_millis(ms) + Duration::from_secs(5));
        let limit = budget.unwrap_or_default().max(Duration::from_secs(10));
        let r = self.try_request(req.clone(), limit);
        r.unwrap_or_else(|| panic!("no response to {req} within {limit:?} (closed={} {})", self.closed, self.close_reason))
    }

    pub fn request_ok(&mut self, req: Value) -> Value {
        let r = self.request(req.clone());
        assert_eq!(r["ok"], json!(true), "request {req} failed: {r}");
        r
    }

    pub fn hello_ext(&mut self, session: &str) -> Value {
        self.request(json!({"type":"hello","client_kind":"extension","session_id":session,"pi_pid":std::process::id()}))
    }

    pub fn hello(&mut self, req: Value) -> Value {
        self.request_ok(req)
    }

    pub fn hello_cli(&mut self) -> Value {
        self.request(json!({"type":"hello","client_kind":"cli"}))
    }

    /// Start a shell task; returns (task_id, pid).
    pub fn start(&mut self, command: &str) -> (String, u32) {
        self.start_with(json!({"type":"start","kind":"shell","command":command,"cwd":task_cwd(),
            "env":task_env(),"run_in_background":true}))
    }

    pub fn start_with(&mut self, req: Value) -> (String, u32) {
        let r = self.request_ok(req);
        (
            r["task_id"].as_str().unwrap().to_string(),
            track(r["pid"].as_u64().unwrap() as u32),
        )
    }

    /// Wait for an event matching `pred`, looking at already-seen events first.
    pub fn wait_event(&mut self, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(e) = self.events.iter().find(|e| pred(e)) {
            return Some(e.clone());
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.recv(deadline) {
                Recv::Frame(f) => {
                    if f["type"] == "event" && pred(&f) {
                        return Some(f);
                    }
                    if f["type"] != "event" {
                        self.pending.push_back(f);
                    }
                }
                Recv::Timeout | Recv::Closed => return None,
            }
        }
    }

    /// Read whatever arrives for `d` (events land in `events`).
    pub fn drain(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        while let Recv::Frame(f) = self.recv(deadline) {
            if f["type"] != "event" {
                self.pending.push_back(f);
            }
        }
    }

    /// Drain frames until the peer closes; true if it closed before timeout.
    pub fn wait_closed(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.recv(deadline) {
                Recv::Frame(f) => {
                    if f["type"] != "event" {
                        self.pending.push_back(f);
                    }
                }
                Recv::Closed => return true,
                Recv::Timeout => return false,
            }
        }
    }

    pub fn task(&mut self, task_id: &str) -> Option<Value> {
        let r = self.request_ok(json!({"type":"list","all":true}));
        r["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["task_id"] == json!(task_id))
            .cloned()
    }

    pub fn status_of(&mut self, task_id: &str) -> Option<String> {
        self.task(task_id)
            .and_then(|t| t["status"].as_str().map(|s| s.to_string()))
    }

    /// Poll `list` until `task_id` reaches a terminal status; returns the record.
    pub fn wait_terminal(&mut self, task_id: &str, timeout: Duration) -> Option<Value> {
        poll_until(timeout, || {
            let t = self.task(task_id)?;
            if t["status"] != "running" {
                Some(t)
            } else {
                None
            }
        })
    }

    /// Read the full output via cursor loop until caught up and terminal.
    pub fn read_all_output(&mut self, task_id: &str, max_bytes: u64) -> (String, u64, Value) {
        let mut cursor = 0u64;
        let mut text = String::new();
        let mut last;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            last = self.request_ok(json!({"type":"output","task_id":task_id,"cursor":cursor,"max_bytes":max_bytes}));
            let chunk = last["chunk"].as_str().unwrap();
            text.push_str(chunk);
            let next = last["next_cursor"].as_u64().unwrap();
            let total = last["total_size"].as_u64().unwrap();
            if next == cursor && last["status"] != "running" && next >= total {
                break;
            }
            if next == cursor {
                std::thread::sleep(Duration::from_millis(20));
            }
            cursor = next;
            assert!(Instant::now() < deadline, "output loop did not catch up");
        }
        (text, cursor, last)
    }
}

// ---------------------------------------------------------------------------
// Helper client process (a real "pi" stand-in that can be kill -9'd)
// ---------------------------------------------------------------------------

/// A separate OS process holding one extension connection. It is this same
/// test binary re-executed to run the `helper_hold_extension_conn` entry
/// point (see lifecycle_adversarial.rs). Killing it with SIGKILL is exactly
/// the "pi crashed" event: the kernel closes the socket.
pub struct HelperClient {
    pub child: Child,
    pub tasks: Vec<(String, u32)>,
    pub lines: Vec<String>,
    _stdout: BufReader<ChildStdout>,
}

impl HelperClient {
    pub fn spawn(home: &Path, session: &str, commands: &[&str]) -> HelperClient {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(exe)
            .args([
                "--exact",
                "helper_hold_extension_conn",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PI_FAMULUS_TEST_HELPER_HOME", home)
            .env("PI_FAMULUS_TEST_HELPER_SESSION", session)
            .env("PI_FAMULUS_TEST_HELPER_CMDS", commands.join("\n"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper client");
        let mut rd = BufReader::new(child.stdout.take().unwrap());
        let mut tasks = Vec::new();
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = rd.read_line(&mut line).expect("helper stdout");
            assert!(n > 0, "helper exited before READY; lines={lines:?}");
            let line = line.trim().to_string();
            if let Some(i) = line.find("PI_FAMULUS_TEST_HELPER_TASK ") {
                let rest = &line[i + "PI_FAMULUS_TEST_HELPER_TASK ".len()..];
                let mut it = rest.split_whitespace();
                let id = it.next().unwrap().to_string();
                let pid: u32 = it.next().unwrap().parse().unwrap();
                tasks.push((id, track(pid)));
            } else if line.contains("PI_FAMULUS_TEST_HELPER_READY") {
                break;
            }
            lines.push(line);
        }
        HelperClient {
            child,
            tasks,
            lines,
            _stdout: rd,
        }
    }

    /// SIGKILL the helper and reap it: the daemon sees an abrupt EOF.
    pub fn crash(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for HelperClient {
    fn drop(&mut self) {
        self.crash();
    }
}

/// Body of the helper process. Returns immediately unless re-exec'd by
/// `HelperClient::spawn`.
pub fn helper_main() {
    let Ok(home) = std::env::var("PI_FAMULUS_TEST_HELPER_HOME") else {
        return;
    };
    let session = std::env::var("PI_FAMULUS_TEST_HELPER_SESSION").unwrap();
    let cmds = std::env::var("PI_FAMULUS_TEST_HELPER_CMDS").unwrap_or_default();
    let stream = Transport::connect(Path::new(&home)).expect("helper connect");
    let mut c = Conn::new(stream);
    let h = c.hello_ext(&session);
    assert_eq!(h["ok"], json!(true), "helper hello: {h}");
    let mut out = std::io::stdout();
    for cmd in cmds.split('\n').filter(|s| !s.is_empty()) {
        let (id, pid) = c.start(cmd);
        writeln!(out, "PI_FAMULUS_TEST_HELPER_TASK {id} {pid}").unwrap();
    }
    writeln!(out, "PI_FAMULUS_TEST_HELPER_READY").unwrap();
    out.flush().unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Extract whitespace-separated integers printed by a task (e.g. `echo $!`).
pub fn pids_in(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .filter_map(|w| w.parse::<u32>().ok())
        .collect()
}

/// Poll a task's output until it contains at least `n` integers (pids).
pub fn wait_for_pids(c: &mut Conn, task_id: &str, n: usize) -> Vec<u32> {
    poll_until(Duration::from_secs(5), || {
        let r = c.request_ok(json!({"type":"output","task_id":task_id,"cursor":0,"max_bytes":65536}));
        let p = pids_in(r["chunk"].as_str().unwrap_or(""));
        if p.len() >= n {
            Some(p.into_iter().map(track).collect())
        } else {
            None
        }
    })
    .unwrap_or_else(|| panic!("task {task_id} did not print {n} pids"))
}
