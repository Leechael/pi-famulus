//! The per-connection intercom port: one client's session registration and
//! ops against the bridge runtime, shared by the legacy client socket and the
//! famulus manager connection (`intercom` request, protocol 7).
//!
//! Extracted from `localrpc.rs` (pi-famulus client protocol) so the
//! famulus manager connection serves the same protocol over manager.sock
//! without a second socket. The manager variant binds the registration to
//! the hello session identity and marks its connection ids in
//! [`State::port_conns`] so a legacy client socket cannot claim the same
//! session (and the manager takes a legacy-owned id over instead).

use super::{str_field, Push, Session, Shared};
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::sync::{mpsc, watch};

/// Who owns the port: a legacy `client.sock` connection, or the famulus
/// manager connection itself.
pub enum PortOrigin {
    /// The legacy local socket (old plugins, the local CLI).
    Client,
    /// The famulus manager connection: `register` is bound to the hello
    /// session identity; `pi_pid` fills the (publicly stripped) pid.
    Manager { session_id: String, pi_pid: u32 },
}

/// One connection's intercom session: the ops of the pi-famulus client
/// protocol plus end-of-life cleanup identical to a client socket
/// disconnect (remove the owned session, settle its idle watchers, unpublish
/// from native adapters).
pub struct Port {
    rt: Shared,
    /// The bridge-runtime connection id (`State::next_conn` space; session
    /// ownership and native alias generations key on it).
    pub conn: u64,
    /// Frames for this session (`deliver`, `receipt`, `idle_notice`, …).
    /// The socket side drains it resolving `Push::written` after the write;
    /// the manager side relays it as `intercom` events with the same ack.
    pub tx: mpsc::Sender<Push>,
    /// Fired when the session is unpublished remotely (`session_unpublish`),
    /// telling the socket/relay side to stop serving this port.
    disconnect: watch::Sender<bool>,
    /// The session id this connection registered, empty before register.
    bound: String,
    origin: PortOrigin,
}

impl Port {
    pub fn new(rt: Shared, tx: mpsc::Sender<Push>, origin: PortOrigin) -> Port {
        let conn = {
            let mut s = rt.state.lock().unwrap();
            s.next_conn += 1;
            s.next_conn
        };
        if matches!(origin, PortOrigin::Manager { .. }) {
            rt.state.lock().unwrap().port_conns.insert(conn);
        }
        let (disconnect, _) = watch::channel(false);
        Port {
            rt,
            conn,
            tx,
            disconnect,
            bound: String::new(),
            origin,
        }
    }

    /// A receiver fired when the port's session is unpublished remotely.
    pub fn disconnected(&self) -> watch::Receiver<bool> {
        self.disconnect.subscribe()
    }

    /// Handle one client-protocol op. Mirrors the legacy client socket:
    /// `Ok` values are wrapped with `ok: true`, `Err` becomes `{"error": e}`.
    pub async fn handle(&mut self, req: &Value) -> Result<Value, String> {
        let conn = self.conn;
        let op = str_field(req, "op");
        // The manager connection serves exactly the old pi plugin's surface.
        // Before register, only registration and read-only lookups; spoofed
        // origins (reply_to/as/inbound/session ops) are not available at all.
        if matches!(self.origin, PortOrigin::Manager { .. }) {
            // `as` would name an arbitrary watcher; a manager connection
            // only ever watches as its own bound session.
            if !str_field(req, "as").is_empty() {
                return Err("op as= is not available to a pi session over the manager connection".into());
            }
            let allowed: &[&str] = if self.bound.is_empty() {
                &["register", "list"]
            } else {
                &[
                    "register",
                    "unregister",
                    "list",
                    "send",
                    "receipt",
                    "subscribe_idle",
                ]
            };
            if !allowed.contains(&op) {
                return Err(if self.bound.is_empty() {
                    "session not registered".to_string()
                } else {
                    format!("op {op:?} is not available to a pi session over the manager connection")
                });
            }
        }
        if !self.bound.is_empty()
            && ["send", "receipt", "subscribe_idle", "inbound"].contains(&op)
            && self
                .rt
                .state
                .lock()
                .unwrap()
                .sessions
                .get(&self.bound)
                .is_none_or(|v| v.owner != conn)
        {
            return Err("session ownership ended".into());
        }
        match op {
            "register" => {
                let mut v = req["session"].clone();
                let id = str_field(&v, "id");
                if id.is_empty() {
                    return Err("session.id required".into());
                }
                let id = if id.starts_with("pi/") {
                    id.to_string()
                } else {
                    format!("pi/{id}")
                };
                match &self.origin {
                    PortOrigin::Manager { session_id, pi_pid } => {
                        // The manager connection speaks for exactly one pi
                        // session: register is bound to the hello session
                        // id, so one connection cannot pose as another.
                        if id != format!("pi/{session_id}") {
                            return Err(format!(
                                "session id mismatch: this connection is bound to pi/{session_id}"
                            ));
                        }
                        v["pid"] = json!(pi_pid);
                    }
                    // Client.sock coexistence is enforced under the state
                    // lock below, together with the takeover.
                    PortOrigin::Client => {}
                }
                v["id"] = json!(id);
                v["harness"] = json!("pi");
                if str_field(&v, "status").is_empty() {
                    v["status"] = json!("idle");
                }
                let caps: HashSet<String> = v["caps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                v["notify_idle"] = json!(caps.contains("idle_state_v1"));
                if v.get("name").is_none() {
                    v["name"] = json!("");
                }
                if v.get("cwd").is_none() {
                    v["cwd"] = json!("");
                }
                v.as_object_mut().unwrap().remove("caps");
                let mut cancelled = Vec::new();
                let mut absences = Vec::new();
                {
                    let mut s = self.rt.state.lock().unwrap();
                    if s.unpublished.get(&id).is_some_and(|(_, blocked)| *blocked) {
                        return Err(
                            "session is unpublished; restore it with the local CLI".into()
                        );
                    }
                    if let PortOrigin::Client = self.origin {
                        // Coexistence with the famulus extension: a session
                        // served over the manager socket is not claimable by
                        // the legacy socket (the extension wins in either
                        // order, with no flapping). Checked under the same
                        // lock as the takeover below.
                        if s.sessions
                            .get(&id)
                            .map(|session| session.owner)
                            .is_some_and(|owner| s.port_conns.contains(&owner))
                        {
                            return Err(format!(
                                "session {id} is served by the pi-famulus extension over the manager socket"
                            ));
                        }
                    }
                    if !self.bound.is_empty()
                        && *self.bound != id
                        && s.sessions
                            .get(&self.bound)
                            .is_some_and(|session| session.owner == conn)
                    {
                        if let Some(old) = s.sessions.remove(&self.bound) {
                            cancelled.push((self.bound.clone(), old.epoch));
                            absences.push((self.bound.clone(), old.owner, old.epoch));
                        }
                        s.exited.insert(self.bound.clone());
                    }
                    let previous = s
                        .sessions
                        .get(&id)
                        .filter(|session| session.owner == conn)
                        .map(|session| (session.epoch, session.idle_since));
                    let (epoch, idle_since) = if let Some((epoch, idle)) = previous {
                        (
                            epoch,
                            if str_field(&v, "status") == "idle" {
                                idle.or(Some(self.rt.clock.now_ms()))
                            } else {
                                None
                            },
                        )
                    } else {
                        if let Some(old) = s.sessions.get(&id) {
                            cancelled.push((id.clone(), old.epoch));
                        }
                        s.next_epoch += 1;
                        (
                            s.next_epoch,
                            if str_field(&v, "status") == "idle" {
                                Some(self.rt.clock.now_ms())
                            } else {
                                None
                            },
                        )
                    };
                    s.unpublished.remove(&id);
                    s.exited.remove(&id);
                    s.sessions.insert(
                        id.clone(),
                        Session {
                            value: v,
                            owner: conn,
                            epoch,
                            tx: self.tx.clone(),
                            disconnect: Some(self.disconnect.clone()),
                            caps,
                            idle_since,
                        },
                    );
                }
                self.bound = id.clone();
                for (watcher, epoch) in cancelled {
                    self.rt.cancel_idle_watcher(&watcher, epoch).await;
                }
                for (id, owner, epoch) in absences {
                    self.rt.native.source_unpublished(&id, owner, epoch).await;
                }
                Ok(json!({"session_id":id}))
            }
            "unregister" => {
                let removed = {
                    let mut state = self.rt.state.lock().unwrap();
                    if state
                        .sessions
                        .get(&self.bound)
                        .is_some_and(|session| session.owner == conn)
                    {
                        let removed = state.sessions.remove(&self.bound);
                        state.exited.insert(self.bound.clone());
                        removed.map(|session| (self.bound.clone(), session.epoch))
                    } else {
                        None
                    }
                };
                self.bound.clear();
                if let Some((watcher, epoch)) = removed {
                    self.rt.cancel_idle_watcher(&watcher, epoch).await;
                    self.rt.native.source_unpublished(&watcher, conn, epoch).await;
                }
                Ok(json!({}))
            }
            "list" => {
                let mut rows = self.rt.directory().await;
                if req["all"] != true {
                    rows.retain(|r| r["all"] != true);
                }
                Ok(json!({"sessions":rows}))
            }
            "send" => {
                let mut reply = str_field(req, "reply_to").to_string();
                if self.bound.is_empty() && reply.is_empty() && !str_field(req, "as").is_empty()
                {
                    let origin = str_field(req, "as");
                    reply = if origin.starts_with("webhook/") {
                        origin.into()
                    } else {
                        format!("webhook/{origin}")
                    };
                }
                let result = self
                    .rt
                    .send(
                        &self.bound,
                        str_field(req, "to"),
                        str_field(req, "body"),
                        &reply,
                        str_field(req, "from_mode"),
                    )
                    .await?;
                Ok(result)
            }
            "inbound" => {
                if !req["session"].is_object() {
                    return Ok(json!({}));
                }
                let raw = str_field(&req["session"], "id");
                let id = if raw.starts_with("pi/") {
                    raw.to_string()
                } else {
                    format!("pi/{raw}")
                };
                if self.rt.state.lock().unwrap().unpublished.contains_key(&id) {
                    return Err("session is unpublished".into());
                }
                let mid = uuid::Uuid::new_v4().to_string();
                let result=super::relay::call(&self.rt,json!({"type":"inbound","message_id":mid,"session_id":id,"from":id,"body":req["body"].as_str().unwrap_or(""),"reply_to":req["reply_to"].as_str().unwrap_or("")})).await;
                match result {
                    Ok(response)
                        if response["type"] == "inbound"
                            && response["message_id"] == mid
                            && matches!(str_field(&response, "status"), "accepted" | "queued") =>
                    {
                        self.rt
                            .record_status(&mid, str_field(&response, "status"), true);
                    }
                    Ok(_) => self.rt.record(
                        "error",
                        &mid,
                        &id,
                        "protocol error: invalid inbound acknowledgement",
                    ),
                    Err(error) => {
                        self.rt.record_status(
                            &mid,
                            if error.contains("not connected") {
                                "unavailable"
                            } else {
                                "unknown"
                            },
                            true,
                        );
                        self.rt.record("error", &mid, &id, &error);
                    }
                }
                Ok(json!({}))
            }
            "receipt" => {
                if self.bound.is_empty() {
                    return Err("receipt requires a registered session".into());
                }
                let status = str_field(req, "status");
                if !["read", "dropped"].contains(&status) || str_field(req, "message_id").is_empty()
                {
                    return Err("message_id required; status must be read or dropped".into());
                }
                self.rt
                    .local_receipt(str_field(req, "message_id"), &self.bound, status)
                    .await;
                Ok(json!({}))
            }
            "subscribe_idle" => {
                self.rt
                    .subscribe(
                        if self.bound.is_empty() {
                            str_field(req, "as")
                        } else {
                            &self.bound
                        },
                        str_field(req, "to"),
                    )
                    .await
            }
            "idle_status" => {
                let mut s = self.rt.state.lock().unwrap();
                s.subscriptions.retain(|_, w| {
                    w.status == "subscribed"
                        || self.rt.clock.now_ms().saturating_sub(w.updated_at) <= 12 * 3600 * 1000
                });
                let w = s
                    .subscriptions
                    .get(str_field(req, "subscription_id"))
                    .ok_or("unknown subscription id")?;
                if !self.bound.is_empty() && self.bound != w.watcher {
                    return Err("subscription belongs to another session".into());
                }
                Ok(json!({"subscription_id":req["subscription_id"],"status":w.status,"from":w.target,"delivery_mode":w.mode}))
            }
            "session_registrations" | "session_unpublish" | "session_restore" => {
                if !self.bound.is_empty() {
                    return Err(
                        "session publication operations require an unregistered local connection"
                            .into(),
                    );
                }
                if !str_field(req, "machine").is_empty() {
                    return Err("session publication operations are local-only".into());
                }
                let absences = {
                    let mut absences = Vec::new();
                    let mut s = self.rt.state.lock().unwrap();
                    if op == "session_registrations" {
                        let mut rows: Vec<_> = s.sessions.values().map(|v| v.value.clone()).collect();
                        if req["all"] == true {
                            for (v, blocked) in s.unpublished.values() {
                                let mut v = v.clone();
                                v["status"] = json!(if *blocked { "unpublished" } else { "restoring" });
                                rows.push(v);
                            }
                        }
                        rows.sort_by(|a, b| str_field(a, "id").cmp(str_field(b, "id")));
                        return Ok(json!({"sessions":rows}));
                    }
                    let ids = req["args"]
                        .as_array()
                        .ok_or("args must contain 1..128 exact pi/ session IDs")?;
                    if ids.is_empty() || ids.len() > 128 {
                        return Err("args must contain 1..128 exact pi/ session IDs".into());
                    }
                    let mut unique = HashSet::new();
                    for id in ids {
                        let id = id
                            .as_str()
                            .ok_or("invalid local Pi session ID; use an exact full pi/ ID")?;
                        if !id.starts_with("pi/")
                            || id.len() <= 3
                            || id.chars().any(|c| c.is_whitespace() || c.is_control())
                        {
                            return Err(
                                "invalid local Pi session ID; use an exact full pi/ ID".into()
                            );
                        }
                        if !s.sessions.contains_key(id) && !s.unpublished.contains_key(id) {
                            return Err(format!("unknown local Pi registration {id:?}"));
                        }
                        unique.insert(id.to_string());
                    }
                    let additional = unique
                        .iter()
                        .filter(|id| !s.unpublished.contains_key(*id))
                        .count();
                    if op == "session_unpublish" && s.unpublished.len() + additional > 1024 {
                        return Err("unpublished registration capacity reached".into());
                    }
                    for id in unique {
                        if op == "session_restore" {
                            if let Some((_, blocked)) = s.unpublished.get_mut(&id) {
                                *blocked = false;
                            }
                            s.exited.remove(&id);
                        } else {
                            if let Some(v) = s.sessions.remove(&id) {
                                absences.push((id.clone(), v.owner, v.epoch, v.disconnect));
                                s.unpublished.insert(id.clone(), (v.value, true));
                            } else if let Some((_, blocked)) = s.unpublished.get_mut(&id) {
                                *blocked = true;
                            }
                            s.exited.remove(&id);
                        }
                    }
                    absences
                };
                for (id, owner, epoch, disconnect) in absences {
                    self.rt.cancel_idle_watcher(&id, epoch).await;
                    self.rt.native.source_unpublished(&id, owner, epoch).await;
                    if let Some(disconnect) = disconnect {
                        let _ = disconnect.send(true);
                    }
                }
                Ok(json!({}))
            }
            "message_status" => {
                let mut s = self.rt.state.lock().unwrap();
                s.prune_deliveries(self.rt.clock.now_ms());
                if !s.history.contains(str_field(req, "message_id")) {
                    return Ok(json!({"message_id":req["message_id"],"status":"unknown"}));
                }
                let d = s
                    .messages
                    .get(str_field(req, "message_id"))
                    .ok_or("unknown message id")?;
                Ok(json!({"message_id":req["message_id"],"status":d.status,"updated_at":d.updated_at}))
            }
            "session_name" => {
                let row = self.rt.resolve(str_field(req, "to")).await?;
                let id = str_field(&row, "id");
                let name = str_field(req, "name");
                if name.is_empty() {
                    return Err("name required".into());
                }
                self.rt.native.rename(&row, name).await?;
                self.rt.state.lock().unwrap().aliases.insert(id.into(), name.into());
                Ok(json!({}))
            }
            "status" => {
                let machine = str_field(req, "machine");
                if !machine.is_empty() && machine != self.rt.machine {
                    super::relay::call(&self.rt, json!({"type":"status","machine_id":machine})).await
                } else {
                    Ok(json!({"report":self.rt.status_report().await}))
                }
            }
            "workspace_list" | "workspace_create" | "workspace_close" | "session_create"
            | "session_close" | "agent_prompt" | "agent_keys" | "agent_read" | "agent_wait" => {
                super::native::herdr(&self.rt, req).await
            }
            _ => Err(format!("unknown op {op:?}")),
        }
    }

    /// End-of-life cleanup, identical to a client socket disconnect: remove
    /// the owned session, settle its idle watchers, unpublish from native
    /// adapters. Idempotent. Returns the session id when this close removed
    /// it (the caller may settle that session's unread deliveries).
    pub async fn close(&mut self) -> Option<String> {
        if matches!(self.origin, PortOrigin::Manager { .. }) {
            self.rt.state.lock().unwrap().port_conns.remove(&self.conn);
        }
        let removed = {
            let mut state = self.rt.state.lock().unwrap();
            if !self.bound.is_empty()
                && state
                    .sessions
                    .get(&self.bound)
                    .is_some_and(|session| session.owner == self.conn)
            {
                state
                    .sessions
                    .remove(&self.bound)
                    .map(|session| (self.bound.clone(), session.epoch))
            } else {
                None
            }
        };
        let _ = self.disconnect.send(true);
        if let Some((id, epoch)) = removed {
            self.rt.cancel_idle_watcher(&id, epoch).await;
            self.rt.native.source_unpublished(&id, self.conn, epoch).await;
            self.bound.clear();
            return Some(id);
        }
        self.bound.clear();
        None
    }
}
