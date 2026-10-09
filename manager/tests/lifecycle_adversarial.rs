#![cfg(unix)]
//! Adversarial black-box lifecycle tests for pi-famulus.
//!
//! Each test targets one cell of the state-transition table in
//! manager/TESTING.md (ids like `C3`, `T5`, `D7` refer to rows there) and
//! asserts the customer-visible invariant: which processes are alive, which
//! files exist, what the wire says. No crate internals are used.
//!
//! Tests d2, d3, d12, t5b, t6b, t6c, o3, o4 began as `#[ignore = "bug: …"]`
//! reproducers that failed against the original code. Each bug is fixed and
//! the test now guards the fix; see manager/TESTING.md "Bugs found".

mod common;

use common::*;
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

/// Re-exec entry point for `HelperClient` (a killable stand-in for pi). It is
/// a no-op unless PI_FAMULUS_TEST_HELPER_HOME is set.
#[test]
#[ignore = "helper entry point, re-executed by crash tests; not a test"]
fn helper_hold_extension_conn() {
    helper_main();
}

const S: fn(u64) -> Duration = Duration::from_secs;
const MS: fn(u64) -> Duration = Duration::from_millis;
// Mirrors the production cap; the debug hook records one byte per WouldBlock.
const DAEMON_LOCK_RETRY_ATTEMPTS: usize = 100;

#[cfg(feature = "test-clock")]
struct CliGuard(Option<std::process::Child>);

#[cfg(feature = "test-clock")]
impl Drop for CliGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                kill_pid(child.id(), libc::SIGCONT);
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn wait_output_contains(c: &mut Conn, task_id: &str, needle: &str) {
    let ok = poll_true(S(5), || {
        let r = c.request_ok(json!({"type":"output","task_id":task_id,"cursor":0,"max_bytes":65536}));
        r["chunk"].as_str().unwrap_or("").contains(needle)
    });
    assert!(ok, "task {task_id} never printed {needle:?}");
}

/// Start a daemon that leaves a byte in `blocked` every time it cannot yet
/// acquire manager.lock. The debug-only hook makes lock contention observable
/// without relying on a scheduler sleep in the lifecycle regression below.
fn spawn_lock_observer_daemon(
    home: &Home,
    blocked: &std::path::Path,
    barrier: Option<&std::path::Path>,
) -> std::process::Child {
    let mut command = std::process::Command::new(BIN);
    command
        .arg("--home")
        .arg(&home.path)
        .env("PI_FAMULUS_TEST_CLOCK", if home.manual { "manual" } else { "" })
        .env("PI_FAMULUS_TEST_OWNER", test_owner())
        .env("PI_FAMULUS_TEST_DAEMON_LOCK_BLOCKED", blocked)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(home.path.join("daemon.stderr"))
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        );
    if let Some(barrier) = barrier {
        command.env("PI_FAMULUS_TEST_DAEMON_LOCK_BARRIER", barrier);
    }
    command.spawn().expect("spawn lock-observing daemon")
}

fn blocked_lock_attempts(path: &std::path::Path) -> usize {
    std::fs::read(path).map(|attempts| attempts.len()).unwrap_or(0)
}

// ===========================================================================
// Daemon: spawn race / singleton (D1, D2, D3)
// ===========================================================================

/// Is manager.lock (the daemon's lifetime lock, §3.1) free right now?
/// Takes and immediately releases it when free.
fn lifetime_lock_free(home: &Home) -> bool {
    lifetime_lock_free_path(&home.path)
}

fn lifetime_lock_free_path(path: &std::path::Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join("manager.lock"))
        .unwrap();
    // SAFETY: flock on an fd we own; released when `f` is dropped.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Everything the diagnostics of a failed D1 round need. Capture a sample
/// before `Home::drop` cleans up a still-live daemon; substring matches on the
/// home path are unsafe (`...-d1` also matches `...-d15`).
fn d1_diagnostics(home: &Home) -> String {
    let pids = daemon_pids_for(&home.path);
    let processes: Vec<String> = pids
        .iter()
        .map(|pid| {
            let pid_arg = pid.to_string();
            let ps = std::process::Command::new("ps")
                .args(["-p", pid_arg.as_str(), "-o", "pid=,ppid=,state=,command="])
                .output();
            let ps = ps
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_else(|e| e.to_string());
            #[cfg(target_os = "macos")]
            let sample = std::process::Command::new("sample")
                .args([pid_arg.as_str(), "1"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_else(|e| e.to_string());
            #[cfg(not(target_os = "macos"))]
            let sample = "sample unavailable on this platform".to_string();
            format!("pid={pid}: ps={ps}\nsample:\n{sample}")
        })
        .collect();
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
    format!(
        "pidfile={:?}; live daemon pids={pids:?}; processes={processes:#?}\nmanager.log:\n{log}",
        home.pidfile_pid()
    )
}

/// D1: N clients race to auto-spawn the daemon on an empty home.
///
/// The singleton invariant (§3.1), checked directly:
/// (a) every client was served by the same daemon: each client opens its
///     own extension session (`start --session`), and sessions live only in
///     the memory of the daemon that served them, so the survivor's
///     `status` must list all of them;
/// (b) exactly one process holds the lifetime lock: the lock is held while
///     the socket's daemon runs and is free the moment that daemon exits,
///     so no other process held it;
/// (c) the daemon process count converges to 1 within 2s. A redundant
///     daemon spawned by a client that lost the race exits "already
///     running" without binding; it may be seen only transiently.
///
/// Counting processes right after the clients return (the old check)
/// flaked twice. A snapshot can catch a redundant daemon before it exits,
/// and the count is not the invariant.
#[test]
fn d1_concurrent_clients_spawn_exactly_one_daemon() {
    let home = Home::new("d1");
    let clock = if home.manual { "manual" } else { "" };
    for round in 0..3 {
        let sessions: Vec<String> = (0..12).map(|i| format!("d1-r{round}-c{i}")).collect();
        // `start` auto-spawns like every task command (`status` never does).
        let kids: Vec<_> = sessions
            .iter()
            .map(|sid| {
                std::process::Command::new(BIN)
                    .arg("--home")
                    .arg(&home.path)
                    .args(["start", "--session", sid, "--", "true"])
                    .env("PI_FAMULUS_TEST_CLOCK", clock)
                    .env("PI_FAMULUS_TEST_OWNER", test_owner())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let mut task_ids = Vec::with_capacity(kids.len());
        for k in kids {
            let out = k.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "round {round}: client failed: {} {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                d1_diagnostics(&home)
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            let task_id = stdout
                .strip_prefix("task_id=")
                .and_then(|line| line.split_whitespace().next())
                .unwrap_or_else(|| panic!("round {round}: start output had no task id: {stdout:?}"));
            task_ids.push(task_id.to_owned());
        }

        // (c) Redundant daemons exit on their own, promptly (no timer is
        // involved, so real time is right in both clock modes).
        let settled = poll_until(S(2), || {
            let live = daemon_pids_for(&home.path);
            (live.len() == 1).then(|| live[0])
        });
        let Some(daemon) = settled else {
            panic!(
                "round {round}: daemon count did not converge to 1 within 2s: {:?}\n{}",
                daemon_pids_for(&home.path),
                d1_diagnostics(&home)
            );
        };

        // The survivor is the daemon on the socket and in the pid file.
        let mut c = home.connect();
        let hello = c.hello_cli();
        assert_eq!(hello["pid"].as_u64(), Some(daemon as u64), "round {round}: socket owner");
        assert_eq!(home.pidfile_pid(), Some(daemon), "round {round}: pid file");

        // (a) It served every client.
        let st = c.request_ok(json!({"type":"status"}));
        let served: Vec<&str> = st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["session_id"].as_str())
            .collect();
        for sid in &sessions {
            assert!(
                served.contains(&sid.as_str()),
                "round {round}: client {sid} was not served by the surviving daemon {daemon}: {st}\n{}",
                d1_diagnostics(&home)
            );
        }
        // All clients have returned from `start`, not necessarily from their
        // `true` tasks. Under the test clock, shutting down with even one task
        // still running waits on the unadvanced 2s kill grace.
        for task_id in &task_ids {
            let done = c.request_ok(json!({ "type": "wait", "task_id": task_id, "budget_ms": 10_000 }));
            assert_eq!(done["done"], true, "round {round}: task {task_id} did not finish: {done}");
        }
        drop(c);

        // (b) The lifetime lock is held while it runs...
        assert!(!lifetime_lock_free(&home), "round {round}: nobody holds manager.lock");
        // ...and free as soon as it has exited: no other process held it.
        let out = home.cli(&["shutdown"], S(10));
        assert!(out.status.success(), "{}", out.stderr);
        assert!(
            poll_true(S(5), || lifetime_lock_free(&home)),
            "round {round}: manager.lock stayed held after shutdown (pid {daemon} alive={})\n{}",
            pid_running(daemon),
            d1_diagnostics(&home)
        );
        assert!(
            poll_true(S(10), || daemon_pids_for(&home.path).is_empty()),
            "round {round}: a pi-famulus daemon outlived shutdown\n{}",
            d1_diagnostics(&home)
        );
    }
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
    eprintln!(
        "d1: redundant daemons that exited \"already running\": {}",
        log.matches("already running").count()
    );
}

/// CPU burners for race reproducers; killed on drop. Drop never runs when the
/// test binary itself is killed (Ctrl-C, a timeout), so each burner also spins
/// only while its parent is alive: an orphan exits instead of pinning a core.
struct Burners(Vec<std::process::Child>);
impl Burners {
    fn start() -> Burners {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Burners(
            (0..n)
                .map(|_| {
                    std::process::Command::new("sh")
                        .args(["-c", "while kill -0 $PPID 2>/dev/null; do :; done"])
                        .spawn()
                        .unwrap()
                })
                .collect(),
        )
    }
}
impl Drop for Burners {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            let _ = b.kill();
            let _ = b.wait();
        }
    }
}

/// Failure-only snapshot: distinguish a rejected singleton claim from a slow
/// startup or a missing/unlinked socket. This is diagnostics, not a race fix.
fn d2_diagnostics(home: &Home, old: u32) -> String {
    use std::os::unix::io::AsRawFd;
    let files: Vec<_> = ["manager.sock", "manager.pid", "manager.pid.tmp", "manager.lock", "manager.spawn.lock"]
        .iter()
        .map(|name| format!("{name}: {:?}", std::fs::symlink_metadata(home.path.join(name))))
        .collect();
    let locks: Vec<_> = ["manager.lock", "manager.spawn.lock"]
        .iter()
        .map(|name| {
            let result = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(home.path.join(name))
                .map(|f| {
                    // SAFETY: probe an fd we own; a successful lock is released
                    // when f drops. Never unlink or create a lock file here.
                    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                        "free".to_string()
                    } else {
                        format!("probe failed: {}", std::io::Error::last_os_error())
                    }
                });
            format!("{name}: {result:?}")
        })
        .collect();
    format!(
        "old pid={old} running={}; files={files:#?}; locks={locks:#?}; raw pidfile={:?}\n{}",
        pid_running(old),
        std::fs::read_to_string(home.pidfile()),
        d1_diagnostics(home)
    )
}

/// D2: the same race, but starting from stale socket/pid files left by a
/// SIGKILLed daemon. Regression guard: clients used to delete "zombie" files
/// themselves, and a slow one could unlink the socket of the daemon a faster
/// client had just spawned. Invariant: every client succeeds, and exactly one
/// reachable daemon remains.
#[test]
fn d2_concurrent_clients_over_stale_files_spawn_exactly_one_daemon() {
    let home = Home::new("d2");
    // Scheduler pressure widens the microsecond race window the way a busy
    // laptop does; without it the race needs many rounds to show.
    let _burn = Burners::start();
    for round in 0..30 {
        // Leave stale files behind: start a daemon, SIGKILL it.
        let out = home.cli(&["ls"], S(10));
        assert!(out.status.success());
        let old = home.pidfile_pid().expect("pid file");
        kill_pid(old, libc::SIGKILL);
        assert!(poll_true(S(3), || !pid_running(old)));
        assert!(home.sock().exists() && home.pidfile().exists(), "stale files expected");

        let kids: Vec<_> = (0..24)
            .map(|_| {
                std::process::Command::new(BIN)
                    .arg("--home")
                    .arg(&home.path)
                    .arg("ls")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let mut failures = Vec::new();
        for k in kids {
            let out = k.wait_with_output().unwrap();
            if !out.status.success() {
                failures.push(String::from_utf8_lossy(&out.stderr).to_string());
            }
        }
        let mut live = daemon_pids_for(&home.path);
        assert!(
            failures.is_empty(),
            "round {round}: clients failed: {failures:?}\n{}",
            d2_diagnostics(&home, old)
        );
        let mut c = home.connect();
        let h = c.hello_cli();
        // A losing spawn can still be finishing its bounded lifetime-lock
        // retry after every CLI has connected to the winner. Wait for those
        // non-serving contenders to exit before asserting the process count.
        assert!(poll_true(S(15), || {
            live = daemon_pids_for(&home.path);
            live.len() <= 1
        }), "round {round}: daemon contenders did not settle\n{}", d2_diagnostics(&home, old));
        assert_eq!(live.len(), 1, "round {round}: daemons alive: {live:?}\n{}", d2_diagnostics(&home, old));
        // The survivor must own the well-known socket; the retained client
        // also prevents idle shutdown while contenders finish their retries.
        assert_eq!(h["pid"].as_u64().map(|p| p as u32), Some(live[0]), "round {round}: socket owner\n{}", d2_diagnostics(&home, old));
        drop(c);
    }
}

/// D3: several `daemon` processes started at the same instant (bypassing the
/// client spawn lock). Invariant: exactly one survives and it owns the socket
/// and the pid file; the others exit 0 ("already running").
#[test]
fn d3_concurrent_daemon_processes_leave_one_survivor() {
    for round in 0..10 {
        let home = Home::new(&format!("d3r{round}"));
        let blocked = home.path.join("blocked-lock-attempts");
        let mut kids: Vec<_> = (0..6)
            .map(|_| spawn_lock_observer_daemon(&home, &blocked, None))
            .collect();
        // Identify and keep the lock owner connected before waiting for the
        // contenders, so the five-second idle shutdown cannot race the wait.
        let mut client = home.connect();
        let owner = client.hello_cli()["pid"]
            .as_u64()
            .expect("owner hello did not identify the daemon") as u32;

        // Five contenders must each finish the production lock-retry cap;
        // the marker proves actual WouldBlock attempts, while 15s is only a
        // safety deadline for scheduling (not a claim-time contract).
        let expected_blocked_attempts = 5 * DAEMON_LOCK_RETRY_ATTEMPTS;
        let mut running = Vec::new();
        assert!(
            poll_true(S(15), || {
                running = kids
                    .iter_mut()
                    .filter_map(|k| k.try_wait().unwrap().is_none().then(|| k.id()))
                    .collect();
                running.len() <= 1
                    && blocked_lock_attempts(&blocked) == expected_blocked_attempts
            }),
            "round {round}: daemons did not converge after bounded lock retries; still running {running:?}, blocked attempts {}, expected {expected_blocked_attempts}",
            blocked_lock_attempts(&blocked)
        );
        for k in kids.iter_mut() {
            let _ = k.kill();
            let _ = k.wait();
        }
        assert_eq!(
            running,
            vec![owner],
            "round {round}: running daemons {running:?}, socket owner {owner}"
        );
        assert_eq!(
            blocked_lock_attempts(&blocked),
            expected_blocked_attempts,
            "round {round}: every losing daemon must exhaust the bounded lock policy"
        );
        assert_eq!(home.pidfile_pid(), Some(owner));
        assert!(home.sock().exists(), "round {round}: owner socket disappeared");
    }
}

// ===========================================================================
// Daemon: crash takes every task down; no re-adoption (D4, T7, T8)
// ===========================================================================

/// Tasks for the crash tests: every shape a crash must clean up.
struct CrashFixture {
    /// (task id, runner pid) of the tasks still running at the crash.
    running: Vec<(String, u32)>,
    /// A finished task whose background child is still alive.
    finished: String,
    /// Every process that must be gone after the crash: runners, the
    /// shells' children, and the finished task's leftover.
    all_pids: Vec<u32>,
    /// A grandchild that ignores SIGTERM: only the SIGKILL after the grace
    /// takes it down.
    term_ignoring: u32,
}

fn start_crash_fixture(c: &mut Conn) -> CrashFixture {
    let (plain, p_plain) = c.start("sleep 300");
    let (stubborn, p_stubborn) = c.start("trap '' TERM; sleep 300 & echo $!; wait");
    let (with_bg, p_with_bg) = c.start("sleep 300 >/dev/null 2>&1 & echo $!; sleep 300");
    let (finished, p_finished) = c.start("sleep 300 >/dev/null 2>&1 & echo $!");
    let g_stubborn = wait_for_pids(c, &stubborn, 1)[0];
    let g_with_bg = wait_for_pids(c, &with_bg, 1)[0];
    let g_finished = wait_for_pids(c, &finished, 1)[0];
    assert_eq!(c.wait_terminal(&finished, S(3)).unwrap()["status"], "completed");
    let all_pids = vec![p_plain, p_stubborn, p_with_bg, p_finished, g_stubborn, g_with_bg, g_finished];
    assert!(all_pids.iter().all(|p| pid_running(*p)), "fixture not running: {all_pids:?}");
    CrashFixture {
        running: vec![(plain, p_plain), (stubborn, p_stubborn), (with_bg, p_with_bg)],
        finished,
        all_pids,
        term_ignoring: g_stubborn,
    }
}

/// After the daemon died without shutting down: everything is gone within
/// the runners' 2s grace (plus slack), the SIGTERM-ignoring grandchild only
/// after the grace, and the next daemon re-adopts nothing.
fn assert_crash_cleaned_up(home: &Home, f: &CrashFixture, crashed_at: Instant) {
    assert!(
        poll_true(MS(1500), || !pid_running(f.running[0].1)),
        "the lifeline did not take the plain task down"
    );
    if crashed_at.elapsed() < MS(1500) {
        assert!(pid_running(f.term_ignoring), "SIGTERM-ignoring grandchild died before the grace");
    }
    let gone = poll_true(S(6), || f.all_pids.iter().all(|p| !pid_running(*p)));
    let alive: Vec<u32> = f.all_pids.iter().copied().filter(|p| pid_running(*p)).collect();
    for p in &alive {
        kill_group(*p, libc::SIGKILL);
    }
    assert!(gone, "outlived the crashed manager: {alive:?}");

    let _d2 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-crash");
    for (id, _) in &f.running {
        let t = c.task(id).expect("listed after restart");
        assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("orphaned"), Some("manager-crash")), "{t}");
        assert_eq!(home.record(id).unwrap()["status"], "orphaned", "persisted");
    }
    assert_eq!(c.task(&f.finished).unwrap()["status"], "completed");
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(log.contains("orphaned=3 loaded=1"), "{log}");
}

/// D4/T7/T8: SIGKILL the daemon while tasks run. The manager is every
/// task's parent: when it ends by any means, every runner sees its lifeline
/// break and takes its process group down (SIGTERM, 2s, SIGKILL). There is
/// no crash recovery: the next daemon marks the running records orphaned
/// (manager-crash) and re-adopts nothing.
#[test]
fn d4_daemon_kill9_takes_every_task_down() {
    let home = Home::new("d4");
    let mut d1 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-crash");
    let f = start_crash_fixture(&mut c);
    d1.kill().unwrap();
    let crashed_at = Instant::now();
    d1.wait().unwrap();
    drop(c);
    assert_crash_cleaned_up(&home, &f, crashed_at);
}

/// D4c: the same when the daemon dies the way a panic in its main future
/// ends it (exit 101, no shutdown path). Test-clock builds only: the crash
/// is triggered by the `debug_crash` request.
#[cfg(feature = "test-clock")]
#[test]
fn d4c_daemon_crash_takes_every_task_down() {
    let home = Home::new("d4c");
    let mut d1 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-crash");
    let f = start_crash_fixture(&mut c);
    let mut k = Conn::new(UnixStream::connect(home.sock()).unwrap());
    k.send(&json!({"v":1,"id":"crash","type":"debug_crash"}));
    let crashed_at = Instant::now();
    let status = wait_child(&mut d1, S(5)).expect("daemon did not crash");
    assert_eq!(status.code(), Some(101), "{status:?}");
    drop(c);
    assert_crash_cleaned_up(&home, &f, crashed_at);
}

/// D4d: a manual-clock daemon exits once its test process is gone. Its idle
/// and handover-grace timers only move when a test advances them, so a
/// daemon left by a killed test binary (Ctrl-C, a timeout) would otherwise
/// hold its tasks forever. The owner here is a stand-in process the test
/// kills; its tasks then go down like after any crash.
#[cfg(feature = "test-clock")]
#[test]
fn d4d_manual_clock_daemon_exits_when_its_test_owner_is_gone() {
    let home = Home::new("d4d");
    let mut owner = KillOnDrop(Some(std::process::Command::new("/bin/sleep").arg("300").spawn().unwrap()));
    let owner_pid = owner.0.as_ref().unwrap().id().to_string();
    let mut d1 = home.start_daemon_from(std::path::Path::new(BIN), &[("PI_FAMULUS_TEST_OWNER", &owner_pid)]);
    let mut c = home.connect();
    c.hello_ext("sess-crash");
    let f = start_crash_fixture(&mut c);
    let mut o = owner.0.take().unwrap();
    o.kill().unwrap();
    o.wait().unwrap();
    let owner_gone_at = Instant::now();
    let status = wait_child(&mut d1, S(3)).expect("daemon outlived its test owner");
    assert!(!status.success(), "{status:?}");
    drop(c);
    assert_crash_cleaned_up(&home, &f, owner_gone_at);
}

/// D4b: a record left "running" is marked orphaned (manager-crash) at the
/// next startup, and its recorded pid is never signalled: pids are reused,
/// and here it names a live process the daemon never started.
/// Kills and reaps the child on every exit path, so a failing assertion
/// cannot leak the fixture's process.
struct KillOnDrop(Option<std::process::Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn d4b_startup_orphans_leftover_records_without_signalling() {
    let home = Home::new("d4b");
    let mut bystander = KillOnDrop(Some(std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap()));
    let pid = bystander.0.as_ref().unwrap().id();
    let dir = home.path.join("sessions/sess-a/tasks");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("sh_0000d4b1.output");
    std::fs::write(&out, "partial\n").unwrap();
    let rec = json!({"task_id":"sh_0000d4b1","session_id":"sess-a","kind":"shell","command":"sleep 30",
        "cwd":"/tmp","pid":pid,"status":"running","exit_code":null,"signal":null,
        "started_at":1,"ended_at":null,"output_path":out,"output_size":0});
    std::fs::write(dir.join("sh_0000d4b1.json"), serde_json::to_vec(&rec).unwrap()).unwrap();

    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let t = c.task("sh_0000d4b1").expect("listed");
    assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("orphaned"), Some("manager-crash")), "{t}");
    assert_eq!(t["output_size"], 8, "output size recovered from the file");
    // Watch the bystander over a window, not one instant: a scan signal
    // delivered late (or a daemon scheduled slowly under CI load) would
    // slip a bare sleep. Every wait here is a poll, per the suite rule.
    let deadline = Instant::now() + S(1);
    loop {
        let alive = bystander.0.as_mut().unwrap().try_wait().unwrap().is_none();
        assert!(alive, "the daemon signalled a pid from an old record");
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(MS(50));
    }
}

/// D25: the startup scan of a large home outlasts the client's wait for the
/// socket (9,671 records took over 2s on 2026-09-26). Invariant: the client
/// that spawned the daemon is served, not told "did not create its socket
/// within 2s" while the daemon then idles out with nobody connected.
#[test]
fn d25_slow_startup_scan_still_serves_the_spawning_client() {
    let home = Home::new("d25");
    let started = Instant::now();
    let out = run_cli_env(
        &home.path,
        &["start", "true"],
        S(20),
        &[("PI_FAMULUS_TEST_SLOW_SCAN_MS", "3000"), ("PI_FAMULUS_TEST_CLOCK", "")],
    );
    let elapsed = started.elapsed();
    assert!(out.status.success(), "stdout: {}\nstderr: {}", out.stdout, out.stderr);
    assert!(out.stdout.contains("task_id="), "{}", out.stdout);
    // PI_FAMULUS_TEST_SLOW_SCAN_MS is honored only under cfg!(debug_assertions), so
    // a release-built daemon would skip the pause and pass this test even if
    // the bind-before-scan ordering it guards had regressed. Requiring the
    // client to have waited near the full 3s proves the hook actually ran,
    // not just that starting a task succeeded quickly.
    assert!(elapsed >= S(2), "the slow-scan hook did not run (took {elapsed:?}); is this a release build?");
}

// ===========================================================================
// Connection loss -> 5s grace -> shutdown (C3, D5, D6, D7)
// ===========================================================================

/// C3/D5: the only extension client process is SIGKILLed. Invariant: nothing
/// is killed during the 5s grace; after it, every task (including a
/// SIGTERM-ignoring one and a grandchild) is dead, records say `killed`, and
/// manager.sock / manager.pid are removed.
#[test]
fn d5_client_crash_grace_then_kill_everything_and_clean_files() {
    let home = Home::new("d5");
    let mut daemon = home.start_daemon();
    let mut helper = HelperClient::spawn(
        &home.path,
        "sess-crash",
        &[
            "sleep 300",
            "trap '' TERM; echo armed; sleep 300",
            "sleep 300 & echo $!; wait",
        ],
    );
    assert_eq!(helper.tasks.len(), 3);
    let (stub_id, _) = helper.tasks[1].clone();
    let (gc_id, _) = helper.tasks[2].clone();
    let gc = {
        let mut cli = home.connect();
        cli.hello_cli();
        wait_output_contains(&mut cli, &stub_id, "armed");
        wait_for_pids(&mut cli, &gc_id, 1)[0]
    }; // cli connection closed again: the helper is the only connection
    let pids: Vec<u32> = helper.tasks.iter().map(|t| t.1).chain([gc]).collect();

    helper.crash();

    // During the grace window the daemon and all tasks are untouched.
    home.advance_almost("idle", 5000);
    assert!(daemon.try_wait().unwrap().is_none(), "daemon exited inside the 5s grace");
    for p in &pids {
        assert!(pid_running(*p), "pid {p} killed inside the 5s grace");
    }
    home.advance_past();
    // Shutdown: SIGTERM, then the TERM-ignoring task holds it for the 2s
    // kill grace.
    home.advance_almost("shutdown-grace", 2000);
    assert!(daemon.try_wait().unwrap().is_none(), "shutdown skipped the kill grace");
    home.advance_past();

    let st = wait_child(&mut daemon, S(12)).expect("daemon must exit after the grace");
    assert!(st.success(), "graceful exit status: {st:?}");
    for p in &pids {
        assert!(poll_true(S(1), || !pid_running(*p)), "pid {p} survived manager shutdown");
    }
    assert!(!home.sock().exists(), "manager.sock left behind");
    assert!(!home.pidfile().exists(), "manager.pid left behind");
    for (id, _) in &helper.tasks {
        assert_eq!(home.record(id).unwrap()["status"], "killed", "record {id}");
    }
}

/// D6: a new connection inside the grace cancels shutdown, and the grace
/// restarts from zero when that connection leaves too.
#[test]
fn d6_connection_inside_grace_cancels_shutdown() {
    let home = Home::new("d6");
    let mut daemon = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id, pid) = a.start("sleep 300");
    drop(a);

    home.advance("idle", 2500);
    let mut b = home.connect();
    assert_eq!(b.hello_cli()["ok"], true);
    // Well past the original deadline (5s) plus the 2s kill grace.
    home.advance_now(5000);
    settle();
    assert!(daemon.try_wait().unwrap().is_none(), "shutdown was not cancelled");
    assert!(pid_running(pid), "task killed although a client reconnected");
    assert_eq!(b.status_of(&id).as_deref(), Some("running"));

    // The grace restarts from zero when b leaves.
    drop(b);
    home.advance_almost("idle", 5000);
    assert!(daemon.try_wait().unwrap().is_none(), "grace did not restart from zero");
    assert!(pid_running(pid));
    home.advance_past();
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(12)).is_some(), "daemon must exit once idle again");
    assert!(poll_true(S(1), || !pid_running(pid)));
}

/// D6b: a brief reconnect inside the grace must restart the grace from the
/// moment that client leaves; the first (cancelled) countdown must not fire.
#[test]
fn d6b_brief_reconnect_restarts_grace() {
    let home = Home::new("d6b");
    let mut daemon = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (_, pid) = a.start("sleep 300");
    drop(a);
    home.advance("idle", 1000);
    let mut b = home.connect();
    b.hello_cli();
    home.advance_now(1000);
    drop(b);
    // The original countdown would fire 3s from here; the new one at 5s.
    home.advance_almost("idle", 5000);
    assert!(daemon.try_wait().unwrap().is_none(), "stale countdown shut the daemon down");
    assert!(pid_running(pid));
    home.advance_past();
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(10)).is_some());
}

/// C1/D7: a connection that never completes hello does not count as active,
/// so a stuck or hostile socket peer cannot keep tasks alive forever.
#[test]
fn d7_unhelloed_connection_does_not_hold_daemon_alive() {
    let home = Home::new("d7");
    let mut daemon = home.start_daemon();
    let _raw = UnixStream::connect(home.sock()).unwrap();
    home.advance_almost("idle", 5000);
    assert!(daemon.try_wait().unwrap().is_none(), "exited before the grace");
    home.advance_past();
    assert!(wait_child(&mut daemon, S(9)).is_some(), "daemon kept alive by a silent connection");
}

/// C5/D8: a hello that arrives while shutdown is in progress is refused (it
/// must neither be accepted nor cancel the shutdown).
#[test]
fn d8_hello_during_shutdown_is_refused_and_does_not_cancel() {
    let home = Home::new("d8");
    let mut daemon = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id, pid) = a.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut a, &id, "armed");

    // Accepted before shutdown, hello after.
    let mut late = home.connect();
    std::thread::sleep(MS(200));
    let out = home.cli(&["shutdown"], S(10));
    assert!(out.status.success(), "shutdown: {}", out.stderr);
    assert!(out.stdout.contains("shutting down"));
    // The SIGTERM-ignoring task holds shutdown in its 2s grace; say hello now.
    let hello = json!({"type":"hello","client_kind":"extension","session_id":"late","pi_pid":1});
    match late.try_request(hello.clone(), S(3)) {
        Some(r) => assert_eq!(r["ok"], false, "hello accepted during shutdown: {r}"),
        None => assert!(late.closed, "hello neither answered nor refused"),
    }
    // A connection made after shutdown began is still accepted, and its
    // hello is refused at once (not left to hang until the client gives up).
    let mut fresh = home.connect();
    let r = fresh
        .try_request(hello, S(2))
        .expect("a hello during shutdown must be answered promptly");
    assert_eq!(r["ok"], false, "hello accepted during shutdown: {r}");
    assert!(r["error"]["message"].as_str().unwrap_or("").contains("shutting down"), "{r}");
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(8)).is_some(), "late hello cancelled shutdown");
    assert!(!pid_running(pid));
    // The still-open extension connection did not block the explicit shutdown.
    drop(a);
}

/// D8b: a CLI command issued while the manager is shutting down does not
/// stall and does not fail: its hello is refused at once, it waits for the
/// old manager to exit, then spawns a successor.
#[test]
fn d8b_cli_during_shutdown_reaches_a_successor() {
    let home = Home::new("d8b");
    let mut daemon = home.start_daemon();
    let old = daemon.id();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id, _) = a.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut a, &id, "armed");
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    // The TERM-ignoring task holds shutdown in its 2s kill grace. (The
    // pause lets the daemon act on the shutdown it just acknowledged.)
    std::thread::sleep(MS(200));
    let path = home.path.clone();
    let clock = if home.manual { "manual" } else { "" };
    let ls = std::thread::spawn(move || run_cli_env(&path, &["ls"], S(20), &[("PI_FAMULUS_TEST_CLOCK", clock)]));
    // Let `ls` meet the shutting-down manager before the grace ends.
    std::thread::sleep(MS(500));
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(8)).is_some());
    let out = ls.join().unwrap();
    assert!(out.status.success(), "ls during shutdown failed: {}", out.stderr);
    let new = home.pidfile_pid().expect("a successor manager");
    assert_ne!(new, old);
    drop(a);
}

/// Regression (D8c, publish run 37439173913): shutdown can take longer
/// than the client's former 5s pid wait under scheduler/process-scan load.
/// Keep the real daemon in its manual grace for 6s: a CLI must wait for its
/// lifetime lock to release, then reach a successor, not spawn too early
/// and fail with E_INTERNAL "manager is shutting down".
#[cfg(feature = "test-clock")]
#[test]
fn d8c_cli_waits_for_slow_shutdown_before_spawning_successor() {
    let home = Home::new("d8c");
    let mut daemon = home.start_daemon();
    let old = daemon.id();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id, _) = a.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut a, &id, "armed");
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    assert!(poll_true(S(3), || {
        clock_request(&home.path, json!({"type":"clock_status"}))["pending"]
            .as_array()
            .is_some_and(|p| p.iter().any(|t| t["label"] == "shutdown-grace"))
    }), "shutdown grace was not armed");

    // Observe the CLI's actual hello before starting the 6s interval.
    // Intentionally delay launch beyond 6s: the former spawn-based interval
    // would release shutdown before this CLI ever met the retiring daemon.
    let retiring = home.path.join("retiring.sock");
    std::fs::rename(home.sock(), &retiring).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(home.sock()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut ls = CliGuard(Some(std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 7; exec \"$@\"", "delayed-cli", BIN])
        .args(["--home", home.path.to_str().unwrap(), "ls"])
        .env("PI_FAMULUS_TEST_CLOCK", "manual")
        .env("PI_FAMULUS_TEST_OWNER", test_owner())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap()));
    let (socket, _) = poll_until(S(10), || listener.accept().ok()).expect("CLI did not connect");
    let mut proxy = Conn::new(socket);
    let hello = match proxy.recv(Instant::now() + S(3)) {
        Recv::Frame(f) => f,
        _ => panic!("CLI did not send hello"),
    };
    assert_eq!(hello["type"], "hello");
    let mut upstream = Conn::new(UnixStream::connect(&retiring).unwrap());
    upstream.send(&hello);
    let refusal = match upstream.recv(Instant::now() + S(3)) {
        Recv::Frame(f) => f,
        _ => panic!("retiring daemon did not answer hello"),
    };
    assert_eq!(refusal["error"], json!({"code":"E_INTERNAL","message":"manager is shutting down"}));
    proxy.send(&refusal);
    drop(proxy);
    drop(listener);
    std::fs::remove_file(home.sock()).unwrap();
    std::fs::rename(&retiring, home.sock()).unwrap();

    // Intentionally hold a lifecycle boundary, not a readiness sleep. The
    // old code exits after 5s even though the daemon is still shutting down.
    let exited_early = poll_true(S(6), || ls.0.as_mut().unwrap().try_wait().unwrap().is_some());
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(8)).is_some());
    assert!(wait_child(ls.0.as_mut().unwrap(), S(10)).is_some(), "CLI did not finish after shutdown");
    let out = ls.0.take().unwrap().wait_with_output().unwrap();
    assert!(!exited_early && out.status.success(),
        "CLI failed before reaching a successor: {}", String::from_utf8_lossy(&out.stderr));
    assert_ne!(home.pidfile_pid().expect("a successor manager"), old);
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(!log.contains("already running"), "spawn attempted before shutdown completed: {log}");
    drop(a);
}

/// Invariant (D8d): a shutdown that never releases its lifetime lock must
/// hit the startup deadline, without launching a competing daemon.
#[cfg(feature = "test-clock")]
#[test]
fn d8d_cli_bounds_wait_for_stuck_shutdown_without_spawning() {
    let home = Home::new("d8d");
    let mut daemon = home.start_daemon();
    let old = daemon.id();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id, _) = a.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut a, &id, "armed");
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    assert!(poll_true(S(3), || {
        clock_request(&home.path, json!({"type":"clock_status"}))["pending"]
            .as_array()
            .is_some_and(|p| p.iter().any(|t| t["label"] == "shutdown-grace"))
    }), "shutdown grace was not armed");

    // Do not advance the shutdown timer until the CLI has timed out. The
    // harness bound must exceed the CLI's whole-flow startup deadline (60s,
    // see client::connect) so the CLI gives up on its own — killing it here
    // would no longer prove boundedness.
    let out = home.cli(&["ls"], S(90));
    assert!(!out.status.success(), "CLI unexpectedly passed a stuck shutdown");
    assert!(out.stderr.contains("manager did not become ready within 60s"), "{}", out.stderr);
    assert_eq!(home.pidfile_pid(), Some(old));
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(!log.contains("already running"), "spawn attempted during stuck shutdown: {log}");
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(8)).is_some());
    drop(a);
}

/// Regression (D8e): concurrent shutdown waiters may miss the brief free
/// lifetime-lock interval when one client starts the successor. Every
/// waiter must use that healthy successor, not wait on its new lock.
#[cfg(feature = "test-clock")]
#[test]
fn d8e_concurrent_shutdown_waiters_share_the_successor() {
    let home = Home::new("d8e");
    let mut daemon = home.start_daemon();
    let old = daemon.id();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id, _) = a.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut a, &id, "armed");
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    assert!(poll_true(S(3), || {
        clock_request(&home.path, json!({"type":"clock_status"}))["pending"]
            .as_array()
            .is_some_and(|p| p.iter().any(|t| t["label"] == "shutdown-grace"))
    }), "shutdown grace was not armed");

    // Proxy one real refusal. Freeze B before delivering it so B is
    // guaranteed to miss the unlocked interval while A starts a successor.
    let retiring = home.path.join("retiring.sock");
    std::fs::rename(home.sock(), &retiring).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(home.sock()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut b = CliGuard(Some(std::process::Command::new(BIN)
        .arg("--home").arg(&home.path).arg("ls")
        .env("PI_FAMULUS_TEST_CLOCK", "manual")
        .env("PI_FAMULUS_TEST_OWNER", test_owner())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap()));
    let (socket, _) = poll_until(S(3), || listener.accept().ok()).expect("B did not connect");
    let mut proxy = Conn::new(socket);
    let hello = match proxy.recv(Instant::now() + S(3)) {
        Recv::Frame(f) => f,
        _ => panic!("B did not send hello"),
    };
    assert_eq!(hello["type"], "hello");
    let mut upstream = Conn::new(UnixStream::connect(&retiring).unwrap());
    upstream.send(&hello);
    let refusal = match upstream.recv(Instant::now() + S(3)) {
        Recv::Frame(f) => f,
        _ => panic!("retiring daemon did not answer hello"),
    };
    assert_eq!(refusal["error"], json!({"code":"E_INTERNAL","message":"manager is shutting down"}));
    kill_pid(b.0.as_ref().unwrap().id(), libc::SIGSTOP);
    proxy.send(&refusal);
    drop(proxy);
    drop(listener);
    std::fs::remove_file(home.sock()).unwrap();
    std::fs::rename(&retiring, home.sock()).unwrap();

    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(8)).is_some());
    assert!(home.cli(&["ls"], S(10)).status.success(), "A could not start a successor");
    let new = home.pidfile_pid().expect("a successor manager");
    assert_ne!(new, old);
    let mut check = home.connect();
    assert_eq!(check.hello_cli()["pid"], new);
    kill_pid(b.0.as_ref().unwrap().id(), libc::SIGCONT);
    assert!(wait_child(b.0.as_mut().unwrap(), S(20)).is_some(), "B did not finish");
    let out = b.0.take().unwrap().wait_with_output().unwrap();
    assert!(out.status.success(), "B waited on a healthy successor: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(home.pidfile_pid(), Some(new), "B must use A's successor");
    drop(a);
}

/// Regression (D8f): a socket-readiness probe can reach a retiring daemon,
/// so the second hello can refuse shutdown too. Also exercise an unrelated
/// E_INTERNAL mentioning shutdown: only the exact refusal is retryable.
#[test]
fn d8f_cli_retries_shutdown_refusal_on_the_post_spawn_hello() {
    let home = Home::new("d8f");
    let path = home.path.clone();
    let blocked = home.path.join("blocked-lock-attempts");
    let blocked_for_peer = blocked.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new().create(true).truncate(false)
            .write(true).open(path.join("manager.lock")).unwrap();
        let mut lock = fd_lock::RwLock::new(file);
        let guard = lock.try_write().unwrap();
        let sock = path.join("manager.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        ready_tx.send(()).unwrap();
        let mut hellos = 0;
        while hellos < 2 {
            let (socket, _) = poll_until(S(5), || listener.accept().ok()).expect("CLI did not reconnect");
            let mut conn = Conn::new(socket);
            let hello = match conn.recv(Instant::now() + S(3)) {
                Recv::Closed => continue, // socket-readiness probe, no hello
                Recv::Frame(f) => f,
                Recv::Timeout => panic!("CLI did not send hello"),
            };
            assert_eq!(hello["type"], "hello");
            hellos += 1;
            let message = if hellos == 1 {
                "unrelated failure mentions manager is shutting down"
            } else {
                "manager is shutting down"
            };
            conn.send(&json!({"v":1,"id":hello["id"],"ok":false,
                "error":{"code":"E_INTERNAL","message":message}}));
        }
        // The unrelated first error took the ordinary spawn path. Keep the
        // fake owner's lock until all bounded WouldBlock attempts are observed;
        // scheduler time is not evidence that the retry policy completed.
        assert!(
            poll_true(S(15), || blocked_lock_attempts(&blocked_for_peer)
                == DAEMON_LOCK_RETRY_ATTEMPTS),
            "first error did not reach bounded spawn refusal; observed {} lock attempts",
            blocked_lock_attempts(&blocked_for_peer)
        );
        assert_eq!(blocked_lock_attempts(&blocked_for_peer), DAEMON_LOCK_RETRY_ATTEMPTS);
        assert!(!lifetime_lock_free_path(&path), "fake peer must still own manager.lock");
        drop(listener);
        std::fs::remove_file(sock).unwrap();
        drop(guard);
    });
    ready_rx.recv_timeout(S(5)).unwrap();
    let blocked_text = blocked.to_str().expect("test home path is UTF-8");
    let clock = if home.manual { "manual" } else { "" };
    let out = run_cli_env(
        &home.path,
        &["ls"],
        S(20),
        &[
            ("PI_FAMULUS_TEST_CLOCK", clock),
            ("PI_FAMULUS_TEST_DAEMON_LOCK_BLOCKED", blocked_text),
        ],
    );
    peer.join().unwrap();
    assert!(out.status.success(), "post-spawn shutdown refusal failed: {}", out.stderr);
    assert_eq!(blocked_lock_attempts(&blocked), DAEMON_LOCK_RETRY_ATTEMPTS);
    assert!(std::fs::read_to_string(home.path.join("manager.log"))
        .unwrap_or_default()
        .contains("already running"), "contended spawn did not refuse before peer lock release");
    let new = home.pidfile_pid().expect("a real successor manager");
    let mut check = home.connect();
    assert_eq!(check.hello_cli()["pid"], new);
}

/// D9: explicit `shutdown` with tasks running and an extension still
/// connected: tasks killed (SIGKILL escalation included), records `killed`,
/// files removed.
#[test]
fn d9_shutdown_command_kills_tasks_and_cleans_files() {
    let home = Home::new("d9");
    let mut daemon = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (id1, p1) = a.start("sleep 300");
    let (id2, p2) = a.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut a, &id2, "armed");
    let out = home.cli(&["shutdown"], S(10));
    assert!(out.status.success());
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(8)).is_some(), "daemon must exit after shutdown");
    assert!(!pid_running(p1) && !pid_running(p2));
    assert!(!home.sock().exists() && !home.pidfile().exists());
    assert_eq!(home.record(&id1).unwrap()["status"], "killed");
    assert_eq!(home.record(&id2).unwrap()["status"], "killed");
    // Extension sees the connection go away (it can then respawn a manager).
    assert!(a.wait_closed(S(3)));
}

// ===========================================================================
// Stale socket / pid files (D10, D11, D12)
// ===========================================================================

/// D10: stale socket + pid file of a SIGKILLed daemon. A CLI call that
/// auto-spawns (`ls`) recovers: a new daemon takes over and serves.
#[test]
fn d10_client_recovers_from_dead_daemon_files() {
    let home = Home::new("d10");
    let out = home.cli(&["ls"], S(10));
    assert!(out.status.success());
    let old = home.pidfile_pid().unwrap();
    kill_pid(old, libc::SIGKILL);
    assert!(poll_true(S(3), || !pid_running(old)));
    assert!(home.sock().exists() && home.pidfile().exists());

    let out = home.cli(&["ls"], S(10));
    assert!(out.status.success(), "{}", out.stderr);
    let new = home.pidfile_pid().unwrap();
    assert_ne!(new, old);
    let mut c = home.connect();
    assert_eq!(c.hello_cli()["pid"].as_u64(), Some(new as u64), "new daemon serves the socket");
}

/// D11: a dead socket file with no pid file (e.g. pid file deleted by hand).
#[test]
fn d11_client_recovers_from_socket_without_pidfile() {
    let home = Home::new("d11");
    // Not a plain bind+drop: see `dead_socket` (that flaked in the suite).
    dead_socket(&home.sock()); // what a SIGKILLed daemon leaves behind
    assert!(home.sock().exists());
    let out = home.cli(&["ls"], S(10));
    assert!(out.status.success(), "{}", out.stderr);
    assert!(home.pidfile_pid().map(pid_running).unwrap_or(false));
}

/// D11b: garbage in manager.pid is treated as "no pid file".
#[test]
fn d11b_client_recovers_from_corrupt_pidfile() {
    let home = Home::new("d11b");
    std::fs::write(home.pidfile(), b"{not json").unwrap();
    let out = home.cli(&["ls"], S(10));
    assert!(out.status.success(), "{}", out.stderr);
}

/// D12: manager.pid names a pid that is alive but is NOT a pi-famulus (pid
/// reuse after a crash/reboot). The manager must still come up.
#[test]
fn d12_reused_pid_in_pidfile_does_not_block_startup() {
    let home = Home::new("d12");
    let mut impostor = std::process::Command::new("sleep").arg("300").spawn().unwrap();
    std::fs::write(
        home.pidfile(),
        format!(r#"{{"pid":{},"version":"0.1.0","started_at":0}}"#, impostor.id()),
    )
    .unwrap();
    let out = home.cli(&["ls"], S(15));
    let _ = impostor.kill();
    let _ = impostor.wait();
    assert!(out.status.success(), "manager blocked by reused pid: {}", out.stderr);
}

/// A transient exclusive manager.lock owner can overlap a successor's first
/// claim after SIGKILL. The successor must keep retrying the actual lock,
/// leaving the stale pid/socket untouched until it acquires ownership.
#[test]
fn stale_files_recover_after_transient_lifetime_lock_contention() {
    // Keep the macOS sockaddr path short: its sun_path limit is much smaller
    // than Linux's and the runner's temporary directory already uses many bytes.
    let home = Home::new("slc");
    let mut original = home.start_daemon();
    let old_pid = original.id();
    kill_pid(old_pid, libc::SIGKILL);
    assert!(
        wait_child(&mut original, S(5)).is_some(),
        "killed daemon was not reaped"
    );
    assert_eq!(home.pidfile_pid(), Some(old_pid));
    assert!(
        home.sock().exists(),
        "SIGKILL should leave the stale socket"
    );

    // This is the same exclusive flock as a paused lifecycle::lock_held
    // probe, held in a different process from the successor daemon.
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.path.join("manager.lock"))
        .unwrap();
    let mut lifecycle_lock = fd_lock::RwLock::new(lock_file);
    let guard = lifecycle_lock.try_write().unwrap();
    let blocked = home.path.join("blocked-lock-attempts");
    let barrier_path = home.path.join("b.sock");
    let listener = UnixListener::bind(&barrier_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut successor = KillOnDrop(Some(spawn_lock_observer_daemon(
        &home,
        &blocked,
        Some(&barrier_path),
    )));

    // The debug hook sends an ACK on its first WouldBlock and blocks reading
    // this socket. The retry cannot proceed until after we release manager.lock
    // and send the resume byte, regardless of how slowly this test is scheduled.
    let (mut barrier, _) = poll_until(S(3), || listener.accept().ok())
        .expect("successor did not observe the held manager.lock");
    barrier.set_read_timeout(Some(S(3))).unwrap();
    barrier.set_write_timeout(Some(S(3))).unwrap();
    let mut observed = [0_u8; 1];
    barrier.read_exact(&mut observed).unwrap();
    assert_eq!(observed, [b'x']);
    assert_eq!(blocked_lock_attempts(&blocked), 1);
    assert_eq!(
        home.pidfile_pid(),
        Some(old_pid),
        "contender cleaned up the stale pid"
    );
    assert!(
        home.sock().exists(),
        "contender unlinked the stale socket without ownership"
    );
    assert!(
        UnixStream::connect(home.sock()).is_err(),
        "stale socket unexpectedly serves"
    );

    drop(guard);
    barrier.write_all(b"x").unwrap();
    drop(barrier);
    drop(listener);
    assert!(
        poll_true(S(8), || {
            home.pidfile_pid().is_some_and(|pid| pid != old_pid)
                && UnixStream::connect(home.sock()).is_ok()
        }),
        "successor did not claim the released lifetime lock"
    );
    let new_pid = home.pidfile_pid().unwrap();
    let mut client = home.connect();
    assert_eq!(client.hello_cli()["pid"], new_pid);
    assert_ne!(new_pid, old_pid);
    assert!(successor.0.as_mut().unwrap().try_wait().unwrap().is_none());
    drop(client);

    assert!(home.cli(&["shutdown"], S(10)).status.success());
    assert!(wait_child(successor.0.as_mut().unwrap(), S(10)).is_some());
}

/// A real daemon that owns manager.lock still wins after the bounded retry
/// window: a contender exits with its informational pid and never binds or
/// replaces the owner's socket/pid files.
#[test]
fn real_lifetime_lock_owner_gets_bounded_refusal_without_second_server() {
    let home = Home::new("rlo");
    let mut owner = home.start_daemon();
    let owner_pid = owner.id();
    // Keep the legitimate owner active while the contender exhausts retries,
    // so the test always checks a live lock owner rather than idle shutdown.
    let mut owner_client = home.connect();
    assert_eq!(owner_client.hello_cli()["pid"], owner_pid);
    let blocked = home.path.join("blocked-lock-attempts");
    let mut duplicate = KillOnDrop(Some(spawn_lock_observer_daemon(&home, &blocked, None)));

    // The bounded policy is the retry cap, not a wall-clock promise that can
    // be distorted by process scheduling on a busy CI runner. Keep an outer
    // safety deadline only to catch a stuck contender.
    let status = wait_child(duplicate.0.as_mut().unwrap(), S(15))
        .expect("contender did not refuse before the safety deadline");
    assert!(status.success(), "duplicate daemon exited with {status}");
    assert_eq!(
        blocked_lock_attempts(&blocked),
        DAEMON_LOCK_RETRY_ATTEMPTS,
        "real owner must keep the contender blocked through the retry cap"
    );
    let out = duplicate.0.take().unwrap().wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("already running (pid {owner_pid})")),
        "{stdout:?}"
    );
    assert_eq!(home.pidfile_pid(), Some(owner_pid));
    assert_eq!(
        daemon_pids_for(&home.path),
        vec![owner_pid],
        "a second daemon survived"
    );
    let mut client = home.connect();
    assert_eq!(client.hello_cli()["pid"], owner_pid);
    assert!(home.sock().exists(), "owner's socket was removed");

    assert!(home.cli(&["shutdown"], S(10)).status.success());
    assert!(wait_child(&mut owner, S(10)).is_some());
}

// ===========================================================================
// Task stop / kill semantics (T3, T4, T5, T6, T9)
// ===========================================================================

/// T4: `stop` sends SIGTERM first — a task with a TERM handler gets to run it.
#[test]
fn t4_stop_sends_sigterm_first() {
    let home = Home::new("t4");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("trap 'echo got-term; exit 7' TERM; echo armed; while :; do sleep 0.05; done");
    wait_output_contains(&mut c, &id, "armed");
    c.request_ok(json!({"type":"stop","task_id":id}));
    let t = c.wait_terminal(&id, S(4)).expect("terminal after stop");
    assert_eq!(t["status"], "killed", "stop always ends as killed: {t}");
    wait_output_contains(&mut c, &id, "got-term");
}

/// T5: a task ignoring SIGTERM survives the 2s grace, then is SIGKILLed.
#[test]
fn t5_sigterm_ignoring_task_is_sigkilled_after_grace() {
    let home = Home::new("t5");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, pid) = c.start("trap '' TERM; echo armed; sleep 300");
    wait_output_contains(&mut c, &id, "armed");
    let r = c.request_ok(json!({"type":"stop","task_id":id}));
    assert_eq!(r["ok"], true);
    home.advance_almost("kill-grace", 2000);
    assert!(pid_running(pid), "SIGKILL arrived before the 2s grace");
    assert_eq!(c.status_of(&id).as_deref(), Some("running"));
    home.advance_past();
    let t = c.wait_terminal(&id, S(5)).expect("SIGKILL escalation");
    assert_eq!(t["status"], "killed");
    assert!(!pid_running(pid));
    let ev = c
        .wait_event(S(2), |e| e["event"] == "task_exited" && e["task_id"] == json!(id))
        .expect("task_exited");
    assert!(ev["exit_code"].is_null(), "{ev}");
    assert!(!ev["signal"].is_null(), "killed task_exited must carry the signal: {ev}");
}

/// T5b: §3.3 says killed task_exited carries `signal:"SIGTERM"|"SIGKILL"`;
/// the extension types it as `string | null`.
#[test]
fn t5b_signal_field_is_signal_name() {
    let home = Home::new("t5b");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("sleep 300");
    c.request_ok(json!({"type":"stop","task_id":id}));
    let ev = c
        .wait_event(S(5), |e| e["event"] == "task_exited" && e["task_id"] == json!(id))
        .expect("task_exited");
    assert!(
        ev["signal"] == "SIGTERM" || ev["signal"] == "SIGKILL",
        "signal should be a name: {ev}"
    );
}

/// R1: tasks run under `pi-famulus __run`, and the record still carries
/// the command's own status: its exit code, the signal it died of, and for
/// a stop the SIGTERM that ended it (not anything about the runner).
#[test]
fn r1_runner_reports_the_commands_real_status() {
    let home = Home::new("r1");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (three, _) = c.start("exit 3");
    let t = c.wait_terminal(&three, S(3)).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64(), t["signal"].as_str()), (Some("failed"), Some(3), None), "{t}");
    let (killed, _) = c.start("kill -KILL $$");
    let t = c.wait_terminal(&killed, S(3)).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64(), t["signal"].as_str()), (Some("failed"), None, Some("SIGKILL")), "{t}");
    let (stopped, _) = c.start("echo armed; sleep 300");
    wait_output_contains(&mut c, &stopped, "armed");
    c.request_ok(json!({"type":"stop","task_id":stopped}));
    let t = c.wait_terminal(&stopped, S(3)).unwrap();
    assert_eq!(
        (t["status"].as_str(), t["signal"].as_str(), t["end_reason"].as_str()),
        (Some("killed"), Some("SIGTERM"), Some("stopped:tool")),
        "{t}"
    );
}

/// T6: stop reaches grandchildren through the process group.
#[test]
fn t6_stop_kills_grandchildren() {
    let home = Home::new("t6");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, pid) = c.start("sleep 300 & echo $!; (sleep 300 & echo $!; wait) & wait");
    let gcs = wait_for_pids(&mut c, &id, 2);
    for g in &gcs {
        assert!(pid_running(*g));
    }
    c.request_ok(json!({"type":"stop","task_id":id}));
    assert!(
        poll_true(S(4), || !pid_running(pid) && gcs.iter().all(|g| !pid_running(*g))),
        "grandchildren {gcs:?} survived stop"
    );
    assert_eq!(c.wait_terminal(&id, S(3)).unwrap()["status"], "killed");
}

/// T6b: leader dies on SIGTERM but a grandchild ignores it. The group must
/// still be SIGKILLed after the grace.
#[test]
fn t6b_stop_kills_term_ignoring_grandchild_after_leader_exits() {
    let home = Home::new("t6b");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("sh -c \"trap '' TERM; echo armed; exec sleep 300\" & echo $!; wait");
    wait_output_contains(&mut c, &id, "armed");
    let gc = wait_for_pids(&mut c, &id, 1)[0];
    c.request_ok(json!({"type":"stop","task_id":id}));
    assert_eq!(c.wait_terminal(&id, S(3)).unwrap()["status"], "killed");
    home.advance_almost("kill-grace", 2000);
    assert!(pid_running(gc), "grandchild SIGKILLed before the grace");
    home.advance_past();
    let dead = poll_true(S(5), || !pid_running(gc));
    kill_pid(gc, libc::SIGKILL);
    assert!(dead, "grandchild {gc} ignored SIGTERM and was never SIGKILLed");
}

/// T6c: a task that backgrounds a child and exits. §3.2: background work must
/// not outlive the last pi, so manager shutdown must reach the leftover group.
#[test]
fn t6c_shutdown_kills_leftover_group_of_exited_task() {
    let home = Home::new("t6c");
    let mut daemon = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("sleep 300 >/dev/null 2>&1 & echo $!");
    let gc = wait_for_pids(&mut c, &id, 1)[0];
    assert_eq!(c.wait_terminal(&id, S(3)).unwrap()["status"], "completed");
    assert!(pid_running(gc));
    drop(c);
    home.advance("idle", 5000);
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(12)).is_some());
    let dead = poll_true(S(2), || !pid_running(gc));
    kill_pid(gc, libc::SIGKILL);
    assert!(dead, "grandchild {gc} outlived the manager");
}

/// T6d: `stop` on a task that already completed but left a background child
/// kills the leftover group (TERM, then KILL after the grace for a child
/// that ignores TERM); the task's recorded status stays `completed`.
/// shutdown_session reaches leftover groups too, without reporting them.
#[test]
fn t6d_stop_and_shutdown_session_reach_leftover_group() {
    let home = Home::new("t6d");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (a, _) = c.start("sh -c \"trap '' TERM; exec sleep 300\" >/dev/null 2>&1 & echo $!");
    let (b, _) = c.start("sleep 300 >/dev/null 2>&1 & echo $!");
    let ga = wait_for_pids(&mut c, &a, 1)[0];
    let gb = wait_for_pids(&mut c, &b, 1)[0];
    assert_eq!(c.wait_terminal(&a, S(3)).unwrap()["status"], "completed");
    assert_eq!(c.wait_terminal(&b, S(3)).unwrap()["status"], "completed");
    assert!(pid_running(ga) && pid_running(gb));

    c.request_ok(json!({"type":"stop","task_id":a}));
    home.advance_almost("kill-grace", 2000);
    assert!(pid_running(ga), "TERM-ignoring leftover killed before the grace");
    home.advance_past();
    let dead = poll_true(S(4), || !pid_running(ga));
    kill_pid(ga, libc::SIGKILL);
    assert!(dead, "leftover child {ga} survived stop");
    assert_eq!(c.status_of(&a).as_deref(), Some("completed"), "status must not change");

    let r = c.request_ok(json!({"type":"shutdown_session"}));
    assert_eq!(r["stopped"], json!([]), "only running tasks are reported: {r}");
    let dead = poll_true(S(3), || !pid_running(gb));
    kill_pid(gb, libc::SIGKILL);
    assert!(dead, "leftover child {gb} survived shutdown_session");
    assert_eq!(c.status_of(&b).as_deref(), Some("completed"));
}

/// T3: timeout_ms is a hard ceiling -> killed; a task finishing before its
/// ceiling is not affected.
#[test]
fn t3_timeout_ms_hard_kill() {
    let home = Home::new("t3");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (slow, pid) = c.start_with(json!({"type":"start","kind":"shell","command":"sleep 300","cwd":"/tmp",
        "env":{"PATH":PATH_ENV},"timeout_ms":300}));
    let (fast, _) = c.start_with(json!({"type":"start","kind":"shell","command":"echo ok","cwd":"/tmp",
        "env":{"PATH":PATH_ENV},"timeout_ms":5000}));
    let t = c.wait_terminal(&slow, S(3)).expect("timeout must kill");
    assert_eq!(t["status"], "killed", "{t}");
    assert!(!pid_running(pid));
    let f = c.wait_terminal(&fast, S(3)).unwrap();
    assert_eq!(f["status"], "completed", "{f}");
    assert_eq!(f["exit_code"], 0);
}

/// T1/T2: exit mapping observed on the wire: exit 0 -> completed, exit N ->
/// failed(N), killed by an outside signal -> failed with signal.
#[test]
fn t2_exit_status_mapping() {
    let home = Home::new("t2");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (ok, _) = c.start("true");
    let (bad, _) = c.start("exit 3");
    let (termish, _) = c.start("exit 143");
    let (killish, _) = c.start("exit 137");
    let (sig, _) = c.start("kill -KILL $$");
    let r = c.wait_terminal(&ok, S(3)).unwrap();
    assert_eq!((r["status"].as_str(), r["exit_code"].as_i64()), (Some("completed"), Some(0)));
    let r = c.wait_terminal(&bad, S(3)).unwrap();
    assert_eq!((r["status"].as_str(), r["exit_code"].as_i64()), (Some("failed"), Some(3)));
    for (id, code) in [(&termish, 143i64), (&killish, 137)] {
        let r = c.wait_terminal(id, S(3)).unwrap();
        assert_eq!((r["status"].as_str(), r["exit_code"].as_i64()), (Some("failed"), Some(code)), "{r}");
        assert!(r["signal"].is_null(), "{r}");
    }
    let r = c.wait_terminal(&sig, S(3)).unwrap();
    assert_eq!(r["status"], "failed", "{r}");
    assert!(r["exit_code"].is_null() && r["signal"] == "SIGKILL", "{r}");
    let w = c.request_ok(json!({"type":"wait","task_id":bad,"budget_ms":100}));
    assert_eq!((w["done"].as_bool(), w["exit_code"].as_i64()), (Some(true), Some(3)));
    // Records on disk agree with the wire.
    assert_eq!(home.record(&bad).unwrap()["status"], "failed");
    // Timestamps are real epoch milliseconds, ended after started.
    let rec = home.record(&ok).unwrap();
    let (started, ended) = (rec["started_at"].as_u64().unwrap(), rec["ended_at"].as_u64().unwrap());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(started > now - 60_000 && started <= ended && ended <= now + 1000, "{rec}");
}

// ===========================================================================
// Sessions: rebind, disconnect, reattach (C4, S2, S3)
// ===========================================================================

/// C4: duplicate hello for the same session: the old connection gets
/// `session_rebound` and is closed; the new one owns the session's tasks and
/// events.
#[test]
fn c4_duplicate_hello_rebinds_session() {
    let home = Home::new("c4");
    let _d = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-dup");
    let (id, _) = a.start("sleep 300");

    let mut b = home.connect();
    assert_eq!(b.hello_ext("sess-dup")["ok"], true);
    assert!(
        a.wait_event(S(3), |e| e["event"] == "session_rebound").is_some(),
        "old connection must get session_rebound"
    );
    assert!(a.wait_closed(S(3)), "old connection must be closed by the server");

    assert_eq!(b.status_of(&id).as_deref(), Some("running"));
    b.request_ok(json!({"type":"stop","task_id":id}));
    assert!(
        b.wait_event(S(5), |e| e["event"] == "task_exited" && e["task_id"] == json!(id)).is_some(),
        "events for the session must follow the new connection"
    );
    let mut cli = home.connect();
    cli.hello_cli();
    let st = cli.request_ok(json!({"type":"status"}));
    let sessions: Vec<_> = st["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["session_id"] == "sess-dup")
        .collect();
    assert_eq!(sessions.len(), 1, "{st}");
    assert_eq!(sessions[0]["connected"], true);
}

/// S2/S3: pi crashes while another pi keeps the manager alive: its tasks keep
/// running past the grace, the session shows disconnected, and a resumed pi
/// (same session id) re-attaches and receives the task's events.
#[test]
fn s3_session_survives_crash_while_others_connected_and_reattaches() {
    let home = Home::new("s3");
    let _d = home.start_daemon();
    let mut keeper = home.connect();
    keeper.hello_ext("keeper");
    let mut helper = HelperClient::spawn(&home.path, "resumable", &["sleep 300"]);
    let (id, pid) = helper.tasks[0].clone();
    helper.crash();

    let mut cli = home.connect();
    cli.hello_cli();
    assert!(
        poll_true(S(3), || {
            let st = cli.request_ok(json!({"type":"status"}));
            st["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["session_id"] == "resumable" && s["connected"] == false)
        }),
        "crashed session must show disconnected"
    );
    drop(cli);
    home.advance_now(6000); // longer than the 5s idle grace
    settle();
    assert!(pid_running(pid), "task of a crashed session killed while another pi is connected");

    let mut resumed = home.connect();
    resumed.hello_ext("resumable");
    assert_eq!(resumed.status_of(&id).as_deref(), Some("running"));
    resumed.request_ok(json!({"type":"stop","task_id":id}));
    assert!(resumed
        .wait_event(S(5), |e| e["event"] == "task_exited" && e["task_id"] == json!(id))
        .is_some());
    // Another session cannot see it.
    let r = keeper.request(json!({"type":"stop","task_id":id}));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_FORBIDDEN");
}

// ===========================================================================
// Backpressure, output, framing (C6, O1-O4, F1-F3)
// ===========================================================================

// C6's real-time latency/RSS canary lives in backpressure.rs so it runs in
// a separate harness from this suite's deliberate CPU saturation (D2).

/// O1: multi-MB output round-trips exactly through cursor reads; server caps
/// a single read so a response never exceeds the 4 MiB frame.
#[test]
fn o1_huge_output_roundtrip() {
    let home = Home::new("o1");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    const N: u64 = 20_000_000;
    let (id, _) = c.start(&format!("head -c {N} /dev/zero | tr '\\0' 'a'"));
    let w = c
        .try_request(json!({"type":"wait","task_id":id,"budget_ms":60000}), S(65))
        .expect("wait response");
    assert_eq!(w["done"], true);
    let big = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":64u64<<20}));
    let len = big["chunk"].as_str().unwrap().len() as u64;
    assert!(len > 0 && len <= 1 << 20, "single read returned {len} bytes");
    let (text, cursor, last) = c.read_all_output(&id, 1 << 20);
    assert_eq!(text.len() as u64, N);
    assert!(text.bytes().all(|b| b == b'a'));
    assert_eq!(cursor, N);
    assert_eq!(last["total_size"], N);
    let rec = c.task(&id).unwrap();
    assert_eq!(rec["output_size"], N, "{rec}");
    assert_eq!(std::fs::metadata(rec["output_path"].as_str().unwrap()).unwrap().len(), N);
    // Reading past EOF is empty, not an error.
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":N+10,"max_bytes":100}));
    assert_eq!(r["chunk"], "");
    assert_eq!(r["next_cursor"], N + 10);
}

/// O2: invalid UTF-8 becomes U+FFFD, but cursors stay byte offsets.
#[test]
fn o2_invalid_utf8_is_lossy_with_byte_cursors() {
    let home = Home::new("o2");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("printf 'ok\\377\\376end\\n'");
    c.request_ok(json!({"type":"wait","task_id":id,"budget_ms":3000}));
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":65536}));
    assert_eq!(r["chunk"], "ok\u{FFFD}\u{FFFD}end\n");
    assert_eq!(r["next_cursor"], 8);
    assert_eq!(r["total_size"], 8);
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":3,"max_bytes":2}));
    assert_eq!(r["chunk"], "\u{FFFD}e");
    assert_eq!(r["next_cursor"], 5);
}

/// O3: output made of JSON-escaped control bytes expands ~6x when framed; a
/// max-size read must still produce a response instead of silently killing
/// the connection's writer.
#[test]
fn o3_control_byte_output_large_read_still_answers() {
    let home = Home::new("o3");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("head -c 1048576 /dev/zero | tr '\\0' '\\001'");
    c.request_ok(json!({"type":"wait","task_id":id,"budget_ms":10000}));
    let r = c.try_request(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":1048576}), S(5));
    let r = r.expect("no response to a 1 MiB output read of control bytes");
    // The read succeeds with a shorter chunk (bounded by its escaped size)
    // instead of failing: the client can page through everything.
    assert_eq!(r["ok"], true, "{}", r["error"]);
    let first = r["chunk"].as_str().unwrap().len() as u64;
    assert!(first > 0 && r["next_cursor"] == first, "cursor must match chunk bytes");
    let (text, cursor, _) = c.read_all_output(&id, 1048576);
    assert_eq!(cursor, 1048576);
    assert_eq!(text.len(), 1048576);
    assert!(text.bytes().all(|b| b == 1));
    let l = c.try_request(json!({"type":"list"}), S(5));
    assert!(l.is_some(), "connection went mute after an oversized response");
}

/// O3c: even the error substituted for an oversized response can be too big
/// (the id alone nearly fills a frame). That reply is dropped, but the
/// connection must keep serving later requests.
#[test]
fn o3c_unanswerable_request_does_not_mute_connection() {
    let home = Home::new("o3c");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_cli();
    let mut req = json!({"v":1,"id":"","type":"stop","task_id":"x"});
    let base = serde_json::to_vec(&req).unwrap().len();
    req["id"] = json!("i".repeat(MAX_FRAME - base));
    c.send(&req);
    // Requests are dispatched concurrently; let the doomed reply be attempted
    // before probing, or the probe could be answered first and prove nothing.
    // The 4 MiB id needs a longer bound under the full parallel stress suite.
    std::thread::sleep(S(2));
    let r = c.try_request(json!({"type":"list"}), S(15));
    assert!(
        r.is_some(),
        "connection went mute after an unanswerable request (closed={}); manager.log:\n{}",
        c.closed,
        std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default()
    );
    assert_eq!(r.unwrap()["ok"], true);
}

/// O4: valid multi-byte UTF-8 (CJK) must survive chunking: concatenating
/// cursor reads with the CLI's default max_bytes must give back the text.
#[test]
fn o4_multibyte_utf8_survives_chunk_boundaries() {
    let home = Home::new("o4");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("yes 中文 | head -n 20000");
    c.request_ok(json!({"type":"wait","task_id":id,"budget_ms":10000}));
    let (text, _, _) = c.read_all_output(&id, 65536);
    assert!(!text.contains('\u{FFFD}'), "replacement chars in valid UTF-8 output");
    assert_eq!(text, "中文\n".repeat(20000));
}

/// O4b: watch events carry the same guarantee: pipe reads split multi-byte
/// characters, but concatenated `output` events reproduce the text exactly
/// and the last event's next_cursor is the total size.
#[test]
fn o4b_watch_events_never_split_utf8() {
    let home = Home::new("o4b");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    // `cat` of a prepared file makes large block writes, so the daemon's
    // 8 KiB pipe reads (not a multiple of the 7-byte line) land mid-character.
    let (id, _) = c.start(
        "f=$(mktemp); yes 中文 | head -n 30000 > \"$f\"; sleep 0.3; cat \"$f\"; rm -f \"$f\"",
    );
    c.request_ok(json!({"type":"watch","task_id":id}));
    let want = "中文\n".repeat(30000);
    assert!(
        c.wait_event(S(20), |e| e["event"] == "task_exited" && e["task_id"] == json!(id)).is_some(),
        "task did not finish"
    );
    // The final event may trail task_exited; wait until the cursor catches up.
    let total = want.len() as u64;
    let caught_up = poll_true(S(5), || {
        let _ = c.wait_event(MS(100), |_| false);
        c.events
            .iter()
            .filter(|e| e["event"] == "output")
            .any(|e| e["next_cursor"] == total)
    });
    let got: String = c
        .events
        .iter()
        .filter(|e| e["event"] == "output" && e["task_id"] == json!(id))
        .map(|e| e["chunk"].as_str().unwrap().to_string())
        .collect();
    let sizes: Vec<usize> = c
        .events
        .iter()
        .filter(|e| e["event"] == "output")
        .map(|e| e["chunk"].as_str().unwrap().len())
        .collect();
    eprintln!("o4b: {} output events, sizes {:?}", sizes.len(), &sizes[..sizes.len().min(12)]);
    assert!(!got.contains('\u{FFFD}'), "replacement chars in watch events");
    assert!(caught_up, "no event reached next_cursor {total}");
    assert_eq!(got.len(), want.len());
    assert_eq!(got, want);
}

/// O4c: the live end of the stream. A partial character the task has not
/// finished writing is held back (not turned into U+FFFD) while it runs;
/// once the task exits, a truncated character at EOF is delivered as one
/// U+FFFD so the cursor reaches the end. Watch events obey the same rules,
/// carry consistent cursors, and are never empty.
#[test]
fn o4c_partial_character_at_live_end_and_eof() {
    let home = Home::new("o4c");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    // a + first byte of 中; then its second byte alone (a read holding only
    // part of a character); then its last byte + b; then c and a dangling
    // first byte of 中 at EOF. 1+3+1+1+1 = 7 bytes in total.
    let (id, _) = c.start(
        "sleep 0.3; printf 'a\\344'; sleep 0.8; printf '\\270'; sleep 0.6; printf '\\255b'; \
         sleep 0.3; printf 'c\\344'",
    );
    c.request_ok(json!({"type":"watch","task_id":id}));
    assert!(
        poll_true(S(3), || c.task(&id).unwrap()["output_size"] == 2),
        "first write not seen"
    );
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":100}));
    assert_eq!(r["status"], "running");
    assert_eq!(r["chunk"], "a", "partial char must be held back while running");
    assert_eq!(r["next_cursor"], 1);

    c.wait_terminal(&id, S(5)).expect("task ends");
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":100}));
    assert_eq!(r["chunk"], "a中bc\u{FFFD}", "truncated char at EOF is delivered");
    assert_eq!(r["next_cursor"], 7);

    let caught_up = poll_true(S(3), || {
        let _ = c.wait_event(MS(50), |_| false);
        c.events.iter().any(|e| e["event"] == "output" && e["next_cursor"] == 7)
    });
    let outs: Vec<_> = c.events.iter().filter(|e| e["event"] == "output").cloned().collect();
    assert!(caught_up, "EOF remainder never flushed to watchers: {outs:?}");
    let got: String = outs.iter().map(|e| e["chunk"].as_str().unwrap()).collect();
    assert_eq!(got, "a中bc\u{FFFD}", "{outs:?}");
    let mut raw = 0u64;
    for e in &outs[..outs.len() - 1] {
        let chunk = e["chunk"].as_str().unwrap();
        assert!(!chunk.is_empty(), "empty output event: {outs:?}");
        raw += chunk.len() as u64;
        assert_eq!(e["next_cursor"], raw, "cursor must follow the bytes sent: {outs:?}");
    }
}

/// O3b: a response that cannot fit a frame (the request id is echoed, so a
/// near-4 MiB id does it) must not silence the connection: later requests
/// on the same connection are still answered.
#[test]
fn o3b_oversized_response_does_not_mute_connection() {
    let home = Home::new("o3b");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_cli();
    // The E_NOT_FOUND error echoes both the id and the task id plus a longer
    // envelope, so a request of exactly 4 MiB yields a response over 4 MiB.
    let task = "t".repeat(2 << 20);
    let mut req = json!({"v":1,"id":"","type":"stop","task_id":task});
    let base = serde_json::to_vec(&req).unwrap().len();
    let id = format!("big{}", "i".repeat(MAX_FRAME - base - 3));
    req["id"] = json!(id);
    assert_eq!(serde_json::to_vec(&req).unwrap().len(), MAX_FRAME);
    c.send(&req);
    let r = c.wait_id(&id, S(5)).expect("oversized response must become an error, not silence");
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_INTERNAL", "{}", r["error"]);
    let r = c.try_request(json!({"type":"list"}), S(5));
    assert!(r.is_some(), "connection went mute after an oversized response");
    assert_eq!(r.unwrap()["ok"], true);
}

/// F1/F2/F3: a frame over 4 MiB closes that connection (before or after
/// hello) without disturbing the daemon or other clients; exactly 4 MiB is
/// accepted; a malformed JSON frame gets E_BAD_REQUEST and the connection
/// stays usable; a truncated frame is harmless.
#[test]
fn f1_frame_limits_and_malformed_input() {
    let home = Home::new("f1");
    let mut daemon = home.start_daemon();
    let mut keep = home.connect();
    keep.hello_ext("sess-keep");
    let (id, pid) = keep.start("sleep 300");

    // Oversized before hello.
    let mut x = home.connect();
    x.send_raw(&((MAX_FRAME as u32) + 1).to_be_bytes()).unwrap();
    let _ = x.send_raw(b"{}");
    assert!(x.wait_closed(S(3)), "oversized pre-hello frame must close the connection");

    // Oversized after hello.
    let mut y = home.connect();
    y.hello_cli();
    y.send_raw(&u32::MAX.to_be_bytes()).unwrap();
    assert!(y.wait_closed(S(3)), "oversized frame must close the connection");

    // Exactly 4 MiB (JSON padded with trailing whitespace) is legal.
    let mut z = home.connect();
    z.hello_cli();
    let mut body = br#"{"v":1,"id":"big","type":"list","all":true}"#.to_vec();
    body.resize(MAX_FRAME, b' ');
    let mut frame = (MAX_FRAME as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    z.send_raw(&frame).unwrap();
    let big = z.wait_id("big", S(10)).expect("no response to an exactly-4MiB frame");
    assert_eq!(big["ok"], true, "4 MiB frame must be accepted: {big}");

    // Malformed JSON after hello: error response, connection survives.
    let garbage = b"{nope";
    let mut f = (garbage.len() as u32).to_be_bytes().to_vec();
    f.extend_from_slice(garbage);
    z.send_raw(&f).unwrap();
    let bad = z.wait_id("", S(3)).expect("malformed frame must get a response");
    assert_eq!(bad["error"]["code"], "E_BAD_REQUEST", "{bad}");
    let r = z.request(json!({"type":"list"}));
    assert_eq!(r["ok"], true, "connection must survive a malformed frame");
    // First message not hello -> error + close.
    let mut w = home.connect();
    let r = w.request(json!({"type":"list"}));
    assert_eq!(r["error"]["code"], "E_BAD_REQUEST");
    assert!(w.wait_closed(S(3)));

    // Truncated frame then hang-up.
    let mut t = home.connect();
    t.send_raw(&100u32.to_be_bytes()).unwrap();
    t.send_raw(b"{\"type\"").unwrap();
    drop(t);

    // Daemon and the long-lived client are unaffected.
    assert!(daemon.try_wait().unwrap().is_none());
    assert!(pid_running(pid));
    assert_eq!(keep.status_of(&id).as_deref(), Some("running"));
}

/// C2: hello validation. Extension hello without session/pi_pid, a path-like
/// session id, and a wrong protocol version are refused and never registered.
#[test]
fn c2_hello_validation() {
    let home = Home::new("c2");
    let _d = home.start_daemon();
    for bad in [
        json!({"type":"hello","client_kind":"extension","pi_pid":1}),
        json!({"type":"hello","client_kind":"extension","session_id":"s"}),
        json!({"type":"hello","client_kind":"extension","session_id":"../x","pi_pid":1}),
        json!({"type":"hello","client_kind":"extension","session_id":"","pi_pid":1}),
    ] {
        let mut c = home.connect();
        let r = c.request(bad.clone());
        assert_eq!(r["ok"], false, "{bad} accepted");
        assert_eq!(r["error"]["code"], "E_BAD_REQUEST", "{r}");
        assert!(c.wait_closed(S(3)));
    }
    let mut c = home.connect();
    let mut hello = json!({"type":"hello","client_kind":"cli"});
    hello["v"] = json!(2);
    hello["id"] = json!("h");
    c.send(&hello);
    let r = c.wait_closed(S(3));
    assert!(r);
    assert!(c.pending.iter().any(|f| f["error"]["code"] == "E_VERSION"), "{:?}", c.pending);
    // A second hello on a live connection is rejected, connection survives.
    let mut c = home.connect();
    c.hello_cli();
    let r = c.hello_cli();
    assert_eq!(r["ok"], false);
    assert_eq!(c.request(json!({"type":"list"}))["ok"], true);
    // After all those refusals no session was registered.
    let st = c.request_ok(json!({"type":"status"}));
    assert_eq!(st["sessions"].as_array().unwrap().len(), 0, "{st}");
}

// ===========================================================================
// Remaining cells: shutdown_session, idempotent stop, restart of terminal
// records, SIGTERM to the daemon, cli-only operations (S5, T12, T13, D15, D16)
// ===========================================================================

/// S5: shutdown_session stops exactly the caller's running tasks.
#[test]
fn s5_shutdown_session_stops_only_own_running_tasks() {
    let home = Home::new("s5");
    let _d = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let mut b = home.connect();
    b.hello_ext("sess-b");
    let (done, _) = a.start("true");
    a.wait_terminal(&done, S(3)).unwrap();
    let (r1, p1) = a.start("sleep 300");
    let (r2, p2) = a.start("sleep 300");
    let (other, po) = b.start("sleep 300");

    let r = a.request_ok(json!({"type":"shutdown_session"}));
    let mut stopped: Vec<String> = r["stopped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    stopped.sort();
    let mut want = vec![r1.clone(), r2.clone()];
    want.sort();
    assert_eq!(stopped, want);
    assert_eq!(a.wait_terminal(&r1, S(4)).unwrap()["status"], "killed");
    assert_eq!(a.wait_terminal(&r2, S(4)).unwrap()["status"], "killed");
    assert!(poll_true(S(3), || !pid_running(p1) && !pid_running(p2)));
    assert_eq!(a.status_of(&done).as_deref(), Some("completed"));
    assert!(pid_running(po), "other session's task was killed");
    assert_eq!(b.status_of(&other).as_deref(), Some("running"));

    let mut cli = home.connect();
    cli.hello_cli();
    let r = cli.request(json!({"type":"shutdown_session"}));
    assert_eq!(r["error"]["code"], "E_SESSION_REQUIRED");
    let r = cli.request(json!({"type":"start","kind":"shell","command":"true"}));
    assert_eq!(r["error"]["code"], "E_SESSION_REQUIRED");
}

/// T12: stop on a terminal task is an idempotent no-op; the terminal status
/// does not change.
#[test]
fn t12_stop_terminal_task_is_noop() {
    let home = Home::new("t12");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("exit 4");
    assert_eq!(c.wait_terminal(&id, S(3)).unwrap()["status"], "failed");
    c.request_ok(json!({"type":"stop","task_id":id}));
    std::thread::sleep(MS(300));
    let t = c.task(&id).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64()), (Some("failed"), Some(4)));
    let r = c.request(json!({"type":"stop","task_id":"sh_00000000"}));
    assert_eq!(r["error"]["code"], "E_NOT_FOUND");
}

/// T13: terminal records survive a manager restart unchanged and their
/// output is still readable (served from disk).
#[test]
fn t13_terminal_records_survive_restart() {
    let home = Home::new("t13");
    let mut d1 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (ok, _) = c.start("seq 1 3000");
    let (bad, _) = c.start("echo boom; exit 2");
    c.wait_terminal(&ok, S(3)).unwrap();
    c.wait_terminal(&bad, S(3)).unwrap();
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    // Nothing is left to kill, so no clock step: shutdown must not wait on
    // the manual clock (see t13b).
    if wait_child(&mut d1, S(8)).is_none() {
        let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
        panic!("daemon did not exit after shutdown\n{log}");
    }

    let _d2 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let t = c.task(&ok).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64()), (Some("completed"), Some(0)));
    let t = c.task(&bad).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64()), (Some("failed"), Some(2)));
    let (text, _, last) = c.read_all_output(&ok, 1000);
    let want: String = (1..=3000).map(|i| format!("{i}\n")).collect();
    assert_eq!(text, want);
    assert_eq!(last["total_size"], want.len() as u64);
    let w = c.request_ok(json!({"type":"wait","task_id":bad,"budget_ms":100}));
    assert_eq!((w["done"].as_bool(), w["exit_code"].as_i64()), (Some(true), Some(2)));
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(log.contains("orphaned=0 loaded=2"), "{log}");
}

/// T13b: shutdown looks at leftover groups as they are now. A finished task
/// whose group was non-empty when its leader exited, but has emptied since,
/// gets no SIGTERM and no 2s grace: nothing of it is left to kill.
///
/// The leftover-group flag is only refreshed by the 500ms `group-poll`,
/// which under the manual clock never runs unless a test steps it. Before
/// the fix, shutdown trusted the stale flag and waited on `shutdown-grace`
/// forever. The same stale flag, set by a transient EPERM from
/// `kill(-pgid, 0)` just after the leader was reaped (macOS, under load),
/// was what hung `t13`.
#[test]
fn t13b_shutdown_skips_leftover_group_that_has_emptied() {
    let home = Home::new("t13b");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("sleep 0.3 >/dev/null 2>&1 & echo $!");
    let gc = wait_for_pids(&mut c, &id, 1)[0];
    c.wait_terminal(&id, S(3)).unwrap();
    // The group outlived its leader, then emptied. No clock step, so the
    // leftover-group poll (manual clock) has not seen it empty.
    assert!(poll_true(S(3), || !pid_running(gc)));
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    let exited = wait_child(&mut d, S(3));
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
    assert!(exited.is_some(), "shutdown waited on an emptied group\n{log}");
    assert!(!log.contains("leftover process group"), "{log}");
}

/// D15: SIGTERM/SIGINT to the daemon is the same graceful shutdown.
#[test]
fn d15_sigterm_to_daemon_is_graceful() {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let home = Home::new(&format!("d15-{sig}"));
        let mut d = home.start_daemon();
        let mut c = home.connect();
        c.hello_ext("sess-a");
        let (id, pid) = c.start("sleep 300");
        kill_pid(d.id(), sig);
        home.advance("shutdown-grace", 2000);
        let st = wait_child(&mut d, S(6)).expect("daemon must exit on signal");
        assert!(st.success(), "{st:?}");
        assert!(!pid_running(pid));
        assert!(!home.sock().exists() && !home.pidfile().exists());
        assert_eq!(home.record(&id).unwrap()["status"], "killed");
    }
}

/// D16: shutdown is cli-only, so an extension cannot shut the manager down
/// for everyone; status is open to extensions and reports the hello cwd.
#[test]
fn d16_extension_cannot_shutdown() {
    let home = Home::new("d16");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    c.hello(json!({"type":"hello","client_kind":"extension","session_id":"sess-a",
        "pi_pid":std::process::id(),"cwd":"/tmp/d16-cwd"}));
    let r = c.request(json!({"type":"shutdown"}));
    assert_eq!(r["error"]["code"], "E_FORBIDDEN");
    // status is read-only and open to extensions (ghost-agent pruning);
    // it echoes the session cwd sent on hello.
    let r = c.request_ok(json!({"type":"status"}));
    let s = &r["sessions"][0];
    assert_eq!((s["session_id"].as_str(), s["cwd"].as_str()), (Some("sess-a"), Some("/tmp/d16-cwd")), "{r}");
    std::thread::sleep(MS(500));
    assert!(d.try_wait().unwrap().is_none(), "extension shut the manager down");
}

// ===========================================================================
// Descriptor hygiene: tasks inherit only stdin/stdout/stderr (T14, S6)
// ===========================================================================

/// T14: a task inherits only fds 0, 1 and 2. The daemon here holds an extra
/// descriptor without close-on-exec (fd 20), as it would after inheriting
/// one from whatever spawned it, or through the window between accept()
/// and FD_CLOEXEC on macOS. Two client connections are open while the task
/// starts. The task lists its own open descriptors.
#[test]
fn t14_tasks_inherit_only_stdio() {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let home = Home::new("t14");
    let extra = std::fs::File::open("/dev/null").unwrap(); // O_CLOEXEC here
    let raw = extra.as_raw_fd();
    let mut cmd = std::process::Command::new(BIN);
    cmd.arg("--home")
        .arg(&home.path)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: only dup2, which is async-signal-safe; the copy at fd 20 has
    // no close-on-exec flag, so the daemon inherits it.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(raw, 20) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _d = cmd.spawn().expect("spawn daemon");
    drop(extra);
    assert!(poll_true(S(3), || UnixStream::connect(home.sock()).is_ok()), "daemon did not start");
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let mut other = home.connect();
    other.hello_ext("sess-b");
    let (id, _) = c.start("for i in $(seq 3 255); do [ -e /dev/fd/$i ] && echo \"fd $i\"; done; echo end");
    c.wait_terminal(&id, S(5)).expect("task finished");
    let (text, _, _) = c.read_all_output(&id, 65536);
    assert_eq!(text, "end\n", "task inherited descriptors beyond stdio");
    drop(other);
}

/// S6: a connection the daemon closes (session rebind) is seen as closed
/// by its client right away, even while tasks are being spawned around that
/// moment and keep running. A task that inherited a copy of the connection
/// (forked between accept() and FD_CLOEXEC) would hide the close until it
/// exits.
#[test]
fn s6_rebound_connection_closes_while_tasks_run() {
    let home = Home::new("s6");
    let _d = home.start_daemon();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spawner = {
        let mut sp = home.connect();
        sp.hello_ext("spawner");
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) && n < 120 {
                sp.start("sleep 30");
                n += 1;
            }
            sp
        })
    };
    let mut cur = home.connect();
    cur.hello_ext("sess-r");
    for round in 0..40 {
        let mut next = home.connect();
        next.hello_ext("sess-r");
        assert!(
            cur.wait_closed(S(2)),
            "round {round}: rebound connection not seen as closed while tasks run"
        );
        cur = next;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _sp = spawner.join().unwrap();
}

/// T15: a command that prints a burst right before it exits has all of it
/// in the terminal record: the exit event's and the record's output_size,
/// and task_output, cover every byte. The runner's status report can arrive
/// while up to a pipe buffer per stream still sits unread by the tee.
#[test]
fn t15_output_printed_just_before_exit_is_all_recorded() {
    let home = Home::new("t15");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    const OUT: u64 = 1 << 20;
    const ERR: u64 = 256 << 10;
    let cmd = format!("head -c {OUT} /dev/zero | tr '\\0' o; head -c {ERR} /dev/zero | tr '\\0' e >&2");
    let ids: Vec<String> = (0..8).map(|_| c.start(&cmd).0).collect();
    for id in &ids {
        let ev = c
            .wait_event(S(20), |e| e["event"] == "task_exited" && e["task_id"] == json!(id))
            .expect("task_exited");
        assert_eq!(ev["output_size"], json!(OUT + ERR), "{id}: exit event output_size");
        let rec = c.task(id).expect("record");
        assert_eq!(rec["output_size"], json!(OUT + ERR), "{id}: record output_size");
        let (_, end, _) = c.read_all_output(id, 1 << 20);
        assert_eq!(end, OUT + ERR, "{id}: task_output");
    }
}

/// T15b: CI (ubuntu-latest, 2026-09-29) recorded 1261568 of t15's 1310720
/// bytes: a pump fell behind for longer than the drain's 50 ms quiet window,
/// and the exit was finalized with its last output still in the pipe. Here
/// the pump pauses 300 ms once it has read all but 16 KiB, after the command
/// has written everything. Invariant: a command that left nothing behind is
/// finalized only after its pipes reach EOF (within the drain's 2 s cap).
#[test]
fn t15b_a_pump_that_falls_behind_still_has_all_output_recorded() {
    let home = Home::new("t15b");
    const OUT: u64 = 256 << 10;
    let stall = format!("{}:300", OUT - (16 << 10));
    let _d = home.start_daemon_from(std::path::Path::new(BIN), &[("PI_FAMULUS_TEST_PUMP_STALL", &stall)]);
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let started = Instant::now();
    let (id, _) = c.start(&format!("head -c {OUT} /dev/zero | tr '\\0' o"));
    let ev = c
        .wait_event(S(20), |e| e["event"] == "task_exited" && e["task_id"] == json!(id))
        .expect("task_exited");
    assert_eq!(ev["output_size"], json!(OUT), "exit event output_size");
    assert_eq!(c.task(&id).expect("record")["output_size"], json!(OUT), "record output_size");
    // The hook runs only in debug builds; without the pause this test
    // cannot fail, so require that the exit waited for it.
    assert!(started.elapsed() >= Duration::from_millis(300), "the pump-stall hook did not run; is this a release build?");
}
