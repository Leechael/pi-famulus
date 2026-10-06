//! Daemon core (design doc §3.1–§3.4): unix socket listener, accept loop,
//! hello handshake + connection registry, request dispatch, event fanout, and
//! the anti-zombie lifecycle — zero active connections for 5s -> graceful
//! shutdown (SIGTERM all task groups -> 2s -> SIGKILL -> clean socket/pid).

use crate::lifecycle::{self, Claim};
use crate::proto::*;
use crate::registry::{self, Access, Registry, TaskEntry};
use crate::task;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// §3.2: zero active connections for this long -> graceful shutdown.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(5);
/// §3.2/§3.3: SIGTERM grace before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Hygiene: a connection must complete hello within this window (it does not
/// count as an active connection until then).
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Server-side cap on raw bytes for one output read.
const MAX_OUTPUT_READ: u64 = 1024 * 1024;
/// Cap on a chunk's JSON-escaped size (control bytes escape to 6 bytes each),
/// leaving room for the response envelope inside the 4 MiB frame.
const CHUNK_JSON_BUDGET: usize = MAX_FRAME_SIZE as usize - 64 * 1024;
/// Poll interval for a process group that outlived its runner (fallback
/// only: normally the runner guards its group and its exit says "empty").
const GROUP_POLL: Duration = Duration::from_millis(500);

type OutTx = mpsc::Sender<OutFrame>;

/// One frame queued for a connection's writer. Output events say which
/// task and cursor they carry, so the writer can record how far the client
/// really got (`ConnHandle::written`).
pub struct OutFrame {
    bytes: Arc<Vec<u8>>,
    output: Option<(String, u64)>,
}

impl OutFrame {
    fn plain(bytes: Arc<Vec<u8>>) -> OutFrame {
        OutFrame {
            bytes,
            output: None,
        }
    }
    fn output(bytes: Arc<Vec<u8>>, task_id: &str, next_cursor: u64) -> OutFrame {
        OutFrame {
            bytes,
            output: Some((task_id.to_string(), next_cursor)),
        }
    }
}

/// Per connection: the output cursor it has been written up to, per watched
/// task (set to where the watch started, then advanced by the writer).
type Written = Arc<Mutex<HashMap<String, u64>>>;
pub type Shared = Arc<Mutex<DaemonState>>;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct ConnHandle {
    pub kind: ClientKind,
    pub session_id: Option<String>,
    pub tx: OutTx,
    /// Fired to make the connection's read loop exit (session rebind).
    pub die: Arc<Notify>,
    pub written: Written,
    /// Its writer task, aborted when an upgrade cannot wait for it to flush.
    pub writer: tokio::task::AbortHandle,
}

pub struct SessionEntry {
    pub pi_pid: u32,
    pub conn_id: Option<u64>,
    pub cwd: Option<String>,
    pub extension_version: Option<String>,
    pub protocol: Option<u32>,
    pub connected_at: u64,
    pub last_seen: u64,
}

pub struct PendingAgent {
    session_id: String,
    child_id: String,
    work_kind: String,
    request_id: String,
    tx: OutTx,
}

pub struct DaemonState {
    pub home: PathBuf,
    pub started_at_ms: u64,
    pub registry: Registry,
    /// Active (hello-completed) connections; the §3.2 idle rule counts these.
    pub conns: HashMap<u64, ConnHandle>,
    pub sessions: HashMap<String, SessionEntry>,
    pub next_conn_id: u64,
    pub idle_timer: Option<tokio::task::JoinHandle<()>>,
    pub shutdown: bool,
    pub shutdown_notify: Arc<Notify>,
    /// Time source for the daemon's own timers (see `clock.rs`).
    pub clock: crate::clock::Clock,
    /// Turned true to park every task's pumps and exit watch between two
    /// reads, so their state sits in the entries (an in-place upgrade).
    pub park_tx: tokio::sync::watch::Sender<bool>,
    /// An in-place upgrade is quiescing: no idle shutdown, no new requests.
    pub upgrading: bool,
    /// Right after an in-place upgrade: every client is reconnecting, so the
    /// zero-connection idle shutdown must not fire yet.
    pub hold_idle: bool,
    pub foreground: bool,
    /// Requests being served, and connection writers still flushing (an
    /// upgrade waits for both).
    pub inflight: Arc<std::sync::atomic::AtomicUsize>,
    pub writers: Arc<std::sync::atomic::AtomicUsize>,
    /// In-place upgrades this pid has gone through, and the latest attempt.
    pub generation: u32,
    pub last_upgrade: Option<UpgradeInfo>,
    /// Asks the accept loop to upgrade in place (the trigger is kept next
    /// to it).
    pub upgrade_notify: Arc<Notify>,
    /// Asked for and being preflighted, or preflighted and ready.
    pub upgrade_pending: bool,
    pub upgrade_ready: Option<crate::handover::Ready>,
    /// Recent `start` request keys ("<session>\0<key>", task id), oldest
    /// first: a retried start returns the task it already started.
    pub start_keys: std::collections::VecDeque<(String, String)>,
    /// Agent permits keyed by session and child id.
    pub agent_permits: HashMap<(String, String), String>,
    /// FIFO requests waiting for a per-kind slot. Responses retain the original
    /// request id so normal client multiplexing handles the daemon-initiated wake.
    pub pending_agents: std::collections::VecDeque<PendingAgent>,
}

/// Bound on remembered start keys.
const START_KEYS_MAX: usize = 512;

impl DaemonState {
    fn new(home: PathBuf, registry: Registry, foreground: bool) -> DaemonState {
        DaemonState {
            home,
            started_at_ms: now_ms(),
            registry,
            conns: HashMap::new(),
            sessions: HashMap::new(),
            next_conn_id: 1,
            idle_timer: None,
            shutdown: false,
            shutdown_notify: Arc::new(Notify::new()),
            clock: crate::clock::Clock::from_env(),
            park_tx: tokio::sync::watch::channel(false).0,
            upgrading: false,
            hold_idle: false,
            foreground,
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            writers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            generation: 0,
            last_upgrade: None,
            upgrade_notify: Arc::new(Notify::new()),
            upgrade_pending: false,
            upgrade_ready: None,
            start_keys: std::collections::VecDeque::new(),
            agent_permits: HashMap::new(),
            pending_agents: std::collections::VecDeque::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(home: PathBuf, foreground: bool, handover: Option<PathBuf>) -> i32 {
    if let Some(path) = handover {
        return run_restored(home, path).await;
    }
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("pi-famulus: cannot create {}: {e}", home.display());
        return 1;
    }
    // §3.1: the lifetime lock on manager.lock decides who the daemon is; the
    // holder removes stale socket/pid files before binding. Held until exit.
    let daemon_lock = match lifecycle::claim_daemon(&home) {
        Ok(Claim::Acquired(guard)) => guard,
        Ok(Claim::AlreadyRunning { pid }) => {
            match pid {
                Some(pid) => println!("pi-famulus already running (pid {pid})"),
                None => println!("pi-famulus already running (starting up)"),
            }
            return 0;
        }
        Err(e) => {
            eprintln!("pi-famulus: daemon lock failed: {e}");
            return 1;
        }
    };

    // The pid file and the socket before the scan: a large home takes
    // seconds to load, longer than a spawning client waits for the socket.
    // A client that connects meanwhile queues in the listen backlog and is
    // accepted once `serve` runs. The pid file first: whoever can connect
    // may read it at once (identity is the lock, the pid file is
    // informational).
    if let Err(e) = lifecycle::write_pid_file(&home, std::process::id()) {
        eprintln!("pi-famulus: cannot write pid file: {e}");
        return 1;
    }
    // Bind the well-known socket (§3.1). A plain tokio UnixListener: the
    // daemon owns its descriptor, which an in-place upgrade hands over.
    let sock = lifecycle::socket_path(&home);
    let listener = match tokio::net::UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pi-famulus: cannot listen on {}: {e}", sock.display());
            return 1;
        }
    };

    let mut registry = Registry::new(home.clone());
    // Test hook: stand in for a scan over a large home (thousands of records).
    if cfg!(debug_assertions) {
        if let Some(ms) = std::env::var("PI_FAMULUS_TEST_SLOW_SCAN_MS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
    let scan = lifecycle::scan_tasks(&home, &mut registry);
    let state: Shared = Arc::new(Mutex::new(DaemonState::new(
        home.clone(),
        registry,
        foreground,
    )));

    lifecycle::log_line(
        &home,
        &format!(
            "daemon started pid={} version={} orphaned={} loaded={}",
            std::process::id(),
            crate::VERSION,
            scan.orphaned,
            scan.loaded
        ),
    );
    crate::events::emit(
        &home,
        None,
        "daemon.start",
        None,
        serde_json::json!({
            "pid": std::process::id(),
            "version": crate::VERSION,
            "protocol": PROTOCOL,
            "orphaned": scan.orphaned,
            "loaded": scan.loaded,
        }),
    );
    if foreground {
        eprintln!(
            "pi-famulus {} listening on {} (pid {})",
            crate::VERSION,
            sock.display(),
            std::process::id()
        );
    }
    serve(state, listener, daemon_lock).await
}

/// Clients get this long to reconnect after an in-place upgrade before the
/// zero-connection idle rule (§3.2) applies again.
const HANDOVER_GRACE: Duration = Duration::from_secs(30);

/// The new image after an in-place upgrade (`daemon --handover <file>`).
async fn run_restored(home: PathBuf, path: PathBuf) -> i32 {
    let restored = match crate::handover::restore(&path) {
        Ok(r) => r,
        Err(e) => {
            // No crash recovery (§3.2): exiting closes the lifeline and
            // every runner takes its group down.
            lifecycle::log_line(&home, &format!("upgrade: restore failed: {e}; exiting"));
            eprintln!("pi-famulus: upgrade restore failed: {e}");
            std::process::exit(1);
        }
    };
    let live = crate::handover::live_ids(&restored);
    let crate::handover::Restored {
        snap,
        listener,
        lock,
        entries,
    } = restored;
    let mut registry = Registry::new(home.clone());
    lifecycle::scan_tasks_except(&home, &mut registry, &live);
    for e in entries {
        registry.tasks.insert(e.record.task_id.clone(), e);
    }
    let mut st = DaemonState::new(home.clone(), registry, snap.foreground);
    st.started_at_ms = snap.started_at_ms;
    st.generation = snap.generation;
    st.hold_idle = true;
    st.last_upgrade = Some(UpgradeInfo {
        at: now_ms(),
        ok: true,
        from_version: snap.from_version.clone(),
        to_version: Some(crate::VERSION.to_string()),
        error: None,
        trigger: snap.trigger.clone(),
    });
    for s in &snap.sessions {
        st.sessions.insert(
            s.session_id.clone(),
            SessionEntry {
                pi_pid: s.pi_pid,
                conn_id: None,
                cwd: s.cwd.clone(),
                extension_version: s.extension_version.clone(),
                protocol: s.protocol,
                connected_at: s.connected_at,
                last_seen: s.last_seen,
            },
        );
    }
    for (sid, key) in snap.start_keys.iter().cloned() {
        st.start_keys.push_back((sid, key));
    }
    let clock = st.clock.clone();
    clock.resume_at(snap.clock_now_ms);
    let state: Shared = Arc::new(Mutex::new(st));
    for id in &live {
        crate::handover::restart_task(&state, id);
        crate::handover::rearm_timers(&state, id);
    }
    lifecycle::log_line(
        &home,
        &format!(
            "upgraded in place: {} -> {} (pid {}, generation {}, {} live task(s))",
            snap.from_version,
            crate::VERSION,
            std::process::id(),
            snap.generation,
            live.len()
        ),
    );
    // Clients reconnect within moments; the idle rule waits for them.
    let s2 = state.clone();
    tokio::spawn(async move {
        clock.sleep("handover-grace", HANDOVER_GRACE).await;
        s2.lock().unwrap().hold_idle = false;
        maybe_arm_idle_timer(&s2);
    });
    serve(state, listener, lock).await
}

/// The accept loop, shared by a fresh daemon and one restored after an
/// in-place upgrade. Also runs the upgrade itself when asked.
async fn serve(
    state: Shared,
    listener: tokio::net::UnixListener,
    daemon_lock: lifecycle::DaemonLockGuard,
) -> i32 {
    use std::os::fd::AsRawFd;
    let home = state.lock().unwrap().home.clone();

    // §3.2: forget gone sessions past their retention, now and periodically.
    spawn_session_gc(&state);
    // In-place upgrade when the binary on disk changes.
    spawn_exe_watch(&state);
    #[cfg(feature = "test-clock")]
    spawn_test_owner_watch(&home);

    // §3.2: the idle rule applies from boot (clients connect within 2s of
    // spawn per §3.1, so this never fires for a healthy startup).
    maybe_arm_idle_timer(&state);

    let (shutdown_notify, upgrade_notify) = {
        let st = state.lock().unwrap();
        (st.shutdown_notify.clone(), st.upgrade_notify.clone())
    };
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();

    // Keep accepting while shutting down: a new client then gets a prompt
    // "manager is shutting down" instead of hanging until its hello timeout
    // (and, under `test-clock`, can still step the manual clock).
    let mut shutdown_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _addr)) => {
                    let s = state.clone();
                    tokio::spawn(async move { handle_conn(s, stream).await });
                }
                Err(e) => {
                    lifecycle::log_line(&home, &format!("accept error: {e}"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = upgrade_notify.notified(), if shutdown_task.is_none() => {
                let ready = state.lock().unwrap().upgrade_ready.take();
                if let Some(ready) = ready {
                    // Returns only if the upgrade did not happen.
                    let _ = crate::handover::perform(&state, listener.as_raw_fd(), daemon_lock.raw_fd(), ready).await;
                }
            }
            _ = shutdown_notify.notified(), if shutdown_task.is_none() => {
                shutdown_task = Some(begin_shutdown(&state));
            }
            _ = async {
                match sigterm.as_mut() {
                    Some(s) => { s.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            }, if shutdown_task.is_none() => {
                lifecycle::log_line(&home, "received SIGTERM");
                shutdown_task = Some(begin_shutdown(&state));
            }
            _ = async {
                match sigint.as_mut() {
                    Some(s) => { s.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            }, if shutdown_task.is_none() => {
                lifecycle::log_line(&home, "received SIGINT");
                shutdown_task = Some(begin_shutdown(&state));
            }
            _ = async {
                match shutdown_task.as_mut() {
                    Some(t) => { let _ = t.await; }
                    None => std::future::pending::<()>().await,
                }
            } => break,
        }
    }
    drop(daemon_lock);
    0
}

/// Poll the resolved install path for this daemon's executable. A path change
/// (for example, from an npm-retired package to its stable sibling) or a file
/// replacement triggers an in-place upgrade when the candidate identity stays
/// unchanged for two polls. A failed handover is not retried until the identity
/// changes again.
fn spawn_exe_watch(state: &Shared) {
    const POLL: Duration = Duration::from_secs(2);
    if crate::handover::exe_path().is_err() {
        return;
    }
    let ident = |p: &std::path::Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p)
            .ok()
            .map(|m| (m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
    };
    let state2 = state.clone();
    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let mut running = ident(std::path::Path::new("/proc/self/exe"));
        #[cfg(not(target_os = "linux"))]
        let mut running = std::env::current_exe().ok().as_deref().and_then(ident);
        let mut seen = running;
        loop {
            tokio::time::sleep(POLL).await;
            let Ok(resolved_exe) = crate::handover::exe_path() else {
                seen = None;
                continue;
            };
            let now = ident(&resolved_exe);
            if now.is_none() || now == running {
                seen = now;
                continue;
            }
            if now != seen {
                seen = now; // changed since the last poll: wait until it settles
                continue;
            }
            running = now;
            crate::handover::request(&state2, "binary-changed");
        }
    });
}

/// Mark the manager as shutting down (new hellos are refused) and run the
/// graceful shutdown as its own task.
fn begin_shutdown(state: &Shared) -> tokio::task::JoinHandle<()> {
    state.lock().unwrap().shutdown = true;
    let s = state.clone();
    tokio::spawn(async move { graceful_shutdown(&s).await })
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

fn valid_session_id(s: &str) -> bool {
    // Session ids become directory names; keep them path-safe.
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn parse_request(bytes: &[u8]) -> Result<Request, (String, String)> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| (String::new(), format!("invalid JSON: {e}")))?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    serde_json::from_value::<Request>(v).map_err(|e| (id, format!("bad request: {e}")))
}

/// Decrements a counter when dropped.
struct CountGuard(Arc<std::sync::atomic::AtomicUsize>);

impl CountGuard {
    fn new(c: &Arc<std::sync::atomic::AtomicUsize>) -> CountGuard {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        CountGuard(c.clone())
    }
}

impl Drop for CountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn writer_task<W: tokio::io::AsyncWrite + Unpin>(
    mut w: W,
    mut rx: mpsc::Receiver<OutFrame>,
    written: Written,
    _live: CountGuard,
) {
    while let Some(frame) = rx.recv().await {
        let payload = &frame.bytes;
        // An oversized frame is dropped, not written: write_frame would
        // refuse it, and ending the writer here would leave the connection
        // mute for every later response. `respond` already substitutes an
        // error for oversized responses, so this is a last line of defence.
        if payload.len() > MAX_FRAME_SIZE as usize {
            continue;
        }
        if let Err(e) = write_frame(&mut w, payload).await {
            // The connection goes mute from here; its reader still runs.
            eprintln!("pi-famulus: connection writer stopped: {e}");
            break;
        }
        if let Some((task, cursor)) = frame.output {
            let mut m = written.lock().unwrap();
            let e = m.entry(task).or_insert(0);
            *e = (*e).max(cursor);
        }
    }
}

async fn handle_conn(state: Shared, stream: tokio::net::UnixStream) {
    let (mut rd, wr) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<OutFrame>(1024);
    let die = Arc::new(Notify::new());
    let (writers, inflight) = {
        let st = state.lock().unwrap();
        (st.writers.clone(), st.inflight.clone())
    };
    let written: Written = Arc::new(Mutex::new(HashMap::new()));
    let writer = tokio::spawn(writer_task(
        wr,
        rx,
        written.clone(),
        CountGuard::new(&writers),
    ))
    .abort_handle();

    // ---- hello: must be the first message on the connection (§3.3) ----
    let first = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut rd)).await {
        Ok(Ok(Some(bytes))) => bytes,
        // EOF before hello is a readiness probe; not worth a line.
        Ok(Ok(None)) => return,
        // Closed without an answer: say why, or a client that did send its
        // hello only sees the connection drop.
        Ok(Err(e)) => {
            let home = state.lock().unwrap().home.clone();
            lifecycle::log_line(&home, &format!("connection closed before hello: {e}"));
            return;
        }
        Err(_) => {
            let home = state.lock().unwrap().home.clone();
            lifecycle::log_line(
                &home,
                &format!("connection closed: no hello within {HELLO_TIMEOUT:?}"),
            );
            return;
        }
    };
    let hello = match parse_request(&first) {
        Ok(r) => r,
        Err((id, msg)) => {
            let _ = tx.send(encode_error(&id, E_BAD_REQUEST, &msg)).await;
            return;
        }
    };
    // Test-only manual-clock requests: answered before (instead of) hello,
    // so they never count as an active connection or cancel the idle timer.
    #[cfg(feature = "test-clock")]
    if matches!(
        hello.kind,
        RequestKind::ClockStatus | RequestKind::ClockAdvance { .. }
    ) {
        respond(&tx, &hello.id, handle_clock(&state, &hello.kind)).await;
        return;
    }
    #[cfg(feature = "test-clock")]
    if matches!(hello.kind, RequestKind::DebugCrash) {
        std::process::exit(101);
    }
    if matches!(hello.v, Some(v) if v != PROTO_VERSION) {
        let _ = tx
            .send(encode_error(
                &hello.id,
                E_VERSION,
                "unsupported protocol version",
            ))
            .await;
        return;
    }
    let (client_kind, session_id, pi_pid, info) = match hello.kind {
        RequestKind::Hello {
            client_kind,
            session_id,
            pi_pid,
            cwd,
            extension_version,
            protocol,
        } => (
            client_kind,
            session_id,
            pi_pid,
            HelloInfo {
                cwd,
                extension_version,
                protocol,
            },
        ),
        _ => {
            let _ = tx
                .send(encode_error(
                    &hello.id,
                    E_BAD_REQUEST,
                    "first message must be hello",
                ))
                .await;
            return;
        }
    };
    if client_kind == ClientKind::Extension {
        let ok = matches!(&session_id, Some(s) if valid_session_id(s)) && pi_pid.is_some();
        if !ok {
            let _ = tx
                .send(encode_error(
                    &hello.id,
                    E_BAD_REQUEST,
                    "extension hello requires session_id and pi_pid",
                ))
                .await;
            return;
        }
    }

    let conn_id = match register_conn(
        &state,
        &tx,
        &die,
        &written,
        &writer,
        client_kind,
        session_id,
        pi_pid,
        info,
    ) {
        Ok(id) => id,
        Err(e) => {
            let _ = tx.send(encode_error(&hello.id, &e.code, &e.message)).await;
            return;
        }
    };
    let started_at = state.lock().unwrap().started_at_ms;
    let _ = tx
        .send(encode_ok(
            &hello.id,
            &HelloOk {
                version: crate::VERSION.to_string(),
                pid: std::process::id(),
                started_at,
                protocol: PROTOCOL,
            },
        ))
        .await;
    resume_carried_watches(&state, conn_id, &tx);

    // ---- request loop (requests may be pipelined; each runs in its own task) ----
    loop {
        let frame = tokio::select! {
            f = read_frame(&mut rd) => match f {
                Ok(Some(b)) => b,
                Ok(None) => break, // EOF: the client closed
                Err(e) => {
                    // io error / oversized frame: the client sees a drop
                    let home = state.lock().unwrap().home.clone();
                    lifecycle::log_line(&home, &format!("connection {conn_id} closed: {e}"));
                    break;
                }
            },
            _ = die.notified() => break, // rebound by a newer connection
        };
        let req = match parse_request(&frame) {
            Ok(r) => r,
            Err((id, msg)) => {
                let _ = tx.send(encode_error(&id, E_BAD_REQUEST, &msg)).await;
                continue;
            }
        };
        if matches!(req.v, Some(v) if v != PROTO_VERSION) {
            let _ = tx
                .send(encode_error(
                    &req.id,
                    E_VERSION,
                    "unsupported protocol version",
                ))
                .await;
            continue;
        }
        let s2 = state.clone();
        let tx2 = tx.clone();
        let guard = CountGuard::new(&inflight);
        let mut park = state.lock().unwrap().park_tx.subscribe();
        tokio::spawn(async move {
            // An in-place upgrade cancels requests at their next await and
            // leaves them unanswered: the client resends them after it
            // reconnects to the new image.
            tokio::select! {
                biased;
                _ = task::parked(&mut park) => {}
                _ = dispatch(s2, conn_id, req, tx2) => {}
            }
            drop(guard);
        });
    }

    // ---- disconnect (§3.2: socket close marks the session disconnected) ----
    {
        let mut st = state.lock().unwrap();
        remove_conn(&mut st, conn_id, "closed");
    }
    maybe_arm_idle_timer(&state);
}

/// Optional hello fields stored per session.
pub struct HelloInfo {
    pub cwd: Option<String>,
    pub extension_version: Option<String>,
    pub protocol: Option<u32>,
}

/// Register a hello'd connection. Same-session rebind: the new connection
/// wins; the old one gets `session_rebound` and is closed (§3.3).
#[allow(clippy::too_many_arguments)]
fn register_conn(
    state: &Shared,
    tx: &OutTx,
    die: &Arc<Notify>,
    written: &Written,
    writer: &tokio::task::AbortHandle,
    kind: ClientKind,
    session_id: Option<String>,
    pi_pid: Option<u32>,
    info: HelloInfo,
) -> Result<u64, ProtoError> {
    let mut st = state.lock().unwrap();
    if st.shutdown {
        return Err(ProtoError::new(E_INTERNAL, SHUTTING_DOWN));
    }
    // A fresh active connection cancels any pending idle shutdown (§3.2).
    if let Some(t) = st.idle_timer.take() {
        t.abort();
    }
    let conn_id = st.next_conn_id;
    st.next_conn_id += 1;
    if kind == ClientKind::Extension {
        let sid = session_id.clone().unwrap_or_default();
        let old = st.sessions.get(&sid).and_then(|s| s.conn_id);
        if let Some(old_id) = old {
            if let Some(old_h) = st.conns.get(&old_id) {
                let _ = old_h
                    .tx
                    .try_send(encode_event(&EventKind::SessionRebound {}));
                old_h.die.notify_one();
            }
            remove_conn(&mut st, old_id, "rebound");
        }
        let now = now_ms();
        // `connected_at` is the first hello this manager saw for the session;
        // a reconnect (pi --resume, rebind) keeps it.
        let connected_at = st.sessions.get(&sid).map(|s| s.connected_at).unwrap_or(now);
        crate::events::emit(
            &st.home,
            Some(&sid),
            "session.connect",
            None,
            serde_json::json!({
                "pi_pid": pi_pid.unwrap_or(0),
                "cwd": info.cwd,
                "extension_version": info.extension_version,
                "protocol": info.protocol,
            }),
        );
        st.sessions.insert(
            sid,
            SessionEntry {
                pi_pid: pi_pid.unwrap_or(0),
                conn_id: Some(conn_id),
                cwd: info.cwd,
                extension_version: info.extension_version,
                protocol: info.protocol,
                connected_at,
                last_seen: now,
            },
        );
    }
    st.conns.insert(
        conn_id,
        ConnHandle {
            kind,
            session_id,
            tx: tx.clone(),
            die: die.clone(),
            written: written.clone(),
            writer: writer.clone(),
        },
    );
    Ok(conn_id)
}

fn remove_conn(st: &mut DaemonState, conn_id: u64, why: &str) {
    if let Some(h) = st.conns.remove(&conn_id) {
        if let Some(sid) = &h.session_id {
            let home = st.home.clone();
            if let Some(s) = st.sessions.get_mut(sid) {
                if s.conn_id == Some(conn_id) {
                    s.conn_id = None; // session now disconnected (§3.2)
                    s.last_seen = now_ms();
                    if why == "closed" {
                        st.agent_permits.retain(|(owner, _), _| owner != sid);
                        st.pending_agents.retain(|p| p.session_id != *sid);
                    }
                    crate::events::emit(
                        &home,
                        Some(sid),
                        "session.disconnect",
                        None,
                        serde_json::json!({ "reason": why }),
                    );
                }
            }
        }
        for t in st.registry.tasks.values_mut() {
            t.watchers.remove(&conn_id);
        }
    }
}

/// A session's subscription to carry over an upgrade, and how far its
/// connection's writer got (known only once the writers have flushed).
pub struct Carried {
    task_id: String,
    session_id: String,
    written: Written,
}

/// Close every client connection (an in-place upgrade). Returns the output
/// subscriptions to carry over and the connections' writers; call
/// [`carry_watches`] once the writers flushed or were aborted.
pub fn close_all_connections(
    state: &Shared,
    why: &str,
) -> (Vec<Carried>, Vec<tokio::task::AbortHandle>) {
    let mut st = state.lock().unwrap();
    let conns: HashMap<u64, (Option<String>, Written)> = st
        .conns
        .iter()
        .map(|(id, h)| (*id, (h.session_id.clone(), h.written.clone())))
        .collect();
    let writers = st.conns.values().map(|h| h.writer.clone()).collect();
    let mut carried = Vec::new();
    for (tid, e) in st.registry.tasks.iter_mut() {
        for w in std::mem::take(&mut e.watchers) {
            if let Some((Some(sid), written)) = conns.get(&w) {
                carried.push(Carried {
                    task_id: tid.clone(),
                    session_id: sid.clone(),
                    written: written.clone(),
                });
            }
        }
    }
    let ids: Vec<u64> = st.conns.keys().copied().collect();
    for id in ids {
        if let Some(h) = st.conns.get(&id) {
            h.die.notify_one();
        }
        remove_conn(&mut st, id, why);
    }
    (carried, writers)
}

/// Remember each carried subscription with the cursor its connection was
/// really written up to: what it did not receive is replayed when the
/// session reconnects.
pub fn carry_watches(state: &Shared, carried: Vec<Carried>) {
    let mut st = state.lock().unwrap();
    for c in carried {
        let Some(e) = st.registry.tasks.get_mut(&c.task_id) else {
            continue;
        };
        let cursor = c
            .written
            .lock()
            .unwrap()
            .get(&c.task_id)
            .copied()
            .unwrap_or(0);
        match e
            .watch_sessions
            .iter_mut()
            .find(|(s, _)| *s == c.session_id)
        {
            Some((_, at)) => *at = (*at).min(cursor),
            None => e.watch_sessions.push((c.session_id, cursor)),
        }
    }
}

/// Most bytes a reconnecting session is sent as missed output per task
/// (8 MiB). Beyond that it catches up with `output(cursor)` itself.
const REPLAY_MAX: u64 = 8 * 1024 * 1024;

/// A session reconnecting after an in-place upgrade gets its output
/// subscriptions back, plus the output it missed while away (from where
/// the old image stopped pushing up to what the fanout has pushed since).
///
/// All of it happens under the state lock, which the fanout also holds to
/// pick its targets, and which `close_all_connections` holds: the replayed
/// events are queued on the connection before any later fanout chunk, and a
/// second upgrade cannot cut a replay in half (it would record a cursor past
/// what the connection got).
fn resume_carried_watches(state: &Shared, conn_id: u64, tx: &OutTx) {
    let mut st = state.lock().unwrap();
    let Some(sid) = st.conns.get(&conn_id).and_then(|h| h.session_id.clone()) else {
        return;
    };
    let st_conns_written = st.conns.get(&conn_id).map(|h| h.written.clone());
    for (tid, e) in st.registry.tasks.iter_mut() {
        let Some(i) = e.watch_sessions.iter().position(|(s, _)| *s == sid) else {
            continue;
        };
        let (_, from) = e.watch_sessions.remove(i);
        e.watchers.insert(conn_id);
        if let Some(h) = st_conns_written.as_ref() {
            h.lock().unwrap().insert(tid.clone(), from);
        }
        let to = e.delivered_cursor;
        let mut cursor = from.max(to.saturating_sub(REPLAY_MAX));
        while cursor < to {
            let want = ((to - cursor) as usize).min(256 * 1024);
            let Ok((bytes, _)) =
                task::read_file_range(std::path::Path::new(&e.record.output_path), cursor, want)
            else {
                break;
            };
            if bytes.is_empty() {
                break;
            }
            // `to` is on a character boundary (the fanout never counts a
            // held-back tail), so only the 256 KiB cut needs care.
            let n = task::utf8_chunk_len(
                &bytes,
                want,
                CHUNK_JSON_BUDGET,
                cursor + (bytes.len() as u64) < to,
            )
            .max(1);
            let chunk = &bytes[..n.min(bytes.len())];
            cursor += chunk.len() as u64;
            let ev = encode_event_bytes(&EventKind::Output {
                task_id: tid.clone(),
                chunk: String::from_utf8_lossy(chunk).into_owned(),
                next_cursor: cursor,
            });
            if tx.try_send(OutFrame::output(ev, tid, cursor)).is_err() {
                break; // queue full: the client catches up with output(cursor)
            }
        }
    }
}

/// §3.2: sweep gone sessions and finished tasks at startup and then every
/// `gc::interval_ms` of the shorter retention. Retentions are read once per
/// daemon.
fn spawn_session_gc(state: &Shared) {
    let (home, clock) = {
        let st = state.lock().unwrap();
        (st.home.clone(), st.clock.clone())
    };
    let read = |r: Result<u64, String>, default: u64| match r {
        Ok(ms) => ms,
        Err(e) => {
            lifecycle::log_line(&home, &format!("config: {e}; using the default 24h"));
            default
        }
    };
    let retention = read(
        crate::gc::retention_ms(&home),
        crate::gc::DEFAULT_RETENTION_MS,
    );
    let task_retention = read(
        crate::gc::task_retention_ms(&home),
        crate::gc::DEFAULT_TASK_RETENTION_MS,
    );
    run_session_gc(state, retention);
    run_task_gc(state, task_retention);
    let state2 = state.clone();
    tokio::spawn(async move {
        let every = Duration::from_millis(crate::gc::interval_ms(retention.min(task_retention)));
        loop {
            clock.sleep("gc", every).await;
            if state2.lock().unwrap().shutdown {
                break;
            }
            run_session_gc(&state2, retention);
            run_task_gc(&state2, task_retention);
        }
    });
}

/// Delete the files of tasks that ended more than `retention_ms` ago and
/// forget them, in every session. A task whose group still lingers is kept.
fn run_task_gc(state: &Shared, retention_ms: u64) {
    let (home, candidates) = {
        let st = state.lock().unwrap();
        let now = now_ms();
        let candidates: Vec<TaskRecord> = st
            .registry
            .tasks
            .values()
            .filter(|e| !e.owns_live_group())
            .filter(|e| {
                e.record
                    .ended_at
                    .is_some_and(|t| now.saturating_sub(t) >= retention_ms)
            })
            .map(|e| e.record.clone())
            .collect();
        (st.home.clone(), candidates)
    };
    if candidates.is_empty() {
        return;
    }
    // Delete files with the state lock released, so an hourly sweep over a
    // large home does not block every status/ls/output/tail handler for the
    // whole batch. A task is forgotten only once its files are gone; a
    // failed remove is retried on the next sweep instead of the record
    // vanishing while the file it named stays behind, unreachable.
    let removed: Vec<String> = candidates
        .into_iter()
        .filter(|r| {
            let output = PathBuf::from(&r.output_path);
            remove_file_if_present(&crate::task::stderr_path_for(&output))
                && remove_file_if_present(&output)
                && remove_file_if_present(&registry::task_json_path(
                    &home,
                    &r.session_id,
                    &r.task_id,
                ))
        })
        .map(|r| r.task_id)
        .collect();
    if removed.is_empty() {
        return;
    }
    {
        let mut st = state.lock().unwrap();
        for id in &removed {
            st.registry.tasks.remove(id);
        }
    }
    lifecycle::log_line(
        &home,
        &format!(
            "gc: removed {} finished task(s): {}",
            removed.len(),
            crate::gc::log_ids(&removed)
        ),
    );
}

/// Remove a file, treating "already gone" as success.
fn remove_file_if_present(path: &std::path::Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn run_session_gc(state: &Shared, retention_ms: u64) {
    let mut st = state.lock().unwrap();
    // Never sweep a session that is connected or still owns live processes.
    let mut keep: HashSet<String> = st
        .sessions
        .iter()
        .filter(|(_, s)| s.conn_id.is_some())
        .map(|(sid, _)| sid.clone())
        .collect();
    for e in st.registry.tasks.values() {
        if e.record.status == TaskStatus::Running || e.owns_live_group() {
            keep.insert(e.record.session_id.clone());
        }
    }
    let home = st.home.clone();
    let removed = crate::gc::sweep(&home, &keep, retention_ms);
    if removed.is_empty() {
        return;
    }
    let gone: HashSet<&String> = removed.iter().collect();
    st.registry
        .tasks
        .retain(|_, e| !gone.contains(&e.record.session_id));
    st.sessions.retain(|sid, _| !gone.contains(sid));
    lifecycle::log_line(
        &home,
        &format!(
            "gc: removed {} gone session(s): {}",
            removed.len(),
            crate::gc::log_ids(&removed)
        ),
    );
    crate::events::emit(
        &home,
        None,
        "session.gc",
        None,
        serde_json::json!({ "removed": removed, "retention_ms": retention_ms }),
    );
}

/// §3.2: arm the 5s idle timer when the last active connection went away.
pub fn maybe_arm_idle_timer(state: &Shared) {
    let mut st = state.lock().unwrap();
    if st.shutdown
        || st.upgrading
        || st.hold_idle
        || !st.conns.is_empty()
        || st.idle_timer.is_some()
    {
        return;
    }
    let state2 = state.clone();
    let clock = st.clock.clone();
    st.idle_timer = Some(tokio::spawn(async move {
        clock.sleep("idle", IDLE_SHUTDOWN).await;
        let fired = {
            let mut st = state2.lock().unwrap();
            if st.conns.is_empty() && !st.shutdown {
                st.shutdown = true;
                Some((st.home.clone(), st.shutdown_notify.clone()))
            } else {
                None
            }
        };
        if let Some((home, notify)) = fired {
            lifecycle::log_line(&home, "no active connections for 5s; graceful shutdown");
            notify.notify_one();
        }
    }));
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

async fn dispatch(state: Shared, conn_id: u64, req: Request, tx: OutTx) {
    let id = req.id;
    match req.kind {
        RequestKind::Hello { .. } => {
            let _ = tx
                .send(encode_error(
                    &id,
                    E_BAD_REQUEST,
                    "connection already said hello",
                ))
                .await;
        }
        RequestKind::Start {
            kind,
            command,
            cwd,
            env,
            timeout_ms,
            origin,
            key,
            ..
        } => {
            let spec = StartSpec {
                kind,
                command,
                cwd,
                env,
                timeout_ms,
                origin,
                key,
            };
            respond(&tx, &id, handle_start(&state, conn_id, spec)).await
        }
        RequestKind::MarkBackground { task_id } => {
            respond(&tx, &id, handle_mark_background(&state, conn_id, &task_id)).await
        }
        RequestKind::Wait { task_id, budget_ms } => {
            respond(
                &tx,
                &id,
                handle_wait(&state, conn_id, &task_id, budget_ms).await,
            )
            .await
        }
        RequestKind::Output {
            task_id,
            cursor,
            max_bytes,
        } => {
            respond(
                &tx,
                &id,
                handle_output(&state, conn_id, &task_id, cursor, max_bytes),
            )
            .await
        }
        RequestKind::Stop { task_id, reason } => {
            respond(
                &tx,
                &id,
                handle_stop(&state, conn_id, &task_id, reason.as_deref()),
            )
            .await
        }
        RequestKind::List {
            all,
            session_id,
            paged,
            after,
        } => {
            respond(
                &tx,
                &id,
                handle_list(&state, conn_id, all, session_id, paged, after),
            )
            .await
        }
        RequestKind::Watch { task_id } => {
            respond(&tx, &id, handle_watch(&state, conn_id, &task_id, true)).await
        }
        RequestKind::Unwatch { task_id } => {
            respond(&tx, &id, handle_watch(&state, conn_id, &task_id, false)).await
        }
        RequestKind::ShutdownSession => {
            respond(&tx, &id, handle_shutdown_session(&state, conn_id)).await
        }
        RequestKind::Status => respond(&tx, &id, handle_status(&state, conn_id)).await,
        RequestKind::AcquireAgent {
            child_id,
            work_kind,
        } => {
            handle_acquire_agent(&state, conn_id, &child_id, &work_kind, &id, &tx).await;
        }
        RequestKind::CancelAcquireAgent { request_id } => {
            respond(
                &tx,
                &id,
                handle_cancel_acquire(&state, conn_id, &request_id),
            )
            .await;
        }
        RequestKind::ReleaseAgent { child_id } => {
            respond(&tx, &id, handle_release_agent(&state, conn_id, &child_id)).await;
        }
        RequestKind::Shutdown => respond(&tx, &id, handle_shutdown(&state, conn_id)).await,
        RequestKind::Upgrade => respond(&tx, &id, handle_upgrade(&state, conn_id)).await,
        #[cfg(feature = "test-clock")]
        ref k @ (RequestKind::ClockStatus | RequestKind::ClockAdvance { .. }) => {
            respond(&tx, &id, handle_clock(&state, k)).await
        }
        #[cfg(feature = "test-clock")]
        RequestKind::DebugCrash => std::process::exit(101),
    }
}

/// Test-only: exit once the test process named by `PI_FAMULUS_TEST_OWNER` is gone.
/// A manual-clock daemon never idles out on its own (only the test advances
/// its timers), so one left by a killed test binary would otherwise run, and
/// hold its tasks, forever. Exiting without a shutdown is a crash: every
/// runner's lifeline breaks and takes its group down. Polls in real time.
#[cfg(feature = "test-clock")]
fn spawn_test_owner_watch(home: &std::path::Path) {
    let Some(owner) = std::env::var("PI_FAMULUS_TEST_OWNER")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return;
    };
    let home = home.to_path_buf();
    std::thread::spawn(move || {
        while crate::sys::pid_alive(owner) {
            std::thread::sleep(Duration::from_millis(200));
        }
        lifecycle::log_line(&home, &format!("test owner {owner} is gone; exiting"));
        std::process::exit(1);
    });
}

/// Test-only: inspect or advance the manual clock (`test-clock` feature).
#[cfg(feature = "test-clock")]
fn handle_clock(
    state: &Shared,
    req: &RequestKind,
) -> Result<crate::clock::ClockStatus, ProtoError> {
    let clock = state.lock().unwrap().clock.clone();
    let Some(m) = clock.manual() else {
        return Err(ProtoError::new(
            E_BAD_REQUEST,
            "manual clock not enabled (start the daemon with PI_FAMULUS_TEST_CLOCK=manual)",
        ));
    };
    Ok(match req {
        RequestKind::ClockAdvance { ms } => m.advance(*ms),
        _ => m.status(),
    })
}

async fn respond<T: Serialize>(tx: &OutTx, id: &str, result: Result<T, ProtoError>) {
    let mut frame = match result {
        Ok(body) => encode_ok(id, &body),
        Err(e) => encode_error(id, &e.code, &e.message),
    };
    // Every request gets an answer: a response that does not fit a frame
    // becomes an error instead of silently disappearing.
    if frame.bytes.len() > MAX_FRAME_SIZE as usize {
        frame = encode_error(id, E_INTERNAL, "response exceeds the 4 MiB frame limit");
    }
    let _ = tx.send(frame).await;
}

fn encode_ok<T: Serialize>(id: &str, body: &T) -> OutFrame {
    OutFrame::plain(Arc::new(encode(&Response {
        v: PROTO_VERSION,
        id: id.to_string(),
        ok: true,
        body,
    })))
}

fn encode_error(id: &str, code: &str, message: &str) -> OutFrame {
    OutFrame::plain(Arc::new(encode(&Response {
        v: PROTO_VERSION,
        id: id.to_string(),
        ok: false,
        body: ErrorBody {
            error: ProtoError::new(code, message),
        },
    })))
}

fn encode_event(kind: &EventKind) -> OutFrame {
    OutFrame::plain(encode_event_bytes(kind))
}

fn encode_event_bytes(kind: &EventKind) -> Arc<Vec<u8>> {
    Arc::new(encode(&Event::new(kind.clone())))
}

fn access_for(st: &DaemonState, conn_id: u64) -> Access {
    match st.conns.get(&conn_id) {
        Some(h) if h.kind == ClientKind::Extension => match &h.session_id {
            Some(sid) => Access::Extension(sid.clone()),
            None => Access::Cli,
        },
        _ => Access::Cli,
    }
}

fn send_event_to_session(state: &Shared, session_id: &str, kind: EventKind) {
    let tx = {
        let st = state.lock().unwrap();
        st.sessions
            .get(session_id)
            .and_then(|s| s.conn_id)
            .and_then(|cid| st.conns.get(&cid))
            .map(|h| h.tx.clone())
    };
    if let Some(tx) = tx {
        let _ = tx.try_send(encode_event(&kind));
    }
}

// ---------------------------------------------------------------------------
// Message handlers
// ---------------------------------------------------------------------------

pub struct StartSpec {
    pub kind: TaskKind,
    pub command: String,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub origin: Option<Origin>,
    pub key: Option<String>,
}

fn handle_start(state: &Shared, conn_id: u64, spec: StartSpec) -> Result<StartOk, ProtoError> {
    let StartSpec {
        kind,
        command,
        cwd,
        env,
        timeout_ms,
        origin,
        key,
    } = spec;
    let (session_id, home) = {
        let st = state.lock().unwrap();
        if st.shutdown {
            return Err(ProtoError::new(E_INTERNAL, SHUTTING_DOWN));
        }
        let h = st
            .conns
            .get(&conn_id)
            .ok_or_else(|| ProtoError::new(E_INTERNAL, "connection gone"))?;
        // §3.3: session_id comes from the connection binding.
        let sid = match &h.session_id {
            Some(s) => s.clone(),
            None => {
                return Err(ProtoError::new(
                    E_SESSION_REQUIRED,
                    "start requires an extension session",
                ))
            }
        };
        (sid, st.home.clone())
    };
    // A resent start (same session, same key) gets the task it started.
    if let Some(k) = &key {
        let st = state.lock().unwrap();
        let want = format!("{session_id}\u{0}{k}");
        let prior = st
            .start_keys
            .iter()
            .rev()
            .find(|(sk, _)| *sk == want)
            .map(|(_, t)| t.clone());
        if let Some(tid) = prior {
            if let Some(e) = st.registry.tasks.get(&tid) {
                return Ok(StartOk {
                    task_id: tid,
                    pid: e.record.pid,
                });
            }
        }
    }
    if command.trim().is_empty() {
        return Err(ProtoError::new(E_BAD_REQUEST, "empty command"));
    }
    let cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into())
    });

    let (task_id, out_path) = {
        let st = state.lock().unwrap();
        let id = st.registry.generate_task_id(kind);
        let path = registry::task_output_path(&home, &session_id, &id);
        (id, path)
    };
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (out_file, _) = task::open_output_files(&out_path)
        .map_err(|e| ProtoError::new(E_INTERNAL, format!("open output: {e}")))?;
    let parts = task::spawn_process(&command, &cwd, &env)
        .map_err(|e| ProtoError::new(E_INTERNAL, format!("spawn failed: {e}")))?;
    let pid = parts.pid;
    let output = Arc::new(Mutex::new(task::OutputState::new(Some(out_file), 0)));

    let now = now_ms();
    let record = TaskRecord {
        task_id: task_id.clone(),
        session_id: session_id.clone(),
        kind,
        command: command.clone(),
        cwd,
        pid,
        status: TaskStatus::Running,
        exit_code: None,
        signal: None,
        started_at: now,
        ended_at: None,
        output_path: out_path.to_string_lossy().into_owned(),
        output_size: 0,
        origin: origin.clone(),
        backgrounded_at: None,
        end_reason: None,
        cpu_user_ms: None,
        cpu_sys_ms: None,
        max_rss_kb: None,
    };
    if let Err(e) = registry::persist_record(&home, &record) {
        let _ = task::signal_group(pid, task::SIGKILL); // don't leak the child
        return Err(ProtoError::new(E_INTERNAL, format!("persist failed: {e}")));
    }

    {
        // `task.start` is written before the task becomes visible, and so
        // before its exit can be watched and logged (see finalize_exit).
        let mut st = state.lock().unwrap();
        crate::events::emit(
            &home,
            Some(&session_id),
            "task.start",
            Some(&task_id),
            serde_json::json!({
                "kind": kind,
                "command": crate::events::clip_chars(&command, crate::events::COMMAND_CHARS),
                "origin": origin,
                "pid": pid,
            }),
        );
        let mut entry = TaskEntry::new_running(record, parts, output, timeout_ms);
        // A monitor exists to stream: its starter watches from spawn on, so a
        // command that prints and exits at once loses nothing to a late watch.
        if kind == TaskKind::Monitor {
            entry.watchers.insert(conn_id);
            if let Some(h) = st.conns.get(&conn_id) {
                h.written.lock().unwrap().insert(task_id.clone(), 0);
            }
        }
        st.registry.tasks.insert(task_id.clone(), entry);
        if let Some(k) = &key {
            st.start_keys
                .push_back((format!("{session_id}\u{0}{k}"), task_id.clone()));
            while st.start_keys.len() > START_KEYS_MAX {
                st.start_keys.pop_front();
            }
        }
    }
    start_task_io(state, &task_id);
    spawn_exit_watch(state, &task_id);
    // §3.3: task_started is always pushed to the owning session.
    send_event_to_session(
        state,
        &session_id,
        EventKind::TaskStarted {
            task_id: task_id.clone(),
            kind,
            command,
            pid,
            ts: now,
        },
    );
    Ok(StartOk { task_id, pid })
}

async fn handle_wait(
    state: &Shared,
    conn_id: u64,
    task_id: &str,
    budget_ms: u64,
) -> Result<WaitOk, ProtoError> {
    let mut rx = {
        let st = state.lock().unwrap();
        let acc = access_for(&st, conn_id);
        let e = st.registry.visible(task_id, &acc)?;
        if e.record.status.is_terminal() {
            return Ok(WaitOk {
                done: true,
                exit_code: e.record.exit_code,
            });
        }
        e.status_tx.subscribe()
    };
    // Re-check after subscribing (finalize may have raced the subscribe).
    if rx.borrow().is_terminal() {
        let ec = state
            .lock()
            .unwrap()
            .registry
            .tasks
            .get(task_id)
            .and_then(|e| e.record.exit_code);
        return Ok(WaitOk {
            done: true,
            exit_code: ec,
        });
    }
    // §3.3: wait is a *budget* wait — on expiry the task keeps running.
    let deadline = tokio::time::sleep(Duration::from_millis(budget_ms));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() || rx.borrow().is_terminal() {
                    let ec = state
                        .lock()
                        .unwrap()
                        .registry
                        .tasks
                        .get(task_id)
                        .and_then(|e| e.record.exit_code);
                    return Ok(WaitOk { done: true, exit_code: ec });
                }
            }
            _ = &mut deadline => return Ok(WaitOk { done: false, exit_code: None }),
        }
    }
}

fn handle_output(
    state: &Shared,
    conn_id: u64,
    task_id: &str,
    cursor: u64,
    max_bytes: u64,
) -> Result<OutputOk, ProtoError> {
    let cap = max_bytes.min(MAX_OUTPUT_READ) as usize;
    // Read a little past the cap so a character straddling it is visible.
    let want = cap + task::UTF8_LOOKAHEAD;
    enum Src {
        Ring(Vec<u8>),
        Disk(String),
    }
    let (src, status, exit_code, total_size) = {
        let st = state.lock().unwrap();
        let acc = access_for(&st, conn_id);
        let e = st.registry.visible(task_id, &acc)?;
        let out = e.output.lock().unwrap();
        let total = out.total_size;
        // Serve from the 64KB ring when the range is fully retained (§3.4);
        // otherwise fall back to the full on-disk stream.
        let ring_start = total.saturating_sub(out.ring.len() as u64);
        let src = if cursor >= ring_start && cursor < total {
            let skip = (cursor - ring_start) as usize;
            let avail = out.ring.len() - skip;
            Src::Ring(out.ring.slice(skip, avail.min(want)))
        } else {
            Src::Disk(e.record.output_path.clone())
        };
        (src, e.record.status, e.record.exit_code, total)
    };
    let mut bytes = match src {
        Src::Ring(b) => b,
        Src::Disk(path) => {
            task::read_file_range(std::path::Path::new(&path), cursor, want)
                .map_err(|e| ProtoError::new(E_INTERNAL, format!("read output: {e}")))?
                .0
        }
    };
    // §3.3: cut at a UTF-8 boundary, within the frame budget after escaping.
    // A truncated sequence at the end of the data is held back while the task
    // can still write the rest. (One straddling the cap never looks truncated:
    // the lookahead always holds the whole character.)
    let n = task::utf8_chunk_len(
        &bytes,
        cap,
        CHUNK_JSON_BUDGET,
        status == TaskStatus::Running,
    );
    bytes.truncate(n);
    let next_cursor = cursor + bytes.len() as u64;
    Ok(OutputOk {
        chunk: String::from_utf8_lossy(&bytes).into_owned(), // §3.3: UTF-8 lossy
        next_cursor,
        status,
        exit_code,
        total_size,
    })
}

fn handle_stop(
    state: &Shared,
    conn_id: u64,
    task_id: &str,
    reason: Option<&str>,
) -> Result<UnitOk, ProtoError> {
    if let Some(r) = reason {
        if !STOP_REASONS.contains(&r) {
            return Err(ProtoError::new(
                E_BAD_REQUEST,
                format!(
                    "unknown stop reason {r:?} (expected one of {})",
                    STOP_REASONS.join(", ")
                ),
            ));
        }
    }
    let (pid, home, sid) = {
        let mut st = state.lock().unwrap();
        let home = st.home.clone();
        let acc = access_for(&st, conn_id);
        let e = st.registry.visible_mut(task_id, &acc)?;
        if !e.owns_live_group() {
            return Ok(UnitOk {}); // idempotent: terminal and nothing left
        }
        // A terminal task with a lingering group keeps its status; stop
        // still takes down what it left behind.
        e.request_kill(&end_reason_for_stop(reason));
        (e.record.pid, home, e.record.session_id.clone())
    };
    crate::events::emit(
        &home,
        Some(&sid),
        "task.stop",
        Some(task_id),
        serde_json::json!({ "reason": reason.unwrap_or("tool") }),
    );
    // §3.3 stop: SIGTERM the process group, 2s grace, then SIGKILL.
    let _ = task::signal_group(pid, task::SIGTERM);
    spawn_kill_reaper(state, task_id, pid);
    Ok(UnitOk {})
}

/// Observability: record that the extension moved a task to the background.
/// Idempotent (the first time is kept); a no-op on a finished task.
fn handle_mark_background(
    state: &Shared,
    conn_id: u64,
    task_id: &str,
) -> Result<UnitOk, ProtoError> {
    let (home, rec) = {
        let mut st = state.lock().unwrap();
        let home = st.home.clone();
        let acc = access_for(&st, conn_id);
        let e = st.registry.visible_mut(task_id, &acc)?;
        if e.record.status != TaskStatus::Running || e.record.backgrounded_at.is_some() {
            return Ok(UnitOk {});
        }
        e.record.backgrounded_at = Some(now_ms());
        let rec = e.record.clone();
        // Under the lock, like task.exit: the line cannot fall after a
        // concurrent exit's, nor after anyone sees `backgrounded_at`.
        crate::events::emit(
            &home,
            Some(&rec.session_id),
            "task.background",
            Some(task_id),
            serde_json::json!({ "after_ms": rec.backgrounded_at.unwrap_or(0).saturating_sub(rec.started_at) }),
        );
        (home, rec)
    };
    if let Err(e) = registry::persist_record(&home, &rec) {
        lifecycle::log_line(&home, &format!("persist {} failed: {e}", rec.task_id));
    }
    Ok(UnitOk {})
}

/// SIGKILL group `pgid` until nothing but (at most) its leader is left.
/// One `kill(-pgid)` can miss a process that is being forked while the
/// signal is delivered (seen on macOS: the runner's `sh`, or a child `sh`
/// forks, right at task start). Bounded: ~200 ms.
async fn kill_group_hard(pgid: u32) {
    for _ in 0..40 {
        let _ = task::signal_group(pgid, task::SIGKILL);
        tokio::time::sleep(Duration::from_millis(5)).await;
        if !crate::sys::group_has_others(pgid) {
            break;
        }
    }
}

/// After the grace, SIGKILL the *group* if anything in it may survive: the
/// leader, or descendants that ignored SIGTERM after the leader died. The
/// due time is kept in the entry (`kill_grace_until_ms`) so an in-place
/// upgrade re-arms the rest of the grace.
fn spawn_kill_reaper(state: &Shared, task_id: &str, pid: u32) {
    let clock = {
        let mut st = state.lock().unwrap();
        let due = st.clock.now_ms() + KILL_GRACE.as_millis() as u64;
        if let Some(e) = st.registry.tasks.get_mut(task_id) {
            e.kill_grace_until_ms = Some(due);
        }
        st.clock.clone()
    };
    arm_kill_reaper(state, task_id, pid, clock, KILL_GRACE);
}

/// Re-arm a stop's SIGKILL escalation with the time it had left.
pub fn rearm_kill_reaper(state: &Shared, task_id: &str, pid: u32, left: Duration) {
    let clock = state.lock().unwrap().clock.clone();
    arm_kill_reaper(state, task_id, pid, clock, left);
}

fn arm_kill_reaper(
    state: &Shared,
    task_id: &str,
    pid: u32,
    clock: crate::clock::Clock,
    grace: Duration,
) {
    let state2 = state.clone();
    let tid = task_id.to_string();
    tokio::spawn(async move {
        clock.sleep("kill-grace", grace).await;
        let group_live = {
            let mut st = state2.lock().unwrap();
            match st.registry.tasks.get_mut(&tid) {
                Some(e) => {
                    e.kill_grace_until_ms = None;
                    e.owns_live_group()
                }
                None => false,
            }
        };
        if group_live {
            kill_group_hard(pid).await;
        }
    });
}

/// Track a process group whose leader exited while members remain, until the
/// group empties. Polling keeps the pgid ours: POSIX does not reuse a pid
/// while a group with that id exists.
pub fn spawn_group_watcher(state: &Shared, task_id: &str, pgid: u32) {
    let state2 = state.clone();
    let tid = task_id.to_string();
    let clock = state.lock().unwrap().clock.clone();
    tokio::spawn(async move {
        loop {
            clock.sleep("group-poll", GROUP_POLL).await;
            if crate::sys::group_has_others(pgid) {
                continue;
            }
            if let Some(e) = state2.lock().unwrap().registry.tasks.get_mut(&tid) {
                e.group_lingering = false;
            }
            break;
        }
    });
}

fn handle_list(
    state: &Shared,
    conn_id: u64,
    _all: bool,
    session_id: Option<String>,
    paged: bool,
    after: Option<String>,
) -> Result<ListOk, ProtoError> {
    let after = match after.as_deref().map(parse_list_cursor) {
        None => None,
        Some(Some(c)) => Some(c),
        Some(None) => return Err(ProtoError::new(E_BAD_REQUEST, "list: bad `after` cursor")),
    };
    let st = state.lock().unwrap();
    let acc = access_for(&st, conn_id);
    let mut tasks: Vec<TaskRecord> = st
        .registry
        .tasks
        .values()
        .filter(|e| match &acc {
            // §3.3: extension connections only ever see their own session.
            Access::Extension(sid) => e.record.session_id == *sid,
            // cli is admin: explicit session filter, otherwise everything.
            Access::Cli => match &session_id {
                Some(s) => e.record.session_id == *s,
                None => true,
            },
        })
        .map(|e| e.record.clone())
        .collect();
    // task_id breaks started_at ties, so pages have a total order.
    tasks.sort_by(|a, b| (a.started_at, &a.task_id).cmp(&(b.started_at, &b.task_id)));
    if !paged && after.is_none() {
        return Ok(ListOk { tasks, next: None });
    }
    if let Some((at, tid)) = &after {
        tasks.retain(|r| (r.started_at, &r.task_id) > (*at, tid));
    }
    // Fill a page up to the frame budget; always at least one record.
    let mut size = 0usize;
    let mut take = 0usize;
    for r in &tasks {
        let n = serde_json::to_vec(r).map(|v| v.len() + 1).unwrap_or(0);
        if take > 0 && size + n > CHUNK_JSON_BUDGET {
            break;
        }
        size += n;
        take += 1;
    }
    let next = (take < tasks.len()).then(|| list_cursor(&tasks[take - 1]));
    tasks.truncate(take);
    Ok(ListOk { tasks, next })
}

/// A paged `list` resumes after `<started_at>/<task_id>` of the last record
/// sent, so a record removed between pages cannot shift the rest.
fn list_cursor(r: &TaskRecord) -> String {
    format!("{}/{}", r.started_at, r.task_id)
}

fn parse_list_cursor(s: &str) -> Option<(u64, String)> {
    let (at, tid) = s.split_once('/')?;
    Some((at.parse().ok()?, tid.to_string()))
}

fn handle_watch(
    state: &Shared,
    conn_id: u64,
    task_id: &str,
    on: bool,
) -> Result<UnitOk, ProtoError> {
    let mut st = state.lock().unwrap();
    let acc = access_for(&st, conn_id);
    let e = st.registry.visible_mut(task_id, &acc)?;
    if on {
        e.watchers.insert(conn_id);
        let base = e.delivered_cursor;
        if let Some(h) = st.conns.get(&conn_id) {
            h.written
                .lock()
                .unwrap()
                .entry(task_id.to_string())
                .or_insert(base);
        }
    } else {
        e.watchers.remove(&conn_id);
    }
    Ok(UnitOk {})
}

fn handle_shutdown_session(state: &Shared, conn_id: u64) -> Result<ShutdownSessionOk, ProtoError> {
    let (victims, home, sid) = {
        let mut st = state.lock().unwrap();
        let h = st
            .conns
            .get(&conn_id)
            .ok_or_else(|| ProtoError::new(E_INTERNAL, "connection gone"))?;
        let sid = match &h.session_id {
            Some(s) => s.clone(),
            None => {
                return Err(ProtoError::new(
                    E_SESSION_REQUIRED,
                    "shutdown_session requires an extension session",
                ))
            }
        };
        // (task_id, pgid, was_running): lingering groups of finished tasks are
        // killed too, but only running tasks are reported as stopped.
        let mut v = Vec::new();
        for e in st.registry.tasks.values_mut() {
            if e.record.session_id == sid && e.owns_live_group() {
                let running = e.record.status == TaskStatus::Running;
                if running {
                    e.request_kill(end_reason::SESSION_END);
                }
                v.push((e.record.task_id.clone(), e.record.pid, running));
            }
        }
        (v, st.home.clone(), sid)
    };
    for (tid, pid, running) in &victims {
        if *running {
            crate::events::emit(
                &home,
                Some(&sid),
                "task.stop",
                Some(tid),
                serde_json::json!({ "reason": "session-end" }),
            );
        }
        let _ = task::signal_group(*pid, task::SIGTERM);
        spawn_kill_reaper(state, tid, *pid);
    }
    Ok(ShutdownSessionOk {
        stopped: victims
            .into_iter()
            .filter(|(_, _, running)| *running)
            .map(|(t, _, _)| t)
            .collect(),
    })
}

async fn handle_acquire_agent(
    state: &Shared,
    conn_id: u64,
    child_id: &str,
    work_kind: &str,
    request_id: &str,
    tx: &OutTx,
) {
    let sid = state
        .lock()
        .unwrap()
        .conns
        .get(&conn_id)
        .and_then(|c| c.session_id.clone());
    let Some(sid) = sid else {
        respond::<AgentAdmissionOk>(
            tx,
            request_id,
            Err(ProtoError::new(
                E_SESSION_REQUIRED,
                "extension session required",
            )),
        )
        .await;
        return;
    };
    let result = {
        let mut st = state.lock().unwrap();
        let key = (sid.clone(), child_id.to_string());
        let budget =
            crate::capacity::max_agents(&st.home).unwrap_or(crate::capacity::DEFAULT_MAX_AGENTS);
        if st.agent_permits.contains_key(&key) {
            Some(AgentAdmissionOk {
                granted: true,
                rejection: None,
            })
        } else if st
            .pending_agents
            .iter()
            .any(|p| p.session_id == sid && p.request_id == request_id)
        {
            None // idempotent retry remains pending
        } else if st.agent_permits.len() >= budget {
            Some(AgentAdmissionOk {
                granted: false,
                rejection: Some("global_capacity".into()),
            })
        } else if admits_kind(
            &st.agent_permits,
            work_kind,
            kind_budget(&st.home, work_kind),
        ) {
            st.agent_permits.insert(key, work_kind.to_string());
            Some(AgentAdmissionOk {
                granted: true,
                rejection: None,
            })
        } else {
            st.pending_agents.push_back(PendingAgent {
                session_id: sid,
                child_id: child_id.into(),
                work_kind: work_kind.into(),
                request_id: request_id.into(),
                tx: tx.clone(),
            });
            None
        }
    };
    if let Some(result) = result {
        respond(tx, request_id, Ok(result)).await;
    }
}

fn handle_cancel_acquire(
    state: &Shared,
    conn_id: u64,
    request_id: &str,
) -> Result<UnitOk, ProtoError> {
    let mut st = state.lock().unwrap();
    let sid = st
        .conns
        .get(&conn_id)
        .and_then(|c| c.session_id.clone())
        .ok_or_else(|| ProtoError::new(E_SESSION_REQUIRED, "extension session required"))?;
    st.pending_agents
        .retain(|p| !(p.session_id == sid && p.request_id == request_id));
    Ok(UnitOk {})
}

fn handle_release_agent(
    state: &Shared,
    conn_id: u64,
    child_id: &str,
) -> Result<UnitOk, ProtoError> {
    let wake = {
        let mut st = state.lock().unwrap();
        let sid = st
            .conns
            .get(&conn_id)
            .and_then(|c| c.session_id.clone())
            .ok_or_else(|| ProtoError::new(E_SESSION_REQUIRED, "extension session required"))?;
        st.agent_permits.remove(&(sid, child_id.to_string()));
        wake_pending(&mut st)
    };
    for (tx, id) in wake {
        let _ = tx.try_send(encode_ok(
            &id,
            &AgentAdmissionOk {
                granted: true,
                rejection: None,
            },
        ));
    }
    Ok(UnitOk {})
}

fn handle_status(state: &Shared, _conn_id: u64) -> Result<StatusOk, ProtoError> {
    let st = state.lock().unwrap();
    // Status is read-only. Extensions need it so task_list can drop ghost
    // agents whose session is no longer connected. Shutdown stays cli-only.
    let sessions = st
        .sessions
        .iter()
        .map(|(sid, s)| SessionInfo {
            session_id: sid.clone(),
            pi_pid: s.pi_pid,
            connected: s.conn_id.is_some(),
            cwd: s.cwd.clone(),
            extension_version: s.extension_version.clone(),
            protocol: s.protocol,
            connected_at: s.connected_at,
            last_seen: if s.conn_id.is_some() {
                now_ms()
            } else {
                s.last_seen
            },
        })
        .collect();
    let running = st
        .registry
        .tasks
        .values()
        .filter(|e| e.record.status == TaskStatus::Running)
        .count();
    let terminal = st.registry.tasks.len() - running;
    Ok(StatusOk {
        version: crate::VERSION.to_string(),
        pid: std::process::id(),
        uptime_ms: now_ms().saturating_sub(st.started_at_ms),
        sessions,
        task_counts: TaskCounts { running, terminal },
        agent_capacity: AgentCapacity {
            used: st.agent_permits.len(),
            total: crate::capacity::max_agents(&st.home).map_err(|e| {
                ProtoError::new(E_INTERNAL, format!("invalid capacity config: {e}"))
            })?,
            by_kind: {
                let mut kinds = HashMap::new();
                for kind in st.agent_permits.values() {
                    let entry = kinds.entry(kind.clone()).or_insert_with(|| AgentKindCapacity {
                        used: 0,
                        total: kind_budget(&st.home, kind),
                    });
                    entry.used += 1;
                }
                kinds.entry("test".into()).or_insert(AgentKindCapacity { used: 0, total: kind_budget(&st.home, "test") });
                kinds.entry("test-suite".into()).or_insert(AgentKindCapacity { used: 0, total: kind_budget(&st.home, "test-suite") });
                kinds
            },
        },
        protocol: PROTOCOL,
        generation: st.generation,
        last_upgrade: st.last_upgrade.clone(),
        exe: crate::handover::exe_path()
            .ok()
            .map(|p| p.display().to_string()),
    })
}

fn handle_upgrade(state: &Shared, conn_id: u64) -> Result<UpgradeOk, ProtoError> {
    let st = state.lock().unwrap();
    if let Some(h) = st.conns.get(&conn_id) {
        if h.kind != ClientKind::Cli {
            return Err(ProtoError::new(
                E_FORBIDDEN,
                "upgrade is a cli-only operation",
            ));
        }
    }
    if st.shutdown {
        return Err(ProtoError::new(E_INTERNAL, SHUTTING_DOWN));
    }
    let generation = st.generation;
    drop(st);
    if !crate::handover::request(state, "cli") {
        return Err(ProtoError::new(
            E_INTERNAL,
            "an upgrade is already in progress",
        ));
    }
    Ok(UpgradeOk {
        from_version: crate::VERSION.to_string(),
        generation,
    })
}

fn handle_shutdown(state: &Shared, conn_id: u64) -> Result<UnitOk, ProtoError> {
    let mut st = state.lock().unwrap();
    // §3.3 marks shutdown as a cli message.
    if let Some(h) = st.conns.get(&conn_id) {
        if h.kind != ClientKind::Cli {
            return Err(ProtoError::new(
                E_FORBIDDEN,
                "shutdown is a cli-only operation",
            ));
        }
    }
    // §3.3: same graceful shutdown as the zero-connection path (§3.2).
    st.shutdown = true;
    st.shutdown_notify.notify_one();
    Ok(UnitOk {})
}

// ---------------------------------------------------------------------------
// Background tasks: output fanout, exit watch
// ---------------------------------------------------------------------------

/// Forward tee'd output chunks to watching connections as `output` events
/// (§3.3: output events only after watch).
/// Start (or restart) a task's tee pumps and output fanout from the
/// descriptors in its entry. A pipe already at EOF (None) is skipped.
pub fn start_task_io(state: &Shared, task_id: &str) {
    start_task_io_with_tee(state, task_id, task::start_tee);
}

// Keep tee creation at a narrow seam so tests can force a real pump read
// before the factory returns, without a global scheduling hook.
fn start_task_io_with_tee(
    state: &Shared,
    task_id: &str,
    start_tee: impl FnOnce(
        Option<std::os::fd::OwnedFd>, Option<std::os::fd::OwnedFd>,
        Arc<Mutex<task::OutputState>>, Option<std::fs::File>,
        mpsc::Sender<task::OutputChunk>, tokio::sync::watch::Receiver<bool>,
    ) -> std::io::Result<task::Tee>,
) {
    let mut st = state.lock().unwrap();
    let park = st.park_tx.subscribe();
    let Some(e) = st.registry.tasks.get_mut(task_id) else {
        return;
    };
    let (stdout, stderr) = (e.stdout_fd.take(), e.stderr_fd.take());
    let mirror = task::open_output_files(std::path::Path::new(&e.record.output_path))
        .ok()
        .map(|(_, err)| err);
    let (tx, rx) = tokio::sync::mpsc::channel(task::CHUNK_CHANNEL_CAP);
    // Snapshot before starting any pump: a new read must belong only to
    // the chunk channel, never also to this file carry.
    // What reached the file but not the watchers: after a park, the
    // incomplete UTF-8 tail the fanout held back. It is completed by the
    // next bytes, so the restarted fanout begins with it.
    let delivered = e.delivered_cursor;
    let total = e.output.lock().unwrap().total_size;
    let carry = if total > delivered {
        let want = (total - delivered).min(MAX_OUTPUT_READ) as usize;
        task::read_file_range(std::path::Path::new(&e.record.output_path), delivered, want)
            .map(|(b, _)| b)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    match start_tee(stdout, stderr, e.output.clone(), mirror, tx, park.clone()) {
        Ok(tee) => e.tee = Some(tee),
        Err(err) => {
            let home = st.home.clone();
            lifecycle::log_line(&home, &format!("tee {task_id}: {err}"));
            return;
        }
    }
    let tid = task_id.to_string();
    let state2 = state.clone();
    e.fanout = Some(tokio::spawn(run_output_fanout(
        state2, tid, rx, delivered, carry, park,
    )));
}

/// Push a task's output chunks to its watchers as `output` events.
///
/// Pipe reads split UTF-8 sequences arbitrarily. An incomplete trailing
/// sequence is held back and prepended to the next read, so events never
/// carry U+FFFD for valid text; `next_cursor` points at the first byte not
/// yet sent (`delivered_cursor`). The remainder is flushed at EOF, but kept
/// back when the pumps were parked: it is completed by the bytes that
/// follow once reading resumes.
async fn run_output_fanout(
    state: Shared,
    tid: String,
    mut rx: tokio::sync::mpsc::Receiver<task::OutputChunk>,
    delivered: u64,
    carry: Vec<u8>,
    park: tokio::sync::watch::Receiver<bool>,
) {
    let mut last_cursor = delivered + carry.len() as u64;
    let mut carry = carry;
    loop {
        let (bytes, next_cursor) = match rx.recv().await {
            Some(c) => {
                last_cursor = c.next_cursor;
                let mut data = std::mem::take(&mut carry);
                data.extend_from_slice(&c.bytes);
                let n = task::utf8_chunk_len(&data, usize::MAX, CHUNK_JSON_BUDGET, true);
                carry = data.split_off(n);
                (data, c.next_cursor - carry.len() as u64)
            }
            None if !carry.is_empty() && !*park.borrow() => {
                (std::mem::take(&mut carry), last_cursor)
            }
            None => break,
        };
        let chunk = task::OutputChunk { bytes, next_cursor };
        let targets: Vec<OutTx> = {
            let mut st = state.lock().unwrap();
            let watcher_ids: Vec<u64> = match st.registry.tasks.get_mut(&tid) {
                Some(e) => {
                    e.record.output_size = e.record.output_size.max(last_cursor); // monotonic (§3.4)
                    e.delivered_cursor = chunk.next_cursor;
                    e.watchers.iter().copied().collect()
                }
                None => break,
            };
            watcher_ids
                .iter()
                .filter_map(|cid| st.conns.get(cid).map(|h| h.tx.clone()))
                .collect()
        };
        if !targets.is_empty() && !chunk.bytes.is_empty() {
            let ev = encode_event_bytes(&EventKind::Output {
                task_id: tid.clone(),
                chunk: String::from_utf8_lossy(&chunk.bytes).into_owned(),
                next_cursor: chunk.next_cursor,
            });
            for tx in targets {
                let _ = tx.try_send(OutFrame::output(ev.clone(), &tid, chunk.next_cursor));
            }
        }
    }
}

/// How a task's process ended, as far as we could observe it.
#[derive(Clone, Copy)]
struct Outcome {
    code: Option<i32>,
    signal: Option<i32>,
    /// Only the runner's status line carries it (see `crate::runner`).
    usage: Option<crate::runner::Usage>,
}

impl Outcome {
    /// The runner's own wait status: it wrote no report, so no usage.
    fn of(status: Option<std::process::ExitStatus>) -> Self {
        Outcome {
            code: status.and_then(|s| s.code()),
            signal: status.and_then(|s| s.signal()),
            usage: None,
        }
    }

    fn reported(r: &crate::runner::Reported) -> Self {
        Outcome {
            code: r.code,
            signal: r.signal,
            usage: r.usage,
        }
    }
}

fn normalize_killed_outcome(kill_requested: bool, outcome: Outcome) -> Outcome {
    if !kill_requested || outcome.signal.is_some() {
        return outcome;
    }
    match outcome.code {
        Some(code) if code == 128 + task::SIGTERM => Outcome {
            code: None,
            signal: Some(task::SIGTERM),
            ..outcome
        },
        Some(code) if code == 128 + task::SIGKILL => Outcome {
            code: None,
            signal: Some(task::SIGKILL),
            ..outcome
        },
        _ => outcome,
    }
}

/// What may be left of the task's process group once its command ended.
enum Leftover {
    /// Nothing: the runner saw an empty group.
    None,
    /// Descendants remain; the runner guards them and its exit means empty.
    Guarded,
    /// Unknown (the runner died without reporting): probe the group.
    Probe,
}

enum FirstSeen {
    Report(Option<crate::runner::Reported>),
    RunnerExit(Option<std::process::ExitStatus>),
}

/// Read the runner's status line (see `crate::runner`). `line` keeps a
/// partial read across calls. None at EOF without a well-formed line.
async fn read_status_line(
    rx: &mut tokio::net::unix::pipe::Receiver,
    line: &mut Vec<u8>,
) -> Option<crate::runner::Reported> {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 64];
    loop {
        if let Some(i) = line.iter().position(|b| *b == b'\n') {
            return crate::runner::parse_status(std::str::from_utf8(&line[..i]).ok()?);
        }
        match rx.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => line.extend_from_slice(&buf[..n]),
        }
    }
}

/// Await the task's end (or the hard timeout), then finalize the record.
///
/// The runner reports the command's real status on its status pipe, and its
/// own exit means its process group is empty (§3.4). Without a report (the
/// runner was SIGKILLed with its group, e.g. after a stop grace or the
/// timeout), the runner's wait status stands in: it died of the same
/// signal as the group.
///
/// The watch resumes from the entry (`exit_phase`, runner, status pipe,
/// partial status line) and, when parked, puts all of it back there.
pub fn spawn_exit_watch(state: &Shared, task_id: &str) {
    let handle = tokio::spawn(run_exit_watch(state.clone(), task_id.to_string()));
    if let Some(e) = state.lock().unwrap().registry.tasks.get_mut(task_id) {
        e.exit_watch = Some(handle);
    }
}

enum Parked {
    Yes,
}

async fn run_exit_watch(state: Shared, tid: String) {
    let (runner, status_rx, line, phase, pid, deadline, timed_out, mut park) = {
        let mut st = state.lock().unwrap();
        let park = st.park_tx.subscribe();
        match st.registry.tasks.get_mut(&tid) {
            Some(e) => (
                e.child.take(),
                e.status_rx.take(),
                std::mem::take(&mut e.status_partial),
                e.exit_phase,
                e.record.pid,
                e.timeout_deadline_ms,
                e.timed_out,
                park,
            ),
            None => return,
        }
    };
    let Some(mut runner) = runner else { return };
    let mut line = line;
    // Put everything back for whoever resumes the watch.
    let put_back = |runner: task::RunnerProc,
                    status_rx: Option<tokio::net::unix::pipe::Receiver>,
                    line: Vec<u8>| {
        if let Some(e) = state.lock().unwrap().registry.tasks.get_mut(&tid) {
            e.child = Some(runner);
            e.status_rx = status_rx;
            e.status_partial = line;
        }
    };
    if phase == registry::ExitPhase::AwaitReport {
        let Some(mut status_rx) = status_rx else {
            return;
        };
        let first = {
            let timeout_left = deadline.map(|d| d.saturating_sub(now_ms()));
            let timeout = async {
                match (timeout_left, timed_out) {
                    (Some(ms), false) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    _ => std::future::pending().await,
                }
            };
            tokio::pin!(timeout);
            let mut timed_out = timed_out;
            loop {
                let wait = runner.wait();
                tokio::pin!(wait);
                let report = read_status_line(&mut status_rx, &mut line);
                tokio::pin!(report);
                tokio::select! {
                    biased;
                    _ = task::parked(&mut park) => break Err(Parked::Yes),
                    r = &mut report => break Ok(FirstSeen::Report(r)),
                    s = &mut wait => break Ok(FirstSeen::RunnerExit(s)),
                    _ = &mut timeout, if !timed_out => {
                        // §3.3: timeout_ms is a hard kill ceiling.
                        timed_out = true;
                        {
                            let mut st = state.lock().unwrap();
                            if let Some(e) = st.registry.tasks.get_mut(&tid) {
                                e.timed_out = true;
                                if e.record.status == TaskStatus::Running {
                                    e.request_kill(end_reason::TIMEOUT);
                                }
                            }
                        }
                        kill_group_hard(pid).await;
                    }
                }
            }
        };
        let first = match first {
            Ok(f) => f,
            Err(Parked::Yes) => {
                put_back(runner, Some(status_rx), line);
                return;
            }
        };
        match first {
            FirstSeen::Report(Some(r)) => {
                let outcome = Outcome::reported(&r);
                let leftover = if r.linger {
                    Leftover::Guarded
                } else {
                    Leftover::None
                };
                wait_tee_drained(&state, &tid, !r.linger).await;
                finalize_exit(&state, &tid, outcome, leftover);
                set_exit_phase(&state, &tid, registry::ExitPhase::AwaitRunnerExit);
            }
            FirstSeen::Report(None) => {
                let s = runner.wait().await;
                wait_tee_drained(&state, &tid, false).await;
                finalize_exit(&state, &tid, Outcome::of(s), Leftover::Probe);
                set_exit_phase(&state, &tid, registry::ExitPhase::Done);
                return;
            }
            FirstSeen::RunnerExit(s) => {
                // A report written just before the runner exited may still
                // be in the pipe; the write end is closed now, so this ends.
                match read_status_line(&mut status_rx, &mut line).await {
                    Some(r) => {
                        let leftover = if r.linger {
                            Leftover::Probe
                        } else {
                            Leftover::None
                        };
                        wait_tee_drained(&state, &tid, !r.linger).await;
                        finalize_exit(&state, &tid, Outcome::reported(&r), leftover);
                    }
                    None => {
                        wait_tee_drained(&state, &tid, false).await;
                        finalize_exit(&state, &tid, Outcome::of(s), Leftover::Probe)
                    }
                }
                set_exit_phase(&state, &tid, registry::ExitPhase::Done);
                return;
            }
        }
    }
    // AwaitRunnerExit: the runner exits once its group is empty (a guardian)
    // or right away; either way it is reaped here.
    tokio::select! {
        biased;
        _ = task::parked(&mut park) => {
            put_back(runner, None, Vec::new());
            return;
        }
        _ = runner.wait() => {}
    }
    if let Some(e) = state.lock().unwrap().registry.tasks.get_mut(&tid) {
        e.group_lingering = false;
        e.exit_phase = registry::ExitPhase::Done;
    }
}

fn set_exit_phase(state: &Shared, tid: &str, phase: registry::ExitPhase) {
    if let Some(e) = state.lock().unwrap().registry.tasks.get_mut(tid) {
        e.exit_phase = phase;
    }
}

/// The record's terminal `output_size` is snapshotted at finalize, so the
/// tee pumps must drain the command's last output first. EOF arrives
/// moments after the runner (and any leftover) closes its inherited write
/// ends. A parked pump (in-place upgrade) has finished too, so it never waits.
///
/// `alone`: the runner reported an empty group, so EOF should come; wait for
/// it rather than for a quiet spell (a pump behind a busy runtime or a full
/// fanout channel went quiet for over 50 ms on CI and lost the last 48 KiB).
/// Capped at 2 s: a process that left the group (`setsid`) can hold a write
/// end forever, and then the snapshot misses whatever it writes after the
/// cap. The output file still gets it; the startup scan and `inspect` read
/// the size from the file.
///
/// Otherwise a leftover may keep the pipes open for good, so the wait also
/// ends once output goes quiet (~50 ms), capped at 500 ms: a stuck pump must
/// not delay the exit event.
async fn wait_tee_drained(state: &Shared, tid: &str, alone: bool) {
    let (rounds, quiet_rounds) = if alone { (400, u32::MAX) } else { (100, 10) };
    let mut last = u64::MAX;
    let mut quiet = 0u32;
    for _ in 0..rounds {
        let size = {
            let st = state.lock().unwrap();
            let Some(e) = st.registry.tasks.get(tid) else {
                return;
            };
            match &e.tee {
                Some(t) if !(t.stdout.is_finished() && t.stderr.is_finished()) => {
                    e.output.lock().unwrap().total_size
                }
                _ => return,
            }
        };
        quiet = if size == last { quiet + 1 } else { 0 };
        if quiet >= quiet_rounds {
            return;
        }
        last = size;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Runner microseconds to the record's milliseconds. Round up so a
/// sub-millisecond measurement is 1 ms rather than 0: `Some(0)` still
/// counts as measured in `stats` and would drag `avg_cores` toward 0.
fn usage_ms(us: u64) -> u64 {
    us.div_ceil(1000)
}

/// Map an observed exit to a terminal status, persist the record, wake
/// `wait`ers, and push `task_exited` to the owning session (§3.3/§3.4).
fn finalize_exit(state: &Shared, task_id: &str, outcome: Outcome, leftover: Leftover) {
    let mut lingering = None;
    let (sid, event) = {
        let mut st = state.lock().unwrap();
        let home = st.home.clone();
        let Some(entry) = st.registry.tasks.get_mut(task_id) else {
            return;
        };
        if entry.record.status.is_terminal() {
            return; // already finalized (e.g. shutdown force-pass)
        }
        let Outcome {
            code,
            signal,
            usage,
        } = normalize_killed_outcome(entry.kill_requested, outcome);
        let now = now_ms();
        entry.record.exit_code = code;
        if let Some(u) = usage {
            entry.record.cpu_user_ms = Some(usage_ms(u.cpu_user_us));
            entry.record.cpu_sys_ms = Some(usage_ms(u.cpu_sys_us));
            entry.record.max_rss_kb = Some(u.max_rss_kb);
        }
        entry.record.signal = signal.map(signal_name);
        entry.record.ended_at = Some(now);
        entry.record.output_size = entry.output.lock().unwrap().total_size;
        entry.record.status = registry::terminal_status(entry.kill_requested, code, signal);
        // Why it ended: our kill's reason; otherwise a natural exit.
        entry.record.end_reason = Some(
            entry
                .kill_reason
                .clone()
                .unwrap_or_else(|| end_reason::EXITED.to_string()),
        );
        if let Err(e) = registry::persist_record(&home, &entry.record) {
            lifecycle::log_line(
                &home,
                &format!("persist {} failed: {e}", entry.record.task_id),
            );
        }
        // The command is gone; descendants it backgrounded may not be.
        let pgid = entry.record.pid;
        match leftover {
            Leftover::None => {}
            Leftover::Guarded => entry.group_lingering = true,
            Leftover::Probe => {
                if crate::sys::group_has_others(pgid) {
                    entry.group_lingering = true;
                    lingering = Some(pgid);
                }
            }
        }
        let event = EventKind::TaskExited {
            task_id: task_id.to_string(),
            exit_code: code,
            signal: entry.record.signal.clone(),
            duration_ms: now.saturating_sub(entry.record.started_at),
            output_path: entry.record.output_path.clone(),
            output_size: entry.record.output_size,
            ts: now,
            end_reason: entry.record.end_reason.clone(),
        };
        // The events.jsonl line goes first, still under the state lock: once
        // anyone can see the task as finished (`wait`, `list`), `task.exit`
        // is on disk, ahead of whatever that observer does next.
        let sid = entry.record.session_id.clone();
        log_task_exit(&home, &sid, &event, &entry.record);
        let _ = entry.status_tx.send(entry.record.status);
        (sid, event)
    };
    if let Some(pgid) = lingering {
        spawn_group_watcher(state, task_id, pgid);
    }
    send_event_to_session(state, &sid, event);
}

/// events.jsonl `task.exit` from a task_exited event, plus the record's
/// resource usage when the runner reported it (absent keys otherwise).
fn log_task_exit(home: &std::path::Path, sid: &str, ev: &EventKind, rec: &TaskRecord) {
    if let EventKind::TaskExited {
        task_id,
        exit_code,
        signal,
        duration_ms,
        end_reason,
        ..
    } = ev
    {
        let mut fields = serde_json::json!({
            "exit_code": exit_code,
            "signal": signal,
            "end_reason": end_reason,
            "duration_ms": duration_ms,
        });
        for (k, v) in [
            ("cpu_user_ms", rec.cpu_user_ms),
            ("cpu_sys_ms", rec.cpu_sys_ms),
            ("max_rss_kb", rec.max_rss_kb),
        ] {
            if let Some(v) = v {
                fields[k] = v.into();
            }
        }
        crate::events::emit(home, Some(sid), "task.exit", Some(task_id), fields);
    }
}

// ---------------------------------------------------------------------------
// Graceful shutdown (§3.2)
// ---------------------------------------------------------------------------

/// Re-probe every leftover group (`TaskEntry::refresh_lingering`). Returns
/// whether any still lingers.
fn refresh_lingering_groups(state: &Shared) -> bool {
    let mut st = state.lock().unwrap();
    let mut any = false;
    for e in st.registry.tasks.values_mut() {
        any |= e.refresh_lingering();
    }
    any
}

async fn graceful_shutdown(state: &Shared) {
    let home = state.lock().unwrap().home.clone();
    lifecycle::log_line(&home, "graceful shutdown: terminating running tasks");

    // 0) Look at leftover groups as they are now, not as the last group
    //    poll saw them: a group that has emptied needs no SIGTERM and must
    //    not hold shutdown in the grace (under the manual test clock the
    //    poll never runs unstepped, so a stale flag would hold it forever).
    //    Right after its leader is reaped, `kill(-pgid, 0)` can answer EPERM
    //    for a moment (macOS, seen under load), so a group that still looks
    //    alive gets a second probe after a short real-time pause.
    if refresh_lingering_groups(state) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        refresh_lingering_groups(state);
    }

    // 1) SIGTERM every process group that may have members: running tasks,
    //    and finished tasks whose leader left descendants behind (§3.2).
    let (pids, running): (Vec<u32>, usize) = {
        let mut st = state.lock().unwrap();
        let mut running = 0;
        let pids = st
            .registry
            .tasks
            .values_mut()
            .filter(|e| e.owns_live_group())
            .map(|e| {
                if e.record.status == TaskStatus::Running {
                    e.request_kill(end_reason::MANAGER_SHUTDOWN); // disk says `killed` (§3.2)
                    running += 1;
                }
                e.record.pid
            })
            .collect();
        (pids, running)
    };
    for pid in &pids {
        let _ = task::signal_group(*pid, task::SIGTERM);
    }
    if !pids.is_empty() {
        // 2) 2s grace, then SIGKILL every group that may still have members,
        //    even if its leader already died (SIGTERM-ignoring descendants).
        let clock = state.lock().unwrap().clock.clone();
        clock.sleep("shutdown-grace", KILL_GRACE).await;
        // A group that emptied during the grace is no longer ours to signal.
        refresh_lingering_groups(state);
        let survivors: Vec<u32> = {
            state
                .lock()
                .unwrap()
                .registry
                .tasks
                .values()
                .filter(|e| e.owns_live_group())
                .map(|e| e.record.pid)
                .collect()
        };
        for pid in survivors {
            kill_group_hard(pid).await;
        }
        // Let exit watchers observe and persist.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // 3) Force-finalize anything still marked running (safety net), with
    //    end_reason "manager-shutdown" (also in manager.log below).
    {
        let mut st = state.lock().unwrap();
        let home = st.home.clone();
        let now = now_ms();
        for e in st.registry.tasks.values_mut() {
            if e.record.status == TaskStatus::Running {
                e.record.status = TaskStatus::Killed;
                e.record.ended_at = Some(now);
                e.record.output_size = e.output.lock().unwrap().total_size;
                e.record.end_reason = Some(
                    e.kill_reason
                        .clone()
                        .unwrap_or_else(|| end_reason::MANAGER_SHUTDOWN.to_string()),
                );
                if let Err(err) = registry::persist_record(&home, &e.record) {
                    lifecycle::log_line(
                        &home,
                        &format!("persist {} failed: {err}", e.record.task_id),
                    );
                }
                let ev = EventKind::TaskExited {
                    task_id: e.record.task_id.clone(),
                    exit_code: None,
                    signal: None,
                    duration_ms: now.saturating_sub(e.record.started_at),
                    output_path: e.record.output_path.clone(),
                    output_size: e.record.output_size,
                    ts: now,
                    end_reason: e.record.end_reason.clone(),
                };
                log_task_exit(&home, &e.record.session_id, &ev, &e.record);
                let _ = e.status_tx.send(TaskStatus::Killed);
            }
        }
    }
    if running > 0 {
        lifecycle::log_line(
            &home,
            &format!("killed {running} task(s) (reason: manager_shutdown)"),
        );
    }
    if pids.len() > running {
        lifecycle::log_line(
            &home,
            &format!(
                "killed {} leftover process group(s) of finished tasks (reason: manager_shutdown)",
                pids.len() - running
            ),
        );
    }
    // Let pending responses (e.g. the shutdown ack) flush to clients.
    tokio::time::sleep(Duration::from_millis(250)).await;
    // 4) Remove socket/pid files and exit (§3.2).
    let _ = lifecycle::cleanup_stale_files(&home);
    crate::events::emit(
        &home,
        None,
        "daemon.shutdown",
        None,
        serde_json::json!({ "pid": std::process::id(), "killed_tasks": running }),
    );
    lifecycle::log_line(&home, "shutdown complete");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn admits_kind(permits: &HashMap<(String, String), String>, kind: &str, budget: usize) -> bool {
    permits
        .values()
        .filter(|value| value.as_str() == kind)
        .count()
        < budget
}

fn kind_budget(home: &std::path::Path, kind: &str) -> usize {
    crate::capacity::max_kind(home, kind).unwrap_or(usize::MAX)
}

fn wake_pending(st: &mut DaemonState) -> Vec<(OutTx, String)> {
    let mut wake = Vec::new();
    let global =
        crate::capacity::max_agents(&st.home).unwrap_or(crate::capacity::DEFAULT_MAX_AGENTS);
    while st.agent_permits.len() < global {
        let Some(pending) = st.pending_agents.front() else {
            break;
        };
        if !admits_kind(
            &st.agent_permits,
            &pending.work_kind,
            kind_budget(&st.home, &pending.work_kind),
        ) {
            break;
        }
        let pending = st.pending_agents.pop_front().unwrap();
        st.agent_permits
            .insert((pending.session_id, pending.child_id), pending.work_kind);
        wake.push((pending.tx, pending.request_id));
    }
    wake
}

fn admit_agent(
    permits: &mut HashMap<(String, String), String>,
    key: (String, String),
    budget: usize,
) -> AgentAdmissionOk {
    if permits.contains_key(&key) {
        return AgentAdmissionOk {
            granted: true,
            rejection: None,
        };
    }
    if permits.len() >= budget {
        return AgentAdmissionOk {
            granted: false,
            rejection: Some("global_capacity".into()),
        };
    }
    permits.insert(key, "unknown".into());
    AgentAdmissionOk {
        granted: true,
        rejection: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a tee that runs before its factory returns must not have
    /// its bytes read again as carry. Also preserve the incomplete raw byte
    /// across the same park/restart seam used by an in-place upgrade.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn utf8_watch_output_is_not_duplicated_when_tee_starts_eagerly() {
        use std::os::fd::AsRawFd;
        let home = std::env::temp_dir().join(format!("pi-famulus-fanout-{}-{}", std::process::id(), now_ms()));
        let path = registry::task_output_path(&home, "sess-utf8", "mon_utf8");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (file, _) = task::open_output_files(&path).unwrap();
        let output = Arc::new(Mutex::new(task::OutputState::new(Some(file), 0)));
        let record: TaskRecord = serde_json::from_value(serde_json::json!({
            "task_id":"mon_utf8", "session_id":"sess-utf8", "kind":"monitor",
            "command":"split UTF-8 pipe producer", "cwd":"/tmp", "pid":0,
            "status":"running", "exit_code":null, "signal":null,
            "started_at":0, "ended_at":null, "output_path":path, "output_size":0
        })).unwrap();
        let (read, write) = crate::sys::pipe_cloexec().unwrap();
        let mut entry = TaskEntry::bare(record, output.clone());
        entry.stdout_fd = Some(read);
        entry.watchers.insert(1);
        let mut registry = Registry::new(home.clone());
        registry.tasks.insert("mon_utf8".into(), entry);
        let state = Arc::new(Mutex::new(DaemonState::new(home.clone(), registry, false)));
        let (tx, mut events) = mpsc::channel(8);
        let writer = tokio::spawn(std::future::pending::<()>());
        state.lock().unwrap().conns.insert(1, ConnHandle {
            kind: ClientKind::Extension, session_id: Some("sess-utf8".into()), tx,
            die: Arc::new(Notify::new()), written: Arc::new(Mutex::new(HashMap::new())),
            writer: writer.abort_handle(),
        });
        // Real initial bytes, not a fabricated OutputChunk. Wait only for the
        // actual append (with a failure watchdog), never for a fixed sleep.
        crate::sys::write_raw(write.as_raw_fd(), b"a\xe4").unwrap();
        let eager_tee = |expected| move |stdout, stderr, output: Arc<Mutex<task::OutputState>>, mirror, tx, park| {
            let tee = task::start_tee(stdout, stderr, output.clone(), mirror, tx, park)?;
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while output.lock().unwrap().total_size < expected {
                assert!(std::time::Instant::now() < until, "real tee did not append {expected} bytes");
                std::thread::yield_now();
            }
            Ok(tee)
        };
        start_task_io_with_tee(&state, "mon_utf8", eager_tee(2));
        let first = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        let first: serde_json::Value = serde_json::from_slice(&first.bytes).unwrap();
        assert_eq!(first["task_id"], "mon_utf8");
        assert_eq!(first["chunk"], "a", "{first}");
        assert_eq!(first["next_cursor"], 1);

        let (tee, fanout) = {
            let mut st = state.lock().unwrap();
            st.park_tx.send_replace(true);
            let e = st.registry.tasks.get_mut("mon_utf8").unwrap();
            (e.tee.take().unwrap(), e.fanout.take().unwrap())
        };
        let stdout = tokio::time::timeout(Duration::from_secs(3), tee.stdout)
            .await.expect("stdout tee did not park").unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(3), tee.stderr)
            .await.expect("stderr tee did not park").unwrap().is_none());
        tokio::time::timeout(Duration::from_secs(3), fanout)
            .await.expect("fanout did not park").unwrap();
        {
            let mut st = state.lock().unwrap();
            let e = st.registry.tasks.get_mut("mon_utf8").unwrap();
            assert_eq!(e.delivered_cursor, 1);
            assert_eq!(e.output.lock().unwrap().total_size, 2);
            e.stdout_fd = stdout;
            st.park_tx.send_replace(false);
        }
        assert!(events.try_recv().is_err(), "incomplete byte must not flush at park");
        crate::sys::write_raw(write.as_raw_fd(), b"\xb8\xadb\n").unwrap();
        start_task_io_with_tee(&state, "mon_utf8", eager_tee(6));
        drop(write);
        let last = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        let last: serde_json::Value = serde_json::from_slice(&last.bytes).unwrap();
        assert_eq!(last["task_id"], "mon_utf8");
        assert_eq!(last["chunk"], "中b\n", "{last}");
        assert_eq!(last["next_cursor"], 6);
        let (tee, fanout) = {
            let mut st = state.lock().unwrap();
            let e = st.registry.tasks.get_mut("mon_utf8").unwrap();
            (e.tee.take().unwrap(), e.fanout.take().unwrap())
        };
        assert!(tokio::time::timeout(Duration::from_secs(3), tee.stdout)
            .await.expect("stdout tee did not reach EOF").unwrap().is_none());
        assert!(tokio::time::timeout(Duration::from_secs(3), tee.stderr)
            .await.expect("stderr tee did not reach EOF").unwrap().is_none());
        tokio::time::timeout(Duration::from_secs(3), fanout)
            .await.expect("fanout did not drain at EOF").unwrap();
        assert!(events.try_recv().is_err(), "no repeated output at EOF");
        {
            let st = state.lock().unwrap();
            let e = &st.registry.tasks["mon_utf8"];
            assert_eq!(e.delivered_cursor, 6);
            assert_eq!(e.record.output_size, 6);
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"a\xe4\xb8\xadb\n");
        writer.abort();
        drop(state);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn global_admission_grants_rejects_and_is_idempotent() {
        let mut permits = HashMap::new();
        assert!(admit_agent(&mut permits, ("s1".into(), "ch1".into()), 1).granted);
        let rejected = admit_agent(&mut permits, ("s2".into(), "ch2".into()), 1);
        assert_eq!(rejected.rejection.as_deref(), Some("global_capacity"));
        assert!(admit_agent(&mut permits, ("s1".into(), "ch1".into()), 1).granted);
    }

    #[test]
    fn session_id_validation() {
        assert!(valid_session_id("abc-DEF_123.x"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../escape"));
        assert!(!valid_session_id("a/b"));
        assert!(!valid_session_id("a b"));
        assert!(!valid_session_id(&"x".repeat(200)));
    }

    #[test]
    fn sub_millisecond_cpu_rounds_up_to_one_ms() {
        assert_eq!(usage_ms(0), 0);
        assert_eq!(usage_ms(1), 1);
        assert_eq!(usage_ms(999), 1);
        assert_eq!(usage_ms(1000), 1);
        assert_eq!(usage_ms(1001), 2);
    }

    #[test]
    fn killed_shell_exit_code_143_is_reported_as_sigterm() {
        let outcome = normalize_killed_outcome(
            true,
            Outcome {
                code: Some(143),
                signal: None,
                usage: None,
            },
        );
        assert_eq!(outcome.code, None);
        assert_eq!(outcome.signal, Some(task::SIGTERM));
    }
}
