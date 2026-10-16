#![cfg(unix)]
//! Real daemon/socket contract, including byte-exact raw output reads.
mod common;
use common::*;
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn monitor_live_records_include_eof_tail_before_exit_and_preserve_raw_reads() {
    let home = Home::new("mon-live");
    let _daemon = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("monitor-records");
    let (tid, _) = c.start_with(json!({"type":"start","kind":"monitor",
        "command":"printf 'hello 世界\\ntrailing'", "cwd":task_cwd(),
        "env":task_env(),"run_in_background":true}));
    c.wait_event(Duration::from_secs(5), |v| {
        v["event"] == "task_exited" && v["task_id"] == tid
    })
    .expect("task exited");
    let output: Vec<&Value> = c
        .events
        .iter()
        .filter(|v| v["event"] == "output" && v["task_id"] == tid)
        .collect();
    let raw: String = output
        .iter()
        .map(|v| v["chunk"].as_str().unwrap())
        .collect();
    assert_eq!(raw, "hello 世界\ntrailing");
    let records: Vec<Value> = output
        .iter()
        .flat_map(|v| v["monitor_records"].as_array().into_iter().flatten())
        .cloned()
        .collect();
    assert_eq!(records.len(), 2, "line and EOF tail metadata: {output:?}");
    assert_eq!(records[0]["preview"], "hello 世界");
    assert_eq!(records[0]["cursor_end"], 13);
    assert_eq!(records[1]["preview"], "trailing");
    assert_eq!(records[1]["cursor_start"], 13);
    assert_eq!(records[1]["cursor_end"], 21);
    assert_eq!(records[1]["kind"], "line");
    assert!(records[1]["fingerprint"]
        .as_str()
        .unwrap()
        .starts_with("blake3:v1:"));
    let (read, cursor, _) = c.read_all_output(&tid, 4);
    assert_eq!(read, raw);
    assert_eq!(cursor, 21);

    // A late watcher receives the original stable identity and observation
    // time, not freshly hashed/truncated transport chunks.
    let mut late = home.connect();
    late.hello_cli();
    late.request_ok(json!({"type":"watch","task_id":tid}));
    late.wait_event(Duration::from_secs(3), |v| {
        v["event"] == "output" && v["next_cursor"] == 21
    })
    .expect("late replay");
    let replay: Vec<Value> = late
        .events
        .iter()
        .filter(|v| v["event"] == "output")
        .flat_map(|v| v["monitor_records"].as_array().into_iter().flatten())
        .cloned()
        .collect();
    assert_eq!(replay, records);
    let count = late.events.len();
    late.request_ok(json!({"type":"watch","task_id":tid}));
    late.drain(Duration::from_millis(50));
    let same_cursor = &late.events[count..];
    assert!(
        same_cursor.iter().any(|v| v["chunk"] == ""
            && v["next_cursor"] == 21
            && v["monitor_records"][0] == records[1]),
        "EOF metadata is replayable even when raw cursor is caught up: {same_cursor:?}"
    );
}

#[test]
fn newline_free_progress_is_timely_incremental_and_unhashed() {
    let home = Home::new("mon-prog");
    let _daemon = home.start_daemon();
    let release = home.path.join("release");
    let mut c = home.connect();
    c.hello_ext("monitor-progress");
    let command = format!(
        "printf first; while [ ! -f '{}' ]; do sleep 0.01; done; printf ' second'",
        release.display()
    );
    let (tid, _) = c.start_with(json!({"type":"start","kind":"monitor", "command":command,
        "cwd":task_cwd(),"env":task_env(),"run_in_background":true}));
    let event = c
        .wait_event(Duration::from_secs(2), |v| {
            v["event"] == "output"
                && v["monitor_records"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|r| r["kind"] == "progress"))
        })
        .expect("progress before command EOF/newline");
    let progress = &event["monitor_records"][0];
    assert_eq!(event["chunk"], "");
    assert_eq!(event["next_cursor"], 5);
    assert_eq!(progress["preview"], "first");
    assert_eq!(progress["cursor_start"], 0);
    assert_eq!(progress["cursor_end"], 5);
    assert!(progress.get("fingerprint").is_none());
    assert!(progress.get("timestamp_fingerprint").is_none());
    std::fs::write(release, b"go").unwrap();
    c.wait_event(Duration::from_secs(3), |v| {
        v["event"] == "task_exited" && v["task_id"] == tid
    })
    .unwrap();
    let completed = c
        .events
        .iter()
        .filter(|v| v["event"] == "output")
        .flat_map(|v| v["monitor_records"].as_array().into_iter().flatten())
        .find(|r| r["kind"] == "line")
        .unwrap();
    assert_eq!(completed["preview"], "first second");
    assert_eq!(completed["cursor_start"], 0);
    assert_eq!(completed["cursor_end"], 12);
    let (raw, cursor, _) = c.read_all_output(&tid, 3);
    assert_eq!((raw.as_str(), cursor), ("first second", 12));
}

#[test]
fn rewatch_replays_progress_at_already_received_raw_cursor() {
    let home = Home::new("mon-reprog");
    let _daemon = home.start_daemon();
    let release = home.path.join("release");
    let mut c = home.connect();
    c.hello_ext("monitor-reprogress");
    let command = format!(
        "printf first; while [ ! -f '{}' ]; do sleep 0.01; done",
        release.display()
    );
    let (tid, _) = c.start_with(json!({"type":"start","kind":"monitor", "command":command,
        "cwd":task_cwd(),"env":task_env(),"run_in_background":true}));
    c.wait_event(Duration::from_secs(2), |v| {
        v["event"] == "output" && v["next_cursor"] == 5
    })
    .unwrap();
    c.request_ok(json!({"type":"unwatch","task_id":tid}));
    // A second watcher observes the real progress timer, rather than depending
    // on a fixed sleep, while the original watcher has no subscription.
    let mut observer = home.connect();
    observer.hello_cli();
    observer.request_ok(json!({"type":"watch","task_id":tid}));
    let progress = observer
        .wait_event(Duration::from_secs(2), |v| {
            v["event"] == "output"
                && v["monitor_records"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|r| r["kind"] == "progress"))
        })
        .expect("real progress timer observed");
    c.events.clear();
    c.request_ok(json!({"type":"watch","task_id":tid}));
    let replay = c
        .wait_event(Duration::from_secs(1), |v| {
            v["event"] == "output"
                && v["monitor_records"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|r| r["kind"] == "progress"))
        })
        .expect("rewatch must not lose progress just because raw cursor is caught up");
    assert_eq!(replay["chunk"], "");
    assert_eq!(replay["next_cursor"], 5);
    assert_eq!(replay["monitor_records"], progress["monitor_records"]);
    std::fs::write(release, b"go").unwrap();
}

#[test]
fn dense_empty_lines_keep_live_records_and_frames_bounded_despite_history_eviction() {
    let home = Home::new("mon-dense");
    let _daemon = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("monitor-dense");
    let (tid, _) = c.start_with(json!({"type":"start","kind":"monitor",
        "command":"awk 'BEGIN { for (i=0; i<10000; i++) printf \"\\n\"; }'",
        "cwd":task_cwd(),"env":task_env(),"run_in_background":true}));
    c.wait_event(Duration::from_secs(5), |v| {
        v["event"] == "task_exited" && v["task_id"] == tid
    })
    .unwrap();
    let output: Vec<_> = c.events.iter().filter(|v| v["event"] == "output").collect();
    let count: usize = output
        .iter()
        .map(|v| v["monitor_records"].as_array().unwrap().len())
        .sum();
    assert_eq!(
        count, 10000,
        "bounded history eviction must not drop fresh live metadata"
    );
    assert!(output
        .iter()
        .all(|v| v["chunk"].as_str().unwrap().len() <= 4096));
    assert!(output
        .iter()
        .all(|v| serde_json::to_vec(v).unwrap().len() < MAX_FRAME));
    let (raw, cursor, _) = c.read_all_output(&tid, 4096);
    assert_eq!(raw, "\n".repeat(10000));
    assert_eq!(cursor, 10000);
}

#[test]
fn hot_upgrade_unknown_partial_line_falls_back_then_framing_resumes() {
    let home = Home::new("mon-up");
    let bin = home.install_copy();
    let _daemon = home.start_daemon_from(&bin, &[]);
    let release = home.path.join("release");
    let mut c = home.connect();
    c.hello_ext("monitor-upgrade");
    let release2 = home.path.join("release2");
    let command = format!("printf before; while [ ! -f '{}' ]; do sleep 0.01; done; printf ' after\\n'; while [ ! -f '{}' ]; do sleep 0.01; done; printf 'new line\\n'", release.display(), release2.display());
    let (tid, _) = c.start_with(json!({"type":"start","kind":"monitor", "command":command,
        "cwd":task_cwd(),"env":task_env(),"run_in_background":true}));
    c.wait_event(Duration::from_secs(3), |v| {
        v["event"] == "output" && v["chunk"] == "before"
    })
    .unwrap();
    let upgrade = home.cli(&["upgrade"], Duration::from_secs(40));
    assert!(
        upgrade.status.success(),
        "{} {}",
        upgrade.stdout,
        upgrade.stderr
    );
    assert!(c.wait_closed(Duration::from_secs(3)));
    let mut reconnected = home.connect();
    reconnected.hello_ext("monitor-upgrade");
    std::fs::write(release, b"go").unwrap();
    reconnected
        .wait_event(Duration::from_secs(3), |v| {
            v["event"] == "output" && v["chunk"].as_str().is_some_and(|s| s.contains(" after\n"))
        })
        .unwrap();
    std::fs::write(release2, b"go").unwrap();
    reconnected
        .wait_event(Duration::from_secs(5), |v| {
            v["event"] == "task_exited" && v["task_id"] == tid
        })
        .unwrap();
    let output: Vec<_> = reconnected
        .events
        .iter()
        .filter(|v| v["event"] == "output")
        .collect();
    assert!(
        output.iter().any(
            |v| v["chunk"].as_str().is_some_and(|s| s.contains(" after\n"))
                && v.get("monitor_records").is_none()
        ),
        "unknown pre-upgrade prefix must never get a false partial hash: {output:?}"
    );
    assert!(
        output
            .iter()
            .flat_map(|v| v["monitor_records"].as_array().into_iter().flatten())
            .any(|r| r["kind"] == "line" && r["preview"] == "new line" && r["cursor_start"] == 13),
        "framing resumes at the next known logical boundary: {output:?}"
    );
    let (raw, cursor, _) = reconnected.read_all_output(&tid, 4);
    assert_eq!(raw, "before after\nnew line\n");
    assert_eq!(cursor, 22);
}
