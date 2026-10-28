//! Hosted subagents: one `pi --mode rpc` process per child, owned by the daemon.
//!
//! The daemon frames nothing of the conversation. It tracks residency from
//! three signals (`agent_start`, `agent_settled`, process exit) plus the
//! retention timer, and it moves JSONL between the extension and the child.

use crate::proto::{self, now_ms, EventKind, ProtoError, E_BAD_REQUEST, E_FORBIDDEN, E_NOT_FOUND};
use crate::task;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const USER_DRIVER_MS: u64 = 10 * 60 * 1000;
const DECISION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct Hosted {
    inner: Arc<Mutex<Inner>>,
    notices: mpsc::UnboundedSender<Notice>,
}

pub struct Notice {
    pub parent_session: String,
    pub child_id: String,
    pub event: EventKind,
}

struct Inner {
    home: PathBuf,
    agents: HashMap<String, Agent>,
    by_file: HashMap<String, String>,
}

struct Agent {
    child_id: String,
    parent_session: String,
    run_id: String,
    name: String,
    token: String,
    argv: Vec<String>,
    cwd: String,
    env: HashMap<String, String>,
    residency: String,
    activity: String,
    driver: String,
    pid: u32,
    session_file: Option<String>,
    pi_session_id: Option<String>,
    cache_expires_at: Option<u64>,
    cache_lifetime_ms: Option<u64>,
    cache_tier: Option<String>,
    pi_version: Option<String>,
    idle_since: Option<u64>,
    last_user_ms: Option<u64>,
    user_spoke: bool,
    last_text: String,
    child_conn: Option<u64>,
    watchers: HashSet<u64>,
    stdin: Option<mpsc::UnboundedSender<String>>,
    pending: HashMap<String, oneshot::Sender<Value>>,
    comms: HashMap<String, oneshot::Sender<String>>,
    stderr: String,
    /// Bumped on every spawn so a late reader from the previous process is ignored.
    generation: u64,
    park_scheduled: bool,
}

struct Limits {
    idle_min: u64,
    grace: u64,
    idle_max: u64,
    max_warm: usize,
}

#[derive(Serialize, Deserialize)]
struct DiskAgent {
    child_id: String,
    parent_session: String,
    run_id: String,
    name: String,
    token: String,
    argv: Vec<String>,
    cwd: String,
    #[serde(default)]
    env: HashMap<String, String>,
    residency: String,
    activity: String,
    driver: String,
    session_file: Option<String>,
    pi_session_id: Option<String>,
    cache_expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_lifetime_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pi_version: Option<String>,
    last_text: String,
}

impl Hosted {
    pub fn new(home: PathBuf, notices: mpsc::UnboundedSender<Notice>) -> Self {
        let hosted = Hosted {
            inner: Arc::new(Mutex::new(Inner {
                home: home.clone(),
                agents: HashMap::new(),
                by_file: HashMap::new(),
            })),
            notices,
        };
        hosted.load();
        hosted
    }

    pub fn watcher_ids(&self, child_id: &str) -> Vec<u64> {
        self.inner.lock().unwrap().agents.get(child_id).map(|a| a.watchers.iter().copied().collect()).unwrap_or_default()
    }

    pub fn warm_count(&self) -> usize {
        self.inner.lock().unwrap().agents.values().filter(|a| a.residency == "warm").count()
    }

    pub fn auth_child(&self, child_id: &str, token: &str) -> Result<String, ProtoError> {
        let g = self.inner.lock().unwrap();
        let agent = g.agents.get(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
        if agent.token != token {
            return Err(ProtoError::new(E_FORBIDDEN, "bad child token"));
        }
        Ok(agent.parent_session.clone())
    }

    pub fn set_child_conn(&self, child_id: &str, conn_id: Option<u64>) {
        if let Some(agent) = self.inner.lock().unwrap().agents.get_mut(child_id) {
            agent.child_conn = conn_id;
        }
    }

    pub fn child_of_conn(&self, child_id: &str) -> Option<u64> {
        self.inner.lock().unwrap().agents.get(child_id).and_then(|a| a.child_conn)
    }

    /// Parent extensions may touch only their own children. A hosted child may
    /// touch itself, and siblings in the same run only when `sibling_input` is
    /// set (send/watch). Park, takeover and UI replies stay with the owner.
    /// The CLI (top, attach) may touch any child.
    pub fn authorize(
        &self,
        role: &str,
        session_id: Option<&str>,
        caller_child: Option<&str>,
        target: &str,
        sibling_input: bool,
    ) -> Result<(), ProtoError> {
        let g = self.inner.lock().unwrap();
        let agent = g.agents.get(target).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
        match role {
            "cli" => Ok(()),
            "child" => {
                let me = caller_child.unwrap_or("");
                if me == target {
                    return Ok(());
                }
                if !sibling_input {
                    return Err(ProtoError::new(E_FORBIDDEN, "a hosted child cannot address that agent"));
                }
                match g.agents.get(me) {
                    Some(mine) if mine.parent_session == agent.parent_session && mine.run_id == agent.run_id => Ok(()),
                    _ => Err(ProtoError::new(E_FORBIDDEN, "a hosted child cannot address that agent")),
                }
            }
            _ => {
                if session_id == Some(agent.parent_session.as_str()) {
                    Ok(())
                } else {
                    Err(ProtoError::new(E_FORBIDDEN, "that subagent belongs to another session"))
                }
            }
        }
    }

    pub fn lookup(&self, session_id: Option<&str>, session_file: Option<&str>) -> Option<crate::proto::HostedNotice> {
        let g = self.inner.lock().unwrap();
        let id = resolve_id(&g, session_id, session_file)?;
        g.agents.get(&id).map(|a| a.notice())
    }

    /// Parent session that owns this hosted child.
    pub fn parent_of(&self, child_id: &str) -> Option<String> {
        self.inner.lock().unwrap().agents.get(child_id).map(|a| a.parent_session.clone())
    }

    /// Same-run siblings, including the caller. A hosted child uses this to address peers.
    pub fn peers(&self, child_id: &str) -> Vec<Value> {
        let g = self.inner.lock().unwrap();
        let Some(me) = g.agents.get(child_id) else { return Vec::new() };
        let parent = me.parent_session.clone();
        let run = me.run_id.clone();
        g.agents
            .values()
            .filter(|a| a.parent_session == parent && a.run_id == run)
            .map(|a| json!({
                "child_id": a.child_id,
                "name": a.name,
                "run_id": a.run_id,
                "activity": a.activity,
                "residency": a.residency,
            }))
            .collect()
    }

    pub fn set_pi_version(&self, child_id: &str, version: &str) {
        if version.is_empty() {
            return;
        }
        let home = {
            let mut g = self.inner.lock().unwrap();
            if let Some(agent) = g.agents.get_mut(child_id) {
                agent.pi_version = Some(version.to_string());
            }
            g.home.clone()
        };
        if let Some(agent) = self.inner.lock().unwrap().agents.get(child_id) {
            agent.persist(&home);
        }
    }

    /// Interactive pi opened a parked session file: the daemon will not respawn it.
    pub fn claim_owner(&self, session_id: &str, session_file: Option<&str>) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        let id = session_file.and_then(|f| g.by_file.get(f).cloned()).filter(|id| {
            g.agents.get(id).is_some_and(|a| a.residency == "parked")
        })?;
        let home = g.home.clone();
        if let Some(agent) = g.agents.get_mut(&id) {
            agent.parent_session = session_id.to_string();
            agent.driver = "external".into();
            agent.persist(&home);
            return Some(id);
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        &self,
        parent_session: &str,
        child_id: &str,
        run_id: &str,
        name: &str,
        argv: Vec<String>,
        cwd: String,
        mut env: HashMap<String, String>,
    ) -> Result<Value, ProtoError> {
        if argv.is_empty() || argv.iter().any(|a| a.is_empty()) {
            return Err(ProtoError::new(E_BAD_REQUEST, "agent argv is empty"));
        }
        {
            let g = self.inner.lock().unwrap();
            if let Some(agent) = g.agents.get(child_id) {
                if agent.parent_session_claimed_externally() {
                    return Err(ProtoError::new(
                        E_FORBIDDEN,
                        format!("{child_id} is open in pi session {}; ask the user or wait", agent.parent_session),
                    ));
                }
                if agent.residency == "warm" {
                    if agent.parent_session != parent_session {
                        return Err(ProtoError::new(
                            E_FORBIDDEN,
                            format!("child {child_id} is hosted for session {}", agent.parent_session),
                        ));
                    }
                    return Ok(agent.public_json());
                }
            }
        }
        let token = fresh_token();
        env.insert("PI_FAMULUS_CHILD_ID".into(), child_id.to_string());
        env.insert("PI_FAMULUS_CHILD_TOKEN".into(), token.clone());
        env.insert("PI_FAMULUS_HOME".into(), self.inner.lock().unwrap().home.display().to_string());
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();
        let generation = {
            let mut g = self.inner.lock().unwrap();
            let generation = g.agents.get(child_id).map(|a| a.generation + 1).unwrap_or(1);
            let agent = g.agents.entry(child_id.to_string()).or_insert_with(|| Agent::new(
                child_id, parent_session, run_id, name, &token, argv.clone(), cwd.clone(),
            ));
            agent.parent_session = parent_session.to_string();
            agent.run_id = run_id.to_string();
            agent.name = name.to_string();
            agent.token = token;
            agent.argv = argv.clone();
            agent.cwd = cwd.clone();
            agent.env = env.clone();
            agent.residency = "warm".into();
            agent.activity = "starting".into();
            agent.generation = generation;
            agent.stdin = Some(stdin_tx);
            agent.park_scheduled = false;
            agent.idle_since = None;
            generation
        };
        let proc = spawn_rpc(&argv, &cwd, &env).await.map_err(|e| {
            self.mark_failed(child_id, &e.to_string());
            ProtoError::new(crate::proto::E_INTERNAL, format!("spawn failed: {e}"))
        })?;
        let pid = proc.pid;
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(agent) = g.agents.get_mut(child_id) {
                agent.pid = pid;
            }
        }
        self.pump(child_id.to_string(), generation, proc.stdout, proc.stderr, proc.stdin, stdin_rx, proc.child);
        let state = match self.rpc_ready(child_id, "get_state", "", "daemon", None).await {
            Ok(v) => v,
            Err(e) => {
                let pid = self.inner.lock().unwrap().agents.get(child_id).map(|a| a.pid).unwrap_or(0);
                self.mark_failed(child_id, &e.message);
                if pid != 0 {
                    let _ = task::signal_group(pid, task::SIGKILL);
                }
                return Err(e);
            }
        };
        if let Some(file) = state.get("sessionFile").and_then(|v| v.as_str()) {
            let mut g = self.inner.lock().unwrap();
            let home = g.home.clone();
            let old = g.agents.get(child_id).and_then(|a| a.session_file.clone());
            if let Some(old) = old {
                g.by_file.remove(&old);
            }
            if let Some(agent) = g.agents.get_mut(child_id) {
                agent.session_file = Some(file.to_string());
                agent.persist(&home);
            }
            g.by_file.insert(file.to_string(), child_id.to_string());
        }
        if let Some(id) = state.get("sessionId").and_then(|v| v.as_str()) {
            if let Some(agent) = self.inner.lock().unwrap().agents.get_mut(child_id) {
                agent.pi_session_id = Some(id.to_string());
            }
        }
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(agent) = g.agents.get_mut(child_id) {
                if agent.activity == "starting" {
                    let streaming = state.get("isStreaming").and_then(|v| v.as_bool()).unwrap_or(false);
                    if streaming {
                        agent.activity = "running".into();
                    } else {
                        agent.activity = "idle".into();
                        agent.idle_since = Some(now_ms());
                    }
                }
            }
        }
        self.emit_state(child_id);
        let g = self.inner.lock().unwrap();
        g.agents.get(child_id).map(|a| a.public_json()).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "child vanished"))
    }

    pub async fn rpc(
        &self,
        child_id: &str,
        op: &str,
        text: &str,
        source: &str,
        since: Option<&str>,
    ) -> Result<Value, ProtoError> {
        if source.starts_with("ch_") && source != child_id && !self.same_run(source, child_id) {
            return Err(ProtoError::new(E_FORBIDDEN, "cross-run messaging not allowed"));
        }
        self.ensure_warm(child_id).await?;
        self.rpc_ready(child_id, op, text, source, since).await
    }

    async fn rpc_ready(
        &self,
        child_id: &str,
        op: &str,
        text: &str,
        source: &str,
        since: Option<&str>,
    ) -> Result<Value, ProtoError> {
        let id = format!("in_{}", fresh_token());
        let line = rpc_line(&id, op, text, since);
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.inner.lock().unwrap();
            let agent = g.agents.get_mut(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
            if source == "user" {
                agent.driver = "user".into();
                agent.last_user_ms = Some(now_ms());
                agent.user_spoke = true;
                agent.idle_since = None;
            }
            let stdin = agent.stdin.clone().ok_or_else(|| ProtoError::new(crate::proto::E_INTERNAL, "child has no stdin"))?;
            agent.pending.insert(id, tx);
            stdin.send(line).map_err(|_| ProtoError::new(crate::proto::E_INTERNAL, "child stdin closed"))?;
        }
        if source == "user" {
            self.emit_state(child_id);
        }
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, rx).await {
            Ok(Ok(v)) => {
                if v.get("success").and_then(|s| s.as_bool()) == Some(false) {
                    let msg = v.get("error").and_then(|e| e.as_str()).unwrap_or("rpc command failed");
                    return Err(ProtoError::new(E_BAD_REQUEST, msg));
                }
                let mut data = v.get("data").cloned().unwrap_or(json!({}));
                let driver = self.inner.lock().unwrap().agents.get(child_id).map(|a| a.driver.clone());
                if let (Some(obj), Some(driver)) = (data.as_object_mut(), driver) {
                    obj.insert("driver".into(), json!(driver));
                }
                Ok(data)
            }
            _ => Err(ProtoError::new(crate::proto::E_INTERNAL, "hosted pi did not answer")),
        }
    }

    pub fn watch(&self, child_id: &str, conn_id: u64) -> Result<(), ProtoError> {
        let mut g = self.inner.lock().unwrap();
        let agent = g.agents.get_mut(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
        agent.watchers.insert(conn_id);
        Ok(())
    }

    pub fn unwatch(&self, child_id: &str, conn_id: u64) {
        if let Some(agent) = self.inner.lock().unwrap().agents.get_mut(child_id) {
            agent.watchers.remove(&conn_id);
        }
    }

    pub fn drop_conn(&self, conn_id: u64) {
        let mut g = self.inner.lock().unwrap();
        for agent in g.agents.values_mut() {
            agent.watchers.remove(&conn_id);
            if agent.child_conn == Some(conn_id) {
                agent.child_conn = None;
            }
        }
    }

    pub async fn comms(
        &self,
        child_id: &str,
        request_id: &str,
        reason: &str,
        message: &str,
    ) -> Result<String, ProtoError> {
        let (parent, name, child_conn) = {
            let g = self.inner.lock().unwrap();
            let agent = g.agents.get(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
            (agent.parent_session.clone(), agent.name.clone(), agent.child_conn)
        };
        if reason != "need_decision" {
            self.send_notice(Notice {
                parent_session: parent,
                child_id: child_id.to_string(),
                event: EventKind::AgentCommsEvent {
                    child_id: child_id.to_string(),
                    request_id: request_id.to_string(),
                    reason: reason.to_string(),
                    message: message.to_string(),
                    name,
                },
            });
            return Ok("ok".into());
        }
        let (tx, rx) = oneshot::channel();
        self.inner.lock().unwrap().agents.get_mut(child_id).map(|a| a.comms.insert(request_id.to_string(), tx));
        self.send_notice(Notice {
            parent_session: parent,
            child_id: child_id.to_string(),
            event: EventKind::AgentCommsEvent {
                child_id: child_id.to_string(),
                request_id: request_id.to_string(),
                reason: reason.to_string(),
                message: message.to_string(),
                name,
            },
        });
        let _ = child_conn;
        match tokio::time::timeout(DECISION_TIMEOUT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => Ok("Supervisor did not respond within 10 minutes; decide yourself and continue.".into()),
        }
    }

    pub fn comms_reply(&self, request_id: &str, child_id: &str, message: &str) -> Result<(), ProtoError> {
        let mut g = self.inner.lock().unwrap();
        if let Some(agent) = g.agents.get_mut(child_id) {
            if let Some(tx) = agent.comms.remove(request_id) {
                let _ = tx.send(message.to_string());
                return Ok(());
            }
        }
        Err(ProtoError::new(E_NOT_FOUND, "no pending decision with that request_id"))
    }

    pub fn cache(&self, child_id: &str, expires_at: Option<u64>, tier: Option<String>, lifetime_ms: Option<u64>) {
        let home = {
            let mut g = self.inner.lock().unwrap();
            if let Some(agent) = g.agents.get_mut(child_id) {
                agent.cache_expires_at = expires_at;
                agent.cache_tier = tier;
                agent.cache_lifetime_ms = lifetime_ms;
            }
            g.home.clone()
        };
        if let Some(agent) = self.inner.lock().unwrap().agents.get(child_id) {
            agent.persist(&home);
        }
        self.emit_state(child_id);
    }

    pub fn ui_response(&self, child_id: &str, rpc_id: &str, value: &Value, cancelled: bool) -> Result<(), ProtoError> {
        let line = if cancelled {
            json!({"type":"extension_ui_response","id": rpc_id, "cancelled": true}).to_string()
        } else {
            let mut body = json!({"type":"extension_ui_response","id": rpc_id});
            if let Some(obj) = body.as_object_mut() {
                if let Some(map) = value.as_object() {
                    for (k, v) in map {
                        obj.insert(k.clone(), v.clone());
                    }
                } else {
                    obj.insert("value".into(), value.clone());
                }
            }
            body.to_string()
        };
        let g = self.inner.lock().unwrap();
        let agent = g.agents.get(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
        let stdin = agent.stdin.clone().ok_or_else(|| ProtoError::new(crate::proto::E_INTERNAL, "child is not warm"))?;
        stdin.send(line).map_err(|_| ProtoError::new(crate::proto::E_INTERNAL, "child stdin closed"))?;
        Ok(())
    }

    pub fn park(&self, child_id: &str, reason: &str) -> Result<(), ProtoError> {
        self.park_inner(child_id, reason)
    }

    /// Abort the hosted process and wait until it has left the session file.
    pub async fn takeover(&self, child_id: &str) -> Result<(), ProtoError> {
        let pid = {
            let g = self.inner.lock().unwrap();
            g.agents.get(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?.pid
        };
        if pid != 0 {
            self.rpc_nowait(child_id, "abort");
        }
        self.park_inner(child_id, "takeover")?;
        wait_pid_gone(pid).await;
        Ok(())
    }

    pub fn on_parent_closed(&self, parent_session: &str) {
        let ids: Vec<String> = {
            let g = self.inner.lock().unwrap();
            g.agents.iter().filter(|(_, a)| a.parent_session == parent_session && a.activity == "running").map(|(id, _)| id.clone()).collect()
        };
        for id in ids {
            self.rpc_nowait(&id, "abort");
        }
    }

    pub fn tick(&self) {
        let now = now_ms();
        let limits = limits_of(&self.inner.lock().unwrap().home);
        let mut park = Vec::new();
        let mut detach = Vec::new();
        {
            let g = self.inner.lock().unwrap();
            let warm_idle: Vec<(String, u64)> = g.agents.values().filter(|a| a.residency == "warm" && a.activity != "running" && a.activity != "starting").map(|a| (a.child_id.clone(), a.idle_since.unwrap_or(now))).collect();
            let mut idle_ids: Vec<String> = warm_idle.iter().map(|(id, _)| id.clone()).collect();
            if warm_idle.len() > limits.max_warm {
                idle_ids.sort_by_key(|id| warm_idle.iter().find(|(i, _)| i == id).map(|(_, t)| *t).unwrap_or(0));
                let extra = warm_idle.len().saturating_sub(limits.max_warm);
                park.extend(idle_ids.iter().take(extra).cloned().map(|id| (id, "warm-cap")));
            }
            for agent in g.agents.values() {
                if agent.residency != "warm" || agent.activity == "running" || agent.activity == "starting" {
                    continue;
                }
                let idle = agent.idle_since.unwrap_or(now);
                let due = park_due_at(idle, agent.cache_expires_at, &limits);
                if now >= due {
                    park.push((agent.child_id.clone(), "retention"));
                }
                if agent.driver == "user" {
                    if let Some(at) = agent.last_user_ms {
                        if now.saturating_sub(at) >= USER_DRIVER_MS {
                            detach.push(agent.child_id.clone());
                        }
                    }
                }
            }
        }
        for (id, reason) in park {
            let _ = self.park_inner(&id, reason);
        }
        for id in detach {
            self.release_driver(&id);
        }
    }

    fn ensure_warm(&self, child_id: &str) -> impl std::future::Future<Output = Result<(), ProtoError>> + '_ {
        let child_id = child_id.to_string();
        async move {
            let snapshot = {
                let g = self.inner.lock().unwrap();
                let agent = g.agents.get(&child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
                if agent.residency == "warm" && agent.stdin.is_some() {
                    return Ok(());
                }
                if agent.residency == "parked" && agent.parent_session_claimed_externally() {
                    return Err(ProtoError::new(
                        E_FORBIDDEN,
                        format!("{child_id} is open in pi session {}; ask the user or wait", agent.parent_session),
                    ));
                }
                (agent.argv.clone(), agent.cwd.clone(), agent.env.clone(), agent.run_id.clone(), agent.name.clone(), agent.parent_session.clone())
            };
            let (argv, cwd, env, run_id, name, parent) = snapshot;
            self.spawn(&parent, &child_id, &run_id, &name, argv, cwd, env).await.map(|_| ())
        }
    }

    fn park_inner(&self, child_id: &str, reason: &str) -> Result<(), ProtoError> {
        let (parent, name) = {
            let mut g = self.inner.lock().unwrap();
            let home = g.home.clone();
            let agent = g.agents.get_mut(child_id).ok_or_else(|| ProtoError::new(E_NOT_FOUND, "unknown child"))?;
            agent.park_scheduled = true;
            agent.stdin.take();
            agent.residency = "parked".into();
            if agent.activity == "running" || agent.activity == "starting" {
                agent.activity = "idle".into();
            }
            agent.driver = "parent".into();
            agent.persist(&home);
            (agent.parent_session.clone(), agent.name.clone())
        };
        self.emit_state(child_id);
        let home = self.inner.lock().unwrap().home.clone();
        crate::events::emit(&home, Some(&parent), "agent.park", Some(child_id), json!({ "reason": reason, "name": name }));
        Ok(())
    }

    fn release_driver(&self, child_id: &str) {
        let notice = {
            let mut g = self.inner.lock().unwrap();
            let Some(agent) = g.agents.get_mut(child_id) else { return };
            if agent.driver != "user" {
                return;
            }
            agent.driver = "parent".into();
            let spoke = agent.user_spoke;
            agent.user_spoke = false;
            spoke.then(|| (agent.parent_session.clone(), agent.name.clone(), agent.last_text.clone()))
        };
        self.emit_state(child_id);
        if let Some((parent, name, last_text)) = notice {
            self.send_notice(Notice {
                parent_session: parent,
                child_id: child_id.to_string(),
                event: EventKind::AgentDetach { child_id: child_id.to_string(), name, last_text },
            });
        }
    }

    fn rpc_nowait(&self, child_id: &str, op: &str) {
        let id = format!("in_{}", fresh_token());
        let line = rpc_line(&id, op, "", None);
        if let Some(agent) = self.inner.lock().unwrap().agents.get(child_id) {
            if let Some(stdin) = &agent.stdin {
                let _ = stdin.send(line);
            }
        }
    }

    fn mark_failed(&self, child_id: &str, error: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some(agent) = g.agents.get_mut(child_id) {
            agent.activity = "failed".into();
            agent.residency = "none".into();
            agent.stderr = error.to_string();
            agent.stdin.take();
        }
    }

    fn emit_state(&self, child_id: &str) {
        let g = self.inner.lock().unwrap();
        if let Some(agent) = g.agents.get(child_id) {
            let event = agent.state_event();
            let parent = agent.parent_session.clone();
            drop(g);
            self.send_notice(Notice { parent_session: parent, child_id: child_id.to_string(), event });
        }
    }

    fn send_notice(&self, notice: Notice) {
        let _ = self.notices.send(notice);
    }

    #[allow(clippy::too_many_arguments)]
    fn pump(
        &self,
        child_id: String,
        generation: u64,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        mut stdin: tokio::process::ChildStdin,
        mut rx: mpsc::UnboundedReceiver<String>,
        mut child: tokio::process::Child,
    ) {
        let hosted = self.clone();
        let out_id = child_id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                hosted.on_line(&out_id, generation, &line);
            }
        });
        let hosted_err = self.clone();
        let err_id = child_id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                hosted_err.push_stderr(&err_id, generation, &line);
            }
        });
        let hosted_in = self.clone();
        let in_id = child_id.clone();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if hosted_in.generation_of(&in_id) != Some(generation) {
                    break;
                }
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            drop(stdin);
        });
        let hosted_wait = self.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            hosted_wait.on_exit(&child_id, generation, status.ok().and_then(|s| s.code()));
        });
    }

    fn generation_of(&self, child_id: &str) -> Option<u64> {
        self.inner.lock().unwrap().agents.get(child_id).map(|a| a.generation)
    }

    fn on_line(&self, child_id: &str, generation: u64, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else { return };
        let mut completed: Option<oneshot::Sender<Value>> = None;
        #[allow(unused_assignments)]
        let mut parent = None;
        let mut forward = false;
        let mut ui_cancel: Option<String> = None;
        let mut capped = Vec::new();
        {
            let mut g = self.inner.lock().unwrap();
            let Some(agent) = g.agents.get_mut(child_id) else { return };
            if agent.generation != generation {
                return;
            }
            parent = Some(agent.parent_session.clone());
            let typ = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if typ == "response" {
                if let Some(id) = value.get("id").and_then(|i| i.as_str()) {
                    completed = agent.pending.remove(id);
                }
            } else {
                forward = true;
                match typ {
                    "agent_start" => {
                        agent.activity = "running".into();
                        agent.idle_since = None;
                    }
                    "agent_settled" => {
                        if agent.activity != "failed" {
                            agent.activity = "idle".into();
                        }
                        agent.idle_since = Some(now_ms());
                        capped = over_cap_ids(&g);
                    }
                    "message_end" => {
                        if let Some(text) = assistant_text(&value) {
                            agent.last_text = text;
                        }
                    }
                    "extension_ui_request" => {
                        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let dialog = matches!(method, "select" | "confirm" | "input" | "editor");
                        if dialog && agent.watchers.is_empty() {
                            ui_cancel = value.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
                            forward = false;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(tx) = completed {
            let _ = tx.send(value.clone());
        }
        if let Some(rpc_id) = ui_cancel {
            let _ = self.ui_response(child_id, &rpc_id, &json!({"cancelled": true}), true);
            let home = self.inner.lock().unwrap().home.clone();
            crate::events::emit(&home, parent.as_deref(), "agent.ui_cancelled", Some(child_id), json!({ "rpc_id": rpc_id }));
        }
        for id in capped {
            let _ = self.park_inner(&id, "warm-cap");
        }
        if forward {
            let event = if value.get("type").and_then(|t| t.as_str()) == Some("extension_ui_request") {
                EventKind::AgentUi { child_id: child_id.to_string(), request: value }
            } else {
                EventKind::AgentRpc { child_id: child_id.to_string(), record: value }
            };
            self.send_notice(Notice { parent_session: parent.unwrap_or_default(), child_id: child_id.to_string(), event });
            self.emit_state(child_id);
        }
    }

    fn push_stderr(&self, child_id: &str, generation: u64, line: &str) {
        let mut g = self.inner.lock().unwrap();
        if let Some(agent) = g.agents.get_mut(child_id) {
            if agent.generation == generation {
                agent.stderr.push_str(line);
                agent.stderr.push('\n');
                if agent.stderr.len() > 4000 {
                    agent.stderr = agent.stderr.split_off(agent.stderr.len() - 4000);
                }
            }
        }
    }

    fn on_exit(&self, child_id: &str, generation: u64, code: Option<i32>) {
        let mut failed = false;
        let parent = {
            let mut g = self.inner.lock().unwrap();
            let home = g.home.clone();
            let Some(agent) = g.agents.get_mut(child_id) else { return };
            if agent.generation != generation {
                return;
            }
            agent.stdin.take();
            agent.pid = 0;
            let parent = agent.parent_session.clone();
            if agent.park_scheduled || agent.residency == "parked" {
                agent.residency = "parked".into();
                if agent.activity == "running" || agent.activity == "starting" {
                    agent.activity = "idle".into();
                }
            } else if code == Some(0) {
                agent.residency = if agent.session_file.is_some() { "parked".into() } else { "none".into() };
                agent.activity = "idle".into();
            } else {
                agent.activity = "failed".into();
                agent.residency = if agent.session_file.is_some() { "parked".into() } else { "none".into() };
                failed = true;
            }
            agent.idle_since = Some(now_ms());
            for (_, tx) in agent.pending.drain() {
                let _ = tx.send(json!({"success": false, "error": "process exited"}));
            }
            agent.persist(&home);
            parent
        };
        if failed {
            let err = self.inner.lock().unwrap().agents.get(child_id).map(|a| a.stderr.clone()).unwrap_or_default();
            let home = self.inner.lock().unwrap().home.clone();
            crate::events::emit(&home, Some(&parent), "agent.exit", Some(child_id), json!({ "error": err, "exit_code": code }));
        }
        self.emit_state(child_id);
    }

    fn load(&self) {
        let home = self.inner.lock().unwrap().home.clone();
        let dir = home.join("agents");
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(disk) = serde_json::from_slice::<DiskAgent>(&bytes) else { continue };
            let mut agent = Agent::new(
                &disk.child_id, &disk.parent_session, &disk.run_id, &disk.name, &disk.token,
                disk.argv, disk.cwd,
            );
            agent.residency = "parked".into();
            agent.activity = "idle".into();
            agent.env = disk.env;
            agent.session_file = disk.session_file.clone();
            agent.pi_session_id = disk.pi_session_id;
            agent.cache_expires_at = disk.cache_expires_at;
            agent.cache_lifetime_ms = disk.cache_lifetime_ms;
            agent.pi_version = disk.pi_version;
            agent.last_text = disk.last_text;
            agent.driver = "parent".into();
            agent.persist(&home);
            let mut g = self.inner.lock().unwrap();
            if let Some(file) = agent.session_file.clone() {
                g.by_file.insert(file, agent.child_id.clone());
            }
            g.agents.insert(agent.child_id.clone(), agent);
        }
    }
}

fn over_cap_ids(g: &Inner) -> Vec<String> {
    let limits = limits_of(&g.home);
    let mut idle: Vec<(String, u64)> = g.agents.values()
        .filter(|a| a.residency == "warm" && a.activity != "running" && a.activity != "starting")
        .map(|a| (a.child_id.clone(), a.idle_since.unwrap_or(0)))
        .collect();
    if idle.len() <= limits.max_warm {
        return Vec::new();
    }
    idle.sort_by_key(|(_, t)| *t);
    let extra = idle.len() - limits.max_warm;
    idle.into_iter().take(extra).map(|(id, _)| id).collect()
}

fn resolve_id(g: &Inner, session_id: Option<&str>, session_file: Option<&str>) -> Option<String> {
    if let Some(s) = session_id {
        if g.agents.contains_key(s) {
            return Some(s.to_string());
        }
        if let Some(id) = g.agents.values().find(|a| a.pi_session_id.as_deref() == Some(s)).map(|a| a.child_id.clone()) {
            return Some(id);
        }
    }
    session_file.and_then(|f| g.by_file.get(f).cloned())
}

async fn wait_pid_gone(pid: u32) {
    if pid == 0 || !task::pid_alive(pid) {
        return;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while task::pid_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if task::pid_alive(pid) {
        let _ = task::signal_group(pid, task::SIGKILL);
        let hard = tokio::time::Instant::now() + Duration::from_secs(2);
        while task::pid_alive(pid) && tokio::time::Instant::now() < hard {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Agent {
    fn new(child_id: &str, parent: &str, run_id: &str, name: &str, token: &str, argv: Vec<String>, cwd: String) -> Self {
        Agent {
            child_id: child_id.to_string(),
            parent_session: parent.to_string(),
            run_id: run_id.to_string(),
            name: name.to_string(),
            token: token.to_string(),
            argv,
            cwd,
            env: HashMap::new(),
            residency: "none".into(),
            activity: "starting".into(),
            driver: "parent".into(),
            pid: 0,
            session_file: None,
            pi_session_id: None,
            cache_expires_at: None,
            cache_lifetime_ms: None,
            cache_tier: None,
            pi_version: None,
            idle_since: None,
            last_user_ms: None,
            user_spoke: false,
            last_text: String::new(),
            child_conn: None,
            watchers: HashSet::new(),
            stdin: None,
            pending: HashMap::new(),
            comms: HashMap::new(),
            stderr: String::new(),
            generation: 0,
            park_scheduled: false,
        }
    }

    fn parent_session_claimed_externally(&self) -> bool {
        self.residency == "parked" && self.driver == "external"
    }

    fn notice(&self) -> crate::proto::HostedNotice {
        crate::proto::HostedNotice {
            child_id: self.child_id.clone(),
            residency: self.residency.clone(),
            activity: self.activity.clone(),
            driver: self.driver.clone(),
            parent_session: Some(self.parent_session.clone()),
            session_file: self.session_file.clone(),
            cache_expires_at: self.cache_expires_at,
            cache_lifetime_ms: self.cache_lifetime_ms,
            pi_version: self.pi_version.clone(),
        }
    }

    fn public_json(&self) -> Value {
        json!({
            "child_id": self.child_id,
            "pid": self.pid,
            "residency": self.residency,
            "activity": self.activity,
            "driver": self.driver,
            "session_file": self.session_file,
            "session_id": self.pi_session_id,
            "cache_expires_at": self.cache_expires_at,
            "cache_lifetime_ms": self.cache_lifetime_ms,
            "pi_version": self.pi_version,
            "run_id": self.run_id,
            "name": self.name,
        })
    }

    fn state_event(&self) -> EventKind {
        EventKind::AgentState {
            child_id: self.child_id.clone(),
            residency: self.residency.clone(),
            activity: self.activity.clone(),
            driver: self.driver.clone(),
            pid: self.pid,
            session_file: self.session_file.clone(),
            cache_expires_at: self.cache_expires_at,
            session_id: self.pi_session_id.clone(),
            cache_lifetime_ms: self.cache_lifetime_ms,
            pi_version: self.pi_version.clone(),
        }
    }

    fn persist(&self, home: &Path) {
        let dir = home.join("agents");
        let _ = std::fs::create_dir_all(&dir);
        let disk = DiskAgent {
            child_id: self.child_id.clone(),
            parent_session: self.parent_session.clone(),
            run_id: self.run_id.clone(),
            name: self.name.clone(),
            token: self.token.clone(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            residency: self.residency.clone(),
            activity: self.activity.clone(),
            driver: self.driver.clone(),
            session_file: self.session_file.clone(),
            pi_session_id: self.pi_session_id.clone(),
            cache_expires_at: self.cache_expires_at,
            cache_lifetime_ms: self.cache_lifetime_ms,
            pi_version: self.pi_version.clone(),
            last_text: self.last_text.clone(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&disk) {
            let path = dir.join(format!("{}.json", self.child_id));
            if std::fs::write(&path, bytes).is_ok() {
                // The record holds provider credentials. Only the owner should read it.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
    }
}

// Sibling routing uses run ids stored on both agents. A source of `ch_…` is a
// sibling only when both share a run. Checked by the caller via `route_sibling`.
impl Hosted {
    pub fn same_run(&self, a: &str, b: &str) -> bool {
        let g = self.inner.lock().unwrap();
        match (g.agents.get(a), g.agents.get(b)) {
            (Some(x), Some(y)) => x.run_id == y.run_id,
            _ => false,
        }
    }
}

struct Spawned {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    pid: u32,
}

async fn spawn_rpc(argv: &[String], cwd: &str, env: &HashMap<String, String>) -> std::io::Result<Spawned> {
    let runner = task::runner_exe()?;
    let mut cmd = crate::sys::command_tokio(runner);
    cmd.arg("__rpc");
    cmd.args(argv);
    cmd.current_dir(cwd);
    cmd.env_clear().envs(env);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    let status_write = {
        let life = task::lifeline()?;
        let (status_read, status_write) = crate::sys::pipe_cloexec()?;
        let status_write = crate::sys::dup_cloexec_high(&status_write)?;
        crate::sys::apply_runner_setup_tokio(&mut cmd, life.read.as_raw_fd(), status_write.as_raw_fd());
        // Keep the read end alive so a write on the status pipe does not die with EPIPE.
        std::mem::forget(status_read);
        status_write
    };
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    drop(status_write); // the runner has its copy at fd 4; drop ours so the number is not reused as stdin
    let pid = child.id().unwrap_or(0);
    #[cfg(windows)]
    if let Some(handle) = child.raw_handle() {
        let _ = crate::sys::assign_job(pid, handle);
    }
    let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("stderr"))?;
    Ok(Spawned { child, stdin, stdout, stderr, pid })
}

fn rpc_line(id: &str, op: &str, text: &str, since: Option<&str>) -> String {
    let body = match op {
        "prompt" => json!({"id": id, "type": "prompt", "message": text}),
        "prompt_steer" => json!({"id": id, "type": "prompt", "message": text, "streamingBehavior": "steer"}),
        "prompt_follow" => json!({"id": id, "type": "prompt", "message": text, "streamingBehavior": "followUp"}),
        "abort" => json!({"id": id, "type": "abort"}),
        "clear_queue" => json!({"id": id, "type": "clear_queue"}),
        "get_state" => json!({"id": id, "type": "get_state"}),
        "get_last_assistant_text" => json!({"id": id, "type": "get_last_assistant_text"}),
        "get_entries" => {
            let mut v = json!({"id": id, "type": "get_entries"});
            if let Some(since) = since.filter(|s| !s.is_empty()) {
                v["since"] = json!(since);
            }
            v
        }
        "get_session_stats" => json!({"id": id, "type": "get_session_stats"}),
        "set_steering_mode" => json!({"id": id, "type": "set_steering_mode", "mode": "all"}),
        _ => json!({"id": id, "type": op, "message": text}),
    };
    body.to_string()
}

fn assistant_text(value: &Value) -> Option<String> {
    let message = value.get("message")?;
    if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?.as_array()?;
    let text: String = content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("");
    if text.is_empty() { None } else { Some(text) }
}

fn fresh_token() -> String {
    let mut buf = [0u8; 16];
    proto::random_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn limits_of(home: &Path) -> Limits {
    let mut limits = Limits { idle_min: 120_000, grace: 15_000, idle_max: 3_600_000, max_warm: 4 };
    let Ok(text) = std::fs::read_to_string(home.join("config.json")) else { return limits };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return limits };
    if let Some(sub) = v.get("subagent") {
        limits.idle_min = u64_field(sub, "idleMinMs", limits.idle_min);
        limits.grace = u64_field(sub, "idleGraceMs", limits.grace);
        limits.idle_max = u64_field(sub, "idleMaxMs", limits.idle_max);
    }
    if let Some(n) = v.get("maxWarmAgents").and_then(|n| n.as_u64()) {
        if n >= 1 { limits.max_warm = n as usize; }
    }
    limits
}

fn u64_field(v: &Value, key: &str, default: u64) -> u64 {
    v.get(key).and_then(|n| n.as_u64()).filter(|n| *n > 0).unwrap_or(default)
}

fn park_due_at(idle_since: u64, cache_expires_at: Option<u64>, limits: &Limits) -> u64 {
    let min = idle_since.saturating_add(limits.idle_min);
    let max = idle_since.saturating_add(limits.idle_max);
    let cache = cache_expires_at.map(|e| e.saturating_add(limits.grace)).unwrap_or(min);
    min.max(cache).min(max.max(min))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Closing the daemon's stdin must produce EOF in the wrapper.
    /// If `status_write` is dropped before spawn, that fd number is reused as
    /// the stdin write end and `dup2` copies it onto fd 4, so closing stdin
    /// never reaches the child.
    #[cfg(unix)]
    #[tokio::test]
    async fn closing_stdin_ends_the_rpc_wrapper() {
        let env: HashMap<String, String> = std::env::vars().collect();
        let mut spawned = spawn_rpc(&["/bin/cat".into()], "/", &env)
            .await
            .expect("spawn /bin/cat via the rpc wrapper");
        assert!(spawned.pid > 0);
        drop(spawned.stdin);
        drop(spawned.stdout);
        drop(spawned.stderr);
        let status = tokio::time::timeout(Duration::from_secs(3), spawned.child.wait())
            .await
            .expect("the rpc wrapper must exit after stdin is closed")
            .expect("wait");
        assert!(status.success(), "cat should exit 0 on stdin EOF, got {status}");
    }
}
