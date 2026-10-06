//! Real-time backpressure canary (C6).
//!
//! Keep this test in its own harness: lifecycle_adversarial deliberately
//! saturates every CPU with burners and concurrent CLI startup races. A
//! wall-clock latency bound must not measure those unrelated tests' load.
//! Plain cargo test and cargo test --features test-clock both run this
//! binary; the original workload, latency and memory bounds are unchanged.
//! Producer success and exact output size also guard the workload itself.

mod common;

use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

const S: fn(u64) -> Duration = Duration::from_secs;
const MS: fn(u64) -> Duration = Duration::from_millis;

/// Invariant (C6): a watcher that never reads its socket must not stall the
/// task, other clients, or grow the daemon's memory with the output volume.
#[test]
fn c6_never_reading_watcher_is_bounded_and_isolated() {
    let home = Home::new("c6");
    let d = home.start_daemon();
    let daemon_pid = d.id();
    let mut other = home.connect();
    other.hello_ext("sess-other");
    let base_rss = rss_bytes(daemon_pid).unwrap();

    const TOTAL: u64 = 256 * 1024 * 1024;
    let mut slow = home.connect();
    slow.hello_ext("sess-slow");
    let (id, _) = slow.start(&format!(
        "sleep 0.3; yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c {TOTAL}"
    ));
    slow.request_ok(json!({"type":"watch","task_id":id}));
    // From here on `slow` is never read again.

    let mut peak = base_rss;
    let mut worst_latency = Duration::ZERO;
    let deadline = Instant::now() + S(90);
    loop {
        let t = Instant::now();
        let r = other
            .try_request(json!({"type":"list","all":true}), S(5))
            .expect("other client starved by a slow watcher");
        worst_latency = worst_latency.max(t.elapsed());
        peak = peak.max(rss_bytes(daemon_pid).unwrap_or(0));
        let _ = r;
        let mut cli = home.connect();
        cli.hello_cli();
        if let Some(rec) = cli.task(&id) {
            if rec["status"] != "running" {
                assert_eq!(rec["status"], "completed", "producer failed: {rec}");
                assert_eq!(rec["exit_code"], 0, "producer failed: {rec}");
                assert_eq!(rec["output_size"], TOTAL, "incomplete backpressure workload: {rec}");
                break;
            }
        }
        assert!(Instant::now() < deadline, "big task stalled behind a slow watcher");
        std::thread::sleep(MS(100));
    }
    let growth = peak.saturating_sub(base_rss);
    eprintln!(
        "c6: rss growth {} MiB, worst latency {worst_latency:?}",
        growth >> 20
    );
    assert!(
        worst_latency < S(2),
        "other client latency {worst_latency:?} while a watcher was stuck"
    );
    assert!(
        growth < 96 * 1024 * 1024,
        "daemon RSS grew by {} MiB for {} MiB of output with a stuck watcher",
        growth >> 20,
        TOTAL >> 20
    );
    // Other clients still get full service.
    let (e, _) = other.start("echo still-alive");
    let w = other.request_ok(json!({"type":"wait","task_id":e,"budget_ms":3000}));
    assert_eq!(w["done"], true);
    drop(slow);
}
