use super::{Delivery, State};
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) fn normalize_drop_reason(reason: &str) -> &str {
    match reason {
        "rate-limited" | "duplicate" | "hop-loop" | "hop-runaway" | "queue-full" => reason,
        _ => "",
    }
}

#[derive(Default)]
pub struct InjectionDedup {
    seen: HashSet<String>,
    order: VecDeque<String>,
    inflight: HashMap<String, tokio::sync::watch::Sender<bool>>,
}
impl InjectionDedup {
    pub(super) fn delivered(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        if !self.seen.insert(id.to_string()) {
            self.order.retain(|entry| entry != id);
        }
        self.order.push_back(id.to_string());
        if self.order.len() > 4096 {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
    }
}
struct InjectionAttempt<'a> {
    runtime: &'a super::Runtime,
    id: String,
    success: bool,
}
impl Drop for InjectionAttempt<'_> {
    fn drop(&mut self) {
        if self.id.is_empty() {
            return;
        }
        let mut state = self.runtime.state.lock().unwrap();
        if self.success {
            state.incoming.delivered(&self.id);
        }
        if let Some(done) = state.incoming.inflight.remove(&self.id) {
            done.send_replace(true);
        }
    }
}
impl super::Runtime {
    pub(super) async fn inject_local(
        &self,
        target: &serde_json::Value,
        frame: serde_json::Value,
        mode: &str,
        pinned: bool,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<String, String> {
        let mid = super::str_field(&frame, "message_id");
        let mut stop = self.stop.subscribe();
        let mut closing = self.closing.subscribe();
        if *stop.borrow() || *closing.borrow() {
            return Err("intercom is shutting down".into());
        }
        let mut attempt = loop {
            let waiter = {
                let mut state = self.state.lock().unwrap();
                if !mid.is_empty() && state.incoming.seen.contains(mid) {
                    state.incoming.delivered(mid);
                    return Ok("delivered".into());
                }
                if let Some(done) = state.incoming.inflight.get(mid) {
                    Some(done.subscribe())
                } else {
                    if !mid.is_empty() {
                        state
                            .incoming
                            .inflight
                            .insert(mid.to_string(), tokio::sync::watch::channel(false).0);
                    }
                    None
                }
            };
            match waiter {
                Some(mut done) => {
                    tokio::select! {_ = stop.changed()=>return Err("intercom is shutting down".into()),_ = closing.changed()=>return Err("intercom is shutting down".into()),_ = done.changed()=>{}}
                }
                None => {
                    break InjectionAttempt {
                        runtime: self,
                        id: mid.to_string(),
                        success: false,
                    }
                }
            }
        };
        let id = super::str_field(target, "id");
        let origin = super::str_field(&frame, "reply_to");
        let result = match super::str_field(target, "harness") {
            "pi" => if pinned {
                self.push_native_snapshot(target, frame.clone(), fence)
                    .await
            } else {
                self.push(id, frame.clone()).await
            }
            .map(|_| "delivered".to_string()),
            "claude" | "codex" => {
                self.native
                    .send(
                        target,
                        origin,
                        super::str_field(&frame, "body"),
                        mode,
                        mid,
                        fence,
                    )
                    .await
            }
            _ => Err("unsupported session harness".into()),
        };
        attempt.success = result.is_ok();
        result
    }
    pub(super) async fn native_receipt(
        &self,
        source: &serde_json::Value,
        frame: &serde_json::Value,
    ) {
        let mid = super::str_field(frame, "orig_msg_id");
        let reporter = super::str_field(source, "id");
        let status = if super::str_field(frame, "status") == "expired"
            && super::str_field(frame, "status_detail") == "refused"
        {
            "refused"
        } else {
            super::str_field(frame, "status")
        };
        let route = {
            let mut state = self.state.lock().unwrap();
            state.prune_deliveries(self.clock.now_ms());
            let admitted = state
                .messages
                .get(mid)
                .filter(|delivery| !delivery.origin.is_empty());
            if admitted.is_some_and(|delivery| delivery.target != reporter) {
                return;
            }
            admitted.is_some() && state.active(mid)
        };
        if route {
            self.receipt_with_reason(
                mid,
                reporter,
                status,
                super::str_field(frame, "drop_reason"),
            )
            .await;
            return;
        }
        self.record_status(mid, status, false);
        let mut receipt = serde_json::json!({"type":"receipt","session_id":reporter,"from":reporter,"message_id":mid,"status":status,"reply_to":mid});
        let reason = super::str_field(frame, "drop_reason");
        if matches!(
            reason,
            "rate-limited" | "duplicate" | "hop-loop" | "hop-runaway" | "queue-full"
        ) {
            receipt["drop_reason"] = serde_json::json!(reason);
        }
        let writer = self.state.lock().unwrap().relay_tx.clone();
        if let Some(writer) = writer {
            let _ = writer.send(receipt.into()).await;
        }
    }
    pub(super) async fn local_receipt(&self, mid: &str, reporter: &str, status: &str) {
        let route = {
            let mut state = self.state.lock().unwrap();
            state.prune_deliveries(self.clock.now_ms());
            let admitted = state
                .messages
                .get(mid)
                .filter(|delivery| !delivery.origin.is_empty());
            if admitted.is_some_and(|delivery| delivery.target != reporter) {
                return;
            }
            let route = admitted.is_some() && state.active(mid);
            if status == "read" && (reporter.starts_with("pi/") || reporter.starts_with("codex/")) {
                state.incoming.delivered(mid);
            }
            route
        };
        if route {
            self.receipt(mid, reporter, status).await;
            return;
        }
        self.record_status(mid, status, false);
        let writer = self.state.lock().unwrap().relay_tx.clone();
        if let Some(writer) = writer {
            let _=writer.send(serde_json::json!({"type":"receipt","from":reporter,"message_id":mid,"status":status}).into()).await;
        }
    }
}

const HISTORY_LIMIT: usize = 4096;
const ROUTE_LIMIT: usize = 1024;
const TTL_MS: u64 = 10 * 60 * 1000;

pub(super) fn rank(status: &str) -> u8 {
    match status {
        "accepted" | "queued" => 1,
        "held" | "unknown" => 2,
        "delivered" => 3,
        "read" | "refused" | "denied" | "expired" | "dropped" | "unavailable" => 4,
        _ => 0,
    }
}
impl Delivery {
    pub fn route_terminal(&self) -> bool {
        if self.origin.is_empty() {
            return false;
        }
        if self.target.starts_with("codex/")
            || self.target.starts_with("pi/") && !self.remote_native
        {
            matches!(
                self.status.as_str(),
                "read" | "denied" | "expired" | "refused" | "dropped"
            )
        } else {
            matches!(
                self.status.as_str(),
                "delivered" | "read" | "denied" | "expired" | "refused" | "dropped"
            )
        }
    }
    pub(super) fn ignores(&self, status: &str, relay: bool) -> bool {
        if rank(&self.status) == 4 {
            return status != self.status;
        }
        let requeue = relay
            && status == "queued"
            && matches!(self.status.as_str(), "accepted" | "held" | "unknown");
        if requeue {
            return false;
        }
        if !relay && self.status == "queued" && status == "accepted" {
            return true;
        }
        rank(status) < rank(&self.status) || (self.status == "held" && status == "unknown")
    }
    pub fn consider(&mut self, status: &str, relay: bool, now: u64) -> bool {
        if rank(status) == 0 || self.ignores(status, relay) {
            return false;
        }
        let changed = self.status != status;
        self.status = status.into();
        self.updated_at = now;
        if status == "held" {
            self.approval_pin = true;
        }
        if status == "delivered" && !self.route_terminal() {
            self.lease_at = now;
        }
        changed
    }
}
impl State {
    pub(super) fn record_status(
        &mut self,
        id: &str,
        status: &str,
        relay: bool,
        now: u64,
    ) -> Result<(), String> {
        if id.is_empty() || rank(status) == 0 {
            return Ok(());
        }
        self.prune_deliveries(now);
        if !self.messages.contains_key(id) {
            self.admit(id, "", "", false, false, now)?;
        }
        if let Some(delivery) = self.messages.get_mut(id) {
            delivery.consider(status, relay, now);
        }
        Ok(())
    }
    pub fn prune_deliveries(&mut self, now: u64) {
        self.history.retain(|id| {
            self.messages
                .get(id)
                .is_some_and(|d| d.status == "held" || now.saturating_sub(d.updated_at) <= TTL_MS)
        });
        for pending in [&mut self.pending_local, &mut self.pending_remote] {
            pending.retain(|id| {
                self.messages
                    .get(id)
                    .is_some_and(|d| d.approval_pin || now.saturating_sub(d.lease_at) <= TTL_MS)
            });
        }
        let retained: HashSet<String> = self
            .history
            .iter()
            .chain(self.pending_local.iter())
            .chain(self.pending_remote.iter())
            .cloned()
            .collect();
        self.messages.retain(|id, _| retained.contains(id));
    }
    pub fn admit(
        &mut self,
        id: &str,
        origin: &str,
        target: &str,
        remote: bool,
        reply_relay: bool,
        now: u64,
    ) -> Result<(), String> {
        self.prune_deliveries(now);
        if let Some(existing) = self.messages.get(id) {
            if existing.origin != origin
                || existing.target != target
                || existing.reply_relay != reply_relay
            {
                return Err("delivery identity conflict".into());
            }
            return Ok(());
        }
        let tracked = reply_relay
            || !remote
                && (target.starts_with("pi/")
                    || target.starts_with("claude/")
                    || target.starts_with("codex/")
                    || origin.starts_with("claude/"))
            || remote && origin.starts_with("claude/");
        let pending = if remote {
            &self.pending_remote
        } else {
            &self.pending_local
        };
        if tracked && pending.len() >= ROUTE_LIMIT {
            return Err("receipt tracking capacity reached".into());
        }
        if self.history.len() >= HISTORY_LIMIT {
            let oldest = self
                .history
                .iter()
                .filter_map(|id| {
                    self.messages
                        .get(id)
                        .filter(|d| rank(&d.status) == 4)
                        .map(|d| (d.updated_at, id.clone()))
                })
                .min();
            if let Some((_, id)) = oldest {
                self.history.remove(&id);
            } else {
                return Err("receipt history capacity reached".into());
            }
        }
        self.messages.insert(
            id.into(),
            Delivery {
                token: std::sync::Arc::new(()),
                origin: origin.into(),
                target: target.into(),
                native_id: String::new(),
                status: "accepted".into(),
                updated_at: now,
                lease_at: now,
                approval_pin: false,
                reply_relay,
                remote_native: remote && origin.starts_with("claude/") && !reply_relay,
                target_endpoint: None,
                origin_endpoint: None,
                reply_endpoint: None,
            },
        );
        self.history.insert(id.into());
        if tracked {
            if remote {
                self.pending_remote.insert(id.into());
            } else {
                self.pending_local.insert(id.into());
            }
        }
        Ok(())
    }
    pub fn renew_history(&mut self, id: &str, now: u64) {
        if self.history.contains(id) {
            return;
        }
        self.prune_deliveries(now);
        if self.history.len() < HISTORY_LIMIT {
            self.history.insert(id.into());
        } else if let Some(oldest) = self
            .history
            .iter()
            .filter_map(|id| {
                self.messages
                    .get(id)
                    .filter(|d| rank(&d.status) == 4)
                    .map(|d| (d.updated_at, id.clone()))
            })
            .min()
        {
            self.history.remove(&oldest.1);
            self.history.insert(id.into());
        }
    }
    pub fn active(&self, id: &str) -> bool {
        self.pending_local.contains(id) || self.pending_remote.contains(id)
    }
    pub fn commit_delivery(&mut self, id: &str, token: &std::sync::Arc<()>) {
        if self.messages.get(id).is_some_and(|delivery| {
            std::sync::Arc::ptr_eq(&delivery.token, token) && delivery.route_terminal()
        }) {
            self.pending_local.remove(id);
            self.pending_remote.remove(id);
        }
    }
    pub fn rekey_delivery(&mut self, from: &str, to: &str) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        if to.is_empty() || self.messages.contains_key(to) {
            return Err("legacy relay message identity conflict".into());
        }
        let delivery = self
            .messages
            .remove(from)
            .ok_or("legacy receipt reservation disappeared")?;
        self.messages.insert(to.into(), delivery);
        if self.history.remove(from) {
            self.history.insert(to.into());
        }
        if self.pending_local.remove(from) {
            self.pending_local.insert(to.into());
        }
        if self.pending_remote.remove(from) {
            self.pending_remote.insert(to.into());
        }
        Ok(())
    }
    pub fn forget_delivery(&mut self, id: &str) {
        self.messages.remove(id);
        self.history.remove(id);
        self.pending_local.remove(id);
        self.pending_remote.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intercom::{
        config::{Config, Identity},
        native::Native,
        Push, Runtime, Session,
    };
    use serde_json::json;
    use std::{sync::Mutex, time::Duration};
    use tokio::sync::{mpsc, watch};

    fn runtime() -> Runtime {
        Runtime {
            cfg: Config {
                local_only: true,
                mcp_listen: String::new(),
                ..Config::default()
            },
            clock: crate::clock::Clock::default(),
            identity: Identity::default(),
            machine: "receipt-retry-fixture".into(),
            stop: watch::channel(false).0,
            closing: watch::channel(false).0,
            rpc_tasks: Mutex::new(Vec::new()),
            native: Native::new(),
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
                incoming: Default::default(),
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
                mcp_status: String::new(),
                native_status: String::new(),
            }),
        }
    }
    fn session(tx: mpsc::Sender<Push>, owner: u64, epoch: u64) -> Session {
        Session {
            value: json!({"id":"pi/origin","harness":"pi"}),
            owner,
            epoch,
            tx,
            disconnect: None,
            caps: HashSet::new(),
            idle_since: None,
        }
    }
    #[tokio::test]
    async fn codex_origin_terminal_receipts_consume_without_a_pi_connection_or_model_turn() {
        let rt = runtime();
        for (mid, target, status) in [
            ("codex-pi-read", "pi/target", "read"),
            ("codex-claude-delivered", "claude/target", "delivered"),
        ] {
            rt.state
                .lock()
                .unwrap()
                .admit(mid, "codex/thread", target, false, false, rt.clock.now_ms())
                .unwrap();
            rt.receipt(mid, target, "held").await;
            assert!(rt.state.lock().unwrap().active(mid));
            rt.receipt(mid, target, status).await;
            let state = rt.state.lock().unwrap();
            assert_eq!(state.messages[mid].status, status);
            assert!(
                !state.active(mid),
                "Codex receipt delivery is a successful no-op, not a missing Pi session"
            );
        }
    }

    #[tokio::test]
    async fn definitive_webhook_400_is_dropped_for_generic_sender_not_unknown() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut size = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((_, value)) = line
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                {
                    size = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; size];
            reader.read_exact(&mut body).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["message"]["body"],
                "actual rejected webhook body"
            );
            reader
                .get_mut()
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let mut rt = runtime();
        rt.cfg.endpoints.push(super::super::config::Endpoint {
            name: "reject".into(),
            url: format!("http://{address}"),
            secret: String::new(),
            auth: "hmac".into(),
        });
        let mid = "definitive-webhook-rejection";
        let result = rt
            .send_resolved(
                "pi/origin",
                json!({"id":"webhook/reject","harness":"webhook"}),
                "actual rejected webhook body",
                "",
                mid,
                None,
                None,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(result["status"], "dropped");
        assert_eq!(rt.state.lock().unwrap().messages[mid].status, "dropped");
    }

    #[tokio::test]
    async fn rearm_retries_the_original_terminal_delivered_instead_of_considering_a_later_read() {
        let rt = runtime();
        let mid = "terminal-delivered-rearm";
        let target = "claude/target";
        let (dead_tx, dead_rx) = mpsc::channel(4);
        drop(dead_rx);
        {
            let mut state = rt.state.lock().unwrap();
            state
                .sessions
                .insert("pi/origin".into(), session(dead_tx, 1, 1));
            state
                .admit(mid, "pi/origin", target, false, false, rt.clock.now_ms())
                .unwrap();
        }
        rt.receipt(mid, target, "delivered").await;
        {
            let state = rt.state.lock().unwrap();
            assert!(state.active(mid));
            assert_eq!(state.messages[mid].status, "delivered");
        }
        let (tx, mut rx) = mpsc::channel(4);
        rt.state
            .lock()
            .unwrap()
            .sessions
            .insert("pi/origin".into(), session(tx, 2, 2));
        let observe = async {
            let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            frame.written.unwrap().send(Ok(())).unwrap();
            frame.value
        };
        let (_, frame) = tokio::join!(rt.receipt(mid, target, "read"), observe);
        assert_eq!(
            frame["status"], "delivered",
            "rearm must physically render the first canonical terminal status"
        );
        let state = rt.state.lock().unwrap();
        assert_eq!(state.messages[mid].status, "delivered");
        assert!(!state.active(mid));
    }

    #[tokio::test]
    async fn old_held_written_ack_does_not_consume_a_new_failed_read_callback() {
        let rt = runtime();
        let mid = "held-is-not-terminal-consume";
        let target = "pi/target";
        let (tx, mut rx) = mpsc::channel(4);
        {
            let mut state = rt.state.lock().unwrap();
            state.sessions.insert("pi/origin".into(), session(tx, 1, 1));
            state
                .admit(mid, "pi/origin", target, false, false, rt.clock.now_ms())
                .unwrap();
        }
        let held = rt.receipt(mid, target, "held");
        tokio::pin!(held);
        let old = tokio::select! {frame=rx.recv()=>frame.unwrap(),_=&mut held=>panic!("held callback must wait for actual-written proof")};
        assert_eq!(old.value["status"], "held");
        let (dead_tx, dead_rx) = mpsc::channel(4);
        drop(dead_rx);
        rt.state
            .lock()
            .unwrap()
            .sessions
            .insert("pi/origin".into(), session(dead_tx, 2, 2));
        rt.receipt(mid, target, "read").await;
        assert!(rt.state.lock().unwrap().active(mid));
        old.written.unwrap().send(Ok(())).unwrap();
        held.await;
        assert!(
            rt.state.lock().unwrap().active(mid),
            "successful old held must not consume the unrendered canonical read"
        );
        let (tx, mut retry) = mpsc::channel(4);
        rt.state
            .lock()
            .unwrap()
            .sessions
            .insert("pi/origin".into(), session(tx, 3, 3));
        let observe = async {
            let frame = retry.recv().await.unwrap();
            assert_eq!(frame.value["status"], "read");
            frame.written.unwrap().send(Ok(())).unwrap();
        };
        tokio::join!(rt.receipt(mid, target, "read"), observe);
        assert!(!rt.state.lock().unwrap().active(mid));
    }

    #[cfg(feature = "test-clock")]
    #[tokio::test]
    async fn old_terminal_written_ack_cannot_consume_a_new_record_with_the_same_id_after_ttl() {
        let mut rt = runtime();
        rt.clock = crate::clock::Clock::manual_for_test();
        let mid = "terminal-record-identity";
        let (tx, mut old_rx) = mpsc::channel(4);
        {
            let mut state = rt.state.lock().unwrap();
            state.sessions.insert("pi/origin".into(), session(tx, 1, 1));
            state
                .admit(
                    mid,
                    "pi/origin",
                    "pi/old-target",
                    false,
                    false,
                    rt.clock.now_ms(),
                )
                .unwrap();
        }
        let old = rt.receipt(mid, "pi/old-target", "read");
        tokio::pin!(old);
        let old_frame = tokio::select! {frame=old_rx.recv()=>frame.unwrap(),_=&mut old=>panic!("old terminal callback must still be pending")};
        rt.clock.manual().unwrap().advance(TTL_MS + 1);
        let (tx, mut new_rx) = mpsc::channel(4);
        {
            let mut state = rt.state.lock().unwrap();
            state.prune_deliveries(rt.clock.now_ms());
            assert!(!state.messages.contains_key(mid));
            state.sessions.insert("pi/origin".into(), session(tx, 2, 2));
            state
                .admit(
                    mid,
                    "pi/origin",
                    "pi/new-target",
                    false,
                    false,
                    rt.clock.now_ms(),
                )
                .unwrap();
        }
        let new = rt.receipt(mid, "pi/new-target", "read");
        tokio::pin!(new);
        let new_frame = tokio::select! {frame=new_rx.recv()=>frame.unwrap(),_=&mut new=>panic!("new terminal callback must wait for its own proof")};
        old_frame.written.unwrap().send(Ok(())).unwrap();
        old.await;
        {
            let state = rt.state.lock().unwrap();
            assert_eq!(state.messages[mid].target, "pi/new-target");
            assert!(
                state.active(mid),
                "old terminal ACK must not consume a new pending record"
            );
        }
        new_frame.written.unwrap().send(Ok(())).unwrap();
        new.await;
        assert!(!rt.state.lock().unwrap().active(mid));
    }

    #[tokio::test]
    async fn nonterminal_delivered_updates_query_and_lease_without_rendering_a_duplicate_receipt() {
        let rt = runtime();
        let mid = "delivered-is-not-read";
        let target = "pi/target";
        let (tx, mut rx) = mpsc::channel(4);
        {
            let mut state = rt.state.lock().unwrap();
            state.sessions.insert("pi/origin".into(), session(tx, 1, 1));
            state
                .admit(mid, "pi/origin", target, false, false, rt.clock.now_ms())
                .unwrap();
        }
        let mut lease = rt.state.lock().unwrap().messages[mid].lease_at;
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let observe = async {
                match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                    Ok(Some(frame)) => {
                        frame.written.unwrap().send(Ok(())).unwrap();
                        Some(frame.value)
                    }
                    _ => None,
                }
            };
            let (_, frame) = tokio::join!(rt.receipt(mid, target, "delivered"), observe);
            assert!(
                frame.is_none(),
                "Go suppresses canonical nonterminal delivered callbacks: {frame:?}"
            );
            let state = rt.state.lock().unwrap();
            assert_eq!(state.messages[mid].status, "delivered");
            assert!(state.active(mid));
            assert!(
                state.messages[mid].lease_at > lease,
                "nonterminal delivered must renew the receipt lease"
            );
            lease = state.messages[mid].lease_at;
        }
        let receive = async {
            let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            frame.written.unwrap().send(Ok(())).unwrap();
            frame.value
        };
        let (_, frame) = tokio::join!(rt.receipt(mid, target, "read"), receive);
        assert_eq!(frame["status"], "read");
        assert!(!rt.state.lock().unwrap().active(mid));
    }

    #[tokio::test]
    async fn failed_held_receipt_retries_after_origin_reconnect_and_terminal_commit_consumes_route()
    {
        let rt = runtime();
        let mid = "receipt-held-reconnect";
        let target = "pi/target";
        let (dead_tx, dead_rx) = mpsc::channel(4);
        drop(dead_rx);
        {
            let mut state = rt.state.lock().unwrap();
            state
                .sessions
                .insert("pi/origin".into(), session(dead_tx, 1, 1));
            state
                .admit(mid, "pi/origin", target, false, false, rt.clock.now_ms())
                .unwrap();
        }
        rt.receipt(mid, target, "held").await;
        {
            let state = rt.state.lock().unwrap();
            assert_eq!(state.messages[mid].status, "held");
            assert!(
                state.active(mid),
                "failed callback must retain its user route"
            );
        }
        let (tx, mut rx) = mpsc::channel(4);
        {
            let mut state = rt.state.lock().unwrap();
            state.sessions.remove("pi/origin");
            state.sessions.insert("pi/origin".into(), session(tx, 2, 2));
        }
        rt.receipt(mid, "pi/forged-reporter", "held").await;
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        for status in ["held", "held", "read"] {
            let receive = async {
                let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                    .await
                    .expect("same legitimate status must retry a pending callback")
                    .unwrap();
                frame.written.unwrap().send(Ok(())).unwrap();
                frame.value
            };
            let (_, frame) = tokio::join!(rt.receipt(mid, target, status), receive);
            assert_eq!(
                frame,
                json!({"type":"receipt","from":target,"status":status,"message_id":mid})
            );
        }
        assert!(!rt.state.lock().unwrap().active(mid));
        rt.receipt(mid, target, "held").await;
        rt.receipt(mid, target, "dropped").await;
        assert_eq!(rt.state.lock().unwrap().messages[mid].status, "read");
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
