//! In-place upgrade (design doc §3.2, `handover.rs`): the daemon execs the
//! binary now at its path, with the same pid, and every task keeps running.
//!
//! Each test runs its daemon from a private copy of the binary
//! (`Home::install_copy`), so replacing that file never affects other tests.
//! Runs in both clock modes; the timers stepped here (`kill-grace`) are
//! re-armed by the new image and stepped the same way.

mod common;

use common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn s(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

fn hello(c: &mut Conn, session: &str) {
    c.request_ok(json!({"type":"hello","client_kind":"extension","session_id":session,
        "pi_pid":std::process::id(),"cwd":"/tmp","protocol":2}));
}

fn start(c: &mut Conn, kind: &str, command: &str, extra: Value) -> (String, u32) {
    let mut req = json!({"type":"start","kind":kind,"command":command,"cwd":"/tmp","env":{"PATH":PATH_ENV}});
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    c.start_with(req)
}

fn status(home: &Home) -> Value {
    let out = home.cli(&["status", "--json"], s(10));
    serde_json::from_str(&out.stdout).unwrap_or_else(|e| {
        let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
        panic!("status --json ({e}): {} {}\nmanager.log:\n{log}", out.stdout, out.stderr)
    })
}

fn upgrade(home: &Home) -> CliOut {
    home.cli(&["upgrade"], s(40))
}

/// Pids in process group `pgid` (the runner and what it started).
fn group(pgid: u32) -> Vec<u32> {
    // pgrep's process enumeration can fail on macOS when sysmond is
    // unavailable, silently hiding live groups behind a nonzero status.
    // ps has the same group information and works independently of sysmond.
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,pgid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let group = fields.next()?.parse::<u32>().ok()?;
            (group == pgid).then_some(pid)
        })
        .collect()
}

fn group_diagnostics(home: &Home, task_id: &str, pgid: u32) -> String {
    let ps = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,state=,command="])
        .output()
        .unwrap();
    let home_path = home.path.clone();
    let home_text = home_path.to_string_lossy();
    let processes: Vec<_> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|line| {
            let mut fields = line.split_whitespace();
            let _pid = fields.next();
            let _ppid = fields.next();
            let group = fields.next().and_then(|p| p.parse::<u32>().ok());
            group == Some(pgid) || line.contains(home_text.as_ref())
        })
        .map(str::to_string)
        .collect();
    let task = run_cli(&home_path, &["show", task_id, "--json"], s(5));
    #[cfg(target_os = "macos")]
    let sample = if pid_alive(pgid) {
        let pid_arg = pgid.to_string();
        std::process::Command::new("sample")
            .args([pid_arg.as_str(), "1"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_else(|e| e.to_string())
    } else {
        "runner pid is no longer alive".to_string()
    };
    #[cfg(not(target_os = "macos"))]
    let sample = "sample unavailable on this platform";
    let log = std::fs::read_to_string(home_path.join("manager.log")).unwrap_or_default();
    format!(
        "pgid={pgid}; group probe={:?}; ps={processes:#?}; show={} {}; sample:\n{sample}\nmanager.log:\n{log}",
        group(pgid), task.stdout, task.stderr
    )
}

/// Every `output` event's text for `task_id`, in arrival order.
fn event_text(events: &[Value], task_id: &str) -> String {
    events
        .iter()
        .filter(|e| e["event"] == "output" && e["task_id"] == task_id)
        .map(|e| e["chunk"].as_str().unwrap_or("").to_string())
        .collect()
}

/// Numbered lines `prefix-0 prefix-1 …` with no gap and no repeat.
fn assert_sequential(text: &str, prefix: &str, what: &str) -> usize {
    let lines: Vec<&str> = text.split_whitespace().collect();
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(*l, format!("{prefix}-{i}"), "{what}: line {i} out of sequence in {text:?}");
    }
    lines.len()
}

/// The core promise: an upgrade is invisible to running work.
#[test]
fn u1_upgrade_keeps_every_task_running() {
    let home = Home::new("u1");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let before = status(&home);
    let mut c = home.connect();
    hello(&mut c, "sess-u1");
    c.request_ok(json!({"type":"acquire_agent","child_id":"ch-u1"}));
    assert_eq!(status(&home)["agent_capacity"]["used"], 1);

    // Streams numbered lines for ~3 s, then exits 7.
    let (stream, stream_pid) = start(&mut c, "shell",
        "i=0; while [ $i -lt 300 ]; do echo line-$i; i=$((i+1)); if [ $((i % 10)) -eq 0 ]; then sleep 0.1; fi; done; exit 7", json!({}));
    // A monitor, auto-watched by this connection.
    let (mon, _) = start(&mut c, "monitor",
        "i=0; while [ $i -lt 250 ]; do echo mon-$i; i=$((i+1)); if [ $((i % 5)) -eq 0 ]; then sleep 0.1; fi; done", json!({}));
    let (sleeper, sleeper_pid) = start(&mut c, "shell", "sleep 300", json!({}));
    // Finishes at once, leaving a grandchild its runner guards.
    let (leftover, leftover_pid) = start(&mut c, "shell", "sleep 300 & exit 0", json!({}));
    // Hard timeout 4 s after start, straddling the upgrade.
    let (timed, _) = start(&mut c, "shell", "sleep 300", json!({"timeout_ms": 4000}));
    c.wait_event(s(5), |e| e["event"] == "task_exited" && e["task_id"] == leftover).expect("leftover task exits");
    let sleeper_group = group(sleeper_pid);
    let leftover_group = group(leftover_pid);
    assert!(
        leftover_group.len() >= 2,
        "runner + grandchild: {leftover_group:?}\n{}",
        group_diagnostics(&home, &leftover, leftover_pid)
    );
    // A wait in flight when the upgrade starts.
    c.send(&json!({"v":1,"id":"inflight-wait","type":"wait","task_id":sleeper,"budget_ms":20000}));
    std::thread::sleep(Duration::from_millis(300));

    let out = upgrade(&home);
    assert!(out.status.success(), "upgrade failed: {} {}", out.stdout, out.stderr);
    assert!(out.stdout.contains("upgraded in place"), "{}", out.stdout);

    // The old connection was closed; the in-flight wait got no answer (the
    // client resends it), certainly not an error.
    assert!(c.wait_closed(s(5)), "connection not closed by the upgrade");
    assert!(c.pending.iter().all(|f| f["id"] != "inflight-wait"), "in-flight wait was answered: {:?}", c.pending);

    let after = status(&home);
    assert_eq!(after["pid"], before["pid"], "same pid");
    assert_eq!(after["generation"], 1);
    assert_eq!(after["agent_capacity"]["used"], 1, "upgrade preserves active agent permits");
    assert_eq!(after["last_upgrade"]["ok"], true);
    assert_eq!(after["last_upgrade"]["trigger"], "cli");
    let human = home.cli(&["status"], s(10)).stdout;
    let v = before["version"].as_str().unwrap();
    assert!(v.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")), "{v}");
    assert!(human.contains(&format!("upgrades: 1 (last: {v} -> {v}, cli, ")), "{human}");
    for p in sleeper_group.iter().chain(&leftover_group) {
        assert!(pid_alive(*p), "pid {p} died in the upgrade");
    }

    // Reconnect as the same session: the monitor subscription comes back
    // with what was missed, and continues.
    let mut c2 = home.connect();
    hello(&mut c2, "sess-u1");
    c2.request_ok(json!({"type":"release_agent","child_id":"ch-u1"}));
    assert_eq!(status(&home)["agent_capacity"]["used"], 0);
    let w = c2.request_ok(json!({"type":"wait","task_id":stream,"budget_ms":15000}));
    assert_eq!((w["done"].as_bool(), w["exit_code"].as_i64()), (Some(true), Some(7)), "real exit code: {w}");
    let file = std::fs::read_to_string(home.path.join(format!("sessions/sess-u1/tasks/{stream}.output"))).unwrap();
    assert_eq!(assert_sequential(&file, "line", "stream output"), 300);
    assert!(!pid_alive(stream_pid) || group(stream_pid).is_empty());

    let w = c2.request_ok(json!({"type":"wait","task_id":mon,"budget_ms":15000}));
    assert_eq!(w["done"], true);
    c2.wait_event(s(3), |e| e["event"] == "task_exited" && e["task_id"] == mon);
    let mut events = c.events.clone();
    events.extend(c2.events.clone());
    let n = assert_sequential(&event_text(&events, &mon), "mon", "monitor events across the upgrade");
    assert_eq!(n, 250, "every monitor line delivered exactly once");

    // The timeout set before the upgrade still fires, from its original start.
    let t = c2.wait_terminal(&timed, s(8)).expect("timed task ends");
    assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("killed"), Some("timeout")), "{t}");

    // Stop still works, including on a guarded leftover group.
    c2.request_ok(json!({"type":"stop","task_id":sleeper}));
    c2.request_ok(json!({"type":"stop","task_id":leftover}));
    home.advance("kill-grace", 2_000);
    let t = c2.wait_terminal(&sleeper, s(5)).unwrap();
    assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("killed"), Some("stopped:tool")));
    assert!(poll_true(s(5), || sleeper_group.iter().chain(&leftover_group).all(|p| !pid_running(*p))),
        "stopped groups gone: {:?} {:?}", group(sleeper_pid), group(leftover_pid));
}

/// Old extensions stay loaded in running pi processes after an upgrade:
/// the new daemon accepts the previous protocol (and none).
#[test]
fn u2_old_protocol_hello_after_upgrade() {
    let home = Home::new("u2");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u2-keep");
    let out = upgrade(&home);
    assert!(out.status.success(), "{} {}", out.stdout, out.stderr);
    for (sid, proto) in [("sess-u2-p1", json!(1)), ("sess-u2-none", Value::Null)] {
        let mut c = home.connect();
        let mut h = json!({"type":"hello","client_kind":"extension","session_id":sid,"pi_pid":std::process::id()});
        if !proto.is_null() {
            h["protocol"] = proto;
        }
        c.request_ok(h);
        let (t, _) = start(&mut c, "shell", "echo hi", json!({}));
        let r = c.wait_terminal(&t, s(5)).unwrap();
        assert_eq!(r["exit_code"], 0, "{sid}: {r}");
    }
}

/// A start resent with the same key after a lost connection returns the task
/// it already started; another session's key is its own.
#[test]
fn u3_start_key_makes_a_resent_start_idempotent() {
    let home = Home::new("u3");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u3");
    let (a, _) = start(&mut c, "shell", "sleep 300", json!({"key":"k-1"}));
    assert!(upgrade(&home).status.success());
    let mut c2 = home.connect();
    hello(&mut c2, "sess-u3");
    let (b, _) = start(&mut c2, "shell", "sleep 300", json!({"key":"k-1"}));
    assert_eq!(a, b, "same key, same task (carried across the upgrade)");
    let mut other = home.connect();
    hello(&mut other, "sess-u3-other");
    let (o, _) = start(&mut other, "shell", "sleep 300", json!({"key":"k-1"}));
    assert_ne!(o, a, "keys are per session");
}

/// A binary that fails the preflight changes nothing: no connection is
/// closed, no task is touched, and the failure is reported.
#[test]
fn u4_bad_binary_is_refused_before_anything_changes() {
    let home = Home::new("u4");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u4");
    let (t, pid) = start(&mut c, "monitor", "i=0; while true; do echo m-$i; i=$((i+1)); sleep 0.05; done", json!({}));
    let bad = home.path.join("bad");
    std::fs::write(&bad, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&bad, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    replace_binary(&bin, &bad);
    let out = upgrade(&home);
    assert!(!out.status.success(), "{}", out.stdout);
    assert!(out.stderr.contains("upgrade not done") && out.stderr.contains("not an upgrade target"), "{}", out.stderr);
    let st = status(&home);
    assert_eq!((st["generation"].as_u64(), st["last_upgrade"]["ok"].as_bool()), (Some(0), Some(false)), "{st}");
    let human = home.cli(&["status"], s(10)).stdout;
    assert!(human.contains("upgrades: 0 (last attempt failed") && human.contains("not an upgrade target"), "{human}");
    // The same connection still works and still streams.
    let before = event_text(&c.events, &t).len();
    c.request_ok(json!({"type":"list"}));
    std::thread::sleep(Duration::from_millis(400));
    c.request_ok(json!({"type":"list"}));
    assert!(event_text(&c.events, &t).len() > before, "monitor kept streaming on the same connection");
    assert!(pid_alive(pid));
    // A successful check with a foreign marker is not an upgrade target.
    std::fs::write(&bad, "#!/bin/sh\necho foreign-handover 1 0.1.0\n").unwrap();
    replace_binary(&bin, &bad);
    let out = upgrade(&home);
    assert!(!out.status.success() && out.stderr.contains("gave no handover answer"), "{}", out.stderr);
    assert_eq!(status(&home)["generation"], 0);
    assert!(pid_alive(pid));
    // A file that is not executable is refused the same way.
    replace_binary(&bin, std::path::Path::new(BIN));
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let out = upgrade(&home);
    assert!(!out.status.success() && out.stderr.contains("cannot run"), "{}", out.stderr);
    assert_eq!(status(&home)["generation"], 0);
    assert!(pid_alive(pid));
}

/// exec itself failing after the quiesce: everything resumes on the old
/// image with no byte lost (test hook PI_FAMULUS_TEST_EXEC_PATH).
#[test]
fn u5_failed_exec_rolls_back() {
    let home = Home::new("u5");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[("PI_FAMULUS_TEST_EXEC_PATH", "/nonexistent/pi-famulus")]);
    let mut c = home.connect();
    hello(&mut c, "sess-u5");
    let (t, pid) = start(&mut c, "monitor",
        "i=0; while [ $i -lt 150 ]; do echo m-$i; i=$((i+1)); if [ $((i % 5)) -eq 0 ]; then sleep 0.1; fi; done", json!({}));
    std::thread::sleep(Duration::from_millis(500));
    let out = upgrade(&home);
    assert!(!out.status.success(), "{}", out.stdout);
    assert!(out.stderr.contains("exec /nonexistent"), "{}", out.stderr);
    assert!(c.wait_closed(s(5)), "connections are closed by the quiesce");
    let mut c2 = home.connect();
    hello(&mut c2, "sess-u5");
    let w = c2.request_ok(json!({"type":"wait","task_id":t,"budget_ms":15000}));
    assert_eq!(w["done"], true);
    c2.wait_event(s(3), |e| e["event"] == "task_exited" && e["task_id"] == t);
    let mut events = c.events.clone();
    events.extend(c2.events.clone());
    assert_eq!(assert_sequential(&event_text(&events, &t), "m", "events across a rollback"), 150);
    let st = status(&home);
    assert_eq!(st["generation"], 0);
    assert_eq!(st["last_upgrade"]["ok"], false);
    assert!(!pid_alive(pid) || group(pid).is_empty());
}

/// The new image failing to restore is a crash: the lifeline closes and
/// every task and grandchild is cleaned up (no crash recovery).
#[test]
fn u6_failed_restore_cleans_up_like_a_crash() {
    let home = Home::new("u6");
    let bin = home.install_copy();
    let d = home.start_daemon_from(&bin, &[("PI_FAMULUS_TEST_FAIL_RESTORE", "1")]);
    let mut c = home.connect();
    hello(&mut c, "sess-u6");
    let (task_id, pid) = start(&mut c, "shell", "sleep 300 & sleep 300", json!({}));
    std::thread::sleep(Duration::from_millis(300));
    let members = group(pid);
    assert!(
        members.len() >= 3,
        "runner, sh, sleeps: {members:?}\n{}",
        group_diagnostics(&home, &task_id, pid)
    );
    let out = upgrade(&home);
    assert!(!out.status.success(), "{}", out.stdout);
    assert!(out.stderr.contains("exited during the upgrade"), "{}", out.stderr);
    let mut d = d;
    assert!(wait_child(&mut d, s(5)).is_some(), "daemon exited");
    assert!(poll_true(s(5), || members.iter().all(|p| !pid_running(*p))), "left alive: {:?}", group(pid));
}

/// Replacing the binary file is enough: the daemon notices and upgrades.
#[test]
fn u7_replacing_the_binary_upgrades_by_itself() {
    let home = Home::new("u7");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u7");
    let (_t, pid) = start(&mut c, "shell", "sleep 300", json!({}));
    let before = status(&home);
    replace_binary(&bin, std::path::Path::new(BIN));
    let deadline = Instant::now() + s(15);
    let after = loop {
        std::thread::sleep(Duration::from_millis(250));
        // A request that lands while the daemon quiesces or execs is closed
        // ("manager closed the connection"; CI, 2026-09-30): the upgrade is
        // in progress, not failed. Ask again.
        let out = home.cli(&["status", "--json"], s(10));
        let st: Value = serde_json::from_str(&out.stdout).unwrap_or(Value::Null);
        if st["generation"] == 1 {
            break st;
        }
        assert!(Instant::now() < deadline, "no automatic upgrade: {} {}", out.stdout, out.stderr);
    };
    assert_eq!(after["pid"], before["pid"]);
    assert_eq!(after["last_upgrade"]["trigger"], "binary-changed");
    assert!(pid_alive(pid));
}

/// A stop's kill grace pending across the upgrade still ends in SIGKILL for
/// a command that ignores SIGTERM.
#[test]
fn u8_kill_grace_carries_over() {
    let home = Home::new("u8");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u8");
    let (t, pid) = start(&mut c, "shell", "trap '' TERM; sleep 300 & wait; sleep 300", json!({}));
    std::thread::sleep(Duration::from_millis(300));
    c.request_ok(json!({"type":"stop","task_id":t}));
    assert!(upgrade(&home).status.success());
    let members = group(pid);
    // On the manual clock the grace cannot elapse during the upgrade, so the
    // group must still be there and only the re-armed grace can end it. On
    // real time a slow upgrade (parallel load) may outlast the 2 s; the
    // outcome below must hold either way.
    if home.manual {
        assert!(
            !members.is_empty(),
            "SIGTERM-ignoring group still there after the upgrade\n{}",
            group_diagnostics(&home, &t, pid)
        );
    }
    // The new image re-armed the grace with what was left of it.
    home.advance_partial("kill-grace", 2_000);
    assert!(poll_true(s(5), || members.iter().all(|p| !pid_running(*p))), "not killed: {:?}", group(pid));
    let mut c2 = home.connect();
    hello(&mut c2, "sess-u8");
    let r = c2.wait_terminal(&t, s(5)).unwrap();
    assert_eq!((r["status"].as_str(), r["end_reason"].as_str()), (Some("killed"), Some("stopped:tool")), "{r}");
}

/// CLI commands in progress (`wait`, `output -f`) ride through an upgrade:
/// their request is resent on a new connection.
#[test]
fn u9_cli_wait_and_follow_survive_an_upgrade() {
    let home = Home::new("u9");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u9");
    let (t, _) = start(&mut c, "shell",
        "i=0; while [ $i -lt 60 ]; do echo f-$i; i=$((i+1)); if [ $((i % 2)) -eq 0 ]; then sleep 0.1; fi; done; exit 3", json!({}));
    let (h1, h2) = (home.path.clone(), home.path.clone());
    let (t1, t2) = (t.clone(), t.clone());
    let wait = std::thread::spawn(move || run_cli(&h1, &["wait", &t1, "--budget-ms", "20000"], s(30)));
    let follow = std::thread::spawn(move || run_cli(&h2, &["output", &t2, "-f"], s(30)));
    std::thread::sleep(Duration::from_millis(700));
    assert!(upgrade(&home).status.success());
    let w = wait.join().unwrap();
    assert!(w.status.success() && w.stdout.contains("done exit_code=3"), "wait: {} {}", w.stdout, w.stderr);
    let f = follow.join().unwrap();
    assert!(f.status.success(), "output -f: {}", f.stderr);
    assert_eq!(assert_sequential(&f.stdout, "f", "output -f across the upgrade"), 60);
    assert_eq!(status(&home)["generation"], 1);
}

/// Regression: a UTF-8 character split across pipe writes, with an upgrade
/// while only its first raw byte is on disk. The held-back tail survives
/// the park; the reconnected watcher gets no duplicate text or U+FFFD.
#[test]
fn u10_split_utf8_character_across_an_upgrade() {
    let home = Home::new("u10");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u10");
    let release = home.path.join("release-utf8-tail");
    // Each initial printf writes one byte. The continuation cannot be
    // produced until we explicitly release it after reconnecting.
    let command = format!("printf 'a'; printf '\\344'; while [ ! -e '{}' ]; do sleep 0.01; done; printf '\\270\\255b\\n'", release.display());
    let (t, _) = start(&mut c, "monitor", &command, json!({}));
    let first = c.wait_event(s(3), |e| e["event"] == "output" && e["task_id"] == t).expect("first half");
    assert_eq!(first["chunk"], "a", "{first:?}");
    assert_eq!(first["next_cursor"], 1);
    let output_path = home.path.join("sessions/sess-u10/tasks").join(format!("{t}.output"));
    assert!(poll_true(s(3), || std::fs::read(&output_path).ok().as_deref() == Some(&b"a\xe4"[..])),
        "tee must consume the incomplete raw byte before the upgrade");
    assert!(upgrade(&home).status.success());
    assert!(c.wait_closed(s(5)), "old connection closed (and read to the end)");
    let mut c2 = home.connect();
    hello(&mut c2, "sess-u10");
    assert_eq!(std::fs::read(&output_path).unwrap(), b"a\xe4");
    std::fs::write(&release, b"continue").unwrap();
    c2.request_ok(json!({"type":"wait","task_id":t,"budget_ms":10000}));
    c2.drain(Duration::from_millis(300));
    let mut events = c.events.clone();
    events.extend(c2.events.clone());
    assert_eq!(event_text(&events, &t), "a中b\n", "{events:?}");
    assert_eq!(std::fs::read(&output_path).unwrap(), b"a\xe4\xb8\xadb\n");
    let mut cursor = 0;
    for event in events.iter().filter(|e| e["event"] == "output" && e["task_id"] == t) {
        cursor += event["chunk"].as_str().unwrap().len() as u64;
        assert_eq!(event["next_cursor"].as_u64(), Some(cursor), "{event:?}");
    }
    assert_eq!(cursor, 6);
}

/// Upgrades back to back while a monitor streams: nothing lost or repeated.
#[test]
fn u11_repeated_upgrades_under_output() {
    let home = Home::new("u11");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u11");
    let (t, _) = start(&mut c, "monitor",
        "i=0; while [ $i -lt 400 ]; do echo r-$i; i=$((i+1)); if [ $((i % 10)) -eq 0 ]; then sleep 0.1; fi; done", json!({}));
    let mut events = Vec::new();
    for round in 1..=5u64 {
        let out = upgrade(&home);
        assert!(out.status.success(), "round {round}: {} {}", out.stdout, out.stderr);
        assert!(c.wait_closed(s(5)));
        events.extend(c.events.clone());
        c = home.connect();
        hello(&mut c, "sess-u11");
        assert_eq!(status(&home)["generation"], round);
    }
    c.request_ok(json!({"type":"wait","task_id":t,"budget_ms":15000}));
    c.drain(Duration::from_millis(300));
    events.extend(c.events.clone());
    assert_eq!(assert_sequential(&event_text(&events, &t), "r", "five upgrades"), 400);
}

/// Right after the exec nobody is connected yet: the zero-connection idle
/// rule (5 s) is held for the handover grace (30 s) so the reconnecting
/// clients find their tasks alive. After the grace the rule applies again
/// (checked on the manual clock only: real time would take 35 s).
#[test]
fn u12_idle_rule_waits_for_clients_after_an_upgrade() {
    let home = Home::new("u12");
    let bin = home.install_copy();
    let mut d = home.start_daemon_from(&bin, &[]);
    let mut c = home.connect();
    hello(&mut c, "sess-u12");
    let (_t, pid) = start(&mut c, "shell", "sleep 300", json!({}));
    assert!(upgrade(&home).status.success());
    assert!(c.wait_closed(s(5)));
    drop(c);
    // No client for longer than the idle grace.
    home.advance_now(6_000);
    settle();
    assert!(pid_alive(pid), "task killed by the idle rule during the handover grace");
    assert!(wait_child(&mut d, Duration::from_millis(10)).is_none(), "daemon exited during the handover grace");
    if home.manual {
        home.advance_partial("handover-grace", 30_000);
        home.advance("idle", 5_000);
        home.advance("shutdown-grace", 2_000);
        assert!(wait_child(&mut d, s(10)).is_some(), "idle rule applies again after the grace");
        assert!(poll_true(s(5), || !pid_running(pid)), "shutdown killed the task");
    }
}

/// Serve `home`'s socket as a manager from before in-place upgrade: replies
/// captured from the real daemon, then aged to protocol 2 (no upgrade
/// fields; `upgrade` is an unknown request, as the 2026-09-23 build said).
fn serve_pre_upgrade_manager(home: &Home) -> std::thread::JoinHandle<()> {
    let _d = home.start_daemon();
    let mut c = home.connect();
    let hello_ok = c.request(json!({"type":"hello","client_kind":"cli","protocol":2}));
    let mut status_ok = c.request(json!({"type":"status"}));
    let unknown = c.request(json!({"type":"no_such_request"}));
    assert_eq!(unknown["ok"], json!(false), "{unknown}");
    c.request(json!({"type":"shutdown"}));
    drop(c);
    assert!(poll_true(s(10), || !home.sock().exists()), "the real daemon went away");

    assert_eq!(status_ok["ok"], json!(true), "{status_ok}");
    status_ok["protocol"] = json!(2);
    status_ok["version"] = json!("0.1.0");
    for k in ["generation", "last_upgrade", "exe"] {
        status_ok.as_object_mut().unwrap().remove(k);
    }
    let mut upgrade_err = unknown.clone();
    upgrade_err["error"]["message"] = json!("bad request: unknown variant `upgrade`, expected one of `hello`, `start`, `status`, `shutdown`");
    let listener = std::os::unix::net::UnixListener::bind(home.sock()).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (hello_ok, status_ok, upgrade_err) = (hello_ok.clone(), status_ok.clone(), upgrade_err.clone());
            std::thread::spawn(move || {
                let mut c = Conn::new(stream);
                while let Recv::Frame(req) = c.recv(Instant::now() + s(30)) {
                    let mut reply = match req["type"].as_str() {
                        Some("hello") => hello_ok.clone(),
                        Some("status") => status_ok.clone(),
                        _ => upgrade_err.clone(),
                    };
                    reply["id"] = req["id"].clone();
                    c.send(&reply);
                }
            });
        }
    })
}

/// D26: `upgrade` against a manager too old to upgrade in place. Invariant:
/// the CLI says what is running and what to do (restart once), instead of
/// passing on the daemon's "unknown variant `upgrade`".
#[test]
fn u13_upgrade_explains_a_manager_that_predates_it() {
    let home = Home::new("u13");
    let _fake = serve_pre_upgrade_manager(&home);
    let out = upgrade(&home);
    assert_eq!(out.status.code(), Some(1), "{} {}", out.stdout, out.stderr);
    assert!(!out.stderr.contains("unknown variant"), "{}", out.stderr);
    assert!(out.stderr.contains("predates in-place upgrade"), "{}", out.stderr);
    assert!(out.stderr.contains("pi-famulus shutdown"), "{}", out.stderr);
}

/// D27: `upgrade` run from a binary other than the daemon's (a fresh
/// target/release while the daemon runs the installed copy). The daemon
/// execs the file at its own path, so the CLI says which file that is before
/// it asks; from the daemon's own binary there is nothing to point out.
#[test]
fn u14_upgrade_names_the_file_it_will_exec() {
    let home = Home::new("u14");
    let bin = home.install_copy();
    let _d = home.start_daemon_from(&bin, &[]);
    let out = upgrade(&home);
    assert!(out.status.success(), "{} {}", out.stdout, out.stderr);
    // exe_path canonicalizes the invoked path (resolving macOS /tmp symlink
    // prefixes like /private/var), so the daemon names its canonical path.
    let note = format!("upgrades to the file at its own path, {}", bin.canonicalize().unwrap().display());
    assert!(out.stderr.contains(&note), "{}", out.stderr);
    assert!(out.stderr.contains(BIN), "names this CLI: {}", out.stderr);

    // The same subcommand, run from the daemon's own file.
    let o = std::process::Command::new(&bin).arg("--home").arg(&home.path).arg("upgrade").output().unwrap();
    let own = (o.status, String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(own.0.success(), "{}", own.1);
    assert!(!own.1.contains("upgrades to the file"), "{}", own.1);
}
