#![cfg(unix)]
//! Tests written to kill cargo-mutants survivors (see manager/TESTING.md,
//! "Mutation score"). Each asserts a customer-visible behaviour that the
//! surviving mutant broke without any existing test noticing.

mod common;

use common::*;
use serde_json::json;
use std::os::unix::process::CommandExt;
use std::time::Duration;

const S: fn(u64) -> Duration = Duration::from_secs;

/// Kills: daemon.rs handle_output ring/disk window arithmetic and the
/// MAX_OUTPUT_READ constant. Any cursor/max_bytes pair returns exactly the
/// bytes at that offset, whether the range sits in the 64KB memory ring, on
/// disk only, or straddles the ring start; a read with enough data available
/// returns exactly max_bytes.
#[test]
fn g1_output_reads_exact_ranges_across_ring_and_disk() {
    let home = Home::new("g1");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let want: String = (1..=30000).map(|i| format!("{i}\n")).collect();
    let total = want.len() as u64; // ~169 KB, well past the 64 KB ring
    // Keep the task running so the live ring (not a reloaded record) serves.
    let (id, _) = c.start("seq 1 30000; sleep 300");
    assert!(
        poll_true(S(5), || {
            let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":1}));
            r["total_size"] == total
        }),
        "output did not reach {total} bytes"
    );
    let ring_start = total - 64 * 1024;
    let cursors = [
        0,
        1,
        1000,
        ring_start - 1,
        ring_start,
        ring_start + 1,
        ring_start + 5000,
        total - 10,
        total - 1,
    ];
    for &cur in &cursors {
        for &max in &[1u64, 7, 4096, 65536, 100_000] {
            let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":cur,"max_bytes":max}));
            let end = (cur + max).min(total) as usize;
            let expect = &want[cur as usize..end];
            assert_eq!(
                r["chunk"].as_str().unwrap(),
                expect,
                "cursor={cur} max={max} (ring starts at {ring_start})"
            );
            assert_eq!(r["next_cursor"], end as u64, "cursor={cur} max={max}");
            assert_eq!(r["status"], "running");
        }
    }
}

/// Kills: sys.rs apply_new_session_std (daemon detach). A daemon that a
/// client auto-spawned must not share the client's process group: Ctrl-C in
/// the terminal that ran the client (SIGINT to its group) must not take the
/// manager and every background task down with it.
#[test]
fn g2_autospawned_daemon_survives_sigint_to_spawning_group() {
    let home = Home::new("g2");
    let mut cli = std::process::Command::new(BIN)
        .arg("--home")
        .arg(&home.path)
        .arg("ls")
        .process_group(0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pgid = cli.id();
    assert!(wait_child(&mut cli, S(10)).expect("cli finished").success());
    let daemon = home.pidfile_pid().expect("daemon pid");
    assert!(pid_running(daemon));
    kill_group(pgid, libc::SIGINT);
    kill_group(pgid, libc::SIGHUP);
    std::thread::sleep(Duration::from_millis(500));
    assert!(pid_running(daemon), "daemon died with the spawning client's process group");
}

/// Kills: lifecycle.rs log_line, daemon.rs graceful_shutdown reason logging.
/// §3.2 requires the kill reason "manager_shutdown"; TaskRecord's field set is
/// fixed, so manager.log (surfaced by `pi-famulus log`) is where it lives.
#[test]
fn g3_manager_log_records_shutdown_reason() {
    let home = Home::new("g3");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    c.start("sleep 300");
    drop(c);
    home.advance("idle", 5000);
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut d, S(12)).is_some());
    let out = home.cli(&["log", "-n", "50"], S(5));
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stdout.contains("daemon started"), "{}", out.stdout);
    assert!(
        out.stdout.contains("killed 1 task(s) (reason: manager_shutdown)"),
        "{}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("leftover process group"),
        "no leftover groups existed: {}",
        out.stdout
    );
    // The `log` subcommand must not have resurrected the manager.
    assert!(!home.sock().exists());
}

/// Kills: daemon.rs handle_list session filter. `list --session X` from the
/// CLI shows exactly X's tasks; without a filter the CLI sees all sessions.
#[test]
fn g4_cli_list_session_filter() {
    let home = Home::new("g4");
    let _d = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let mut b = home.connect();
    b.hello_ext("sess-b");
    let (ta, _) = a.start("sleep 300");
    let (tb, _) = b.start("sleep 300");
    let mut cli = home.connect();
    cli.hello_cli();
    let ids = |r: &serde_json::Value| -> Vec<String> {
        r["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["task_id"].as_str().unwrap().to_string())
            .collect()
    };
    let r = cli.request_ok(json!({"type":"list","all":true,"session_id":"sess-a"}));
    assert_eq!(ids(&r), vec![ta.clone()]);
    let r = cli.request_ok(json!({"type":"list","all":true}));
    let mut all = ids(&r);
    all.sort();
    let mut want = vec![ta.clone(), tb.clone()];
    want.sort();
    assert_eq!(all, want);
    // An extension cannot widen its view with a session filter.
    let r = a.request_ok(json!({"type":"list","all":true,"session_id":"sess-b"}));
    assert_eq!(ids(&r), vec![ta]);
    let out = home.cli(&["list", "--session", "sess-b"], S(5));
    assert!(out.stdout.contains(&tb) && !out.stdout.contains("sess-a"), "{}", out.stdout);
}

/// Kills: daemon.rs handle_status task counting.
#[test]
fn g5_status_task_counts() {
    let home = Home::new("g5");
    let _d = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let (done1, _) = a.start("true");
    let (done2, _) = a.start("exit 1");
    a.start("sleep 300");
    a.wait_terminal(&done1, S(3)).unwrap();
    a.wait_terminal(&done2, S(3)).unwrap();
    let mut cli = home.connect();
    cli.hello_cli();
    let st = cli.request_ok(json!({"type":"status"}));
    assert_eq!(st["task_counts"], json!({"running":1,"terminal":2}), "{st}");
    let s: Vec<_> = st["sessions"].as_array().unwrap().iter().collect();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["pi_pid"], std::process::id());
}

/// Kills: daemon.rs maybe_arm_idle_timer guard. Clients leave one at a time
/// with a long gap: when the *last* one leaves, the idle countdown must still
/// run and take the manager (and its tasks) down.
#[test]
fn g7_staggered_disconnects_still_reach_idle_shutdown() {
    let home = Home::new("g7");
    let mut d = home.start_daemon();
    let mut a = home.connect();
    a.hello_ext("sess-a");
    let mut b = home.connect();
    b.hello_ext("sess-b");
    let (_, pid) = b.start("sleep 300");
    drop(a);
    // Longer than the 5s grace while b is still connected.
    home.advance_now(6000);
    settle();
    assert!(d.try_wait().unwrap().is_none(), "shut down while a client was connected");
    drop(b);
    home.advance("idle", 5000);
    home.advance("shutdown-grace", 2000);
    assert!(
        wait_child(&mut d, S(10)).is_some(),
        "daemon never idle-exited after the last client left"
    );
    assert!(poll_true(S(1), || !pid_running(pid)));
}

/// Kills: task.rs read_file_range NotFound guard. An output file that exists
/// but cannot be read is an error, not silently empty output.
#[test]
fn g8_unreadable_output_is_an_error_not_empty() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new("g8");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    // 100 KB: cursor 0 falls outside the 64 KB ring and is served from disk.
    let (id, _) = c.start("head -c 100000 /dev/zero | tr '\\0' 'z'");
    let t = c.wait_terminal(&id, S(5)).unwrap();
    let path = t["output_path"].as_str().unwrap().to_string();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = c.request(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":100}));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores file modes; nothing to assert
    }
    assert_eq!(r["ok"], false, "unreadable output reported as success: {r}");
    assert_eq!(r["error"]["code"], "E_INTERNAL");
}

/// Kills: daemon.rs respond / writer_task frame-size boundary (`>` vs `>=`).
/// A response of exactly 4 MiB is legal (§3.3 "max frame 4 MiB") and must
/// arrive intact, not be replaced by an error or dropped.
#[test]
fn g9_response_of_exactly_max_frame_is_delivered() {
    let home = Home::new("g9");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_cli();
    // Response size = fixed envelope + |id| + |task_id| (both echoed once).
    let probe = c.request(json!({"type":"stop","task_id":"t"}));
    assert_eq!(probe["error"]["code"], "E_NOT_FOUND");
    let probe_len = serde_json::to_vec(&probe).unwrap().len();
    let probe_id_len = probe["id"].as_str().unwrap().len();
    let envelope = probe_len - probe_id_len - 1;
    let task = "t".repeat(1 << 20);
    let id = format!("g9-{}", "i".repeat(MAX_FRAME - envelope - task.len() - 3));
    let req = json!({"v":1,"id":id,"type":"stop","task_id":task});
    assert!(serde_json::to_vec(&req).unwrap().len() <= MAX_FRAME);
    c.send(&req);
    let r = c.wait_id(&id, S(10)).expect("exactly-4MiB response was dropped");
    assert_eq!(serde_json::to_vec(&r).unwrap().len(), MAX_FRAME, "test arithmetic");
    assert_eq!(r["error"]["code"], "E_NOT_FOUND", "4 MiB response was replaced");
}

/// CPU time used by `pid` so far, in milliseconds (`ps -o time=`).
fn cpu_ms(pid: u32) -> u64 {
    let o = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    // [[dd-]hh:]mm:ss[.cc]
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let (rest, frac) = s.split_once('.').unwrap_or((s.as_str(), "0"));
    let secs = rest
        .split(':')
        .fold(0u64, |acc, p| acc * 60 + p.trim_start_matches('-').parse::<u64>().unwrap_or(0));
    let frac_ms = format!("{frac:0<3}")[..3].parse::<u64>().unwrap_or(0);
    secs * 1000 + frac_ms
}

/// Kills: daemon.rs spawn_output_fanout EOF handling (a busy loop after the
/// tee channel closes). A daemon whose tasks have all finished must be idle.
#[test]
fn g10_daemon_is_idle_after_watched_tasks_finish() {
    let home = Home::new("g10");
    let d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    for _ in 0..3 {
        let (id, _) = c.start("sleep 0.2; echo done");
        c.request_ok(json!({"type":"watch","task_id":id}));
        c.wait_terminal(&id, S(5)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    let before = cpu_ms(d.id());
    std::thread::sleep(Duration::from_millis(1500));
    let used = cpu_ms(d.id()) - before;
    assert!(used < 300, "idle daemon used {used}ms CPU in 1.5s");
}

/// A leftover process group stops being tracked once it empties (its
/// guardian runner exits), so shutdown only reports (and signals) groups
/// that still have members; a pgid that died is never signalled.
#[test]
fn g11_shutdown_counts_only_live_leftover_groups() {
    let home = Home::new("g11");
    let mut d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (gone, gone_runner) = c.start("sleep 0.3 >/dev/null 2>&1 & echo $!");
    let (live, _) = c.start("sleep 300 >/dev/null 2>&1 & echo $!");
    c.wait_terminal(&gone, S(3)).unwrap();
    c.wait_terminal(&live, S(3)).unwrap();
    let gone_gc = wait_for_pids(&mut c, &gone, 1)[0];
    let gc = wait_for_pids(&mut c, &live, 1)[0];
    // The first group empties; its guardian runner then exits.
    assert!(poll_true(S(3), || !pid_running(gone_gc) && !pid_running(gone_runner)));
    settle();
    drop(c);
    assert!(home.cli(&["shutdown"], S(10)).status.success());
    home.advance("shutdown-grace", 2000);
    assert!(wait_child(&mut d, S(10)).is_some());
    assert!(poll_true(S(2), || !pid_running(gc)), "live leftover survived");
    let log = std::fs::read_to_string(home.path.join("manager.log")).unwrap();
    assert!(
        log.contains("killed 1 leftover process group(s) of finished tasks"),
        "{log}"
    );
    assert!(!log.contains("killed 0 task(s)"), "no task was running: {log}");
}

/// Kills: lifecycle.rs clean_if_no_daemon (doctor). With no daemon, doctor
/// removes stale socket/pid files; with a live daemon (whatever manager.pid
/// says) it touches nothing and reports the socket healthy.
#[test]
fn g13_doctor_cleans_only_without_a_daemon() {
    let home = Home::new("g13");
    // Nothing at all: healthy, nothing to do.
    let out = home.cli(&["doctor"], S(5));
    assert!(out.stdout.contains("no stale files"), "{}", out.stdout);
    assert!(out.stdout.trim_end().ends_with("ok"), "{}", out.stdout);

    dead_socket(&home.sock());
    std::fs::write(home.pidfile(), br#"{"pid":1,"version":"0","started_at":0}"#).unwrap();
    let out = home.cli(&["doctor"], S(5));
    // Stale files are fixed, not failures: exit 0, ends "ok".
    assert!(out.status.success(), "{}", out.stdout);
    assert_eq!(out.stdout.matches("fixed daemon: not running; removed stale").count(), 2, "{}", out.stdout);
    assert!(out.stdout.trim_end().ends_with("ok"), "{}", out.stdout);
    assert!(!home.sock().exists() && !home.pidfile().exists(), "stale files kept");

    let _d = home.start_daemon();
    let pid = home.pidfile_pid().unwrap();
    let out = home.cli(&["doctor"], S(5));
    assert!(out.stdout.contains("hello ok"), "{}", out.stdout);
    assert!(home.sock().exists() && home.pidfile_pid() == Some(pid), "doctor touched a live daemon");
    assert!(out.stdout.trim_end().ends_with("ok"), "{}", out.stdout);

    // Lock held but the socket is gone: a problem, and nothing is cleaned.
    std::fs::remove_file(home.sock()).unwrap();
    let out = home.cli(&["doctor"], S(5));
    assert!(out.stdout.contains("NOT responding"), "{}", out.stdout);
    assert!(out.stdout.lines().any(|l| l == "1 problem(s) found"), "{}", out.stdout);
    assert_eq!(out.status.code(), Some(1), "a failure exits 1");
    assert!(home.pidfile_pid() == Some(pid), "doctor cleaned under a live daemon");

    // The lock itself cannot be checked: reported as a problem.
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let home2 = Home::new("g13b");
        let lock = home2.path.join("manager.lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
        let out = home2.cli(&["doctor"], S(5));
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(out.stdout.contains("FAIL  daemon: lock check failed"), "{}", out.stdout);
        assert!(out.stdout.lines().any(|l| l == "1 problem(s) found"), "{}", out.stdout);
        assert_eq!(out.status.code(), Some(1));
    }
}

/// Kills: daemon.rs handle_output lookahead. Tiny max_bytes over 4-byte
/// characters still makes progress and never corrupts them.
#[test]
fn g12_tiny_max_bytes_over_wide_chars() {
    let home = Home::new("g12");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("printf 'a\\360\\237\\230\\200\\344\\270\\255b'"); // a😀中b
    c.wait_terminal(&id, S(3)).unwrap();
    for max in 1..=5u64 {
        let (text, cursor, _) = c.read_all_output(&id, max);
        assert_eq!(text, "a😀中b", "max_bytes={max}");
        assert_eq!(cursor, 9);
    }
    // One read with max_bytes 1 at the emoji returns the whole emoji.
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":1,"max_bytes":1}));
    assert_eq!((r["chunk"].as_str(), r["next_cursor"].as_u64()), (Some("😀"), Some(5)));
}

/// Kills: lifecycle.rs scan_tasks output-size recovery. Output written
/// before a manager crash stays readable, and correctly sized, on the
/// orphaned record the next daemon keeps.
#[test]
fn g6_crashed_task_output_survives_on_the_orphaned_record() {
    let home = Home::new("g6");
    let mut d1 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, _) = c.start("echo before-crash; sleep 300");
    assert!(poll_true(S(5), || c.task(&id).unwrap()["output_size"] == 13));
    drop(c);
    d1.kill().unwrap();
    d1.wait().unwrap();

    let _d2 = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let t = c.task(&id).unwrap();
    assert_eq!((t["status"].as_str(), t["output_size"].as_u64()), (Some("orphaned"), Some(13)), "{t}");
    let r = c.request_ok(json!({"type":"output","task_id":id,"cursor":0,"max_bytes":100}));
    assert_eq!(r["chunk"], "before-crash\n");
    assert_eq!(r["total_size"], 13);
    assert_eq!(r["status"], "orphaned");
}

/// Kills: runner.rs guardian poll (without its sleep the runner of a
/// finished task that left a child behind busy-loops until the child
/// exits). The guardian must be idle.
#[test]
fn g14_guardian_runner_does_not_spin() {
    let home = Home::new("g14");
    let _d = home.start_daemon();
    let mut c = home.connect();
    c.hello_ext("sess-a");
    let (id, runner) = c.start("sleep 300 >/dev/null 2>&1 & echo $!");
    let gc = wait_for_pids(&mut c, &id, 1)[0];
    assert_eq!(c.wait_terminal(&id, S(3)).unwrap()["status"], "completed");
    assert!(pid_running(runner), "the runner guards the leftover child");
    std::thread::sleep(Duration::from_millis(300));
    let before = cpu_ms(runner);
    std::thread::sleep(Duration::from_millis(1500));
    let used = cpu_ms(runner) - before;
    // gc is the background child's pid, not a process-group id.
    kill_pid(gc, libc::SIGKILL);
    assert!(used < 150, "guardian runner used {used}ms CPU in 1.5s");
}
