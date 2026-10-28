//! Hosted subagents: the daemon runs a `pi --mode rpc` stand-in and relays JSONL.
#![cfg(unix)]

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::time::Duration;

const FAKE_PI: &str = r#"
import json, sys, os
def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    rid = msg.get("id")
    kind = msg.get("type")
    def reply(data):
        emit({"type": "response", "id": rid, "command": kind, "success": True, "data": data})
    if kind == "get_state":
        reply({
            "sessionId": os.environ.get("PI_FAMULUS_CHILD_ID", "x"),
            "sessionFile": os.environ.get("FAKE_SESSION_FILE", "/tmp/fake-session.jsonl"),
            "isStreaming": False,
            "messageCount": 0,
        })
    elif kind == "prompt":
        reply({"disposition": "started"})
        emit({"type": "agent_start"})
        emit({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "pong"}]}})
        emit({"type": "agent_end"})
        emit({"type": "agent_settled"})
    elif kind == "abort":
        reply({})
        emit({"type": "agent_settled", "aborted": True})
    else:
        reply({})
"#;

fn spawn_fake(home: &Home, conn: &mut Conn, child_id: &str) -> Value {
    let script = home.path.join("fake-pi.py");
    if !script.exists() {
        fs::write(&script, FAKE_PI).unwrap();
    }
    let session = home.path.join(format!("{child_id}.jsonl"));
    let mut env = task_env();
    env["FAKE_SESSION_FILE"] = json!(session);
    conn.request_ok(json!({
        "type": "agent_spawn",
        "child_id": child_id,
        "run_id": "run_hosted",
        "name": "worker",
        "argv": ["python3", script],
        "cwd": "/tmp",
        "env": env,
    }))
}

#[test]
fn hosted_prompt_is_relayed_and_lookup_sees_the_session() {
    let home = Home::new("hosted-relay");
    let _daemon = home.start_daemon();
    let mut conn = home.connect();
    conn.hello_ext("sess-host");
    let spawned = spawn_fake(&home, &mut conn, "ch_aabbccdd");
    assert_eq!(spawned["residency"], "warm", "{spawned}");
    // get_state said the child is not streaming, so it is idle rather than stuck in starting.
    assert_eq!(spawned["activity"], "idle", "{spawned}");
    let prompted = conn.request_ok(json!({
        "type": "agent_input",
        "child_id": "ch_aabbccdd",
        "op": "prompt",
        "text": "hi",
        "source": "parent",
    }));
    assert_eq!(prompted["disposition"], "started", "{prompted}");
    let found = conn.request_ok(json!({
        "type": "hosted_lookup",
        "session_id": "ch_aabbccdd",
    }));
    assert_eq!(found["hosted"]["child_id"], "ch_aabbccdd", "{found}");
    assert_eq!(found["hosted"]["residency"], "warm");
}

#[test]
fn a_warm_agent_keeps_the_daemon_alive_after_its_parent_disconnects() {
    let home = Home::new("hosted-lifetime");
    let daemon = home.start_daemon();
    let mut conn = home.connect();
    conn.hello_ext("sess-life");
    let spawned = spawn_fake(&home, &mut conn, "ch_lifeline");
    assert_eq!(spawned["ok"], json!(true));
    drop(conn);
    // Past the 5s idle grace on the daemon's own clock: a real sleep never
    // moves the manual clock, so under test-clock it proved nothing.
    settle();
    home.advance_now(6_000);
    settle();
    let mut again = home.connect();
    let hello = again.hello_cli();
    assert_eq!(hello["ok"], json!(true), "daemon exited while a subagent was warm: {hello}");
    drop(again);
    drop(daemon);
}

#[test]
fn warm_child_visibility_tracks_daemon_death_and_restart() {
    let home = Home::new("hosted-visibility");
    let mut daemon = home.start_daemon();
    let mut conn = home.connect();
    conn.hello_ext("sess-gone");
    spawn_fake(&home, &mut conn, "ch_visibility");
    let records = home.path.join("sessions/sess-gone/agents");
    fs::create_dir_all(&records).unwrap();
    fs::write(records.join("ch_visibility.json"), json!({
        "v":1,"kind":"agent","child_id":"ch_visibility","run_id":"run_hosted",
        "session_id":"sess-gone","name":"worker","agent":"worker","status":"running",
        "started_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    }).to_string()).unwrap();
    drop(conn);
    let shown = || {
        let out = home.cli(&["show", "ch_visibility", "--json"], Duration::from_secs(10));
        assert!(out.status.success(), "{}", out.stderr);
        serde_json::from_str::<Value>(&out.stdout).unwrap()
    };
    assert_eq!(shown()["agent"]["status"], "running", "warm child outlives parent");
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert_eq!(shown()["agent"]["status"], "interrupted", "dead daemon cannot own warm children");
    let _restarted = home.start_daemon();
    let mut cli = home.connect();
    cli.hello_cli();
    let lookup = cli.request_ok(json!({"type":"hosted_lookup","session_id":"ch_visibility"}));
    assert_eq!(lookup["hosted"]["residency"], "parked");
    let disk: Value = serde_json::from_slice(&fs::read(home.path.join("agents/ch_visibility.json")).unwrap()).unwrap();
    assert_eq!(disk["residency"], "parked", "persist the restart reset for offline readers");
    assert_eq!(disk["activity"], "idle");
    assert_eq!(shown()["agent"]["status"], "interrupted", "restart must not resurrect the old process");
    let listed = home.cli(&["ls", "--all", "--json"], Duration::from_secs(10));
    assert!(listed.status.success(), "{}", listed.stderr);
    let rows: Vec<Value> = serde_json::from_str(&listed.stdout).unwrap();
    assert!(!rows.iter().any(|row| row["id"] == "ch_visibility" && row["running"] == true));
}

/// A hosted child's foreground shell must not keep running after the
/// child process is gone.
#[test]
fn a_killed_hosted_child_stops_its_shell() {
    let home = Home::new("hosted-orphan-shell");
    let _daemon = home.start_daemon();
    let mut conn = home.connect();
    conn.hello_ext("sess-orphan");
    let spawned = spawn_fake(&home, &mut conn, "ch_orphan01");
    let child_pid = spawned["pid"].as_u64().unwrap_or(0) as u32;
    assert!(child_pid > 0, "hosted child pid from spawn: {spawned}");
    let (task_id, _shell_pid) = conn.start_with(json!({
        "type": "start",
        "kind": "shell",
        "command": "sleep 300",
        "cwd": task_cwd(),
        "env": task_env(),
        "run_in_background": true,
        "origin": {"via": "child-bash", "child_id": "ch_orphan01", "run_id": "run_hosted"},
    }));
    assert_eq!(conn.task(&task_id).unwrap()["status"], "running");
    let _ = unsafe { libc::kill(child_pid as i32, libc::SIGKILL) };
    let stopped = poll_until(Duration::from_secs(5), || {
        let rec = conn.task(&task_id)?;
        (rec["status"] != "running").then_some(rec)
    });
    let rec = stopped.expect("the child's shell must stop after the hosted process dies");
    assert_ne!(rec["status"], "running", "{rec}");
}
