#![cfg(unix)]
//! Real-time canaries for the daemon's two lifecycle constants.
//!
//! Under `--features test-clock` the other suites step these timers on the
//! manual clock, so they never see real durations. These tests always run the
//! daemon on real time (`Home::new_real`) and pin the actual values:
//! - the idle grace: the last client leaves, and 5s later tasks are SIGTERMed;
//! - the kill grace: a stopped task that ignores SIGTERM is SIGKILLed 2s later.
//!
//! The lower bound is the contract (never early). The upper bound is loose so
//! a loaded machine does not fail it, but tight enough to catch a doubled
//! constant.

mod common;

use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

/// Wait for `pid` to die and return when it happened, relative to `t0`.
fn death_after(t0: Instant, pid: u32, timeout: Duration) -> Option<Duration> {
    poll_until(timeout, || (!pid_running(pid)).then(|| t0.elapsed()))
}

#[test]
fn canary_idle_grace_is_5s() {
    let home = Home::new_real("canary-idle");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (_, pid) = c.start("sleep 300");
    drop(c);
    let t0 = Instant::now();
    let died = death_after(t0, pid, Duration::from_secs(10)).expect("task never killed after the idle grace");
    assert!(
        died >= Duration::from_millis(4900),
        "task killed {died:?} after the last client left, before the 5s grace"
    );
    assert!(died <= Duration::from_millis(6500), "idle grace took {died:?}, expected ~5s");
    assert!(wait_child(&mut d, Duration::from_secs(5)).is_some());
}

#[test]
fn canary_kill_grace_is_2s() {
    let home = Home::new_real("canary-kill");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, pid) = c.start("trap '' TERM; echo armed; sleep 300");
    assert!(poll_true(Duration::from_secs(5), || {
        let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":64}));
        r["chunk"].as_str().unwrap_or("").contains("armed")
    }));
    let t0 = Instant::now();
    c.request_ok(json!({"type":"stop","task_id":id}));
    let died = death_after(t0, pid, Duration::from_secs(6)).expect("TERM-ignoring task never SIGKILLed");
    assert!(died >= Duration::from_millis(1900), "SIGKILL after {died:?}, before the 2s grace");
    assert!(died <= Duration::from_millis(3500), "kill grace took {died:?}, expected ~2s");
    assert_eq!(c.wait_terminal(&id, Duration::from_secs(3)).unwrap()["status"], "killed");
}
