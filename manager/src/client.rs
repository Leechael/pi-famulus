//! CLI client (design doc §3.1 startup flow, §3.5 subcommands): connection
//! plumbing, the commands that act on the daemon (start/stop/wait/output/
//! kill-session/shutdown), `log`/`tail`, and `doctor`. Read-only inspection
//! (status/sessions/ls/show/agent/events) lives in [`crate::inspect`].
//!
//! Auto-spawn: `start`, `stop`, `wait`, `output`, `kill-session`, `ls`, and
//! `log`/`tail` on a task id start the daemon when none runs. `status`,
//! `sessions`, `show`, `agent`, `events`, `doctor` and `shutdown` never do.

use crate::fmt;
use crate::inspect::{self, Live, Target};
use crate::lifecycle;
use crate::outln;
use crate::proto::*;
use crate::task;
use interprocess::local_socket::tokio::prelude::*; // trait for Stream::connect
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{ReadHalf, WriteHalf};

/// How this client presents itself in the hello handshake (§3.3).
pub enum HelloMode {
    Cli,
    /// CLI `start` needs a session to own the task (start is session-bound).
    Extension { session_id: String },
}

pub struct Conn {
    rd: ReadHalf<Stream>,
    wr: WriteHalf<Stream>,
}

// Framing violations are permanent protocol failures, not disconnections.
const PROTOCOL_FRAMING_PREFIX: &str = "protocol framing: ";

impl Conn {
    /// Send one request, wait for its matching response (events and other
    /// frames are skipped — a short-lived CLI has no event consumer).
    pub async fn roundtrip<T: DeserializeOwned>(&mut self, kind: RequestKind) -> Result<T, String> {
        let id = new_request_id();
        let req = Request {
            v: Some(PROTO_VERSION),
            id: id.clone(),
            kind,
        };
        write_frame(&mut self.wr, &encode(&req))
            .await
            .map_err(|e| format!("send: {e}"))?;
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(30), read_frame(&mut self.rd))
                .await
                .map_err(|_| "response timed out".to_string())?
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        format!("{PROTOCOL_FRAMING_PREFIX}{e}")
                    } else {
                        format!("recv: {e}")
                    }
                })?
                .ok_or_else(|| "manager closed the connection".to_string())?;
            let v: serde_json::Value =
                serde_json::from_slice(&frame).map_err(|e| format!("bad response JSON: {e}"))?;
            if v.get("id").and_then(|x| x.as_str()) != Some(id.as_str()) {
                continue; // event or unrelated frame
            }
            if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
                let resp: Response<T> = serde_json::from_value(v)
                    .map_err(|e| format!("bad response payload: {e}"))?;
                return Ok(resp.body);
            }
            let err: Response<ErrorBody> =
                serde_json::from_value(v).map_err(|e| format!("bad error payload: {e}"))?;
            return Err(format!("{}: {}", err.body.error.code, err.body.error.message));
        }
    }
}

/// Every task record the daemon holds (for `session_id`, or all), fetched
/// in frame-sized pages. A daemon without paging answers in one frame and
/// sends no `next`, so this also works against it.
pub async fn list_tasks(conn: &mut Conn, session_id: Option<String>) -> Result<Vec<TaskRecord>, String> {
    let mut tasks = Vec::new();
    let mut after = None;
    loop {
        let page: ListOk = conn
            .roundtrip(RequestKind::List {
                all: true,
                session_id: session_id.clone(),
                paged: true,
                after: after.take(),
            })
            .await?;
        tasks.extend(page.tasks);
        match page.next {
            Some(n) => after = Some(n),
            None => return Ok(tasks),
        }
    }
}

/// A CLI connection that outlives an in-place upgrade of the daemon, which
/// closes every connection and leaves requests in flight unanswered. Use it
/// only for idempotent requests (`output`, `wait`, `list`, `status`): one
/// whose connection dropped is resent once on a fresh connection.
pub struct Resilient {
    home: PathBuf,
    conn: Conn,
}

impl Resilient {
    pub async fn connect(home: &Path) -> Result<Resilient, String> {
        let conn = match connect(home, &HelloMode::Cli).await {
            // Closed during the hello: an upgrade is under way.
            Err(e) if is_disconnect(&e) => reconnect(home).await?,
            r => r?,
        };
        Ok(Resilient { home: home.to_path_buf(), conn })
    }

    pub async fn call<T: DeserializeOwned>(&mut self, kind: RequestKind) -> Result<T, String> {
        match self.conn.roundtrip(kind.clone()).await {
            Err(e) if is_disconnect(&e) => {
                self.conn = reconnect(&self.home).await.map_err(|re| format!("{e}; reconnect: {re}"))?;
                self.conn.roundtrip(kind).await
            }
            r => r,
        }
    }
}

/// `inspect::snapshot`, taken again if an in-place upgrade closed the
/// connection under it.
async fn snapshot_resilient(home: &Path) -> Result<inspect::Snapshot, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match inspect::snapshot(home, Live::Spawn).await {
            Err(e) if is_disconnect(&e) && std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            r => return r,
        }
    }
}

fn is_disconnect(e: &str) -> bool {
    e == "manager closed the connection" || e.starts_with("send:") || e.starts_with("recv:")
}

/// Reconnect to the daemon that just closed our connection. During an
/// upgrade the socket stays bound, so this succeeds as soon as the new image
/// accepts; allow the whole preflight-free part of an upgrade (a few s).
async fn reconnect(home: &Path) -> Result<Conn, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match connect_existing(home, &HelloMode::Cli).await {
            Ok(c) => return Ok(c),
            Err(e) if std::time::Instant::now() > deadline => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Connect + spawn flow (§3.1)
// ---------------------------------------------------------------------------

/// Connect to a running daemon and say hello; never spawns one.
pub async fn connect_existing(home: &Path, mode: &HelloMode) -> Result<Conn, String> {
    let sock = lifecycle::socket_path(home);
    let name = sock
        .as_os_str()
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| format!("bad socket path: {e}"))?;
    let stream = Stream::connect(name)
        .await
        .map_err(|e| format!("connect {}: {e}", sock.display()))?;
    let (rd, wr) = tokio::io::split(stream);
    let mut conn = Conn { rd, wr };
    let hello = match mode {
        HelloMode::Cli => RequestKind::Hello {
            client_kind: ClientKind::Cli,
            session_id: None,
            pi_pid: None,
            cwd: None,
            extension_version: None,
            protocol: Some(PROTOCOL),
        },
        HelloMode::Extension { session_id } => RequestKind::Hello {
            client_kind: ClientKind::Extension,
            session_id: Some(session_id.clone()),
            pi_pid: Some(std::process::id()),
            cwd: std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()),
            extension_version: Some(format!("pi-famulus-cli/{}", crate::VERSION)),
            protocol: Some(PROTOCOL),
        },
    };
    let _: HelloOk = conn.roundtrip(hello).await?;
    Ok(conn)
}

/// Wait for a lifetime-lock owner without spawning against it. A pid
/// can remain a zombie or be reused, and concurrent clients may miss the
/// brief unlocked interval when another client starts the successor. A
/// successful hello therefore also ends the wait: it proves that a serving
/// manager is ready. Bound each connect + hello probe so a stalled socket
/// cannot hide a released lock or a ready successor until the outer startup
/// deadline, which still bounds the entire wait.
async fn wait_for_manager_exit(home: &Path, mode: &HelloMode) -> Result<Option<Conn>, String> {
    while lifecycle::lock_held(home) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Use the same per-attempt budget as socket readiness; don't let
        // one hello consume the whole 15s startup deadline.
        match tokio::time::timeout(Duration::from_secs(2), connect_existing(home, mode)).await {
            Ok(Ok(conn)) => return Ok(Some(conn)),
            Ok(Err(e)) if is_shutting_down(&e) || is_disconnect(&e) || e.starts_with("connect ") => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {} // Recheck the lifetime lock after a stalled probe.
        }
    }
    Ok(None)
}

fn is_shutting_down(e: &str) -> bool {
    e.strip_prefix("E_INTERNAL: ") == Some(SHUTTING_DOWN)
}

async fn wait_for_socket(home: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let sock = lifecycle::socket_path(home);
        if let Ok(name) = sock.as_os_str().to_fs_name::<GenericFilePath>() {
            if Stream::connect(name).await.is_ok() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Spawn `pi-famulus daemon` detached (own session, output to manager.log)
/// so it outlives this short-lived CLI process (§3.1 step 3).
fn spawn_daemon(home: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(lifecycle::log_path(home))
        .map_err(|e| format!("open manager.log: {e}"))?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--home")
        .arg(home)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    crate::sys::apply_new_session_std(&mut cmd);
    cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    Ok(())
}

/// §3.1 client startup flow: connect; on failure take the spawn lock and
/// spawn (or wait for the in-progress spawn), then retry once. A zombie
/// socket is handled the same way: the spawned daemon, holding manager.lock,
/// removes it. Shutdown refusals can occur on either hello; a retiring
/// manager can also remove its endpoint before releasing the lifetime lock.
/// Wait for that lock before spawning a successor. Bound the whole flow, not
/// just each wait, so a stuck or repeatedly shutting-down daemon cannot
/// keep a CLI alive indefinitely.
pub async fn connect(home: &Path, mode: &HelloMode) -> Result<Conn, String> {
    tokio::time::timeout(Duration::from_secs(15), connect_with_retry(home, mode))
        .await
        .map_err(|_| "cannot reach pi-famulus: manager did not become ready within 15s".to_string())?
}

async fn connect_with_retry(home: &Path, mode: &HelloMode) -> Result<Conn, String> {
    let mut spawned = false;
    loop {
        let last_err = match connect_existing(home, mode).await {
            Ok(c) => return Ok(c),
            Err(e) => e,
        };
        if last_err.starts_with(PROTOCOL_FRAMING_PREFIX) {
            return Err(format!("cannot reach pi-famulus: {last_err}"));
        }
        if is_shutting_down(&last_err)
            || ((is_disconnect(&last_err) || last_err.starts_with("connect "))
                && lifecycle::lock_held(home))
        {
            if let Some(conn) = wait_for_manager_exit(home, mode)
                .await
                .map_err(|e| format!("cannot reach pi-famulus: {e}"))?
            {
                return Ok(conn);
            }
            // A shutdown refusal on the post-spawn hello is not a failed
            // successor: readiness may have probed the retiring socket.
            // Reconnect first: another waiting client may have spawned it.
            spawned = false;
            continue;
        }
        if spawned {
            return Err(format!("cannot reach pi-famulus: {last_err}"));
        }
        spawned = true;
        // Clients never delete socket/pid files: only the daemon holding
        // manager.lock may (§3.1). A client cleaning up here could unlink the
        // socket of a daemon another client just spawned.
        // Steps 2–4: spawn lock; winner spawns, losers wait for the socket.
        std::fs::create_dir_all(home).map_err(|e| format!("create {}: {e}", home.display()))?;
        match lifecycle::try_acquire_spawn_lock(home) {
            Ok(Some(guard)) => {
                spawn_daemon(home)?;
                let ok = wait_for_socket(home, Duration::from_secs(2)).await;
                drop(guard); // release the spawn lock (§3.1 step 3)
                if !ok {
                    return Err("cannot reach pi-famulus: spawned daemon did not create its socket within 2s".into());
                }
            }
            Ok(None) => {
                // Someone else is spawning; just wait.
                if !wait_for_socket(home, Duration::from_secs(2)).await {
                    return Err("cannot reach pi-famulus: another client is spawning the manager, but it did not come up".into());
                }
            }
            Err(e) => return Err(format!("cannot reach pi-famulus: spawn lock: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Commands that act on the daemon
// ---------------------------------------------------------------------------

/// `output`: task bytes through the protocol cursor. `max_bytes` caps the
/// total printed (never more; a character that would cross it is left out).
/// For an agent id, prints the agent's result tail.
pub async fn cmd_output(home: &Path, typed: &str, follow: bool, max_bytes: Option<u64>) -> Result<(), String> {
    let snap = snapshot_resilient(home).await?;
    let task_id = match inspect::resolve(&snap, typed)? {
        Target::Task(t) => t.task_id,
        Target::Agent(a) => {
            let text = a.result_tail.unwrap_or_default();
            let text = match max_bytes {
                Some(m) => cut_utf8(&text, m as usize).to_string(),
                None => text,
            };
            crate::out::bytes(text.as_bytes());
            return Ok(());
        }
        Target::Run(r, _) => return Err(format!("{r} is a run; pick one of its children (`show {r}`)")),
    };
    let mut conn = Resilient::connect(home).await?;
    let mut cursor = 0u64;
    let mut remaining = max_bytes;
    loop {
        let want = match remaining {
            Some(0) => break,
            Some(r) => r.min(65536),
            None => 65536,
        };
        let resp: OutputOk = conn
            .call(RequestKind::Output {
                task_id: task_id.clone(),
                cursor,
                max_bytes: want,
            })
            .await?;
        // The daemon may return a whole character slightly past `want`
        // (§3.3 progress rule); never print past the user's cap.
        let chunk = match remaining {
            Some(r) => cut_utf8(&resp.chunk, r as usize),
            None => resp.chunk.as_str(),
        };
        crate::out::bytes(chunk.as_bytes());
        if let Some(r) = remaining.as_mut() {
            *r -= chunk.len() as u64;
            if chunk.len() < resp.chunk.len() {
                break;
            }
        }
        cursor = resp.next_cursor;
        let caught_up = cursor >= resp.total_size;
        if resp.status.is_terminal() && caught_up {
            break;
        }
        if !follow && caught_up {
            break;
        }
        if resp.chunk.is_empty() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    Ok(())
}

/// Longest prefix of `s` of at most `max` bytes ending on a char boundary.
fn cut_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

pub async fn cmd_stop(home: &Path, typed: &str) -> Result<(), String> {
    let snap = inspect::snapshot(home, Live::Spawn).await?;
    let t = match inspect::resolve(&snap, typed)? {
        Target::Task(t) => t,
        Target::Agent(_) | Target::Run(..) => {
            return Err("agents run inside pi; stop from /tasks or ask the agent".into())
        }
    };
    let mut conn = connect(home, &HelloMode::Cli).await?;
    // Sent even for a finished task: the daemon then clears any leftover
    // process group it still tracks, without changing the record.
    let _: UnitOk = conn
        .roundtrip(RequestKind::Stop {
            task_id: t.task_id.clone(),
            reason: Some("cli".into()),
        })
        .await?;
    if t.status.is_terminal() {
        let why = t.end_reason.clone().unwrap_or_else(|| {
            serde_json::to_value(t.status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
        });
        outln!("{} already finished ({why})", t.task_id);
    } else {
        outln!("stopped {}", t.task_id);
    }
    Ok(())
}

/// kill-session is built from list + stop so the wire protocol stays exactly
/// as documented (§3.3 has no cross-session shutdown message).
pub async fn cmd_kill_session(home: &Path, session_id: &str) -> Result<(), String> {
    let mut conn = connect(home, &HelloMode::Cli).await?;
    let tasks = list_tasks(&mut conn, Some(session_id.to_string())).await?;
    let mut stopped = 0usize;
    for t in tasks {
        if t.status == TaskStatus::Running {
            let _: UnitOk = conn
                .roundtrip(RequestKind::Stop {
                    task_id: t.task_id.clone(),
                    reason: Some("cli".into()),
                })
                .await?;
            outln!("stopped {}", t.task_id);
            stopped += 1;
        }
    }
    outln!("session {session_id}: stopped {stopped} task(s)");
    Ok(())
}

pub async fn cmd_shutdown(home: &Path) -> Result<(), String> {
    let Ok(mut conn) = connect_existing(home, &HelloMode::Cli).await else {
        outln!("pi-famulus is not running");
        return Ok(());
    };
    let _: UnitOk = conn.roundtrip(RequestKind::Shutdown).await?;
    outln!("manager shutting down");
    Ok(())
}

/// Whether two paths name the same file on disk, not just the same string.
/// `current_exe()` does not always resolve symlinks (a symlinked install
/// dir, `/tmp` vs `/private/tmp` on macOS, `..` segments), so comparing raw
/// strings can call the same binary "not this CLI" over nothing but
/// spelling. A path that cannot be canonicalized (deleted, unreadable) is
/// compared as given rather than dropped, so the note still fires when the
/// two really do differ.
fn same_file_by_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// `pi-famulus upgrade`: ask the running daemon to exec the binary now at
/// its path, then report how it went (from the new image's status).
pub async fn cmd_upgrade(home: &Path) -> Result<(), String> {
    let Ok(mut conn) = connect_existing(home, &HelloMode::Cli).await else {
        outln!("pi-famulus is not running; the next client starts the installed binary");
        return Ok(());
    };
    let before: StatusOk = conn.roundtrip(RequestKind::Status).await?;
    if before.protocol < PROTOCOL_UPGRADE {
        return Err(format!(
            "the running manager (pid {}, {}, protocol {}) predates in-place upgrade, so it cannot upgrade itself. \
             Restart it once: `pi-famulus shutdown` (stops its running tasks); the next client starts the installed binary",
            before.pid, before.version, before.protocol
        ));
    }
    // The daemon execs the file at its own path, not this CLI: say so when
    // they differ (a fresh target/release against the installed daemon).
    let me = crate::handover::exe_path().ok();
    if let (Some(exe), Some(me)) = (&before.exe, &me) {
        if !same_file_by_path(Path::new(exe), me.as_path()) {
            eprintln!(
                "note: the manager upgrades to the file at its own path, {exe}, not this CLI ({}, {}). \
                 To run this build, install it there first",
                me.display(),
                crate::VERSION
            );
        }
    }
    let asked = now_ms();
    let ok: UpgradeOk = conn.roundtrip(RequestKind::Upgrade).await?;
    drop(conn);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::time::Instant::now() > deadline {
            return Err("no answer from the manager within 30s of the upgrade".into());
        }
        if !lifecycle::lock_held(home) {
            return Err(format!(
                "the manager (pid {}) exited during the upgrade; its tasks were cleaned up. See manager.log",
                before.pid
            ));
        }
        let Ok(mut c) = connect_existing(home, &HelloMode::Cli).await else { continue };
        let Ok(st) = c.roundtrip::<StatusOk>(RequestKind::Status).await else { continue };
        if st.pid != before.pid {
            return Err(format!(
                "the manager was replaced by a new process (pid {} -> {}): the upgrade did not carry over",
                before.pid, st.pid
            ));
        }
        match st.last_upgrade {
            Some(u) if u.at >= asked && u.ok && st.generation > ok.generation => {
                outln!(
                    "upgraded in place: {} -> {} (pid {}, generation {}, {} running task(s) kept)",
                    u.from_version,
                    u.to_version.as_deref().unwrap_or("?"),
                    st.pid,
                    st.generation,
                    st.task_counts.running
                );
                return Ok(());
            }
            Some(u) if u.at >= asked && !u.ok => {
                return Err(format!(
                    "upgrade not done, still running {}: {}",
                    u.from_version,
                    u.error.unwrap_or_default()
                ));
            }
            _ => continue,
        }
    }
}

/// Extra CLI convenience (not in §3.5): start a task. Needs a session to own
/// the task, so this hellos as an extension connection.
pub async fn cmd_start(
    home: &Path,
    session: &str,
    kind: &str,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    background: bool,
    command: &str,
) -> Result<(), String> {
    let kind = match kind {
        "shell" => TaskKind::Shell,
        "monitor" => TaskKind::Monitor,
        other => return Err(format!("unknown kind {other:?} (expected shell|monitor)")),
    };
    let cwd = match cwd {
        Some(c) => Some(c),
        None => Some(
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|e| e.to_string())?,
        ),
    };
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let mut conn = connect(
        home,
        &HelloMode::Extension {
            session_id: session.to_string(),
        },
    )
    .await?;
    let res: StartOk = conn
        .roundtrip(RequestKind::Start {
            kind,
            command: command.to_string(),
            cwd,
            env,
            run_in_background: background,
            timeout_ms,
            origin: None,
            key: None,
        })
        .await?;
    outln!("task_id={} pid={}", res.task_id, res.pid);
    Ok(())
}

/// Extra CLI convenience (not in §3.5): budget-wait on a task or an agent.
pub async fn cmd_wait(home: &Path, typed: &str, budget_ms: u64) -> Result<(), String> {
    let snap = snapshot_resilient(home).await?;
    let task_id = match inspect::resolve(&snap, typed)? {
        Target::Task(t) => t.task_id,
        Target::Agent(a) => return inspect::wait_agent(home, &a.child_id, budget_ms).await,
        Target::Run(r, _) => return Err(format!("{r} is a run; wait on one of its children")),
    };
    let mut conn = Resilient::connect(home).await?;
    let res: WaitOk = conn
        .call(RequestKind::Wait {
            task_id: task_id.to_string(),
            budget_ms,
        })
        .await?;
    if res.done {
        match res.exit_code {
            Some(c) => outln!("done exit_code={c}"),
            None => outln!("done exit_code=null"),
        }
    } else {
        outln!("not done (budget expired; task still running)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// log / tail
// ---------------------------------------------------------------------------

/// `log`: manager.log (timestamps rendered as local time), or a task's
/// output file, or (for an agent id) the agent's transcript.
pub async fn cmd_log(
    home: &Path,
    follow: bool,
    lines: usize,
    task_id: Option<&str>,
    stderr: bool,
) -> Result<(), String> {
    match task_id {
        None => {
            if stderr {
                return Err("--stderr requires a TASK_ID (manager.log has no stderr stream)".into());
            }
            let path = lifecycle::log_path(home);
            tail_file(&path, follow, lines, true).await
        }
        Some(id) => cmd_log_task(home, id, follow, lines, stderr).await,
    }
}

async fn cmd_log_task(
    home: &Path,
    typed: &str,
    follow: bool,
    lines: usize,
    stderr: bool,
) -> Result<(), String> {
    let snap = inspect::snapshot(home, Live::IfRunning).await?;
    let task = match inspect::resolve(&snap, typed)? {
        Target::Task(t) => t,
        Target::Agent(a) => {
            if stderr {
                return Err("--stderr applies to shell/monitor tasks; agents have a transcript only".into());
            }
            return inspect::print_transcript(home, &a, false, follow, lines).await;
        }
        Target::Run(r, _) => return Err(format!("{r} is a run; pick one of its children (`show {r}`)")),
    };
    let task_id = task.task_id.clone();
    let path = if stderr {
        task::stderr_path_for(Path::new(&task.output_path))
    } else {
        PathBuf::from(&task.output_path)
    };
    if !path.exists() {
        if stderr {
            return Err(format!(
                "no stderr file for {task_id} yet (path {}); task may have written nothing to stderr, or it was started before stderr capture was added — restart the task",
                path.display()
            ));
        }
        return Err(format!("no output file at {}", path.display()));
    }
    tail_file(&path, follow, lines, false).await
}

/// Resolve a user-typed id against the known set.
/// Accepts exact match, unique suffix/prefix/substring, and unique near-miss
/// (Levenshtein ≤ 2) so typos like `cmon_…` for `mon_…` still work.
/// Not found: one line, with the closest known id only.
pub fn resolve_task_id(typed: &str, known: &[String]) -> Result<String, String> {
    if known.iter().any(|k| k == typed) {
        return Ok(typed.to_string());
    }
    let hits: Vec<&String> = known
        .iter()
        .filter(|k| {
            k.ends_with(typed)
                || typed.ends_with(k.as_str())
                || k.contains(typed)
                || typed.contains(k.as_str())
        })
        .collect();
    if hits.len() == 1 {
        eprintln!("note: resolved '{typed}' → '{}'", hits[0]);
        return Ok(hits[0].clone());
    }
    if hits.len() > 1 {
        let shown: Vec<&str> = hits.iter().take(5).map(|s| s.as_str()).collect();
        let more = if hits.len() > 5 { format!(" and {} more", hits.len() - 5) } else { String::new() };
        return Err(format!("ambiguous id '{typed}' (matches {}{more})", shown.join(", ")));
    }
    let near: Vec<&String> = known.iter().filter(|k| edit_distance(typed, k) <= 2).collect();
    if near.len() == 1 {
        eprintln!("note: resolved '{typed}' → '{}'", near[0]);
        return Ok(near[0].clone());
    }
    let closest = known.iter().min_by_key(|k| edit_distance(typed, k));
    Err(match closest {
        Some(k) => format!("unknown id '{typed}' (did you mean '{k}'?)"),
        None => format!("unknown id '{typed}' (no tasks or agents yet)"),
    })
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// manager.log lines start with `[<epoch ms>]`; show local time instead.
pub fn humanize_log_line(line: &str) -> String {
    if let Some(rest) = line.strip_prefix('[') {
        if let Some((ts, msg)) = rest.split_once(']') {
            if let Ok(ms) = ts.parse::<u64>() {
                return format!("[{}]{msg}", fmt::datetime_ms(ms));
            }
        }
    }
    line.to_string()
}

async fn tail_file(path: &Path, follow: bool, lines: usize, humanize: bool) -> Result<(), String> {
    let render = |l: &str| if humanize { humanize_log_line(l) } else { l.to_string() };
    let data = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&data);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    for line in &all[start..] {
        outln!("{}", render(line));
    }
    if !follow {
        return Ok(());
    }
    let mut offset = data.len() as u64;
    let mut pending = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if size < offset {
            offset = 0; // truncated/rotated
        }
        if size > offset {
            let (bytes, next) = task::read_file_range(path, offset, (size - offset) as usize)
                .map_err(|e| e.to_string())?;
            offset = next;
            if humanize {
                pending.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(i) = pending.find('\n') {
                    let l: String = pending.drain(..=i).collect();
                    outln!("{}", render(l.trim_end_matches('\n')));
                }
            } else {
                crate::out::bytes(&bytes);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    failures: usize,
}

impl Report {
    fn ok(&mut self, check: &str, detail: impl AsRef<str>) {
        outln!("ok    {check}: {}", detail.as_ref());
    }
    fn fixed(&mut self, check: &str, detail: impl AsRef<str>) {
        outln!("fixed {check}: {}", detail.as_ref());
    }
    fn warn(&mut self, check: &str, detail: impl AsRef<str>) {
        outln!("warn  {check}: {}", detail.as_ref());
    }
    fn fail(&mut self, check: &str, detail: impl AsRef<str>) {
        self.failures += 1;
        outln!("FAIL  {check}: {}", detail.as_ref());
    }
}

/// Longest unix socket path the platform accepts (sun_path minus NUL).
const MAX_SOCKET_PATH: usize = if cfg!(target_os = "macos") { 103 } else { 107 };
/// Sessions dir size that deserves a warning (events.jsonl has no rotation).
const SESSIONS_WARN_BYTES: u64 = 100 * 1024 * 1024;

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// §3.5 doctor: health checks. Stale socket/pid files are fixed (only while
/// holding manager.lock); everything else is reported. Exit status 1 when
/// any check fails.
pub async fn cmd_doctor(home: &Path) -> i32 {
    let mut r = Report::default();
    outln!("home:   {}", home.display());
    outln!("socket: {}", lifecycle::socket_path(home).display());
    outln!("lock:   {}", lifecycle::daemon_lock_path(home).display());
    if !home.is_dir() {
        r.fail("home", "does not exist (nothing has run with this --home / PI_FAMULUS_HOME)");
        outln!("{} problem(s) found", r.failures);
        return 1;
    }
    r.ok("home", "exists");

    let sock_len = lifecycle::socket_path(home).as_os_str().len();
    if sock_len > MAX_SOCKET_PATH {
        r.fail("socket path", format!("{sock_len} bytes, longer than the {MAX_SOCKET_PATH}-byte unix socket limit; use a shorter home"));
    } else {
        r.ok("socket path", format!("{sock_len} bytes"));
    }

    // config.json (optional) and the manager path it or the env configures.
    let cfg_path = home.join("config.json");
    let mut manager_path: Option<(String, &str)> = std::env::var("PI_FAMULUS_MANAGER_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|p| (p, "PI_FAMULUS_MANAGER_PATH"));
    match std::fs::read(&cfg_path) {
        Err(_) => r.ok("config.json", "absent (defaults)"),
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Err(e) => r.fail("config.json", format!("does not parse: {e}")),
            Ok(v) => {
                r.ok("config.json", "parses");
                if manager_path.is_none() {
                    if let Some(p) = v.get("managerPath").and_then(|p| p.as_str()) {
                        manager_path = Some((p.to_string(), "config.json managerPath"));
                    }
                }
            }
        },
    }
    match crate::gc::retention_ms(home) {
        Ok(ms) => r.ok("session retention", format!("gone sessions kept {}", crate::fmt::human_duration(ms))),
        Err(e) => r.fail("session retention", format!("{e}; the daemon uses the default 24h")),
    }
    match crate::gc::task_retention_ms(home) {
        Ok(ms) => r.ok("task retention", format!("finished tasks kept {}", crate::fmt::human_duration(ms))),
        Err(e) => r.fail("task retention", format!("{e}; the daemon uses the default 24h")),
    }
    if let Some((p, source)) = &manager_path {
        if Path::new(p).is_file() {
            r.ok("manager path", format!("{p} ({source})"));
        } else {
            r.fail("manager path", format!("{p} from {source} does not exist; the extension ignores it"));
        }
    }

    match lifecycle::read_pid_file(home) {
        Some(pf) => outln!("pid file: pid={} version={} started_at={}", pf.pid, pf.version, pf.started_at),
        None => outln!("pid file: absent"),
    }
    // Liveness is the daemon's lifetime lock on manager.lock, not the pid
    // (the pid may have been reused by an unrelated process).
    let mut status: Option<StatusOk> = None;
    let daemon_running = match lifecycle::clean_if_no_daemon(home) {
        Ok(None) => {
            match connect_existing(home, &HelloMode::Cli).await {
                Ok(mut c) => match c.roundtrip::<StatusOk>(RequestKind::Status).await {
                    Ok(st) => {
                        r.ok("daemon", format!("running, pid {}, hello ok", st.pid));
                        status = Some(st);
                    }
                    Err(e) => r.fail("daemon", format!("holds the lock but status failed: {e}")),
                },
                Err(e) => r.fail("daemon", format!("holds the lock but the socket is NOT responding ({e}); not cleaning")),
            }
            true
        }
        Ok(Some(removed)) => {
            if removed.is_empty() {
                r.ok("daemon", "not running; no stale files");
            }
            for p in &removed {
                r.fixed("daemon", format!("not running; removed stale {}", p.display()));
            }
            false
        }
        Err(e) => {
            r.fail("daemon", format!("lock check failed: {e}"));
            false
        }
    };

    // Protocol per connected session.
    let connected: HashSet<String> = status
        .as_ref()
        .map(|st| st.sessions.iter().filter(|s| s.connected).map(|s| s.session_id.clone()).collect())
        .unwrap_or_default();
    if let Some(st) = &status {
        for s in st.sessions.iter().filter(|s| s.connected) {
            match s.protocol {
                Some(p) if p == st.protocol => r.ok("protocol", format!("session {} speaks {p}", s.session_id)),
                Some(p) => r.fail(
                    "protocol",
                    format!("session {} speaks {p}, manager speaks {}; update the extension or the manager", s.session_id, st.protocol),
                ),
                None => r.fail(
                    "protocol",
                    format!("session {} did not announce a protocol (older extension); manager speaks {}", s.session_id, st.protocol),
                ),
            }
        }
    }

    // Stale agent records: running according to the file, session gone.
    for a in inspect::load_agent_records(home, &connected).iter().filter(|a| a.stale) {
        r.fail("agent record", format!("{} says running but session {} is gone", a.child_id, a.session_id));
    }

    // Orphan pids: task records still `running` with no daemon to own them.
    if !daemon_running {
        for t in crate::registry::load_all_records(home) {
            if t.status != TaskStatus::Running {
                continue;
            }
            if task::pid_alive(t.pid) {
                r.fail("orphan pid", format!("{} (pid {}) still runs with no manager: its runner should have taken it down, or the pid now belongs to another process. The next manager marks it orphaned without signalling it; check `ps -p {}` and kill it if it is the task", t.task_id, t.pid, t.pid));
            } else {
                r.ok("dead task", format!("{} will be marked orphaned when the manager starts", t.task_id));
            }
        }
    }

    let size = dir_size(&home.join("sessions"));
    if size > SESSIONS_WARN_BYTES {
        r.warn("sessions dir", format!("{} MiB (no rotation yet; consider pruning old sessions)", size >> 20));
    } else {
        r.ok("sessions dir", format!("{} KiB", size >> 10));
    }

    if r.failures == 0 {
        outln!("ok");
        0
    } else {
        outln!("{} problem(s) found", r.failures);
        1
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::{cut_utf8, humanize_log_line, resolve_task_id, same_file_by_path};

    #[test]
    fn exact_and_typo_prefix() {
        let known = vec!["mon_e1351cb1".into(), "sh_071f52c1".into()];
        assert_eq!(resolve_task_id("mon_e1351cb1", &known).unwrap(), "mon_e1351cb1");
        assert_eq!(resolve_task_id("cmon_e1351cb1", &known).unwrap(), "mon_e1351cb1");
        assert_eq!(resolve_task_id("e1351cb1", &known).unwrap(), "mon_e1351cb1");
    }

    #[test]
    fn unknown_is_one_line_with_closest_only() {
        let known: Vec<String> = vec!["mon_e1351cb1".into(), "sh_aaaaaaaa".into(), "ch_bbbbbbbb".into()];
        let err = resolve_task_id("mon_e1351zzz", &known).unwrap_err();
        assert_eq!(err, "unknown id 'mon_e1351zzz' (did you mean 'mon_e1351cb1'?)");
        assert!(!err.contains("sh_aaaaaaaa"), "must not dump every id: {err}");
        let err = resolve_task_id("x", &[]).unwrap_err();
        assert!(!err.contains('\n'));
    }

    #[test]
    fn helpers() {
        assert_eq!(cut_utf8("a中b", 3), "a");
        assert_eq!(cut_utf8("a中b", 4), "a中");
        assert_eq!(cut_utf8("ab", 9), "ab");
        let l = humanize_log_line("[1790000000123] daemon started");
        assert!(l.ends_with("] daemon started") && l.contains(".123]") && !l.contains("1790000000123"), "{l}");
        assert_eq!(humanize_log_line("plain"), "plain");
    }

    #[test]
    fn same_file_by_path_follows_symlinks() {
        let dir = std::env::temp_dir().join(format!("pi-famulus-same-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(&real, b"x").unwrap();
        let link = dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_file_by_path(&real, &link), "a symlink and its target are the same file");
        let other = dir.join("other");
        std::fs::write(&other, b"y").unwrap();
        assert!(!same_file_by_path(&real, &other), "two distinct files are not the same file");
        let missing = dir.join("missing");
        assert!(!same_file_by_path(&real, &missing), "a real file differs from one that does not exist");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
