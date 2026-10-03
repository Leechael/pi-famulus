//! Process engine (design doc §3.4): spawn in a new session/process group,
//! merged stdout+stderr tee (64KB memory ring + full disk append), kill helpers.
//!
//! Pipe creation uses `Stdio::piped()` (CLOEXEC, no raw fds). Session leadership
//! and group signalling live in [`crate::sys`] — the only production `unsafe`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
#[cfg(unix)]
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub use crate::sys::{pid_alive, signal_group, SIGKILL, SIGTERM};

/// Best-effort live CPU for requested process groups, read from one Linux
/// `/proc` scan. Values are cumulative user/system milliseconds for currently
/// visible members; reaped descendants are intentionally not inferred.
pub fn live_group_cpu_ms_many(pgids: &[u32]) -> HashMap<u32, Option<(u64, u64)>> {
    let mut readings: HashMap<u32, Option<(u64, u64)>> =
        pgids.iter().copied().map(|pgid| (pgid, None)).collect();
    let Some(ticks) = crate::sys::clock_ticks_per_second() else {
        return readings;
    };
    let Ok(groups) = crate::sys::group_cpu_ticks_by_pgid(pgids) else {
        return readings;
    };
    for (pgid, (user, system)) in groups {
        readings.insert(
            pgid,
            Some((
                user.saturating_mul(1000) / ticks,
                system.saturating_mul(1000) / ticks,
            )),
        );
    }
    readings
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveCpuReading {
    pub user_ms: u64,
    pub system_ms: u64,
    /// Recent CPU use as a percentage of one core; multi-process groups may
    /// exceed 100%. Absent until two samples are available.
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct LiveCpuSample {
    at: std::time::Instant,
    user_ms: u64,
    system_ms: u64,
    raw_user_ms: u64,
    raw_system_ms: u64,
    rate_valid: bool,
}

/// Monotonic best-effort CPU snapshots and interval rates by task id.
#[derive(Default)]
pub struct LiveCpuTracker {
    previous: HashMap<String, LiveCpuSample>,
}

impl LiveCpuTracker {
    pub fn update(
        &mut self,
        task_id: &str,
        at: std::time::Instant,
        user_ms: u64,
        system_ms: u64,
    ) -> LiveCpuReading {
        let previous = self.previous.get(task_id).copied();
        let total_user_ms = previous.map_or(user_ms, |p| p.user_ms.max(user_ms));
        let total_system_ms = previous.map_or(system_ms, |p| p.system_ms.max(system_ms));
        let percent = previous.and_then(|p| {
            if !p.rate_valid || user_ms < p.raw_user_ms || system_ms < p.raw_system_ms {
                // A failed interval or a process leaving the visible group
                // invalidates the old aggregate baseline. Rebase now; a later
                // sample can still report CPU from surviving/new members.
                return None;
            }
            let elapsed = at.checked_duration_since(p.at)?.as_millis();
            if elapsed == 0 {
                return None;
            }
            let user_delta = user_ms - p.raw_user_ms;
            let system_delta = system_ms - p.raw_system_ms;
            Some((user_delta.saturating_add(system_delta)) as f64 * 100.0 / elapsed as f64)
        });
        // Keep cumulative totals monotonic even though `/proc` only covers
        // members still visible in the group, while separately retaining the
        // raw aggregate as the interval baseline.
        self.previous.insert(
            task_id.to_string(),
            LiveCpuSample {
                at,
                user_ms: total_user_ms,
                system_ms: total_system_ms,
                raw_user_ms: user_ms,
                raw_system_ms: system_ms,
                rate_valid: true,
            },
        );
        LiveCpuReading {
            user_ms: total_user_ms,
            system_ms: total_system_ms,
            percent,
        }
    }

    /// Exclude the interval containing a failed sample without discarding the
    /// last cumulative CPU totals.
    pub fn mark_unavailable(&mut self, task_id: &str) {
        if let Some(previous) = self.previous.get_mut(task_id) {
            previous.rate_valid = false;
        }
    }

    pub fn retain(&mut self, active: &HashSet<String>) {
        self.previous.retain(|task_id, _| active.contains(task_id));
    }
}

/// Status-pipe reader (Unix unix-pipe; Windows tokio file over an anonymous pipe).
#[cfg(unix)]
pub type StatusRx = pipe::Receiver;
#[cfg(windows)]
pub type StatusRx = tokio::fs::File;

/// stdout/stderr read ends parked between tee pumps (Unix fds; Windows unused).
#[cfg(unix)]
pub type PipeFd = OwnedFd;
#[cfg(windows)]
pub type PipeFd = ();

/// Live stdout/stderr sources for the tee.
#[cfg(unix)]
pub type TeeSource = OwnedFd;
#[cfg(windows)]
pub type TeeSource = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

/// §3.4: in-memory ring buffer is 64KB; the disk file keeps the full stream.
pub const RING_CAPACITY: usize = 64 * 1024;

/// Cap on a single tee read / fanout chunk.
const READ_CHUNK: usize = 8192;

/// Bounded tee→fanout channel. Worst case ≈ `CAP * READ_CHUNK` bytes in flight
/// (~512 KiB), so a slow watcher cannot grow process memory unboundedly.
pub const CHUNK_CHANNEL_CAP: usize = 64;

// ---------------------------------------------------------------------------
// Output buffering
// ---------------------------------------------------------------------------

pub struct RingBuffer {
    buf: VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        RingBuffer {
            buf: VecDeque::with_capacity(cap.min(1024)),
            cap,
        }
    }

    /// Hard capacity (always `RING_CAPACITY` for task output rings).
    #[allow(dead_code)] // used by unit tests + memory-bound assertions
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            // Faster path: only the tail of a huge write survives.
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        self.buf.extend(data);
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Copy out `len` bytes starting at `skip` (from the oldest retained byte).
    pub fn slice(&self, skip: usize, len: usize) -> Vec<u8> {
        self.buf.iter().skip(skip).take(len).copied().collect()
    }
}

/// Shared output state for a task: ring tail + full disk file + total size.
/// `file` is None for records loaded from disk (the file is only read then).
pub struct OutputState {
    pub ring: RingBuffer,
    pub total_size: u64,
    pub file: Option<File>,
}

impl OutputState {
    pub fn new(file: Option<File>, total_size: u64) -> Self {
        OutputState {
            ring: RingBuffer::new(RING_CAPACITY),
            total_size,
            file,
        }
    }

    /// Append one chunk: disk (full) + ring (truncated). Returns next cursor.
    pub fn append(&mut self, data: &[u8]) -> u64 {
        if let Some(f) = self.file.as_mut() {
            // Unbuffered write; readers opening the path see it immediately.
            let _ = f.write_all(data);
        }
        self.ring.push(data);
        self.total_size += data.len() as u64;
        self.total_size
    }
}

/// One tee'd chunk, delivered to the watch-event fanout task.
pub struct OutputChunk {
    pub bytes: Vec<u8>,
    pub next_cursor: u64,
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

#[cfg(test)]
pub struct SpawnedTask {
    /// The task's runner (`pi-famulus __run`), leader of its process group.
    pub child: Child,
    /// Read end of the runner's status pipe (see `crate::runner`).
    pub status: StatusRx,
    pub pid: u32,
    pub output: Arc<Mutex<OutputState>>,
    pub chunks: mpsc::Receiver<OutputChunk>,
    /// Strong count of active tee pumps (stdout + stderr). Starts at 2;
    /// each pump drops its token on EOF. Tests wait until this hits 0.
    pub tee_remaining: Arc<AtomicUsize>,
}

/// A task's runner process, as the daemon waits for it: the tokio `Child`
/// it spawned, or a bare pid inherited across an in-place upgrade (still
/// our child, so `waitpid` works, but tokio cannot adopt it).
pub enum RunnerProc {
    Child(Child),
    #[allow(dead_code)] // built by the in-place upgrade's restore
    Pid(u32),
}

impl RunnerProc {
    /// Wait for the runner to exit. Cancel-safe: dropping the future before
    /// it completes never loses a reaped status.
    pub async fn wait(&mut self) -> Option<std::process::ExitStatus> {
        match self {
            RunnerProc::Child(c) => c.wait().await.ok(),
            RunnerProc::Pid(pid) => loop {
                match crate::sys::waitpid_nohang(*pid) {
                    Ok(Some(s)) => return Some(s),
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                    Err(_) => return None,
                }
            },
        }
    }
}

/// Sibling path for the stderr-only inspection file next to `<id>.output`.
pub fn stderr_path_for(output_path: &Path) -> PathBuf {
    let s = output_path.to_string_lossy();
    if let Some(stem) = s.strip_suffix(".output") {
        PathBuf::from(format!("{stem}.stderr"))
    } else {
        output_path.with_extension("stderr")
    }
}

/// The daemon's lifeline (§3.2): every runner holds a copy of the read end;
/// only this process holds the write end, so any end of the daemon (even
/// `kill -9`) is an EOF every runner sees. A single owner, so an in-place
/// exec handover has exactly one descriptor to carry across.
///
/// On Windows the Job Object's `KILL_ON_JOB_CLOSE` is the lifeline; this
/// type still exists so shared call sites compile, but pipes are unused.
#[cfg(unix)]
pub struct Lifeline {
    /// Read end, numbered >= 10, close-on-exec (placed at fd 3 in runners).
    pub read: OwnedFd,
    /// Write end, close-on-exec: it must never reach a task. Never written;
    /// only its closing matters (an in-place upgrade keeps it open).
    pub write: OwnedFd,
}

#[cfg(unix)]
static LIFELINE: OnceLock<Lifeline> = OnceLock::new();

/// Install the lifeline inherited across an in-place upgrade: the same pipe
/// every runner already holds. Both ends go back to close-on-exec.
#[cfg(unix)]
pub fn adopt_lifeline(read: OwnedFd, write: OwnedFd) -> io::Result<()> {
    crate::sys::set_cloexec(read.as_raw_fd())?;
    crate::sys::set_cloexec(write.as_raw_fd())?;
    LIFELINE
        .set(Lifeline { read, write })
        .map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "lifeline already set"))
}

#[cfg(unix)]
pub fn lifeline() -> io::Result<&'static Lifeline> {
    if let Some(l) = LIFELINE.get() {
        return Ok(l);
    }
    let (r, w) = crate::sys::pipe_cloexec()?;
    let read = crate::sys::dup_cloexec_high(&r)?;
    // A racing initializer wins; our pipe is simply dropped.
    Ok(LIFELINE.get_or_init(|| Lifeline { read, write: w }))
}

/// Path of the binary that provides `__run`: this executable. Unit tests
/// run inside the test harness, so they use the `pi-famulus` binary cargo
/// builds next to it.
pub fn runner_exe() -> io::Result<PathBuf> {
    #[cfg(test)]
    {
        let exe = std::env::current_exe()?;
        // target/<profile>/deps/pi_famulus-<hash> -> target/<profile>/pi-famulus
        if let Some(profile_dir) = exe.parent().and_then(|d| d.parent()) {
            return Ok(profile_dir.join("pi-famulus"));
        }
        return Ok(exe);
    }
    #[cfg(all(not(test), target_os = "linux"))]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(all(not(test), not(target_os = "linux")))]
    {
        crate::handover::exe_path()
    }
}

/// The runner process and the descriptors the daemon reads it through.
pub struct ProcessParts {
    pub child: Child,
    /// Read end of the runner's status pipe (see `crate::runner`).
    pub status: StatusRx,
    pub pid: u32,
    /// Read ends of the task's stdout / stderr (Unix: OwnedFd for handover;
    /// Windows: async readers — no in-place upgrade).
    pub stdout: TeeSource,
    pub stderr: TeeSource,
    /// Windows job generation from [`crate::sys::assign_job`]. Zero on Unix,
    /// where the process group id is not recycled out from under us.
    pub job_generation: u64,
}

/// Spawn `<runner> __run <command>` as a session leader (setsid in
/// pre_exec, §3.4) so the whole process tree can be signalled as one group.
/// The runner execs `sh -c` / `cmd /c <command>` in that group, holds the
/// lifeline (Unix) or sits in a Job Object (Windows), and reports status.
///
/// stdout and stderr come back as platform tee sources; [`start_tee`] reads
/// them. On Unix, keeping them as descriptors lets the daemon park readers
/// and carry pipes across an in-place upgrade.
pub fn spawn_process(command: &str, cwd: &str, env: &HashMap<String, String>) -> io::Result<ProcessParts> {
    #[cfg(unix)]
    {
        spawn_process_unix(command, cwd, env)
    }
    #[cfg(windows)]
    {
        spawn_process_windows(command, cwd, env)
    }
}

#[cfg(unix)]
fn spawn_process_unix(command: &str, cwd: &str, env: &HashMap<String, String>) -> io::Result<ProcessParts> {
    let lifeline = lifeline()?;
    let (status_read, status_write) = crate::sys::pipe_cloexec()?;
    let status_write = crate::sys::dup_cloexec_high(&status_write)?;
    let runner = runner_exe()?;
    let mut cmd = Command::new(&runner);
    cmd.arg("__run").arg(command);
    cmd.current_dir(cwd);
    // §3.3: env is the complete environment; the client builds it.
    cmd.env_clear().envs(env);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    crate::sys::apply_runner_setup_tokio(&mut cmd, lifeline.read.as_raw_fd(), status_write.as_raw_fd());

    let mut child = cmd.spawn().map_err(|e| {
        io::Error::new(e.kind(), format!("cannot spawn runner {}: {e}", runner.display()))
    })?;
    drop(status_write); // the runner has its copy at fd 4
    let status = pipe::Receiver::from_owned_fd(status_read)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stdout pipe missing"))?
        .into_owned_fd()?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stderr pipe missing"))?
        .into_owned_fd()?;
    let pid = child.id().unwrap_or(0);
    Ok(ProcessParts { child, status, pid, stdout, stderr, job_generation: 0 })
}

#[cfg(windows)]
fn spawn_process_windows(command: &str, cwd: &str, env: &HashMap<String, String>) -> io::Result<ProcessParts> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use tokio::io::AsyncWriteExt;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Pipes::CreatePipe;

    // Neither end is inheritable: std only keeps handles out of concurrently
    // spawned children for the stdio pipes it creates under its own spawn
    // lock. The write end reaches the runner through `sys::hand_over`.
    let mut read_h: HANDLE = std::ptr::null_mut();
    let mut write_h: HANDLE = std::ptr::null_mut();
    let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, std::ptr::null(), 0) };
    if ok == 0 || read_h.is_null() || write_h.is_null() || read_h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let status_read = unsafe { OwnedHandle::from_raw_handle(read_h as RawHandle) };
    let status_write = unsafe { OwnedHandle::from_raw_handle(write_h as RawHandle) };

    let runner = runner_exe()?;
    let mut cmd = Command::new(&runner);
    cmd.arg("__run").arg(command);
    cmd.current_dir(cwd);
    cmd.env_clear().envs(env);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    crate::sys::apply_runner_setup_tokio(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| {
        io::Error::new(e.kind(), format!("cannot spawn runner {}: {e}", runner.display()))
    })?;
    let pid = child.id().unwrap_or(0);
    let process = child
        .raw_handle()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "runner exited at spawn"))?;
    // The runner waits on stdin for the gate, so nothing has started yet.
    let generation = match crate::sys::assign_job(pid, process) {
        Ok(g) => g,
        Err(e) => {
            let _ = child.start_kill();
            return Err(e);
        }
    };
    let gate = (|| {
        let theirs = crate::sys::hand_over(status_write, process)?;
        let shell = crate::sys::task_shell();
        let kind = match shell.kind {
            crate::sys::ShellKind::Posix => "posix",
            crate::sys::ShellKind::Cmd => "cmd",
        };
        Ok::<_, io::Error>(format!("{theirs}\n{kind}\n{}\n", shell.program.display()))
    })();
    let gate = match gate {
        Ok(g) => g,
        Err(e) => {
            let _ = child.start_kill();
            crate::sys::drop_job(pid, generation);
            return Err(e);
        }
    };
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stdin pipe missing"))?;
    tokio::spawn(async move {
        let _ = stdin.write_all(gate.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let status = tokio::fs::File::from_std(std::fs::File::from(status_read));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stderr pipe missing"))?;
    Ok(ProcessParts {
        child,
        status,
        pid,
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        job_generation: generation,
    })
}

/// Open (append) the merged output file and its `.stderr` sibling.
pub fn open_output_files(output_path: &Path) -> io::Result<(File, File)> {
    let out = OpenOptions::new().create(true).append(true).open(output_path)?;
    let err = OpenOptions::new().create(true).append(true).open(stderr_path_for(output_path))?;
    Ok((out, err))
}

/// The two tee pumps of one task. Each returns its pipe's descriptor when it
/// was parked (Unix), or None once the pipe hit EOF / Windows (no park).
#[allow(dead_code)] // awaited when the tee is parked (in-place upgrade)
pub struct Tee {
    pub stdout: tokio::task::JoinHandle<Option<PipeFd>>,
    pub stderr: tokio::task::JoinHandle<Option<PipeFd>>,
}

/// Start reading a task's stdout / stderr: both append to the merged
/// `.output` file + ring, and stderr is mirrored into `stderr_mirror`.
pub fn start_tee(
    stdout: Option<TeeSource>,
    stderr: Option<TeeSource>,
    output: Arc<Mutex<OutputState>>,
    stderr_mirror: Option<File>,
    tx: mpsc::Sender<OutputChunk>,
    park: tokio::sync::watch::Receiver<bool>,
) -> io::Result<Tee> {
    // Only count pipes that exist: a None source finishing must not be the
    // "last" decrement that closes the file while its sibling is parked.
    let n_open = stdout.is_some() as usize + stderr.is_some() as usize;
    let open = Arc::new(AtomicUsize::new(n_open));
    if n_open == 0 {
        output.lock().unwrap().file = None;
    }
    #[cfg(unix)]
    {
        let out_rx = stdout.map(pipe::Receiver::from_owned_fd).transpose()?;
        let err_rx = stderr.map(pipe::Receiver::from_owned_fd).transpose()?;
        let (o, t, p, n) = (output.clone(), tx.clone(), park.clone(), open.clone());
        let stdout = tokio::spawn(async move {
            let parked = match out_rx {
                Some(r) => {
                    let parked = pump_unix(r, o.clone(), t, None, p).await;
                    pump_done(&n, &o, parked.is_some());
                    parked
                }
                None => None,
            };
            parked
        });
        let stderr = tokio::spawn(async move {
            let parked = match err_rx {
                Some(r) => {
                    let parked = pump_unix(r, output.clone(), tx, stderr_mirror, park).await;
                    pump_done(&open, &output, parked.is_some());
                    parked
                }
                None => None,
            };
            parked
        });
        Ok(Tee { stdout, stderr })
    }
    #[cfg(windows)]
    {
        let (o, t, p, n) = (output.clone(), tx.clone(), park.clone(), open.clone());
        let stdout = tokio::spawn(async move {
            match stdout {
                Some(r) => {
                    let parked = pump_async(r, o.clone(), t, None, p).await;
                    pump_done(&n, &o, parked);
                }
                None => {}
            }
            None
        });
        let stderr = tokio::spawn(async move {
            match stderr {
                Some(r) => {
                    let parked = pump_async(r, output.clone(), tx, stderr_mirror, park).await;
                    pump_done(&open, &output, parked);
                }
                None => {}
            }
            None
        });
        Ok(Tee { stdout, stderr })
    }
}

/// A finished task stays in the registry until gc evicts it, so its output
/// writer is closed once both pipes are drained rather than with the
/// record. A parked pump keeps it: the tee resumes after the handover.
fn pump_done(open: &AtomicUsize, output: &Mutex<OutputState>, parked: bool) {
    // A parked pump is not a closed source. Decrementing it lets a sibling
    // that hit EOF (the last decrement, and not itself parked) drop the
    // merged writer. After a failed upgrade the parked pipe is restarted
    // against the same OutputState, which does not reopen that writer, so
    // disk output would stop.
    if parked {
        return;
    }
    if open.fetch_sub(1, Ordering::SeqCst) == 1 {
        output.lock().unwrap().file = None;
    }
}

/// Spawn a task and start its tee (unit tests and simple callers): the
/// runner, the output state over `output_path`, and the chunk channel.
/// Must be called from inside a Tokio runtime.
#[cfg(test)]
pub fn spawn(
    command: &str,
    cwd: &str,
    env: &HashMap<String, String>,
    output_path: &Path,
) -> io::Result<SpawnedTask> {
    let parts = spawn_process(command, cwd, env)?;
    let (out_file, err_file) = open_output_files(output_path)?;
    let output = Arc::new(Mutex::new(OutputState::new(Some(out_file), 0)));
    let (tx, rx) = mpsc::channel(CHUNK_CHANNEL_CAP);
    let (_park_tx, park) = tokio::sync::watch::channel(false);
    let tee = start_tee(Some(parts.stdout), Some(parts.stderr), output.clone(), Some(err_file), tx, park)?;
    let tee_remaining = Arc::new(AtomicUsize::new(2));
    for h in [tee.stdout, tee.stderr] {
        let t = tee_remaining.clone();
        tokio::spawn(async move {
            let _ = h.await;
            t.fetch_sub(1, Ordering::SeqCst);
        });
    }
    // Keep the park sender alive for the test's lifetime: dropping it would
    // end `changed()` waits, not park the pumps (they wait for `true`).
    std::mem::forget(_park_tx);
    Ok(SpawnedTask {
        child: parts.child,
        status: parts.status,
        pid: parts.pid,
        output,
        chunks: rx,
        tee_remaining,
    })
}

/// Resolves once `park` is true (immediately if it already is).
pub async fn parked(park: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = park.wait_for(|p| *p).await;
}

/// Test hook `PI_FAMULUS_TEST_PUMP_STALL=<bytes>:<ms>`: a pump pauses once, after
/// it has read at least `bytes`, as a pump held up by a busy runtime or a
/// full fanout channel would. Placed near the end of a command's output, the
/// pause outlasts the command: its exit is seen with output still unread.
fn test_pump_stall() -> Option<(u64, std::time::Duration)> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let v = std::env::var("PI_FAMULUS_TEST_PUMP_STALL").ok()?;
    let (bytes, ms) = v.split_once(':')?;
    Some((bytes.parse().ok()?, std::time::Duration::from_millis(ms.parse().ok()?)))
}

#[cfg(unix)]
async fn pump_unix(
    mut reader: pipe::Receiver,
    out: Arc<Mutex<OutputState>>,
    tx: mpsc::Sender<OutputChunk>,
    mut mirror: Option<File>,
    mut park: tokio::sync::watch::Receiver<bool>,
) -> Option<OwnedFd> {
    let mut buf = [0u8; READ_CHUNK];
    let mut stall = test_pump_stall();
    let mut read_total = 0u64;
    loop {
        let n = tokio::select! {
            biased;
            // Checked before every read: parking happens only between reads.
            _ = parked(&mut park) => return reader.into_nonblocking_fd().ok(),
            // Cancel-safe: bytes leave the pipe only when this completes.
            r = reader.read(&mut buf) => match r {
                Ok(0) => return None,
                Ok(n) => n,
                // tokio retries EINTR internally, so any error here is terminal.
                Err(_) => return None,
            },
        };
        if let Some((after, d)) = stall {
            read_total += n as u64;
            if read_total >= after {
                stall = None;
                tokio::time::sleep(d).await;
            }
        }
        let chunk = buf[..n].to_vec();
        if let Some(f) = mirror.as_mut() {
            let _ = f.write_all(&chunk);
        }
        let next_cursor = out.lock().unwrap().append(&chunk);
        if tx.send(OutputChunk { bytes: chunk, next_cursor }).await.is_err() {
            return None;
        }
    }
}

#[cfg(windows)]
async fn pump_async(
    mut reader: TeeSource,
    out: Arc<Mutex<OutputState>>,
    tx: mpsc::Sender<OutputChunk>,
    mut mirror: Option<File>,
    mut park: tokio::sync::watch::Receiver<bool>,
) -> bool {
    let mut buf = [0u8; READ_CHUNK];
    let mut stall = test_pump_stall();
    let mut read_total = 0u64;
    loop {
        let n = tokio::select! {
            biased;
            _ = parked(&mut park) => return true,
            r = reader.read(&mut buf) => match r {
                Ok(0) => return false,
                Ok(n) => n,
                Err(_) => return false,
            },
        };
        if let Some((after, d)) = stall {
            read_total += n as u64;
            if read_total >= after {
                stall = None;
                tokio::time::sleep(d).await;
            }
        }
        let chunk = buf[..n].to_vec();
        if let Some(f) = mirror.as_mut() {
            let _ = f.write_all(&chunk);
        }
        let next_cursor = out.lock().unwrap().append(&chunk);
        if tx.send(OutputChunk { bytes: chunk, next_cursor }).await.is_err() {
            return false;
        }
    }
}

// ---------------------------------------------------------------------------
// Chunk boundaries (output responses & watch events)
// ---------------------------------------------------------------------------

/// Extra bytes to read past `max` so a character straddling the limit can be
/// recognised (UTF-8 sequences are at most 4 bytes).
pub const UTF8_LOOKAHEAD: usize = 3;

/// Bytes `c` occupies inside a JSON string as serde_json writes it.
fn json_escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        c if (c as u32) < 0x20 => 6, // \u00XX
        c => c.len_utf8(),
    }
}

/// How many leading bytes of `buf` to send as one chunk (§3.3 output).
///
/// - Never cuts inside a valid UTF-8 sequence: the cut backs off to the
///   previous character boundary, so a lossy decode never invents U+FFFD for
///   valid text and `next_cursor` never skips bytes.
/// - An incomplete sequence at the very end of `buf` is held back while
///   `more_may_follow` (the rest has not been read or written yet).
/// - The JSON-escaped size of the decoded chunk stays within `budget`, so the
///   response frame cannot exceed the 4 MiB cap (control bytes escape to 6).
/// - Progress: if the first character alone exceeds `max`, it is still sent
///   whole (a chunk may exceed `max` by up to 3 bytes). `max == 0` returns 0.
///
/// `buf` may hold up to `max + UTF8_LOOKAHEAD` bytes.
pub fn utf8_chunk_len(buf: &[u8], max: usize, budget: usize, more_may_follow: bool) -> usize {
    if max == 0 {
        return 0;
    }
    let mut pos = 0usize;
    let mut cost = 0usize;
    for chunk in buf.utf8_chunks() {
        for c in chunk.valid().chars() {
            let (len, esc) = (c.len_utf8(), json_escaped_len(c));
            if pos + len > max || cost + esc > budget {
                return if pos == 0 { len } else { pos };
            }
            pos += len;
            cost += esc;
        }
        let bad = chunk.invalid();
        if bad.is_empty() {
            continue;
        }
        // A truncated (not malformed) sequence ending the buffer may be
        // completed by bytes not yet available: hold it back.
        let at_end = pos + bad.len() == buf.len();
        let truncated = std::str::from_utf8(bad)
            .err()
            .is_some_and(|e| e.error_len().is_none());
        if at_end && truncated && more_may_follow {
            return pos;
        }
        // Malformed bytes decode to one U+FFFD (3 bytes) per invalid run.
        if pos + bad.len() > max || cost + 3 > budget {
            return if pos == 0 { bad.len() } else { pos };
        }
        pos += bad.len();
        cost += 3;
    }
    pos
}

// ---------------------------------------------------------------------------
// File reading (output requests)
// ---------------------------------------------------------------------------

/// Read up to `max` bytes starting at byte `offset`. Missing file or an
/// offset at/past EOF yields an empty chunk with the offset unchanged.
pub fn read_file_range(path: &Path, offset: u64, max: usize) -> io::Result<(Vec<u8>, u64)> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), offset)),
        Err(e) => return Err(e),
    };
    // Seeking past EOF is fine for a regular file; the read is then empty.
    // read_to_end retries EINTR itself.
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.take(max as u64).read_to_end(&mut buf)?;
    let next = offset + buf.len() as u64;
    Ok((buf, next))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    #[test]
    fn live_cpu_tracker_keeps_totals_monotonic_and_rebases_after_process_exit() {
        let mut tracker = LiveCpuTracker::default();
        let at = std::time::Instant::now();
        let first = tracker.update("sh_1", at, 100, 20);
        assert_eq!((first.user_ms, first.system_ms, first.percent), (100, 20, None));

        let second = tracker.update("sh_1", at + Duration::from_secs(5), 300, 40);
        assert_eq!((second.user_ms, second.system_ms), (300, 40));
        assert_eq!(second.percent, Some(4.4));

        // A lower visible-process aggregate preserves the total but resets
        // the interval baseline instead of hiding subsequent CPU forever.
        let third = tracker.update("sh_1", at + Duration::from_secs(10), 50, 10);
        assert_eq!((third.user_ms, third.system_ms, third.percent), (300, 40, None));
        let fourth = tracker.update("sh_1", at + Duration::from_secs(15), 70, 20);
        assert_eq!((fourth.user_ms, fourth.system_ms), (300, 40));
        assert_eq!(fourth.percent, Some(0.6));
        tracker.retain(&HashSet::new());
        assert_eq!(tracker.update("sh_1", at + Duration::from_secs(20), 1, 2).percent, None);
    }

    #[test]
    fn live_cpu_tracker_does_not_average_over_a_failed_interval() {
        let mut tracker = LiveCpuTracker::default();
        let at = std::time::Instant::now();
        tracker.update("sh_1", at, 100, 20);
        tracker.mark_unavailable("sh_1");
        let recovered = tracker.update("sh_1", at + Duration::from_secs(10), 200, 40);
        assert_eq!((recovered.user_ms, recovered.system_ms), (200, 40));
        assert_eq!(recovered.percent, None);
        let reading = tracker.update("sh_1", at + Duration::from_secs(15), 210, 45);
        assert_eq!(reading.percent, Some(0.3));
    }

    #[test]
    fn ring_buffer_caps_at_capacity() {
        let mut r = RingBuffer::new(1024);
        let data = vec![7u8; 3000];
        r.push(&data);
        assert_eq!(r.len(), 1024);
        r.push(b"abc");
        assert_eq!(r.len(), 1024);
        let tail = r.slice(1021, 3);
        assert_eq!(tail, b"abc");
        // A write larger than the cap keeps only its tail.
        r.push(&vec![1u8; 5000]);
        assert_eq!(r.len(), 1024);
        assert!(r.slice(0, 1024).iter().all(|b| *b == 1));
    }

    #[test]
    fn output_state_ring_hard_cap_is_ring_capacity() {
        let mut st = OutputState::new(None, 0);
        assert_eq!(st.ring.capacity(), RING_CAPACITY);
        let big = vec![9u8; RING_CAPACITY * 3];
        st.append(&big);
        assert_eq!(st.ring.len(), RING_CAPACITY);
        assert_eq!(st.total_size, big.len() as u64);
        assert_eq!(st.ring.capacity(), RING_CAPACITY);
    }

    #[test]
    fn read_file_range_offsets() {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-task-test-{}-{}",
            std::process::id(),
            crate::proto::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.bin");
        std::fs::write(&path, b"0123456789").unwrap();

        let (bytes, next) = read_file_range(&path, 0, 4).unwrap();
        assert_eq!(bytes, b"0123");
        assert_eq!(next, 4);
        let (bytes, next) = read_file_range(&path, next, 100).unwrap();
        assert_eq!(bytes, b"456789");
        assert_eq!(next, 10);
        let (bytes, next) = read_file_range(&path, next, 100).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(next, 10);
        // Missing file -> empty, offset unchanged.
        let (bytes, next) = read_file_range(&dir.join("nope"), 5, 10).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(next, 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn chunk_len_backs_off_to_char_boundary() {
        let text = "a中文".as_bytes(); // 1 + 3 + 3 bytes
        let big = usize::MAX;
        assert_eq!(utf8_chunk_len(text, 1, big, true), 1);
        assert_eq!(utf8_chunk_len(text, 2, big, true), 1); // not inside 中
        assert_eq!(utf8_chunk_len(text, 3, big, true), 1);
        assert_eq!(utf8_chunk_len(text, 4, big, true), 4);
        assert_eq!(utf8_chunk_len(text, 6, big, true), 4);
        assert_eq!(utf8_chunk_len(text, 7, big, true), 7);
        // Progress: a first char wider than max is sent whole.
        assert_eq!(utf8_chunk_len(&text[1..], 1, big, true), 3);
        assert_eq!(utf8_chunk_len(text, 0, big, true), 0);
        // Truncated tail: held back while more may follow, sent at EOF.
        let cut = &text[..5]; // "a中" + first byte of 文
        assert_eq!(utf8_chunk_len(cut, 100, big, true), 4);
        assert_eq!(utf8_chunk_len(cut, 100, big, false), 5);
        assert_eq!(utf8_chunk_len(&cut[4..], 100, big, true), 0);
        // Malformed bytes are not held back, even when more may follow.
        assert_eq!(utf8_chunk_len(b"ok\xff\xfeend", 100, big, true), 7);
        assert_eq!(utf8_chunk_len(b"ok\xff", 100, big, true), 3);
        // A malformed run that ends exactly at max is included; one that
        // would cross max is not split.
        assert_eq!(utf8_chunk_len(b"abcd\xe4\xb8A", 6, big, false), 6);
        assert_eq!(utf8_chunk_len(b"abcd\xe4\xb8A", 5, big, false), 4);
        // Concatenating chunks cut at every max reproduces the text exactly.
        let s = "中文\n".repeat(50);
        for max in 1..12 {
            let (mut pos, mut out) = (0usize, String::new());
            while pos < s.len() {
                let end = (pos + max + UTF8_LOOKAHEAD).min(s.len());
                let n = utf8_chunk_len(&s.as_bytes()[pos..end], max, big, end < s.len());
                assert!(n > 0, "no progress at {pos} max {max}");
                out.push_str(std::str::from_utf8(&s.as_bytes()[pos..pos + n]).unwrap());
                pos += n;
            }
            assert_eq!(out, s, "max {max}");
        }
    }

    #[test]
    fn chunk_len_respects_escaped_budget() {
        let ctl = vec![1u8; 1000]; // each escapes to \u0001 (6 bytes)
        assert_eq!(utf8_chunk_len(&ctl, 1000, 600, false), 100);
        let quotes = vec![b'"'; 1000]; // \" (2 bytes)
        assert_eq!(utf8_chunk_len(&quotes, 1000, 600, false), 300);
        let bad = vec![0xffu8; 1000]; // each -> U+FFFD (3 bytes)
        assert_eq!(utf8_chunk_len(&bad, 1000, 600, false), 200);
        let enc = serde_json::to_string(&String::from_utf8_lossy(&ctl[..100])).unwrap();
        assert_eq!(enc.len() - 2, 600, "cost model matches serde_json");
        // Plain text costs its byte length: space (0x20) is not escaped.
        assert_eq!(utf8_chunk_len(&[b' '; 1000], 1000, 1000, false), 1000);
        assert_eq!(utf8_chunk_len(&[0x1f; 10], 10, 60, false), 10);
        // A budget smaller than one char still makes progress.
        assert_eq!(utf8_chunk_len(&ctl, 1000, 1, false), 1);
    }

    #[test]
    fn pid_alive_checks() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(99_999_999)); // invalid/ESRCH/EINVAL all map to dead
    }

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-task-test-{tag}-{}-{}",
            std::process::id(),
            crate::proto::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn drain_chunks(rx: &mut mpsc::Receiver<OutputChunk>) -> Vec<u8> {
        let mut collected = Vec::new();
        let drain = async {
            while let Some(c) = rx.recv().await {
                collected.extend_from_slice(&c.bytes);
            }
        };
        tokio::time::timeout(Duration::from_secs(30), drain)
            .await
            .expect("tee should reach EOF after child exit");
        collected
    }

    /// Wait for the child while concurrently draining the bounded tee channel.
    /// Draining after `wait` alone deadlocks once the channel fills (pump stops
    /// reading → child blocks on write → wait never finishes).
    async fn wait_and_drain(
        t: &mut SpawnedTask,
    ) -> (std::process::ExitStatus, Vec<u8>) {
        let mut chunks = std::mem::replace(
            &mut t.chunks,
            mpsc::channel(1).1, // placeholder; unused after take
        );
        let wait = t.child.wait();
        let drain = drain_chunks(&mut chunks);
        let (status, collected) = tokio::join!(wait, drain);
        (status.unwrap(), collected)
    }

    async fn wait_tee_idle(tee: &Arc<AtomicUsize>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tee.load(Ordering::SeqCst) != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "tee pumps did not finish; remaining={}",
                tee.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_captures_merged_output_and_exit() {
        let dir = unique_dir("echo");
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("t.output");
        let env = HashMap::new();
        // The task reports its own process group. Asking from outside
        // (getpgid) raced the task's exit: on macOS a finished task is
        // already gone for getpgid, and a stress run caught that.
        let mut t = spawn(
            "printf 'out-line\\n'; printf 'err-line\\n' >&2; echo \"pgid=$(ps -o pgid= -p $$ | tr -d ' ')\"",
            "/",
            &env,
            &out_path,
        )
        .unwrap();
        let _ = &dir;

        let (status, collected) = wait_and_drain(&mut t).await;
        assert_eq!(status.code(), Some(0), "runner signal: {:?}", status.signal());
        wait_tee_idle(&t.tee_remaining).await;

        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("out-line"), "stdout captured: {text:?}");
        assert!(text.contains("err-line"), "stderr merged: {text:?}");
        // Child leads its own process group/session (setsid, §3.4).
        assert!(text.contains(&format!("pgid={}\n", t.pid)), "own process group: {text:?}");

        let st = t.output.lock().unwrap();
        assert_eq!(st.total_size, collected.len() as u64);
        assert_eq!(st.ring.len(), collected.len());
        drop(st);
        // Disk holds the full merged stream.
        let on_disk = std::fs::read(&out_path).unwrap();
        assert_eq!(on_disk, collected);
        // stderr-only inspection file holds only the err stream.
        let err_disk = std::fs::read(stderr_path_for(&out_path)).unwrap();
        assert_eq!(String::from_utf8_lossy(&err_disk), "err-line\n");
        // stdout must not leak into the stderr file.
        assert!(!err_disk.windows(b"out-line".len()).any(|w| w == b"out-line"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn signal_group_kills_whole_tree() {
        let dir = unique_dir("kill");
        let out_path = dir.join("t.output");
        let env = HashMap::new();
        // Invariant: one group signal kills an existing process tree. Killing
        // immediately after spawn instead races the runner's fork of sh: on
        // macOS a half-created child can miss that signal and keep the output
        // pipe open. Early-stop races belong to the daemon's kill_group_hard
        // tests; here the shell acknowledges both descendants before we kill.
        let mut t = spawn(
            "sleep 300 & first=$!; sleep 300 & second=$!; printf '%s %s %s\\n' \"$$\" \"$first\" \"$second\"; wait",
            "/",
            &env,
            &out_path,
        )
        .unwrap();
        // Failure-only cleanup, including a readiness timeout during fork.
        // The assertion below still exercises exactly one group signal.
        struct KillOnFailure(Option<u32>);
        impl Drop for KillOnFailure {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    for _ in 0..40 {
                        let _ = signal_group(pid, SIGKILL);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }
        let mut cleanup = KillOnFailure(Some(t.pid));
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            while !bytes.contains(&b'\n') {
                let c = t.chunks.recv().await.expect("shell exited before reporting its tree");
                bytes.extend_from_slice(&c.bytes);
            }
            String::from_utf8(bytes).unwrap()
        })
        .await
        .expect("shell must report its existing descendants before group kill");
        let descendants: Vec<u32> = ready.split_whitespace().map(|p| p.parse().unwrap()).collect();
        assert_eq!(descendants.len(), 3, "shell and two children: {ready:?}");
        assert_ne!(descendants[1], descendants[2], "distinct children: {ready:?}");
        let members = crate::sys::group_members(t.pid).unwrap();
        for pid in &descendants {
            assert_ne!(*pid, t.pid, "descendant must not be the runner");
            assert!(pid_alive(*pid), "descendant {pid} must exist before kill");
            assert!(members.contains(pid), "descendant {pid} not in group {}: {members:?}", t.pid);
        }
        assert!(pid_alive(t.pid));
        signal_group(t.pid, SIGKILL).unwrap();
        let (status, _) = wait_and_drain(&mut t).await;
        assert_eq!(status.signal(), Some(SIGKILL));
        // Reparented zombies may await init's reap; none may still run.
        let pids = descendants.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let ps = std::process::Command::new("ps")
            .args(["-p", &pids, "-o", "stat="])
            .output()
            .unwrap();
        assert!(ps.status.success() || ps.status.code() == Some(1), "ps failed: {ps:?}");
        let states = String::from_utf8(ps.stdout).unwrap();
        assert!(states.lines().all(|s| s.trim_start().starts_with('Z')), "surviving descendants: {states:?}");
        // Signalling a dead group is a no-op: ESRCH is success. On macOS a
        // group whose last member (the reparented grandchild) is still an
        // unreaped zombie answers EPERM instead, which a stress run caught.
        // Callers ignore the result either way.
        if let Err(e) = signal_group(t.pid, SIGKILL) {
            assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e}");
        }
        wait_tee_idle(&t.tee_remaining).await;
        cleanup.0 = None;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fixed amount of CPU work (no sleep: its duration is not CPU, and
    /// macOS CI rounds sleeps). About 0.4 s of user time on an M-series Mac.
    pub(crate) const BURN: &str = "awk 'BEGIN{for(i=0;i<10000000;i++)s+=i}'";
    /// Well under BURN's cost on any machine, well over two idle shells'.
    pub(crate) const BURN_FLOOR_US: u64 = 100_000;

    async fn reported(t: &mut SpawnedTask) -> crate::runner::Reported {
        let _ = wait_and_drain(t).await;
        let mut line = String::new();
        t.status.read_to_string(&mut line).await.unwrap();
        crate::runner::parse_status(&line).unwrap_or_else(|| panic!("status line: {line:?}"))
    }

    /// The runner's usage covers the whole wait chain under `sh`: here the
    /// CPU is spent only by a grandchild (two shells that cannot exec away
    /// stand between it and the runner).
    #[tokio::test]
    async fn runner_reports_cpu_of_waited_for_descendants() {
        let dir = unique_dir("cpu");
        let env = HashMap::new();
        let cmd = format!("sh -c \"{}\"; true", BURN.replace('"', "\\\""));
        let cmd = format!("sh -c '{}'; true", cmd.replace('\'', "'\\''"));
        let mut t = spawn(&cmd, "/", &env, &dir.join("t.output")).unwrap();
        let r = reported(&mut t).await;
        assert_eq!(r.code, Some(0), "{cmd}");
        let u = r.usage.expect("usage reported");
        assert!(u.cpu_user_us >= BURN_FLOOR_US, "grandchild CPU missing: {u:?} for {cmd}");
        assert!(u.max_rss_kb > 0, "{u:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A process that reaps a child and then execs: POSIX says exec keeps
    /// the children's times, and Linux does; macOS drops them (measured:
    /// `sh -c "<burn>; exec true"` reports 0.00 s user under
    /// `/usr/bin/time -l`, 0.42 s without the `exec`). Pinned per platform
    /// so a change in either shows up.
    #[tokio::test]
    async fn runner_usage_across_exec_is_platform_dependent() {
        let dir = unique_dir("cpuexec");
        let env = HashMap::new();
        let mut t = spawn(&format!("{BURN}; exec true"), "/", &env, &dir.join("t.output")).unwrap();
        let u = reported(&mut t).await.usage.expect("usage reported");
        if cfg!(target_os = "macos") {
            assert!(u.cpu_user_us < BURN_FLOOR_US, "macOS kept CPU across exec: {u:?}");
        } else {
            assert!(u.cpu_user_us >= BURN_FLOOR_US, "CPU lost across exec: {u:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The documented gap: work nobody waits for before `sh` exits is not
    /// in the report (the runner reports when `sh` ends).
    #[tokio::test]
    async fn runner_usage_excludes_unwaited_background_work() {
        let dir = unique_dir("cpubg");
        let env = HashMap::new();
        let mut t = spawn(&format!("{BURN} & exit 0"), "/", &env, &dir.join("t.output")).unwrap();
        let pid = t.pid;
        let mut chunks = std::mem::replace(&mut t.chunks, mpsc::channel(1).1);
        tokio::spawn(async move { drain_chunks(&mut chunks).await });
        let mut line = Vec::new();
        let mut buf = [0u8; 256];
        while !line.contains(&b'\n') {
            let n = t.status.read(&mut buf).await.unwrap();
            assert!(n > 0, "no status line");
            line.extend_from_slice(&buf[..n]);
        }
        let r = crate::runner::parse_status(std::str::from_utf8(&line).unwrap()).unwrap();
        signal_group(pid, SIGKILL).ok();
        let _ = t.child.wait().await;
        assert!(r.linger, "the burn should still run: {r:?}");
        let u = r.usage.expect("usage reported");
        assert!(u.cpu_user_us < BURN_FLOOR_US, "background work counted: {u:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// (a) Multi-MB child output must not inflate the in-memory ring past 64KB;
    /// disk + `total_size` still reflect the full stream.
    #[tokio::test]
    #[cfg(unix)]
    async fn large_output_keeps_ring_capped() {
        let dir = unique_dir("large");
        let out_path = dir.join("t.output");
        let env = HashMap::new();
        // 4 MiB of 'A' on stdout — well above RING_CAPACITY.
        let bytes: usize = 4 * 1024 * 1024;
        let cmd = format!(
            "dd if=/dev/zero bs=1024 count={} 2>/dev/null | tr '\\0' 'A'",
            bytes / 1024
        );
        let mut t = spawn(&cmd, "/", &env, &out_path).unwrap();
        let (status, collected) = wait_and_drain(&mut t).await;
        assert_eq!(status.code(), Some(0));
        wait_tee_idle(&t.tee_remaining).await;

        assert_eq!(collected.len(), bytes, "tee must forward full stream");
        let st = t.output.lock().unwrap();
        assert_eq!(st.ring.capacity(), RING_CAPACITY);
        assert_eq!(st.ring.len(), RING_CAPACITY, "ring hard cap");
        assert_eq!(st.total_size, bytes as u64);
        drop(st);

        let on_disk = std::fs::metadata(&out_path).unwrap().len();
        assert_eq!(on_disk, bytes as u64, "disk must hold the full stream");
        // Spot-check: ring tail is all 'A'.
        let ring_tail = t.output.lock().unwrap().ring.slice(0, RING_CAPACITY);
        assert!(ring_tail.iter().all(|b| *b == b'A'));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// (b) After exit + drain, chunk receiver is closed and tee pumps are gone.
    #[tokio::test]
    #[cfg(unix)]
    async fn tee_pumps_finish_after_exit() {
        let dir = unique_dir("tee-join");
        let out_path = dir.join("t.output");
        let env = HashMap::new();
        let mut t = spawn("printf 'hi\\n'", "/", &env, &out_path).unwrap();
        assert_eq!(t.tee_remaining.load(Ordering::SeqCst), 2);
        let (_, _) = wait_and_drain(&mut t).await;
        wait_tee_idle(&t.tee_remaining).await;
        // Further recv stays None (channel closed) — placeholder rx is empty/closed.
        assert!(t.chunks.try_recv().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// (d) Short-lived start/stop cycles must not panic; rings stay capped.
    #[tokio::test]
    #[cfg(unix)]
    async fn repeated_start_stop_stress() {
        let dir = unique_dir("stress");
        let env = HashMap::new();
        for i in 0..50 {
            let out_path = dir.join(format!("t{i}.output"));
            let mut t = spawn("sleep 30", "/", &env, &out_path).unwrap();
            // Concurrent drain while we signal — avoids bounded-channel stalls
            // if the shell writes anything on signal.
            let mut chunks = std::mem::replace(&mut t.chunks, mpsc::channel(1).1);
            let tee = t.tee_remaining.clone();
            let drain = tokio::spawn(async move { drain_chunks(&mut chunks).await });
            // Right after spawn the runner may be forking `sh`, which can
            // miss a single group signal: kill until only the leader is left
            // (what the daemon's kill paths do).
            for _ in 0..40 {
                // Once only the unreaped leader is left, macOS answers EPERM
                // (not ESRCH) for the group: ignored, as the daemon does.
                let _ = signal_group(t.pid, SIGKILL);
                tokio::time::sleep(Duration::from_millis(5)).await;
                if !crate::sys::group_has_others(t.pid) {
                    break;
                }
            }
            let status = t.child.wait().await.unwrap();
            assert!(status.signal().is_some() || status.code().is_some());
            let _ = drain.await.unwrap();
            wait_tee_idle(&tee).await;
            let st = t.output.lock().unwrap();
            assert!(st.ring.len() <= RING_CAPACITY);
            assert_eq!(st.ring.capacity(), RING_CAPACITY);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Optional RSS sanity: after multi-MB tee + drain, peak RSS should not
    /// grow by anything close to the full output size (ring + bounded channel
    /// dominate). Run with:
    /// `cargo test -p pi-famulus rss_stays_bounded_after_large_output -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "RSS sampling is coarse; run manually on Darwin/Linux"]
    #[cfg(unix)]
    async fn rss_stays_bounded_after_large_output() {
        let before = crate::sys::max_rss_bytes();
        let dir = unique_dir("rss");
        let out_path = dir.join("t.output");
        let env = HashMap::new();
        let bytes: usize = 8 * 1024 * 1024;
        let cmd = format!(
            "dd if=/dev/zero bs=1024 count={} 2>/dev/null | tr '\\0' 'B'",
            bytes / 1024
        );
        let mut t = spawn(&cmd, "/", &env, &out_path).unwrap();
        let (_, collected) = wait_and_drain(&mut t).await;
        wait_tee_idle(&t.tee_remaining).await;
        assert_eq!(collected.len(), bytes);
        assert_eq!(t.output.lock().unwrap().ring.len(), RING_CAPACITY);

        let after = crate::sys::max_rss_bytes();
        let growth = after.saturating_sub(before);
        // Allow generous overhead (allocator, tokio, copies) but far below 8 MiB.
        let ceiling = 3 * 1024 * 1024u64;
        assert!(
            growth < ceiling,
            "RSS grew by {growth} bytes (before={before}, after={after}); \
             expected << full {bytes}-byte stream (ring is {RING_CAPACITY}B, \
             chunk channel ≤ {}B). Re-run with --nocapture for detail.",
            CHUNK_CHANNEL_CAP * READ_CHUNK
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One pump parks and the sibling hits EOF. The parked pump is the one
    /// a failed upgrade resumes; the merged output file must still receive
    /// what it writes afterwards.
    #[tokio::test]
    #[cfg(unix)]
    async fn mixed_park_and_eof_keeps_the_output_file() {
        let dir = unique_dir("park-eof");
        std::fs::create_dir_all(&dir).unwrap();
        let out_path = dir.join("t.output");
        let (out_file, _) = open_output_files(&out_path).unwrap();
        let output = Arc::new(Mutex::new(OutputState::new(Some(out_file), 0)));
        let open = Arc::new(AtomicUsize::new(2));
        let (park_tx, park_rx) = tokio::sync::watch::channel(false);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(async move {
            while rx.recv().await.is_some() {}
        });

        let (mut stderr_w, stderr_r) = tokio::net::unix::pipe::pipe().unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut stderr_w, b"err-before\n").await.unwrap();
        let output_e = output.clone();
        let open_e = open.clone();
        let tx_e = tx.clone();
        let stderr_pump = tokio::spawn(async move {
            let parked = pump_unix(stderr_r, output_e.clone(), tx_e, None, park_rx).await;
            pump_done(&open_e, &output_e, parked.is_some());
            parked
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let on_disk = std::fs::read(&out_path).unwrap_or_default();
            if on_disk.windows(b"err-before\n".len()).any(|w| w == b"err-before\n") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "stderr pump did not write");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        park_tx.send(true).unwrap();
        let parked = stderr_pump.await.unwrap();
        assert!(parked.is_some(), "stderr pump should park with the write end still open");
        assert!(output.lock().unwrap().file.is_some());

        // Sibling reaches EOF after the park. Park is cleared so this pump
        // observes EOF instead of parking too; it is the last close.
        let stdout_park = park_tx.subscribe();
        park_tx.send(false).unwrap();
        let (mut stdout_w, stdout_r) = tokio::net::unix::pipe::pipe().unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut stdout_w, b"out-before\n").await.unwrap();
        drop(stdout_w);
        let parked_out = pump_unix(stdout_r, output.clone(), tx.clone(), None, stdout_park).await;
        assert!(parked_out.is_none(), "stdout should hit EOF");
        pump_done(&open, &output, false);
        assert!(
            output.lock().unwrap().file.is_some(),
            "a parked sibling must keep the merged writer"
        );

        // Resume the parked pipe (what start_task_io does after a failed
        // upgrade) and write more. The writer was not reopened.
        tokio::io::AsyncWriteExt::write_all(&mut stderr_w, b"err-after\n").await.unwrap();
        drop(stderr_w);
        let reader = tokio::net::unix::pipe::Receiver::from_owned_fd(parked.unwrap()).unwrap();
        let parked_again = pump_unix(reader, output.clone(), tx, None, park_tx.subscribe()).await;
        assert!(parked_again.is_none());
        drop(output);
        let text = String::from_utf8(std::fs::read(&out_path).unwrap()).unwrap();
        assert!(text.contains("err-before\n"), "{text:?}");
        assert!(text.contains("out-before\n"), "{text:?}");
        assert!(text.contains("err-after\n"), "{text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

}
