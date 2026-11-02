#![cfg(all(unix, feature = "test-clock"))]
//! Canaries for the daemon's two lifecycle constants, on simulated time.
//!
//! - the idle grace: the last client leaves, and 5s later tasks are SIGTERMed;
//! - the kill grace: a stopped task that ignores SIGTERM is SIGKILLed 2s later.
//!
//! Each boundary is exact and deterministic: the timer is armed with exactly
//! the constant left, the task survives 1ms before it, and is acted on at it.
//! That catches a changed constant in either direction without sleeping.
//! That the normal build's clock really sleeps those durations is proven on
//! paused tokio time by the daemon's unit test
//! `real_clock_idle_and_kill_graces_sleep_exactly_their_constants`.

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;

/// The single pending `label` timer, armed and not yet stepped: its full
/// duration is left.
fn armed_ms(home: &Home, label: &str) -> u64 {
    let mut due = None;
    assert!(
        poll_true(Duration::from_secs(10), || {
            let status = clock_request(&home.path, json!({"type":"clock_status"}));
            due = status["pending"]
                .as_array()
                .and_then(|p| p.iter().find(|t| t["label"] == label))
                .and_then(|t| t["due_in_ms"].as_u64());
            due.is_some()
        }),
        "no {label} timer was armed"
    );
    due.unwrap()
}

fn step(home: &Home, ms: u64) {
    let r = clock_request(&home.path, json!({"type":"clock_advance","ms":ms}));
    assert_eq!(r["ok"], true, "clock_advance: {r}");
}

#[test]
fn canary_idle_grace_is_5s() {
    let home = Home::new("canary-idle");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (_, pid) = c.start("sleep 300");
    drop(c);
    assert_eq!(armed_ms(&home, "idle"), 5000, "idle grace armed with the wrong duration");
    step(&home, 4999);
    assert!(pid_running(pid), "task killed before the 5s idle grace");
    step(&home, 1);
    assert!(
        poll_true(Duration::from_secs(10), || !pid_running(pid)),
        "task survived the 5s idle grace"
    );
    // Graceful shutdown then holds the TERM grace on the same clock.
    if pid_running(d.id()) {
        home.advance("shutdown-grace", 2000);
    }
    assert!(wait_child(&mut d, Duration::from_secs(10)).is_some());
}

#[test]
fn canary_kill_grace_is_2s() {
    let home = Home::new("canary-kill");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, pid) = c.start("trap '' TERM; echo armed; sleep 300");
    assert!(poll_true(Duration::from_secs(10), || {
        let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":64}));
        r["chunk"].as_str().unwrap_or("").contains("armed")
    }));
    c.request_ok(json!({"type":"stop","task_id":id}));
    assert_eq!(armed_ms(&home, "kill-grace"), 2000, "kill grace armed with the wrong duration");
    step(&home, 1999);
    assert!(pid_running(pid), "SIGKILL before the 2s grace");
    step(&home, 1);
    assert!(
        poll_true(Duration::from_secs(10), || !pid_running(pid)),
        "TERM-ignoring task never SIGKILLed after the 2s grace"
    );
    assert_eq!(c.wait_terminal(&id, Duration::from_secs(10)).unwrap()["status"], "killed");
}
