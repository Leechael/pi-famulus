//! Black-box tests for the observability contract (manager + CLI side):
//! protocol additions (origin, mark_background, stop.reason, end_reason,
//! hello extension_version/protocol, status protocol), the manager's
//! events.jsonl writes, and the inspection CLI (ls/show/agent/events/
//! sessions/status/doctor/log/tail/output/stop).
//!
//! Extension-owned files (agent records, transcripts, extension events) are
//! written here as fixtures in the contract's format, since the extension
//! side may land later.

mod common;

use common::*;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

const S: fn(u64) -> Duration = Duration::from_secs;
const MS: fn(u64) -> Duration = Duration::from_millis;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Extension-style hello that announces the current protocol.
fn hello_v2(c: &mut Conn, session: &str, cwd: &str) -> Value {
    c.request_ok(json!({"type":"hello","client_kind":"extension","session_id":session,
        "pi_pid":std::process::id(),"cwd":cwd,"extension_version":"0.9.0-test","protocol":2}))
}

fn start(c: &mut Conn, cmd: &str, extra: Value) -> (String, u32) {
    let mut req = json!({"type":"start","kind":"shell","command":cmd,"cwd":"/tmp","env":{"PATH":PATH_ENV}});
    if let (Some(o), Some(e)) = (req.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    c.start_with(req)
}

fn events_of(home: &Home, sid: &str) -> Vec<Value> {
    let p = home.path.join("sessions").join(sid).join("events.jsonl");
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad event line ({e}): {l}")))
        .collect()
}

fn wait_event(home: &Home, sid: &str, ty: &str, id: Option<&str>) -> Value {
    poll_until(S(5), || {
        events_of(home, sid)
            .into_iter()
            .find(|e| e["type"] == ty && id.map_or(true, |i| e["id"] == i))
    })
    .unwrap_or_else(|| panic!("no {ty} event for {id:?} in {sid}: {:?}", events_of(home, sid)))
}

fn agent_fixture(home: &Home, sid: &str, rec: Value) {
    let dir = home.path.join("sessions").join(sid).join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let id = rec["child_id"].as_str().unwrap().to_string();
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&rec).unwrap()).unwrap();
}

fn transcript_fixture(home: &Home, sid: &str, child: &str, lines: &[Value]) -> PathBuf {
    let dir = home.path.join("sessions").join(sid).join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{child}.jsonl"));
    let mut f = std::fs::File::create(&p).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
    p
}

fn append_event(home: &Home, sid: &str, ev: Value) {
    let p = home.path.join("sessions").join(sid).join("events.jsonl");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
    f.write_all(format!("{ev}\n").as_bytes()).unwrap();
}

/// Minimal display width for assertions (CJK and fullwidth = 2).
fn width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let u = c as u32;
            if (0x4E00..=0x9FFF).contains(&u) || (0xFF00..=0xFF60).contains(&u) || (0x3040..=0x30FF).contains(&u) {
                2
            } else {
                1
            }
        })
        .sum()
}

fn cli_ok(home: &Home, args: &[&str]) -> CliOut {
    let out = home.cli(args, S(15));
    assert!(out.status.success(), "{args:?} failed: {}{}", out.stdout, out.stderr);
    out
}

// ===========================================================================
// Protocol additions
// ===========================================================================

/// start.origin is stored; mark_background records backgrounded_at once;
/// every way a task can end sets end_reason, and task_exited carries it.
#[test]
fn p1_origin_background_and_end_reasons() {
    let home = Home::new("p1");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-p1", "/tmp");

    let origin = json!({"via":"child-bash","child_id":"ch_0000beef","run_id":"run_0000cafe"});
    let (a, _) = start(&mut c, "sleep 300", json!({"origin": origin}));
    assert_eq!(c.task(&a).unwrap()["origin"], origin);

    let t0 = now_ms();
    c.request_ok(json!({"type":"mark_background","task_id":a}));
    let bg = c.task(&a).unwrap()["backgrounded_at"].as_u64().expect("backgrounded_at");
    assert!(bg >= t0 && bg <= now_ms() + 1000);
    std::thread::sleep(MS(20));
    c.request_ok(json!({"type":"mark_background","task_id":a}));
    assert_eq!(c.task(&a).unwrap()["backgrounded_at"].as_u64(), Some(bg), "first time is kept");
    assert_eq!(home.record(&a).unwrap()["backgrounded_at"].as_u64(), Some(bg), "persisted");

    // stop reasons
    let cases = [
        (Some("cli"), "stopped:cli"),
        (Some("tui"), "stopped:tui"),
        (Some("tool"), "stopped:tool"),
        (None, "stopped:tool"),
        (Some("timeout"), "timeout"),
        (Some("rate-limit"), "rate-limit"),
        (Some("session-end"), "session-end"),
    ];
    for (reason, want) in cases {
        let (id, _) = start(&mut c, "sleep 300", json!({}));
        let mut req = json!({"type":"stop","task_id":id});
        if let Some(r) = reason {
            req["reason"] = json!(r);
        }
        c.request_ok(req);
        let t = c.wait_terminal(&id, S(5)).unwrap();
        assert_eq!(t["end_reason"], want, "reason {reason:?}: {t}");
        assert_eq!(t["status"], "killed");
        let ev = c
            .wait_event(S(3), |e| e["event"] == "task_exited" && e["task_id"] == json!(id))
            .unwrap();
        assert_eq!(ev["end_reason"], want, "task_exited for {reason:?}: {ev}");
    }
    let r = c.request(json!({"type":"stop","task_id":a,"reason":"because"}));
    assert_eq!(r["error"]["code"], "E_BAD_REQUEST", "{r}");
    assert_eq!(c.status_of(&a).as_deref(), Some("running"), "a rejected stop does nothing");

    // natural exits (any code) and the hard timeout
    let (ok, _) = start(&mut c, "true", json!({}));
    let (bad, _) = start(&mut c, "exit 3", json!({}));
    let (slow, _) = start(&mut c, "sleep 300", json!({"timeout_ms": 200}));
    assert_eq!(c.wait_terminal(&ok, S(3)).unwrap()["end_reason"], "exited");
    assert_eq!(c.wait_terminal(&bad, S(3)).unwrap()["end_reason"], "exited");
    assert_eq!(c.wait_terminal(&slow, S(3)).unwrap()["end_reason"], "timeout");

    // shutdown_session: session-end
    let (x, _) = start(&mut c, "sleep 300", json!({}));
    c.request_ok(json!({"type":"shutdown_session"}));
    assert_eq!(c.wait_terminal(&x, S(5)).unwrap()["end_reason"], "session-end");
    assert_eq!(c.wait_terminal(&a, S(5)).unwrap()["end_reason"], "session-end");
    // The first reason wins: a task that ignores the cli stop's SIGTERM is
    // still running when shutdown_session arrives, and keeps "stopped:cli".
    let (y, _) = start(&mut c, "trap '' TERM; echo armed; sleep 300", json!({}));
    assert!(poll_true(S(3), || c.task(&y).unwrap()["output_size"].as_u64().unwrap_or(0) > 0));
    c.request_ok(json!({"type":"stop","task_id":y,"reason":"cli"}));
    c.request_ok(json!({"type":"shutdown_session"}));
    home.advance("kill-grace", 2000);
    assert_eq!(c.wait_terminal(&y, S(6)).unwrap()["end_reason"], "stopped:cli");
}

/// manager-crash and manager-shutdown end reasons.
#[test]
fn p2_end_reasons_across_manager_lifecycle() {
    let home = Home::new("p2");
    let mut d1 = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-p2", "/tmp");
    let (crashed, runner_pid) = start(&mut c, "echo $$; exec sleep 300", json!({}));
    let cmd_pid = wait_for_pids(&mut c, &crashed, 1)[0];
    drop(c);
    d1.kill().unwrap();
    d1.wait().unwrap();
    assert!(
        poll_true(S(4), || !pid_running(runner_pid) && !pid_running(cmd_pid)),
        "the lifeline takes the task down, command included"
    );

    let mut d2 = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-p2", "/tmp");
    assert_eq!(c.task(&crashed).unwrap()["end_reason"], "manager-crash");
    let ev = wait_event(&home, "sess-p2", "task.exit", Some(&crashed));
    assert_eq!(ev["end_reason"], "manager-crash");
    let (keep, _) = start(&mut c, "sleep 300", json!({}));
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut d2, S(10)).is_some());
    assert_eq!(home.record(&keep).unwrap()["end_reason"], "manager-shutdown");
    let ev = wait_event(&home, "sess-p2", "task.exit", Some(&keep));
    assert_eq!(ev["end_reason"], "manager-shutdown");
}

/// hello's extension_version/protocol are stored per session; status
/// returns the manager's protocol and session timing.
#[test]
fn p3_hello_protocol_and_status() {
    let home = Home::new("p3");
    let _d = home.start_daemon();
    let t0 = now_ms();
    let mut a = home.connect();
    let hello = hello_v2(&mut a, "sess-p3", "/tmp/p3");
    assert_eq!(hello["protocol"], 5, "hello reports maximum supported protocol");
    let mut cli = home.connect();
    cli.hello_cli();
    let st = cli.request_ok(json!({"type":"status"}));
    assert_eq!(st["protocol"], 5, "{st}");
    let s = &st["sessions"][0];
    assert_eq!((s["protocol"].as_u64(), s["extension_version"].as_str()), (Some(2), Some("0.9.0-test")), "{st}");
    let since = s["connected_at"].as_u64().unwrap();
    assert!(since >= t0 && since <= now_ms());
    drop(a);
    assert!(poll_true(S(3), || {
        let st = cli.request_ok(json!({"type":"status"}));
        st["sessions"][0]["connected"] == false
    }));
    let st = cli.request_ok(json!({"type":"status"}));
    let seen = st["sessions"][0]["last_seen"].as_u64().unwrap();
    assert!(seen >= since && seen <= now_ms(), "{st}");
    // reconnecting keeps the first connected_at
    let mut b = home.connect();
    hello_v2(&mut b, "sess-p3", "/tmp/p3");
    let st = cli.request_ok(json!({"type":"status"}));
    assert_eq!(st["sessions"][0]["connected_at"].as_u64(), Some(since));
}

/// A fixed amount of CPU work, never a sleep (wall time is not CPU, and
/// macOS CI rounds sleeps). About 0.4 s of user time on an M-series Mac.
const BURN: &str = "awk 'BEGIN{for(i=0;i<10000000;i++)s+=i}'";
/// Well under BURN's cost, well over what the idle shells around it use.
const BURN_FLOOR_MS: u64 = 100;

fn cpu_user_ms(v: &Value) -> Option<u64> {
    v["cpu_user_ms"].as_u64()
}

/// Exit-time CPU accounting: the record (live and on disk) and the
/// `task.exit` line carry the CPU of the command's whole wait chain, a
/// cooperative stop still reports it, and a runner SIGKILLed by the hard
/// timeout reports nothing (absent, not zero).
#[test]
fn p4_cpu_usage_on_record_event_and_list() {
    let home = Home::new("p4");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-p4", "/tmp");

    // Only a grandchild burns: neither shell above it can exec away.
    let (a, _) = start(&mut c, &format!("sh -c \"{BURN}; true\"; true"), json!({}));
    let t = c.wait_terminal(&a, S(30)).unwrap();
    assert_eq!(t["status"], "completed", "{t}");
    let cpu = cpu_user_ms(&t).unwrap_or_else(|| panic!("no cpu_user_ms: {t}"));
    assert!(cpu >= BURN_FLOOR_MS, "grandchild CPU missing: {t}");
    assert!(t["cpu_sys_ms"].is_u64() && t["max_rss_kb"].as_u64().unwrap_or(0) > 0, "{t}");
    let disk = home.record(&a).unwrap();
    assert_eq!(cpu_user_ms(&disk), Some(cpu), "persisted: {disk}");
    let exit = wait_event(&home, "sess-p4", "task.exit", Some(&a));
    assert_eq!(cpu_user_ms(&exit), Some(cpu), "{exit}");
    assert_eq!(exit["max_rss_kb"], t["max_rss_kb"], "{exit}");
    let rows: Value = serde_json::from_str(&cli_ok(&home, &["ls", "--all", "--json"]).stdout).unwrap();
    let row = rows.as_array().unwrap().iter().find(|r| r["id"] == a).unwrap();
    assert_eq!(cpu_user_ms(row), Some(cpu), "ls --json: {row}");
    let shown = cli_ok(&home, &["show", &a]).stdout;
    let line = shown.lines().find(|l| l.starts_with("cpu:")).unwrap_or_else(|| panic!("no cpu line:\n{shown}"));
    assert!(line.contains("cores avg") && line.contains("peak rss"), "{line}");
    let st: Value = serde_json::from_str(&cli_ok(&home, &["stats", "--json", "--session", "sess-p4"]).stdout).unwrap();
    assert_eq!(st[0]["agent"], "main sess-p4", "{st}");
    assert_eq!(st[0]["cpu_user_ms"].as_u64(), Some(cpu), "stats from the live daemon: {st}");

    // A stop whose SIGTERM the command obeys: the runner survives it and
    // reports what was spent before.
    let (b, _) = start(&mut c, &format!("{BURN}; echo burned; sleep 300"), json!({}));
    assert!(poll_true(S(30), || c.task(&b).unwrap()["output_size"].as_u64().unwrap_or(0) > 0));
    c.request_ok(json!({"type":"stop","task_id":b,"reason":"cli"}));
    let t = c.wait_terminal(&b, S(5)).unwrap();
    assert_eq!(t["status"], "killed", "{t}");
    assert!(cpu_user_ms(&t).unwrap_or(0) >= BURN_FLOOR_MS, "stopped task lost its CPU: {t}");

    // The hard timeout SIGKILLs the whole group, runner included: no report.
    let (x, _) = start(&mut c, "sleep 300", json!({"timeout_ms": 200}));
    let t = c.wait_terminal(&x, S(5)).unwrap();
    assert_eq!(t["end_reason"], "timeout", "{t}");
    for k in ["cpu_user_ms", "cpu_sys_ms", "max_rss_kb"] {
        assert!(t.get(k).is_none(), "{k} without a report: {t}");
    }
    let exit = wait_event(&home, "sess-p4", "task.exit", Some(&x));
    assert!(exit.get("cpu_user_ms").is_none(), "{exit}");
    let shown = cli_ok(&home, &["show", &x]).stdout;
    assert!(shown.contains("cpu:          not measured"), "{shown}");
}

// ===========================================================================
// events.jsonl (manager writes)
// ===========================================================================

#[test]
fn e1_manager_writes_session_and_task_events() {
    let home = Home::new("e1");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-e1", "/tmp/e1");
    let long_cmd = format!("sleep 300 # {}", "x".repeat(1000));
    let (a, pid) = start(&mut c, &long_cmd, json!({"origin":{"via":"bash-fg"}}));
    c.request_ok(json!({"type":"mark_background","task_id":a}));
    c.request_ok(json!({"type":"stop","task_id":a,"reason":"tui"}));
    c.wait_terminal(&a, S(5)).unwrap();
    drop(c);
    home.advance("idle", 5000);
    assert!(wait_child(&mut d, S(12)).is_some());

    let evs = events_of(&home, "sess-e1");
    let types: Vec<&str> = evs.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        ["session.connect", "task.start", "task.background", "task.stop", "task.exit", "session.disconnect"],
        "{evs:#?}"
    );
    for e in &evs {
        assert_eq!(e["src"], "manager");
        assert!(e["ts"].as_u64().is_some());
    }
    assert_eq!(evs[0]["pi_pid"], std::process::id());
    assert_eq!(evs[0]["cwd"], "/tmp/e1");
    assert_eq!(evs[0]["protocol"], 2);
    let start = &evs[1];
    assert_eq!((start["id"].as_str(), start["kind"].as_str()), (Some(a.as_str()), Some("shell")));
    assert_eq!(start["origin"], json!({"via":"bash-fg"}));
    assert_eq!(start["pid"], pid);
    let cmd = start["command"].as_str().unwrap();
    assert_eq!(cmd.chars().count(), 200, "command clipped to 200 chars");
    assert!(cmd.ends_with('…'));
    assert_eq!(evs[3]["reason"], "tui");
    let exit = &evs[4];
    assert_eq!(exit["end_reason"], "stopped:tui");
    assert!(exit["signal"].is_string() && exit["exit_code"].is_null() && exit["duration_ms"].is_u64(), "{exit}");
    // daemon-level events live in <home>/events.jsonl
    let daemon: Vec<Value> = std::fs::read_to_string(home.path.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let dt: Vec<&str> = daemon.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(dt, ["daemon.start", "daemon.shutdown"]);
    assert_eq!(daemon[0]["protocol"], 5);
}

/// Oversized fields are truncated so every line stays below 4 KiB.
#[test]
fn e2_event_lines_stay_below_4k() {
    let home = Home::new("e2");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-e2", &"/tmp/".repeat(2000));
    let origin = json!({"via":"child-bash","child_id":"c".repeat(9000),"run_id":"r".repeat(9000)});
    let (a, _) = start(&mut c, "true", json!({"origin": origin}));
    c.wait_terminal(&a, S(3)).unwrap();
    wait_event(&home, "sess-e2", "task.exit", Some(&a));
    let raw = std::fs::read_to_string(home.path.join("sessions/sess-e2/events.jsonl")).unwrap();
    for line in raw.lines() {
        assert!(line.len() + 1 < 4096, "line of {} bytes", line.len());
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(v["type"].is_string() && v["ts"].is_u64());
    }
    let start_ev = events_of(&home, "sess-e2").into_iter().find(|e| e["type"] == "task.start").unwrap();
    assert_eq!(start_ev["truncated"], true);
    assert_eq!(start_ev["id"], a, "the id is never truncated");
    let connect = events_of(&home, "sess-e2").into_iter().find(|e| e["type"] == "session.connect").unwrap();
    assert_eq!(connect["truncated"], true);
}

/// Concurrent appends from the manager and several extension-style writers
/// never interleave: every line parses and nothing is lost.
#[test]
fn e3_concurrent_appends_never_interleave() {
    let home = Home::new("e3");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-e3", "/tmp");
    let path = home.path.join("sessions/sess-e3/events.jsonl");
    const WRITERS: usize = 8;
    const LINES: usize = 300;
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let path = path.clone();
            std::thread::spawn(move || {
                for i in 0..LINES {
                    // ~1-3.5 KiB lines, written the way the extension must:
                    // one O_APPEND write per line.
                    let pad = "p".repeat(1000 + (i * 37 + w * 101) % 2500);
                    let line = format!(
                        "{}\n",
                        json!({"ts":now_ms(),"src":"extension","type":"wake.emit","id":format!("w{w}-{i}"),"ids":[format!("w{w}-{i}")],"pad":pad})
                    );
                    assert!(line.len() < 4096);
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
                    assert_eq!(f.write(line.as_bytes()).unwrap(), line.len());
                }
            })
        })
        .collect();
    // Meanwhile the manager writes its own events.
    let mut ids = Vec::new();
    for _ in 0..40 {
        ids.push(start(&mut c, "true", json!({})).0);
    }
    for w in writers {
        w.join().unwrap();
    }
    for id in &ids {
        c.wait_terminal(id, S(5)).unwrap();
    }
    for id in &ids {
        wait_event(&home, "sess-e3", "task.exit", Some(id));
    }
    let evs = events_of(&home, "sess-e3"); // panics on any malformed line
    let ext = evs.iter().filter(|e| e["src"] == "extension").count();
    let starts = evs.iter().filter(|e| e["type"] == "task.start").count();
    let exits = evs.iter().filter(|e| e["type"] == "task.exit").count();
    assert_eq!((ext, starts, exits), (WRITERS * LINES, 40, 40));
    // The CLI reads it back without complaint.
    let out = cli_ok(&home, &["events", "--session", "sess-e3", "--json"]);
    assert_eq!(out.stdout.lines().count(), evs.len());
    assert!(out.stderr.is_empty(), "{}", out.stderr);
}

/// A task's event lines are on disk, in causal order, before any client can
/// see the change they record. Once `list` (or `wait`) shows a task as
/// finished, its `task.exit` line is already in the session's events.jsonl,
/// after its `task.start`. A client that reacts (e.g. disconnects) therefore
/// never gets its own line in ahead of it: that race flaked `e1` under
/// full-suite load. `list` is polled from a second connection because it is
/// served by another worker as soon as the state lock is free, which is the
/// path that raced.
#[test]
fn e5_event_lines_precede_the_state_they_record() {
    let home = Home::new("e5");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-e5", "/tmp");
    let mut cli = home.connect();
    cli.hello_cli();
    for i in 0..100 {
        let (id, _) = start(&mut c, "true", json!({}));
        cli.wait_terminal(&id, S(5)).expect("task finished");
        let evs = events_of(&home, "sess-e5");
        let pos = |ty: &str| evs.iter().position(|e| e["type"] == ty && e["id"] == json!(id));
        let (s, x) = (pos("task.start"), pos("task.exit"));
        assert!(x.is_some(), "round {i}: task shown as finished, but task.exit is not written yet");
        assert!(s < x, "round {i}: task.exit written before task.start");
    }
}

/// `events`: malformed lines are skipped (with a note), filters work, the
/// merge across sessions is time-ordered, and -f picks up new lines.
#[test]
fn e4_events_cli_filters_and_skips_malformed() {
    let home = Home::new("e4");
    let base = now_ms() - 60_000;
    append_event(&home, "sess-aaaaaaaa-1", json!({"ts":base + 3,"src":"extension","type":"wake.emit","ids":["sh_11111111"],"batch":1}));
    append_event(&home, "sess-bbbbbbbb-2", json!({"ts":base + 1,"src":"manager","type":"task.start","id":"sh_11111111","kind":"shell"}));
    let p = home.path.join("sessions/sess-aaaaaaaa-1/events.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(b"this is not json\n{\"no\":\"ts\"}\n[1,2]\n").unwrap();
    drop(f);
    append_event(&home, "sess-aaaaaaaa-1", json!({"ts":base + 2,"src":"extension","type":"wake.deliver","id":"sh_22222222","mode":"steer"}));
    append_event(&home, "sess-aaaaaaaa-1", json!({"ts":base - 3_600_000,"src":"extension","type":"agent.start","child_id":"ch_33333333"}));

    let out = cli_ok(&home, &["events"]);
    assert!(out.stderr.contains("skipped 3 malformed"), "{}", out.stderr);
    let types: Vec<&str> = out.stdout.lines().map(|l| l.split_whitespace().nth(4).unwrap()).collect();
    assert_eq!(types, ["agent.start", "task.start", "wake.deliver", "wake.emit"], "{}", out.stdout);

    let out = cli_ok(&home, &["events", "--id", "sh_11111111", "--json"]);
    let got: Vec<Value> = out.stdout.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(got.len(), 2, "id matches `id` and `ids[]`: {}", out.stdout);
    assert_eq!(got[0]["session"], "sess-bbbbbbbb-2");
    let out = cli_ok(&home, &["events", "--id", "ch_33333333"]);
    assert_eq!(out.stdout.lines().count(), 1, "child_id matches too");
    let out = cli_ok(&home, &["events", "--session", "sess-aaaa"]);
    assert_eq!(out.stdout.lines().count(), 3);
    let out = cli_ok(&home, &["events", "--since", "30m"]);
    assert_eq!(out.stdout.lines().count(), 3, "the hour-old event is filtered out");
    // no daemon was started by any of this
    assert!(!home.sock().exists());

    // -f follows new lines
    let mut child = std::process::Command::new(BIN)
        .args(["--home", home.path.to_str().unwrap(), "events", "-f", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Lines arrive through a channel so a missed event fails the test
    // instead of blocking it forever.
    let rd = std::io::BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in std::io::BufRead::lines(rd).map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    for _ in 0..4 {
        rx.recv_timeout(S(10)).expect("history line from events -f");
    }
    // Appended the moment history is out: a line written while the
    // follower starts must not fall between history and following.
    append_event(&home, "sess-new", json!({"ts":now_ms(),"src":"extension","type":"monitor.drop","id":"mon_1","lines":7}));
    let line = rx.recv_timeout(S(5));
    let _ = child.kill();
    let _ = child.wait();
    let line = line.expect("events -f never printed the line appended right after its history");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!((v["type"].as_str(), v["session"].as_str(), v["lines"].as_u64()), (Some("monitor.drop"), Some("sess-new"), Some(7)));
}

// ===========================================================================
// CLI: ls / show / agent / sessions / status
// ===========================================================================

fn record_fixture(home: &Home, sid: &str, id: &str, started_at: u64, extra: Value) {
    let dir = home.path.join("sessions").join(sid).join("tasks");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(format!("{id}.output"));
    std::fs::write(&out, b"old output\n").unwrap();
    let mut rec = json!({"task_id":id,"session_id":sid,"kind":"shell","command":"echo old","cwd":"/tmp",
        "pid":1,"status":"completed","exit_code":0,"signal":null,"started_at":started_at,
        "ended_at":started_at + 1000,"output_path":out,"output_size":11,"end_reason":"exited"});
    for (k, v) in extra.as_object().unwrap() {
        rec[k] = v.clone();
    }
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&rec).unwrap()).unwrap();
}

#[test]
fn c1_ls_columns_filters_json_and_cjk() {
    let home = Home::new("c1");
    // Two sessions sharing their first 8 characters, and an old record.
    record_fixture(&home, "0199aaaa-1111", "sh_0000a001", now_ms() - 7_200_000, json!({"cwd":"/tmp/elsewhere"}));
    agent_fixture(&home, "0199aaaa-2222", json!({"v":1,"kind":"agent","child_id":"ch_0000b001","run_id":"run_0000c001",
        "session_id":"0199aaaa-2222","name":"alpha","agent":"worker","model":"m1","status":"completed",
        "started_at":now_ms() - 5000,"ended_at":now_ms() - 1000,"end_reason":"completed"}));
    let _d = home.start_daemon();
    let mut c = home.connect();
    let cwd = std::env::temp_dir().canonicalize().unwrap();
    hello_v2(&mut c, "0199aaaa-2222", cwd.to_str().unwrap());
    let cjk = format!("echo {}", "中文".repeat(60));
    let (a, _) = c.start_with(json!({"type":"start","kind":"shell","command":cjk,"cwd":cwd,"env":{"PATH":PATH_ENV}}));
    let (b, _) = c.start_with(json!({"type":"start","kind":"shell","command":"sleep 300\nsecond line","cwd":cwd,"env":{"PATH":PATH_ENV}}));
    c.wait_terminal(&a, S(3)).unwrap();

    let out = cli_ok(&home, &["ls", "--all"]);
    let lines: Vec<&str> = out.stdout.lines().collect();
    let header: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(header, ["ID", "KIND", "SESSION", "CWD", "STATUS", "TIME", "DUR", "CPU", "CORES", "NOW", "EXIT", "REASON", "TITLE"]);
    let rows: Vec<&str> = lines[1..].to_vec();
    // The connected session's work, running and finished. 0199aaaa-1111 never
    // connected, so its finished task is not listed (still reachable via show).
    assert_eq!(rows.len(), 3, "{}", out.stdout);
    assert!(!out.stdout.contains("sh_0000a001"), "{}", out.stdout);
    assert!(cli_ok(&home, &["show", "sh_0000a001"]).stdout.contains("sh_0000a001"), "gone work stays inspectable");
    let running = rows.iter().find(|r| r.starts_with(&b)).expect("running task listed");
    // SESSION: shortest unique prefix, at least 8 chars
    assert!(running.contains("0199aaaa-2 ") && running.ends_with("sleep 300"), "{}", out.stdout);
    let agent = rows.iter().find(|r| r.starts_with("ch_0000b001")).expect("agents are listed");
    assert!(agent.contains(" agent ") && agent.contains("alpha (worker) m1") && agent.contains("completed"), "{agent}");
    // CJK: TITLE truncated by display width, all rows the same width budget
    let cjk_row = rows.iter().find(|r| r.starts_with(&a)).unwrap();
    assert!(cjk_row.ends_with('…'), "{cjk_row}");
    let title_start = lines[0].find("TITLE").unwrap();
    let title = &cjk_row[cjk_row.char_indices().nth(lines[0][..title_start].chars().count()).unwrap().0..];
    assert!(width(title) <= 60, "title {} columns: {title}", width(title));
    assert!(cjk_row.contains("exited") && cjk_row.contains(" 0 "), "{cjk_row}");
    // CPU / CORES: measured for the finished task, live samples for running tasks.
    let cols = |row: &str| -> (String, String, String) {
        let f: Vec<&str> = row.split_whitespace().collect();
        (f[7].to_string(), f[8].to_string(), f[9].to_string())
    };
    let (cpu, cores, now) = cols(cjk_row);
    assert!(cpu.ends_with('s') && cpu[..cpu.len() - 1].parse::<f64>().is_ok(), "CPU {cpu:?}: {cjk_row}");
    assert!(cores.parse::<f64>().is_ok(), "CORES {cores:?}: {cjk_row}");
    assert_eq!(now, "-", "terminal tasks have no recent sample: {cjk_row}");
    assert_eq!(cols(running), ("-".to_string(), "-".to_string(), "-".to_string()), "{running}");

    // filters
    let ids = |args: &[&str]| -> Vec<String> {
        let out = cli_ok(&home, args);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        v.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
    };
    let mut all = ids(&["ls", "--all", "--json"]);
    all.sort();
    let mut want = vec![a.clone(), b.clone(), "ch_0000b001".into()];
    want.sort();
    assert_eq!(all, want);
    assert_eq!(ids(&["ls", "--all", "--json", "--session", "0199aaaa-2"]).len(), 3);
    assert!(ids(&["ls", "--all", "--json", "--session", "0199aaaa-1"]).is_empty());
    assert_eq!(ids(&["ls", "--all", "--json", "--since", "1h"]).len(), 3);
    let mut in_cwd = ids(&["ls", "--all", "--json", "--cwd", cwd.to_str().unwrap()]);
    in_cwd.sort();
    let mut want_cwd = vec![a.clone(), b.clone(), "ch_0000b001".into()];
    want_cwd.sort();
    assert_eq!(in_cwd, want_cwd, "agents inherit their session's cwd");
    let out = cli_ok(&home, &["ls", "--all", "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let row = v.as_array().unwrap().iter().find(|r| r["id"] == b).unwrap();
    assert_eq!((row["kind"].as_str(), row["status"].as_str(), row["title"].as_str()), (Some("shell"), Some("running"), Some("sleep 300")));
    // A task's work kind (only setup noise here: `other`); agents have none.
    assert_eq!(row["work_kind"], "other", "{row}");
    let agent = v.as_array().unwrap().iter().find(|r| r["id"] == "ch_0000b001").unwrap();
    assert!(agent.get("work_kind").is_none(), "{agent}");
    let bad = home.cli(&["ls", "--since", "10x"], S(5));
    assert!(!bad.status.success());
}

/// `stats` from records on disk (no daemon, and it starts none): every
/// retained session counts, gone ones included; tasks group by their
/// child (named from its agent record) or the session's main agent, and by
/// work kind; CPU sums measured tasks, the rest are UNMEASURED.
#[test]
fn c11_stats_by_agent_and_kind() {
    let home = Home::new("c11");
    let now = now_ms();
    let child = |c: &str| json!({"via":"child-bash","child_id":c,"run_id":"run_0000d001"});
    record_fixture(&home, "0199dddd-1111", "sh_0000d001", now - 60_000, json!({"command":"cd x && pdm run test > log 2>&1; tail -n 50 log",
        "origin":child("ch_0000d001"),"ended_at":now - 20_000,"cpu_user_ms":150_000,"cpu_sys_ms":10_000,"max_rss_kb":900_000}));
    record_fixture(&home, "0199dddd-1111", "sh_0000d002", now - 50_000, json!({"command":"pytest tests/a.py",
        "origin":child("ch_0000d001"),"ended_at":now - 40_000,"status":"killed","end_reason":"timeout"}));
    record_fixture(&home, "0199dddd-1111", "sh_0000d003", now - 30_000, json!({"command":"git status",
        "ended_at":now - 29_000,"cpu_user_ms":5,"cpu_sys_ms":5,"max_rss_kb":10}));
    record_fixture(&home, "0199dddd-2222", "sh_0000d004", now - 3 * 3_600_000, json!({"command":"rg foo",
        "origin":child("ch_0000d002"),"cpu_user_ms":20,"cpu_sys_ms":0,"max_rss_kb":10}));
    agent_fixture(&home, "0199dddd-1111", json!({"child_id":"ch_0000d001","run_id":"run_0000d001","session_id":"0199dddd-1111",
        "name":"wave2-kms","agent":"worker","status":"completed","started_at":now - 70_000}));

    let json_of = |args: &[&str]| -> Vec<Value> {
        let out = cli_ok(&home, args);
        serde_json::from_str::<Value>(&out.stdout).unwrap().as_array().unwrap().clone()
    };
    let g = json_of(&["stats", "--by", "agent,kind", "--json"]);
    let find = |agent: &str, kind: &str| g.iter().find(|r| r["agent"] == agent && r["kind"] == kind).cloned()
        .unwrap_or_else(|| panic!("no {agent}/{kind} in {g:#?}"));
    let suite = find("wave2-kms (ch_0000d001)", "test-suite");
    assert_eq!((suite["tasks"].as_u64(), suite["cpu_ms"].as_u64(), suite["wall_ms"].as_u64()), (Some(1), Some(160_000), Some(40_000)));
    assert_eq!(suite["avg_cores"].as_f64(), Some(4.0));
    let killed = find("wave2-kms (ch_0000d001)", "test");
    assert_eq!((killed["measured"].as_u64(), killed["killed"].as_u64(), killed["killed_wall_ms"].as_u64()), (Some(0), Some(1), Some(10_000)));
    assert!(killed.get("avg_cores").is_none(), "nothing measured: {killed}");
    let main = g.iter().find(|r| r["kind"] == "git").unwrap();
    assert!(main["agent"].as_str().unwrap().starts_with("main 0199dddd-1"), "{main}");
    assert!(main.get("child_id").is_none(), "{main}");
    // The gone session's task counts too; its child has no record: the id.
    find("ch_0000d002", "read/search");
    assert_eq!(g[0]["kind"], "test-suite", "sorted by CPU");

    let by_agent = json_of(&["stats", "--json"]);
    let kms = by_agent.iter().find(|r| r["child_id"] == "ch_0000d001").unwrap();
    assert_eq!((kms["tasks"].as_u64(), kms["measured"].as_u64(), kms["killed"].as_u64()), (Some(2), Some(1), Some(1)));
    assert!(kms.get("kind").is_none());
    // The killed, unmeasured task's 10 s must not dilute CORES: 160 s of
    // CPU over the 40 s of measured wall, not over all 50 s.
    assert_eq!(kms["avg_cores"].as_f64(), Some(4.0), "{kms}");
    let text = cli_ok(&home, &["stats"]).stdout;
    let kms_line: Vec<&str> = text.lines().find(|l| l.starts_with("wave2-kms")).unwrap().split_whitespace().collect();
    assert_eq!(kms_line[2..], ["2", "50s", "2m40s", "4.0", "1", "1", "10s"], "{text}");

    assert_eq!(json_of(&["stats", "--by", "kind", "--json", "--session", "0199dddd-2"]).len(), 1);
    let recent = json_of(&["stats", "--by", "kind", "--json", "--since", "1h"]);
    assert!(recent.iter().all(|r| r["kind"] != "read/search"), "--since drops the 3h-old task: {recent:#?}");

    let text = cli_ok(&home, &["stats", "--by", "kind"]).stdout;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["KIND", "TASKS", "WALL", "CPU", "CORES", "UNMEASURED", "KILLED", "KILLED-WALL"]);
    let suite_line: Vec<&str> = lines.iter().find(|l| l.starts_with("test-suite")).unwrap().split_whitespace().collect();
    assert_eq!(suite_line[1..], ["1", "40s", "2m40s", "4.0", "0", "0", "0ms"], "{text}");
    let total: Vec<&str> = lines.last().unwrap().split_whitespace().collect();
    assert_eq!(total[..2], ["TOTAL", "4"], "{text}");

    let bad = home.cli(&["stats", "--by", "model"], S(5));
    assert!(!bad.status.success() && bad.stderr.contains("bad --by"), "{}", bad.stderr);
    assert!(!home.sock().exists(), "stats must not start the daemon");
}

/// Run the CLI with its stdout on a pseudo-terminal (script(1)), the way a
/// person runs it, and return what reached the terminal. None when `script`
/// isn't runnable here (missing, or no pty available), so a minimal host
/// skips just the pager test instead of failing to launch it.
fn cli_on_tty(home: &Home, args: &[&str], env: &[(&str, &str)]) -> Option<String> {
    let mut argv: Vec<String> = vec![BIN.into(), "--home".into(), home.path.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let mut cmd = std::process::Command::new("script");
    if cfg!(target_os = "macos") {
        cmd.arg("-q").arg("/dev/null").args(&argv);
    } else {
        let quoted: Vec<String> = argv.iter().map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect();
        cmd.arg("-qec").arg(quoted.join(" ")).arg("/dev/null");
    }
    let out = cmd.envs(env.iter().copied()).stdin(std::process::Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pager: on a terminal, listings go through `PI_FAMULUS_PAGER` / `PAGER` (default
/// `less -FRX`); never when stdout is not a terminal, with `--no-pager`, or
/// when following.
#[test]
fn c10_pager_on_a_terminal_only() {
    let home = Home::new("c10");
    append_event(&home, "sess-c10", json!({"ts":now_ms(),"src":"manager","type":"task.start","id":"sh_0000c10a"}));
    let pager = [("PI_FAMULUS_PAGER", "sed s/^/PAGED:/"), ("PAGER", "false")];

    let Some(paged) = cli_on_tty(&home, &["events"], &pager) else {
        eprintln!("skipping c10_pager_on_a_terminal_only: script(1) is not runnable here");
        return;
    };
    assert!(paged.contains("PAGED:") && paged.contains("sh_0000c10a"), "{paged:?}");
    let via_pager = cli_on_tty(&home, &["events"], &[("PI_FAMULUS_PAGER", ""), ("PAGER", "sed s/^/PAGER:/")]).unwrap();
    assert!(via_pager.contains("PAGER:"), "PAGER is the fallback: {via_pager:?}");

    let plain = cli_on_tty(&home, &["--no-pager", "events"], &pager).unwrap();
    assert!(plain.contains("sh_0000c10a") && !plain.contains("PAGED:"), "{plain:?}");
    let cat = cli_on_tty(&home, &["events"], &[("PI_FAMULUS_PAGER", "cat")]).unwrap();
    assert!(cat.contains("sh_0000c10a"), "{cat:?}");
    let piped = run_cli_env(&home.path, &["events"], S(5), &pager);
    assert!(piped.stdout.contains("sh_0000c10a") && !piped.stdout.contains("PAGED:"), "not a terminal: {}", piped.stdout);
}

/// ls order and default: only running work unless `--all`; running first,
/// then finished, each newest first. An agent's time is its last transcript
/// message (a long-running agent that just spoke is recent), not its start.
#[test]
fn c1b_ls_running_first_newest_first_agents_by_last_message() {
    let home = Home::new("c1b");
    let sid = "sess-c1b";
    let now = now_ms();
    record_fixture(&home, sid, "sh_0000f001", now - 2 * 3_600_000, json!({}));
    agent_fixture(&home, sid, json!({"child_id":"ch_0000a0ld","session_id":sid,"name":"old","agent":"w",
        "status":"completed","started_at":now - 3 * 3_600_000,"ended_at":now - 10_000}));
    transcript_fixture(&home, sid, "ch_0000a0ld", &[
        json!({"ts":now - 3 * 3_600_000,"role":"user","text":"go"}),
        json!({"ts":now - 10_000,"role":"assistant","text":"done"}),
    ]);
    agent_fixture(&home, sid, json!({"child_id":"ch_0000a0run","session_id":sid,"name":"busy","agent":"w",
        "status":"running","started_at":now - 5 * 3_600_000}));
    transcript_fixture(&home, sid, "ch_0000a0run", &[json!({"ts":now - 60_000,"role":"assistant","text":"working"})]);
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, sid, "/tmp");
    let (r1, _) = start(&mut c, "sleep 300", json!({}));
    std::thread::sleep(MS(20));
    let (r2, _) = start(&mut c, "sleep 301", json!({}));

    let ids = |args: &[&str]| -> Vec<String> {
        let v: Value = serde_json::from_str(&cli_ok(&home, args).stdout).unwrap();
        v.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(ids(&["ls", "--json"]), [r2.as_str(), r1.as_str(), "ch_0000a0run"], "running only, newest first");
    assert_eq!(
        ids(&["ls", "--json", "--all"]),
        [r2.as_str(), r1.as_str(), "ch_0000a0run", "ch_0000a0ld", "sh_0000f001"],
        "running group first; the old agent's last message is newer than the old shell's start"
    );
    let v: Value = serde_json::from_str(&cli_ok(&home, &["ls", "--json", "-a"]).stdout).unwrap();
    let old = v.as_array().unwrap().iter().find(|r| r["id"] == "ch_0000a0ld").unwrap();
    assert_eq!(old["active_at"], now - 10_000, "{old}");
    let header = cli_ok(&home, &["ls"]).stdout.lines().next().unwrap().to_string();
    assert!(header.split_whitespace().any(|h| h == "TIME"), "{header}");
}

/// `status`, `doctor` and a plain `ls` never pay for an agent's transcript
/// read unless that agent is actually about to be shown with its TIME: a
/// fifo with no writer blocks a real `File::open` forever, so a bounded CLI
/// call proves nothing tried to open it.
#[test]
fn c1c_status_and_doctor_never_read_agent_transcripts() {
    let home = Home::new("c1c");
    let sid = "sess-c1c";
    agent_fixture(
        &home,
        sid,
        json!({"child_id":"ch_0000c1c1","session_id":sid,"name":"stuck","agent":"w",
        "status":"completed","started_at":now_ms() - 5000,"ended_at":now_ms() - 1000}),
    );
    let dir = home.path.join("sessions").join(sid).join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("ch_0000c1c1.jsonl");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
    let _d = home.start_daemon();
    let mut conn = home.connect();
    conn.request_ok(json!({"type":"hello","client_kind":"extension","session_id":sid,
        "pi_pid":std::process::id(),"cwd":"/tmp","extension_version":"0.9.0-test","protocol":5}));
    // Bounded well under the fifo's indefinite block: none of these may open it.
    let out = home.cli(&["status", "--json"], S(3));
    assert!(out.status.success(), "status: {}{}", out.stdout, out.stderr);
    let out = home.cli(&["doctor"], S(3));
    assert!(out.status.success(), "doctor: {}{}", out.stdout, out.stderr);
    // The agent is finished work of a connected session: hidden by the
    // running-only default before anything reads its transcript.
    let out = home.cli(&["ls"], S(3));
    assert!(out.status.success(), "ls: {}{}", out.stdout, out.stderr);
    assert!(!out.stdout.contains("ch_0000c1c1"), "{}", out.stdout);
}

/// A bad `--since` must be rejected before filter_rows reads any surviving
/// row's transcript: parsing it late, after the read, would make an invalid
/// filter pay for work whose result it then discards.
#[test]
fn c1d_ls_since_is_validated_before_any_transcript_read() {
    let home = Home::new("c1d");
    let sid = "sess-c1d";
    agent_fixture(
        &home,
        sid,
        json!({"child_id":"ch_0000c1d1","session_id":sid,"name":"busy","agent":"w",
        "status":"running","started_at":now_ms() - 5000}),
    );
    let dir = home.path.join("sessions").join(sid).join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("ch_0000c1d1.jsonl");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
    let _d = home.start_daemon();
    let mut conn = home.connect();
    hello_v2(&mut conn, sid, "/tmp");
    // The running agent survives ls's cheap filters with no --all needed,
    // so it would be a transcript-read candidate if --since parsed late.
    let out = home.cli(&["ls", "--since", "10x"], S(3));
    assert!(!out.status.success(), "bad --since must be rejected: {}{}", out.stdout, out.stderr);
}

#[test]
fn c2_show_task_agent_and_run() {
    let home = Home::new("c2");
    let sid = "sess-c2";
    agent_fixture(&home, sid, json!({"v":1,"kind":"agent","child_id":"ch_0000c201","run_id":"run_0000c2ff",
        "session_id":sid,"name":"broken","agent":"worker","model":"m9","status":"failed",
        "started_at":now_ms() - 9000,"ended_at":now_ms() - 1000,"error":"529 overloaded_error",
        "end_reason":"model-error","prompt_head":"summarize the repo","tool_calls":4,
        "result_tail":(1..=30).map(|i| format!("result line {i}")).collect::<Vec<_>>().join("\n")}));
    agent_fixture(&home, sid, json!({"child_id":"ch_0000c202","run_id":"run_0000c2ff","session_id":sid,
        "name":"ok","agent":"worker","status":"completed","started_at":now_ms() - 8000}));
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, sid, "/tmp");
    let (t, pid) = start(&mut c, "for i in $(seq 1 25); do echo line $i; done; sleep 300",
        json!({"origin":{"via":"child-bash","child_id":"ch_0000c201","run_id":"run_0000c2ff"}}));
    c.request_ok(json!({"type":"mark_background","task_id":t}));
    assert!(poll_true(S(3), || c.task(&t).unwrap()["output_size"].as_u64().unwrap_or(0) > 60));
    append_event(&home, sid, json!({"ts":now_ms(),"src":"extension","type":"wake.emit","ids":[t],"kind":"task"}));
    append_event(&home, sid, json!({"ts":now_ms() + 5,"src":"extension","type":"wake.deliver","ids":[t],"kind":"task","mode":"steer"}));

    let out = cli_ok(&home, &["show", &t]);
    let s = &out.stdout;
    for want in [
        format!("id:           {t}"),
        "kind:         shell".into(),
        "work:         other".into(),
        "status:       running".into(),
        format!("session:      {sid} (connected, pi pid {})", std::process::id()),
        "cwd:          /tmp".into(),
        format!("pid:          {pid}"),
        "spawned by:   child-bash (ch_0000c201, run run_0000c2ff)".into(),
        "--- last 10 line(s) of output ---".into(),
        "line 25".into(),
    ] {
        assert!(s.contains(&want), "missing {want:?} in:\n{s}");
    }
    assert!(s.contains("backgrounded: after "), "{s}");
    assert!(s.contains("wake:") && s.contains("delivered") && s.contains("(steer)"), "{s}");
    assert!(!s.contains("line 15\n"), "only the last 10 lines: {s}");
    // fuzzy id + json
    let out = cli_ok(&home, &["show", &t[3..], "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["task"]["task_id"], t);
    assert_eq!(v["work_kind"], "other");
    assert_eq!(v["output_tail"].as_array().unwrap().len(), 10);
    assert_eq!(v["events"].as_array().unwrap().len(), 2 + 2, "task.start/background + wake.*: {v}");

    let out = cli_ok(&home, &["show", "ch_0000c201"]);
    let s = &out.stdout;
    for want in [
        "kind:         agent",
        "name:         broken (worker) m9",
        "run:          run_0000c2ff",
        "status:       failed (model-error)",
        "error:        529 overloaded_error",
        "tool calls:   4",
        "--- prompt ---",
        "summarize the repo",
        "--- result (last 20 line(s)) ---",
        "result line 30",
    ] {
        assert!(s.contains(want), "missing {want:?} in:\n{s}");
    }
    assert!(s.contains(&format!("shells:       {t} (running)")), "{s}");
    assert!(!s.contains("result line 10\n"), "result tail is 20 lines: {s}");
    assert!(s.contains("transcript:") && s.contains("ch_0000c201.jsonl"), "{s}");

    let out = cli_ok(&home, &["show", "run_0000c2ff"]);
    assert!(out.stdout.contains("children:     2 (2 finished)") && out.stdout.contains("ch_0000c202"), "{}", out.stdout);

    // not found: one line, closest match only
    let out = home.cli(&["show", "ch_0000c209"], S(5));
    assert!(!out.status.success());
    assert_eq!(out.stderr.trim(), "pi-famulus: unknown id 'ch_0000c209' (did you mean 'ch_0000c201'?)");
    assert!(!out.stderr.contains(&t), "no id dump: {}", out.stderr);
}

#[test]
fn c3_agent_transcript_and_ch_ids_in_log_tail_output_wait_stop() {
    let home = Home::new("c3");
    let sid = "sess-c3";
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, sid, "/tmp");
    agent_fixture(&home, sid, json!({"child_id":"ch_0000c301","run_id":"run_0000c3ff","session_id":sid,
        "name":"alpha","agent":"worker","status":"running","started_at":now_ms(),"prompt_head":"count to three",
        "result_tail":"one\ntwo\nthree"}));
    let tpath = transcript_fixture(&home, sid, "ch_0000c301", &[
        json!({"role":"system","text":"SYSTEM PROMPT","ts":now_ms()}),
        json!({"role":"user","text":"You are a worker agent. Rules...\n\ncount to three","ts":now_ms()}),
        json!({"role":"assistant","text":"one","ts":now_ms()}),
        json!({"role":"tool","tool":"bash","args":"echo two","isError":false,"text":"two","ts":now_ms()}),
    ]);

    let out = cli_ok(&home, &["agent", "ch_0000c301"]);
    let s = &out.stdout;
    assert!(!s.contains("SYSTEM PROMPT") && !s.contains("You are a worker"), "preamble hidden: {s}");
    assert!(s.contains("user: count to three") && s.contains("preamble hidden; --full"), "{s}");
    assert!(s.contains("assistant: one") && s.contains("tool bash(echo two) → two"), "{s}");
    let out = cli_ok(&home, &["agent", "c301", "--full"]);
    assert!(out.stdout.contains("system: SYSTEM PROMPT") && out.stdout.contains("You are a worker"), "{}", out.stdout);

    // log/tail on a ch_ id render the transcript; -f follows
    let out = cli_ok(&home, &["log", "ch_0000c301", "-n", "1"]);
    assert_eq!(out.stdout.lines().count(), 1);
    assert!(out.stdout.contains("tool bash(echo two)"), "{}", out.stdout);
    let mut tail = std::process::Command::new(BIN)
        .args(["--home", home.path.to_str().unwrap(), "tail", "ch_0000c301", "-n", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(MS(400));
    let mut f = std::fs::OpenOptions::new().append(true).open(&tpath).unwrap();
    writeln!(f, "{}", json!({"role":"assistant","text":"three","ts":now_ms()})).unwrap();
    let mut rd = std::io::BufReader::new(tail.stdout.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut rd, &mut line).unwrap();
    let _ = tail.kill();
    let _ = tail.wait();
    assert!(line.contains("assistant: three"), "{line}");

    // output = the agent's result; wait polls the record; stop is refused
    let out = cli_ok(&home, &["output", "ch_0000c301"]);
    assert_eq!(out.stdout, "one\ntwo\nthree");
    let out = cli_ok(&home, &["wait", "ch_0000c301", "--budget-ms", "300"]);
    assert!(out.stdout.contains("not done"), "{}", out.stdout);
    let rec_path = home.path.join("sessions").join(sid).join("agents/ch_0000c301.json");
    let mut rec: Value = serde_json::from_slice(&std::fs::read(&rec_path).unwrap()).unwrap();
    rec["status"] = json!("completed");
    std::fs::write(&rec_path, serde_json::to_vec(&rec).unwrap()).unwrap();
    let out = cli_ok(&home, &["wait", "ch_0000c301", "--budget-ms", "3000"]);
    assert_eq!(out.stdout.trim(), "done status=completed");
    let out = home.cli(&["stop", "ch_0000c301"], S(5));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr.trim(), "pi-famulus: agents run inside pi; stop from /tasks or ask the agent");
}

#[test]
fn c4_stop_messages_and_reason() {
    let home = Home::new("c4");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-c4", "/tmp");
    let (run, _) = start(&mut c, "sleep 300", json!({}));
    let (done, _) = start(&mut c, "true", json!({}));
    c.wait_terminal(&done, S(3)).unwrap();
    let out = cli_ok(&home, &["stop", &run]);
    assert_eq!(out.stdout.trim(), format!("stopped {run}"));
    assert_eq!(c.wait_terminal(&run, S(5)).unwrap()["end_reason"], "stopped:cli");
    let out = cli_ok(&home, &["stop", &done]);
    assert_eq!(out.stdout.trim(), format!("{done} already finished (exited)"));
    let out = cli_ok(&home, &["stop", &run]);
    assert_eq!(out.stdout.trim(), format!("{run} already finished (stopped:cli)"));
}

#[test]
fn c5_sessions_connected_and_gone() {
    let home = Home::new("c5");
    // A session from an earlier pi, known only from disk.
    let old = now_ms() - 3_600_000;
    append_event(&home, "sess-gone-1", json!({"ts":old,"src":"manager","type":"session.connect","pi_pid":4242,"cwd":"/tmp/gone"}));
    append_event(&home, "sess-gone-1", json!({"ts":old + 60_000,"src":"manager","type":"session.disconnect"}));
    record_fixture(&home, "sess-gone-1", "sh_0000c501", old, json!({}));
    agent_fixture(&home, "sess-gone-1", json!({"child_id":"ch_0000c501","session_id":"sess-gone-1","name":"x",
        "agent":"worker","status":"running","started_at":old}));
    // Without a daemon: nothing connected.
    let out = cli_ok(&home, &["sessions"]);
    assert!(out.stdout.contains("not running"), "{}", out.stdout);
    assert!(!home.sock().exists(), "sessions must not start the daemon");

    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-live-1", "/tmp/live");
    let (_t, _) = start(&mut c, "sleep 300", json!({}));
    let out = cli_ok(&home, &["sessions"]);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["SESSION", "PI_PID", "STATE", "CWD", "SINCE", "LAST_SEEN", "RUNNING", "TASKS", "AGENTS"]
    );
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    let live: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(live[0], "sess-liv", "shortest unique prefix, at least 8 chars");
    assert_eq!((live[1], live[2], live[3]), (std::process::id().to_string().as_str(), "connected", "/tmp/live"));
    assert_eq!(&live[live.len() - 3..], ["1", "1", "0"]);

    // A gone session is not listed (its stale "running" agent does not count
    // as running work), but its records stay inspectable until retention ends.
    assert!(!out.stdout.contains("sess-gon"), "{}", out.stdout);
    let out = cli_ok(&home, &["sessions", "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let ids: Vec<&str> = v.as_array().unwrap().iter().map(|s| s["session_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["sess-live-1"]);
    assert!(cli_ok(&home, &["show", "sh_0000c501"]).stdout.contains("sh_0000c501"));
    assert!(cli_ok(&home, &["events", "--session", "sess-gone"]).stdout.contains("session.connect"));
    assert!(home.cli(&["sessions", "-a"], S(5)).status.code() == Some(2), "-a is gone");
}

#[test]
fn c6_status_counts_uptime_and_not_running() {
    let home = Home::new("c6");
    let out = home.cli(&["status"], S(5));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr.trim(), "pi-famulus: pi-famulus is not running");
    assert!(!home.sock().exists(), "status must not start the daemon");

    agent_fixture(&home, "sess-c6", json!({"child_id":"ch_0000c601","session_id":"sess-c6","name":"a","agent":"w","status":"completed","tokens_input":35,"tokens_output":5}));
    agent_fixture(&home, "sess-c6", json!({"child_id":"ch_0000c602","session_id":"sess-c6","name":"b","agent":"w","status":"failed","tokens_input":12,"tokens_output":4}));
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-c6", "/tmp");
    let (t, _) = start(&mut c, "true", json!({}));
    c.wait_terminal(&t, S(3)).unwrap();
    start(&mut c, "sleep 300", json!({}));
    std::thread::sleep(MS(1100));
    let out = cli_ok(&home, &["status"]);
    let s = &out.stdout;
    // The version names the commit the binary was built from ("0.1.0" alone
    // cannot tell builds apart), and status names the daemon's binary: the
    // file an upgrade execs, which need not be the CLI's own. Compare
    // against the version embedded at build time (same as main.rs's
    // VERSION), not a fresh `git rev-parse`: shelling out again would fail
    // outside a git checkout, where build.rs already fell back to
    // "unknown", and would drift from the built-from commit on any tree
    // whose HEAD moved since the daemon binary was built.
    let version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("PI_FAMULUS_GIT_SHA"));
    assert!(s.contains(&format!("version:  {version} (protocol 5)")), "{s}");
    let bin = std::fs::canonicalize(BIN).unwrap();
    // Fresh /tmp targets can be reported as /tmp or /private/tmp on macOS.
    let reported_bin = s.lines().find_map(|line| line.strip_prefix("binary:   ")).expect("binary path");
    assert_eq!(std::fs::canonicalize(reported_bin).unwrap(), bin, "{s}");
    let uptime = s.lines().find(|l| l.starts_with("uptime:")).unwrap();
    assert!(uptime.ends_with('s') && !uptime.contains('.'), "human uptime: {uptime}");
    assert!(s.contains("tasks:    1 running, 3 finished (shells 1/1, agents 0/2)"), "{s}");
    assert!(s.contains("agent tokens: 47 input / 9 output"), "{s}");
    let out = cli_ok(&home, &["status", "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["protocol"], 5);
    assert_eq!(v["agent_counts"], json!({"running":0,"terminal":2}));
    assert_eq!(v["agent_tokens"], json!({"input":47,"output":9}));
}

#[test]
fn c6_cli_hello_identifies_the_new_product() {
    let home = Home::new("c6-hello");
    let _d = home.start_daemon();
    cli_ok(&home, &["start", "--session", "sess-cli-brand", "--", "sleep 300"]);
    let out = cli_ok(&home, &["status", "--json"]);
    let status: Value = serde_json::from_str(&out.stdout).unwrap();
    let session = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == "sess-cli-brand")
        .expect("CLI start registered its extension-style session");
    let version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("PI_FAMULUS_GIT_SHA"));
    assert_eq!(session["extension_version"], format!("pi-famulus-cli/{version}"));
    assert_eq!(session["protocol"], 5);
}

// ===========================================================================
// CLI: output bytes, SIGPIPE, log timestamps
// ===========================================================================

#[test]
fn c7_output_max_bytes_sigpipe_and_log_timestamps() {
    let home = Home::new("c7");
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-c7", "/tmp");
    let (big, _) = start(&mut c, "head -c 3000000 /dev/zero | tr '\\0' 'a'", json!({}));
    let (cjk, _) = start(&mut c, "yes 中文 | head -n 3000", json!({}));
    c.wait_terminal(&big, S(10)).unwrap();
    c.wait_terminal(&cjk, S(10)).unwrap();

    let out = cli_ok(&home, &["output", &big, "--max-bytes", "120"]);
    assert_eq!(out.stdout.len(), 120, "--max-bytes is a total cap");
    let out = cli_ok(&home, &["output", &big, "--max-bytes", "100000"]);
    assert_eq!(out.stdout.len(), 100_000, "caps across several reads too");
    let out = cli_ok(&home, &["output", &cjk, "--max-bytes", "121"]);
    // 7-byte lines: 17 whole lines (119 bytes) + "中" (3) would pass 121.
    assert!(out.stdout.len() <= 121 && out.stdout.len() >= 118, "{}", out.stdout.len());
    assert!(!out.stdout.contains('\u{FFFD}'));
    let out = cli_ok(&home, &["output", &big]);
    assert_eq!(out.stdout.len(), 3_000_000, "no cap without --max-bytes");

    // SIGPIPE: a closed reader ends the CLI quietly with status 0.
    for args in [format!("output {big}"), "ls".into(), "events".into(), format!("log {big}")] {
        let script = format!(
            "set -o pipefail; '{BIN}' --home '{}' {args} | head -c 1 >/dev/null",
            home.path.display()
        );
        let o = std::process::Command::new("bash").args(["-c", &script]).output().unwrap();
        assert!(o.status.success(), "{args}: {:?} stderr={}", o.status, String::from_utf8_lossy(&o.stderr));
        assert!(o.stderr.is_empty(), "{args}: {}", String::from_utf8_lossy(&o.stderr));
    }

    let out = cli_ok(&home, &["log", "-n", "5"]);
    for l in out.stdout.lines() {
        assert!(l.starts_with("[20") && l.as_bytes()[24] == b']', "human timestamp: {l}");
    }
}

// ===========================================================================
// doctor
// ===========================================================================

#[test]
fn c8_doctor_checks_and_exit_status() {
    // Nonexistent home: failure, and doctor does not create it.
    let missing = std::env::temp_dir().join(format!("pi-famulus-test-{}-c8-missing", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    let out = run_cli(&missing, &["doctor"], S(5));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.contains("FAIL  home: does not exist"), "{}", out.stdout);
    assert!(!missing.exists(), "doctor created the home");

    // Healthy empty home.
    let home = Home::new("c8");
    let out = run_cli(&home.path, &["doctor"], S(5));
    assert_eq!(out.status.code(), Some(0), "{}", out.stdout);
    assert!(out.stdout.contains("ok    sessions dir:"), "{}", out.stdout);

    // The product-specific manager path environment overrides config.json.
    std::fs::write(home.path.join("config.json"), br#"{"managerPath":"/nonexistent/from-config"}"#).unwrap();
    let out = run_cli_env(
        &home.path,
        &["doctor"],
        S(5),
        &[("PI_FAMULUS_MANAGER_PATH", "/nonexistent/pi-famulus-env")],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.contains("/nonexistent/pi-famulus-env from PI_FAMULUS_MANAGER_PATH"), "{}", out.stdout);
    assert!(!out.stdout.contains("/nonexistent/from-config"), "{}", out.stdout);

    // Broken config.json, missing managerPath, stale agent record, orphan pid.
    std::fs::write(home.path.join("config.json"), b"{ not json").unwrap();
    let out = run_cli(&home.path, &["doctor"], S(5));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.contains("FAIL  config.json: does not parse"), "{}", out.stdout);
    std::fs::write(home.path.join("config.json"), br#"{"managerPath":"/nonexistent/pi-famulus"}"#).unwrap();
    agent_fixture(&home, "sess-c8", json!({"child_id":"ch_0000c801","session_id":"sess-c8","name":"x","agent":"w","status":"running"}));
    let mut orphan = std::process::Command::new("sleep").arg("300").spawn().unwrap();
    record_fixture(&home, "sess-c8", "sh_0000c801", now_ms(), json!({"status":"running","pid":orphan.id(),"exit_code":null,"ended_at":null,"end_reason":null}));
    let out = run_cli(&home.path, &["doctor"], S(5));
    let _ = orphan.kill();
    let _ = orphan.wait();
    assert_eq!(out.status.code(), Some(1));
    let s = &out.stdout;
    assert!(s.contains("FAIL  manager path: /nonexistent/pi-famulus from config.json managerPath does not exist"), "{s}");
    assert!(s.contains("FAIL  agent record: ch_0000c801 says running but session sess-c8 is gone"), "{s}");
    assert!(s.contains(&format!("FAIL  orphan pid: sh_0000c801 (pid {})", orphan.id())), "{s}");
    assert!(s.lines().any(|l| l == "3 problem(s) found"), "{s}");

    // Socket path too long for a unix socket.
    let long = home.path.join("x".repeat(60)).join("y".repeat(60));
    std::fs::create_dir_all(&long).unwrap();
    let out = run_cli(&long, &["doctor"], S(5));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.contains("FAIL  socket path:"), "{}", out.stdout);
}

/// Gone-session retention: a disconnected session's files are deleted once
/// `goneSessionRetention` passes, unless it still runs something; connected
/// sessions are never touched. `0s` makes the next sweep (1 s cadence) act.
#[test]
fn g1_gone_sessions_are_swept_after_retention() {
    let home = Home::new("g1");
    std::fs::create_dir_all(&home.path).unwrap();
    std::fs::write(home.path.join("config.json"), r#"{"goneSessionRetention":"0s"}"#).unwrap();
    let _d = home.start_daemon();

    let mut live = home.connect();
    hello_v2(&mut live, "sess-g-live", "/tmp");
    let (live_task, _) = start(&mut live, "true", json!({}));
    live.wait_terminal(&live_task, S(3)).unwrap();

    let mut done = home.connect();
    hello_v2(&mut done, "sess-g-done", "/tmp");
    let (done_task, _) = start(&mut done, "true", json!({}));
    done.wait_terminal(&done_task, S(3)).unwrap();

    let mut busy = home.connect();
    hello_v2(&mut busy, "sess-g-busy", "/tmp");
    let (busy_task, busy_pid) = start(&mut busy, "sleep 300", json!({}));

    drop(done);
    drop(busy);
    let sessions = home.path.join("sessions");
    let swept = poll_true(S(10), || {
        home.advance("gc", 1_000);
        !sessions.join("sess-g-done").exists()
    });
    assert!(swept, "gone, finished session not swept");
    assert!(sessions.join("sess-g-live").exists(), "connected session kept");
    assert!(sessions.join("sess-g-busy").exists(), "gone session with a running task kept");
    assert!(pid_alive(busy_pid), "sweeping never touches live processes");

    // Forgotten everywhere: listings and by-id lookups.
    assert!(!home.cli(&["show", &done_task], S(5)).status.success(), "swept task is gone from show");
    let ls = cli_ok(&home, &["ls", "--all"]).stdout;
    assert!(ls.contains(&busy_task), "running work of a gone session stays listed: {ls}");
    assert!(ls.contains(&live_task), "connected session's finished work is listed: {ls}");
    assert!(!ls.contains(&done_task), "{ls}");
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
    assert!(log.contains("gc: removed 1 gone session(s): sess-g-done"), "{log}");
    kill_group(busy_pid, 9);
}

/// Finished-task retention: in every session, connected or not, a finished
/// task's files (record, output, stderr) are deleted `finishedTaskRetention`
/// after it ended. A pi session left open for days otherwise keeps every
/// command's output (9,671 records, ~100 MB after 9 days on 2026-09-26),
/// and the daemon loads all of it at startup. Running tasks, leftover
/// process groups, agents and the session's events stay.
#[test]
fn g15_finished_tasks_expire_in_a_connected_session() {
    let home = Home::new("g4");
    std::fs::create_dir_all(&home.path).unwrap();
    std::fs::write(home.path.join("config.json"), r#"{"finishedTaskRetention":"0s"}"#).unwrap();
    agent_fixture(&home, "sess-g4", json!({"child_id":"ch_0000g401","session_id":"sess-g4","name":"a","agent":"w","status":"completed"}));
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-g4", "/tmp");
    let (running, running_pid) = start(&mut c, "sleep 300", json!({}));
    let (leftover, leftover_pid) = start(&mut c, "sleep 300 & exit 0", json!({}));
    let (done, _) = start(&mut c, "echo out; echo err >&2", json!({}));
    c.wait_terminal(&done, S(3)).unwrap();
    c.wait_terminal(&leftover, S(3)).unwrap();

    let tasks = home.path.join("sessions/sess-g4/tasks");
    let swept = poll_true(S(10), || {
        home.advance("gc", 1_000);
        !tasks.join(format!("{done}.json")).exists()
    });
    assert!(swept, "finished task not swept");
    for ext in ["output", "stderr"] {
        assert!(!tasks.join(format!("{done}.{ext}")).exists(), "{done}.{ext} left behind");
    }
    for id in [&running, &leftover] {
        assert!(tasks.join(format!("{id}.json")).exists(), "{id} kept");
    }
    assert!(home.path.join("sessions/sess-g4/events.jsonl").exists(), "events kept");
    assert!(home.path.join("sessions/sess-g4/agents/ch_0000g401.json").exists(), "agents kept");
    assert!(pid_alive(running_pid), "sweeping never touches live processes");

    // The json file goes before the in-memory record: `run_task_gc` deletes
    // with the state lock released, then forgets. `show` talks to the daemon,
    // so a check in that window still finds the task. Wait the forget out.
    let forgotten = poll_true(S(5), || !home.cli(&["show", &done], S(5)).status.success());
    assert!(forgotten, "swept task is gone from show");
    let ls = cli_ok(&home, &["ls", "-a"]).stdout;
    assert!(ls.contains(&running) && ls.contains(&leftover) && !ls.contains(&done), "{ls}");
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
    assert!(log.contains(&format!("gc: removed 1 finished task(s): {done}")), "{log}");
    kill_group(running_pid, 9);
    kill_group(leftover_pid, 9);
}

/// A sweep that cannot delete a finished task's files must not forget the
/// task either: the record is the only thing that says the files still need
/// deleting, so losing it while the files stay behind orphans them until a
/// restart's startup scan (or forever, if nothing ever re-scans). Block the
/// delete with an unwritable tasks dir, sweep, and check the record is still
/// there; unblock it and check the next sweep finishes the job.
#[test]
fn g15b_finished_task_survives_a_failed_delete_for_retry() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory modes; nothing to assert
    }
    let home = Home::new("g4b");
    std::fs::create_dir_all(&home.path).unwrap();
    std::fs::write(home.path.join("config.json"), r#"{"finishedTaskRetention":"0s"}"#).unwrap();
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, "sess-g4b", "/tmp");
    let (done, _) = start(&mut c, "true", json!({}));
    c.wait_terminal(&done, S(3)).unwrap();

    let tasks_dir = home.path.join("sessions/sess-g4b/tasks");
    // Restores the dir to writable even if an assertion below panics, so a
    // failed run doesn't leave an unwritable dir behind for the Home's own
    // cleanup (or a later test) to choke on.
    struct RestorePerms(PathBuf);
    impl Drop for RestorePerms {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::set_permissions(&tasks_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = RestorePerms(tasks_dir.clone());
    home.advance("gc", 1_000);
    std::thread::sleep(MS(300));
    // Assert survival *before* the dir goes writable again: once it does,
    // the background sweep (polling at least once a second) can delete the
    // record at any moment, racing these checks against the assertion below
    // that it is still there.
    assert!(tasks_dir.join(format!("{done}.json")).exists(), "record lost before its files could be deleted");
    assert!(home.cli(&["show", &done], S(5)).status.success(), "swept from show while its files still exist");

    std::fs::set_permissions(&tasks_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let swept = poll_true(S(10), || {
        home.advance("gc", 1_000);
        !tasks_dir.join(format!("{done}.json")).exists()
    });
    assert!(swept, "retry sweep never finished deleting the task");
    for ext in ["output", "stderr"] {
        assert!(!tasks_dir.join(format!("{done}.{ext}")).exists(), "{done}.{ext} left behind");
    }
}

#[test]
fn g2_doctor_flags_a_bad_retention() {
    let home = Home::new("g2");
    std::fs::create_dir_all(&home.path).unwrap();
    std::fs::write(home.path.join("config.json"), r#"{"goneSessionRetention":"soon"}"#).unwrap();
    let out = home.cli(&["doctor"], S(10));
    assert_eq!(out.status.code(), Some(1), "{}", out.stdout);
    assert!(out.stdout.contains("session retention") && out.stdout.contains("soon"), "{}", out.stdout);
    std::fs::write(home.path.join("config.json"), r#"{"goneSessionRetention":"2h"}"#).unwrap();
    let out = home.cli(&["doctor"], S(10));
    assert!(out.stdout.contains("gone sessions kept 2h"), "{}", out.stdout);
    std::fs::write(home.path.join("config.json"), r#"{"finishedTaskRetention":"later"}"#).unwrap();
    let out = home.cli(&["doctor"], S(10));
    assert_eq!(out.status.code(), Some(1), "{}", out.stdout);
    assert!(out.stdout.contains("task retention") && out.stdout.contains("later"), "{}", out.stdout);
}

/// Between a daemon crash and the next daemon start, the disk still says
/// "running". Every task dies with its manager (lifeline), so a CLI that finds
/// no daemon must not report it as running (found in acceptance of PR #2).
#[test]
fn g3_no_daemon_means_nothing_is_running() {
    let home = Home::new("g3");
    record_fixture(&home, "sess-g3", "sh_0000d301", now_ms() - 5_000,
        json!({"status":"running","exit_code":null,"ended_at":null,"end_reason":null}));
    let out = cli_ok(&home, &["show", "sh_0000d301"]);
    assert!(!out.stdout.contains("running"), "{}", out.stdout);
    assert!(out.stdout.contains("orphaned") && out.stdout.contains("manager-crash"), "{}", out.stdout);
    assert!(!home.sock().exists(), "show must not start the daemon");
}

/// c9: the inspection commands keep working when the daemon holds more
/// task records than fit in one 4 MiB frame (seen live: 9,585 records from
/// nine days of history made `sessions` fail with "response exceeds the
/// 4 MiB frame limit"). 50 records with 100 KB commands are ~5 MB.
#[test]
fn c9_inspection_works_past_one_frame_of_records() {
    let home = Home::new("c9");
    let sid = "sess-c9";
    let _d = home.start_daemon();
    let mut c = home.connect();
    hello_v2(&mut c, sid, "/tmp");
    let pad = "x".repeat(100_000);
    let ids: Vec<String> = (0..50).map(|i| start(&mut c, &format!(": {i} {pad}"), json!({})).0).collect();
    for id in &ids {
        // Per task: a `list` here would hit the very limit under test.
        let w = c.request_ok(json!({"type":"wait","task_id":id,"budget_ms":5000}));
        assert_eq!(w["done"], json!(true), "{id}: {w}");
    }
    let out = cli_ok(&home, &["sessions"]);
    assert!(out.stdout.contains("50"), "sessions counts every task: {}", out.stdout);
    let out = cli_ok(&home, &["ls", "--all", "--json"]);
    let rows: Value = serde_json::from_str(&out.stdout).expect("ls --json");
    assert_eq!(rows.as_array().map(|a| a.len()), Some(50), "ls lists every task");
    let last = ids.last().unwrap();
    let out = cli_ok(&home, &["show", last]);
    assert!(out.stdout.contains(last.as_str()), "{}", out.stdout);
}
