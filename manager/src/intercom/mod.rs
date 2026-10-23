// `configure` refuses on Windows, so the Unix-socket native adapters below are
// compiled there but unreachable.
#![cfg_attr(
    windows,
    allow(dead_code, unused_imports, unused_mut, unused_variables)
)]
pub mod cli;
pub mod config;
mod deliveries;
mod herdr;
mod http;
mod idle;
mod localrpc;
pub(crate) mod native;
mod relay;
mod service;

use config::{Config, Identity};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

static CONFIG: OnceLock<Config> = OnceLock::new();
/// Bounds alias retirement and publication cleanup so a wedged native peer
/// cannot hold daemon exit or an upgrade.
const NATIVE_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
pub fn configure(c: Config) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = c;
        Err("intercom native adapters support Linux/macOS only".into())
    }
    #[cfg(not(windows))]
    CONFIG
        .set(c)
        .map_err(|_| "intercom already configured".into())
}
pub fn enabled() -> bool {
    CONFIG.get().is_some()
}
pub fn configuration() -> Option<&'static Config> {
    CONFIG.get()
}
pub use deliveries::CarriedInjection;
/// Injection dedup entries carried across an in-place upgrade; the next `start`
/// seeds its dedup table from them so a relay redelivery is not injected twice.
static CARRIED_DEDUP: Mutex<Vec<CarriedInjection>> = Mutex::new(Vec::new());
pub fn carry_dedup(entries: Vec<CarriedInjection>) {
    *CARRIED_DEDUP.lock().unwrap() = entries;
}
pub fn carried_dedup() -> Vec<CarriedInjection> {
    CARRIED_DEDUP.lock().unwrap().clone()
}
pub type Shared = Arc<Runtime>;
pub struct Push {
    pub value: Value,
    pub written: Option<oneshot::Sender<Result<(), String>>>,
    pub fence: Option<tokio::sync::OwnedMutexGuard<()>>,
}
impl From<Value> for Push {
    fn from(value: Value) -> Self {
        Self {
            value,
            written: None,
            fence: None,
        }
    }
}
pub struct Session {
    pub value: Value,
    pub owner: u64,
    pub epoch: u64,
    pub tx: mpsc::Sender<Push>,
    pub disconnect: Option<watch::Sender<bool>>,
    pub caps: HashSet<String>,
    pub idle_since: Option<u64>,
}
#[derive(Clone)]
pub struct Delivery {
    pub token: Arc<()>,
    pub origin: String,
    pub target: String,
    pub native_id: String,
    pub status: String,
    pub updated_at: u64,
    pub lease_at: u64,
    pub approval_pin: bool,
    pub retry_at: u64,
    pub retry_delay: u64,
    pub forwarding: bool,
    pub drop_reason: String,
    pub reply_relay: bool,
    pub remote_native: bool,
    pub target_endpoint: Option<Value>,
    pub origin_endpoint: Option<Value>,
    pub reply_endpoint: Option<Value>,
}
pub struct State {
    pub sessions: HashMap<String, Session>,
    pub unpublished: HashMap<String, (Value, bool)>,
    pub exited: HashSet<String>,
    pub messages: HashMap<String, Delivery>,
    pub history: HashSet<String>,
    pub pending_local: HashSet<String>,
    /// Herdr workspaces a relay peer created; the only ones it may close or extend.
    pub relay_workspaces: HashSet<String>,
    pub pending_remote: HashSet<String>,
    pub incoming: deliveries::InjectionDedup,
    pub subscriptions: HashMap<String, Idle>,
    pub idle_stable: HashMap<String, u64>,
    pub next_epoch: u64,
    pub next_conn: u64,
    pub events: VecDeque<Value>,
    pub seq: u64,
    pub remote: Vec<Value>,
    pub relay_tx: Option<mpsc::Sender<Push>>,
    pub control_outbox: VecDeque<Value>,
    pub relay_message_ids: bool,
    pub relay_generation: u64,
    pub legacy_active: Option<relay::LegacyActive>,
    pub pending: HashMap<String, oneshot::Sender<Value>>,
    pub aliases: HashMap<String, String>,
    pub mcp_status: String,
    pub native_status: String,
}
#[derive(Clone)]
pub struct Idle {
    pub watcher: String,
    pub target: String,
    pub mode: String,
    pub status: String,
    pub updated_at: u64,
    pub created: u64,
    pub epoch: Option<u64>,
    pub backend: String,
    pub remote_origin: String,
    pub remote_machine: String,
    pub native: bool,
    pub source_identity: String,
    pub admitting: bool,
    pub superseded: bool,
}
pub struct Runtime {
    pub cfg: Config,
    pub clock: crate::clock::Clock,
    pub identity: Identity,
    pub machine: String,
    pub state: Mutex<State>,
    pub stop: watch::Sender<bool>,
    pub closing: watch::Sender<bool>,
    pub rpc_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub native: native::Native,
    pub legacy_gate: tokio::sync::Mutex<()>,
    pub tool_status: tokio::sync::OnceCell<Value>,
    pub mcp_url: Mutex<String>,
}
pub struct Running {
    pub runtime: Shared,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    #[cfg(unix)]
    socket_identity: (u64, u64),
}
impl Running {
    /// Shuts down, then returns the injection dedup table once in-flight
    /// injections have recorded their outcomes, for an upgrade to carry.
    pub async fn shutdown_for_handover(self) -> Vec<CarriedInjection> {
        let runtime = self.runtime.clone();
        self.shutdown().await;
        let settled = tokio::time::timeout(Duration::from_secs(2), async {
            while runtime.state.lock().unwrap().incoming.injecting() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if settled.is_err() {
            eprintln!("intercom: in-flight injections outlived shutdown; carrying without them");
        }
        let carried = runtime.state.lock().unwrap().incoming.carried();
        carried
    }
    pub async fn shutdown(self) {
        self.runtime.drain().await;
        self.runtime.stop.send_replace(true);
        let connections = std::mem::take(&mut *self.runtime.rpc_tasks.lock().unwrap());
        for connection in &connections {
            connection.abort();
        }
        for connection in connections {
            let _ = connection.await;
        }
        if tokio::time::timeout(NATIVE_CLOSE_TIMEOUT, self.runtime.native.close())
            .await
            .is_err()
        {
            eprintln!("intercom native: close timed out");
        }
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(m) = std::fs::symlink_metadata(&self.runtime.cfg.sock) {
                if (m.dev(), m.ino()) == self.socket_identity {
                    let _ = std::fs::remove_file(&self.runtime.cfg.sock);
                }
            }
        }
    }
}
impl Runtime {
    /// Stop admitting work and settle idle watches as `unavailable` while
    /// the native bridges and the watching processes are still alive; only
    /// then stop the bridges.
    pub async fn drain(&self) {
        self.closing.send_replace(true);
        self.finish_idle("unavailable").await;
        self.native.stop_accepting();
    }
}
pub async fn start(c: Config, clock: crate::clock::Clock) -> Result<Running, String> {
    c.validate()?;
    if !c.local_only && c.server.is_empty() {
        return Err("server URL is required".into());
    }
    if !c.local_only && c.token.is_empty() && !c.state_dir.join("device.json").exists() {
        return Err(
            "enroll this device or pass a device credential; use --local-only for local messages"
                .into(),
        );
    }
    let identity = Identity::load(&c.state_dir)?;
    let mut cfg = c;
    if !identity.credential.is_empty() {
        cfg.token = identity.credential.clone();
    }
    let mut mcp_token_error = None;
    if !cfg.mcp_listen.is_empty() && cfg.mcp_token.trim().is_empty() {
        match config::generated_mcp_token(&cfg.state_dir) {
            Ok(token) => cfg.mcp_token = token,
            Err(error) => mcp_token_error = Some(format!("MCP token: {error}")),
        }
    }
    if !cfg.local_only && cfg.token.is_empty() {
        return Err(
            "enroll this device or pass a device credential; use --local-only for local messages"
                .into(),
        );
    }
    let host = config::hostname();
    let name = if cfg.name.is_empty() {
        std::env::var("USER").unwrap_or_else(|_| "user".into())
    } else {
        cfg.name.clone()
    };
    let (stop, _) = watch::channel(false);
    let (closing, _) = watch::channel(false);
    let runtime = Arc::new(Runtime {
        cfg,
        clock,
        identity,
        machine: format!("{name}@{host}"),
        stop,
        closing,
        rpc_tasks: Mutex::new(Vec::new()),
        native: native::Native::new(),
        legacy_gate: tokio::sync::Mutex::new(()),
        tool_status: tokio::sync::OnceCell::new(),
        mcp_url: Mutex::new(String::new()),
        state: Mutex::new(State {
            sessions: HashMap::new(),
            unpublished: HashMap::new(),
            exited: HashSet::new(),
            messages: HashMap::new(),
            history: HashSet::new(),
            pending_local: HashSet::new(),
            pending_remote: HashSet::new(),
            relay_workspaces: HashSet::new(),
            incoming: deliveries::InjectionDedup::from_carried(std::mem::take(
                &mut *CARRIED_DEDUP.lock().unwrap(),
            )),
            subscriptions: HashMap::new(),
            idle_stable: HashMap::new(),
            next_epoch: 0,
            next_conn: 0,
            events: VecDeque::new(),
            seq: 0,
            remote: Vec::new(),
            relay_tx: None,
            control_outbox: VecDeque::new(),
            relay_message_ids: false,
            relay_generation: 0,
            legacy_active: None,
            pending: HashMap::new(),
            aliases: HashMap::new(),
            mcp_status: "disabled".into(),
            native_status: "starting".into(),
        }),
    });
    let (listener, socket_identity) = localrpc::bind(&runtime.cfg.sock).await?;
    let rt = runtime.clone();
    let mut tasks = vec![tokio::spawn(async move {
        localrpc::serve(rt, listener).await;
    })];
    let rt = runtime.clone();
    tasks.push(tokio::spawn(async move {
        rt.maintenance().await;
    }));
    let rt = runtime.clone();
    tasks.push(tokio::spawn(async move {
        let mut stop = rt.stop.subscribe();
        let mut closing = rt.closing.subscribe();
        tokio::select! {
            _ = stop.changed() => {},
            _ = closing.changed() => {},
            _ = rt.tool_status.get_or_init(probe_tool_status) => {},
        }
    }));
    let mut running = Running {
        runtime: runtime.clone(),
        tasks,
        #[cfg(unix)]
        socket_identity,
    };
    match native::start(&runtime).await {
        Ok(()) => runtime.state.lock().unwrap().native_status = "running".into(),
        Err(error) => {
            runtime.state.lock().unwrap().native_status = format!("degraded: {error}");
            runtime.record("error", "", "native", &error);
            eprintln!("intercom native: {error}");
        }
    }
    if !runtime.cfg.local_only {
        let rt = runtime.clone();
        running.tasks.push(tokio::spawn(async move {
            relay::run(rt).await;
        }));
    }
    if !runtime.cfg.mcp_listen.is_empty() {
        let started = match mcp_token_error {
            Some(error) => Err(error),
            None => http::start(runtime.clone()).await,
        };
        match started {
            Ok(t) => {
                runtime.state.lock().unwrap().mcp_status = "running".into();
                running.tasks.push(t);
            }
            Err(error) => {
                runtime.state.lock().unwrap().mcp_status = format!("error: {error}");
                runtime.record("error", "", "mcp", &error);
                eprintln!("intercom MCP: {error}");
            }
        }
    }
    Ok(running)
}
impl Runtime {
    fn generated_mcp_token_file(&self) -> String {
        let path = self.cfg.state_dir.join(config::MCP_TOKEN_FILE);
        match std::fs::read_to_string(&path) {
            Ok(token) if token.trim() == self.cfg.mcp_token => path.to_string_lossy().into_owned(),
            _ => String::new(),
        }
    }
    pub fn record(&self, kind: &str, mid: &str, session: &str, status: &str) {
        let mut s = self.state.lock().unwrap();
        s.seq += 1;
        let seq = s.seq;
        s.events.push_back(json!({"seq":seq,"ts":crate::proto::now_ms(),"kind":kind,"message_id":mid,"session_id":session,"status":status}));
        while s.events.len() > self.cfg.event_log_size.max(1) {
            s.events.pop_front();
        }
    }
    pub async fn directory(&self) -> Vec<Value> {
        let (mut rows, remote): (Vec<Value>, Vec<Value>) = {
            let s = self.state.lock().unwrap();
            (
                s.sessions.values().map(|s| s.value.clone()).collect(),
                s.remote.clone(),
            )
        };
        rows.extend(self.native.list().await);
        herdr::overlay(self, &mut rows).await;
        for ep in &self.cfg.endpoints {
            rows.push(json!({"id":format!("webhook/{}",ep.name),"name":ep.name,"harness":"webhook","status":"idle","cwd":""}));
        }
        merge_remote(&mut rows, remote, &self.machine);
        let s = self.state.lock().unwrap();
        for row in &mut rows {
            if let Some(name) = s.aliases.get(str_field(row, "id")) {
                row["name"] = json!(name);
            }
        }
        rows.sort_by(|a, b| str_field(a, "id").cmp(str_field(b, "id")));
        rows.dedup_by(|a, b| a["id"] == b["id"]);
        rows
    }
    pub fn local_agent(&self, row: &Value) -> bool {
        (str_field(row, "machine_id").is_empty() || str_field(row, "machine_id") == self.machine)
            && row["all"] != true
            && ["pi", "claude", "codex"].contains(&str_field(row, "harness"))
    }
    pub async fn resolve(&self, to: &str) -> Result<Value, String> {
        let rows = self.directory().await;
        if let Some(v) = rows.iter().find(|v| str_field(v, "id") == to) {
            return Ok(v.clone());
        }
        let matches: Vec<_> = rows
            .into_iter()
            .filter(|v| {
                str_field(v, "name") == to
                    || str_field(v, "name")
                        .to_lowercase()
                        .contains(&to.to_lowercase())
            })
            .collect();
        match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => Err(format!("unknown session {to:?}")),
            _ => Err(format!("ambiguous session name {to:?}")),
        }
    }
    pub async fn push(&self, target: &str, value: Value) -> Result<(), String> {
        let tx = self
            .state
            .lock()
            .unwrap()
            .sessions
            .get(target)
            .map(|s| s.tx.clone())
            .ok_or_else(|| "session unavailable".to_string())?;
        let (written, rx) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            tx.send(Push {
                value,
                written: Some(written),
                fence: None,
            }),
        )
        .await
        .map_err(|_| "plugin write timeout".to_string())?
        .map_err(|_| "plugin disconnected".to_string())?;
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .map_err(|_| "plugin write outcome unknown".to_string())?
            .map_err(|_| "plugin write outcome unknown".to_string())?
    }
    pub async fn send(
        &self,
        from: &str,
        to: &str,
        body: &str,
        reply: &str,
        mode: &str,
    ) -> Result<Value, String> {
        let target = self.resolve(to).await?;
        let origin = if from.is_empty() { reply } else { from };
        self.send_resolved(
            origin,
            target,
            body,
            mode,
            &uuid::Uuid::new_v4().to_string(),
            None,
            None,
        )
        .await
    }
    pub async fn native_inbound(
        &self,
        source: Value,
        target: Value,
        reply_endpoint: Value,
        native_mid: &str,
        body: &str,
        mode: &str,
    ) -> Result<Value, String> {
        self.native.validate_source(&source)?;
        if native_mid.is_empty() {
            return Err("native message ID is empty".into());
        }
        let origin = str_field(&source, "id");
        let to = str_field(&target, "id").to_string();
        let remote = !str_field(&target, "machine_id").is_empty()
            && str_field(&target, "machine_id") != self.machine;
        let mid = if remote {
            uuid::Uuid::new_v4().to_string()
        } else {
            native_mid.to_string()
        };
        let guard = self.native.route_guard(&reply_endpoint).await?;
        let result = self
            .send_resolved(
                origin,
                target,
                body,
                mode,
                &mid,
                Some((&source, &reply_endpoint, native_mid)),
                Some(guard),
            )
            .await;
        if let Ok(response) = &result {
            if !remote {
                let status = str_field(response, "status");
                if let Err(error) = self
                    .native
                    .receipt(
                        origin,
                        Some(&source),
                        Some(&reply_endpoint),
                        &to,
                        native_mid,
                        status,
                        "",
                    )
                    .await
                {
                    self.record("error", native_mid, &to, &error);
                }
            }
        }
        if let Err(error) = &result {
            let status = if remote
                && (error.contains("refuse")
                    || error.contains("isolatePeerMachines")
                    || error.contains("settings")
                    || error.contains("remote source"))
            {
                "refused"
            } else {
                "dropped"
            };
            if let Err(callback) = self
                .native
                .receipt(
                    origin,
                    Some(&source),
                    Some(&reply_endpoint),
                    &to,
                    native_mid,
                    status,
                    "",
                )
                .await
            {
                self.record("error", native_mid, &to, &callback);
            }
        }
        result
    }
    #[allow(clippy::too_many_arguments)] // Preserve the explicit send admission and write fence.
    async fn send_resolved(
        &self,
        origin: &str,
        target: Value,
        body: &str,
        mode: &str,
        mid: &str,
        native: Option<(&Value, &Value, &str)>,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<Value, String> {
        if *self.closing.borrow() {
            return Err("intercom is shutting down".into());
        }
        check_from_mode(mode)?;
        let to = str_field(&target, "id");
        let remote = !str_field(&target, "machine_id").is_empty()
            && str_field(&target, "machine_id") != self.machine;
        if remote && self.cfg.remote_messages == "refuse" {
            return Err("cross-machine operation refused: remote_messages=refuse".into());
        }
        if remote && origin.starts_with("claude/") {
            self.native.check_remote_source(origin)?;
        }
        let origin_endpoint = native
            .map(|(source, _, _)| source.clone())
            .or_else(|| self.native.endpoint(origin));
        let reply_endpoint = match native {
            Some((_, reply, _)) => Some(reply.clone()),
            None if origin.starts_with("claude/") && !remote => self.native.reply_endpoint(to).ok(),
            _ => None,
        };
        {
            let mut state = self.state.lock().unwrap();
            state.prune_deliveries(self.clock.now_ms());
            state.admit(mid, origin, to, remote, false, self.clock.now_ms())?;
            if let Some(delivery) = state.messages.get_mut(mid) {
                delivery.target_endpoint = Some(target.clone());
                delivery.origin_endpoint = origin_endpoint;
                delivery.reply_endpoint = reply_endpoint;
                if let Some((_, _, native_mid)) = native {
                    delivery.native_id = native_mid.to_string();
                }
            }
        }
        if remote {
            return relay::send(self, origin, to, body, mode, mid, fence).await;
        }
        let frame = json!({"type":"deliver","session_id":to,"from":origin,"body":body,"reply_to":origin,"message_id":mid});
        let result = if str_field(&target, "harness") == "webhook" {
            relay::webhook(&self.cfg, to, origin, body, mid).await
        } else {
            self.inject_local(&target, frame, mode, native.is_some(), fence)
                .await
        };
        match result {
            Ok(status) => {
                self.initial_status(mid, &status);
                if status == "read" {
                    self.local_receipt(mid, to, "read").await;
                }
                Ok(json!({"message_id":mid,"status":if status=="read"{"delivered"}else{&status}}))
            }
            Err(error) => {
                let status = if str_field(&target, "harness") == "codex"
                    && (error.starts_with("native send outcome unknown:")
                        || error.starts_with("Desktop send outcome unknown:")
                        || error.starts_with("Desktop write outcome unknown:"))
                {
                    "unknown"
                } else {
                    "dropped"
                };
                self.record_status(mid, status, false);
                Ok(json!({"message_id":mid,"status":status,"error":error}))
            }
        }
    }
    async fn push_native_snapshot(
        &self,
        target: &Value,
        value: Value,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<(), String> {
        let id = str_field(target, "id");
        let matches = |session: &Session| {
            target["_alias_owner"].as_u64() == Some(session.owner)
                && target["_alias_epoch"].as_u64() == Some(session.epoch)
        };
        let tx = {
            let state = self.state.lock().unwrap();
            state
                .sessions
                .get(id)
                .filter(|session| matches(session))
                .map(|session| session.tx.clone())
                .ok_or("native alias plugin generation changed")?
        };
        let permit = tokio::time::timeout(Duration::from_secs(5), tx.reserve_owned())
            .await
            .map_err(|_| "plugin write timeout".to_string())?
            .map_err(|_| "plugin disconnected".to_string())?;
        let (written, rx) = oneshot::channel();
        {
            let state = self.state.lock().unwrap();
            if *self.closing.borrow() || !state.sessions.get(id).is_some_and(matches) {
                return Err("native alias plugin generation changed".into());
            }
            permit.send(Push {
                value,
                written: Some(written),
                fence,
            });
        }
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .map_err(|_| "plugin write outcome unknown".to_string())?
            .map_err(|_| "plugin write outcome unknown".to_string())?
    }
    fn initial_status(&self, mid: &str, status: &str) {
        self.record_status(
            mid,
            if status == "read" {
                "delivered"
            } else {
                status
            },
            false,
        );
    }
    fn record_status(&self, mid: &str, status: &str, relay: bool) {
        if let Err(error) =
            self.state
                .lock()
                .unwrap()
                .record_status(mid, status, relay, self.clock.now_ms())
        {
            eprintln!("intercom receipt {mid} not tracked: {error}");
        }
    }
    pub(super) async fn status_report(&self) -> Value {
        let mut report = self
            .tool_status
            .get_or_init(probe_tool_status)
            .await
            .clone();
        report["herdr_socket"] = json!(herdr::socket_path().to_string_lossy());
        match herdr::call("ping", json!({}), std::time::Duration::from_secs(2)).await {
            Ok(pong) => {
                report["herdr"] = pong["version"].clone();
                if pong["protocol"].as_u64().unwrap_or(0) > 0 {
                    report["herdr_protocol"] = pong["protocol"].clone();
                }
            }
            Err(_) => {
                report["herdr"] = json!(if str_field(&report, "herdr_path").is_empty() {
                    "not installed"
                } else {
                    "not running"
                });
            }
        }
        if str_field(&report, "codex_path").is_empty() {
            report["codex"] = json!("not installed");
        } else {
            report["codex_install"] = json!(codex_install(std::path::Path::new(str_field(
                &report,
                "codex_path"
            ))));
            report.as_object_mut().unwrap().extend(
                self.native
                    .codex_status()
                    .await
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        let rows = self.directory().await;
        let mut claude = 0;
        let mut pi = 0;
        let mut codex = 0;
        let mut panes = 0;
        for row in rows.iter().filter(|row| {
            let machine = str_field(row, "machine_id");
            (machine.is_empty() || machine == self.machine) && row["all"] != true
        }) {
            match str_field(row, "harness") {
                "claude" => claude += 1,
                "pi" => pi += 1,
                "codex" => codex += 1,
                _ => {}
            }
            if !str_field(row, "herdr_pane_id").is_empty() {
                panes += 1;
            }
        }
        let (relay, mcp, native, mut legacy) = {
            let state = self.state.lock().unwrap();
            (
                if self.cfg.local_only {
                    "disabled (local_only)"
                } else if state.relay_tx.is_some() {
                    "connected"
                } else {
                    "disconnected"
                },
                state.mcp_status.clone(),
                state.native_status.clone(),
                state
                    .sessions
                    .iter()
                    .filter(|(_, session)| !session.caps.contains("read_receipts"))
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>(),
            )
        };
        legacy.sort();
        report.as_object_mut().unwrap().extend(json!({"machine":self.machine,"daemon":"running","pid":std::process::id(),"daemon_version":crate::VERSION,"cwd":std::env::current_dir().map(|path|path.to_string_lossy().into_owned()).unwrap_or_default(),"socket":self.cfg.sock,"relay":relay,"server":self.cfg.server,"mcp_listen":self.cfg.mcp_listen,"mcp_status":mcp,"mcp_url":if mcp=="running"{self.mcp_url.lock().unwrap().clone()}else{String::new()},"mcp_token_file":if mcp=="running"{self.generated_mcp_token_file()}else{String::new()},"native_status":native}).as_object().unwrap().clone());
        for (key, count) in [
            ("claude_n", claude),
            ("pi_n", pi),
            ("codex_n", codex),
            ("in_panes_n", panes),
        ] {
            if count > 0 {
                report[key] = json!(count);
            }
        }
        if !legacy.is_empty() {
            report["pi_legacy"] = json!(legacy);
        }
        report
    }
    pub async fn receipt(&self, mid: &str, reporter: &str, status: &str) {
        self.receipt_with_reason(mid, reporter, status, "").await;
    }
    pub(super) async fn receipt_with_reason(
        &self,
        mid: &str,
        reporter: &str,
        status: &str,
        reason: &str,
    ) {
        let reason = deliveries::normalize_drop_reason(reason);
        let delivery = {
            let mut s = self.state.lock().unwrap();
            s.prune_deliveries(self.clock.now_ms());
            let active = s.active(mid);
            if !active {
                return;
            }
            s.renew_history(mid, self.clock.now_ms());
            let Some(d) = s.messages.get_mut(mid) else {
                return;
            };
            if d.target != reporter || deliveries::rank(status) == 0 {
                return;
            }
            let rearm = active && d.route_terminal() && d.status != status;
            let ignored = d.ignores(status, false);
            if !rearm {
                d.consider(status, false, self.clock.now_ms());
            }
            if ignored && !rearm || d.status == "delivered" && !d.route_terminal() {
                return;
            }
            // Fence maintenance retries off while the first terminal forward is
            // in flight, and keep its reason for any genuine retry.
            d.drop_reason = reason.into();
            if d.route_terminal() {
                d.forwarding = true;
            }
            d.clone()
        };
        self.record("receipt", mid, reporter, &delivery.status);
        if delivery.route_terminal() {
            self.retry_receipt(mid, delivery).await;
        } else {
            self.forward_receipt(mid, delivery, reason).await;
        }
    }
    async fn forward_receipt(&self, mid: &str, delivery: Delivery, reason: &str) -> bool {
        let result = if delivery.reply_relay {
            let tx = self.state.lock().unwrap().relay_tx.clone();
            match tx {Some(tx)=>tx.send(json!({"type":"receipt","message_id":mid,"session_id":delivery.target,"from":delivery.target,"status":delivery.status}).into()).await.map_err(|_|"relay disconnected".to_string()),None=>Err("relay disconnected".into())}
        } else if delivery.origin.starts_with("claude/") {
            self.native
                .receipt(
                    &delivery.origin,
                    delivery.origin_endpoint.as_ref(),
                    delivery.reply_endpoint.as_ref(),
                    &delivery.target,
                    if delivery.native_id.is_empty() {
                        mid
                    } else {
                        &delivery.native_id
                    },
                    &delivery.status,
                    reason,
                )
                .await
        } else if delivery.origin.starts_with("codex/") {
            Ok(())
        } else {
            self.push(&delivery.origin,json!({"type":"receipt","from":delivery.target,"status":delivery.status,"message_id":mid})).await
        };
        if result.is_ok() && delivery.route_terminal() {
            self.state
                .lock()
                .unwrap()
                .commit_delivery(mid, &delivery.token);
        }
        result.is_ok()
    }
    async fn retry_receipt(&self, mid: &str, delivery: Delivery) {
        let mut forward = Forwarding {
            runtime: self,
            mid,
            token: delivery.token.clone(),
            forwarded: false,
        };
        forward.forwarded = self
            .forward_receipt(mid, delivery.clone(), &delivery.drop_reason)
            .await;
    }
}
/// Clears a delivery's in-flight forward even when the forwarding future is
/// cancelled, so maintenance can retry it.
struct Forwarding<'a> {
    runtime: &'a Runtime,
    mid: &'a str,
    token: Arc<()>,
    forwarded: bool,
}
impl Drop for Forwarding<'_> {
    fn drop(&mut self) {
        let mut state = self.runtime.state.lock().unwrap();
        if let Some(current) = state
            .messages
            .get_mut(self.mid)
            .filter(|current| Arc::ptr_eq(&current.token, &self.token))
        {
            current.forwarding = false;
            if !self.forwarded {
                current.retry_failed(self.runtime.clock.now_ms());
            }
        }
    }
}
/// Remote rows never shadow a local session, and a remote row cannot claim
/// this machine (or no machine) and thereby pass as local.
fn merge_remote(rows: &mut Vec<Value>, remote: Vec<Value>, machine: &str) {
    let local: HashSet<String> = rows.iter().map(|r| str_field(r, "id").to_string()).collect();
    rows.extend(remote.into_iter().filter(|row| {
        let owner = str_field(row, "machine_id");
        !owner.is_empty() && owner != machine && !local.contains(str_field(row, "id"))
    }));
}
pub fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
const FROM_MODES: [&str; 3] = ["", "prompting", "bypass"];
fn check_from_mode(mode: &str) -> Result<(), String> {
    if FROM_MODES.contains(&mode) {
        Ok(())
    } else {
        Err("from_mode must be prompting or bypass".into())
    }
}
/// An intercom session id: `<kind>/<id>` where the id half follows the daemon
/// session-id grammar. Ids are interpolated into native envelopes and reply
/// instructions, so anything else is refused at trust boundaries.
pub(crate) fn valid_intercom_id(id: &str) -> bool {
    id.split_once('/').is_some_and(|(kind, rest)| {
        !kind.is_empty()
            && kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && crate::daemon::valid_session_id(rest)
    })
}
/// The directory row as published beyond this daemon: `_`-prefixed fields are
/// internal routing state and the pid is only meaningful on this host.
pub(crate) fn public_row(mut row: Value) -> Value {
    if let Some(fields) = row.as_object_mut() {
        fields.retain(|key, _| !key.starts_with('_') && key != "pid");
    }
    row
}

async fn probe_tool_status() -> Value {
    let mut report = json!({});
    for tool in ["claude", "pi", "herdr", "codex"] {
        if let Some((path, version)) = tool_version(tool).await {
            report[format!("{tool}_path")] = json!(path.to_string_lossy());
            if tool != "herdr" {
                report[tool] = json!(version);
            }
        }
    }
    report
}

fn codex_install(path: &std::path::Path) -> &'static str {
    let home = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::home().join(".codex"));
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if resolved.to_string_lossy().contains(".app/")
        || resolved
            .to_string_lossy()
            .to_lowercase()
            .contains("codex.app")
    {
        "desktop"
    } else if home.join("packages/standalone/current/codex").exists() {
        "standalone"
    } else {
        "npm"
    }
}

pub(super) async fn tool_status_report() -> Value {
    let mut report = json!({"herdr_socket":herdr::socket_path().to_string_lossy()});
    for tool in ["claude", "pi", "codex"] {
        if let Some((path, version)) = tool_version(tool).await {
            report[tool] = json!(version);
            report[format!("{tool}_path")] = json!(path.to_string_lossy());
            if tool == "codex" {
                let home = std::env::var_os("CODEX_HOME")
                    .filter(|value| !value.is_empty())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| config::home().join(".codex"));
                let resolved = std::fs::canonicalize(&path).unwrap_or(path);
                let desktop = resolved.to_string_lossy().contains(".app/")
                    || resolved
                        .to_string_lossy()
                        .to_lowercase()
                        .contains("codex.app");
                report["codex_install"] = json!(if desktop {
                    "desktop"
                } else if home.join("packages/standalone/current/codex").exists() {
                    "standalone"
                } else {
                    "npm"
                });
                let socket = ["CODEX_APP_SERVER_SOCK", "CODEX_APP_SERVER_SOCKET"]
                    .into_iter()
                    .find_map(|key| {
                        std::env::var_os(key).and_then(|value| match value.to_str() {
                            Some(text) if text.trim().is_empty() => None,
                            Some(text) => Some(std::path::PathBuf::from(text.trim())),
                            None => Some(std::path::PathBuf::from(value)),
                        })
                    })
                    .unwrap_or_else(|| home.join("app-server-control/app-server-control.sock"));
                #[cfg(unix)]
                let reachable = matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        tokio::net::UnixStream::connect(&socket)
                    )
                    .await,
                    Ok(Ok(_))
                );
                #[cfg(not(unix))]
                let reachable = false;
                report["codex_app_server"] = json!(if reachable {
                    format!("external listener ({})", socket.display())
                } else {
                    "not attached".into()
                });
            }
        }
    }
    if let Some((path, _)) = tool_version("herdr").await {
        report["herdr_path"] = json!(path.to_string_lossy());
        match herdr::call("ping", json!({}), std::time::Duration::from_secs(2)).await {
            Ok(pong) => {
                report["herdr"] = pong["version"].clone();
                if pong["protocol"].as_u64().unwrap_or(0) > 0 {
                    report["herdr_protocol"] = pong["protocol"].clone();
                }
            }
            Err(_) => report["herdr"] = json!("not running"),
        }
    } else {
        report["herdr"] = json!("not installed");
    }
    report
}

async fn tool_version(name: &str) -> Option<(std::path::PathBuf, String)> {
    for directory in std::env::split_paths(&std::env::var_os("PATH")?) {
        let path = directory.join(name);
        if !path.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
            if unsafe { libc::access(cpath.as_ptr(), libc::X_OK) } != 0 {
                continue;
            }
        }
        if !path.is_absolute() {
            return None;
        }
        #[cfg(unix)]
        let mut command = {
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .arg("-c")
                .arg("exec \"$1\" --version 2>&1")
                .arg("pi-famulus-tool-version")
                .arg(&path);
            command
        };
        #[cfg(not(unix))]
        let mut command = {
            let mut command = tokio::process::Command::new(&path);
            command.arg("--version");
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let version =
            match tokio::time::timeout(std::time::Duration::from_secs(2), command.output()).await {
                Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
        return Some((path, version));
    }
    None
}
