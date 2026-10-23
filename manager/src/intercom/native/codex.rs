use super::super::{str_field, Shared};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

const CACHE_TTL: u64 = 20_000;
const FULL_REFRESH: u64 = 60_000;
const RECEIPT_TTL: u64 = 600_000;
const CALL_TIMEOUT: Duration = Duration::from_secs(8);
const OPT_OUT: [&str; 14] = [
    "item/started",
    "item/completed",
    "item/autoApprovalReview/started",
    "item/autoApprovalReview/completed",
    "item/agentMessage/delta",
    "item/plan/delta",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "item/mcpToolCall/progress",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
];

#[derive(Clone)]
pub(super) struct Client {
    inner: Arc<Inner>,
}
struct Inner {
    connection: tokio::sync::Mutex<Option<Connection>>,
    data: Arc<Data>,
    owner: Weak<super::super::Runtime>,
    clock: crate::clock::Clock,
    stop: watch::Receiver<bool>,
    closing: watch::Receiver<bool>,
    probe: Mutex<Probe>,
}
#[derive(Default)]
struct Data {
    cache: Mutex<Cache>,
    wake: Notify,
    generation: AtomicU64,
}
#[derive(Default)]
struct Cache {
    rows: Vec<Value>,
    at: Option<u64>,
    full_at: Option<u64>,
    dirty: bool,
    overlay: Option<HashMap<String, Patch>>,
}
#[derive(Clone, Default)]
struct Patch {
    status: Option<String>,
    name: Option<String>,
    at: Option<u64>,
    closed: bool,
}
#[derive(Default)]
struct Probe {
    fingerprint: Option<(u64, u64, i64, i64)>,
    failures: usize,
    retry_at: u64,
}
impl Probe {
    fn quiet(&self, fingerprint: (u64, u64, i64, i64), now: u64) -> bool {
        self.failures >= 3 && self.fingerprint == Some(fingerprint) && now < self.retry_at
    }
    fn failed(&mut self, fingerprint: (u64, u64, i64, i64), now: u64) {
        if self.fingerprint != Some(fingerprint) {
            *self = Self::default();
            self.fingerprint = Some(fingerprint);
        }
        self.failures += 1;
        if self.failures >= 3 {
            self.retry_at = now.saturating_add(60_000);
        }
    }
}

#[derive(Clone)]
struct Connection {
    tx: mpsc::Sender<Wire>,
    state: Arc<Mutex<HostState>>,
    alive: Arc<AtomicBool>,
    next: Arc<AtomicU64>,
    generation: u64,
    _task: Arc<AbortTask>,
}
struct AbortTask(tokio::task::AbortHandle);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Alive(Arc<AtomicBool>);
impl Drop for Alive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[derive(Default)]
struct HostState {
    needs_user: HashMap<String, String>,
    tracker: Tracker,
}
#[derive(Default)]
struct Tracker {
    waiting: HashMap<(String, String), Vec<(String, u64)>>,
    completed: HashMap<(String, String), u64>,
    fired: HashMap<(String, String), HashSet<String>>,
}
impl Tracker {
    fn prune(&mut self, now: u64) {
        self.completed.retain(|key, at| {
            if now.saturating_sub(*at) > RECEIPT_TTL {
                self.fired.remove(key);
                false
            } else {
                true
            }
        });
        self.waiting.retain(|_, rows| {
            rows.retain(|(_, at)| now.saturating_sub(*at) <= RECEIPT_TTL);
            !rows.is_empty()
        });
    }
    fn remember(&mut self, thread: &str, turn: &str, mid: &str, now: u64) -> bool {
        if turn.is_empty() || mid.is_empty() {
            return false;
        }
        self.prune(now);
        let key = (thread.into(), turn.into());
        if self.fired.get(&key).is_some_and(|ids| ids.contains(mid))
            || self
                .waiting
                .get(&key)
                .is_some_and(|rows| rows.iter().any(|(id, _)| id == mid))
        {
            return false;
        }
        if self.completed.contains_key(&key) {
            self.fired.entry(key).or_default().insert(mid.into());
            return true;
        }
        self.waiting.entry(key).or_default().push((mid.into(), now));
        false
    }
    fn complete(&mut self, thread: &str, turn: &str, status: &str, now: u64) -> Vec<String> {
        if thread.is_empty() || turn.is_empty() || status != "completed" {
            return Vec::new();
        }
        self.prune(now);
        let key = (thread.into(), turn.into());
        self.completed.insert(key.clone(), now);
        self.waiting
            .remove(&key)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(mid, _)| {
                self.fired
                    .entry(key.clone())
                    .or_default()
                    .insert(mid.clone())
                    .then_some(mid)
            })
            .collect()
    }
    fn drop_thread(&mut self, thread: &str) {
        self.waiting.retain(|(id, _), _| id != thread);
        self.completed.retain(|(id, _), _| id != thread);
        self.fired.retain(|(id, _), _| id != thread);
    }
}
#[derive(Debug)]
struct RpcError {
    code: Option<i64>,
    message: String,
}
impl RpcError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }
}
struct Pending {
    method: String,
    deadline: Instant,
    reply: oneshot::Sender<Result<Value, RpcError>>,
}
enum Wire {
    Call {
        id: u64,
        method: String,
        params: Value,
        pending: Pending,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    },
    Cancel(u64),
    Notify {
        method: String,
    },
}
struct Cancel {
    tx: mpsc::Sender<Wire>,
    id: u64,
}
impl Drop for Cancel {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Wire::Cancel(self.id));
    }
}

pub(super) async fn start(
    rt: &Shared,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let client = Client {
        inner: Arc::new(Inner {
            connection: tokio::sync::Mutex::new(None),
            data: Arc::new(Data::default()),
            owner: Arc::downgrade(rt),
            clock: rt.clock.clone(),
            stop,
            closing: rt.closing.subscribe(),
            probe: Mutex::new(Probe::default()),
        }),
    };
    *rt.native.codex.lock().unwrap() = Some(client.clone());
    #[cfg(unix)]
    {
        Some(tokio::spawn(async move {
            client.maintain().await;
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = client;
        None
    }
}

impl super::Native {
    pub async fn codex_status(&self) -> Value {
        let desktop = self
            .desktop
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|client| client.connected());
        let mut report =
            json!({"codex_app_server":if desktop {"Desktop IPC attached"} else {"not attached"}});
        #[cfg(unix)]
        if !desktop && diagnostic_reachable().await {
            report["codex_app_server"] = json!("external listener");
        }
        let client = self.codex.lock().unwrap().clone();
        if let Some(client) = client {
            let connection = client
                .inner
                .connection
                .lock()
                .await
                .as_ref()
                .filter(|connection| connection.alive.load(Ordering::Acquire))
                .cloned();
            if let Some(connection) = connection {
                if let Ok(listing) = connection.call("hooks/list", json!({"cwds":[]})).await {
                    if untrusted_plugin_hooks(&listing) {
                        report["codex_hooks"] =
                            json!("untrusted; run pi-famulus intercom codex setup");
                    }
                }
            }
        }
        report
    }
}

fn untrusted_plugin_hooks(listing: &Value) -> bool {
    if listing["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty())
    {
        return false;
    }
    let mut seen = HashSet::new();
    listing["data"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .any(|hook| {
            str_field(hook, "pluginId") == "intercom@agent-intercom"
                && !str_field(hook, "currentHash").is_empty()
                && seen.insert(str_field(hook, "key"))
                && matches!(
                    str_field(hook, "trustStatus"),
                    "untrusted" | "modified" | "absent" | ""
                )
        })
}

#[cfg(unix)]
async fn diagnostic_reachable() -> bool {
    tokio::time::timeout(Duration::from_secs(1), async {
        let stream = tokio::net::UnixStream::connect(control_socket()).await.map_err(|e| e.to_string())?;
        let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/rpc", stream).await.map_err(|e| e.to_string())?;
        super::codex_call(&mut ws, "initialize", json!({"clientInfo":{"name":"agent-intercom","title":"agent-intercom","version":"0.1.0"},"capabilities":{"optOutNotificationMethods":OPT_OUT}})).await?;
        ws.send(Message::text(json!({"method":"initialized"}).to_string())).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    }).await.is_ok_and(|result| result.is_ok())
}

#[derive(Debug)]
pub(super) enum HostError {
    Attach(String),
    Ownership(String),
    Operation(String),
}
impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Attach(error) | Self::Ownership(error) | Self::Operation(error) => {
                formatter.write_str(error)
            }
        }
    }
}
impl From<String> for HostError {
    fn from(error: String) -> Self {
        Self::Operation(error)
    }
}
impl std::error::Error for HostError {}

impl Client {
    pub(super) fn idle_generation(&self) -> u64 {
        self.inner.data.generation.load(Ordering::Acquire)
    }
    pub fn list(&self) -> Vec<Value> {
        let cache = self.inner.data.cache.lock().unwrap();
        if cache.dirty
            || cache
                .at
                .is_none_or(|at| self.inner.clock.now_ms().saturating_sub(at) >= CACHE_TTL)
        {
            self.inner.data.wake.notify_one();
        }
        cache.rows.clone()
    }
    pub async fn send(
        &self,
        thread: &str,
        body: String,
        mid: &str,
        fence: &mut Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<Option<String>, HostError> {
        validate_thread(thread)?;
        let connection = self
            .ensure(false)
            .await
            .map_err(HostError::Attach)?
            .ok_or_else(|| HostError::Attach("codex app-server unavailable".into()))?;
        let loaded = connection
            .loaded(None)
            .await
            .map_err(|e| HostError::Ownership(format!("codex thread ownership: {}", e.message)))?;
        if !loaded.iter().any(|id| id == thread) {
            return Ok(None);
        }
        let (_, active) = connection.route(thread).await?;
        let method = if active.is_empty() {
            "turn/start"
        } else {
            "turn/steer"
        };
        let mut params = json!({"threadId":thread,"input":[{"type":"text","text":body}]});
        if !active.is_empty() {
            params["expectedTurnId"] = json!(active);
        }
        let reply = connection
            .call_fenced(method, params, fence.take())
            .await
            .map_err(|error| {
                if matches!(error.code, Some(-32600 | -32602)) {
                    format!("codex send rejected: {}", error.message)
                } else {
                    format!("native send outcome unknown: {}", error.message)
                }
            })?;
        let turn = if method == "turn/steer" {
            str_field(&reply, "turnId")
        } else {
            str_field(&reply["turn"], "id")
        };
        if turn.is_empty() {
            return Err(
                format!("native send outcome unknown: {method} response missing turn id").into(),
            );
        }
        let early = connection.state.lock().unwrap().tracker.remember(
            thread,
            turn,
            mid,
            self.inner.clock.now_ms(),
        );
        Ok(Some(if early { "read" } else { "delivered" }.into()))
    }
    pub async fn rename(&self, thread: &str, name: &str) -> Result<(), String> {
        validate_thread(thread)?;
        let name = name.trim();
        if name.is_empty() {
            return Err("name required".into());
        }
        let connection = self
            .ensure(false)
            .await?
            .ok_or("codex app-server unavailable")?;
        connection
            .call("thread/name/set", json!({"threadId":thread,"name":name}))
            .await
            .map_err(|e| e.message)?;
        self.inner.data.cache.lock().unwrap().dirty = true;
        self.inner.data.wake.notify_one();
        Ok(())
    }
    pub async fn idle_state(&self, thread: &str) -> Result<Option<String>, HostError> {
        validate_thread(thread)?;
        let connection = self
            .ensure(false)
            .await
            .map_err(HostError::Attach)?
            .ok_or_else(|| HostError::Attach("codex app-server unavailable".into()))?;
        let loaded = connection.loaded(None).await.map_err(|e| e.message)?;
        if !loaded.iter().any(|id| id == thread) {
            return Ok(None);
        }
        let (status, _) = connection.route(thread).await?;
        if !self.current(&connection).await {
            return Err(HostError::Operation(
                "codex host connection changed during observation".into(),
            ));
        }
        if connection
            .state
            .lock()
            .unwrap()
            .needs_user
            .values()
            .any(|id| id == thread)
        {
            return Ok(Some("waiting".into()));
        }
        normalize_idle(&status)
            .map(|state| Some(state.into()))
            .map_err(HostError::Operation)
    }
    async fn current(&self, connection: &Connection) -> bool {
        connection.alive.load(Ordering::Acquire)
            && self.inner.data.generation.load(Ordering::Acquire) == connection.generation
            && !*self.inner.stop.borrow()
            && !*self.inner.closing.borrow()
    }
    #[cfg(unix)]
    async fn ensure(&self, background: bool) -> Result<Option<Connection>, String> {
        let mut current = self.inner.connection.lock().await;
        if *self.inner.stop.borrow() || *self.inner.closing.borrow() {
            return Err("codex adapter closed".into());
        }
        if let Some(connection) = current.as_ref().filter(|c| c.alive.load(Ordering::Acquire)) {
            return Ok(Some(connection.clone()));
        }
        *current = None;
        let path = control_socket();
        let fingerprint = fingerprint(&path);
        if background {
            match fingerprint {
                None => {
                    *self.inner.probe.lock().unwrap() = Probe::default();
                    self.inner.data.cache.lock().unwrap().rows.clear();
                    return Ok(None);
                }
                Some(fingerprint)
                    if self
                        .inner
                        .probe
                        .lock()
                        .unwrap()
                        .quiet(fingerprint, self.inner.clock.now_ms()) =>
                {
                    self.inner.data.cache.lock().unwrap().rows.clear();
                    return Ok(None);
                }
                _ => {}
            }
        }
        let connected = self.open(&path).await;
        match connected {
            Ok(connection) => {
                *self.inner.probe.lock().unwrap() = Probe::default();
                self.inner.data.cache.lock().unwrap().full_at = None;
                *current = Some(connection.clone());
                Ok(Some(connection))
            }
            Err(error) => {
                if background {
                    if let Some(fingerprint) = fingerprint {
                        self.inner
                            .probe
                            .lock()
                            .unwrap()
                            .failed(fingerprint, self.inner.clock.now_ms());
                    }
                }
                Err(error)
            }
        }
    }
    #[cfg(not(unix))]
    async fn ensure(&self, _background: bool) -> Result<Option<Connection>, String> {
        Err("Codex app-server supports Linux/macOS only".into())
    }
    #[cfg(unix)]
    async fn open(&self, path: &std::path::Path) -> Result<Connection, String> {
        let mut stop = self.inner.stop.clone();
        let mut closing = self.inner.closing.clone();
        let connect = async {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| e.to_string())?;
            tokio_tungstenite::client_async("ws://localhost/rpc", stream)
                .await
                .map(|(ws, _)| ws)
                .map_err(|e| e.to_string())
        };
        let ws = tokio::select! {
            _ = stop.changed() => return Err("codex adapter closed".into()),
            _ = closing.changed() => return Err("codex adapter closed".into()),
            result = tokio::time::timeout(Duration::from_secs(3), connect) => result.map_err(|_| "codex app-server attachment timed out".to_string())??,
        };
        let generation = self.inner.data.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (tx, rx) = mpsc::channel(64);
        let state = Arc::new(Mutex::new(HostState::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(run_connection(
            ws,
            rx,
            state.clone(),
            alive.clone(),
            generation,
            self.inner.data.clone(),
            self.inner.owner.clone(),
            self.inner.clock.clone(),
            stop,
            closing,
        ));
        let connection = Connection {
            tx,
            state,
            alive,
            generation,
            next: Arc::new(AtomicU64::new(0)),
            _task: Arc::new(AbortTask(task.abort_handle())),
        };
        // Register before initialize: cancellation or shutdown must reap even a
        // host that upgrades successfully but never answers the handshake.
        if let Some(rt) = self.inner.owner.upgrade() {
            let mut tasks = rt.native.tasks.lock().unwrap();
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
        connection.call("initialize", json!({"clientInfo":{"name":"agent-intercom","title":"agent-intercom","version":"0.1.0"},
            "capabilities":{"optOutNotificationMethods":OPT_OUT}})).await.map_err(|e| format!("codex initialize: {}", e.message))?;
        connection
            .tx
            .send(Wire::Notify {
                method: "initialized".into(),
            })
            .await
            .map_err(|_| "codex connection closed".to_string())?;
        Ok(connection)
    }
    #[cfg(unix)]
    async fn maintain(&self) {
        let mut stop = self.inner.stop.clone();
        let mut closing = self.inner.closing.clone();
        let mut retry_at = 0;
        loop {
            if *stop.borrow() || *closing.borrow() {
                return;
            }
            let now = self.inner.clock.now_ms();
            let due = {
                let cache = self.inner.data.cache.lock().unwrap();
                cache.dirty
                    || cache
                        .at
                        .is_none_or(|at| now.saturating_sub(at) >= CACHE_TTL)
                    || cache
                        .full_at
                        .is_none_or(|at| now.saturating_sub(at) >= FULL_REFRESH)
            };
            if due && now >= retry_at {
                let refreshed = tokio::select! {
                    _ = stop.changed() => return,
                    _ = closing.changed() => return,
                    result = tokio::time::timeout(Duration::from_secs(30), self.refresh()) => result,
                };
                if !matches!(refreshed, Ok(Ok(()))) {
                    let mut cache = self.inner.data.cache.lock().unwrap();
                    cache.overlay = None;
                    cache.dirty = true;
                    retry_at = self.inner.clock.now_ms().saturating_add(5_000);
                } else {
                    retry_at = 0;
                }
            }
            tokio::select! {
                _ = stop.changed() => return,
                _ = closing.changed() => return,
                _ = self.inner.data.wake.notified() => {},
                _ = self.inner.clock.sleep("codex_cache", Duration::from_secs(1)) => {},
            }
        }
    }
    #[cfg(unix)]
    async fn refresh(&self) -> Result<(), String> {
        let Some(connection) = self.ensure(true).await? else {
            return Ok(());
        };
        let now = self.inner.clock.now_ms();
        let full = {
            let mut cache = self.inner.data.cache.lock().unwrap();
            cache.overlay = Some(HashMap::new());
            cache.dirty = false;
            cache
                .full_at
                .is_none_or(|at| now.saturating_sub(at) >= FULL_REFRESH)
        };
        let rows = connection.threads(full).await?;
        if !self.current(&connection).await {
            return Err("codex connection changed during list".into());
        }
        let mut cache = self.inner.data.cache.lock().unwrap();
        if full {
            cache.rows = rows;
        } else {
            merge(&mut cache.rows, rows);
        }
        for (thread, patch) in cache.overlay.take().unwrap_or_default() {
            patch.apply(&mut cache.rows, &thread);
        }
        cache.at = Some(self.inner.clock.now_ms());
        if full {
            cache.full_at = cache.at;
        }
        Ok(())
    }
}

fn validate_thread(thread: &str) -> Result<(), String> {
    if thread.is_empty() || thread.starts_with("codex/") {
        Err("invalid native Codex thread id".into())
    } else {
        Ok(())
    }
}
fn busy(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "inprogress" | "in_progress" | "working" | "busy" | "active" | "streaming" | "running"
    )
}
fn parse_status(status: &Value) -> String {
    if let Some(status) = status.as_str().filter(|s| !s.is_empty()) {
        return status.into();
    }
    match status.get("type").and_then(Value::as_str) {
        Some("notLoaded") => "offline".into(),
        Some(kind) => kind.to_ascii_lowercase(),
        None => "unknown".into(),
    }
}
fn normalize_idle(status: &str) -> Result<&'static str, String> {
    match status.to_ascii_lowercase().as_str() {
        "idle" => Ok("idle"),
        "waiting" | "waitingonapproval" | "waitingonuserinput" => Ok("waiting"),
        "offline" | "notloaded" => Ok("notLoaded"),
        _ if busy(status) => Ok("active"),
        _ => Err(format!(
            "codex host state {status:?} is not an idle observation"
        )),
    }
}
fn thread_id(params: &Value) -> &str {
    let id = str_field(params, "threadId");
    if id.is_empty() {
        str_field(&params["thread"], "id")
    } else {
        id
    }
}
fn raw_id(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => Some(value.to_string()),
    }
}
fn row(thread: &Value) -> Result<Value, String> {
    let id = str_field(thread, "id");
    if id.is_empty() {
        return Err("Codex thread row missing id".into());
    }
    Ok(
        json!({"id":format!("codex/{id}"),"name":thread["name"].as_str().unwrap_or(""),
        "harness":"codex","status":parse_status(&thread["status"]),"cwd":thread["cwd"].as_str().unwrap_or(""),
        "updated_at":thread["updatedAt"].as_i64().unwrap_or(0).saturating_mul(1000),"notify_idle":true}),
    )
}
fn merge(rows: &mut Vec<Value>, fresh: Vec<Value>) {
    for row in fresh {
        if let Some(old) = rows.iter_mut().find(|old| old["id"] == row["id"]) {
            *old = row;
        } else {
            rows.push(row);
        }
    }
}
impl Patch {
    fn apply(&self, rows: &mut Vec<Value>, thread: &str) -> bool {
        let id = format!("codex/{thread}");
        if self.closed {
            rows.retain(|row| row["id"] != id);
            return true;
        }
        let Some(row) = rows.iter_mut().find(|row| row["id"] == id) else {
            return false;
        };
        if let Some(status) = &self.status {
            row["status"] = json!(status);
        }
        if let Some(name) = &self.name {
            row["name"] = json!(name);
        }
        if let Some(at) = self.at {
            row["updated_at"] = json!(at);
        }
        true
    }
}
fn notify(
    data: &Data,
    host: &mut HostState,
    method: &str,
    params: &Value,
    now: u64,
) -> Vec<String> {
    let thread = thread_id(params);
    match method {
        "serverRequest/resolved" => {
            if let Some(id) = raw_id(&params["requestId"]) {
                host.needs_user.remove(&id);
            }
        }
        "turn/completed" => {
            let turn = params["turnId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| str_field(&params["turn"], "id"));
            return host
                .tracker
                .complete(thread, turn, str_field(&params["turn"], "status"), now);
        }
        "thread/closed" => {
            host.needs_user.retain(|_, id| id != thread);
            host.tracker.drop_thread(thread);
        }
        _ => {}
    }
    if thread.is_empty() {
        return Vec::new();
    }
    let mut cache = data.cache.lock().unwrap();
    let patch = match method {
        "thread/status/changed" => Patch {
            status: params
                .get("status")
                .filter(|v| !v.is_null())
                .or_else(|| params["thread"].get("status"))
                .map(parse_status),
            at: Some(now),
            ..Patch::default()
        },
        "thread/name/updated" => Patch {
            name: params["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| params["thread"]["name"].as_str().filter(|s| !s.is_empty()))
                .map(String::from),
            ..Patch::default()
        },
        "thread/closed" => Patch {
            closed: true,
            ..Patch::default()
        },
        "thread/started" => {
            cache.dirty = true;
            data.wake.notify_one();
            return Vec::new();
        }
        _ => return Vec::new(),
    };
    if !patch.apply(&mut cache.rows, thread) {
        cache.dirty = true;
        data.wake.notify_one();
    }
    if let Some(overlay) = cache.overlay.as_mut() {
        let old = overlay.entry(thread.into()).or_default();
        if patch.closed {
            *old = patch;
        } else {
            if patch.name.is_some() {
                old.name = patch.name;
            }
            if patch.status.is_some() {
                old.status = patch.status;
                old.at = patch.at;
            }
        }
    }
    Vec::new()
}

impl Connection {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_fenced(method, params, None).await
    }
    async fn call_fenced(
        &self,
        method: &str,
        params: Value,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<Value, RpcError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(RpcError::transport("codex app-server connection closed"));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, receive) = oneshot::channel();
        let deadline = Instant::now() + CALL_TIMEOUT;
        let _cancel = Cancel {
            tx: self.tx.clone(),
            id,
        };
        tokio::time::timeout_at(deadline, async {
            self.tx
                .send(Wire::Call {
                    id,
                    method: method.into(),
                    params,
                    fence,
                    pending: Pending {
                        method: method.into(),
                        deadline,
                        reply,
                    },
                })
                .await
                .map_err(|_| RpcError::transport("codex app-server connection closed"))?;
            receive
                .await
                .map_err(|_| RpcError::transport("codex app-server connection closed"))?
        })
        .await
        .map_err(|_| RpcError::transport(format!("rpc {method}: timeout")))?
    }
    async fn loaded(&self, limit: Option<usize>) -> Result<Vec<String>, RpcError> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut cursors = HashSet::new();
        let mut cursor = String::new();
        loop {
            let mut params = json!({});
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            if !cursor.is_empty() {
                params["cursor"] = json!(cursor);
            }
            let page = self.call("thread/loaded/list", params).await?;
            let values = page["data"]
                .as_array()
                .ok_or_else(|| RpcError::transport("malformed thread/loaded/list page"))?;
            for value in values {
                let id = value
                    .as_str()
                    .ok_or_else(|| RpcError::transport("malformed loaded thread id"))?;
                if seen.insert(id.to_string()) {
                    result.push(id.to_string());
                }
            }
            cursor = next_cursor(&page).map_err(RpcError::transport)?;
            if cursor.is_empty() {
                return Ok(result);
            }
            if !cursors.insert(cursor.clone()) {
                return Err(RpcError::transport("thread/loaded/list cursor loop"));
            }
        }
    }
    async fn route(&self, thread: &str) -> Result<(String, String), String> {
        let response = self
            .call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":true}),
            )
            .await
            .map_err(|e| e.message)?;
        route_state(&response, thread)
    }
    async fn threads(&self, full: bool) -> Result<Vec<Value>, String> {
        let mut result = Vec::new();
        let mut ids = HashSet::new();
        let mut cursors = HashSet::new();
        let mut cursor = String::new();
        loop {
            let mut params =
                json!({"limit":if full {50}else{100},"archived":false,"sortKey":"recency_at"});
            if !cursor.is_empty() {
                params["cursor"] = json!(cursor);
            }
            let page = self
                .call("thread/list", params)
                .await
                .map_err(|e| e.message)?;
            for thread in page["data"]
                .as_array()
                .ok_or("malformed thread/list page")?
            {
                let row = row(thread)?;
                if ids.insert(str_field(&row, "id").to_string()) {
                    result.push(row);
                }
            }
            if !full {
                return Ok(result);
            }
            cursor = next_cursor(&page)?;
            if cursor.is_empty() {
                break;
            }
            if !cursors.insert(cursor.clone()) {
                return Err("thread/list cursor loop".into());
            }
        }
        // Older hosts may lack loaded/list; a successful stored catalog remains
        // useful, but never establishes ownership for send/idle observation.
        if let Ok(loaded) = self.loaded(Some(50)).await {
            for id in loaded {
                if ids.contains(&format!("codex/{id}")) {
                    continue;
                }
                let Ok(response) = self.call("thread/read", json!({"threadId":id})).await else {
                    continue;
                };
                if str_field(&response["thread"], "id") != id {
                    continue;
                }
                if let Ok(row) = row(&response["thread"]) {
                    if ids.insert(str_field(&row, "id").to_string()) {
                        result.push(row);
                    }
                }
            }
        }
        Ok(result)
    }
}
fn next_cursor(page: &Value) -> Result<String, String> {
    match page.get("nextCursor") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(cursor)) => Ok(cursor.clone()),
        _ => Err("malformed Codex pagination cursor".into()),
    }
}
fn route_state(response: &Value, thread: &str) -> Result<(String, String), String> {
    let row = &response["thread"];
    if str_field(row, "id") != thread {
        return Err(format!(
            "thread {thread} missing or mismatched in host reply"
        ));
    }
    let status = parse_status(&row["status"]);
    if !busy(&status) {
        return Ok((status, String::new()));
    }
    let turn = row["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|turn| !str_field(turn, "id").is_empty() && busy(&parse_status(&turn["status"])))
        .ok_or_else(|| format!("active thread {thread} has no active turn id"))?;
    Ok((status, str_field(turn, "id").into()))
}

#[cfg(unix)]
fn control_socket() -> PathBuf {
    ["CODEX_APP_SERVER_SOCK", "CODEX_APP_SERVER_SOCKET"]
        .into_iter()
        .find_map(|key| {
            std::env::var_os(key).filter(|value| !value.to_string_lossy().trim().is_empty())
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| super::super::config::home().join(".codex"))
                .join("app-server-control/app-server-control.sock")
        })
}
#[cfg(unix)]
fn fingerprint(path: &std::path::Path) -> Option<(u64, u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino(), m.mtime(), m.mtime_nsec()))
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn run_connection(
    mut ws: super::CodexWs,
    mut commands: mpsc::Receiver<Wire>,
    state: Arc<Mutex<HostState>>,
    alive: Arc<AtomicBool>,
    generation: u64,
    data: Arc<Data>,
    owner: Weak<super::super::Runtime>,
    clock: crate::clock::Clock,
    mut stop: watch::Receiver<bool>,
    mut closing: watch::Receiver<bool>,
) {
    let _alive = Alive(alive);
    let mut pending = HashMap::<u64, Pending>::new();
    let mut expiry = tokio::time::interval(Duration::from_millis(100));
    let mut receipts = tokio::task::JoinSet::new();
    loop {
        if *stop.borrow() || *closing.borrow() {
            break;
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = closing.changed() => break,
            _ = receipts.join_next(), if !receipts.is_empty() => {},
            _ = expiry.tick() => {
                pending.retain(|_, request| !request.reply.is_closed() && request.deadline > Instant::now());
                state.lock().unwrap().tracker.prune(clock.now_ms());
            },
            command = commands.recv() => match command {
                Some(Wire::Call { id, method, params, pending: request,fence }) => {
                    if request.reply.is_closed() || request.deadline <= Instant::now() { continue; }
                    if pending.len() >= 512 {
                        let _ = request.reply.send(Err(RpcError::transport("codex pending request limit reached")));
                        continue;
                    }
                    let value = json!({"id":id,"method":method,"params":params});
                    let deadline = request.deadline;
                    pending.insert(id, request);
                    let written = tokio::time::timeout_at(deadline, ws.send(Message::Text(value.to_string().into())));
                    tokio::select! {
                        _ = stop.changed() => break,
                        _ = closing.changed() => break,
                        result = written => if !matches!(result, Ok(Ok(()))) { break; },
                    }
                    drop(fence);
                },
                Some(Wire::Notify { method }) => {
                    if !matches!(tokio::time::timeout(CALL_TIMEOUT, ws.send(Message::Text(json!({"method":method}).to_string().into()))).await, Ok(Ok(()))) { break; }
                },
                Some(Wire::Cancel(id)) => { pending.remove(&id); },
                None => break,
            },
            frame = ws.next() => {
                let Some(Ok(frame)) = frame else { break; };
                let value = match frame {
                    Message::Text(text) => match serde_json::from_str::<Value>(&text) { Ok(value) => value, Err(_) => continue },
                    Message::Binary(bytes) => match serde_json::from_slice::<Value>(&bytes) { Ok(value) => value, Err(_) => continue },
                    Message::Ping(payload) => {
                        if !matches!(tokio::time::timeout(CALL_TIMEOUT, ws.send(Message::Pong(payload))).await, Ok(Ok(()))) { break; }
                        continue;
                    },
                    Message::Close(_) => break,
                    _ => continue,
                };
                let method = str_field(&value, "method");
                if !method.is_empty() {
                    if data.generation.load(Ordering::Acquire) != generation { continue; }
                    let params = &value["params"];
                    if !value["id"].is_null() {
                        let thread = thread_id(params);
                        let id = raw_id(&params["requestId"]).or_else(|| raw_id(&value["id"])).unwrap_or_else(|| thread.into());
                        state.lock().unwrap().needs_user.insert(id, thread.into());
                        // A passive observer never resolves approvals or input
                        // requests, and a method+id frame is NOT a response.
                        continue;
                    }
                    let messages = notify(&data, &mut state.lock().unwrap(), method, params, clock.now_ms());
                    if !messages.is_empty() {
                        let owner = owner.clone();
                        let thread = thread_id(params).to_string();
                        receipts.spawn(async move {
                            if let Some(rt) = owner.upgrade() {
                                for mid in messages { rt.local_receipt(&mid, &format!("codex/{thread}"), "read").await; }
                            }
                        });
                    }
                } else if let Some(id) = value["id"].as_u64() {
                    if let Some(request) = pending.remove(&id) {
                        let result = if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
                            Err(RpcError { code: error["code"].as_i64(), message: format!("rpc {}: {}", request.method, str_field(error,"message")) })
                        } else { Ok(value["result"].clone()) };
                        let _ = request.reply.send(result);
                    }
                }
            },
        }
    }
    receipts.abort_all();
    while receipts.join_next().await.is_some() {}
    data.wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct Fixture {
        client: Client,
        host: tokio::task::JoinHandle<()>,
        path: PathBuf,
        _stop: watch::Sender<bool>,
        _closing: watch::Sender<bool>,
    }
    #[cfg(unix)]
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.host.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }
    #[cfg(unix)]
    async fn fixture(mut handler: impl FnMut(&Value) -> Vec<Value> + Send + 'static) -> Fixture {
        let path = PathBuf::from(format!("/tmp/pi-codex-{}.sock", uuid::Uuid::new_v4()));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let replies = if req["method"] == "initialize" {
                    assert_eq!(
                        req["params"]["capabilities"]["optOutNotificationMethods"],
                        json!(OPT_OUT)
                    );
                    vec![json!({"id":req["id"],"result":{}})]
                } else if req["method"] == "initialized" {
                    Vec::new()
                } else {
                    handler(&req)
                };
                if replies.last().is_some_and(Value::is_null) {
                    for reply in replies.into_iter().filter(|reply| !reply.is_null()) {
                        ws.feed(Message::Text(reply.to_string().into()))
                            .await
                            .unwrap();
                    }
                    ws.feed(Message::Close(None)).await.unwrap();
                    let _ = ws.flush().await;
                    return;
                }
                for reply in replies {
                    if ws
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
        let (stop, stop_rx) = watch::channel(false);
        let (closing, closing_rx) = watch::channel(false);
        let client = Client {
            inner: Arc::new(Inner {
                connection: tokio::sync::Mutex::new(None),
                data: Arc::new(Data::default()),
                owner: Weak::new(),
                clock: crate::clock::Clock::default(),
                stop: stop_rx,
                closing: closing_rx,
                probe: Mutex::new(Probe::default()),
            }),
        };
        let connection = client.open(&path).await.unwrap();
        *client.inner.connection.lock().await = Some(connection);
        Fixture {
            client,
            host,
            path,
            _stop: stop,
            _closing: closing,
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn preturn_ownership_and_status_failures_keep_the_physical_fence_and_exact_phase() {
        for ownership_error in [true, false] {
            let fixture = fixture(move |request| {
                let result = match str_field(request, "method") {
                    "thread/loaded/list" if ownership_error => return vec![json!({"id":request["id"],"error":{"code":-32601,"message":"loaded IDs unsupported"}})],
                    "thread/loaded/list" => json!({"data":["t"]}),
                    "thread/read" => json!({"thread":{"id":"different-thread","status":"idle"}}),
                    method => panic!("preflight failure wrote a turn: {method}"),
                };
                vec![json!({"id":request["id"],"result":result})]
            }).await;
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let mut fence = Some(lock.clone().lock_owned().await);
            let error = fixture
                .client
                .send("t", "body".into(), "message", &mut fence)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    (&error, ownership_error),
                    (HostError::Ownership(_), true) | (HostError::Operation(_), false)
                ),
                "{error:?}"
            );
            assert!(fence.is_some());
            assert!(lock.try_lock().is_err());
            drop(fence);
            assert!(lock.try_lock().is_ok());
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_connection_full_pages_loaded_union_then_incremental_cache() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let host_log = log.clone();
        let fixture = fixture(move |req| {
            host_log.lock().unwrap().push(req.clone());
            let params = &req["params"];
            let result = match str_field(req, "method") {
                "thread/list" if params["limit"] == 100 => json!({"data":[{"id":"a","name":"renamed","status":"idle"}],"nextCursor":"ignored-incremental"}),
                "thread/list" if params["cursor"] == "second" => json!({"data":[{"id":"older","status":"idle"},{"id":"a"}]}),
                "thread/list" => json!({"data":[{"id":"a","status":"idle"}],"nextCursor":"second"}),
                "thread/loaded/list" if params["cursor"] == "loaded-second" => json!({"data":["fresh"]}),
                "thread/loaded/list" => json!({"data":["a"],"nextCursor":"loaded-second"}),
                "thread/read" => json!({"thread":{"id":params["threadId"],"name":"fresh-name","status":"idle"}}),
                "thread/name/set" => json!({}),
                method => panic!("unexpected method {method}"),
            };
            vec![json!({"id":req["id"],"result":result})]
        }).await;
        fixture.client.refresh().await.unwrap();
        assert_eq!(fixture.client.list().len(), 3);
        fixture.client.refresh().await.unwrap();
        let rows = fixture.client.list();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().find(|r| r["id"] == "codex/a").unwrap()["name"],
            "renamed"
        );
        fixture
            .client
            .rename("older", "history-name")
            .await
            .unwrap();
        let log = log.lock().unwrap();
        assert!(log.iter().any(|r| r["params"]["cursor"] == "second"));
        assert!(log.iter().any(|r| r["params"]["cursor"] == "loaded-second"));
        assert_eq!(
            log.iter().filter(|r| r["method"] == "thread/list").count(),
            3
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_server_request_with_same_id_is_not_a_response_and_idle_is_fresh() {
        let fixture = fixture(|req| {
            let result = match str_field(req, "method") {
                "thread/loaded/list" => return vec![
                    json!({"id":req["id"],"method":"item/commandExecution/requestApproval","params":{"threadId":"t","requestId":"approval"}}),
                    json!({"id":req["id"],"result":{"data":["t"]}}),
                ],
                "thread/read" => json!({"thread":{"id":"t","status":"active","turns":[{"id":"active","status":"inProgress"}]}}),
                "resolve" => return vec![json!({"method":"serverRequest/resolved","params":{"requestId":"approval"}}),json!({"id":req["id"],"result":{}})],
                method => panic!("unexpected method {method}"),
            };
            vec![json!({"id":req["id"],"result":result})]
        }).await;
        fixture.client.inner.data.cache.lock().unwrap().rows =
            vec![row(&json!({"id":"t","status":"idle"})).unwrap()];
        assert_eq!(
            fixture.client.idle_state("t").await.unwrap().as_deref(),
            Some("waiting")
        );
        let connection = fixture.client.ensure(false).await.unwrap().unwrap();
        connection.call("resolve", json!({})).await.unwrap();
        let (status, turn) = connection.route("t").await.unwrap();
        assert_eq!((status.as_str(), turn.as_str()), ("active", "active"));
        assert!(!connection
            .state
            .lock()
            .unwrap()
            .needs_user
            .values()
            .any(|id| id == "t"));
        assert_eq!(fixture.client.list()[0]["status"], "idle");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn busy_send_steers_exact_turn_and_correlates_early_completed_only() {
        for status in ["completed", "failed", "interrupted", ""] {
            let fixture = fixture(move |req| {
                let result = match str_field(req, "method") {
                    "thread/loaded/list" => json!({"data":["t"]}),
                    "thread/read" => json!({"thread":{"id":"t","status":"active","turns":[{"id":"old","status":"inProgress"},{"id":"new","status":"running"}]}}),
                    "turn/steer" => {
                        assert_eq!(req["params"]["expectedTurnId"], "new");
                        return vec![json!({"method":"turn/completed","params":{"threadId":"t","turn":{"id":"new","status":status}}}),
                            json!({"id":req["id"],"result":{"turnId":"new"}})];
                    },
                    method => panic!("unexpected mutation {method}"),
                };
                vec![json!({"id":req["id"],"result":result})]
            }).await;
            assert_eq!(
                fixture
                    .client
                    .send("t", "peer-body".into(), "m", &mut None)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(if status == "completed" {
                    "read"
                } else {
                    "delivered"
                })
            );
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn internal_send_failure_is_unknown_never_not_owned_fallback() {
        let writes = Arc::new(AtomicU64::new(0));
        let host_writes = writes.clone();
        let fixture = fixture(move |req| {
            let result = match str_field(req, "method") {
                "thread/loaded/list" => json!({"data":["t"]}),
                "thread/read" => json!({"thread":{"id":"t","status":"idle"}}),
                "turn/start" => {
                    host_writes.fetch_add(1, Ordering::Relaxed);
                    return vec![json!({"id":req["id"],"error":{"code":-32603,"message":"internal failure"}})];
                },
                method => panic!("unexpected method {method}"),
            };
            vec![json!({"id":req["id"],"result":result})]
        }).await;
        let error = fixture
            .client
            .send("t", "body".into(), "m", &mut None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outcome unknown"), "{error}");
        assert_eq!(writes.load(Ordering::Relaxed), 1);
        assert_eq!(
            fixture
                .client
                .send("not-owned", "body".into(), "other", &mut None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(writes.load(Ordering::Relaxed), 1);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn authoritative_turn_ack_survives_immediate_host_close() {
        let fixture = fixture(|req| {
            let result = match str_field(req, "method") {
                "thread/loaded/list" => json!({"data":["t"]}),
                "thread/read" => json!({"thread":{"id":"t","status":"idle"}}),
                "turn/start" => {
                    return vec![
                        json!({"id":req["id"],"result":{"turn":{"id":"turn"}}}),
                        Value::Null,
                    ]
                }
                method => panic!("unexpected method {method}"),
            };
            vec![json!({"id":req["id"],"result":result})]
        })
        .await;
        assert_eq!(
            fixture
                .client
                .send("t", "body".into(), "m", &mut None)
                .await
                .unwrap()
                .as_deref(),
            Some("delivered")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn thread_started_during_refresh_keeps_cache_invalidated() {
        let fixture = fixture(|req| {
            if req["method"] == "thread/list" {
                vec![
                    json!({"method":"thread/started","params":{"thread":{"id":"new"}}}),
                    json!({"id":req["id"],"result":{"data":[]}}),
                ]
            } else {
                vec![json!({"id":req["id"],"result":{"data":[]}})]
            }
        })
        .await;
        fixture.client.refresh().await.unwrap();
        assert!(fixture.client.inner.data.cache.lock().unwrap().dirty);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_rpc_does_not_interrupt_sibling_or_close_connection() {
        let fixture = fixture(|req| {
            if req["method"] == "stall" {
                vec![]
            } else {
                vec![json!({"id":req["id"],"result":{"ok":true}})]
            }
        })
        .await;
        let connection = fixture.client.ensure(false).await.unwrap().unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            connection.call("stall", json!({}))
        )
        .await
        .is_err());
        assert_eq!(
            connection.call("sibling", json!({})).await.unwrap()["ok"],
            true
        );
        assert!(connection.alive.load(Ordering::Acquire));
        fixture._closing.send(true).unwrap();
        assert!(connection.call("after-close", json!({})).await.is_err());
    }

    #[test]
    fn parse_rows_use_assigned_names_and_millisecond_activity() {
        let row = row(&json!({"id":"t","preview":"not a display name","updatedAt":42,"status":{"type":"notLoaded"}})).unwrap();
        assert_eq!(row["name"], "");
        assert_eq!(row["updated_at"], 42000);
        assert_eq!(row["status"], "offline");
        assert_eq!(row["notify_idle"], true);
    }
    #[test]
    fn route_requires_exact_identity_and_latest_active_turn() {
        let value = json!({"thread":{"id":"t","status":{"type":"active"},"turns":[
            {"id":"old","status":"inProgress"},{"id":"finished","status":"completed"},{"id":"new","status":"running"}
        ]}});
        assert_eq!(
            route_state(&value, "t").unwrap(),
            ("active".into(), "new".into())
        );
        assert!(route_state(&value, "other").is_err());
        assert!(route_state(
            &json!({"thread":{"id":"t","status":"active","turns":[]}}),
            "t"
        )
        .is_err());
    }
    #[test]
    fn idle_states_never_infer_exit_from_missing_or_unknown() {
        assert_eq!(normalize_idle("idle").unwrap(), "idle");
        assert_eq!(normalize_idle("offline").unwrap(), "notLoaded");
        assert_eq!(normalize_idle("waitingOnApproval").unwrap(), "waiting");
        for status in [
            "active",
            "running",
            "inProgress",
            "in_progress",
            "streaming",
        ] {
            assert_eq!(normalize_idle(status).unwrap(), "active");
        }
        for status in ["", "unknown", "systemError", "exited"] {
            assert!(normalize_idle(status).is_err());
        }
    }
    #[test]
    fn receipt_tracker_handles_early_completion_and_duplicate_events() {
        let mut tracker = Tracker::default();
        assert!(tracker.complete("t", "turn", "completed", 0).is_empty());
        assert!(tracker.remember("t", "turn", "m1", 1));
        assert!(!tracker.remember("t", "turn", "m1", 2));
        assert!(tracker.remember("t", "turn", "m2", 2));
        assert!(tracker.complete("t", "turn", "completed", 3).is_empty());
    }
    #[test]
    fn receipt_tracker_requires_exact_thread_turn_and_completed_status() {
        let mut tracker = Tracker::default();
        tracker.remember("t", "turn", "m", 0);
        for status in ["", "failed", "interrupted", "inProgress"] {
            assert!(tracker.complete("t", "turn", status, 1).is_empty());
        }
        assert!(tracker.complete("other", "turn", "completed", 1).is_empty());
        assert!(tracker.complete("t", "other", "completed", 1).is_empty());
        assert_eq!(tracker.complete("t", "turn", "completed", 2), vec!["m"]);
        let mut next_connection = Tracker::default();
        assert!(next_connection
            .complete("t", "turn", "completed", 3)
            .is_empty());
    }
    #[test]
    fn receipt_tracker_bounds_memory_and_drops_closed_threads() {
        let mut tracker = Tracker::default();
        tracker.remember("t", "turn", "m", 0);
        assert!(tracker
            .complete("t", "turn", "completed", RECEIPT_TTL + 1)
            .is_empty());
        tracker.drop_thread("t");
        assert!(!tracker.remember("t", "turn", "new", RECEIPT_TTL + 2));
    }
    #[test]
    fn cache_notification_patch_survives_in_flight_refresh() {
        let data = Data::default();
        data.cache.lock().unwrap().rows =
            vec![row(&json!({"id":"t","name":"old","status":"idle"})).unwrap()];
        data.cache.lock().unwrap().overlay = Some(HashMap::new());
        let mut host = HostState::default();
        notify(
            &data,
            &mut host,
            "thread/status/changed",
            &json!({"threadId":"t","status":{"type":"active"}}),
            100,
        );
        notify(
            &data,
            &mut host,
            "thread/name/updated",
            &json!({"thread":{"id":"t","name":"new"}}),
            101,
        );
        let mut cache = data.cache.lock().unwrap();
        cache.rows = vec![row(&json!({"id":"t","name":"stale","status":"idle"})).unwrap()];
        for (thread, patch) in cache.overlay.take().unwrap() {
            patch.apply(&mut cache.rows, &thread);
        }
        assert_eq!(cache.rows[0]["name"], "new");
        assert_eq!(cache.rows[0]["status"], "active");
        assert_eq!(cache.rows[0]["updated_at"], 100);
    }
    #[test]
    fn closed_notification_clears_approvals_cache_and_correlation() {
        let data = Data::default();
        data.cache.lock().unwrap().rows = vec![row(&json!({"id":"t"})).unwrap()];
        let mut host = HostState::default();
        host.needs_user.insert("request".into(), "t".into());
        host.tracker.remember("t", "turn", "m", 0);
        notify(
            &data,
            &mut host,
            "thread/closed",
            &json!({"threadId":"t"}),
            1,
        );
        assert!(host.needs_user.is_empty());
        assert!(data.cache.lock().unwrap().rows.is_empty());
        assert!(host
            .tracker
            .complete("t", "turn", "completed", 2)
            .is_empty());
    }
    #[test]
    fn probe_quiet_period_expires_and_metadata_changes_reset_failures() {
        let mut probe = Probe::default();
        let first = (1, 2, 3, 4);
        for _ in 0..3 {
            probe.failed(first, 0);
        }
        assert!(probe.quiet(first, 59_999));
        assert!(!probe.quiet(first, 60_000));
        assert_eq!(probe.retry_at, 60_000);
        assert!(!probe.quiet((1, 2, 4, 0), 1));
        probe.failed((1, 2, 4, 0), 1);
        assert_eq!(probe.failures, 1);
    }
    #[test]
    fn incremental_merge_keeps_older_pages_and_replaces_changed_rows() {
        let mut rows = vec![json!({"id":"first","name":"old"}), json!({"id":"older"})];
        merge(
            &mut rows,
            vec![json!({"id":"first","name":"new"}), json!({"id":"recent"})],
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["name"], "new");
        assert_eq!(rows[1]["id"], "older");
    }
}
