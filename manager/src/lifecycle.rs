//! Singleton & startup lifecycle (design doc §3.1, §3.2): base dir resolution,
//! pid claim, spawn lock, zombie cleanup, and the startup scan that marks a
//! crashed daemon's records orphaned.

use crate::proto::{now_ms, TaskStatus};
use crate::registry::{self, Registry, TaskEntry};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(debug_assertions)]
use std::io::Read;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Base directory & well-known paths (§3.1)
// ---------------------------------------------------------------------------

/// Priority: --home flag > PI_FAMULUS_HOME env > ~/.pi/agent/pi-famulus.
pub fn resolve_home(flag: Option<&Path>) -> PathBuf {
    if let Some(p) = flag {
        return p.to_path_buf();
    }
    if let Some(env) = std::env::var_os("PI_FAMULUS_HOME") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".pi").join("agent").join("pi-famulus")
}

pub fn socket_path(home: &Path) -> PathBuf {
    home.join("manager.sock")
}

pub fn pid_path(home: &Path) -> PathBuf {
    home.join("manager.pid")
}

pub fn lock_path(home: &Path) -> PathBuf {
    home.join("manager.spawn.lock")
}

/// Held by the running daemon for its whole lifetime (singleton identity).
pub fn daemon_lock_path(home: &Path) -> PathBuf {
    home.join("manager.lock")
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("manager.log")
}

/// Append one line to manager.log (best effort; never fails the caller).
pub fn log_line(home: &Path, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path(home)) {
        let _ = writeln!(f, "[{}] {msg}", now_ms());
    }
}

// ---------------------------------------------------------------------------
// Pid file & daemon claim (§3.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub version: String,
    pub started_at: u64,
}

pub fn read_pid_file(home: &Path) -> Option<PidFile> {
    let bytes = fs::read(pid_path(home)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_pid_file(home: &Path, pid: u32) -> io::Result<()> {
    let pf = PidFile {
        pid,
        version: crate::VERSION.to_string(),
        started_at: now_ms(),
    };
    let tmp = home.join("manager.pid.tmp");
    fs::write(&tmp, serde_json::to_vec(&pf).map_err(io::Error::other)?)?;
    fs::rename(&tmp, pid_path(home))?;
    Ok(())
}

/// Remove socket + pid files (stale after a dead manager, or at shutdown).
pub fn cleanup_stale_files(home: &Path) -> io::Result<()> {
    for p in [socket_path(home), pid_path(home)] {
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub enum Claim {
    /// This process is the daemon for `home`. Keep the guard alive for the
    /// daemon's whole lifetime; the OS releases the lock when it exits, even
    /// on SIGKILL.
    Acquired(DaemonLockGuard),
    /// The lock was unavailable on every attempt in the bounded claim window.
    /// `pid` comes from manager.pid and is informational; it may be stale or absent.
    AlreadyRunning { pid: Option<u32> },
}

/// Exclusive lock on manager.lock, held by the daemon for its lifetime.
pub struct DaemonLockGuard {
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

impl DaemonLockGuard {
    /// The lock file's descriptor. An in-place upgrade keeps it open across
    /// the exec, so the lock (it belongs to the open file) is never released.
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self._guard.as_raw_fd()
    }
}

/// Take over the daemon lock from a descriptor inherited across an in-place
/// upgrade. The lock is already ours; re-locking the same open file is a
/// no-op that must succeed.
pub fn adopt_daemon_lock(fd: std::os::fd::OwnedFd) -> io::Result<DaemonLockGuard> {
    let lock: &'static mut fd_lock::RwLock<std::fs::File> =
        Box::leak(Box::new(fd_lock::RwLock::new(std::fs::File::from(fd))));
    let guard = lock.try_write()?;
    Ok(DaemonLockGuard { _guard: guard })
}

fn open_daemon_lock(home: &Path) -> io::Result<fd_lock::RwLock<std::fs::File>> {
    // std opens files with O_CLOEXEC, so task processes never inherit the
    // lock: after a daemon crash, live tasks cannot keep a new daemon out.
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(daemon_lock_path(home))?;
    Ok(fd_lock::RwLock::new(f))
}

#[cfg(debug_assertions)]
fn note_daemon_lock_would_block(attempt: usize) -> io::Result<()> {
    if let Some(path) = std::env::var_os("PI_FAMULUS_TEST_DAEMON_LOCK_BLOCKED") {
        let mut marker = OpenOptions::new().create(true).append(true).open(path)?;
        marker.write_all(b"x")?;
    }
    if attempt == 0 {
        if let Some(path) = std::env::var_os("PI_FAMULUS_TEST_DAEMON_LOCK_BARRIER") {
            let mut barrier = std::os::unix::net::UnixStream::connect(path)?;
            barrier.write_all(b"x")?;
            let mut resume = [0_u8; 1];
            barrier.read_exact(&mut resume)?;
        }
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn note_daemon_lock_would_block(_attempt: usize) -> io::Result<()> {
    Ok(())
}

/// Try the leaked lock through a raw pointer so a failed attempt doesn't
/// borrow it for `'static` and prevent the next bounded attempt.
///
/// # Safety
/// `lock` must point to a `RwLock<File>` leaked for the process lifetime.
unsafe fn try_write_daemon_lock(
    lock: *mut fd_lock::RwLock<std::fs::File>,
) -> io::Result<DaemonLockGuard> {
    // SAFETY: the caller guarantees this object remains allocated for the
    // guard's 'static lifetime; a WouldBlock result returns no guard.
    let guard = (&mut *lock).try_write()?;
    Ok(DaemonLockGuard { _guard: guard })
}

const DAEMON_LOCK_CLAIM_ATTEMPTS: usize = 100;
const DAEMON_LOCK_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// §3.1 singleton claim. Identity is the lifetime lock on manager.lock, not
/// pid liveness: the pid in manager.pid can belong to an unrelated process
/// after a crash or reboot. Only the lock holder touches socket/pid files, so
/// whatever it finds is stale and is removed before binding. Brief lock
/// contention is retried regardless of the pid file; sustained contention
/// gets a bounded refusal without touching either endpoint.
pub fn claim_daemon(home: &Path) -> io::Result<Claim> {
    let lock = Box::into_raw(Box::new(open_daemon_lock(home)?));
    for attempt in 0..DAEMON_LOCK_CLAIM_ATTEMPTS {
        // SAFETY: `lock` came from Box::into_raw and is retained with the
        // lifetime guard on success; on WouldBlock, no guard is returned.
        match unsafe { try_write_daemon_lock(lock) } {
            Ok(guard) => {
                cleanup_stale_files(home)?;
                return Ok(Claim::Acquired(guard));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                note_daemon_lock_would_block(attempt)?;
                if attempt + 1 == DAEMON_LOCK_CLAIM_ATTEMPTS {
                    return Ok(Claim::AlreadyRunning {
                        pid: read_pid_file(home).map(|p| p.pid),
                    });
                }
                std::thread::sleep(DAEMON_LOCK_RETRY_INTERVAL);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the bounded daemon-lock loop always returns")
}

/// True when some process holds manager.lock (a daemon is alive). Takes and
/// releases the lock when it is free; touches no file.
pub fn lock_held(home: &Path) -> bool {
    match open_daemon_lock(home) {
        Ok(mut lock) => matches!(lock.try_write(), Err(e) if e.kind() == io::ErrorKind::WouldBlock),
        Err(_) => false,
    }
}

/// True when a daemon currently holds manager.lock. Takes and releases the
/// lock when it is free, so call it only from short-lived tools (doctor).
#[cfg(test)]
pub fn daemon_running(home: &Path) -> io::Result<bool> {
    Ok(clean_if_no_daemon_with(home, |_| Ok(()))?.is_none())
}

/// Doctor: when no daemon holds manager.lock, remove stale socket/pid files
/// while holding the lock (so a daemon cannot start mid-cleanup). Returns
/// `None` when a daemon is running (nothing touched), otherwise the list of
/// files that were removed.
pub fn clean_if_no_daemon(home: &Path) -> io::Result<Option<Vec<PathBuf>>> {
    clean_if_no_daemon_with(home, |home| {
        let mut removed = Vec::new();
        for p in [socket_path(home), pid_path(home)] {
            if p.exists() {
                fs::remove_file(&p)?;
                removed.push(p);
            }
        }
        Ok(removed)
    })
}

fn clean_if_no_daemon_with<T>(
    home: &Path,
    f: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<Option<T>> {
    let mut lock = open_daemon_lock(home)?;
    let res = match lock.try_write() {
        Ok(_guard) => Some(f(home)?),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
        Err(e) => return Err(e),
    };
    Ok(res)
}

// ---------------------------------------------------------------------------
// Spawn lock (client side, §3.1 step 2)
// ---------------------------------------------------------------------------

/// Guard for the fd-lock on manager.spawn.lock. The underlying lock object is
/// leaked so the guard can be returned; a CLI process holds it for seconds at
/// most and the OS releases the lock on exit anyway.
pub struct SpawnLockGuard {
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

/// Non-blocking trylock. Ok(None) = someone else is spawning right now.
pub fn try_acquire_spawn_lock(home: &Path) -> io::Result<Option<SpawnLockGuard>> {
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_path(home))?;
    let lock: &'static mut fd_lock::RwLock<std::fs::File> =
        Box::leak(Box::new(fd_lock::RwLock::new(f)));
    match lock.try_write() {
        Ok(guard) => Ok(Some(SpawnLockGuard { _guard: guard })),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Startup scan (§3.4): no crash recovery
// ---------------------------------------------------------------------------

pub struct ScanResult {
    /// Records left "running" by a daemon that died without shutting down,
    /// now marked orphaned (end_reason manager-crash).
    pub orphaned: usize,
    /// Already-terminal records loaded for list/output visibility.
    pub loaded: usize,
}

/// Wait until process group `pgid` has no members at all, or `bound`
/// elapses. Probe-only: `kill(-pgid, 0)`-style enumeration, never a signal.
fn wait_group_gone(pgid: u32, bound: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < bound {
        match crate::sys::group_members(pgid) {
            Ok(pids) if pids.is_empty() => return,
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            // Cannot enumerate: never block startup on it.
            Err(_) => return,
        }
    }
}

/// Load every record. A record still "running" belonged to a daemon that
/// died without shutting down; its runners saw the lifeline break and took
/// their process groups down (§3.2), so there is nothing to re-adopt. The
/// record is marked orphaned. Nothing is signalled: the recorded pid may
/// already belong to an unrelated process.
pub fn scan_tasks(home: &Path, registry: &mut Registry) -> ScanResult {
    scan_tasks_except(home, registry, &std::collections::HashSet::new())
}

/// [`scan_tasks`], skipping the tasks in `live`: after an in-place upgrade
/// those are still running and come from the handover, not from disk.
pub fn scan_tasks_except(
    home: &Path,
    registry: &mut Registry,
    live: &std::collections::HashSet<String>,
) -> ScanResult {
    let mut result = ScanResult { orphaned: 0, loaded: 0 };
    for mut rec in registry::load_all_records(home) {
        if live.contains(&rec.task_id) {
            continue;
        }
        // The persisted output_size lags the output file: a running task's
        // is only written at exit. The file can no longer grow, so it is
        // the truth.
        if let Ok(m) = fs::metadata(&rec.output_path) {
            rec.output_size = rec.output_size.max(m.len());
        }
        if rec.status == TaskStatus::Running {
            // The runner is taking the task's process group down (lifeline
            // teardown, §3.2: SIGTERM, a 2 s grace, then SIGKILL until the
            // group is empty). Wait for it, bounded: the orphan exit wake
            // must not send the agent back into a command that is still
            // running. Probing only — the recorded pid may since belong to
            // an unrelated process, so nothing here ever signals it.
            wait_group_gone(rec.pid, std::time::Duration::from_millis(3500));
            rec.status = TaskStatus::Orphaned;
            rec.end_reason = Some(crate::proto::end_reason::MANAGER_CRASH.to_string());
            let now = now_ms();
            rec.ended_at = Some(now);
            let _ = registry::persist_record(home, &rec);
            crate::events::emit(
                home,
                Some(&rec.session_id),
                "task.exit",
                Some(&rec.task_id),
                serde_json::json!({
                    "exit_code": null,
                    "signal": null,
                    "end_reason": crate::proto::end_reason::MANAGER_CRASH,
                    "duration_ms": now.saturating_sub(rec.started_at),
                }),
            );
            registry.tasks.insert(rec.task_id.clone(), TaskEntry::terminal(rec));
            result.orphaned += 1;
        } else {
            registry.tasks.insert(rec.task_id.clone(), TaskEntry::terminal(rec));
            result.loaded += 1;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-lc-test-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn home_resolution_priority() {
        // flag wins over everything (§3.1).
        let flag = PathBuf::from("/tmp/pi-famulus-flag");
        assert_eq!(resolve_home(Some(&flag)), flag);
        // env beats the default.
        std::env::set_var("PI_FAMULUS_HOME", "/tmp/pi-famulus-env");
        assert_eq!(resolve_home(None), PathBuf::from("/tmp/pi-famulus-env"));
        std::env::remove_var("PI_FAMULUS_HOME");
        // default: ~/.pi/agent/pi-famulus
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert_eq!(resolve_home(None), home.join(".pi/agent/pi-famulus"));
    }

    #[test]
    fn claim_is_exclusive_and_ignores_pid_liveness() {
        let home = temp_home("claim");
        // A live but unrelated pid in manager.pid (pid reuse) does not block.
        write_pid_file(&home, std::process::id()).unwrap();
        fs::write(socket_path(&home), b"").unwrap();
        let first = match claim_daemon(&home).unwrap() {
            Claim::Acquired(g) => g,
            Claim::AlreadyRunning { .. } => panic!("a reused pid must not block the claim"),
        };
        // The lock holder removed the stale files.
        assert!(!pid_path(&home).exists());
        assert!(!socket_path(&home).exists());
        assert!(daemon_running(&home).unwrap());
        // A second claim while the first is held is refused and leaves the
        // owner's files alone.
        write_pid_file(&home, 4242).unwrap();
        match claim_daemon(&home).unwrap() {
            Claim::AlreadyRunning { pid } => assert_eq!(pid, Some(4242)),
            Claim::Acquired(_) => panic!("second claim must be refused"),
        }
        assert!(pid_path(&home).exists());
        drop(first);
        assert!(!daemon_running(&home).unwrap());
        match claim_daemon(&home).unwrap() {
            Claim::Acquired(_) => {}
            Claim::AlreadyRunning { .. } => panic!("a released lock must be claimable"),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// A record left "running" is marked orphaned (manager-crash) whether or
    /// not its pid is alive, and nothing is signalled: here the recorded pid
    /// is this very test process, which must survive the scan.
    #[test]
    fn scan_marks_leftover_running_tasks_orphaned_without_signalling() {
        let home = temp_home("scan");
        let rec = crate::proto::TaskRecord {
            task_id: "sh_0000000a".into(),
            session_id: "s1".into(),
            kind: crate::proto::TaskKind::Shell,
            command: "sleep 1".into(),
            cwd: "/tmp".into(),
            pid: std::process::id(), // alive, and not ours to signal
            status: TaskStatus::Running,
            exit_code: None,
            signal: None,
            started_at: now_ms(),
            ended_at: None,
            output_path: registry::task_output_path(&home, "s1", "sh_0000000a")
                .to_string_lossy()
                .into_owned(),
            output_size: 3,
            origin: None,
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            max_rss_kb: None,
        };
        registry::persist_record(&home, &rec).unwrap();
        let mut reg = Registry::new(home.clone());
        let res = scan_tasks(&home, &mut reg);
        assert_eq!(res.orphaned, 1);
        let e = &reg.tasks["sh_0000000a"];
        assert_eq!(e.record.status, TaskStatus::Orphaned);
        assert_eq!(e.record.end_reason.as_deref(), Some("manager-crash"));
        assert!(e.record.ended_at.is_some());
        // Persisted too.
        let loaded = registry::load_all_records(&home);
        assert_eq!(loaded[0].status, TaskStatus::Orphaned);
        std::fs::remove_dir_all(&home).ok();
    }

    /// The exit path persists `output_size` when the child exits, which can
    /// be before the tee has drained the pipe. The output file is the truth:
    /// a loaded terminal record must report its full size.
    #[test]
    fn scan_recovers_output_size_of_terminal_records_from_the_file() {
        let home = temp_home("scan-size");
        let out = registry::task_output_path(&home, "s1", "sh_0000000b");
        let rec = crate::proto::TaskRecord {
            task_id: "sh_0000000b".into(),
            session_id: "s1".into(),
            kind: crate::proto::TaskKind::Shell,
            command: "seq 1 3".into(),
            cwd: "/tmp".into(),
            pid: 99_999_999,
            status: TaskStatus::Completed,
            exit_code: Some(0),
            signal: None,
            started_at: now_ms(),
            ended_at: Some(now_ms()),
            output_path: out.to_string_lossy().into_owned(),
            output_size: 0, // snapshot taken before the pipe was drained
            origin: None,
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            max_rss_kb: None,
        };
        registry::persist_record(&home, &rec).unwrap();
        fs::write(&out, b"1\n2\n3\n").unwrap();
        let mut reg = Registry::new(home.clone());
        let res = scan_tasks(&home, &mut reg);
        assert_eq!(res.loaded, 1);
        let e = &reg.tasks["sh_0000000b"];
        assert_eq!(e.record.output_size, 6);
        assert_eq!(e.output.lock().unwrap().total_size, 6);
        std::fs::remove_dir_all(&home).ok();
    }
}
