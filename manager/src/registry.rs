//! Task registry, session namespace, and TaskRecord disk persistence
//! (design doc §3.4). Records live at
//! `<home>/sessions/<session_id>/tasks/<task_id>.json` and are written
//! atomically (tmp file + rename).

use crate::proto::{
    ProtoError, TaskKind, TaskRecord, TaskStatus, E_FORBIDDEN, E_NOT_FOUND,
};
use crate::task::{OutputState, RunnerProc, StatusRx, TeeSource};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub fn tasks_dir(home: &Path, session_id: &str) -> PathBuf {
    home.join("sessions").join(session_id).join("tasks")
}

pub fn task_json_path(home: &Path, session_id: &str, task_id: &str) -> PathBuf {
    tasks_dir(home, session_id).join(format!("{task_id}.json"))
}

pub fn task_output_path(home: &Path, session_id: &str, task_id: &str) -> PathBuf {
    tasks_dir(home, session_id).join(format!("{task_id}.output"))
}

// ---------------------------------------------------------------------------
// Task entry (runtime state + persisted record)
// ---------------------------------------------------------------------------

/// Where a running task's exit watch stands. Kept in the entry so an
/// in-place upgrade can resume the watch where it left off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitPhase {
    /// Waiting for the runner's status line, its exit, or the timeout.
    AwaitReport,
    /// Finalized from the report; waiting for the runner to exit. With
    /// `group_lingering` it guards leftovers, otherwise it only needs reaping.
    AwaitRunnerExit,
    /// Nothing left to wait for.
    Done,
}

pub struct TaskEntry {
    pub record: TaskRecord,
    /// The task's runner (`pi-famulus __run`), taken by the exit watch at
    /// spawn time; None for records loaded from disk.
    pub child: Option<RunnerProc>,
    /// Read end of the runner's status pipe, taken with `child`.
    pub status_rx: Option<StatusRx>,
    pub output: Arc<Mutex<OutputState>>,
    /// Status broadcast for `wait` waiters; receives the terminal status once.
    pub status_tx: watch::Sender<TaskStatus>,
    /// Set by stop/timeout/shutdown so the exit path maps to `killed` (§3.4).
    pub kill_requested: bool,
    /// end_reason for a kill we initiated (stop/timeout/shutdown); the first
    /// reason wins. None + a natural exit = "exited".
    pub kill_reason: Option<String>,
    /// Connection ids subscribed to output events (§3.3 watch).
    pub watchers: HashSet<u64>,
    /// Hard kill ceiling as an absolute wall-clock time (ms since epoch), so
    /// an in-place upgrade carries it over unchanged.
    pub timeout_deadline_ms: Option<u64>,
    /// Where the exit watch stands.
    pub exit_phase: ExitPhase,
    /// The hard timeout fired and the group was killed for it.
    pub timed_out: bool,
    /// A stop's SIGKILL escalation is due at this time on the daemon's clock
    /// (`Clock::now_ms`: wall time, or virtual time on the test clock).
    pub kill_grace_until_ms: Option<u64>,
    /// stdout / stderr pipe read ends while no pump reads them (before the
    /// tee starts, and while parked). None once that pipe hit EOF.
    pub stdout_fd: Option<TeeSource>,
    pub stderr_fd: Option<TeeSource>,
    /// The running tee pumps and output fanout, while started.
    pub tee: Option<crate::task::Tee>,
    pub fanout: Option<tokio::task::JoinHandle<()>>,
    /// The running exit watch, while started.
    pub exit_watch: Option<tokio::task::JoinHandle<()>>,
    /// A status line the exit watch read only in part (kept across a park).
    pub status_partial: Vec<u8>,
    /// Output bytes the fanout has pushed to watchers, as a cursor (an
    /// incomplete trailing UTF-8 sequence it holds back is not counted).
    pub delivered_cursor: u64,
    /// Sessions that watched this task when an in-place upgrade closed
    /// their connections, with the cursor they had received up to. Resumed
    /// (and the gap sent) when the session reconnects.
    pub watch_sessions: Vec<(String, u64)>,
    /// The command exited but other members of its process group (children
    /// it backgrounded) are still alive, guarded by the runner. The group is
    /// still ours to kill on stop/shutdown (§3.2: background work must not
    /// outlive the manager); the runner's exit clears the flag.
    pub group_lingering: bool,
}

impl TaskEntry {
    /// The task's process group may still have members: the leader runs, or
    /// it exited leaving descendants behind.
    pub fn owns_live_group(&self) -> bool {
        self.record.status == TaskStatus::Running || self.group_lingering
    }

    /// Re-probe a leftover group now and clear `group_lingering` once it has
    /// emptied. The flag is otherwise refreshed only by the group poll, which
    /// can be a tick behind (or, on the manual test clock, never run).
    /// Returns whether the group still lingers.
    pub fn refresh_lingering(&mut self) -> bool {
        // Only members other than the runner count: a guardian that has not
        // yet noticed its group emptied leaves nothing to kill.
        if self.group_lingering && !crate::sys::group_has_others(self.record.pid) {
            self.group_lingering = false;
        }
        self.group_lingering
    }

    /// Mark a running task as killed by us, for `end_reason` (first reason
    /// wins: a stop followed by a shutdown stays "stopped:…").
    pub fn request_kill(&mut self, end_reason: &str) {
        if self.record.status != TaskStatus::Running {
            return;
        }
        self.kill_requested = true; // exit path maps this to `killed` (§3.4)
        if self.kill_reason.is_none() {
            self.kill_reason = Some(end_reason.to_string());
        }
    }

    /// An entry with no runner, pipes or timers attached.
    pub fn bare(record: TaskRecord, output: Arc<Mutex<OutputState>>) -> Self {
        let (status_tx, _) = watch::channel(record.status);
        TaskEntry {
            record,
            child: None,
            status_rx: None,
            output,
            status_tx,
            kill_requested: false,
            kill_reason: None,
            watchers: HashSet::new(),
            timeout_deadline_ms: None,
            exit_phase: ExitPhase::Done,
            timed_out: false,
            kill_grace_until_ms: None,
            stdout_fd: None,
            stderr_fd: None,
            tee: None,
            fanout: None,
            exit_watch: None,
            status_partial: Vec::new(),
            delivered_cursor: 0,
            watch_sessions: Vec::new(),
            group_lingering: false,
        }
    }

    /// A freshly spawned task: runner, status pipe, and the stdout / stderr
    /// descriptors its tee will read.
    pub fn new_running(
        record: TaskRecord,
        parts: crate::task::ProcessParts,
        output: Arc<Mutex<OutputState>>,
        timeout_ms: Option<u64>,
    ) -> Self {
        let mut e = TaskEntry::bare(record, output);
        e.child = Some(RunnerProc::Child(parts.child));
        e.status_rx = Some(parts.status);
        e.stdout_fd = Some(parts.stdout);
        e.stderr_fd = Some(parts.stderr);
        e.timeout_deadline_ms = timeout_ms.map(|ms| e.record.started_at + ms);
        e.exit_phase = ExitPhase::AwaitReport;
        e
    }

    /// A terminal record loaded from disk (kept for list/output visibility).
    pub fn terminal(record: TaskRecord) -> Self {
        let total = record.output_size;
        TaskEntry::bare(record, Arc::new(Mutex::new(OutputState::new(None, total))))
    }
}

/// §3.4 state machine: map an observed exit to the terminal status.
/// (None, None) = no status was observable (the runner's wait failed) ->
/// completed with exit_code null.
pub fn terminal_status(
    kill_requested: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> TaskStatus {
    if kill_requested {
        return TaskStatus::Killed;
    }
    match (exit_code, signal) {
        (Some(0), _) => TaskStatus::Completed,
        (Some(_), _) => TaskStatus::Failed,
        (None, Some(_)) => TaskStatus::Failed,
        (None, None) => TaskStatus::Completed,
    }
}

// ---------------------------------------------------------------------------
// Access control (session namespace, §3.3)
// ---------------------------------------------------------------------------

pub enum Access {
    Extension(String),
    Cli,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    pub tasks: HashMap<String, TaskEntry>,
    home: PathBuf,
}

impl Registry {
    pub fn new(home: PathBuf) -> Self {
        Registry {
            tasks: HashMap::new(),
            home,
        }
    }

    /// §3.3: `<kind prefix>_<8 hex>`, collision-checked against memory + disk.
    pub fn generate_task_id(&self, kind: TaskKind) -> String {
        loop {
            let mut b = [0u8; 4];
            crate::proto::random_bytes(&mut b);
            let id = format!("{}_{:08x}", kind.prefix(), u32::from_be_bytes(b));
            if self.tasks.contains_key(&id) {
                continue;
            }
            // Check every session dir for a stale record with the same id.
            let taken_on_disk = fs::read_dir(self.home.join("sessions"))
                .map(|dirs| {
                    dirs.flatten().any(|d| {
                        task_json_path(&self.home, &d.file_name().to_string_lossy(), &id).exists()
                    })
                })
                .unwrap_or(false);
            if !taken_on_disk {
                return id;
            }
        }
    }

    /// Session-scoped lookup: extension connections may only touch their own
    /// session's tasks (cross-session access -> E_FORBIDDEN); cli is admin.
    pub fn visible(&self, task_id: &str, access: &Access) -> Result<&TaskEntry, ProtoError> {
        match self.tasks.get(task_id) {
            None => Err(ProtoError::new(
                E_NOT_FOUND,
                format!("no such task: {task_id}"),
            )),
            Some(e) => match access {
                Access::Extension(sid) if e.record.session_id != *sid => Err(ProtoError::new(
                    E_FORBIDDEN,
                    "task belongs to another session",
                )),
                _ => Ok(e),
            },
        }
    }

    pub fn visible_mut(
        &mut self,
        task_id: &str,
        access: &Access,
    ) -> Result<&mut TaskEntry, ProtoError> {
        match self.tasks.get_mut(task_id) {
            None => Err(ProtoError::new(
                E_NOT_FOUND,
                format!("no such task: {task_id}"),
            )),
            Some(e) => match access {
                Access::Extension(sid) if e.record.session_id != *sid => Err(ProtoError::new(
                    E_FORBIDDEN,
                    "task belongs to another session",
                )),
                _ => Ok(e),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence (atomic tmp + rename)
// ---------------------------------------------------------------------------

pub fn persist_record(home: &Path, record: &TaskRecord) -> io::Result<()> {
    let dir = tasks_dir(home, &record.session_id);
    fs::create_dir_all(&dir)?;
    let final_path = task_json_path(home, &record.session_id, &record.task_id);
    let tmp_path = dir.join(format!("{}.json.tmp", record.task_id));
    let mut persisted = record.clone();
    persisted.live_cpu_user_ms = None;
    persisted.live_cpu_sys_ms = None;
    persisted.live_cpu_percent = None;
    persisted.live_cpu_sampled_at = None;
    persisted.live_cpu_stale = false;
    fs::write(&tmp_path, serde_json::to_vec(&persisted).map_err(io::Error::other)?)?;
    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Load every persisted TaskRecord (any session). Corrupt files are skipped.
pub fn load_all_records(home: &Path) -> Vec<TaskRecord> {
    let mut out = Vec::new();
    let sessions = match fs::read_dir(home.join("sessions")) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for session_entry in sessions.flatten() {
        let tasks = match fs::read_dir(session_entry.path().join("tasks")) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for task_file in tasks.flatten() {
            let path = task_file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(rec) = serde_json::from_slice::<TaskRecord>(&bytes) {
                    out.push(rec);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-reg-test-{tag}-{}-{}",
            std::process::id(),
            crate::proto::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_record(home: &Path, session: &str, task_id: &str) -> TaskRecord {
        TaskRecord {
            task_id: task_id.into(),
            session_id: session.into(),
            kind: TaskKind::Shell,
            command: "echo hi".into(),
            cwd: "/tmp".into(),
            pid: 4242,
            status: TaskStatus::Running,
            exit_code: None,
            signal: None,
            started_at: 1726000000000,
            ended_at: None,
            output_path: task_output_path(home, session, task_id)
                .to_string_lossy()
                .into_owned(),
            output_size: 0,
            origin: None,
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            max_rss_kb: None,
            live_cpu_user_ms: None,
            live_cpu_sys_ms: None,
            live_cpu_percent: None,
            live_cpu_sampled_at: None,
            live_cpu_stale: false,
        }
    }

    #[test]
    fn task_id_format() {
        let home = temp_home("idfmt");
        let reg = Registry::new(home.clone());
        let id = reg.generate_task_id(TaskKind::Shell);
        assert!(id.starts_with("sh_"));
        assert_eq!(id.len(), 3 + 8);
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
        let id2 = reg.generate_task_id(TaskKind::Monitor);
        assert!(id2.starts_with("mon_"));
        assert_ne!(id, id2);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn persist_and_load_roundtrip() {
        let home = temp_home("persist");
        let mut rec = sample_record(&home, "sess-a", "sh_deadbeef");
        rec.live_cpu_user_ms = Some(250);
        rec.live_cpu_sys_ms = Some(50);
        rec.live_cpu_percent = Some(12.5);
        rec.live_cpu_sampled_at = Some(1726000000500);
        rec.live_cpu_stale = true;
        persist_record(&home, &rec).unwrap();
        let persisted = load_all_records(&home);
        assert_eq!(persisted[0].live_cpu_user_ms, None);
        assert_eq!(persisted[0].live_cpu_sys_ms, None);
        assert_eq!(persisted[0].live_cpu_percent, None);
        assert_eq!(persisted[0].live_cpu_sampled_at, None);
        assert!(!persisted[0].live_cpu_stale);
        // Atomic write leaves no tmp files behind.
        let dir = tasks_dir(&home, "sess-a");
        assert!(task_json_path(&home, "sess-a", "sh_deadbeef").exists());
        assert!(!dir.join("sh_deadbeef.json.tmp").exists());

        rec.status = TaskStatus::Completed;
        rec.exit_code = Some(0);
        rec.ended_at = Some(1726000001000);
        rec.output_size = 12;
        persist_record(&home, &rec).unwrap();

        let loaded = load_all_records(&home);
        assert_eq!(loaded.len(), 1);
        let r = &loaded[0];
        assert_eq!(r.task_id, "sh_deadbeef");
        assert_eq!(r.session_id, "sess-a");
        assert_eq!(r.status, TaskStatus::Completed);
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.ended_at, Some(1726000001000));
        assert_eq!(r.output_size, 12);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn session_visibility_rules() {
        let home = temp_home("acl");
        let mut reg = Registry::new(home.clone());
        let rec = sample_record(&home, "sess-a", "sh_00000001");
        reg.tasks.insert(
            "sh_00000001".into(),
            TaskEntry::bare(rec, Arc::new(Mutex::new(OutputState::new(None, 0)))),
        );
        let own = Access::Extension("sess-a".into());
        let other = Access::Extension("sess-b".into());
        let cli = Access::Cli;
        assert!(reg.visible("sh_00000001", &own).is_ok());
        let err = reg.visible("sh_00000001", &other).err().unwrap();
        assert_eq!(err.code, E_FORBIDDEN);
        assert!(reg.visible("sh_00000001", &cli).is_ok());
        let err = reg.visible("sh_00000099", &cli).err().unwrap();
        assert_eq!(err.code, E_NOT_FOUND);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn state_machine_mapping() {
        // §3.4: running -> completed (exit 0) / failed (exit!=0 or signal) /
        // killed (stop) / orphaned (after a manager crash, set by lifecycle).
        assert_eq!(
            terminal_status(false, Some(0), None),
            TaskStatus::Completed
        );
        assert_eq!(terminal_status(false, Some(1), None), TaskStatus::Failed);
        assert_eq!(
            terminal_status(false, None, Some(15)),
            TaskStatus::Failed
        );
        assert_eq!(
            terminal_status(true, Some(0), None),
            TaskStatus::Killed
        );
        assert_eq!(
            terminal_status(true, None, Some(9)),
            TaskStatus::Killed
        );
        // No observable status -> completed.
        assert_eq!(terminal_status(false, None, None), TaskStatus::Completed);
    }
}
