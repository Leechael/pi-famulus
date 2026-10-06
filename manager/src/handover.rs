//! In-place upgrade (design doc §3.2): replace the running daemon with the
//! binary now at its executable path, without disturbing any task.
//!
//! The daemon `exec()`s the new binary. The pid stays the same, so every
//! task runner (`pi-famulus __run`) is still its child and `waitpid` still
//! works, and descriptors without close-on-exec survive:
//!
//! - the listening socket (never re-bound: a client connecting during the
//!   upgrade waits in the backlog instead of failing);
//! - the daemon lock (`manager.lock`), so no other daemon can claim it;
//! - both ends of the lifeline. Every runner holds the read end; if the
//!   write end closed, every task would be torn down (§3.2);
//! - each task's stdout / stderr / status pipe read ends.
//!
//! Everything else goes into `<home>/handover.json`. Sequence:
//!
//! 1. Preflight: run `<new binary> __handover-check`. It must answer with
//!    this handover format. A missing, truncated or incompatible binary
//!    stops the upgrade here, with nothing touched.
//! 2. Quiesce: park every task's pumps and exit watch (their state lands in
//!    the task entries; a pump stops only between two reads), let the
//!    output fanout push what it holds, then close every client connection
//!    and let the writers flush. Requests still in flight get no answer:
//!    clients resend them after reconnecting.
//! 3. Write `handover.json`, clear close-on-exec on the inherited
//!    descriptors, and exec.
//! 4. If exec fails, nothing is lost: the descriptors go back to
//!    close-on-exec, the tasks resume, and the failure is recorded
//!    (`status.last_upgrade`).
//!
//! The new image (`daemon --handover <file>`) restores from the file. If it
//! cannot, it exits: the lifeline then closes and every task is cleaned up,
//! which is the same thing a crash does (§3.2, no crash recovery).

use crate::daemon::{self, Shared};
use crate::proto::*;
use crate::registry::{ExitPhase, TaskEntry};
use crate::task;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Version of `handover.json` and the fd contract. The new binary must
/// speak it (`__handover-check`).
pub const FORMAT: u32 = 1;
/// Hidden subcommand answering the preflight.
pub const CHECK_ARG: &str = "__handover-check";
const CHECK_PREFIX: &str = "pi-famulus-handover";

/// How long the quiesce may take before the upgrade is abandoned.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long connection writers get to flush before exec.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// This daemon's executable path. Linux reports a replaced binary as
/// "<path> (deleted)"; the upgrade wants the file now at `<path>`.
pub fn exe_path() -> std::io::Result<PathBuf> {
    let p = std::env::current_exe()?;
    let s = p.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(orig) => PathBuf::from(orig),
        None => p,
    })
}

pub fn check_line() -> String {
    format!("{CHECK_PREFIX} {FORMAT} {}", crate::VERSION)
}

pub fn file_path(home: &Path) -> PathBuf {
    home.join("handover.json")
}

#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    pub generation: u32,
    pub from_version: String,
    pub to_version: String,
    pub trigger: String,
    pub started_at_ms: u64,
    pub foreground: bool,
    pub listener_fd: RawFd,
    pub lock_fd: RawFd,
    pub lifeline_read_fd: RawFd,
    pub lifeline_write_fd: RawFd,
    pub sessions: Vec<SessionSnap>,
    pub tasks: Vec<TaskSnap>,
    /// Recent `start` request keys → task id (retried starts are answered
    /// with the task they already started).
    #[serde(default)]
    pub start_keys: Vec<(String, String)>,
    /// The daemon clock's `now_ms` (kill-grace deadlines are in its units;
    /// the manual test clock continues from here).
    #[serde(default)]
    pub clock_now_ms: u64,
    /// Machine-wide agent permit keys survive older in-place handovers.
    #[serde(default)]
    pub agent_permits: Vec<(String, String)>,
    /// Preserve protocol-5 kind and cancel ownership across newer handovers.
    #[serde(default)]
    pub agent_permit_details: Vec<(String, String, String, String)>,
}

#[derive(Serialize, Deserialize)]
pub struct SessionSnap {
    pub session_id: String,
    pub pi_pid: u32,
    pub cwd: Option<String>,
    pub extension_version: Option<String>,
    pub protocol: Option<u32>,
    pub connected_at: u64,
    pub last_seen: u64,
}

/// A task that still has something alive: a runner, pipes, or a leftover
/// process group. Finished tasks reload from disk.
#[derive(Serialize, Deserialize)]
pub struct TaskSnap {
    pub record: TaskRecord,
    pub kill_requested: bool,
    pub kill_reason: Option<String>,
    pub timeout_deadline_ms: Option<u64>,
    pub timed_out: bool,
    pub kill_grace_until_ms: Option<u64>,
    pub exit_phase: ExitPhase,
    pub group_lingering: bool,
    /// The runner, while it is still ours to reap.
    pub runner_pid: Option<u32>,
    pub status_fd: Option<RawFd>,
    pub status_partial: Vec<u8>,
    pub stdout_fd: Option<RawFd>,
    pub stderr_fd: Option<RawFd>,
    pub delivered_cursor: u64,
    /// Sessions that watched this task; they are subscribed again (and sent
    /// what they missed) when they reconnect.
    pub watch_sessions: Vec<(String, u64)>,
}

impl TaskSnap {
    fn fds(&self) -> impl Iterator<Item = RawFd> {
        [self.status_fd, self.stdout_fd, self.stderr_fd].into_iter().flatten()
    }
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// Ask the binary at `exe` whether it can take over. Returns its version.
/// The first run of a freshly installed binary can be slow (macOS assesses
/// its signature), hence the generous timeout; nothing waits on it.
pub async fn preflight(exe: &Path) -> Result<String, String> {
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(exe)
            .arg(CHECK_ARG)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| format!("{} did not answer the handover check within 20s", exe.display()))?
    .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} failed the handover check ({}): not an upgrade target",
            exe.display(),
            out.status
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    match (it.next(), it.next(), it.next()) {
        (Some(CHECK_PREFIX), Some(fmt), Some(version)) if fmt == FORMAT.to_string() => Ok(version.to_string()),
        (Some(CHECK_PREFIX), Some(fmt), _) => Err(format!(
            "{} speaks handover format {fmt}, this daemon {FORMAT}: stop the daemon to switch",
            exe.display()
        )),
        _ => Err(format!("{} gave no handover answer", exe.display())),
    }
}

// ---------------------------------------------------------------------------
// Quiesce + exec (old image)
// ---------------------------------------------------------------------------

/// Ask for an upgrade: run the preflight in the background (the daemon keeps
/// serving meanwhile) and, if the new binary can take over, have the accept
/// loop perform it. False when one is already under way.
pub fn request(state: &Shared, trigger: &str) -> bool {
    {
        let mut st = state.lock().unwrap();
        if st.shutdown || st.upgrading || st.upgrade_pending {
            return false;
        }
        st.upgrade_pending = true;
    }
    let state = state.clone();
    let trigger = trigger.to_string();
    tokio::spawn(async move {
        let from = crate::VERSION.to_string();
        let checked = match exe_path() {
            Ok(exe) => preflight(&exe).await.map(|v| (exe, v)),
            Err(e) => Err(format!("current_exe: {e}")),
        };
        match checked {
            Ok((exe, to)) => {
                let mut st = state.lock().unwrap();
                st.upgrade_ready = Some(Ready { exe, to_version: to, trigger });
                st.upgrade_notify.notify_one();
            }
            Err(e) => {
                state.lock().unwrap().upgrade_pending = false;
                fail(&state, &from, None, &trigger, e);
            }
        }
    });
    true
}

/// A preflighted upgrade, waiting for the accept loop.
pub struct Ready {
    pub exe: PathBuf,
    pub to_version: String,
    pub trigger: String,
}

/// Perform a preflighted upgrade. Returns only when it did not happen (the
/// error); on success the process image is replaced.
pub async fn perform(state: &Shared, listener_fd: RawFd, lock_fd: RawFd, ready: Ready) -> String {
    let Ready { exe, to_version, trigger } = ready;
    let trigger = trigger.as_str();
    let (home, from_version) = {
        let mut st = state.lock().unwrap();
        st.upgrade_pending = false;
        (st.home.clone(), crate::VERSION.to_string())
    };
    crate::lifecycle::log_line(&home, &format!("upgrade: {from_version} -> {to_version} ({trigger}); quiescing"));

    // 1. Park: no request is served and no task is read from now on.
    {
        let mut st = state.lock().unwrap();
        st.upgrading = true;
        // send_replace: `send` would drop the value when nobody listens.
        st.park_tx.send_replace(true);
    }
    if let Err(e) = tokio::time::timeout(QUIESCE_TIMEOUT, park_all(state)).await {
        let msg = format!("quiesce timed out ({e}); resumed");
        resume(state);
        return fail(state, &from_version, Some(to_version), trigger, msg);
    }
    // 2. Close every client connection and let the writers flush what the
    //    fanout pushed, so `delivered_cursor` is what clients really got.
    let (carried, writer_tasks) = daemon::close_all_connections(state, "upgrade");
    let writers = state.lock().unwrap().writers.clone();
    let flushed = |d: Duration| {
        let writers = writers.clone();
        async move {
            tokio::time::timeout(d, async {
                while writers.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok()
        }
    };
    if !flushed(FLUSH_TIMEOUT).await {
        // A client that stopped reading: stop its writer, so what it was
        // written up to is final (the rest is replayed on reconnect).
        for w in &writer_tasks {
            w.abort();
        }
        flushed(Duration::from_millis(500)).await;
    }
    daemon::carry_watches(state, carried);

    // 3. Snapshot, hand the descriptors over, exec.
    let snap = match snapshot(state, listener_fd, lock_fd, &from_version, &to_version, trigger) {
        Ok(s) => s,
        Err(e) => {
            resume(state);
            return fail(state, &from_version, Some(to_version), trigger, e);
        }
    };
    let mut fds: Vec<RawFd> = vec![snap.listener_fd, snap.lock_fd, snap.lifeline_read_fd, snap.lifeline_write_fd];
    for t in &snap.tasks {
        fds.extend(t.fds());
    }
    let path = file_path(&home);
    let written = serde_json::to_vec(&snap)
        .map_err(|e| e.to_string())
        .and_then(|b| write_atomic(&path, &b).map_err(|e| e.to_string()));
    if let Err(e) = written {
        resume(state);
        return fail(state, &from_version, Some(to_version), trigger, format!("write {}: {e}", path.display()));
    }
    for fd in &fds {
        let _ = crate::sys::clear_cloexec(*fd);
    }
    crate::events::emit(
        &home,
        None,
        "daemon.upgrade",
        None,
        serde_json::json!({ "from": from_version, "to": to_version, "trigger": trigger, "tasks": snap.tasks.len() }),
    );
    // Test hook (debug builds): exec something that does not exist, to
    // exercise the path where exec fails after the quiesce.
    let exec_target = match std::env::var("PI_FAMULUS_TEST_EXEC_PATH") {
        Ok(p) if cfg!(debug_assertions) => PathBuf::from(p),
        _ => exe.clone(),
    };
    let mut cmd = std::process::Command::new(&exec_target);
    cmd.arg("--home").arg(&home).arg("daemon").arg("--handover").arg(&path);
    if snap.foreground {
        cmd.arg("--foreground");
    }
    let err = std::os::unix::process::CommandExt::exec(&mut cmd);

    // exec returned: the old image keeps running.
    for fd in &fds {
        let _ = crate::sys::set_cloexec(*fd);
    }
    let _ = std::fs::remove_file(&path);
    resume(state);
    fail(state, &from_version, Some(to_version), trigger, format!("exec {}: {err}", exec_target.display()))
}

fn fail(state: &Shared, from: &str, to: Option<String>, trigger: &str, error: String) -> String {
    let mut st = state.lock().unwrap();
    crate::lifecycle::log_line(&st.home, &format!("upgrade failed ({trigger}): {error}"));
    st.last_upgrade = Some(UpgradeInfo {
        at: now_ms(),
        ok: false,
        from_version: from.to_string(),
        to_version: to,
        error: Some(error.clone()),
        trigger: trigger.to_string(),
    });
    error
}

/// Wait for every task's pumps, fanout and exit watch to stop, collecting
/// the pumps' descriptors back into the entries.
async fn park_all(state: &Shared) {
    // In-flight requests are cancelled at their next await; wait for them.
    let inflight = state.lock().unwrap().inflight.clone();
    while inflight.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let jobs: Vec<(String, Option<task::Tee>, Option<tokio::task::JoinHandle<()>>, Option<tokio::task::JoinHandle<()>>)> = {
        let mut st = state.lock().unwrap();
        st.registry
            .tasks
            .iter_mut()
            .map(|(id, e)| (id.clone(), e.tee.take(), e.fanout.take(), e.exit_watch.take()))
            .collect()
    };
    for (id, tee, fanout, exit_watch) in jobs {
        if let Some(tee) = tee {
            let out = tee.stdout.await.ok().flatten();
            let err = tee.stderr.await.ok().flatten();
            if let Some(e) = state.lock().unwrap().registry.tasks.get_mut(&id) {
                e.stdout_fd = out;
                e.stderr_fd = err;
            }
        }
        if let Some(f) = fanout {
            let _ = f.await;
        }
        if let Some(w) = exit_watch {
            let _ = w.await;
        }
    }
}

/// Undo a quiesce that did not end in exec: tasks are read again, requests
/// served again.
fn resume(state: &Shared) {
    let ids: Vec<String> = {
        let mut st = state.lock().unwrap();
        st.park_tx.send_replace(false);
        st.upgrading = false;
        st.registry.tasks.keys().cloned().collect()
    };
    for id in ids {
        restart_task(state, &id);
    }
    daemon::maybe_arm_idle_timer(state);
}

/// Start whatever a task still needs: its tee (if it has pipes) and its exit
/// watch (if its runner is still to be reaped).
pub fn restart_task(state: &Shared, id: &str) {
    let (io, watch) = {
        let st = state.lock().unwrap();
        match st.registry.tasks.get(id) {
            Some(e) => (
                e.stdout_fd.is_some() || e.stderr_fd.is_some(),
                e.child.is_some() && e.exit_phase != ExitPhase::Done,
            ),
            None => return,
        }
    };
    if io {
        daemon::start_task_io(state, id);
    }
    if watch {
        daemon::spawn_exit_watch(state, id);
    }
}

fn snapshot(
    state: &Shared,
    listener_fd: RawFd,
    lock_fd: RawFd,
    from: &str,
    to: &str,
    trigger: &str,
) -> Result<Snapshot, String> {
    let lifeline = task::lifeline().map_err(|e| format!("lifeline: {e}"))?;
    let st = state.lock().unwrap();
    let sessions = st
        .sessions
        .iter()
        .map(|(sid, s)| SessionSnap {
            session_id: sid.clone(),
            pi_pid: s.pi_pid,
            cwd: s.cwd.clone(),
            extension_version: s.extension_version.clone(),
            protocol: s.protocol,
            connected_at: s.connected_at,
            last_seen: s.last_seen,
        })
        .collect();
    let mut tasks = Vec::new();
    for e in st.registry.tasks.values() {
        let has_runner = e.child.is_some() && e.exit_phase != ExitPhase::Done;
        let has_pipes = e.stdout_fd.is_some() || e.stderr_fd.is_some();
        if !(has_runner || has_pipes || e.group_lingering || e.record.status == TaskStatus::Running) {
            continue;
        }
        let runner_pid = match &e.child {
            Some(task::RunnerProc::Child(c)) => c.id(),
            Some(task::RunnerProc::Pid(p)) => Some(*p),
            None => None,
        };
        let mut record = e.record.clone();
        record.output_size = e.output.lock().unwrap().total_size;
        tasks.push(TaskSnap {
            record,
            kill_requested: e.kill_requested,
            kill_reason: e.kill_reason.clone(),
            timeout_deadline_ms: e.timeout_deadline_ms,
            timed_out: e.timed_out,
            kill_grace_until_ms: e.kill_grace_until_ms,
            exit_phase: e.exit_phase,
            group_lingering: e.group_lingering,
            runner_pid: if has_runner { runner_pid } else { None },
            status_fd: e.status_rx.as_ref().map(|r| r.as_raw_fd()),
            status_partial: e.status_partial.clone(),
            stdout_fd: e.stdout_fd.as_ref().map(|f| f.as_raw_fd()),
            stderr_fd: e.stderr_fd.as_ref().map(|f| f.as_raw_fd()),
            delivered_cursor: e.delivered_cursor,
            watch_sessions: e.watch_sessions.clone(),
        });
    }
    Ok(Snapshot {
        format: FORMAT,
        generation: st.generation + 1,
        from_version: from.to_string(),
        to_version: to.to_string(),
        trigger: trigger.to_string(),
        started_at_ms: st.started_at_ms,
        foreground: st.foreground,
        listener_fd,
        lock_fd,
        lifeline_read_fd: lifeline.read.as_raw_fd(),
        lifeline_write_fd: lifeline.write.as_raw_fd(),
        sessions,
        tasks,
        start_keys: st.start_keys.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        clock_now_ms: st.clock.now_ms(),
        agent_permits: st.agent_permits.keys().cloned().collect(),
        agent_permit_details: st
            .agent_permits
            .iter()
            .map(|((session_id, child_id), permit)| {
                (
                    session_id.clone(),
                    child_id.clone(),
                    permit.work_kind.clone(),
                    permit.request_id.clone(),
                )
            })
            .collect(),
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Restore (new image)
// ---------------------------------------------------------------------------

pub struct Restored {
    pub snap: Snapshot,
    pub listener: tokio::net::UnixListener,
    pub lock: crate::lifecycle::DaemonLockGuard,
    pub entries: Vec<TaskEntry>,
}

/// Read the handover file and take over the inherited descriptors. Any
/// error means the daemon must exit (the lifeline then cleans up).
pub fn restore(path: &Path) -> Result<Restored, String> {
    if cfg!(debug_assertions) && std::env::var("PI_FAMULUS_TEST_FAIL_RESTORE").as_deref() == Ok("1") {
        return Err("PI_FAMULUS_TEST_FAIL_RESTORE=1".into());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let _ = std::fs::remove_file(path);
    let snap: Snapshot = serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if snap.format != FORMAT {
        return Err(format!("handover format {} (this binary speaks {FORMAT})", snap.format));
    }
    // SAFETY (for every from_raw_fd below): the numbers were inherited from
    // the previous image of this process, which owned them and passed them
    // on through the handover file; nothing else in this image holds them.
    let own = |fd: RawFd| -> Result<OwnedFd, String> {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!("inherited fd {fd} is not open"));
        }
        crate::sys::set_cloexec(fd).map_err(|e| format!("fd {fd}: {e}"))?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let lock = crate::lifecycle::adopt_daemon_lock(own(snap.lock_fd)?).map_err(|e| format!("daemon lock: {e}"))?;
    task::adopt_lifeline(own(snap.lifeline_read_fd)?, own(snap.lifeline_write_fd)?)
        .map_err(|e| format!("lifeline: {e}"))?;
    let listener = {
        let std_l = std::os::unix::net::UnixListener::from(own(snap.listener_fd)?);
        std_l.set_nonblocking(true).map_err(|e| format!("listener: {e}"))?;
        tokio::net::UnixListener::from_std(std_l).map_err(|e| format!("listener: {e}"))?
    };
    let mut entries = Vec::new();
    for t in &snap.tasks {
        let output = {
            let path = Path::new(&t.record.output_path);
            let (file, _) = task::open_output_files(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let total = file.metadata().map(|m| m.len()).unwrap_or(t.record.output_size);
            let mut out = task::OutputState::new(Some(file), total);
            let keep = task::RING_CAPACITY as u64;
            if let Ok((tail, _)) = task::read_file_range(path, total.saturating_sub(keep), task::RING_CAPACITY) {
                out.ring.push(&tail);
            }
            Arc::new(Mutex::new(out))
        };
        let mut record = t.record.clone();
        record.output_size = output.lock().unwrap().total_size;
        let mut e = TaskEntry::bare(record, output);
        e.kill_requested = t.kill_requested;
        e.kill_reason = t.kill_reason.clone();
        e.timeout_deadline_ms = t.timeout_deadline_ms;
        e.timed_out = t.timed_out;
        e.kill_grace_until_ms = t.kill_grace_until_ms;
        e.exit_phase = t.exit_phase;
        e.group_lingering = t.group_lingering;
        e.child = t.runner_pid.map(task::RunnerProc::Pid);
        if e.child.is_none() && e.exit_phase != ExitPhase::Done {
            e.exit_phase = ExitPhase::Done;
        }
        e.status_rx = match t.status_fd {
            Some(fd) => Some(
                tokio::net::unix::pipe::Receiver::from_owned_fd(own(fd)?).map_err(|e| format!("status fd {fd}: {e}"))?,
            ),
            None => None,
        };
        e.status_partial = t.status_partial.clone();
        e.stdout_fd = t.stdout_fd.map(own).transpose()?;
        e.stderr_fd = t.stderr_fd.map(own).transpose()?;
        e.delivered_cursor = t.delivered_cursor;
        e.watch_sessions = t.watch_sessions.clone();
        entries.push(e);
    }
    Ok(Restored { snap, listener, lock, entries })
}

/// Task ids that come from the handover (not to be reloaded from disk).
pub fn live_ids(r: &Restored) -> HashSet<String> {
    r.snap.tasks.iter().map(|t| t.record.task_id.clone()).collect()
}

/// Re-arm what the old image had pending for a restored task: the rest of a
/// stop's kill grace, and the poll of a leftover group no runner guards.
pub fn rearm_timers(state: &Shared, id: &str) {
    let (grace, lingering_unguarded, pid) = {
        let st = state.lock().unwrap();
        let now = st.clock.now_ms();
        let Some(e) = st.registry.tasks.get(id) else { return };
        (
            e.kill_grace_until_ms.map(|d| d.saturating_sub(now)),
            e.group_lingering && e.exit_phase == ExitPhase::Done,
            e.record.pid,
        )
    };
    if let Some(left) = grace {
        daemon::rearm_kill_reaper(state, id, pid, Duration::from_millis(left));
    }
    if lingering_unguarded {
        daemon::spawn_group_watcher(state, id, pid);
    }
}

