//! Whether a directory row's session process still runs on this host, judged
//! from the process itself. The registry `status` is advisory (the herdr
//! overlay can mark a live session offline), so it never decides liveness.

use super::str_field;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The session's process exists (and its start time matches the one
    /// recorded, when one was).
    Alive,
    /// The row is still listed but its process is gone or the pid was reused.
    Exited,
    /// This daemon cannot tell: no vouched pid, or another machine's row.
    Unknown,
}

impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Alive => "alive",
            Liveness::Exited => "exited",
            Liveness::Unknown => "unknown",
        }
    }
}

/// The local process a directory row speaks for, when this daemon can vouch
/// for it: `(pid, recorded start time or "")`.
///
/// - Another machine's row never names a local process, whatever pid it carries.
/// - A pi row's pid is vouched only when the row is owned by a manager
///   connection (`manager_port`): the daemon set it from the hello's `pi_pid`.
///   A client-socket registration copies the caller's session object, so its
///   pid (and any `_proc_start`) is self-declared.
/// - A Claude row comes from the native registry scan, which already checked
///   the pid against the registry file name and its recorded `procStart`.
/// - Codex, webhook and herdr placeholder (`all`) rows carry no process.
pub fn vouched_process(row: &Value, machine: &str, manager_port: bool) -> Option<(u32, String)> {
    let owner = str_field(row, "machine_id");
    if (!owner.is_empty() && owner != machine) || row["all"] == true {
        return None;
    }
    let start = match str_field(row, "harness") {
        // deferred: recording the pi process start time at manager registration
        // (port.rs, beside `v["pid"]`) | impact: once pi exits, a reused pid keeps its
        // recorded session `alive`; a stop still acts only on a pane herdr identifies as the
        // session, but after the pane is closed it reports `still_running` for good and the
        // record never becomes `exited` | trigger: before anything signals a pi pid, or when
        // stuck `still_running` records are seen
        "pi" if manager_port => "",
        "claude" => str_field(row, "_proc_start"),
        _ => return None,
    };
    Some((session_pid(row["pid"].as_u64()?)?, start.to_string()))
}

/// pid 0 and 1, and values the kernel would read as a process group, are
/// never a session's process.
fn session_pid(pid: u64) -> Option<u32> {
    (2..=i32::MAX as u64).contains(&pid).then_some(pid as u32)
}

/// Probe `pid` without signalling it. `recorded_start`, when not empty, must
/// still match the process's start time; a mismatch means the pid was reused.
// deferred: zombie detection | impact: an exited but unreaped process reads `alive`, so
// stop_session's grace wait reports `still_running` and the record stays `alive` until its
// parent (normally herdr's pane) reaps it | trigger: unreaped pi processes are seen after
// herdr closes their panes
#[cfg(unix)]
pub fn probe(pid: u32, recorded_start: &str) -> Liveness {
    if session_pid(u64::from(pid)).is_none() {
        return Liveness::Unknown;
    }
    if !crate::sys::pid_alive(pid) {
        return Liveness::Exited;
    }
    if recorded_start.is_empty() {
        return Liveness::Alive;
    }
    match super::native::process_start(pid) {
        start if start == recorded_start => Liveness::Alive,
        // Unreadable: gone in between, or it cannot be read at all.
        start if start.is_empty() && crate::sys::pid_alive(pid) => Liveness::Unknown,
        _ => Liveness::Exited,
    }
}
/// No start-time identity off Unix: a recorded start cannot be checked, so
/// such a pid is unknown rather than possibly a reused one.
#[cfg(not(unix))]
pub fn probe(pid: u32, recorded_start: &str) -> Liveness {
    if session_pid(u64::from(pid)).is_none() {
        Liveness::Unknown
    } else if !crate::sys::pid_alive(pid) {
        Liveness::Exited
    } else if recorded_start.is_empty() {
        Liveness::Alive
    } else {
        Liveness::Unknown
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    const HERE: &str = "machine-here";

    fn me() -> u32 {
        std::process::id()
    }

    /// A pid that certainly existed and is now gone: a child this test
    /// spawned and reaped itself.
    fn reaped_pid() -> u32 {
        let mut child = crate::sys::command_std("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_live_process_is_alive_with_and_without_a_recorded_start() {
        assert_eq!(probe(me(), ""), Liveness::Alive);
        let start = super::super::native::process_start(me());
        assert!(!start.is_empty(), "process start of self unreadable");
        assert_eq!(probe(me(), &start), Liveness::Alive);
    }

    #[test]
    fn a_reaped_process_is_exited() {
        let pid = reaped_pid();
        assert_eq!(probe(pid, ""), Liveness::Exited);
        assert_eq!(probe(pid, "whatever was recorded"), Liveness::Exited);
    }

    #[test]
    fn a_reused_pid_with_another_start_time_is_exited() {
        assert_eq!(
            probe(me(), "a start time this process never had"),
            Liveness::Exited
        );
    }

    #[test]
    fn pid_zero_one_and_group_values_are_never_probed_as_a_session() {
        for pid in [0, 1, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(probe(pid, ""), Liveness::Unknown, "{pid}");
        }
        for pid in [0u64, 1, i32::MAX as u64 + 1, u64::from(u32::MAX) + 7] {
            assert_eq!(session_pid(pid), None, "{pid}");
        }
        assert_eq!(session_pid(u64::from(me())), Some(me()));
    }

    #[test]
    fn a_manager_pi_row_vouches_its_pid_but_not_a_self_declared_start() {
        let row = json!({"id":"pi/a","harness":"pi","pid":me(),"_proc_start":"forged"});
        assert_eq!(
            vouched_process(&row, HERE, true),
            Some((me(), String::new()))
        );
        // The advisory status never decides: an offline-marked row whose
        // process runs is alive.
        let offline = json!({"id":"pi/a","harness":"pi","pid":me(),"status":"offline"});
        let (pid, start) = vouched_process(&offline, HERE, true).unwrap();
        assert_eq!(probe(pid, &start), Liveness::Alive);
    }

    #[test]
    fn a_client_socket_pi_row_is_never_vouched() {
        let row = json!({"id":"pi/a","harness":"pi","pid":me()});
        assert_eq!(vouched_process(&row, HERE, false), None);
    }

    #[test]
    fn another_machines_row_is_never_checked_against_a_local_pid() {
        for harness in ["pi", "claude"] {
            let row = json!({"id":format!("{harness}/r"),"harness":harness,"pid":me(),"machine_id":"machine-elsewhere","_proc_start":""});
            assert_eq!(vouched_process(&row, HERE, true), None, "{row}");
        }
        // A row naming this machine (or none) is local.
        let row = json!({"id":"pi/a","harness":"pi","pid":me(),"machine_id":HERE});
        assert_eq!(
            vouched_process(&row, HERE, true),
            Some((me(), String::new()))
        );
    }

    #[test]
    fn a_claude_row_carries_its_recorded_start() {
        let row = json!({"id":"claude/c","harness":"claude","pid":me(),"_proc_start":"recorded"});
        assert_eq!(
            vouched_process(&row, HERE, false),
            Some((me(), "recorded".to_string()))
        );
        let (pid, start) = vouched_process(&row, HERE, false).unwrap();
        assert_eq!(probe(pid, &start), Liveness::Exited);
    }

    #[test]
    fn rows_without_a_session_process_are_unknown() {
        for row in [
            json!({"id":"codex/t","harness":"codex","pid":me()}),
            json!({"id":"webhook/w","harness":"webhook","pid":me()}),
            json!({"id":"p1","harness":"claude","pid":me(),"all":true}),
            json!({"id":"pi/a","harness":"pi"}),
            json!({"id":"pi/a","harness":"pi","pid":"123"}),
            json!({"id":"pi/a","harness":"pi","pid":0}),
            json!({"id":"pi/a","harness":"pi","pid":1}),
            json!({"id":"claude/c","harness":"claude","pid":1,"_proc_start":"x"}),
        ] {
            assert_eq!(vouched_process(&row, HERE, true), None, "{row}");
        }
    }
}
