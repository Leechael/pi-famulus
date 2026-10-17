#![cfg(unix)]
//! Black-box shutdown-wait regressions: real CLI processes, framed sockets,
//! OS lifetime locks, and real successor daemons. No manager internals linked.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const S: fn(u64) -> Duration = Duration::from_secs;

// A failed assertion must not leave a CLI waiting on our fixture socket.
struct CliGuard(Option<Child>);

impl CliGuard {
    fn spawn(home: &Home, session: &str) -> Self {
        Self(Some(
            Command::new(BIN)
                .arg("--home")
                .arg(&home.path)
                .args(["kill-session", session])
                .env(
                    "PI_FAMULUS_TEST_CLOCK",
                    if home.manual { "manual" } else { "" },
                )
                .env("PI_FAMULUS_TEST_OWNER", test_owner())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn CLI"),
        ))
    }
}

impl Drop for CliGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn accept_hello(listener: &UnixListener) -> (Conn, Value) {
    let (socket, _) = poll_until(S(3), || listener.accept().ok()).expect("CLI did not connect");
    let mut conn = Conn::new(socket);
    let hello = match conn.recv(Instant::now() + S(3)) {
        Recv::Frame(frame) => frame,
        _ => panic!("CLI did not send hello"),
    };
    assert_eq!(hello["v"], 1);
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["client_kind"], "cli");
    assert!(hello["id"].is_string());
    (conn, hello)
}

fn refuse_shutdown(listener: &UnixListener) {
    let (mut conn, hello) = accept_hello(listener);
    conn.send(&json!({"v":1,"id":hello["id"],"ok":false,
        "error":{"code":"E_INTERNAL","message":"manager is shutting down"}}));
}

/// Regression (PR40 / 4194337595): after an exact shutdown refusal, an
/// unanswered probe must not hide a released lifetime lock until startup's
/// global 15s deadline. Keep that old connection open through CLI completion.
#[test]
fn stalled_shutdown_probe_does_not_prevent_spawning_successor() {
    let home = Home::new("probe-spawn");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.path.join("manager.lock"))
        .unwrap();
    let mut lock = fd_lock::RwLock::new(file);
    let guard = lock.try_write().unwrap();
    let listener = UnixListener::bind(home.sock()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cli = CliGuard::spawn(&home, "probe-spawn-session");

    refuse_shutdown(&listener);
    let (mut stalled, _) = accept_hello(&listener);
    // Observing the second hello proves the CLI entered the shutdown wait;
    // releasing ownership before this point would not reproduce the bug.
    assert!(home.pidfile_pid().is_none());
    assert!(
        daemon_pids_for(&home.path).is_empty(),
        "spawned against the held lock"
    );
    let released_at = Instant::now();
    drop(listener);
    fs::remove_file(home.sock()).unwrap();
    drop(guard);

    assert!(
        wait_child(cli.0.as_mut().unwrap(), S(20)).is_some(),
        "CLI did not finish"
    );
    let out = cli.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stalled probe consumed startup deadline: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        released_at.elapsed() < S(5),
        "one probe consumed the global 15s budget"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "session probe-spawn-session: stopped 0 task(s)\n"
    );
    assert!(out.stderr.is_empty());
    assert!(
        stalled.wait_closed(S(1)),
        "timed-out probe was not cancelled"
    );
    let successor = home.pidfile_pid().expect("CLI created a real successor");
    let mut check = home.connect();
    assert_eq!(check.hello_cli()["pid"], successor);
    assert_eq!(
        daemon_pids_for(&home.path),
        vec![successor],
        "competing successor daemons"
    );
    let log = fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(
        !log.contains("already running"),
        "spawned before old lock released: {log}"
    );
}

/// Regression: the retiring manager can remove its endpoint before it
/// releases manager.lock. Initial hello EOF (not a shutdown refusal) must
/// wait for that owner, not spawn a losing daemon and fail its 2s socket wait.
#[test]
fn initial_hello_eof_waits_for_endpoint_gap_before_spawning_successor() {
    let home = Home::new("probe-endpoint-gap");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.path.join("manager.lock"))
        .unwrap();
    let mut lock = fd_lock::RwLock::new(file);
    let guard = lock.try_write().unwrap();
    let listener = UnixListener::bind(home.sock()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cli = CliGuard::spawn(&home, "endpoint-gap-session");

    // Anchor the ownership window to this CLI's actual initial hello. Remove
    // the listener before sending EOF so every subsequent connect sees the
    // missing endpoint while the lifetime lock is still held.
    let (initial, _) = accept_hello(&listener);
    drop(listener);
    fs::remove_file(home.sock()).unwrap();
    drop(initial);
    let exited_early = poll_true(S(3), || {
        cli.0.as_mut().unwrap().try_wait().unwrap().is_some()
    });
    if exited_early {
        let out = cli.0.take().unwrap().wait_with_output().unwrap();
        panic!(
            "CLI failed during held-lock endpoint gap: {}; manager.log: {}",
            String::from_utf8_lossy(&out.stderr),
            fs::read_to_string(home.path.join("manager.log")).unwrap_or_default()
        );
    }
    assert!(!home.sock().exists());
    assert!(home.pidfile_pid().is_none());
    assert!(
        daemon_pids_for(&home.path).is_empty(),
        "spawned a competing daemon"
    );
    // spawn_daemon opens manager.log before launching: this also detects a
    // transient loser that has already exited with AlreadyRunning.
    assert!(
        !home.path.join("manager.log").exists(),
        "spawned while lifetime lock was held"
    );
    drop(guard);

    assert!(
        wait_child(cli.0.as_mut().unwrap(), S(8)).is_some(),
        "CLI did not finish after lock release"
    );
    let out = cli.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "endpoint-gap retry failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "session endpoint-gap-session: stopped 0 task(s)\n"
    );
    assert!(out.stderr.is_empty());
    let successor = home.pidfile_pid().expect("CLI created a real successor");
    let mut check = home.connect();
    assert_eq!(check.hello_cli()["pid"], successor);
    assert_eq!(daemon_pids_for(&home.path), vec![successor]);
    let log = fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(!log.contains("already running"), "premature spawn: {log}");
}

/// A framing violation is permanent, both on the initial hello and after
/// a known shutdown refusal. It must not be retried as a transport failure.
fn assert_oversized_hello_is_fatal(after_shutdown: bool) {
    let home = Home::new(if after_shutdown { "probe-oversized" } else { "initial-oversized" });
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.path.join("manager.lock"))
        .unwrap();
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.try_write().unwrap();
    let pid_file = serde_json::to_vec(&json!({
        "pid": 0, "version": "fixture", "started_at": 0
    })).unwrap();
    fs::write(home.pidfile(), &pid_file).unwrap();
    let listener = UnixListener::bind(home.sock()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cli = CliGuard::spawn(&home, "oversized-session");

    if after_shutdown {
        refuse_shutdown(&listener);
    }
    let (mut probe, _) = accept_hello(&listener);
    // The real CLI has sent hello. Only the invalid length is needed: frame
    // validation must reject it before trying to read a payload.
    probe.send_raw(&(MAX_FRAME as u32 + 1).to_be_bytes()).unwrap();
    assert!(
        wait_child(cli.0.as_mut().unwrap(), S(3)).is_some(),
        "oversized hello frame was retried or stalled"
    );
    let out = cli.0.take().unwrap().wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "pi-famulus: cannot reach pi-famulus: protocol framing: frame length 4194305 exceeds 4 MiB limit\n"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(fs::read(home.pidfile()).unwrap(), pid_file, "client changed the owner's pid file");
    assert!(home.sock().exists(), "client deleted the owner's socket");
    assert!(daemon_pids_for(&home.path).is_empty(), "spawned a competing daemon");
    assert!(!home.path.join("manager.log").exists(), "unexpected spawn attempt");
    assert!(probe.wait_closed(S(1)), "failed handshake connection was not closed");
}

#[test]
fn initial_oversized_hello_frame_is_fatal_without_spawning() {
    assert_oversized_hello_is_fatal(false);
}

#[test]
fn shutdown_wait_oversized_probe_frame_is_fatal_without_spawning() {
    assert_oversized_hello_is_fatal(true);
}

/// Invariant: a protocol error on a shutdown-wait probe is fatal, not a
/// transport timeout to swallow. Neither ownership nor protocol files change.
#[test]
fn shutdown_wait_propagates_other_protocol_errors_without_spawning() {
    let home = Home::new("probe-protocol");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.path.join("manager.lock"))
        .unwrap();
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.try_write().unwrap();
    let listener = UnixListener::bind(home.sock()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut cli = CliGuard::spawn(&home, "probe-protocol-session");

    refuse_shutdown(&listener);
    let (mut probe, hello) = accept_hello(&listener);
    probe.send(&json!({"v":1,"id":hello["id"],"ok":false,
        "error":{"code":"E_VERSION","message":"unsupported protocol version"}}));
    assert!(
        wait_child(cli.0.as_mut().unwrap(), S(3)).is_some(),
        "protocol error was swallowed"
    );
    let out = cli.0.take().unwrap().wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "pi-famulus: cannot reach pi-famulus: E_VERSION: unsupported protocol version\n"
    );
    assert!(out.stdout.is_empty());
    assert!(home.pidfile_pid().is_none());
    assert!(daemon_pids_for(&home.path).is_empty());
    assert!(
        !home.path.join("manager.log").exists(),
        "unexpected spawn attempt"
    );
    assert!(home.sock().exists(), "client deleted the owner's socket");
}
