//! Portable black-box suite: the lifecycle-table cells (manager/TESTING.md)
//! that hold on every platform, run against the real binary on Linux, macOS
//! and Windows.
//!
//! The Unix suites (`protocol`, `lifecycle_adversarial`, `mutation_gaps`,
//! `observability`, `upgrade`, `timing_canary`) pin the same contract with
//! `sh` idioms, signals and process groups. This one states it with the
//! `taskkit` program (see `common/kit.rs`) and process probes that exist
//! everywhere, so CI on `windows-latest` covers the scenarios Windows users
//! hit: named-pipe IPC, Job Object trees and lifelines, the task shell,
//! the singleton daemon, idle shutdown and the CLI.
//!
//! Test names keep the cell ids of the table (`t6c_…` covers T6c). Cells
//! that only exist on Unix (SIGTERM handlers and the kill grace observed by
//! a TERM-ignoring task, process-group signals, inherited fds, exec
//! handover) stay in the Unix suites; TESTING.md lists the Windows
//! counterpart of each.
//!
//! `harness = false`: the binary is also the `taskkit` task program and the
//! helper client, so `main` dispatches first (see `kit::main`).

mod common;

use common::kit::{self, kit};
use common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const S: fn(u64) -> Duration = Duration::from_secs;
const MS: fn(u64) -> Duration = Duration::from_millis;

fn main() {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut tests: Vec<(&'static str, kit::TestFn)> = vec![
        ("c2_hello_validation", c2_hello_validation),
        ("f1_frame_limits_and_malformed_input", f1_frame_limits_and_malformed_input),
        ("c4_duplicate_hello_rebinds_session", c4_duplicate_hello_rebinds_session),
        ("s3_crashed_session_survives_and_reattaches", s3_crashed_session_survives_and_reattaches),
        ("s5_shutdown_session_stops_only_own_tasks", s5_shutdown_session_stops_only_own_tasks),
        ("t1_exit_codes_output_and_events", t1_exit_codes_output_and_events),
        ("t3_timeout_ms_hard_kill", t3_timeout_ms_hard_kill),
        ("t4_stop_kills_a_running_task", t4_stop_kills_a_running_task),
        ("t6_stop_kills_the_whole_tree", t6_stop_kills_the_whole_tree),
        ("t6c_leftover_outlives_its_command_until_shutdown", t6c_leftover_outlives_its_command_until_shutdown),
        ("t6d_stop_reaches_the_leftover_of_a_finished_task", t6d_stop_reaches_the_leftover_of_a_finished_task),
        ("t6e_leftover_that_exits_leaves_nothing_to_kill", t6e_leftover_that_exits_leaves_nothing_to_kill),
        ("t7_daemon_kill_takes_every_task_down", t7_daemon_kill_takes_every_task_down),
        ("t12_stop_terminal_task_is_noop", t12_stop_terminal_task_is_noop),
        ("t13_terminal_records_survive_restart", t13_terminal_records_survive_restart),
        ("t14_wait_budget_expires_while_running", t14_wait_budget_expires_while_running),
        ("t16_utf8_split_across_writes", t16_utf8_split_across_writes),
        ("o1_large_output_exact_bytes", o1_large_output_exact_bytes),
        ("o5_stderr_is_merged_and_mirrored", o5_stderr_is_merged_and_mirrored),
        ("e1_task_gets_its_cwd_and_env", e1_task_gets_its_cwd_and_env),
        ("q1_quoted_arguments_reach_the_program", q1_quoted_arguments_reach_the_program),
        ("q2_commands_run_in_a_posix_shell", q2_commands_run_in_a_posix_shell),
        ("x1_concurrent_starts_and_stops_finish_promptly", x1_concurrent_starts_and_stops_finish_promptly),
        ("d1_concurrent_clients_spawn_one_daemon", d1_concurrent_clients_spawn_one_daemon),
        ("d3_concurrent_daemons_leave_one_survivor", d3_concurrent_daemons_leave_one_survivor),
        ("d5_last_client_crash_idles_out_and_kills_tasks", d5_last_client_crash_idles_out_and_kills_tasks),
        ("d6_hello_inside_grace_cancels_shutdown", d6_hello_inside_grace_cancels_shutdown),
        ("d9_cli_shutdown_kills_tasks_and_cleans_files", d9_cli_shutdown_kills_tasks_and_cleans_files),
        ("d10_cli_recovers_from_a_killed_daemon", d10_cli_recovers_from_a_killed_daemon),
        ("d12_reused_pid_in_pidfile_does_not_block_startup", d12_reused_pid_in_pidfile_does_not_block_startup),
        ("d16_extension_cannot_shutdown", d16_extension_cannot_shutdown),
        ("d17_doctor_cleans_only_without_a_daemon", d17_doctor_cleans_only_without_a_daemon),
        ("h1_home_spellings_reach_one_daemon", h1_home_spellings_reach_one_daemon),
        ("cli1_start_wait_output_ls_show_stop", cli1_start_wait_output_ls_show_stop),
        ("cli2_auto_spawned_daemon_outlives_the_cli", cli2_auto_spawned_daemon_outlives_the_cli),
    ];
    #[cfg(windows)]
    tests.extend([
        ("q3_cmd_fallback_passes_the_command_verbatim", q3_cmd_fallback_passes_the_command_verbatim as kit::TestFn),
        ("u1_upgrade_is_refused_and_the_daemon_keeps_serving", u1_upgrade_is_refused_and_the_daemon_keeps_serving),
    ]);
    kit::main(&tests);
}

fn ext(home: &Home, session: &str) -> Conn {
    let mut c = home.connect();
    let h = c.hello_ext(session);
    assert_eq!(h["ok"], true, "hello: {h}");
    c
}

fn start_req(command: &str) -> Value {
    json!({"type":"start","kind":"shell","command":command,"cwd":task_cwd(),"env":task_env()})
}

fn output_of(c: &mut Conn, task_id: &str) -> String {
    let r = c.request_ok(json!({"type":"output","task_id":task_id,"cursor":0,"max_bytes":1 << 20}));
    r["chunk"].as_str().unwrap_or("").to_string()
}

/// Normalise line endings for comparisons of text a shell may have produced.
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Is manager.lock (the daemon's lifetime lock, §3.1) held right now?
fn lifetime_lock_held(home: &Home) -> bool {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.path.join("manager.lock"))
        .unwrap();
    let mut lock = fd_lock::RwLock::new(f);
    let held = lock.try_write().is_err();
    held
}

// ===========================================================================
// Connections and sessions (C2, C4, C7, C8, S2, S3, S5, S6)
// ===========================================================================

/// C2: an extension hello without session/pi_pid or with a path-like id is
/// refused and closed; a second hello on a live connection is rejected.
fn c2_hello_validation() {
    let home = Home::new("w-c2");
    let _d = home.start_daemon();
    for bad in [
        json!({"type":"hello","client_kind":"extension","pi_pid":1}),
        json!({"type":"hello","client_kind":"extension","session_id":"s"}),
        json!({"type":"hello","client_kind":"extension","session_id":"..\\x","pi_pid":1}),
        json!({"type":"hello","client_kind":"extension","session_id":"../x","pi_pid":1}),
    ] {
        let mut c = home.connect();
        let r = c.request(bad.clone());
        assert_eq!(r["error"]["code"], "E_BAD_REQUEST", "{bad} -> {r}");
        assert!(c.wait_closed(S(3)), "refused hello must close the connection");
    }
    let mut c = home.connect();
    c.hello_cli();
    assert_eq!(c.hello_cli()["ok"], false);
    assert_eq!(c.request(json!({"type":"list"}))["ok"], true);
    let st = c.request_ok(json!({"type":"status"}));
    assert_eq!(st["sessions"].as_array().unwrap().len(), 0, "{st}");
}

/// C7/C8: an oversized frame closes only that connection; exactly 4 MiB is
/// accepted; malformed JSON gets E_BAD_REQUEST and the connection survives.
fn f1_frame_limits_and_malformed_input() {
    let home = Home::new("w-f1");
    let mut daemon = home.start_daemon();
    let mut keep = ext(&home, "sess-keep");
    let (id, pid) = keep.start(&kit(&["sleep", "60000"]));

    let mut y = home.connect();
    y.hello_cli();
    y.send_raw(&u32::MAX.to_be_bytes()).unwrap();
    assert!(y.wait_closed(S(3)), "oversized frame must close the connection");

    let mut z = home.connect();
    z.hello_cli();
    let mut body = br#"{"v":1,"id":"big","type":"list","all":true}"#.to_vec();
    body.resize(MAX_FRAME, b' ');
    let mut frame = (MAX_FRAME as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    z.send_raw(&frame).unwrap();
    let big = z.wait_id("big", S(10)).expect("no response to an exactly-4MiB frame");
    assert_eq!(big["ok"], true, "{big}");

    let garbage = b"{nope";
    let mut f = (garbage.len() as u32).to_be_bytes().to_vec();
    f.extend_from_slice(garbage);
    z.send_raw(&f).unwrap();
    let bad = z.wait_id("", S(3)).expect("malformed frame must get a response");
    assert_eq!(bad["error"]["code"], "E_BAD_REQUEST", "{bad}");
    assert_eq!(z.request(json!({"type":"list"}))["ok"], true);

    assert!(daemon.try_wait().unwrap().is_none());
    assert!(pid_running(pid));
    assert_eq!(keep.status_of(&id).as_deref(), Some("running"));
}

/// C4: a duplicate hello moves the session to the new connection; the old
/// one gets `session_rebound` and is closed.
fn c4_duplicate_hello_rebinds_session() {
    let home = Home::new("w-c4");
    let _d = home.start_daemon();
    let mut a = ext(&home, "sess-dup");
    let (id, _) = a.start(&kit(&["sleep", "60000"]));
    let mut b = ext(&home, "sess-dup");
    assert!(a.wait_event(S(3), |e| e["event"] == "session_rebound").is_some());
    assert!(a.wait_closed(S(3)), "old connection must be closed");
    b.request_ok(json!({"type":"stop","task_id":id}));
    assert!(
        b.wait_event(S(10), |e| e["event"] == "task_exited" && e["task_id"] == json!(id)).is_some(),
        "events must follow the new connection"
    );
}

/// S2/S3/S6: a crashed pi's tasks keep running while another client keeps the
/// manager alive; a resumed pi re-attaches; another session is refused.
fn s3_crashed_session_survives_and_reattaches() {
    let home = Home::new("w-s3");
    let _d = home.start_daemon();
    let mut keeper = ext(&home, "keeper");
    let cmd = kit(&["sleep", "60000"]);
    let mut helper = HelperClient::spawn(&home.path, "resumable", &[cmd.as_str()]);
    let (id, pid) = helper.tasks[0].clone();
    helper.crash();
    let ok = poll_true(S(5), || {
        let st = keeper.request_ok(json!({"type":"status"}));
        st["sessions"].as_array().unwrap().iter().any(|s| s["session_id"] == "resumable" && s["connected"] == false)
    });
    assert!(ok, "crashed session must show disconnected");
    home.advance_now(6000);
    settle();
    assert!(pid_running(pid), "task of a crashed session killed while another client is connected");
    let r = keeper.request(json!({"type":"stop","task_id":id}));
    assert_eq!(r["error"]["code"], "E_FORBIDDEN", "{r}");
    let mut resumed = ext(&home, "resumable");
    assert_eq!(resumed.status_of(&id).as_deref(), Some("running"));
    resumed.request_ok(json!({"type":"stop","task_id":id}));
    assert!(resumed.wait_event(S(10), |e| e["event"] == "task_exited" && e["task_id"] == json!(id)).is_some());
    assert!(poll_true(S(5), || !pid_running(pid)));
}

/// S5: shutdown_session stops exactly the caller's running tasks.
fn s5_shutdown_session_stops_only_own_tasks() {
    let home = Home::new("w-s5");
    let _d = home.start_daemon();
    let mut a = ext(&home, "sess-a");
    let mut b = ext(&home, "sess-b");
    let (r1, p1) = a.start(&kit(&["sleep", "60000"]));
    let (other, po) = b.start(&kit(&["sleep", "60000"]));
    let r = a.request_ok(json!({"type":"shutdown_session"}));
    assert_eq!(r["stopped"], json!([r1]), "{r}");
    assert_eq!(a.wait_terminal(&r1, S(8)).unwrap()["status"], "killed");
    assert!(poll_true(S(5), || !pid_running(p1)));
    assert!(pid_running(po), "other session's task was killed");
    assert_eq!(b.status_of(&other).as_deref(), Some("running"));
}

// ===========================================================================
// Tasks (T1-T16, output)
// ===========================================================================

/// T1/T2: exit 0 -> completed, exit N -> failed(N), exact output, real
/// timestamps, `task_started` / `task_exited` to the owning session.
fn t1_exit_codes_output_and_events() {
    let home = Home::new("w-t1");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (ok, _) = c.start(&kit(&["echo", "hello", "world"]));
    let (bad, _) = c.start(&format!("{} && {}", kit(&["echo", "before"]), kit(&["exit", "3"])));
    let r = c.wait_terminal(&ok, S(10)).unwrap();
    assert_eq!((r["status"].as_str(), r["exit_code"].as_i64()), (Some("completed"), Some(0)), "{r}");
    assert_eq!(lf(&output_of(&mut c, &ok)), "hello world\n");
    let r = c.wait_terminal(&bad, S(10)).unwrap();
    assert_eq!((r["status"].as_str(), r["exit_code"].as_i64()), (Some("failed"), Some(3)), "{r}");
    assert!(r["signal"].is_null(), "{r}");
    assert_eq!(lf(&output_of(&mut c, &bad)), "before\n");
    let w = c.request_ok(json!({"type":"wait","task_id":bad,"budget_ms":100}));
    assert_eq!((w["done"].as_bool(), w["exit_code"].as_i64()), (Some(true), Some(3)));
    assert!(c.wait_event(S(5), |e| e["event"] == "task_exited" && e["task_id"] == json!(ok)).is_some());
    assert_eq!(home.record(&bad).unwrap()["status"], "failed");
    let rec = home.record(&ok).unwrap();
    let (started, ended) = (rec["started_at"].as_u64().unwrap(), rec["ended_at"].as_u64().unwrap());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    assert!(started > now - 60_000 && started <= ended && ended <= now + 1000, "{rec}");
}

/// T3: timeout_ms is a hard ceiling -> killed (end_reason timeout); a task
/// finishing earlier is unaffected.
fn t3_timeout_ms_hard_kill() {
    let home = Home::new("w-t3");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let mut slow = start_req(&kit(&["tree", "60000"]));
    slow["timeout_ms"] = json!(500);
    let (slow, _) = c.start_with(slow);
    let pids = wait_for_pids(&mut c, &slow, 2);
    let mut fast = start_req(&kit(&["echo", "ok"]));
    fast["timeout_ms"] = json!(10_000);
    let (fast, _) = c.start_with(fast);
    let t = c.wait_terminal(&slow, S(8)).expect("timeout must kill");
    assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("killed"), Some("timeout")), "{t}");
    assert!(poll_true(S(5), || pids.iter().all(|p| !pid_running(*p))), "timeout left {pids:?} running");
    let f = c.wait_terminal(&fast, S(8)).unwrap();
    assert_eq!((f["status"].as_str(), f["exit_code"].as_i64()), (Some("completed"), Some(0)), "{f}");
}

/// T4: stop -> killed, end_reason `stopped:tool`, reported to the session.
fn t4_stop_kills_a_running_task() {
    let home = Home::new("w-t4");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, pid) = c.start(&kit(&["sleep", "60000"]));
    assert!(pid_running(pid));
    c.request_ok(json!({"type":"stop","task_id":id}));
    let t = c.wait_terminal(&id, S(8)).expect("stop must end the task");
    assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("killed"), Some("stopped:tool")), "{t}");
    assert!(t["exit_code"].is_null(), "{t}");
    assert!(c.wait_event(S(5), |e| e["event"] == "task_exited" && e["task_id"] == json!(id)).is_some());
    assert!(poll_true(S(5), || !pid_running(pid)), "runner survived stop");
}

/// T6: stop takes the command and every descendant down.
fn t6_stop_kills_the_whole_tree() {
    let home = Home::new("w-t6");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["tree", "60000"]));
    let pids = wait_for_pids(&mut c, &id, 2);
    assert!(pids.iter().all(|p| pid_running(*p)), "{pids:?}");
    c.request_ok(json!({"type":"stop","task_id":id}));
    assert_eq!(c.wait_terminal(&id, S(8)).unwrap()["status"], "killed");
    let gone = poll_true(S(5), || pids.iter().all(|p| !pid_running(*p)));
    let alive: Vec<u32> = pids.iter().copied().filter(|p| pid_running(*p)).collect();
    for p in &alive {
        kill_pid(*p, SIGKILL);
    }
    assert!(gone, "stop left descendants running: {alive:?}");
}

/// T6c: a command that exits leaving a child behind is `completed`; the
/// child keeps running (the runner guards it), and the manager's shutdown
/// takes it down (§3.2: background work never outlives the manager).
fn t6c_leftover_outlives_its_command_until_shutdown() {
    let home = Home::new("w-t6c");
    let mut d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["leave", "60000"]));
    let left = wait_for_pids(&mut c, &id, 1)[0];
    let t = c.wait_terminal(&id, S(10)).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64()), (Some("completed"), Some(0)), "{t}");
    let deadline = Instant::now() + MS(1500);
    while Instant::now() < deadline {
        assert!(pid_running(left), "the leftover child died with its command");
        std::thread::sleep(MS(100));
    }
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut d, S(10)).is_some(), "daemon must exit");
    let gone = poll_true(S(5), || !pid_running(left));
    if !gone {
        kill_pid(left, SIGKILL);
    }
    assert!(gone, "leftover {left} outlived the manager");
}

/// T6d: `stop` on a finished task whose leftover still runs kills the
/// leftover; the record keeps its terminal status.
fn t6d_stop_reaches_the_leftover_of_a_finished_task() {
    let home = Home::new("w-t6d");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["leave", "60000"]));
    let left = wait_for_pids(&mut c, &id, 1)[0];
    assert_eq!(c.wait_terminal(&id, S(10)).unwrap()["status"], "completed");
    assert!(pid_running(left), "the leftover child died with its command");
    c.request_ok(json!({"type":"stop","task_id":id}));
    home.advance_partial("kill-grace", 2000);
    let gone = poll_true(S(6), || !pid_running(left));
    if !gone {
        kill_pid(left, SIGKILL);
    }
    assert!(gone, "stop did not reach the leftover {left}");
    assert_eq!(c.status_of(&id).as_deref(), Some("completed"));
}

/// T6e: a leftover that exits by itself leaves nothing to kill: a later
/// shutdown does not wait out a kill grace for it.
fn t6e_leftover_that_exits_leaves_nothing_to_kill() {
    let home = Home::new("w-t6e");
    let mut d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["leave", "300"]));
    let left = wait_for_pids(&mut c, &id, 1)[0];
    c.wait_terminal(&id, S(10)).unwrap();
    assert!(poll_true(S(5), || !pid_running(left)));
    std::thread::sleep(MS(300));
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    let exited = wait_child(&mut d, S(3));
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default();
    assert!(exited.is_some(), "shutdown waited on an emptied leftover\n{log}");
    assert!(!log.contains("leftover process group"), "{log}");
}

/// T7/T8/D4: the daemon is killed outright. Every task goes with it
/// (running ones, their descendants, a finished task's leftover); the next
/// daemon marks the running records orphaned (manager-crash) and signals
/// nothing.
fn t7_daemon_kill_takes_every_task_down() {
    let home = Home::new("w-t7");
    let mut d1 = home.start_daemon();
    let mut c = ext(&home, "sess-crash");
    let (plain, p_plain) = c.start(&kit(&["sleep", "60000"]));
    let (tree, _) = c.start(&kit(&["tree", "60000"]));
    let tree_pids = wait_for_pids(&mut c, &tree, 2);
    let (finished, _) = c.start(&kit(&["leave", "60000"]));
    let left = wait_for_pids(&mut c, &finished, 1)[0];
    assert_eq!(c.wait_terminal(&finished, S(10)).unwrap()["status"], "completed");
    let mut all = vec![p_plain, left];
    all.extend(&tree_pids);
    assert!(all.iter().all(|p| pid_running(*p)), "fixture not running: {all:?}");

    d1.kill().unwrap();
    d1.wait().unwrap();
    drop(c);
    let gone = poll_true(S(8), || all.iter().all(|p| !pid_running(*p)));
    let alive: Vec<u32> = all.iter().copied().filter(|p| pid_running(*p)).collect();
    for p in &alive {
        kill_pid(*p, SIGKILL);
    }
    assert!(gone, "outlived the killed manager: {alive:?}");

    let _d2 = home.start_daemon();
    let mut c = ext(&home, "sess-crash");
    for id in [&plain, &tree] {
        let t = c.task(id).expect("listed after restart");
        assert_eq!((t["status"].as_str(), t["end_reason"].as_str()), (Some("orphaned"), Some("manager-crash")), "{t}");
    }
    assert_eq!(c.task(&finished).unwrap()["status"], "completed");
}

/// T12: stop on a terminal task is a no-op; an unknown id is E_NOT_FOUND.
fn t12_stop_terminal_task_is_noop() {
    let home = Home::new("w-t12");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["exit", "4"]));
    assert_eq!(c.wait_terminal(&id, S(10)).unwrap()["status"], "failed");
    c.request_ok(json!({"type":"stop","task_id":id}));
    std::thread::sleep(MS(300));
    let t = c.task(&id).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64()), (Some("failed"), Some(4)));
    let r = c.request(json!({"type":"stop","task_id":"sh_00000000"}));
    assert_eq!(r["error"]["code"], "E_NOT_FOUND");
}

/// T13: terminal records and their exact output survive a restart.
fn t13_terminal_records_survive_restart() {
    let home = Home::new("w-t13");
    let mut d1 = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (ok, _) = c.start(&kit(&["bytes", "300000"]));
    let (bad, _) = c.start(&format!("{} && {}", kit(&["echo", "boom"]), kit(&["exit", "2"])));
    c.wait_terminal(&ok, S(10)).unwrap();
    c.wait_terminal(&bad, S(10)).unwrap();
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    assert!(wait_child(&mut d1, S(10)).is_some(), "daemon did not exit after shutdown");

    let _d2 = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let t = c.task(&bad).unwrap();
    assert_eq!((t["status"].as_str(), t["exit_code"].as_i64()), (Some("failed"), Some(2)));
    let (text, _, last) = c.read_all_output(&ok, 65536);
    assert_eq!(last["total_size"], 300000);
    assert!(text.as_bytes() == kit::bytes_pattern(300000), "output changed across the restart");
}

/// T14: a `wait` whose budget expires answers done:false; the task runs on.
fn t14_wait_budget_expires_while_running() {
    let home = Home::new("w-t14");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, pid) = c.start(&kit(&["sleep", "60000"]));
    let w = c.request_ok(json!({"type":"wait","task_id":id,"budget_ms":300}));
    assert_eq!(w["done"], false, "{w}");
    assert!(pid_running(pid));
    assert_eq!(c.status_of(&id).as_deref(), Some("running"));
}

/// T16: multi-byte characters written one byte at a time are never split or
/// replaced, in `output` reads and in watch events.
fn t16_utf8_split_across_writes() {
    let home = Home::new("w-t16");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["utf8"]));
    c.request_ok(json!({"type":"watch","task_id":id}));
    c.wait_terminal(&id, S(10)).unwrap();
    assert_eq!(output_of(&mut c, &id), "aé中😀b\n");
    c.drain(MS(300));
    let watched: String = c
        .events
        .iter()
        .filter(|e| e["event"] == "output" && e["task_id"] == json!(id))
        .map(|e| e["chunk"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(watched, "aé中😀b\n");
}

/// O1: 16 MiB of output arrive byte-exact on disk and on the wire, and the
/// terminal record counts every byte.
fn o1_large_output_exact_bytes() {
    const N: u64 = 16 << 20;
    let home = Home::new("w-o1");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["bytes", &N.to_string()]));
    let t = c.wait_terminal(&id, S(60)).unwrap();
    assert_eq!(t["status"], "completed", "{t}");
    assert_eq!(t["output_size"], N, "{t}");
    let path = t["output_path"].as_str().unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), N);
    let (text, cursor, _) = c.read_all_output(&id, 1 << 20);
    assert_eq!((text.len() as u64, cursor), (N, N));
}

/// stderr lands in the merged output and in the `.stderr` mirror.
fn o5_stderr_is_merged_and_mirrored() {
    let home = Home::new("w-o5");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&format!("{} && {}", kit(&["echo", "to-out"]), kit(&["stderr", "to-err"])));
    let t = c.wait_terminal(&id, S(10)).unwrap();
    let out = lf(&output_of(&mut c, &id));
    assert!(out.contains("to-out\n") && out.contains("to-err\n"), "{out:?}");
    let mirror = format!("{}.stderr", t["output_path"].as_str().unwrap().trim_end_matches(".output"));
    assert_eq!(lf(&std::fs::read_to_string(&mirror).unwrap()), "to-err\n");
}

/// §3.3: the task runs in the requested cwd with exactly the requested env.
fn e1_task_gets_its_cwd_and_env() {
    let home = Home::new("w-e1");
    let _d = home.start_daemon_from(std::path::Path::new(BIN), &[("PI_FAMULUS_NOT_SENT", "leak")]);
    let mut c = ext(&home, "sess-a");
    let dir = home.path.join("work dir");
    std::fs::create_dir_all(&dir).unwrap();
    let mut env = task_env();
    env["PI_FAMULUS_PROBE"] = json!("probe value");
    let (id, _) = c.start_with(json!({"type":"start","kind":"shell",
        "command":format!("{} && {} && {}", kit(&["cwd"]), kit(&["env","PI_FAMULUS_PROBE"]), kit(&["env","PI_FAMULUS_NOT_SENT"])),
        "cwd":dir.to_string_lossy(),"env":env}));
    c.wait_terminal(&id, S(10)).unwrap();
    let out = lf(&output_of(&mut c, &id));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out:?}");
    let same = |a: &str, b: &std::path::Path| {
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).ok();
        canon(std::path::Path::new(a)) == canon(b)
    };
    assert!(same(lines[0], &dir), "cwd {:?} != {}", lines[0], dir.display());
    assert_eq!(&lines[1..], ["probe value", "<unset>"]);
}

/// The command string reaches the shell verbatim: double-quoted arguments
/// with spaces arrive as single arguments.
fn q1_quoted_arguments_reach_the_program() {
    let home = Home::new("w-q1");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["argv", "\"hello world\"", "plain", "\"a&b\""]));
    let t = c.wait_terminal(&id, S(10)).unwrap();
    assert_eq!(t["exit_code"], 0, "{t}; output {:?}", output_of(&mut c, &id));
    assert_eq!(lf(&output_of(&mut c, &id)), "hello world\nplain\na&b\n");
}

/// Commands come from pi's bash tool: they must run in a POSIX shell, as
/// pi itself runs them (on Windows pi uses Git Bash).
fn q2_commands_run_in_a_posix_shell() {
    let home = Home::new("w-q2");
    let _d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start("x=21; echo \"answer=$((x * 2))\"; test -n \"$x\" && echo 'single quoted'");
    let t = c.wait_terminal(&id, S(10)).unwrap();
    let out = lf(&output_of(&mut c, &id));
    assert_eq!((t["exit_code"].as_i64(), out.as_str()), (Some(0), "answer=42\nsingle quoted\n"), "{t}");
}

/// Windows without bash: the `cmd.exe` fallback gets the command line
/// verbatim (`cmd /d /s /c "<command>"`), not re-quoted for a C runtime.
#[cfg(windows)]
fn q3_cmd_fallback_passes_the_command_verbatim() {
    let home = Home::new("w-q3");
    let _d = home.start_daemon_from(std::path::Path::new(BIN), &[("PI_FAMULUS_SHELL", "cmd.exe")]);
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&format!("{} && echo %PI_FAMULUS_Q3%", kit(&["argv", "\"hello world\"", "plain"])));
    let mut env = task_env();
    env["PI_FAMULUS_Q3"] = json!("expanded");
    let (id2, _) = c.start_with(json!({"type":"start","kind":"shell","command":"echo %PI_FAMULUS_Q3%","cwd":task_cwd(),"env":env}));
    let t = c.wait_terminal(&id, S(10)).unwrap();
    assert_eq!(t["exit_code"], 0, "{t}; output {:?}", output_of(&mut c, &id));
    assert!(lf(&output_of(&mut c, &id)).starts_with("hello world\nplain\n"), "{:?}", output_of(&mut c, &id));
    c.wait_terminal(&id2, S(10)).unwrap();
    assert_eq!(lf(&output_of(&mut c, &id2)), "expanded\n");
}

/// Many tasks started and stopped at once from several connections: every
/// stopped task finishes promptly with its whole tree gone, while the others
/// keep running. A pipe end or job escaping into a concurrently spawned
/// task (inheritable handles on Windows) shows up here as a stop that never
/// completes or a descendant that survives.
fn x1_concurrent_starts_and_stops_finish_promptly() {
    let home = Home::new("w-x1");
    let _d = home.start_daemon();
    let _keeper = ext(&home, "keeper");
    let scope = test_scope();
    let started: Vec<(String, Vec<u32>, bool)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|t| {
                let home = &home;
                s.spawn(move || {
                    enter_test_scope(scope);
                    let mut c = ext(home, &format!("sess-x{t}"));
                    let mut v = Vec::new();
                    for i in 0..8 {
                        let victim = i % 2 == 0;
                        let (id, _) = c.start(&kit(&["tree", "60000"]));
                        v.push((id, victim));
                    }
                    let mut out = Vec::new();
                    for (id, victim) in v {
                        let pids = wait_for_pids(&mut c, &id, 2);
                        out.push((id, pids, victim));
                    }
                    for (id, _, victim) in &out {
                        if *victim {
                            c.request_ok(json!({"type":"stop","task_id":id}));
                        }
                    }
                    for (id, _, victim) in &out {
                        if *victim {
                            let t = c.wait_terminal(id, S(8));
                            assert!(t.is_some(), "stopped task {id} did not finish");
                        }
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    for (id, pids, victim) in &started {
        if *victim {
            assert!(poll_true(S(5), || pids.iter().all(|p| !pid_running(*p))), "{id}: stop left {pids:?}");
        } else {
            assert!(pids.iter().all(|p| pid_running(*p)), "{id}: a bystander task died: {pids:?}");
        }
    }
}

// ===========================================================================
// Daemon (D1, D3, D5, D6, D9, D10, D12, D16, D17)
// ===========================================================================

/// D1: clients racing to auto-spawn are all served by one daemon, which
/// holds manager.lock.
fn d1_concurrent_clients_spawn_one_daemon() {
    let home = Home::new("w-d1");
    let outs: Vec<CliOut> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let path = home.path.clone();
                let clock = if home.manual { "manual" } else { "" };
                s.spawn(move || {
                    let sess = format!("race-{i}");
                    let cmd = kit(&["echo", "x"]);
                    run_cli_env(&path, &["start", "--session", &sess, &cmd], S(30), &[("PI_FAMULUS_TEST_CLOCK", clock)])
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for o in &outs {
        assert!(o.status.success(), "a racing client failed: {}{}", o.stdout, o.stderr);
    }
    let pid = home.pidfile_pid().expect("a daemon");
    let mut c = home.connect();
    assert_eq!(c.hello_cli()["pid"].as_u64(), Some(pid as u64));
    let st = c.request_ok(json!({"type":"status"}));
    for i in 0..8 {
        let sess = format!("race-{i}");
        assert!(st["sessions"].as_array().unwrap().iter().any(|s| s["session_id"] == json!(sess)), "{sess} not served by {pid}: {st}");
    }
    assert!(lifetime_lock_held(&home), "nobody holds manager.lock");
}

/// D3/D12: several `daemon` processes at once: exactly one serves, the rest
/// exit 0 ("already running") without touching it.
fn d3_concurrent_daemons_leave_one_survivor() {
    let home = Home::new("w-d3");
    let mut kids: Vec<_> = (0..6).map(|_| home.spawn_daemon()).collect();
    assert!(poll_true(S(10), || home.reachable() && home.pidfile_pid().is_some()));
    let survivor = home.pidfile_pid().unwrap();
    let mut exited = 0;
    for k in kids.iter_mut() {
        if k.id() == survivor {
            continue;
        }
        let st = wait_child(k, S(10)).expect("a losing daemon did not exit");
        assert!(st.success(), "a losing daemon failed: {st:?}");
        exited += 1;
    }
    assert_eq!(exited, 5);
    let mut c = home.connect();
    assert_eq!(c.hello_cli()["pid"].as_u64(), Some(survivor as u64));
}

/// D5/C9/T11: the only client crashes. Nothing happens during the 5 s idle
/// grace; then every task is killed, records say `killed`, the pid file is
/// gone and the daemon exits 0.
fn d5_last_client_crash_idles_out_and_kills_tasks() {
    let home = Home::new("w-d5");
    let mut daemon = home.start_daemon();
    let (a, b) = (kit(&["sleep", "60000"]), kit(&["tree", "60000"]));
    let mut helper = HelperClient::spawn(&home.path, "sess-crash", &[a.as_str(), b.as_str()]);
    let tree_id = helper.tasks[1].0.clone();
    let pids: Vec<u32> = {
        let mut cli = home.connect();
        cli.hello_cli();
        let mut p = wait_for_pids(&mut cli, &tree_id, 2);
        p.push(helper.tasks[0].1);
        p
    };
    helper.crash();
    home.advance_almost("idle", 5000);
    assert!(daemon.try_wait().unwrap().is_none(), "daemon exited inside the 5s grace");
    assert!(pids.iter().all(|p| pid_running(*p)), "killed inside the 5s grace");
    home.advance_past();
    home.advance("shutdown-grace", 2000);
    let st = wait_child(&mut daemon, S(15)).expect("daemon must exit after the grace");
    assert!(st.success(), "{st:?}");
    assert!(poll_true(S(5), || pids.iter().all(|p| !pid_running(*p))), "survived the shutdown");
    assert!(!home.pidfile().exists(), "manager.pid left behind");
    assert!(!home.reachable(), "socket still answers");
    for (id, _) in &helper.tasks {
        let r = home.record(id).unwrap();
        assert_eq!((r["status"].as_str(), r["end_reason"].as_str()), (Some("killed"), Some("manager-shutdown")), "{r}");
    }
}

/// D6: a hello inside the grace cancels the shutdown.
fn d6_hello_inside_grace_cancels_shutdown() {
    let home = Home::new("w-d6");
    let mut daemon = home.start_daemon();
    let mut a = ext(&home, "sess-a");
    let (id, pid) = a.start(&kit(&["sleep", "60000"]));
    drop(a);
    home.advance("idle", 2500);
    let mut b = home.connect();
    assert_eq!(b.hello_cli()["ok"], true);
    home.advance_now(5000);
    settle();
    assert!(daemon.try_wait().unwrap().is_none(), "shutdown was not cancelled");
    assert!(pid_running(pid));
    assert_eq!(b.status_of(&id).as_deref(), Some("running"));
}

/// D9: `shutdown` with an extension still connected kills the tasks, cleans
/// the files, and closes the extension's connection.
fn d9_cli_shutdown_kills_tasks_and_cleans_files() {
    let home = Home::new("w-d9");
    let mut daemon = home.start_daemon();
    let mut a = ext(&home, "sess-a");
    let (id, pid) = a.start(&kit(&["sleep", "60000"]));
    let out = home.cli(&["shutdown"], S(10));
    assert!(out.status.success(), "{}", out.stderr);
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut daemon, S(10)).is_some(), "daemon must exit after shutdown");
    assert!(poll_true(S(5), || !pid_running(pid)));
    assert!(!home.pidfile().exists());
    assert_eq!(home.record(&id).unwrap()["status"], "killed");
    assert!(a.wait_closed(S(3)), "the extension must see the connection close");
}

/// D10: after a killed daemon (stale pid file), an auto-spawning CLI call
/// recovers with a new daemon.
fn d10_cli_recovers_from_a_killed_daemon() {
    let home = Home::new("w-d10");
    assert!(home.cli(&["ls"], S(15)).status.success());
    let old = home.pidfile_pid().unwrap();
    kill_pid(old, SIGKILL);
    assert!(poll_true(S(5), || !pid_running(old)));
    assert!(home.pidfile().exists());
    let out = home.cli(&["ls"], S(15));
    assert!(out.status.success(), "{}", out.stderr);
    let new = home.pidfile_pid().unwrap();
    assert_ne!(new, old);
    let mut c = home.connect();
    assert_eq!(c.hello_cli()["pid"].as_u64(), Some(new as u64));
}

/// D13: manager.pid names a live process that is not a manager.
fn d12_reused_pid_in_pidfile_does_not_block_startup() {
    let home = Home::new("w-d12");
    let mut impostor = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["taskkit", "sleep", "30000"])
        .spawn()
        .unwrap();
    std::fs::write(home.pidfile(), format!(r#"{{"pid":{},"version":"0.1.0","started_at":0}}"#, impostor.id())).unwrap();
    let out = home.cli(&["ls"], S(15));
    let alive = impostor.try_wait().unwrap().is_none();
    let _ = impostor.kill();
    let _ = impostor.wait();
    assert!(out.status.success(), "manager blocked by a reused pid: {}", out.stderr);
    assert!(alive, "the daemon signalled the pid from manager.pid");
}

/// D15: an extension cannot shut the manager down; status is open to it.
fn d16_extension_cannot_shutdown() {
    let home = Home::new("w-d16");
    let mut d = home.start_daemon();
    let mut c = ext(&home, "sess-a");
    assert_eq!(c.request(json!({"type":"shutdown"}))["error"]["code"], "E_FORBIDDEN");
    assert_eq!(c.request(json!({"type":"status"}))["ok"], true);
    std::thread::sleep(MS(300));
    assert!(d.try_wait().unwrap().is_none(), "extension shut the manager down");
}

/// D17: doctor removes a stale pid file only while no daemon runs, and
/// never touches a live one.
fn d17_doctor_cleans_only_without_a_daemon() {
    let home = Home::new("w-d17");
    let out = home.cli(&["doctor"], S(10));
    assert!(out.stdout.contains("no stale files"), "{}", out.stdout);
    std::fs::write(home.pidfile(), br#"{"pid":1,"version":"0","started_at":0}"#).unwrap();
    let out = home.cli(&["doctor"], S(10));
    assert!(out.status.success(), "{}{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("removed stale"), "{}", out.stdout);
    assert!(!home.pidfile().exists(), "stale pid file kept");

    let _d = home.start_daemon();
    let pid = home.pidfile_pid().unwrap();
    let out = home.cli(&["doctor"], S(10));
    assert!(out.stdout.contains("hello ok"), "{}", out.stdout);
    assert!(out.status.success(), "{}", out.stdout);
    assert_eq!(home.pidfile_pid(), Some(pid), "doctor touched a live daemon");
}

/// One home, spelled differently (trailing separator; on Windows also `/`
/// separators and another letter case, which name the same directory),
/// reaches the same daemon instead of spawning one that cannot get the lock.
fn h1_home_spellings_reach_one_daemon() {
    let home = Home::new("w-h1");
    assert!(home.cli(&["ls"], S(15)).status.success());
    let pid = home.pidfile_pid().unwrap();
    let base = home.path.to_string_lossy().into_owned();
    let mut spellings = vec![format!("{base}{}", std::path::MAIN_SEPARATOR)];
    if cfg!(windows) {
        spellings.push(base.replace('\\', "/"));
        spellings.push(base.to_uppercase());
    }
    for spelled in spellings {
        let out = run_cli_env(std::path::Path::new(&spelled), &["status", "--json"], S(15), &[]);
        assert!(out.status.success(), "--home {spelled}: {}{}", out.stdout, out.stderr);
        let st: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", out.stdout));
        assert_eq!(st["pid"].as_u64(), Some(pid as u64), "--home {spelled} reached another daemon: {st}");
    }
}

/// The CLI end to end: start, wait, output, ls, show, stop, status.
fn cli1_start_wait_output_ls_show_stop() {
    let home = Home::new("w-cli1");
    let cwd = task_cwd();
    let cmd = kit(&["echo", "cli-roundtrip"]);
    let out = home.cli(&["start", "--cwd", &cwd, &cmd], S(15));
    assert!(out.status.success(), "{}{}", out.stdout, out.stderr);
    let id = out.stdout.trim().strip_prefix("task_id=").unwrap().split_whitespace().next().unwrap().to_string();
    let out = home.cli(&["wait", &id, "--budget-ms", "10000"], S(15));
    assert_eq!(lf(&out.stdout).trim(), "done exit_code=0", "{}", out.stderr);
    let out = home.cli(&["output", &id], S(10));
    assert_eq!(lf(&out.stdout), "cli-roundtrip\n");
    let out = home.cli(&["show", &id], S(10));
    assert!(out.status.success() && out.stdout.contains(&id), "{}{}", out.stdout, out.stderr);
    let out = home.cli(&["stop", &id], S(10));
    assert!(out.stdout.contains("already finished"), "{}{}", out.stdout, out.stderr);

    let long = kit(&["sleep", "60000"]);
    let out = home.cli(&["start", "--cwd", &cwd, &long], S(15));
    let id2 = out.stdout.trim().strip_prefix("task_id=").unwrap().split_whitespace().next().unwrap().to_string();
    let out = home.cli(&["wait", &id2, "--budget-ms", "200"], S(10));
    assert!(out.stdout.starts_with("not done"), "{}", out.stdout);
    let out = home.cli(&["ls", "--json"], S(10));
    assert!(out.stdout.contains(&id2), "{}{}", out.stdout, out.stderr);
    let out = home.cli(&["stop", &id2], S(10));
    assert_eq!(lf(&out.stdout).trim(), format!("stopped {id2}"), "{}", out.stderr);
    home.advance_partial("kill-grace", 2000);
    let out = home.cli(&["wait", &id2, "--budget-ms", "10000"], S(15));
    assert!(out.stdout.starts_with("done"), "{}", out.stdout);
    let out = home.cli(&["status", "--json"], S(10));
    let st: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", out.stdout));
    assert_eq!(st["pid"].as_u64(), home.pidfile_pid().map(u64::from), "{st}");
}

/// §3.1 step 3: the daemon a CLI call spawns is detached: it keeps serving
/// after that CLI process has exited, and runs tasks started later. It holds
/// none of the CLI's stdio: a reader of the CLI's output sees EOF when the
/// CLI exits, not when the daemon does.
fn cli2_auto_spawned_daemon_outlives_the_cli() {
    let home = Home::new("w-cli2");
    let t0 = Instant::now();
    let out = home.cli(&["ls"], S(15));
    assert!(out.status.success(), "{}{}", out.stdout, out.stderr);
    assert!(t0.elapsed() < S(4), "the CLI's output stayed open for {:?}", t0.elapsed());
    let pid = home.pidfile_pid().expect("spawned daemon");
    std::thread::sleep(MS(500));
    assert!(pid_running(pid), "the spawned daemon died with its CLI");
    let mut c = ext(&home, "sess-a");
    let (id, _) = c.start(&kit(&["echo", "after"]));
    c.wait_terminal(&id, S(10)).unwrap();
    assert_eq!(lf(&output_of(&mut c, &id)), "after\n");
}

/// Windows has no in-place upgrade: `upgrade` says so, and the daemon keeps
/// its pid and its tasks.
#[cfg(windows)]
fn u1_upgrade_is_refused_and_the_daemon_keeps_serving() {
    let home = Home::new("w-u1");
    let _d = home.start_daemon();
    let pid = home.pidfile_pid().unwrap();
    let mut c = ext(&home, "sess-a");
    let (id, tpid) = c.start(&kit(&["sleep", "60000"]));
    let out = home.cli(&["upgrade"], S(30));
    assert!(!out.status.success(), "{}{}", out.stdout, out.stderr);
    assert!(format!("{}{}", out.stdout, out.stderr).contains("not supported on Windows"), "{}{}", out.stdout, out.stderr);
    assert_eq!(home.pidfile_pid(), Some(pid));
    assert!(pid_running(tpid));
    assert_eq!(c.status_of(&id).as_deref(), Some("running"));
}
