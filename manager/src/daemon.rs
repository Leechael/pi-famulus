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
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
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
/// Best-effort live process-group CPU refresh interval.
const LIVE_CPU_POLL: Duration = Duration::from_secs(2);

type OutTx = mpsc::Sender<OutFrame>;

/// One frame queued for a connection's writer. Output events say which
/// task and cursor they carry, so the writer can record how far the client
/// really got (`ConnHandle::written`). `ack` frames (the intercom port
/// relay) resolve only after the bytes hit the socket: delivered/read
/// receipt semantics depend on that ordering.
pub struct OutFrame {
    bytes: Arc<Vec<u8>>,
    output: Option<(String, u64)>,
    ack: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

impl OutFrame {
    fn plain(bytes: Arc<Vec<u8>>) -> OutFrame {
        OutFrame {
            bytes,
            output: None,
            ack: None,
        }
    }
    fn output(bytes: Arc<Vec<u8>>, task_id: &str, next_cursor: u64) -> OutFrame {
        OutFrame {
            bytes,
            output: Some((task_id.to_string(), next_cursor)),
            ack: None,
        }
    }
    fn ack(
        bytes: Arc<Vec<u8>>,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    ) -> OutFrame {
        OutFrame {
            bytes,
            output: None,
            ack: Some(ack),
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConnRole {
    Parent,
    Cli,
    Child,
}

pub struct ConnHandle {
    pub kind: ClientKind,
    pub role: ConnRole,
    pub child_id: Option<String>,
    pub session_id: Option<String>,
    pub tx: OutTx,
    /// Fired to make the connection's read loop exit (session rebind).
    pub die: Arc<Notify>,
    pub written: Written,
    /// Its writer task, aborted when an upgrade cannot wait for it to flush.
    #[cfg_attr(windows, allow(dead_code))]
    pub writer: tokio::task::AbortHandle,
    /// Holds a supervision lease (`serve` / `top`) until it disconnects.
    pub supervisor: bool,
}

pub struct SessionEntry {
    pub pi_pid: u32,
    pub conn_id: Option<u64>,
    pub cwd: Option<String>,
    pub extension_version: Option<String>,
    pub protocol: Option<u32>,
    pub connected_at: u64,
    pub last_seen: u64,
    /// Set by shutdown_session so a later socket close is not a crash.
    pub ended_clean: bool,
}

pub struct PendingAgent {
    session_id: String,
    child_id: String,
    work_kind: String,
    /// One waiter per (session, child); aliases let a resent request id share it.
    request_ids: Vec<String>,
    tx: OutTx,
    /// Waiters already queued when this one joined.
    queued_behind: u64,
    /// A sibling-started turn: drop the permit when the child settles.
    release_on_settle: bool,
    /// Hosted child that asked for this permit. None means the parent session.
    requester_child: Option<String>,
}

#[derive(Clone)]
pub struct AgentPermit {
    pub(crate) work_kind: String,
    /// Only this request id owns cancellation authority for the held permit.
    pub(crate) request_id: String,
    /// Set when a hosted sibling acquired the permit. Released on settle.
    pub(crate) release_on_settle: bool,
    /// The sibling prompt has been seen running, so a later idle is its settle.
    pub(crate) turn_started: bool,
}

pub(crate) struct UndeliveredGrant {
    child_id: String,
    queued_behind: Option<u64>,
    /// None: the parent session's connection. Some: that hosted child only.
    requester_child: Option<String>,
}

struct AgentGrant {
    session_id: String,
    child_id: String,
    request_id: String,
    tx: OutTx,
    queued_behind: Option<u64>,
}

/// Asks the accept loop to make the messaging bridge match the current
/// leases (Supervised iff some connection holds one).
struct SupervisionRequest {
    reply: Option<tokio::sync::oneshot::Sender<String>>,
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
    #[cfg_attr(windows, allow(dead_code))]
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
    pub agent_permits: HashMap<(String, String), AgentPermit>,
    /// FIFO requests waiting for a per-kind slot, one entry per child.
    pub pending_agents: std::collections::VecDeque<PendingAgent>,
    /// Queued grants that hit a full/closed output channel; retried on session activity.
    pub undelivered_agent_grants: HashMap<(String, String), UndeliveredGrant>,
    /// One-shot cancellation tombstones for cancel/acquire dispatch races.
    pub cancelled_acquires: HashSet<(String, String, String)>,
    /// Serializes budget reads with admission commits without holding `state`
    /// across config metadata/file I/O.
    pub agent_admission: Arc<Mutex<()>>,
    /// Cached global budget; per-kind settings remain part of CapacityConfig.
    pub agent_budget: Arc<Mutex<crate::capacity::MaxAgentsCache>>,
    /// Hosted `pi --mode rpc` subagents.
    pub hosted: crate::hosted::Hosted,
    agent_notices: Option<mpsc::UnboundedReceiver<crate::hosted::Notice>>,
    supervision: mpsc::UnboundedSender<SupervisionRequest>,
    supervision_rx: Option<mpsc::UnboundedReceiver<SupervisionRequest>>,
    /// The messaging bridge runtime currently served over `intercom`
    /// requests; `None` while the bridge is down (and why, in
    /// `intercom_error`). Swapped by the accept loop on every bridge
    /// (re)start; brief locked access only (clone the Arc).
    pub intercom_rt: Arc<Mutex<Option<crate::intercom::Shared>>>,
    /// Why the bridge is down, when `intercom_rt` is `None`.
    pub intercom_error: Arc<Mutex<Option<String>>>,
    /// One intercom op queue per manager connection that sent an
    /// `intercom` request (protocol 7). The map lock is held only for
    /// get/insert/remove, never across an op: a worker task per connection
    /// owns the port and serializes its ops in wire order.
    pub(crate) intercom_ports: Arc<Mutex<HashMap<u64, IntercomEntry>>>,
}

/// One connection's intercom op queue. A worker task owns the port and
/// serves the queue in wire order; close and reset only signal it, so a
/// slow op never delays teardown or the accept loop.
pub(crate) struct IntercomEntry {
    tx: mpsc::Sender<IntercomOp>,
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Bumped by reset_intercom_ports: the worker drops its port and
    /// answers queued ops with E_INTERCOM_RESET. Keeping the entry (and
    /// the worker) preserves wire order across the reset.
    reset: tokio::sync::watch::Sender<u64>,
}

/// One queued intercom op: the client-protocol request plus the request id
/// to answer with.
struct IntercomOp {
    id: String,
    req: serde_json::Value,
}

/// Bound on remembered start keys.
const START_KEYS_MAX: usize = 512;
/// Admission waiters are bounded per session and across the daemon.
const PENDING_AGENTS_PER_SESSION_MAX: usize = 64;
const PENDING_AGENTS_GLOBAL_MAX: usize = 256;
/// A child can have a small number of in-flight request-id aliases, but never
/// an unbounded number of responses attached to one queue entry.
const PENDING_AGENT_REQUEST_IDS_MAX: usize = 8;

impl DaemonState {
    fn new(home: PathBuf, registry: Registry, foreground: bool) -> DaemonState {
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let (supervision, supervision_rx) = mpsc::unbounded_channel();
        let hosted_home = home.clone();
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
            undelivered_agent_grants: HashMap::new(),
            cancelled_acquires: HashSet::new(),
            agent_admission: Arc::new(Mutex::new(())),
            agent_budget: Arc::new(Mutex::new(crate::capacity::MaxAgentsCache::default())),
            hosted: crate::hosted::Hosted::new(hosted_home, notice_tx),
            agent_notices: Some(notice_rx),
            supervision,
            supervision_rx: Some(supervision_rx),
            intercom_rt: Arc::new(Mutex::new(None)),
            intercom_error: Arc::new(Mutex::new(None)),
            intercom_ports: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(home: PathBuf, foreground: bool, handover: Option<PathBuf>) -> i32 {
    if let Some(path) = handover {
        #[cfg(unix)]
        {
            return run_restored(home, path).await;
        }
        #[cfg(windows)]
        {
            let _ = path;
            eprintln!("pi-famulus: in-place upgrade is not supported on Windows");
            return 1;
        }
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
    // Bind the well-known socket / named pipe (§3.1).
    let sock = lifecycle::socket_path(&home);
    let listener = match crate::ipc::bind(&home).await {
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
    start_agent_loops(&state);
    serve(state, listener, daemon_lock).await
}

/// Clients get this long to reconnect after an in-place upgrade before the
/// zero-connection idle rule (§3.2) applies again.
#[cfg_attr(windows, allow(dead_code))]
const HANDOVER_GRACE: Duration = Duration::from_secs(30);

/// The new image after an in-place upgrade (`daemon --handover <file>`).
#[cfg(unix)]
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
    crate::intercom::carry_dedup(snap.intercom_dedup.clone());
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
                ended_clean: false,
            },
        );
    }
    for (sid, key) in snap.start_keys.iter().cloned() {
        st.start_keys.push_back((sid, key));
    }
    if snap.agent_permit_details.is_empty() {
        // Compatibility with pre-work-kind handover snapshots: retain these
        // global permits, but they have no kind/cancellation owner to restore.
        st.agent_permits
            .extend(snap.agent_permits.iter().cloned().map(|key| {
                (
                    key,
                    AgentPermit {
                        work_kind: String::new(),
                        request_id: String::new(),
                        release_on_settle: false,
                        turn_started: false,
                    },
                )
            }));
    } else {
        st.agent_permits
            .extend(snap.agent_permit_details.into_iter().map(
                |(session_id, child_id, work_kind, request_id)| {
                    (
                        (session_id, child_id),
                        AgentPermit {
                            work_kind,
                            request_id,
                            release_on_settle: false,
                            turn_started: false,
                        },
                    )
                },
            ));
    }
    for (session_id, child_id, release_on_settle, turn_started) in snap.agent_permit_flags {
        if let Some(permit) = st.agent_permits.get_mut(&(session_id, child_id)) {
            permit.release_on_settle = release_on_settle;
            permit.turn_started = turn_started;
        }
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
        let (home, admission_lock) = {
            let st = s2.lock().unwrap();
            (st.home.clone(), st.agent_admission.clone())
        };
        // Reconciliation can grant queued children after reclaiming leases.
        // Read config outside `state`, and serialize its snapshot with direct
        // admissions before committing those grants.
        let _admission = admission_lock.lock().unwrap();
        let capacity = crate::capacity::load(&home).ok();
        let wake = {
            let mut st = s2.lock().unwrap();
            st.hold_idle = false;
            reap_disconnected_agent_permits(&mut st);
            capacity
                .as_ref()
                .map(|capacity| wake_pending_with_capacity(&mut st, capacity))
                .unwrap_or_default()
        };
        drop(_admission);
        send_agent_grants(&s2, wake);
        maybe_arm_idle_timer(&s2);
    });
    start_agent_loops(&state);
    serve(state, listener, lock).await
}

/// Handover keeps permits while sessions have a chance to reconnect. Once
/// that grace ends, sessions still disconnected cannot own live children.
#[cfg_attr(windows, allow(dead_code))]
fn reap_disconnected_agent_permits(st: &mut DaemonState) {
    let connected: HashSet<String> = st
        .sessions
        .iter()
        .filter_map(|(sid, session)| session.conn_id.map(|_| sid.clone()))
        .collect();
    // A permit carries its work kind, so removing its owner also removes that
    // kind's usage. Do not reap a lease whose session reconnected during grace.
    st.agent_permits
        .retain(|(sid, _), _| connected.contains(sid));
    st.pending_agents
        .retain(|pending| connected.contains(&pending.session_id));
    st.cancelled_acquires
        .retain(|(sid, _, _)| connected.contains(sid));
    let active_permits: HashSet<(String, String)> = st.agent_permits.keys().cloned().collect();
    st.undelivered_agent_grants.retain(|(sid, _), grant| {
        connected.contains(sid) && active_permits.contains(&(sid.clone(), grant.child_id.clone()))
    });
}

/// Ctrl-C, or an injected stand-in. Pin the returned future once, outside
/// the accept loop. A registration error completes the inner future with
/// `Err`; polling that future again panics, so the `Err` is turned into a
/// pending wait here and that wait must survive later iterations.
#[cfg(any(windows, test))]
fn ctrl_c_or_pending(
    home: &std::path::Path,
    ctrl_c: impl std::future::Future<Output = std::io::Result<()>>,
) -> impl std::future::Future<Output = ()> {
    let home = home.to_path_buf();
    async move {
        match ctrl_c.await {
            Ok(()) => {}
            Err(e) => {
                lifecycle::log_line(&home, &format!("Ctrl-C listener error: {e}"));
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Starts the messaging bridge every manager runs; a failure leaves the
/// manager (and its tasks) running without messaging.
async fn start_bridge(
    home: &Path,
    clock: crate::clock::Clock,
) -> (Option<crate::intercom::Running>, Option<String>) {
    let started = match crate::intercom::bridge_config(home) {
        Ok(config) => crate::intercom::start(config, clock).await,
        Err(error) => Err(error),
    };
    match started {
        Ok(running) => (Some(running), None),
        Err(error) => {
            lifecycle::log_line(
                home,
                &format!("intercom: {error}; continuing without messaging"),
            );
            (None, Some(error))
        }
    }
}

/// The accept loop, shared by a fresh daemon and one restored after an
/// in-place upgrade. Also runs the upgrade itself when asked.
async fn serve(
    state: Shared,
    listener: crate::ipc::Listener,
    daemon_lock: lifecycle::DaemonLockGuard,
) -> i32 {
    let home = state.lock().unwrap().home.clone();

    let intercom_clock = state.lock().unwrap().clock.clone();
    // Restarted after a failed in-place upgrade, which only Unix performs.
    #[cfg_attr(windows, allow(unused_mut))]
    let (mut intercom, mut bridge_error) = start_bridge(&home, intercom_clock.clone()).await;
    publish_bridge(&state, &intercom, &bridge_error);

    // §3.2: forget gone sessions past their retention, now and periodically.
    spawn_session_gc(&state);
    let live_cpu_sampler = spawn_live_cpu_sampler(&state);
    // In-place upgrade when the binary on disk changes (Unix only).
    #[cfg(unix)]
    spawn_exe_watch(&state);
    #[cfg(feature = "test-clock")]
    spawn_test_owner_watch(&home);

    // §3.2: the idle rule applies from boot (clients connect within 2s of
    // spawn per §3.1, so this never fires for a healthy startup).
    maybe_arm_idle_timer(&state);

    let (shutdown_notify, upgrade_notify, mut supervision_rx) = {
        let mut st = state.lock().unwrap();
        (
            st.shutdown_notify.clone(),
            st.upgrade_notify.clone(),
            st.supervision_rx.take().expect("supervision receiver"),
        )
    };
    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    #[cfg(windows)]
    let ctrl_c = ctrl_c_or_pending(&home, tokio::signal::ctrl_c());
    #[cfg(windows)]
    tokio::pin!(ctrl_c);

    // Keep accepting while shutting down: a new client then gets a prompt
    // "manager is shutting down" instead of hanging until its hello timeout
    // (and, under `test-clock`, can still step the manual clock).
    let mut shutdown_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok(stream) => {
                    let s = state.clone();
                    tokio::spawn(async move { handle_conn(s, stream).await });
                }
                Err(e) => {
                    lifecycle::log_line(&home, &format!("accept error: {e}"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some(request) = supervision_rx.recv(), if shutdown_task.is_none() => {
                let status = apply_supervision(&state, &mut intercom, &mut bridge_error).await;
                bridge_changed(&state, &intercom, &bridge_error).await;
                if let Some(reply) = request.reply {
                    let _ = reply.send(status);
                }
            },
            _ = upgrade_notify.notified(), if shutdown_task.is_none() => {
                #[cfg(unix)]
                {
                    let ready = state.lock().unwrap().upgrade_ready.take();
                    if let Some(ready) = ready {
                        // Returns only if the upgrade did not happen.
                        if let Some(runtime) = intercom.take() {
                            crate::intercom::carry_dedup(runtime.shutdown_for_handover().await);
                        }
                        let _ = crate::handover::perform(&state, listener.as_raw_fd(), daemon_lock.raw_fd(), ready).await;
                        if intercom.is_none() {
                            (intercom, bridge_error) = start_bridge(&home, intercom_clock.clone()).await;
                            apply_supervision(&state, &mut intercom, &mut bridge_error).await;
                            bridge_changed(&state, &intercom, &bridge_error).await;
                        }
                    }
                }
                #[cfg(windows)]
                {
                    let _ = &upgrade_notify;
                    // Take the ready value out of the lock before fail_unsupported
                    // re-locks the same non-reentrant Mutex (edition 2021 temporary scope
                    // would otherwise hold the guard across that call).
                    let ready = state.lock().unwrap().upgrade_ready.take();
                    if let Some(ready) = ready {
                        let _ = crate::handover::fail_unsupported(&state, ready);
                    }
                }
            }
            _ = shutdown_notify.notified(), if shutdown_task.is_none() => {
                shutdown_task = Some(begin_shutdown(&state, intercom.as_ref()));
            }
            _ = async {
                #[cfg(unix)]
                {
                    match sigterm.as_mut() {
                        Some(s) => { s.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                }
                #[cfg(windows)]
                {
                    // The adapter is pinned outside the loop. A fresh wrapper
                    // here would poll the already-completed signal future
                    // after a registration error and panic the daemon.
                    (&mut ctrl_c).await;
                }
            }, if shutdown_task.is_none() => {
                lifecycle::log_line(&home, if cfg!(windows) { "received Ctrl-C" } else { "received SIGTERM" });
                shutdown_task = Some(begin_shutdown(&state, intercom.as_ref()));
            }
            _ = async {
                #[cfg(unix)]
                {
                    match sigint.as_mut() {
                        Some(s) => { s.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                }
                #[cfg(windows)]
                {
                    std::future::pending::<()>().await
                }
            }, if shutdown_task.is_none() => {
                lifecycle::log_line(&home, "received SIGINT");
                shutdown_task = Some(begin_shutdown(&state, intercom.as_ref()));
            }
            _ = async {
                match shutdown_task.as_mut() {
                    Some(t) => { let _ = t.await; }
                    None => std::future::pending::<()>().await,
                }
            } => break,
        }
    }
    if let Some(intercom) = intercom {
        intercom.shutdown().await;
    }
    live_cpu_sampler.abort();
    drop(daemon_lock);
    0
}

fn spawn_live_cpu_sampler(state: &Shared) -> tokio::task::JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let mut tracker = task::LiveCpuTracker::default();
        loop {
            tokio::time::sleep(LIVE_CPU_POLL).await;
            let running: Vec<(String, u32)> = {
                let st = state.lock().unwrap();
                st.registry
                    .tasks
                    .values()
                    .filter(|entry| entry.record.status == TaskStatus::Running)
                    .map(|entry| (entry.record.task_id.clone(), entry.record.pid))
                    .collect()
            };
            let active: HashSet<String> = running.iter().map(|(id, _)| id.clone()).collect();
            tracker.retain(&active);
            let at = std::time::Instant::now();
            let sampled_at = now_ms();
            let pgids: Vec<u32> = running.iter().map(|(_, pgid)| *pgid).collect();
            let samples = task::live_group_cpu_ms_many(&pgids);
            let readings: Vec<(String, Option<task::LiveCpuReading>)> = running
                .into_iter()
                .map(|(id, pgid)| {
                    let reading = match samples.get(&pgid).copied().flatten() {
                        Some((user, system)) => Some(tracker.update(&id, at, user, system)),
                        None => {
                            tracker.mark_unavailable(&id);
                            None
                        }
                    };
                    (id, reading)
                })
                .collect();
            if readings.is_empty() {
                continue;
            }
            let mut st = state.lock().unwrap();
            for (id, reading) in readings {
                let Some(entry) = st.registry.tasks.get_mut(&id) else {
                    continue;
                };
                if entry.record.status != TaskStatus::Running {
                    continue;
                }
                apply_live_cpu_sample(&mut entry.record, reading, sampled_at);
            }
        }
    })
}

fn apply_live_cpu_sample(
    record: &mut TaskRecord,
    reading: Option<task::LiveCpuReading>,
    sampled_at: u64,
) {
    match reading {
        Some(reading) => {
            record.live_cpu_user_ms =
                Some(record.live_cpu_user_ms.unwrap_or(0).max(reading.user_ms));
            record.live_cpu_sys_ms =
                Some(record.live_cpu_sys_ms.unwrap_or(0).max(reading.system_ms));
            record.live_cpu_percent = reading.percent;
            record.live_cpu_sampled_at = Some(sampled_at);
            record.live_cpu_stale = false;
        }
        None => {
            record.live_cpu_percent = None;
            record.live_cpu_stale = true;
        }
    }
}

/// Poll the resolved install path for this daemon's executable. A path change
/// (for example, from an npm-retired package to its stable sibling) or a file
/// replacement triggers an in-place upgrade when the candidate identity stays
/// unchanged for two polls. A failed handover is not retried until the identity
/// changes again.
#[cfg(unix)]
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
/// Intercom drains first: idle watchers learn `unavailable` while the native
/// bridges and the tasks that may be watching are still alive.
fn begin_shutdown(
    state: &Shared,
    intercom: Option<&crate::intercom::Running>,
) -> tokio::task::JoinHandle<()> {
    state.lock().unwrap().shutdown = true;
    let s = state.clone();
    let intercom = intercom.map(|running| running.runtime.clone());
    tokio::spawn(async move {
        if let Some(runtime) = intercom {
            runtime.drain().await;
        }
        graceful_shutdown(&s).await
    })
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

pub(crate) fn valid_session_id(s: &str) -> bool {
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
        // mute for every later response. The frame's ack (the intercom
        // relay) gets an explicit Err so the delivery is settled instead of
        // hanging, and the writer stays alive. `respond` already
        // substitutes an error for oversized responses, so this is a last
        // line of defence.
        if payload.len() > MAX_FRAME_SIZE as usize {
            if let Some(ack) = frame.ack {
                let _ = ack.send(Err("frame exceeds the 4 MiB limit".to_string()));
            }
            continue;
        }
        let write_result = write_frame(&mut w, payload).await;
        if let Err(e) = &write_result {
            // The connection goes mute from here; its reader still runs.
            eprintln!("pi-famulus: connection writer stopped: {e}");
        }
        // The output cursor advances only on a successful write: a failed
        // frame was not received, and the resend after a reconnect must
        // replay it.
        if write_result.is_ok() {
            if let Some((task, cursor)) = frame.output {
                let mut m = written.lock().unwrap();
                let e = m.entry(task).or_insert(0);
                *e = (*e).max(cursor);
            }
        }
        let failed = write_result.is_err();
        if let Some(ack) = frame.ack {
            let _ = ack.send(write_result.map_err(|e| e.to_string()));
        }
        if failed {
            break;
        }
    }
}

async fn handle_conn(state: Shared, stream: crate::ipc::Incoming) {
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
            role,
            session_file,
            child_id,
            token,
            pi_version,
        } => {
            if let Some(asked) = role {
                let allowed = matches!(
                    (asked, client_kind),
                    (HelloRole::Cli, ClientKind::Cli)
                        | (HelloRole::Child | HelloRole::Parent, ClientKind::Extension)
                );
                if !allowed {
                    let _ = tx
                        .send(encode_error(
                            &hello.id,
                            E_BAD_REQUEST,
                            "hello role does not match the client kind",
                        ))
                        .await;
                    return;
                }
            }
            let role = match role {
                Some(HelloRole::Child) => ConnRole::Child,
                Some(HelloRole::Cli) => ConnRole::Cli,
                _ if client_kind == ClientKind::Cli => ConnRole::Cli,
                _ => ConnRole::Parent,
            };
            (
                client_kind,
                session_id,
                pi_pid,
                HelloInfo {
                    cwd,
                    extension_version,
                    protocol,
                    role,
                    child_id,
                    token,
                    session_file,
                    pi_version,
                },
            )
        }
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
    if client_kind == ClientKind::Extension && info.role != ConnRole::Child {
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
    let mut session_id = session_id;
    if info.role == ConnRole::Child {
        let child_id = info.child_id.clone().unwrap_or_default();
        let token = info.token.clone().unwrap_or_default();
        let auth = {
            let st = state.lock().unwrap();
            st.hosted.auth_child(&child_id, &token)
        };
        match auth {
            Ok(parent) => session_id = Some(parent),
            Err(e) => {
                let _ = tx.send(encode_error(&hello.id, &e.code, &e.message)).await;
                return;
            }
        }
    }

    let notice_role = info.role;
    let notice_file = info.session_file.clone();
    let notice_session = session_id.clone();
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
    let (started_at, hosted_notice) = {
        let st = state.lock().unwrap();
        let notice = if notice_role != ConnRole::Child {
            st.hosted
                .lookup(notice_session.as_deref(), notice_file.as_deref())
        } else {
            None
        };
        if notice_role == ConnRole::Parent {
            if let Some(file) = notice_file.as_deref() {
                st.hosted
                    .claim_owner(notice_session.as_deref().unwrap_or(""), Some(file));
            }
        }
        (st.started_at_ms, notice)
    };
    let _ = tx
        .send(encode_ok(
            &hello.id,
            &HelloOk {
                version: crate::VERSION.to_string(),
                pid: std::process::id(),
                started_at,
                protocol: PROTOCOL,
                hosted: hosted_notice,
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
        // Intercom ops (protocol 7) are queued to the connection's own
        // worker instead of being dispatched as independent tasks: a
        // connection's ops must apply in wire order, like a client socket.
        if let RequestKind::Intercom { op } = req.kind {
            let id = req.id;
            match enqueue_intercom_op(&state, conn_id) {
                Ok(queue) => {
                    if queue.send(IntercomOp { id: id.clone(), req: op }).await.is_err() {
                        let _ = tx
                            .send(encode_error(&id, E_INTERNAL, "intercom port closed"))
                            .await;
                    }
                }
                Err(error) => {
                    let _ = tx.send(encode_error(&id, &error.code, &error.message)).await;
                }
            }
            continue;
        }
        let s2 = state.clone();
        let tx2 = tx.clone();
        let guard = CountGuard::new(&inflight);
        let mut park = state.lock().unwrap().park_tx.subscribe();
        if matches!(&req.kind, RequestKind::AcquireAgent { .. }) {
            // Preserve wire arrival order for admission requests on one
            // connection. Other RPCs can remain pipelined in independent tasks.
            tokio::select! {
                biased;
                _ = task::parked(&mut park) => {}
                _ = dispatch(s2, conn_id, req, tx2) => {}
            }
            drop(guard);
        } else {
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
    }

    // ---- disconnect (§3.2: socket close marks the session disconnected) ----
    {
        let mut st = state.lock().unwrap();
        remove_conn(&mut st, conn_id, "closed");
    }
    close_intercom_port(&state, conn_id).await;
    maybe_arm_idle_timer(&state);
}

/// Optional hello fields stored per session.
pub struct HelloInfo {
    pub cwd: Option<String>,
    pub extension_version: Option<String>,
    pub protocol: Option<u32>,
    pub role: ConnRole,
    pub child_id: Option<String>,
    pub token: Option<String>,
    pub session_file: Option<String>,
    pub pi_version: Option<String>,
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
    if kind == ClientKind::Extension && info.role != ConnRole::Child {
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
                ended_clean: false,
            },
        );
    }
    if info.role == ConnRole::Child {
        if let Some(child_id) = info.child_id.clone() {
            st.hosted.set_child_conn(&child_id, Some(conn_id));
            if let Some(version) = info.pi_version.as_deref() {
                st.hosted.set_pi_version(&child_id, version);
            }
        }
    }
    st.conns.insert(
        conn_id,
        ConnHandle {
            kind,
            role: info.role,
            child_id: info.child_id.clone(),
            session_id,
            tx: tx.clone(),
            die: die.clone(),
            written: written.clone(),
            writer: writer.clone(),
            supervisor: false,
        },
    );
    Ok(conn_id)
}

fn remove_conn(st: &mut DaemonState, conn_id: u64, why: &str) {
    if let Some(h) = st.conns.remove(&conn_id) {
        if h.supervisor {
            let _ = st.supervision.send(SupervisionRequest { reply: None });
        }
        st.hosted.drop_conn(conn_id);
        if h.role != ConnRole::Child {
            if let Some(sid) = h.session_id.clone() {
                let owns = st.sessions.get(&sid).and_then(|s| s.conn_id) == Some(conn_id);
                if owns && why == "closed" {
                    st.hosted.on_parent_closed(&sid);
                }
            }
        }
        if let Some(sid) = &h.session_id {
            let home = st.home.clone();
            if let Some(s) = st.sessions.get_mut(sid) {
                if s.conn_id == Some(conn_id) {
                    s.conn_id = None; // session now disconnected (§3.2)
                    s.last_seen = now_ms();
                    st.pending_agents.retain(|p| p.session_id != *sid);
                    st.cancelled_acquires.retain(|(owner, _, _)| owner != sid);
                    if why == "closed" {
                        st.agent_permits.retain(|(owner, _), _| owner != sid);
                        st.undelivered_agent_grants
                            .retain(|(owner, _), _| owner != sid);
                    }
                    let reason = disconnect_reason(why, s);
                    s.ended_clean = false;
                    crate::events::emit(
                        &home,
                        Some(sid),
                        "session.disconnect",
                        None,
                        serde_json::json!({ "reason": reason }),
                    );
                }
            }
        }
        for t in st.registry.tasks.values_mut() {
            t.watchers.remove(&conn_id);
        }
        let wake = wake_pending(st);
        send_agent_grants_locked(st, wake);
    }
}

/// A session's subscription to carry over an upgrade, and how far its
/// connection's writer got (known only once the writers have flushed).
#[cfg_attr(windows, allow(dead_code))]
pub struct Carried {
    task_id: String,
    session_id: String,
    written: Written,
}

/// Close every client connection (an in-place upgrade). Returns the output
/// subscriptions to carry over and the connections' writers; call
/// [`carry_watches`] once the writers flushed or were aborted.
#[cfg_attr(windows, allow(dead_code))]
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
#[cfg_attr(windows, allow(dead_code))]
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
        let path = e.record.output_path.clone();
        queue_output_replay(tx, tid, &path, from, to, e.monitor.as_ref());
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

fn release_settled_sibling_permit(
    st: &mut DaemonState,
    notice: &crate::hosted::Notice,
) -> Vec<AgentGrant> {
    let activity = match &notice.event {
        EventKind::AgentState { activity, .. } => activity.as_str(),
        _ => return Vec::new(),
    };
    let key = (notice.parent_session.clone(), notice.child_id.clone());
    if activity == "running" {
        if let Some(permit) = st.agent_permits.get_mut(&key) {
            if permit.release_on_settle {
                permit.turn_started = true;
            }
        }
        return Vec::new();
    }
    if activity != "idle" && activity != "failed" {
        return Vec::new();
    }
    // A parked respawn emits idle before the sibling prompt starts a turn.
    let release = st
        .agent_permits
        .get(&key)
        .is_some_and(|permit| permit.release_on_settle && permit.turn_started);
    if !release {
        return Vec::new();
    }
    st.agent_permits.remove(&key);
    st.undelivered_agent_grants.retain(|(owner, _), grant| {
        owner != &notice.parent_session || grant.child_id != notice.child_id
    });
    wake_pending(st)
}

fn start_agent_loops(state: &Shared) {
    let rx = state.lock().unwrap().agent_notices.take();
    if let Some(mut rx) = rx {
        let state = state.clone();
        tokio::spawn(async move {
            while let Some(notice) = rx.recv().await {
                let wake = {
                    let mut st = state.lock().unwrap();
                    let wake = release_settled_sibling_permit(&mut st, &notice);
                    let mut targets = Vec::new();
                    if let Some(session) = st.sessions.get(&notice.parent_session) {
                        if let Some(conn_id) = session.conn_id {
                            targets.push(conn_id);
                        }
                    }
                    if let Some(conn_id) = st.hosted.child_of_conn(&notice.child_id) {
                        targets.push(conn_id);
                    }
                    targets.extend(st.hosted.watcher_ids(&notice.child_id));
                    targets.sort_unstable();
                    targets.dedup();
                    for conn_id in targets {
                        if let Some(conn) = st.conns.get(&conn_id) {
                            let _ = conn.tx.try_send(encode_event(&notice.event));
                        }
                    }
                    wake
                };
                send_agent_grants(&state, wake);
                if matches!(
                    &notice.event,
                    EventKind::AgentState { activity, .. } if activity == "failed"
                ) {
                    stop_child_bash_of(&state, &notice.child_id);
                }
            }
        });
    }
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            let clock = state.lock().unwrap().clock.clone();
            clock
                .sleep("agent-retention", Duration::from_millis(250))
                .await;
            if state.lock().unwrap().shutdown {
                break;
            }
            state.lock().unwrap().hosted.tick();
        }
    });
}

/// §3.2: arm the 5s idle timer when the last parent/cli connection went away
/// and no hosted subagent is still warm.
pub fn maybe_arm_idle_timer(state: &Shared) {
    let mut st = state.lock().unwrap();
    let session_conn = st.conns.values().any(|c| c.role != ConnRole::Child);
    let warm = st.hosted.warm_count() > 0;
    if st.shutdown
        || st.upgrading
        || st.hold_idle
        || session_conn
        || warm
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
            let session_conn = st.conns.values().any(|c| c.role != ConnRole::Child);
            let warm = st.hosted.warm_count() > 0;
            if !session_conn && !warm && !st.shutdown {
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
    let re_served_grants = service_unconfirmed_agent_grants(&state, conn_id, &tx);
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
            label,
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
                label,
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
            // Catch-up must be queued under the state lock (with the ok),
            // or a live fanout chunk can overtake the replay and leave a
            // gap at the start of the stream.
            if let Err(error) = handle_watch_on(&state, conn_id, &task_id, &id) {
                respond::<UnitOk>(&tx, &id, Err(error)).await;
            }
        }
        RequestKind::Unwatch { task_id } => {
            respond(&tx, &id, handle_unwatch(&state, conn_id, &task_id)).await
        }
        RequestKind::ShutdownSession => {
            respond(&tx, &id, handle_shutdown_session(&state, conn_id)).await
        }
        RequestKind::Status => respond(&tx, &id, handle_status(&state, conn_id)).await,
        RequestKind::AcquireAgent {
            child_id,
            work_kind,
        } => {
            handle_acquire_agent(
                &state,
                conn_id,
                &child_id,
                &work_kind,
                &id,
                &re_served_grants,
                &tx,
            )
            .await;
        }
        RequestKind::CancelAcquireAgent {
            request_id,
            child_id,
        } => match handle_cancel_acquire(&state, conn_id, &request_id, &child_id) {
            Ok(wake) => {
                respond::<UnitOk>(&tx, &id, Ok(UnitOk {})).await;
                send_agent_grants(&state, wake);
            }
            Err(error) => respond::<UnitOk>(&tx, &id, Err(error)).await,
        },
        RequestKind::CapacityChanged => {
            let result = handle_capacity_changed(&state, conn_id);
            match result {
                Ok(wake) => {
                    respond::<UnitOk>(&tx, &id, Ok(UnitOk {})).await;
                    send_agent_grants(&state, wake);
                }
                Err(error) => respond::<UnitOk>(&tx, &id, Err(error)).await,
            }
        }
        RequestKind::ReleaseAgent { child_id } => {
            respond(&tx, &id, handle_release_agent(&state, conn_id, &child_id)).await;
        }
        RequestKind::AgentSpawn {
            child_id,
            run_id,
            name,
            argv,
            cwd,
            env,
        } => match parent_session(&state, conn_id) {
            Ok(parent) => {
                let hosted = {
                    let st = state.lock().unwrap();
                    st.hosted.clone()
                };
                respond(
                    &tx,
                    &id,
                    hosted
                        .spawn(&parent, &child_id, &run_id, &name, argv, cwd, env)
                        .await,
                )
                .await
            }
            Err(e) => respond(&tx, &id, Err::<serde_json::Value, _>(e)).await,
        },
        RequestKind::AgentInput {
            child_id,
            op,
            text,
            source,
            since,
        } => match authorize_agent(&state, conn_id, &child_id, true) {
            Err(e) => respond(&tx, &id, Err::<serde_json::Value, _>(e)).await,
            Ok(()) => {
                let hosted = {
                    let st = state.lock().unwrap();
                    st.hosted.clone()
                };
                let source = if source.is_empty() {
                    "parent".into()
                } else {
                    source
                };
                respond(
                    &tx,
                    &id,
                    hosted
                        .rpc(&child_id, &op, &text, &source, since.as_deref())
                        .await,
                )
                .await
            }
        },
        RequestKind::AgentWatch { child_id } => {
            let result = authorize_agent(&state, conn_id, &child_id, true).and_then(|()| {
                let st = state.lock().unwrap();
                st.hosted.watch(&child_id, conn_id).map(|()| UnitOk {})
            });
            respond(&tx, &id, result).await
        }
        RequestKind::AgentUnwatch { child_id } => {
            {
                let st = state.lock().unwrap();
                st.hosted.unwatch(&child_id, conn_id);
            }
            respond(&tx, &id, Ok::<_, ProtoError>(UnitOk {})).await
        }
        RequestKind::AgentComms {
            request_id,
            reason,
            message,
        } => {
            let (child_id, hosted) = {
                let st = state.lock().unwrap();
                (
                    st.conns
                        .get(&conn_id)
                        .and_then(|c| c.child_id.clone())
                        .unwrap_or_default(),
                    st.hosted.clone(),
                )
            };
            match hosted
                .comms(&child_id, &request_id, &reason, &message)
                .await
            {
                Ok(reply) => {
                    respond(
                        &tx,
                        &id,
                        Ok::<_, ProtoError>(serde_json::json!({ "reply": reply })),
                    )
                    .await
                }
                Err(e) => respond(&tx, &id, Err::<serde_json::Value, _>(e)).await,
            }
        }
        RequestKind::AgentCommsReply {
            request_id,
            child_id,
            message,
        } => {
            let result = authorize_agent(&state, conn_id, &child_id, false).and_then(|()| {
                let st = state.lock().unwrap();
                st.hosted
                    .comms_reply(&request_id, &child_id, &message)
                    .map(|()| UnitOk {})
            });
            respond(&tx, &id, result).await
        }
        RequestKind::AgentUiResponse {
            child_id,
            rpc_id,
            value,
            cancelled,
        } => {
            let result = authorize_agent(&state, conn_id, &child_id, false).and_then(|()| {
                let st = state.lock().unwrap();
                st.hosted
                    .ui_response(&child_id, &rpc_id, &value, cancelled)
                    .map(|()| UnitOk {})
            });
            respond(&tx, &id, result).await
        }
        RequestKind::HostedLookup {
            session_id,
            session_file,
        } => {
            let notice = {
                let st = state.lock().unwrap();
                st.hosted
                    .lookup(session_id.as_deref(), session_file.as_deref())
            };
            respond(
                &tx,
                &id,
                Ok::<_, ProtoError>(serde_json::json!({ "hosted": notice })),
            )
            .await
        }
        RequestKind::AgentCache {
            expires_at,
            tier,
            lifetime_ms,
        } => {
            {
                let st = state.lock().unwrap();
                let child_id = st
                    .conns
                    .get(&conn_id)
                    .and_then(|c| c.child_id.clone())
                    .unwrap_or_default();
                st.hosted.cache(&child_id, expires_at, tier, lifetime_ms);
            }
            respond(&tx, &id, Ok::<_, ProtoError>(UnitOk {})).await
        }
        RequestKind::AgentPark { child_id } => {
            let result = authorize_agent(&state, conn_id, &child_id, false).and_then(|()| {
                let st = state.lock().unwrap();
                st.hosted.park(&child_id, "interrupt").map(|()| UnitOk {})
            });
            respond(&tx, &id, result).await
        }
        RequestKind::AgentTakeover { child_id } => {
            match authorize_agent(&state, conn_id, &child_id, false) {
                Err(e) => respond(&tx, &id, Err::<UnitOk, _>(e)).await,
                Ok(()) => {
                    let hosted = {
                        let st = state.lock().unwrap();
                        st.hosted.clone()
                    };
                    respond(
                        &tx,
                        &id,
                        hosted.takeover(&child_id).await.map(|()| UnitOk {}),
                    )
                    .await
                }
            }
        }
        RequestKind::AgentPeers => {
            let (child_id, hosted, is_child) = {
                let st = state.lock().unwrap();
                let conn = st.conns.get(&conn_id);
                (
                    conn.and_then(|c| c.child_id.clone()).unwrap_or_default(),
                    st.hosted.clone(),
                    conn.is_some_and(|c| c.role == ConnRole::Child),
                )
            };
            if !is_child {
                respond(
                    &tx,
                    &id,
                    Err::<serde_json::Value, _>(ProtoError::new(
                        E_FORBIDDEN,
                        "agent_peers is for a hosted child",
                    )),
                )
                .await;
            } else {
                respond(
                    &tx,
                    &id,
                    Ok::<_, ProtoError>(serde_json::json!({ "peers": hosted.peers(&child_id) })),
                )
                .await;
            }
        }
        RequestKind::Supervise {} => {
            respond(&tx, &id, handle_supervise(&state, conn_id).await).await;
        }
        RequestKind::Intercom { .. } => {
            // Intercepted in the connection's read loop so a connection's
            // ops serialize in wire order; reaching dispatch means the
            // enqueue failed before the loop could answer.
            respond::<UnitOk>(
                &tx,
                &id,
                Err(ProtoError::new(E_INTERNAL, "intercom op was not queued")),
            )
            .await;
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

fn authorize_agent(
    state: &Shared,
    conn_id: u64,
    child_id: &str,
    sibling_input: bool,
) -> Result<(), ProtoError> {
    let st = state.lock().unwrap();
    let conn = st
        .conns
        .get(&conn_id)
        .ok_or_else(|| ProtoError::new(E_INTERNAL, "connection gone"))?;
    let role = match conn.role {
        ConnRole::Cli => "cli",
        ConnRole::Child => "child",
        ConnRole::Parent => "parent",
    };
    let session = conn.session_id.clone();
    let caller = conn.child_id.clone();
    st.hosted.authorize(
        role,
        session.as_deref(),
        caller.as_deref(),
        child_id,
        sibling_input,
    )
}

fn parent_session(state: &Shared, conn_id: u64) -> Result<String, ProtoError> {
    let st = state.lock().unwrap();
    let conn = st
        .conns
        .get(&conn_id)
        .ok_or_else(|| ProtoError::new(E_INTERNAL, "connection gone"))?;
    if conn.role == ConnRole::Child {
        return Err(ProtoError::new(
            E_FORBIDDEN,
            "a hosted child cannot spawn subagents",
        ));
    }
    conn.session_id.clone().ok_or_else(|| {
        ProtoError::new(
            E_SESSION_REQUIRED,
            "agent_spawn requires an extension session",
        )
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
    #[cfg(debug_assertions)]
    mark_test_writer_queue_full(tx, id);
    let _ = tx.send(frame).await;
}

#[cfg(debug_assertions)]
fn mark_test_writer_queue_full(tx: &OutTx, id: &str) {
    if !id.starts_with("p16-status-") || tx.capacity() != 0 {
        return;
    }
    if let Some(marker) = std::env::var_os("PI_FAMULUS_TEST_WRITER_QUEUE_FULL_MARKER") {
        let _ = std::fs::write(marker, b"full");
    }
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
    pub label: Option<String>,
}

/// A client label is display text: one line, no control or bidi formatting
/// characters, and no longer than a clipped command, so it cannot break,
/// reorder, or spoof a row, or bloat records.
fn clean_label(raw: &str) -> Option<String> {
    let bidi = |c: char| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    let line: String = raw
        .chars()
        .map(|c| if c.is_control() || bidi(c) { ' ' } else { c })
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then(|| crate::events::clip_chars(&line, crate::events::COMMAND_CHARS))
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
        label,
    } = spec;
    let label = label.as_deref().and_then(clean_label);
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
        live_cpu_user_ms: None,
        live_cpu_sys_ms: None,
        live_cpu_percent: None,
        live_cpu_sampled_at: None,
        live_cpu_stale: false,
        label: label.clone(),
    };
    if let Err(e) = registry::persist_record(&home, &record) {
        let _ = task::signal_group(pid, task::SIGKILL); // don't leak the child
        #[cfg(windows)]
        crate::sys::drop_job(pid, parts.job_generation);
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
                "origin": origin.clone(),
                "pid": pid,
                "label": label,
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
    #[cfg(windows)]
    if crate::sys::task_shell().kind == crate::sys::ShellKind::Posix {
        tokio::spawn(watch_shell_start(state.clone(), task_id.clone(), pid));
    }
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
            origin: origin.clone(),
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

/// A hosted child died. Stop the foreground shells it started so they do
/// not keep running with nobody reading their output.
fn stop_child_bash_of(state: &Shared, child_id: &str) {
    let victims: Vec<(String, u32, String)> = {
        let mut st = state.lock().unwrap();
        let mut victims = Vec::new();
        for e in st.registry.tasks.values_mut() {
            let ours = e.record.origin.as_ref().is_some_and(|origin| {
                origin.via == "child-bash" && origin.child_id.as_deref() == Some(child_id)
            });
            if !ours || !e.owns_live_group() {
                continue;
            }
            if e.record.status == TaskStatus::Running {
                e.request_kill("stopped:tool");
            }
            victims.push((
                e.record.task_id.clone(),
                e.record.pid,
                e.record.session_id.clone(),
            ));
        }
        victims
    };
    for (task_id, pid, sid) in victims {
        let home = state.lock().unwrap().home.clone();
        crate::events::emit(
            &home,
            Some(&sid),
            "task.stop",
            Some(&task_id),
            serde_json::json!({ "reason": "tool", "child_id": child_id }),
        );
        let _ = task::signal_group(pid, task::SIGTERM);
        spawn_kill_reaper(state, &task_id, pid);
    }
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
#[cfg_attr(windows, allow(dead_code))]
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
/// group empties. On POSIX the pid is not reused while that group exists.
/// On Windows `job_generation` is the job assigned for this pid; a later
/// task may reuse the pid, and a stale watcher must not drop that job.
pub fn spawn_group_watcher(state: &Shared, task_id: &str, pgid: u32, job_generation: u64) {
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
            #[cfg(windows)]
            crate::sys::drop_job(pgid, job_generation);
            #[cfg(not(windows))]
            let _ = job_generation;
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

/// Subscribe to a task's output stream, replaying anything the fanout has
/// already advanced past. A late `watch` used to start at `delivered_cursor`
/// with no catch-up, so bytes that landed with no watchers (CI: the leading
/// `a` of `aé中😀b`) were never pushed.
fn handle_watch_on(
    state: &Shared,
    conn_id: u64,
    task_id: &str,
    req_id: &str,
) -> Result<(), ProtoError> {
    let mut st = state.lock().unwrap();
    let acc = access_for(&st, conn_id);
    let e = st.registry.visible_mut(task_id, &acc)?;
    e.watchers.insert(conn_id);
    let path = e.record.output_path.clone();
    let to = e.delivered_cursor;
    let Some(h) = st.conns.get(&conn_id) else {
        return Err(ProtoError::new(E_INTERNAL, "connection gone"));
    };
    let tx = h.tx.clone();
    let from = {
        let mut written = h.written.lock().unwrap();
        // First watch starts at 0 so catch-up covers the gap; a repeat
        // watch keeps the cursor the writer already flushed (idempotent).
        *written.entry(task_id.to_string()).or_insert(0)
    };
    // ok before replay so the client sees the response, then the gap, then
    // live chunks — and all of it is ordered ahead of any fanout still
    // waiting on this same lock.
    let _ = tx.try_send(encode_ok(req_id, &UnitOk {}));
    let monitor = st
        .registry
        .tasks
        .get(task_id)
        .and_then(|e| e.monitor.as_ref());
    queue_output_replay(&tx, task_id, &path, from, to, monitor);
    Ok(())
}

fn handle_unwatch(state: &Shared, conn_id: u64, task_id: &str) -> Result<UnitOk, ProtoError> {
    let mut st = state.lock().unwrap();
    let acc = access_for(&st, conn_id);
    let e = st.registry.visible_mut(task_id, &acc)?;
    e.watchers.remove(&conn_id);
    Ok(UnitOk {})
}

/// Queue `[from, to)` from the on-disk output onto `tx` as `output` events.
/// `to` is on a character boundary (fanout never counts a held-back tail).
fn queue_output_replay(
    tx: &OutTx,
    task_id: &str,
    path: &str,
    from: u64,
    to: u64,
    monitor: Option<&crate::monitor::MonitorState>,
) {
    let mut cursor = from.max(to.saturating_sub(REPLAY_MAX));
    // The last raw byte may have been written before its progress/EOF metadata.
    // Replay records at that same cursor too; consumers track record identity
    // separately from their raw-byte cursor.
    if let Some(records) = monitor.and_then(|m| m.replay_at(cursor)) {
        if !records.is_empty() {
            let ev = encode_event_bytes(&EventKind::Output {
                task_id: task_id.to_string(),
                chunk: String::new(),
                next_cursor: cursor,
                monitor_records: Some(records),
            });
            if tx.try_send(OutFrame::output(ev, task_id, cursor)).is_err() {
                return;
            }
        }
    }
    while cursor < to {
        // One byte can be one empty-line record. Cap monitor raw slices so
        // worst-case metadata expansion remains well below the 4 MiB frame.
        let limit = if monitor.is_some() { 4096 } else { 256 * 1024 };
        let want = ((to - cursor) as usize).min(limit);
        let Ok((bytes, _)) = task::read_file_range(std::path::Path::new(path), cursor, want) else {
            break;
        };
        if bytes.is_empty() {
            break;
        }
        let n = task::utf8_chunk_len(
            &bytes,
            want,
            CHUNK_JSON_BUDGET,
            cursor + (bytes.len() as u64) < to,
        )
        .max(1);
        let chunk = &bytes[..n.min(bytes.len())];
        let start = cursor;
        cursor += chunk.len() as u64;
        let ev = encode_event_bytes(&EventKind::Output {
            task_id: task_id.to_string(),
            chunk: String::from_utf8_lossy(chunk).into_owned(),
            next_cursor: cursor,
            monitor_records: monitor.and_then(|m| m.replay(start, cursor)),
        });
        if tx.try_send(OutFrame::output(ev, task_id, cursor)).is_err() {
            break; // queue full: the client catches up with output(cursor)
        }
    }
}

fn disconnect_reason(why: &str, session: &SessionEntry) -> String {
    if why != "closed" {
        return why.to_string();
    }
    if session.ended_clean {
        return "shutdown".into();
    }
    if session.pi_pid != 0 && !crate::sys::pid_alive(session.pi_pid) {
        return "crash".into();
    }
    "closed".into()
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
        if let Some(session) = st.sessions.get_mut(&sid) {
            session.ended_clean = true;
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

/// Parent acquires use the connection's session. A hosted child acquires for a
/// same-run sibling under that sibling's parent session, so the turn shares
/// the machine permit with the parent.
fn admission_owner(
    st: &DaemonState,
    conn_id: u64,
    child_id: &str,
) -> Result<(String, u32, bool), ProtoError> {
    let conn = st
        .conns
        .get(&conn_id)
        .ok_or_else(|| ProtoError::new(E_INTERNAL, "connection gone"))?;
    let from_child = conn.role == ConnRole::Child;
    let sid = if from_child {
        let caller = conn.child_id.clone().unwrap_or_default();
        st.hosted
            .authorize("child", None, Some(&caller), child_id, true)?;
        st.hosted
            .parent_of(child_id)
            .ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?
    } else {
        conn.session_id
            .clone()
            .ok_or_else(|| ProtoError::new(E_SESSION_REQUIRED, "extension session required"))?
    };
    let protocol = st
        .sessions
        .get(&sid)
        .and_then(|s| s.protocol)
        .unwrap_or(if from_child { 6 } else { 0 });
    Ok((sid, protocol, from_child))
}

async fn handle_acquire_agent(
    state: &Shared,
    conn_id: u64,
    child_id: &str,
    work_kind: &str,
    request_id: &str,
    re_served_grants: &HashSet<String>,
    tx: &OutTx,
) {
    let session = {
        let st = state.lock().unwrap();
        admission_owner(&st, conn_id, child_id).map(|(sid, protocol, from_child)| {
            (
                sid,
                protocol,
                st.home.clone(),
                st.agent_budget.clone(),
                st.agent_admission.clone(),
                from_child,
            )
        })
    };
    let (sid, client_protocol, home, budget_cache, admission_lock, release_on_settle) =
        match session {
            Ok(parts) => parts,
            Err(error) => {
                respond::<AgentAdmissionOk>(tx, request_id, Err(error)).await;
                return;
            }
        };
    // The #46 global budget cache remains authoritative for the machine-wide
    // cap. Serialize its refresh and this state commit so an older read cannot
    // overtake a newer admission; config I/O remains outside `state`.
    let admission = admission_lock.lock().unwrap();
    let global_budget = budget_cache.lock().unwrap().get(&home);
    #[cfg(debug_assertions)]
    pause_after_agent_budget_read(child_id);
    // Per-kind settings are loaded alongside the global cap for v5 scheduling.
    let capacity = crate::capacity::load(&home);
    let (result, wake) = {
        let mut st = state.lock().unwrap();
        let still_here = st.conns.get(&conn_id).is_some_and(|conn| {
            if conn.role == ConnRole::Child {
                true
            } else {
                conn.session_id.as_deref() == Some(sid.as_str())
            }
        });
        if !still_here {
            (
                Some(Err(ProtoError::new(
                    E_SESSION_REQUIRED,
                    "extension session required",
                ))),
                Vec::new(),
            )
        } else {
            let request_key = (sid.clone(), child_id.to_string(), request_id.to_string());
            if st.cancelled_acquires.remove(&request_key) {
                (None, Vec::new())
            } else {
                let wake = match &capacity {
                    Ok(capacity) => wake_pending_with_capacity(&mut st, capacity),
                    Err(_) => Vec::new(),
                };
                let key = (sid.clone(), child_id.to_string());
                let grant_still_undelivered = st
                    .undelivered_agent_grants
                    .get(&(sid.clone(), request_id.to_string()))
                    .is_some_and(|grant| grant.child_id == child_id);
                if st.agent_permits.contains_key(&key)
                    && (grant_still_undelivered || re_served_grants.contains(request_id))
                {
                    // A queued grant is already being (re-)served under this id.
                    (None, wake)
                } else if st.agent_permits.contains_key(&key) {
                    // Acquires are idempotent per (session, child). Reconnects and
                    // same-id backpressure retries must never consume another slot.
                    (
                        Some(Ok(AgentAdmissionOk {
                            granted: true,
                            rejection: None,
                            queued_behind: None,
                        })),
                        wake,
                    )
                } else if let Some(pending) = st
                    .pending_agents
                    .iter_mut()
                    .find(|p| p.session_id == sid && p.child_id == child_id)
                {
                    if !pending.request_ids.iter().any(|id| id == request_id) {
                        if pending.request_ids.len() >= PENDING_AGENT_REQUEST_IDS_MAX {
                            (
                                Some(Ok(AgentAdmissionOk {
                                    granted: false,
                                    rejection: Some("capacity_queue_full".into()),
                                    queued_behind: None,
                                })),
                                wake,
                            )
                        } else {
                            pending.request_ids.push(request_id.to_string());
                            (None, wake)
                        }
                    } else {
                        (None, wake)
                    }
                } else {
                    let budget = global_budget
                        .as_ref()
                        .map(|budget| *budget)
                        .map_err(|error| {
                            ProtoError::new(E_INTERNAL, format!("invalid capacity config: {error}"))
                        });
                    match budget {
                        Err(error) => (Some(Err(error)), wake),
                        Ok(budget) if st.agent_permits.len() >= budget => (
                            Some(Ok(AgentAdmissionOk {
                                granted: false,
                                rejection: Some("global_capacity".into()),
                                queued_behind: None,
                            })),
                            wake,
                        ),
                        Ok(_) if client_protocol < 5 => {
                            st.agent_permits.insert(
                                key,
                                AgentPermit {
                                    work_kind: String::new(),
                                    request_id: request_id.to_string(),
                                    release_on_settle,
                                    turn_started: false,
                                },
                            );
                            (
                                Some(Ok(AgentAdmissionOk {
                                    granted: true,
                                    rejection: None,
                                    queued_behind: None,
                                })),
                                wake,
                            )
                        }
                        Ok(_) => {
                            let kind = if work_kind.is_empty() {
                                "other"
                            } else {
                                work_kind
                            };
                            let kind_budget = match &capacity {
                                Ok(capacity) => capacity.max_kind(kind).map_err(|error| {
                                    ProtoError::new(
                                        E_INTERNAL,
                                        format!("invalid capacity config: {error}"),
                                    )
                                }),
                                Err(error) => Err(ProtoError::new(
                                    E_INTERNAL,
                                    format!("invalid capacity config: {error}"),
                                )),
                            };
                            match kind_budget {
                                Err(error) => (Some(Err(error)), wake),
                                Ok(kind_budget)
                                    if admits_kind(&st.agent_permits, kind, kind_budget) =>
                                {
                                    st.agent_permits.insert(
                                        key,
                                        AgentPermit {
                                            work_kind: kind.to_string(),
                                            request_id: request_id.to_string(),
                                            release_on_settle,
                                            turn_started: false,
                                        },
                                    );
                                    (
                                        Some(Ok(AgentAdmissionOk {
                                            granted: true,
                                            rejection: None,
                                            queued_behind: None,
                                        })),
                                        wake,
                                    )
                                }
                                Ok(_) => {
                                    let session_pending = st
                                        .pending_agents
                                        .iter()
                                        .filter(|p| p.session_id == sid)
                                        .count();
                                    if session_pending >= PENDING_AGENTS_PER_SESSION_MAX
                                        || st.pending_agents.len() >= PENDING_AGENTS_GLOBAL_MAX
                                    {
                                        (
                                            Some(Ok(AgentAdmissionOk {
                                                granted: false,
                                                rejection: Some("capacity_queue_full".into()),
                                                queued_behind: None,
                                            })),
                                            wake,
                                        )
                                    } else {
                                        let queued_behind = st.pending_agents.len() as u64;
                                        let requester_child = if release_on_settle {
                                            st.conns.get(&conn_id).and_then(|c| c.child_id.clone())
                                        } else {
                                            None
                                        };
                                        st.pending_agents.push_back(PendingAgent {
                                            session_id: sid,
                                            child_id: child_id.into(),
                                            work_kind: kind.into(),
                                            request_ids: vec![request_id.into()],
                                            tx: tx.clone(),
                                            queued_behind,
                                            release_on_settle,
                                            requester_child,
                                        });
                                        (None, wake)
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    drop(admission);
    send_agent_grants(state, wake);
    if let Some(result) = result {
        respond(tx, request_id, result).await;
    }
}

#[cfg(debug_assertions)]
fn pause_after_agent_budget_read(child_id: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PAUSED: AtomicBool = AtomicBool::new(false);
    if std::env::var("PI_FAMULUS_TEST_AGENT_BUDGET_PAUSE_CHILD").as_deref() != Ok(child_id) {
        return;
    }
    if PAUSED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Ok(marker) = std::env::var("PI_FAMULUS_TEST_AGENT_BUDGET_MARKER") {
        let _ = std::fs::write(marker, b"read");
    }
    let release_marker = std::env::var_os("PI_FAMULUS_TEST_AGENT_BUDGET_RELEASE_MARKER");
    tokio::task::block_in_place(|| {
        if let Some(marker) = release_marker {
            let marker = std::path::PathBuf::from(marker);
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !marker.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            let ms = std::env::var("PI_FAMULUS_TEST_AGENT_BUDGET_PAUSE_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(500);
            std::thread::sleep(Duration::from_millis(ms));
        }
    });
}

fn handle_cancel_acquire(
    state: &Shared,
    conn_id: u64,
    request_id: &str,
    child_id: &str,
) -> Result<Vec<AgentGrant>, ProtoError> {
    let mut st = state.lock().unwrap();
    let sid = st
        .conns
        .get(&conn_id)
        .and_then(|c| c.session_id.clone())
        .ok_or_else(|| ProtoError::new(E_SESSION_REQUIRED, "extension session required"))?;
    let permit_key = (sid.clone(), child_id.to_string());
    let pending_index = st.pending_agents.iter().position(|pending| {
        pending.session_id == sid
            && pending.child_id == child_id
            && pending.request_ids.iter().any(|id| id == request_id)
    });
    if let Some(index) = pending_index {
        let pending = st.pending_agents.get_mut(index).unwrap();
        pending.request_ids.retain(|id| id != request_id);
        if pending.request_ids.is_empty() {
            st.pending_agents.remove(index);
        }
    } else if st
        .agent_permits
        .get(&permit_key)
        .is_some_and(|permit| permit.request_id == request_id)
    {
        st.agent_permits.remove(&permit_key);
        st.undelivered_agent_grants
            .retain(|(owner, _), grant| owner != &sid || grant.child_id != child_id);
    } else if !st.agent_permits.contains_key(&permit_key) {
        // One-shot tombstone closes cancel-before-acquire dispatch races. Once
        // consumed, a retry with this id is evaluated as a new acquire.
        let tombstone = (sid.clone(), child_id.to_string(), request_id.to_string());
        st.cancelled_acquires.insert(tombstone.clone());
        while st.cancelled_acquires.len() > 512 {
            let Some(oldest) = st
                .cancelled_acquires
                .iter()
                .find(|entry| *entry != &tombstone)
                .cloned()
            else {
                break;
            };
            st.cancelled_acquires.remove(&oldest);
        }
    }
    Ok(wake_pending(&mut st))
}

fn handle_capacity_changed(state: &Shared, conn_id: u64) -> Result<Vec<AgentGrant>, ProtoError> {
    let mut st = state.lock().unwrap();
    if !matches!(st.conns.get(&conn_id), Some(h) if h.kind == ClientKind::Cli) {
        return Err(ProtoError::new(
            E_FORBIDDEN,
            "capacity_changed is a cli-only operation",
        ));
    }
    Ok(wake_pending(&mut st))
}

fn clear_delivered_grant(state: &Shared, session_id: &str, request_id: &str, child_id: &str) {
    let mut st = state.lock().unwrap();
    let key = (session_id.to_string(), request_id.to_string());
    if st
        .undelivered_agent_grants
        .get(&key)
        .is_some_and(|grant| grant.child_id == child_id)
    {
        st.undelivered_agent_grants.remove(&key);
    }
}

fn send_agent_grants_locked(st: &mut DaemonState, wake: Vec<AgentGrant>) {
    for grant in wake {
        let frame = encode_ok(
            &grant.request_id,
            &AgentAdmissionOk {
                granted: true,
                rejection: None,
                queued_behind: grant.queued_behind,
            },
        );
        if grant.tx.try_send(frame).is_ok() {
            let key = (grant.session_id, grant.request_id);
            if st
                .undelivered_agent_grants
                .get(&key)
                .is_some_and(|held| held.child_id == grant.child_id)
            {
                st.undelivered_agent_grants.remove(&key);
            }
        }
        // Full/closed retains the grant state. The next request from this
        // session re-serves it through service_unconfirmed_agent_grants().
    }
}

fn send_agent_grants(state: &Shared, wake: Vec<AgentGrant>) {
    let mut delivered = Vec::new();
    for grant in wake {
        let frame = encode_ok(
            &grant.request_id,
            &AgentAdmissionOk {
                granted: true,
                rejection: None,
                queued_behind: grant.queued_behind,
            },
        );
        if grant.tx.try_send(frame).is_ok() {
            delivered.push((grant.session_id, grant.request_id, grant.child_id));
        }
    }
    if !delivered.is_empty() {
        let mut st = state.lock().unwrap();
        for (session_id, request_id, child_id) in delivered {
            let key = (session_id, request_id);
            if st
                .undelivered_agent_grants
                .get(&key)
                .is_some_and(|grant| grant.child_id == child_id)
            {
                st.undelivered_agent_grants.remove(&key);
            }
        }
    }
}

fn service_unconfirmed_agent_grants(state: &Shared, conn_id: u64, tx: &OutTx) -> HashSet<String> {
    let pending = {
        let st = state.lock().unwrap();
        let Some(conn) = st.conns.get(&conn_id) else {
            return HashSet::new();
        };
        let is_child = conn.role == ConnRole::Child;
        let session_id = conn.session_id.clone();
        let caller = conn.child_id.clone();
        st.undelivered_agent_grants
            .iter()
            .filter(|((owner, _), grant)| {
                if is_child {
                    grant.requester_child.is_some() && grant.requester_child == caller
                } else {
                    session_id.as_ref() == Some(owner) && grant.requester_child.is_none()
                }
            })
            .map(|((owner, request_id), grant)| {
                (
                    owner.clone(),
                    request_id.clone(),
                    grant.child_id.clone(),
                    grant.queued_behind,
                )
            })
            .collect::<Vec<_>>()
    };
    let mut re_served = HashSet::new();
    for (sid, request_id, child_id, queued_behind) in pending {
        if tx
            .try_send(encode_ok(
                &request_id,
                &AgentAdmissionOk {
                    granted: true,
                    rejection: None,
                    queued_behind,
                },
            ))
            .is_ok()
        {
            clear_delivered_grant(state, &sid, &request_id, &child_id);
            re_served.insert(request_id);
        }
    }
    re_served
}

fn handle_release_agent(
    state: &Shared,
    conn_id: u64,
    child_id: &str,
) -> Result<UnitOk, ProtoError> {
    let wake = {
        let mut st = state.lock().unwrap();
        let sid = admission_owner(&st, conn_id, child_id)?.0;
        st.agent_permits
            .remove(&(sid.clone(), child_id.to_string()));
        st.undelivered_agent_grants
            .retain(|(owner, _), grant| owner != &sid || grant.child_id != child_id);
        wake_pending(&mut st)
    };
    send_agent_grants(state, wake);
    Ok(UnitOk {})
}

fn handle_status(state: &Shared, _conn_id: u64) -> Result<StatusOk, ProtoError> {
    let (home, budget_cache) = {
        let st = state.lock().unwrap();
        (st.home.clone(), st.agent_budget.clone())
    };
    let total = budget_cache
        .lock()
        .unwrap()
        .get(&home)
        .map_err(|e| ProtoError::new(E_INTERNAL, format!("invalid capacity config: {e}")))?;
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
    let capacity_config = crate::capacity::load(&st.home).map_err(|error| {
        ProtoError::new(E_INTERNAL, format!("invalid capacity config: {error}"))
    })?;
    Ok(StatusOk {
        version: crate::VERSION.to_string(),
        pid: std::process::id(),
        uptime_ms: now_ms().saturating_sub(st.started_at_ms),
        sessions,
        task_counts: TaskCounts { running, terminal },
        agent_capacity: Some(AgentCapacity {
            used: st.agent_permits.len(),
            total,
            by_kind: {
                let mut kinds = HashMap::new();
                for kind in crate::capacity::WORK_KINDS {
                    let Ok(kind_total) = capacity_config.max_kind(kind) else {
                        // Per-kind values are advisory; a malformed kind should
                        // not make the entire status endpoint unavailable.
                        continue;
                    };
                    kinds.insert(
                        (*kind).to_string(),
                        AgentKindCapacity {
                            used: st
                                .agent_permits
                                .values()
                                .filter(|permit| permit.work_kind == *kind)
                                .count(),
                            total: kind_total,
                        },
                    );
                }
                for permit in st
                    .agent_permits
                    .values()
                    .filter(|permit| !permit.work_kind.is_empty())
                {
                    if !kinds.contains_key(&permit.work_kind) {
                        let entry = kinds
                            .entry(permit.work_kind.clone())
                            .or_insert(AgentKindCapacity { used: 0, total });
                        entry.used += 1;
                    }
                }
                kinds
            },
        }),
        protocol: PROTOCOL,
        generation: st.generation,
        last_upgrade: st.last_upgrade.clone(),
        exe: crate::handover::exe_path()
            .ok()
            .map(|p| p.display().to_string()),
    })
}

async fn handle_supervise(state: &Shared, conn_id: u64) -> Result<SuperviseOk, ProtoError> {
    let sender = {
        let mut st = state.lock().unwrap();
        if st.shutdown || st.upgrading {
            return Err(ProtoError::new(
                E_INTERNAL,
                "manager is shutting down or upgrading",
            ));
        }
        let Some(conn) = st
            .conns
            .get_mut(&conn_id)
            .filter(|conn| conn.role == ConnRole::Cli)
        else {
            return Err(ProtoError::new(
                E_FORBIDDEN,
                "supervise is a cli-only operation",
            ));
        };
        conn.supervisor = true;
        st.supervision.clone()
    };
    let (reply, response) = tokio::sync::oneshot::channel();
    sender
        .send(SupervisionRequest { reply: Some(reply) })
        .map_err(|_| ProtoError::new(E_INTERNAL, "manager stopped before supervising"))?;
    let intercom = response
        .await
        .map_err(|_| ProtoError::new(E_INTERNAL, "manager stopped before supervising"))?;
    Ok(SuperviseOk { intercom })
}

// ---------------------------------------------------------------------------
// Intercom ports (protocol 7): the extension's messaging bridge over the
// manager connection
// ---------------------------------------------------------------------------

const E_INTERCOM_DISABLED: &str = "E_INTERCOM_DISABLED";
const E_INTERCOM_UNAVAILABLE: &str = "E_INTERCOM_UNAVAILABLE";
/// The bridge restarted under this port; re-register and retry idempotent
/// ops. `send` is never retried by the extension.
const E_INTERCOM_RESET: &str = "E_INTERCOM_RESET";

/// Publish the current bridge (and why it is down) for `intercom` requests.
/// Brief locked access: callers only clone the runtime Arc out of it.
/// Returns true when the published bridge (or its error) changed.
fn publish_bridge(
    state: &Shared,
    intercom: &Option<crate::intercom::Running>,
    bridge_error: &Option<String>,
) -> bool {
    let (rt_slot, error_slot) = {
        let st = state.lock().unwrap();
        (st.intercom_rt.clone(), st.intercom_error.clone())
    };
    let mut rt_guard = rt_slot.lock().unwrap();
    let changed = match rt_guard.as_ref() {
        Some(rt) => intercom
            .as_ref()
            .is_none_or(|running| !Arc::ptr_eq(rt, &running.runtime)),
        None => intercom.is_some(),
    } || *error_slot.lock().unwrap() != *bridge_error;
    if changed {
        *rt_guard = intercom.as_ref().map(|running| running.runtime.clone());
        *error_slot.lock().unwrap() = bridge_error.clone();
    }
    changed
}

/// The bridge changed (restarted or stopped): existing ports belong to the
/// old runtime. Push `intercom_reset` to their connections so the extension
/// re-registers, as if its socket had closed, then close the ports. A
/// supervision lease flip without a bridge restart leaves ports alone.
async fn bridge_changed(
    state: &Shared,
    intercom: &Option<crate::intercom::Running>,
    bridge_error: &Option<String>,
) {
    if publish_bridge(state, intercom, bridge_error) {
        reset_intercom_ports(state).await;
    }
}

async fn reset_intercom_ports(state: &Shared) {
    let entries: Vec<(u64, IntercomEntry)> = {
        // Both locks, brief: same order as enqueue_intercom_op. Entries
        // STAY in the map: each connection keeps its one worker, so wire
        // order survives the reset.
        let st = state.lock().unwrap();
        let guard = st.intercom_ports.lock().unwrap();
        guard.iter().map(|(id, e)| (*id, IntercomEntry {
            tx: e.tx.clone(),
            shutdown: e.shutdown.clone(),
            reset: e.reset.clone(),
        })).collect()
    };
    if entries.is_empty() {
        return;
    }
    // Best-effort reset notice; the worker ALSO bumps the reset watch, so
    // queued ops are answered deterministically even when this event is
    // lost (full out-queue).
    {
        let st = state.lock().unwrap();
        for (conn_id, _) in &entries {
            if let Some(handle) = st.conns.get(conn_id) {
                let _ = handle
                    .tx
                    .try_send(encode_event(&EventKind::IntercomReset {}));
            }
        }
    }
    for (_, entry) in entries {
        entry.reset.send_modify(|generation| *generation += 1);
    }
}


/// Close one connection's queue (socket close, session rebind). Brief:
/// signals the worker, which cancels any in-flight op and closes the port;
/// teardown never waits on the op itself.
async fn close_intercom_port(state: &Shared, conn_id: u64) {
    let entry = {
        let st = state.lock().unwrap();
        let mut guard = st.intercom_ports.lock().unwrap();
        guard.remove(&conn_id)
    };
    if let Some(entry) = entry {
        let _ = entry.shutdown.send(true);
        // Dropping the sender ends the worker's queue once it drains.
        drop(entry);
    }
}


/// Relay one port's push frames onto its manager connection as `intercom`
/// events. `Push::written` resolves only after the frame was actually
/// written to the socket (or failed), because delivered/read receipt
/// semantics depend on it; `Push::fence` is released with the ack. An
/// oversized frame is settled as an error without touching the connection
/// (and without killing the relay); the relay only dies with the
/// connection's output channel.
async fn intercom_port_relay(mut rx: mpsc::Receiver<crate::intercom::Push>, out: OutTx) {
    while let Some(push) = rx.recv().await {
        let frame = encode_event(&EventKind::Intercom {
            frame: push.value,
        });
        let written = if frame.bytes.len() > MAX_FRAME_SIZE as usize {
            Err("intercom frame exceeds the 4 MiB limit".to_string())
        } else {
            let (ack, wait) = tokio::sync::oneshot::channel();
            match out.send(OutFrame::ack(frame.bytes, ack)).await {
                Ok(()) => wait
                    .await
                    .unwrap_or_else(|_| Err("connection closed before write".to_string())),
                // The connection is gone; nothing more can be delivered.
                Err(_) => break,
            }
        };
        if let Some(reply) = push.written {
            let _ = reply.send(written);
        }
        drop(push.fence);
    }
    // The connection is gone: fail whatever is still queued.
    while let Some(push) = rx.recv().await {
        if let Some(reply) = push.written {
            let _ = reply.send(Err("connection closed".to_string()));
        }
        drop(push.fence);
    }
}

/// Serve one intercom op for a manager connection (protocol 7). Extension
/// parent connections only: the daemon keeps one port per connection and
/// answers with the op's result; frames for the session arrive as
/// `intercom` events. When the bridge is down the request fails with a
/// structured error (`disabled` vs `unavailable: <why>`).
/// Queue one intercom op for a connection. Called from the connection's
/// read loop, so a connection's ops are serialized per connection in wire
/// order by its worker — the same sequential semantics a client socket
/// connection has. The map lock is held only to look up or create the
/// queue, never across an op.
fn enqueue_intercom_op(
    state: &Shared,
    conn_id: u64,
) -> Result<mpsc::Sender<IntercomOp>, ProtoError> {
    let ports = {
        let st = state.lock().unwrap();
        st.intercom_ports.clone()
    };
    // The daemon-state lock is taken before the ports lock here and in
    // close/reset, so the connection liveness check cannot race teardown.
    let st = state.lock().unwrap();
    let handle = st
        .conns
        .get(&conn_id)
        .ok_or_else(|| ProtoError::new(E_INTERNAL, "connection gone"))?;
    if handle.kind != ClientKind::Extension {
        return Err(ProtoError::new(
            E_FORBIDDEN,
            "intercom is available to extension sessions only",
        ));
    }
    match handle.role {
        ConnRole::Parent => {}
        ConnRole::Cli => {
            return Err(ProtoError::new(
                E_FORBIDDEN,
                "intercom is available to pi sessions, not the cli",
            ))
        }
        ConnRole::Child => {
            return Err(ProtoError::new(
                E_FORBIDDEN,
                "intercom is not available to a hosted child",
            ))
        }
    }
    let session_id = handle
        .session_id
        .clone()
        .ok_or_else(|| ProtoError::new(E_SESSION_REQUIRED, "intercom requires a session"))?;
    let pi_pid = st.sessions.get(&session_id).map(|entry| entry.pi_pid).unwrap_or(0);
    let mut guard = ports.lock().unwrap();
    if let Some(entry) = guard.get(&conn_id) {
        return Ok(entry.tx.clone());
    }
    let (tx, rx) = mpsc::channel::<IntercomOp>(64);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    // Bumped on every bridge restart so the worker drops its (dead-runtime)
    // port and answers queued ops deterministically instead of racing a
    // replacement worker.
    let (reset, reset_rx) = tokio::sync::watch::channel(0u64);
    let out = handle.tx.clone();
    let worker_state = state.clone();
    tokio::spawn(intercom_worker(
        worker_state,
        conn_id,
        session_id,
        pi_pid,
        rx,
        out,
        shutdown_rx,
        reset_rx,
    ));
    guard.insert(
        conn_id,
        IntercomEntry {
            tx: tx.clone(),
            shutdown,
            reset,
        },
    );
    Ok(tx)
}

/// A watch pair whose receiver's initial value is already seen, so
/// `changed()` pends until a real signal. The SENDER IS RETURNED TOO and
/// must be kept alive for as long as the receiver is used: `changed()` on
/// a receiver whose sender was dropped completes immediately with an error,
/// which would busy-loop the worker's select.
fn idle_watch() -> (
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<bool>,
) {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let _ = rx.borrow_and_update();
    (tx, rx)
}

/// Serve one connection's intercom ops in wire order. The worker owns the
/// port; it re-reads the bridge runtime before every op, so a restart or a
/// reset can never leave a port attached to a dead runtime, and it never
/// holds anything but the port across an op: a slow op delays only that
/// connection's later intercom ops, never other sessions, and never the
/// accept loop's close/reset paths.
#[allow(unused_assignments)] // the watch senders matter alive, not read
#[allow(clippy::too_many_arguments)] // the connection context plus its three wake sources
async fn intercom_worker(
    state: Shared,
    _conn_id: u64,
    session_id: String,
    pi_pid: u32,
    mut ops: mpsc::Receiver<IntercomOp>,
    out: OutTx,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    mut reset: tokio::sync::watch::Receiver<u64>,
) {
    let (push_tx, push_rx) = mpsc::channel::<crate::intercom::Push>(128);
    let relay = tokio::spawn(intercom_port_relay(push_rx, out.clone()));
    // The port and its bridge runtime; the watches start never-fired and
    // are replaced with each port so an unpublish interrupts the wait
    // below. The senders are never read; keeping them alive is what
    // matters (a dropped sender makes changed() complete immediately).
    let mut port: Option<crate::intercom::port::Port> = None;
    let mut port_rt: Option<crate::intercom::Shared> = None;
    let (mut unpublished_tx, mut unpublished) = idle_watch();
    let _ = &unpublished_tx;
    let _ = reset.borrow_and_update();
    loop {
        // (Re)create the port whenever the bridge runtime changed under it.
        let current = current_intercom_rt(&state);
        let on_current =
            matches!((&port_rt, &current), (Some(rt), Some(now)) if std::sync::Arc::ptr_eq(rt, now));
        if !on_current {
            if let Some(mut old) = port.take() {
                let _ = old.close().await;
            }
            port_rt = None;
            (unpublished_tx, unpublished) = idle_watch();
            if let Some(rt) = current {
                let created = crate::intercom::port::Port::new(
                    rt.clone(),
                    push_tx.clone(),
                    crate::intercom::port::PortOrigin::Manager {
                        session_id: session_id.clone(),
                        pi_pid,
                    },
                );
                unpublished = created.disconnected();
                // subscribe() starts with the current value unseen, which
                // would make the first changed() return immediately; mark
                // it seen so only a real unpublish wakes the select.
                let _ = unpublished.borrow_and_update();
                port = Some(created);
                port_rt = Some(rt);
            }
        }
        // An unpublish that landed DURING the previous op is not lost:
        // the in-op select's teardown runs Port::close(), which re-sends
        // the disconnect watch, so the select below observes it.
        enum Wake {
            Shutdown,
            Unpublished,
            Reset,
            Op(Option<IntercomOp>),
        }
        let wake = tokio::select! {
            biased;
            _ = shutdown.changed() => Wake::Shutdown,
            _ = unpublished.changed() => Wake::Unpublished,
            _ = reset.changed() => Wake::Reset,
            op = ops.recv() => Wake::Op(op),
        };
        let op = match wake {
            Wake::Shutdown => break,
            Wake::Op(None) => break,
            Wake::Unpublished => {
                // The session was unpublished remotely: tell the extension
                // to re-register. Its next op lands on a fresh, unbound
                // port and gets a clear error until it is restored.
                let _ = out.try_send(encode_event(&EventKind::IntercomReset {}));
                if let Some(mut old) = port.take() {
                    let _ = old.close().await;
                }
                port_rt = None;
                (unpublished_tx, unpublished) = idle_watch();
                continue;
            }
            Wake::Reset => {
                // The bridge restarted: the port belongs to the dead
                // runtime; the extension re-registers on the fresh port
                // its next op creates. Answer every queued op with a
                // deterministic reset error, in order, after whatever op
                // was in flight has been answered. Idempotent ops may be
                // retried by the extension after re-registering; send may
                // not.
                if let Some(mut old) = port.take() {
                    let _ = old.close().await;
                }
                port_rt = None;
                (unpublished_tx, unpublished) = idle_watch();
                while let Ok(queued) = ops.try_recv() {
                    if out
                        .send(encode_error(
                            &queued.id,
                            E_INTERCOM_RESET,
                            "intercom reset: re-register and retry idempotent ops",
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                continue;
            }
            Wake::Op(Some(op)) => op,
        };
        let Some(port) = port.as_mut() else {
            let _ = out.send(encode_bridge_down(&state, &op.id)).await;
            continue;
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                // Cancelled mid-op: run the cleanup the op would have
                // done (takeover removals, watcher settle) before dying.
                let _ = port.close().await;
                Err("connection closed".to_string())
            }
            _ = unpublished.changed() => {
                let _ = port.close().await;
                Err("session ownership ended".to_string())
            }
            result = port.handle(&op.req) => result,
        };
        let frame = match &result {
            Ok(value) => encode_ok_value(&op.id, value),
            Err(error) => encode_error(&op.id, E_BAD_REQUEST, error),
        };
        if out.send(frame).await.is_err() {
            break;
        }
    }
    // Connection teardown: settle unread deliveries as dropped, but only
    // when the pi session is really gone — a clean shutdown_session, or a
    // dead pi process (the daemon's crash signal). A plain reconnect gap
    // clears conn_id before we run; the session's queue must survive for
    // the re-registration, and the extension reports dropped itself when
    // the session actually changes.
    if let Some(mut port) = port.take() {
        let removed = port.close().await;
        if let Some(id) = removed {
            let gone = {
                let st = state.lock().unwrap();
                match st.sessions.get(&session_id) {
                    Some(entry) => {
                        entry.ended_clean
                            || (entry.pi_pid != 0 && !crate::sys::pid_alive(entry.pi_pid))
                    }
                    None => true,
                }
            };
            if gone {
                if let Some(rt) = port_rt {
                    rt.settle_unread_target(&id, "session closed").await;
                }
            }
        }
    }
    relay.abort();
}

/// The bridge runtime currently published for intercom, if any.
fn current_intercom_rt(state: &Shared) -> Option<crate::intercom::Shared> {
    state.lock().unwrap().intercom_rt.lock().unwrap().clone()
}

/// A success response whose body is the op's JSON result.
fn encode_ok_value(id: &str, body: &serde_json::Value) -> OutFrame {
    OutFrame::plain(Arc::new(encode(&Response {
        v: PROTO_VERSION,
        id: id.to_string(),
        ok: true,
        body,
    })))
}

/// The structured bridge-down error: `disabled` is a deliberate off switch,
/// anything else is an outage the extension retries.
fn encode_bridge_down(state: &Shared, id: &str) -> OutFrame {
    let error = state.lock().unwrap().intercom_error.clone();
    let message = error
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "bridge not running".to_string());
    let disabled = message.starts_with("disabled by");
    encode_error(
        id,
        if disabled {
            E_INTERCOM_DISABLED
        } else {
            E_INTERCOM_UNAVAILABLE
        },
        &message,
    )
}


/// The running bridge restarts when its configuration changed and also when
/// the loaded identity changed: `device.json`-only updates (re-enrollment, a
/// new identity) never touch the configuration file, but the bridge's relay
/// credential and device id came from the identity loaded at start.
fn bridge_stale(
    current: &crate::intercom::Running,
    fresh: &crate::intercom::config::Config,
) -> bool {
    if current.config() != fresh {
        return true;
    }
    identity_changed_since(&current.runtime.identity, &fresh.state_dir)
}
fn identity_changed_since(
    current: &crate::intercom::config::Identity,
    state_dir: &std::path::Path,
) -> bool {
    match crate::intercom::config::Identity::load(state_dir) {
        Ok(loaded) => {
            loaded.device_id != current.device_id
                || loaded.tenant_id != current.tenant_id
                || loaded.credential != current.credential
                || loaded.server != current.server
        }
        Err(_) => false,
    }
}
/// Makes the bridge's Supervised-only capabilities match the leases held
/// now, and describes the bridge for a lease holder. Becoming Supervised
/// rereads the configuration: a bridge that failed at boot, or one whose
/// configuration changed since (enrollment, `service install`), restarts.
async fn apply_supervision(
    state: &Shared,
    running: &mut Option<crate::intercom::Running>,
    bridge_error: &mut Option<String>,
) -> String {
    let (leased, home, clock) = {
        let st = state.lock().unwrap();
        (
            st.conns.values().any(|conn| conn.supervisor),
            st.home.clone(),
            st.clock.clone(),
        )
    };
    if leased && !running.as_ref().is_some_and(|running| running.supervised()) {
        let fresh = crate::intercom::bridge_config(&home);
        let stale = match (running.as_ref(), &fresh) {
            (Some(current), Ok(config)) => bridge_stale(current, config),
            (None, _) => true,
            (Some(_), Err(_)) => false,
        };
        if stale {
            if let Some(current) = running.take() {
                crate::intercom::carry_dedup(current.shutdown_for_handover().await);
            }
            (*running, *bridge_error) = start_bridge(&home, clock).await;
        }
    }
    let Some(running) = running.as_mut() else {
        return format!(
            "unavailable: {}",
            bridge_error.as_deref().unwrap_or("not running")
        );
    };
    if leased {
        running.supervise().await;
    } else {
        running.unsupervise().await;
    }
    if running.supervised() {
        "supervised".into()
    } else {
        "self-managed".into()
    }
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
    #[cfg(windows)]
    {
        let _ = state;
        Err(ProtoError::new(
            E_INTERNAL,
            "in-place upgrade is not supported on Windows; restart the manager after replacing the binary",
        ))
    }
    #[cfg(unix)]
    {
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
        Option<task::TeeSource>,
        Option<task::TeeSource>,
        Arc<Mutex<task::OutputState>>,
        Option<std::fs::File>,
        mpsc::Sender<task::OutputChunk>,
        tokio::sync::watch::Receiver<bool>,
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
    if let Some(monitor) = e.monitor.as_mut() {
        if monitor.cursor() != delivered {
            *monitor = crate::monitor::MonitorState::new(delivered);
        }
    }
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
    let monitor = state
        .lock()
        .unwrap()
        .registry
        .tasks
        .get(&tid)
        .is_some_and(|e| e.monitor.is_some());
    // Delay the first tick: tokio::interval fires immediately, which would
    // emit progress for a complete-but-not-yet-EOF tail on short commands.
    let mut progress = tokio::time::interval_at(
        tokio::time::Instant::now() + crate::monitor::PROGRESS_INTERVAL,
        crate::monitor::PROGRESS_INTERVAL,
    );
    progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let received = tokio::select! {
            chunk = rx.recv() => chunk,
            _ = progress.tick(), if monitor => {
                emit_monitor_output(&state, &tid, None, false, last_cursor, None);
                continue;
            }
        };
        let (bytes, next_cursor) = match received {
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
            None => {
                if !*park.borrow() {
                    emit_monitor_output(&state, &tid, None, true, last_cursor, None);
                }
                break;
            }
        };
        let mut offset = 0;
        let start = next_cursor - bytes.len() as u64;
        while offset < bytes.len() {
            let remaining = &bytes[offset..];
            let n = if monitor {
                task::utf8_chunk_len(remaining, 4096, CHUNK_JSON_BUDGET, false).max(1)
            } else {
                remaining.len()
            };
            offset += n;
            emit_monitor_output(
                &state,
                &tid,
                Some(&bytes[offset - n..offset]),
                false,
                last_cursor,
                Some(start + offset as u64),
            );
        }
    }
}

/// Queue bytes and their logical records under the same lock as watch replay.
/// Empty-chunk progress/EOF events do not advance the raw-byte cursor.
fn emit_monitor_output(
    state: &Shared,
    tid: &str,
    bytes: Option<&[u8]>,
    eof: bool,
    output_size: u64,
    next_cursor: Option<u64>,
) {
    let mut st = state.lock().unwrap();
    let Some(e) = st.registry.tasks.get_mut(tid) else {
        return;
    };
    e.record.output_size = e.record.output_size.max(output_size);
    let from = e.delivered_cursor;
    if let Some(cursor) = next_cursor {
        e.delivered_cursor = cursor;
    }
    let cursor = e.delivered_cursor;
    let records = e.monitor.as_mut().and_then(|m| {
        // History eviction must never disable fresh live metadata. A chunk
        // starting in an inherited unknown line still falls back as a unit.
        let available = m.available(from);
        let records = match bytes {
            Some(bytes) => m.push(bytes, now_ms()),
            None if eof => m.finish(now_ms()),
            None => m.progress(now_ms()),
        };
        available.then_some(records)
    });
    if bytes.is_none() && records.as_ref().is_none_or(Vec::is_empty) {
        return;
    }
    let watchers: Vec<_> = e.watchers.iter().copied().collect();
    let ev = encode_event_bytes(&EventKind::Output {
        task_id: tid.to_string(),
        chunk: String::from_utf8_lossy(bytes.unwrap_or_default()).into_owned(),
        next_cursor: cursor,
        monitor_records: records,
    });
    for cid in watchers {
        if let Some(h) = st.conns.get(&cid) {
            let _ = h.tx.try_send(OutFrame::output(ev.clone(), tid, cursor));
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
        #[cfg(unix)]
        {
            Outcome {
                code: status.and_then(|s| s.code()),
                signal: status.and_then(|s| s.signal()),
                usage: None,
            }
        }
        #[cfg(windows)]
        {
            let code = status.and_then(|s| s.code());
            match code {
                Some(c) if c == 128 + task::SIGTERM => Outcome {
                    code: None,
                    signal: Some(task::SIGTERM),
                    usage: None,
                },
                Some(c) if c == 128 + task::SIGKILL => Outcome {
                    code: None,
                    signal: Some(task::SIGKILL),
                    usage: None,
                },
                other => Outcome {
                    code: other,
                    signal: None,
                    usage: None,
                },
            }
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
    rx: &mut task::StatusRx,
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
/// Kill a task that still looks like Git Bash start-up (MSYS runtime race,
/// see `shellstart`): no output and nothing but the shell in its job for
/// `START_HANG_AFTER_MS`. A silent builtin-only command is the same shape,
/// so the watch can kill one; the job has no signal that the shell has
/// begun the command. Ends with the task's start-up; a daemon upgrade
/// drops it (the next daemon does not re-arm it).
#[cfg(windows)]
async fn watch_shell_start(state: Shared, tid: String, pid: u32) {
    use crate::shellstart::{serialize::phase, HangVerdict, HangWatch};
    let mut watch = HangWatch::default();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let output_size = {
            let st = state.lock().unwrap();
            match st.registry.tasks.get(&tid) {
                Some(e) if e.record.status == TaskStatus::Running && e.kill_reason.is_none() => {
                    e.output.lock().unwrap().total_size
                }
                _ => return,
            }
        };
        match watch.observe(now_ms(), output_size, phase(pid)) {
            HangVerdict::Wait => {}
            HangVerdict::Done => return,
            HangVerdict::Hung => {
                {
                    let mut st = state.lock().unwrap();
                    let home = st.home.clone();
                    if let Some(e) = st.registry.tasks.get_mut(&tid) {
                        e.request_kill(crate::shellstart::SHELL_START_HUNG);
                    }
                    lifecycle::log_line(
                        &home,
                        &format!("task {tid}: shell start-up hung; killing"),
                    );
                }
                kill_group_hard(pid).await;
                return;
            }
        }
    }
}

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
    let put_back = |runner: task::RunnerProc, status_rx: Option<task::StatusRx>, line: Vec<u8>| {
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
        #[cfg(windows)]
        crate::sys::drop_job(e.record.pid, e.job_generation);
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
                _ if e.fanout.as_ref().is_some_and(|f| !f.is_finished()) => {
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
    let mut lingering: Option<(u32, u64)> = None;
    let (sid, event) = {
        let mut st = state.lock().unwrap();
        let home = st.home.clone();
        let Some(entry) = st.registry.tasks.get_mut(task_id) else {
            return;
        };
        #[cfg(windows)]
        let job_pid = entry.record.pid;
        #[cfg(windows)]
        let job_generation = entry.job_generation;
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
        entry.record.live_cpu_user_ms = None;
        entry.record.live_cpu_sys_ms = None;
        entry.record.live_cpu_percent = None;
        entry.record.live_cpu_sampled_at = None;
        entry.record.live_cpu_stale = false;
        // Why it ended: our kill's reason; otherwise a natural exit.
        entry.record.end_reason = Some(
            entry
                .kill_reason
                .clone()
                .unwrap_or_else(|| end_reason::EXITED.to_string()),
        );
        // Likely Git Bash start-up crash (MSYS runtime race, see
        // `shellstart`): crash status, MSYS marker, and a short tail.
        // A nested shell can look the same after side effects.
        #[cfg(windows)]
        if entry.kill_reason.is_none()
            && crate::sys::task_shell().kind == crate::sys::ShellKind::Posix
        {
            let out = entry.output.lock().unwrap();
            let skip = out
                .ring
                .len()
                .saturating_sub(crate::shellstart::MAX_STARTUP_FAILURE_OUTPUT as usize);
            let tail = out.ring.slice(skip, out.ring.len() - skip);
            if crate::shellstart::is_startup_crash(code, out.total_size, &tail) {
                entry.record.end_reason = Some(crate::shellstart::SHELL_START_FAILED.to_string());
            }
        }
        if let Err(e) = registry::persist_record(&home, &entry.record) {
            lifecycle::log_line(
                &home,
                &format!("persist {} failed: {e}", entry.record.task_id),
            );
        }
        // The command is gone; descendants it backgrounded may not be.
        let pgid = entry.record.pid;
        match leftover {
            Leftover::None => {
                #[cfg(windows)]
                crate::sys::drop_job(job_pid, job_generation);
            }
            Leftover::Guarded => entry.group_lingering = true,
            Leftover::Probe => {
                if crate::sys::group_has_others(pgid) {
                    entry.group_lingering = true;
                    lingering = Some((pgid, entry.job_generation));
                } else {
                    #[cfg(windows)]
                    crate::sys::drop_job(job_pid, job_generation);
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
    if let Some((pgid, generation)) = lingering {
        spawn_group_watcher(state, task_id, pgid, generation);
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
                e.record.live_cpu_user_ms = None;
                e.record.live_cpu_sys_ms = None;
                e.record.live_cpu_percent = None;
                e.record.live_cpu_sampled_at = None;
                e.record.live_cpu_stale = false;
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

fn admits_kind(
    permits: &HashMap<(String, String), AgentPermit>,
    kind: &str,
    budget: usize,
) -> bool {
    permits
        .values()
        .filter(|permit| permit.work_kind == kind)
        .count()
        < budget
}

/// Grant the earliest eligible waiter, skipping a kind-blocked head so another
/// kind can use its independent budget. Removing an eligible index preserves
/// FIFO order among waiters of every individual kind.
fn wake_pending(st: &mut DaemonState) -> Vec<AgentGrant> {
    let Ok(capacity) = crate::capacity::load(&st.home) else {
        return Vec::new();
    };
    wake_pending_with_capacity(st, &capacity)
}

fn wake_pending_with_capacity(
    st: &mut DaemonState,
    capacity: &crate::capacity::CapacityConfig,
) -> Vec<AgentGrant> {
    let mut wake = Vec::new();
    let Ok(global) = capacity.max_agents() else {
        return wake;
    };
    while st.agent_permits.len() < global {
        let eligible = st.pending_agents.iter().position(|pending| {
            capacity
                .max_kind(&pending.work_kind)
                .is_ok_and(|budget| admits_kind(&st.agent_permits, &pending.work_kind, budget))
        });
        let Some(index) = eligible else {
            break;
        };
        let pending = st.pending_agents.remove(index).unwrap();
        let owner_request_id = pending.request_ids[0].clone();
        st.agent_permits.insert(
            (pending.session_id.clone(), pending.child_id.clone()),
            AgentPermit {
                work_kind: pending.work_kind,
                request_id: owner_request_id,
                release_on_settle: pending.release_on_settle,
                turn_started: false,
            },
        );
        for request_id in pending.request_ids {
            st.undelivered_agent_grants.insert(
                (pending.session_id.clone(), request_id.clone()),
                UndeliveredGrant {
                    child_id: pending.child_id.clone(),
                    queued_behind: Some(pending.queued_behind),
                    requester_child: pending.requester_child.clone(),
                },
            );
            wake.push(AgentGrant {
                session_id: pending.session_id.clone(),
                child_id: pending.child_id.clone(),
                request_id,
                tx: pending.tx.clone(),
                queued_behind: Some(pending.queued_behind),
            });
        }
    }
    wake
}

#[cfg(test)]
fn admit_agent(
    permits: &mut HashMap<(String, String), AgentPermit>,
    key: (String, String),
    budget: usize,
) -> AgentAdmissionOk {
    if permits.contains_key(&key) {
        return AgentAdmissionOk {
            granted: true,
            rejection: None,
            queued_behind: None,
        };
    }
    if permits.len() >= budget {
        return AgentAdmissionOk {
            granted: false,
            rejection: Some("global_capacity".into()),
            queued_behind: None,
        };
    }
    permits.insert(
        key.clone(),
        AgentPermit {
            work_kind: "unknown".into(),
            request_id: format!("request-{}", key.1),
            release_on_settle: false,
            turn_started: false,
        },
    );
    AgentAdmissionOk {
        granted: true,
        rejection: None,
        queued_behind: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn writer_records_the_output_cursor_only_on_a_successful_write() {
        let (a, b) = tokio::io::duplex(64);
        drop(b); // every write fails
        let written: Written = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel(8);
        let writers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn(writer_task(a, rx, written.clone(), CountGuard::new(&writers)));
        tx.send(OutFrame::output(Arc::new(vec![0u8; 16]), "sh_x", 42))
            .await
            .unwrap();
        drop(tx);
        let _ = task.await;
        assert!(
            written.lock().unwrap().is_empty(),
            "a failed write advanced the resend cursor"
        );
    }

    #[tokio::test]
    async fn an_oversized_intercom_frame_errors_without_killing_the_relay() {
        let (tx, rx) = mpsc::channel::<crate::intercom::Push>(8);
        let (out, mut out_rx) = mpsc::channel::<OutFrame>(8);
        let relay = tokio::spawn(intercom_port_relay(rx, out));
        let big = serde_json::json!({"type": "deliver", "body": "x".repeat(MAX_FRAME_SIZE as usize + 64)});
        let (w, wait) = tokio::sync::oneshot::channel();
        tx.send(crate::intercom::Push {
            value: big,
            written: Some(w),
            fence: None,
        })
        .await
        .unwrap();
        // The oversized push settles as an error, promptly, and never
        // reaches the connection.
        let result = tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .expect("oversized frame settles promptly")
            .expect("oversized frame settles");
        assert!(result.is_err(), "oversized frame settled as ok: {result:?}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
                .await
                .is_err(),
            "the oversized frame reached the connection"
        );
        // The relay is alive: the next frame flows and settles.
        let (w2, wait2) = tokio::sync::oneshot::channel();
        tx.send(crate::intercom::Push {
            value: serde_json::json!({"type": "receipt"}),
            written: Some(w2),
            fence: None,
        })
        .await
        .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("relay still forwards after the oversized frame")
            .expect("relay still forwards");
        if let Some(ack) = frame.ack {
            let _ = ack.send(Ok(()));
        }
        assert!(wait2.await.expect("receipt settles").is_ok());
        relay.abort();
    }

    #[tokio::test]
    async fn intercom_enqueue_rejects_cli_and_child_connections() {
        let home = std::env::temp_dir().join(format!("pi-famulus-iq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let state: Shared = Arc::new(Mutex::new(DaemonState::new(
            home.clone(),
            crate::registry::Registry::new(home.clone()),
            false,
        )));
        let mk = |role: ConnRole, kind: ClientKind, session: Option<&str>| {
            let (tx, rx) = mpsc::channel(8);
            let writer = tokio::spawn(std::future::pending::<()>()).abort_handle();
            (
                ConnHandle {
                    kind,
                    role,
                    child_id: None,
                    session_id: session.map(str::to_string),
                    tx,
                    die: Arc::new(Notify::new()),
                    written: Arc::new(Mutex::new(HashMap::new())),
                    writer,
                    supervisor: false,
                },
                rx,
            )
        };
        let (cli_handle, _cli_rx) = mk(ConnRole::Cli, ClientKind::Cli, None);
        state.lock().unwrap().conns.insert(1, cli_handle);
        let (child_handle, _child_rx) =
            mk(ConnRole::Child, ClientKind::Extension, Some("child-session"));
        state.lock().unwrap().conns.insert(2, child_handle);
        let (parent_handle, mut parent_rx) =
            mk(ConnRole::Parent, ClientKind::Extension, Some("parent-session"));
        state.lock().unwrap().conns.insert(3, parent_handle);
        let cli = enqueue_intercom_op(&state, 1).unwrap_err();
        assert_eq!(cli.code, E_FORBIDDEN, "{cli:?}");
        let child = enqueue_intercom_op(&state, 2).unwrap_err();
        assert_eq!(child.code, E_FORBIDDEN, "{child:?}");
        // A parent connection without a session is rejected too.
        let (sessionless_handle, _sessionless_rx) =
            mk(ConnRole::Parent, ClientKind::Extension, None);
        state.lock().unwrap().conns.insert(4, sessionless_handle);
        let no_session = enqueue_intercom_op(&state, 4).unwrap_err();
        assert_eq!(no_session.code, E_SESSION_REQUIRED, "{no_session:?}");
        // A live parent connection gets a queue and a worker, and the
        // worker answers the op.
        let queue = enqueue_intercom_op(&state, 3).unwrap();
        queue
            .send(IntercomOp {
                id: "q".into(),
                req: serde_json::json!({"op":"list"}),
            })
            .await
            .unwrap();
        let frame = parent_rx.recv().await.expect("the worker answers the op");
        let response = std::str::from_utf8(&frame.bytes).unwrap();
        let response: serde_json::Value = serde_json::from_str(response).unwrap();
        // The bare DaemonState publishes no bridge runtime, so the worker
        // answers with the structured bridge-down error — deterministic on
        // any host. What matters here is that the op reached the worker and
        // got an answered envelope echoing the request id; the code pins the
        // exact contract instead of merely "some response".
        assert_eq!(response["id"], "q", "{response}");
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(
            response["error"]["code"], E_INTERCOM_UNAVAILABLE,
            "{response}"
        );
        drop(queue);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn client_labels_are_one_clipped_line() {
        assert_eq!(
            clean_label("watch\n\x1b[31mbuild\t "),
            Some("watch [31mbuild".into())
        );
        assert_eq!(clean_label(" \n "), None);
        assert_eq!(
            clean_label("tail \u{202E}gol.exe"),
            Some("tail gol.exe".into())
        );
        let long = clean_label(&"x".repeat(500)).unwrap();
        assert_eq!(long.chars().count(), crate::events::COMMAND_CHARS);
    }

    fn running_record_for_cpu_test() -> TaskRecord {
        serde_json::from_value(json!({
            "task_id": "sh_cpu_test",
            "session_id": "s1",
            "kind": "shell",
            "command": "sleep 10",
            "cwd": "/tmp",
            "pid": 1234,
            "status": "running",
            "started_at": 1,
            "output_path": "/tmp/output",
            "output_size": 0
        }))
        .unwrap()
    }

    #[test]
    fn post_handover_live_sample_preserves_existing_cpu_high_water() {
        let mut record = running_record_for_cpu_test();
        record.live_cpu_user_ms = Some(5_000);
        record.live_cpu_sys_ms = Some(700);
        apply_live_cpu_sample(
            &mut record,
            Some(task::LiveCpuReading {
                user_ms: 120,
                system_ms: 30,
                percent: None,
            }),
            2_000,
        );
        assert_eq!(
            (record.live_cpu_user_ms, record.live_cpu_sys_ms),
            (Some(5_000), Some(700))
        );
        assert_eq!(record.live_cpu_sampled_at, Some(2_000));
        assert!(!record.live_cpu_stale);
    }

    #[test]
    fn failed_live_cpu_sample_clears_rate_but_preserves_totals_and_last_timestamp() {
        let mut record = running_record_for_cpu_test();
        apply_live_cpu_sample(
            &mut record,
            Some(task::LiveCpuReading {
                user_ms: 120,
                system_ms: 30,
                percent: Some(24.0),
            }),
            1_000,
        );
        assert_eq!(record.live_cpu_sampled_at, Some(1_000));
        assert!(!record.live_cpu_stale);

        apply_live_cpu_sample(&mut record, None, 6_000);
        assert_eq!(
            (record.live_cpu_user_ms, record.live_cpu_sys_ms),
            (Some(120), Some(30))
        );
        assert_eq!(record.live_cpu_percent, None);
        assert_eq!(record.live_cpu_sampled_at, Some(1_000));
        assert!(record.live_cpu_stale);

        let mut never_sampled = running_record_for_cpu_test();
        apply_live_cpu_sample(&mut never_sampled, None, 6_000);
        assert_eq!(never_sampled.live_cpu_sampled_at, None);
        assert!(never_sampled.live_cpu_stale);
    }

    /// Regression: a tee that runs before its factory returns must not have
    /// its bytes read again as carry. Also preserve the incomplete raw byte
    /// across the same park/restart seam used by an in-place upgrade.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn utf8_watch_output_is_not_duplicated_when_tee_starts_eagerly() {
        use std::os::fd::AsRawFd;
        let home = std::env::temp_dir().join(format!(
            "pi-famulus-fanout-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let path = registry::task_output_path(&home, "sess-utf8", "mon_utf8");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (file, _) = task::open_output_files(&path).unwrap();
        let output = Arc::new(Mutex::new(task::OutputState::new(Some(file), 0)));
        let record: TaskRecord = serde_json::from_value(serde_json::json!({
            "task_id":"mon_utf8", "session_id":"sess-utf8", "kind":"monitor",
            "command":"split UTF-8 pipe producer", "cwd":"/tmp", "pid":0,
            "status":"running", "exit_code":null, "signal":null,
            "started_at":0, "ended_at":null, "output_path":path, "output_size":0
        }))
        .unwrap();
        let (read, write) = crate::sys::pipe_cloexec().unwrap();
        let mut entry = TaskEntry::bare(record, output.clone());
        entry.stdout_fd = Some(read);
        entry.watchers.insert(1);
        let mut registry = Registry::new(home.clone());
        registry.tasks.insert("mon_utf8".into(), entry);
        let state = Arc::new(Mutex::new(DaemonState::new(home.clone(), registry, false)));
        let (tx, mut events) = mpsc::channel(8);
        let writer = tokio::spawn(std::future::pending::<()>());
        state.lock().unwrap().conns.insert(
            1,
            ConnHandle {
                kind: ClientKind::Extension,
                role: ConnRole::Parent,
                child_id: None,
                session_id: Some("sess-utf8".into()),
                tx,
                die: Arc::new(Notify::new()),
                written: Arc::new(Mutex::new(HashMap::new())),
                writer: writer.abort_handle(),
                supervisor: false,
            },
        );
        // Real initial bytes, not a fabricated OutputChunk. Wait only for the
        // actual append (with a failure watchdog), never for a fixed sleep.
        crate::sys::write_raw(write.as_raw_fd(), b"a\xe4").unwrap();
        let eager_tee = |expected| {
            move |stdout, stderr, output: Arc<Mutex<task::OutputState>>, mirror, tx, park| {
                let tee = task::start_tee(stdout, stderr, output.clone(), mirror, tx, park)?;
                let until = std::time::Instant::now() + Duration::from_secs(3);
                while output.lock().unwrap().total_size < expected {
                    assert!(
                        std::time::Instant::now() < until,
                        "real tee did not append {expected} bytes"
                    );
                    std::thread::yield_now();
                }
                Ok(tee)
            }
        };
        start_task_io_with_tee(&state, "mon_utf8", eager_tee(2));
        let first = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
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
            .await
            .expect("stdout tee did not park")
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(3), tee.stderr)
            .await
            .expect("stderr tee did not park")
            .unwrap()
            .is_none());
        tokio::time::timeout(Duration::from_secs(3), fanout)
            .await
            .expect("fanout did not park")
            .unwrap();
        {
            let mut st = state.lock().unwrap();
            let e = st.registry.tasks.get_mut("mon_utf8").unwrap();
            assert_eq!(e.delivered_cursor, 1);
            assert_eq!(e.output.lock().unwrap().total_size, 2);
            e.stdout_fd = stdout;
            st.park_tx.send_replace(false);
        }
        assert!(
            events.try_recv().is_err(),
            "incomplete byte must not flush at park"
        );
        crate::sys::write_raw(write.as_raw_fd(), b"\xb8\xadb\n").unwrap();
        start_task_io_with_tee(&state, "mon_utf8", eager_tee(6));
        drop(write);
        let last = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
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
            .await
            .expect("stdout tee did not reach EOF")
            .unwrap()
            .is_none());
        assert!(tokio::time::timeout(Duration::from_secs(3), tee.stderr)
            .await
            .expect("stderr tee did not reach EOF")
            .unwrap()
            .is_none());
        tokio::time::timeout(Duration::from_secs(3), fanout)
            .await
            .expect("fanout did not drain at EOF")
            .unwrap();
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

    /// Regression for p16: exercise real request dispatch with a bounded writer
    /// channel whose capacity cannot change behind the test. Socket fullness is
    /// transient, and any later session request may re-serve a retained grant.
    async fn queued_grant_after_full_writer(retry_acquire: bool) {
        let home = std::env::temp_dir().join(format!(
            "pi-famulus-grant-{}-{}-{retry_acquire}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.json"), r#"{"maxAgents":3,"maxTest":1}"#).unwrap();
        let state = Arc::new(Mutex::new(DaemonState::new(
            home.clone(),
            Registry::new(home.clone()),
            false,
        )));
        let (holder_tx, mut holder_rx) = mpsc::channel(2);
        let (waiter_tx, mut waiter_rx) = mpsc::channel(2);
        let writer = tokio::spawn(std::future::pending::<()>());
        let mut connections = Vec::new();
        for (session, tx) in [("p16-holder", &holder_tx), ("p16-waiter", &waiter_tx)] {
            connections.push(
                register_conn(
                    &state,
                    tx,
                    &Arc::new(Notify::new()),
                    &Arc::new(Mutex::new(HashMap::new())),
                    &writer.abort_handle(),
                    ClientKind::Extension,
                    Some(session.into()),
                    Some(std::process::id()),
                    HelloInfo {
                        cwd: None,
                        extension_version: None,
                        protocol: Some(5),
                        role: ConnRole::Parent,
                        child_id: None,
                        token: None,
                        session_file: None,
                        pi_version: None,
                    },
                )
                .unwrap(),
            );
        }
        let request = |id: &str, kind: &str, child: &str| {
            let mut input = json!({ "id": id, "type": kind });
            if !child.is_empty() {
                input["child_id"] = json!(child);
            }
            if kind == "acquire_agent" {
                input["work_kind"] = json!("test");
            }
            parse_request(input.to_string().as_bytes()).unwrap()
        };
        dispatch(
            state.clone(),
            connections[0],
            request("p16-hold", "acquire_agent", "ch_hold"),
            holder_tx.clone(),
        )
        .await;
        let held: serde_json::Value =
            serde_json::from_slice(&holder_rx.try_recv().unwrap().bytes).unwrap();
        assert_eq!(held["granted"], true);
        dispatch(
            state.clone(),
            connections[1],
            request("p16-wait", "acquire_agent", "ch_wait"),
            waiter_tx.clone(),
        )
        .await;
        assert!(
            waiter_rx.try_recv().is_err(),
            "waiter must not bypass held capacity"
        );

        for id in ["p16-status-0", "p16-status-1"] {
            dispatch(
                state.clone(),
                connections[1],
                request(id, "status", ""),
                waiter_tx.clone(),
            )
            .await;
        }
        assert_eq!(waiter_tx.capacity(), 0);
        dispatch(
            state.clone(),
            connections[0],
            request("p16-release", "release_agent", "ch_hold"),
            holder_tx.clone(),
        )
        .await;
        let released: serde_json::Value =
            serde_json::from_slice(&holder_rx.try_recv().unwrap().bytes).unwrap();
        assert_eq!(released["ok"], true);
        assert_eq!(waiter_tx.capacity(), 0, "release must hit the full writer");
        assert_eq!(
            state
                .lock()
                .unwrap()
                .undelivered_agent_grants
                .get(&("p16-waiter".into(), "p16-wait".into()))
                .map(|grant| grant.child_id.as_str()),
            Some("ch_wait")
        );
        for id in ["p16-status-0", "p16-status-1"] {
            let frame: serde_json::Value =
                serde_json::from_slice(&waiter_rx.try_recv().unwrap().bytes).unwrap();
            assert_eq!(
                frame["id"], id,
                "full writer must not lose queued responses"
            );
        }
        let next = if retry_acquire {
            request("p16-wait", "acquire_agent", "ch_wait")
        } else {
            request("p16-after-drain", "status", "")
        };
        dispatch(state.clone(), connections[1], next, waiter_tx.clone()).await;
        let grant: serde_json::Value =
            serde_json::from_slice(&waiter_rx.try_recv().unwrap().bytes).unwrap();
        assert_eq!(grant["id"], "p16-wait");
        assert_eq!(grant["granted"], true);
        if !retry_acquire {
            let status: serde_json::Value =
                serde_json::from_slice(&waiter_rx.try_recv().unwrap().bytes).unwrap();
            assert_eq!(status["id"], "p16-after-drain");
            assert_eq!(status["agent_capacity"]["by_kind"]["test"]["used"], 1);
        }
        assert!(
            waiter_rx.try_recv().is_err(),
            "one request must not duplicate its retained grant"
        );
        {
            let st = state.lock().unwrap();
            assert!(st.pending_agents.is_empty());
            assert!(st.undelivered_agent_grants.is_empty());
            assert_eq!(
                st.agent_permits.len(),
                1,
                "retry must not allocate another permit"
            );
        }
        writer.abort();
        drop(state);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn queued_grant_survives_full_writer_and_same_id_retry() {
        tokio::time::timeout(Duration::from_secs(3), queued_grant_after_full_writer(true))
            .await
            .expect("same-id grant recovery stalled");
    }

    #[tokio::test]
    async fn queued_grant_survives_full_writer_and_status_request() {
        tokio::time::timeout(
            Duration::from_secs(3),
            queued_grant_after_full_writer(false),
        )
        .await
        .expect("status-triggered grant recovery stalled");
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

    /// A Ctrl-C registration error must not complete the shutdown branch, and
    /// the next accepted connection must not poll that future again.
    #[tokio::test]
    async fn ctrl_c_registration_error_survives_the_next_connection() {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct OnceErr {
            polled: bool,
        }
        impl Future for OnceErr {
            type Output = std::io::Result<()>;
            fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                assert!(!self.polled, "ctrl-c future polled after it completed");
                self.polled = true;
                Poll::Ready(Err(std::io::Error::other(
                    "ctrl-c handler already registered",
                )))
            }
        }

        let home = std::env::temp_dir().join(format!("pi-famulus-ctrlc-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let ctrl = super::ctrl_c_or_pending(&home, OnceErr { polled: false });
        tokio::pin!(ctrl);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<&'static str>(2);
        tx.try_send("conn").unwrap();
        let mut accepted = 0u32;
        for _ in 0..4 {
            tokio::select! {
                biased;
                _ = &mut ctrl => panic!("registration failure must not shut the daemon down"),
                msg = rx.recv() => {
                    assert_eq!(msg, Some("conn"));
                    accepted += 1;
                    if accepted == 1 {
                        tx.try_send("conn").unwrap();
                    } else {
                        break;
                    }
                }
            }
        }
        assert_eq!(accepted, 2);
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod intercom_bridge_tests {
    use super::identity_changed_since;

    #[test]
    fn bridge_staleness_detects_device_json_only_identity_changes() {
        let dir = std::env::temp_dir().join(format!(
            "famulus-ident-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let enrolled = crate::intercom::config::Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            credential: "credential-one".into(),
            server: "https://relay.invalid".into(),
            ..Default::default()
        };
        enrolled.save(&dir).unwrap();
        assert!(
            !identity_changed_since(&enrolled, &dir),
            "an unchanged device.json must not restart the bridge"
        );
        // Re-enrollment rotates the credential and device id in device.json
        // alone; the configuration file never changes.
        let rotated = crate::intercom::config::Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            credential: "credential-two".into(),
            server: "https://relay.invalid".into(),
            ..Default::default()
        };
        rotated.save(&dir).unwrap();
        assert!(
            identity_changed_since(&enrolled, &dir),
            "a device.json-only identity change must reload the bridge"
        );
        assert!(!identity_changed_since(&rotated, &dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
