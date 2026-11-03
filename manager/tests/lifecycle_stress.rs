#![cfg(unix)]
//! D2, the lifecycle race that loads every CPU on purpose, in a test binary
//! of its own.
//!
//! It used to live in tests/lifecycle_adversarial.rs, where libtest ran it
//! next to that binary's other tests: its burners starved d3, d8f and the
//! lock tests, whose waits are real time, and those failed in clusters on
//! darwin and linux-arm64. Alone in its process, cargo test (one binary at a
//! time) and scripts/cargo-test-parallel.mjs (SOLO) give it the machine.

mod common;

use common::*;
use std::time::Duration;

const S: fn(u64) -> Duration = Duration::from_secs;

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
                    common::command("sh")
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
        daemon_diagnostics(home)
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
                common::famulus_command(BIN)
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
        // Same scheduling slack as d3: a loaded darwin-x64 runner can still
        // be retrying after 15s.
        assert!(poll_true(S(45), || {
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
