use super::*;
use futures_util::StreamExt;

const IDLE_TTL: u64 = 12 * 3600 * 1000;
const IDLE_RECORDS: usize = 1024;
const IDLE_PENDING: usize = 32;
const IDLE_PER_WATCHER: usize = 4;
const IDLE_FLUSH: Duration = Duration::from_secs(5);

struct Admission {
    status: String,
    admitted: bool,
    previous: Option<(String, Idle)>,
}
struct AdmissionPin<'a> {
    runtime: &'a Runtime,
    id: String,
}
impl Drop for AdmissionPin<'_> {
    fn drop(&mut self) {
        if let Some(record) = self
            .runtime
            .state
            .lock()
            .unwrap()
            .subscriptions
            .get_mut(&self.id)
        {
            record.admitting = false;
        }
    }
}
fn sweep(records: &mut HashMap<String, Idle>, now: u64) {
    records.retain(|_, record| {
        record.status == "subscribed"
            || record.admitting
            || now.saturating_sub(record.updated_at) <= IDLE_TTL
    });
    while records.len() >= IDLE_RECORDS {
        let oldest = records
            .iter()
            .filter(|(_, record)| record.status != "subscribed" && !record.admitting)
            .map(|(id, record)| (record.updated_at, id.clone()))
            .min();
        let Some((_, id)) = oldest else {
            break;
        };
        records.remove(&id);
    }
}
fn clear_stable(stable: &mut HashMap<String, u64>, target: &str) {
    stable.retain(|key, _| key.split_once('\0').is_none_or(|(id, _)| id != target));
}
fn admit(
    records: &mut HashMap<String, Idle>,
    stable: &mut HashMap<String, u64>,
    id: &str,
    record: Idle,
    now: u64,
) -> Result<Admission, String> {
    if let Some(old) = records.get(id) {
        if old.watcher != record.watcher
            || old.target != record.target
            || old.remote_origin != record.remote_origin
            || old.remote_machine != record.remote_machine
            || old.native != record.native
            || old.epoch != record.epoch
            || old.source_identity != record.source_identity
        {
            return Err("conflicting idle subscription id".into());
        }
        return Ok(Admission {
            status: old.status.clone(),
            admitted: false,
            previous: None,
        });
    }
    sweep(records, now);
    if records.len() >= IDLE_RECORDS {
        return Err("idle result capacity reached".into());
    }
    let previous = if record.native && !record.source_identity.is_empty() {
        records
            .iter()
            .find(|(_, old)| {
                old.native
                    && old.status == "subscribed"
                    && old.watcher == record.watcher
                    && old.target == record.target
                    && old.source_identity == record.source_identity
            })
            .map(|(id, old)| (id.clone(), old.clone()))
    } else {
        None
    };
    let discount = usize::from(previous.is_some());
    if records
        .values()
        .filter(|record| record.status == "subscribed")
        .count()
        - discount
        >= IDLE_PENDING
    {
        return Err("idle: subscription capacity reached".into());
    }
    if records
        .values()
        .filter(|old| old.status == "subscribed" && old.watcher == record.watcher)
        .count()
        - discount
        >= IDLE_PER_WATCHER
    {
        return Err("idle: watcher subscription capacity reached".into());
    }
    let previous = previous.map(|(id, mut old)| {
        old.status = "cancelled".into();
        old.updated_at = now;
        old.superseded = true;
        records.insert(id.clone(), old.clone());
        (id, old)
    });
    clear_stable(stable, &record.target);
    records.insert(id.into(), record);
    Ok(Admission {
        status: "subscribed".into(),
        admitted: true,
        previous,
    })
}
fn terminal(records: &mut HashMap<String, Idle>, id: &str, status: &str, now: u64) -> Option<Idle> {
    let record = records
        .get_mut(id)
        .filter(|record| record.status == "subscribed")?;
    record.status = if now.saturating_sub(record.created) >= IDLE_TTL {
        "expired"
    } else {
        status
    }
    .into();
    record.updated_at = now;
    Some(record.clone())
}
fn session_state(status: &str) -> &'static str {
    match status {
        "busy" | "active" | "running" => "busy",
        "idle" | "shell" => "idle",
        "exited" => "exited",
        "waiting" | "systemError" | "systemerror" | "error" => "waiting",
        _ => "unavailable",
    }
}
fn settled(
    stable: &mut HashMap<String, u64>,
    target: &str,
    identity: &str,
    observation: &str,
    now: u64,
) -> bool {
    let key = format!("{target}\0{identity}");
    stable.retain(|candidate, _| {
        candidate
            .split_once('\0')
            .is_none_or(|(id, _)| id != target)
            || *candidate == key
    });
    if observation != "idle" {
        clear_stable(stable, target);
        return observation == "exited";
    }
    let since = stable.entry(key).or_insert(now);
    now.saturating_sub(*since) >= 750
}
fn notice_correlated(record: &Idle, frame: &Value) -> bool {
    record.backend == "remote"
        && !record.remote_machine.is_empty()
        && record.remote_machine == str_field(frame, "origin_machine")
        && record.target == str_field(frame, "from")
        && record.watcher == str_field(frame, "watcher")
}
fn cancel_correlated(record: &Idle, frame: &Value) -> bool {
    !record.remote_origin.is_empty()
        && record.remote_origin == str_field(frame, "origin_machine")
        && record.target == str_field(frame, "session_id")
        && record.watcher == str_field(frame, "watcher")
}
fn cancel_record(
    records: &mut HashMap<String, Idle>,
    id: &str,
    frame: &Value,
    now: u64,
) -> Result<Option<Idle>, String> {
    let Some(record) = records.get(id) else {
        return Err("uncorrelated idle cancellation".into());
    };
    if !cancel_correlated(record, frame) {
        return Err("uncorrelated idle cancellation".into());
    }
    let record = record.clone();
    Ok(Some(
        terminal(records, id, "unavailable", now).unwrap_or(record),
    ))
}
fn target_row(rows: &[Value], target: &str) -> Result<Value, String> {
    if let Some(row) = rows.iter().find(|row| str_field(row, "id") == target) {
        return Ok(row.clone());
    }
    let mut matches = rows.iter().filter(|row| str_field(row, "name") == target);
    let row = matches
        .next()
        .cloned()
        .ok_or_else(|| format!("unknown idle target {target:?}"))?;
    if matches.any(|other| other["id"] != row["id"]) {
        return Err(format!("target {target:?} is ambiguous"));
    }
    Ok(row)
}
fn terminal_needs_cancel(record: &Idle) -> bool {
    record.backend == "native" || (record.backend == "remote" && record.status == "expired")
}
fn remote_ack(reply: &Value, id: &str) -> Result<Option<&'static str>, String> {
    if str_field(reply, "type") != "idle_subscribe" || str_field(reply, "subscription_id") != id {
        return Err("uncorrelated target idle subscription acknowledgement".into());
    }
    match str_field(reply, "status") {
        "subscribed" => Ok(None),
        "idle" => Ok(Some("idle")),
        "exited" => Ok(Some("exited")),
        "unavailable" => Ok(Some("unavailable")),
        status => Err(format!(
            "invalid target idle subscription status {status:?}"
        )),
    }
}

impl Runtime {
    pub async fn subscribe(&self, watcher: &str, target: &str) -> Result<Value, String> {
        self.start_idle(
            &uuid::Uuid::new_v4().to_string(),
            watcher,
            target,
            false,
            "",
            "",
        )
        .await
    }
    pub async fn start_idle(
        &self,
        id: &str,
        watcher: &str,
        target: &str,
        native: bool,
        relay_origin: &str,
        source_identity: &str,
    ) -> Result<Value, String> {
        if *self.closing.borrow() {
            return Err("intercom is shutting down".into());
        }
        if watcher.is_empty() || target.is_empty() || watcher == target {
            return Err("idle watch requires distinct watcher and target session ids".into());
        }
        if uuid::Uuid::parse_str(id).is_err() {
            return Err("idle subscription id must be a UUID".into());
        }
        let rows = self.directory().await;
        let row = target_row(&rows, target)?;
        let target = str_field(&row, "id").to_string();
        let origin = rows
            .iter()
            .find(|row| str_field(row, "id") == watcher && self.local_agent(row));
        if origin.is_none() && relay_origin.is_empty() {
            return Err(format!("watcher {watcher:?} is not a local session"));
        }
        if watcher == target {
            return Err("cannot subscribe to yourself".into());
        }
        let harness = str_field(&row, "harness");
        if !["claude", "pi", "codex"].contains(&harness) {
            return Err(format!("target {target} has no observable agent lifecycle"));
        }
        if harness == "pi" && row["notify_idle"] != true {
            return Err(
                "Pi target lacks settled idle state reporting; upgrade its intercom plugin".into(),
            );
        }
        if row["all"] == true || ["offline", "notLoaded"].contains(&str_field(&row, "status")) {
            return Err(format!("target {target} is not available"));
        }
        let machine = str_field(&row, "machine_id");
        let remote = !machine.is_empty() && machine != self.machine;
        if remote && !relay_origin.is_empty() {
            return Err("idle watch target is not local to this daemon".into());
        }
        if remote {
            if row["notify_idle"] != true {
                return Err("target daemon does not advertise idle subscriptions; upgrade both daemons and relay".into());
            }
            if self.cfg.remote_messages == "refuse" {
                return Err("cross-machine operation refused: remote_messages=refuse".into());
            }
            if watcher.starts_with("claude/") {
                self.native.check_remote_source(watcher)?;
            }
        } else if !relay_origin.is_empty() && self.cfg.remote_messages == "refuse" {
            return Err("cross-machine operation refused: remote_messages=refuse".into());
        }
        let backend = if remote {
            "remote"
        } else if harness == "claude" {
            "native"
        } else {
            "local"
        };
        let now = self.clock.now_ms();
        let (admission, epoch, mode) = {
            let mut state = self.state.lock().unwrap();
            if *self.closing.borrow() {
                return Err("intercom is shutting down".into());
            }
            let pi_watcher = relay_origin.is_empty()
                && origin.is_some_and(|row| str_field(row, "harness") == "pi");
            let epoch = if pi_watcher {
                Some(
                    state
                        .sessions
                        .get(watcher)
                        .ok_or("watcher is no longer connected")?
                        .epoch,
                )
            } else {
                None
            };
            let mode = if native {
                "native"
            } else if !relay_origin.is_empty() {
                "relay"
            } else if pi_watcher
                && state
                    .sessions
                    .get(watcher)
                    .is_some_and(|session| session.caps.contains("idle_state_v1"))
            {
                "pi_push"
            } else {
                "query_only"
            };
            let record = Idle {
                watcher: watcher.into(),
                target: target.clone(),
                mode: mode.into(),
                status: "subscribed".into(),
                updated_at: now,
                created: now,
                epoch,
                backend: backend.into(),
                remote_origin: relay_origin.into(),
                remote_machine: machine.into(),
                native,
                source_identity: source_identity.into(),
                admitting: true,
                superseded: false,
            };
            let State {
                subscriptions,
                idle_stable,
                ..
            } = &mut *state;
            let admission = admit(subscriptions, idle_stable, id, record, now)?;
            let mode = subscriptions[id].mode.clone();
            (admission, epoch, mode)
        };
        if !admission.admitted {
            return Ok(
                json!({"subscription_id":id,"status":admission.status,"delivery_mode":mode}),
            );
        }
        let _pin = AdmissionPin {
            runtime: self,
            id: id.into(),
        };
        if let Some((previous_id, previous)) = admission.previous {
            self.cancel_observer(&previous_id, &previous).await;
        }
        if !self.idle_watcher_current(watcher, epoch) {
            self.cancel_idle_watcher(watcher, epoch.unwrap_or(0)).await;
            return Err("watcher connection ended".into());
        }
        let mut closing = self.closing.subscribe();
        let is_closing = *closing.borrow();
        if is_closing {
            self.complete_idle(id, "unavailable").await;
            return Err("intercom is shutting down".into());
        }
        let backend_call = async {
            if backend == "native" {
                self.native
                    .subscribe_native(&row, id, self.clock.now_ms())
                    .await?;
                Ok(None)
            } else if backend == "remote" {
                // Publish ownership before forwarding a freshly registered
                // watcher; the authenticated relay validates this snapshot.
                // Rows cross to the relay, so internal fields (`_`-prefixed
                // routing state and the host-only pid) must be stripped first.
                let sessions: Vec<_> = self
                    .directory()
                    .await
                    .into_iter()
                    .filter(|row| {
                        (str_field(row, "machine_id").is_empty()
                            || str_field(row, "machine_id") == self.machine)
                            && !str_field(row, "id").starts_with("webhook/")
                    })
                    .map(super::public_row)
                    .collect();
                let writer = self
                    .state
                    .lock()
                    .unwrap()
                    .relay_tx
                    .clone()
                    .ok_or("not connected to relay")?;
                writer
                    .send(
                        json!({"type":"snapshot","machine_id":self.machine,"sessions":sessions})
                            .into(),
                    )
                    .await
                    .map_err(|_| "relay connection closed".to_string())?;
                let reply = relay::call(self, json!({"type":"idle_subscribe","machine_id":machine,"session_id":target,"watcher":watcher,"subscription_id":id})).await?;
                remote_ack(&reply, id)
            } else {
                Ok(None)
            }
        };
        let result: Result<Option<&'static str>, String> = tokio::select! {
            _ = closing.changed() => Err("intercom is shutting down".into()),
            result = tokio::time::timeout(Duration::from_secs(5), backend_call) => result.unwrap_or_else(|_| Err("idle subscription acknowledgement timed out".into())),
        };
        let snapshot = self.state.lock().unwrap().subscriptions.get(id).cloned();
        if snapshot.as_ref().is_none_or(|record| record.superseded) {
            if let Some(record) = snapshot {
                self.cancel_observer(id, &record).await;
            }
            return Ok(json!({"subscription_id":id,"status":"cancelled","delivery_mode":mode}));
        }
        let snapshot = snapshot.unwrap();
        if snapshot.status != "subscribed" {
            self.cancel_observer(id, &snapshot).await;
        }
        if !self.idle_watcher_current(watcher, epoch)
            || (epoch.is_some() && snapshot.status == "unavailable")
        {
            self.cancel_idle_watcher(watcher, epoch.unwrap_or(0)).await;
            self.cancel_observer(id, &snapshot).await;
            return Err("watcher connection ended".into());
        }
        match result {
            Err(error) => {
                self.complete_idle(id, "unavailable").await;
                self.cancel_observer(id, &snapshot).await;
                return Err(error);
            }
            Ok(Some(status)) => self.complete_idle(id, status).await,
            Ok(None) => {}
        }
        let status = self
            .state
            .lock()
            .unwrap()
            .subscriptions
            .get(id)
            .map(|record| record.status.clone())
            .unwrap_or_else(|| "unavailable".into());
        Ok(json!({"subscription_id":id,"status":status,"delivery_mode":mode}))
    }
    fn idle_watcher_current(&self, watcher: &str, epoch: Option<u64>) -> bool {
        epoch.is_none_or(|epoch| {
            self.state
                .lock()
                .unwrap()
                .sessions
                .get(watcher)
                .is_some_and(|session| session.epoch == epoch)
        })
    }
    async fn cancel_observer(&self, id: &str, record: &Idle) {
        if record.backend == "native" {
            let _ = self.native.cancel_idle(id).await;
        }
        if record.backend == "remote" {
            let _ = relay::enqueue_control(self, json!({"type":"idle_cancel","id":uuid::Uuid::new_v4().to_string(),
                "machine_id":record.remote_machine,"subscription_id":id,"watcher":record.watcher,"session_id":record.target})).await;
        }
    }
    pub async fn cancel_idle_watcher(&self, watcher: &str, epoch: u64) {
        let records = {
            let mut state = self.state.lock().unwrap();
            let ids: Vec<_> = state
                .subscriptions
                .iter()
                .filter(|(_, record)| {
                    record.status == "subscribed"
                        && record.watcher == watcher
                        && record.epoch == Some(epoch)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    let record = terminal(
                        &mut state.subscriptions,
                        &id,
                        "unavailable",
                        self.clock.now_ms(),
                    )?;
                    clear_stable(&mut state.idle_stable, &record.target);
                    Some((id, record))
                })
                .collect::<Vec<_>>()
        };
        for (id, record) in records {
            self.cancel_observer(&id, &record).await;
        }
    }
    pub async fn complete_idle(&self, id: &str, status: &str) {
        if !["idle", "exited", "unavailable", "expired"].contains(&status) {
            return;
        }
        let record = {
            let mut state = self.state.lock().unwrap();
            terminal(&mut state.subscriptions, id, status, self.clock.now_ms())
        };
        if let Some(record) = record {
            self.dispatch_idle(id, record).await;
        }
    }
    async fn dispatch_idle(&self, id: &str, record: Idle) {
        if record.superseded {
            return;
        }
        if terminal_needs_cancel(&record) {
            self.cancel_observer(id, &record).await;
        }
        if !record.remote_origin.is_empty() {
            let _ = relay::enqueue_control(self, json!({"type":"idle_notice","id":uuid::Uuid::new_v4().to_string(),
                "machine_id":record.remote_origin,"subscription_id":id,"watcher":record.watcher,"from":record.target,"status":record.status})).await;
        } else if record.native {
            // Native peers own their own 12h timer. The adapter may release its
            // reverse route on expired but must not put it on the native wire.
            let _ = self.native.idle_notice(id, &record.status).await;
        } else if record.mode == "pi_push" {
            let tx = self
                .state
                .lock()
                .unwrap()
                .sessions
                .get(&record.watcher)
                .filter(|session| Some(session.epoch) == record.epoch)
                .map(|session| session.tx.clone());
            if let Some(tx) = tx {
                let (written, received) = oneshot::channel();
                let value = json!({"type":"idle_notice","session_id":record.watcher,"subscription_id":id,"from":record.target,"status":record.status});
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    tx.send(Push {
                        value,
                        written: Some(written),
                        fence: None,
                    })
                    .await
                    .map_err(|_| ())?;
                    received.await.map_err(|_| ())?.map_err(|_| ())
                })
                .await;
            }
        }
    }
    pub async fn idle_wire(&self, frame: Value) -> Option<Value> {
        if frame["rpc_reply"] == true {
            return None;
        }
        let kind = str_field(&frame, "type");
        if !["idle_subscribe", "idle_cancel", "idle_notice"].contains(&kind) {
            return None;
        }
        let origin = str_field(&frame, "origin_machine");
        if origin.is_empty() {
            return None;
        }
        let id = str_field(&frame, "subscription_id");
        let result = if kind == "idle_subscribe" {
            self.start_idle(
                id,
                str_field(&frame, "watcher"),
                str_field(&frame, "session_id"),
                false,
                origin,
                "",
            )
            .await
            .map(|value| json!({"status":value["status"]}))
        } else if kind == "idle_cancel" {
            let record = {
                let mut state = self.state.lock().unwrap();
                let result =
                    cancel_record(&mut state.subscriptions, id, &frame, self.clock.now_ms());
                if let Ok(Some(record)) = &result {
                    clear_stable(&mut state.idle_stable, &record.target);
                }
                result
            };
            match record {
                Ok(Some(record)) => {
                    self.cancel_observer(id, &record).await;
                    Ok(json!({}))
                }
                Ok(None) => Ok(json!({})),
                Err(error) => Err(error),
            }
        } else {
            let record = self
                .state
                .lock()
                .unwrap()
                .subscriptions
                .get(id)
                .filter(|record| notice_correlated(record, &frame))
                .cloned();
            match record {
                Some(_) => match str_field(&frame, "status") {
                    "idle" | "exited" | "unavailable" => {
                        self.complete_idle(id, str_field(&frame, "status")).await;
                        Ok(json!({}))
                    }
                    "expired" => {
                        self.complete_idle(id, "unavailable").await;
                        Ok(json!({}))
                    }
                    _ => Err("invalid idle notice state".into()),
                },
                None => Err("uncorrelated idle notice".into()),
            }
        };
        if kind == "idle_notice" {
            if let Err(error) = result {
                self.record("error", "", str_field(&frame, "from"), &error);
                if !str_field(&frame, "id").is_empty() {
                    return Some(
                        json!({"type":"error","id":frame["id"],"code":"unavailable","message":error}),
                    );
                }
            }
            return None;
        }
        Some(match result {
            Ok(mut value) => {
                value["type"] = frame["type"].clone();
                value["id"] = frame["id"].clone();
                value["subscription_id"] = frame["subscription_id"].clone();
                value
            }
            Err(error) => {
                json!({"type":"error","id":frame["id"],"code":"unavailable","message":error})
            }
        })
    }
    async fn refresh_idle(&self) {
        let targets = {
            let mut state = self.state.lock().unwrap();
            let targets: HashSet<_> = state
                .subscriptions
                .values()
                .filter(|record| record.status == "subscribed" && record.backend == "local")
                .map(|record| record.target.clone())
                .collect();
            state.idle_stable.retain(|key, _| {
                key.split_once('\0')
                    .is_some_and(|(target, _)| targets.contains(target))
            });
            targets
        };
        let observations = futures_util::stream::iter(targets)
            .map(|target| async move {
                let observation = if target.starts_with("pi/") {
                    let state = self.state.lock().unwrap();
                    if state.exited.contains(&target) {
                        Ok(("exited".into(), "pi-explicit-exit".into()))
                    } else if let Some(session) = state
                        .sessions
                        .get(&target)
                        .filter(|session| session.caps.contains("idle_state_v1"))
                    {
                        let status = if session.idle_since.is_some() {
                            "idle"
                        } else {
                            session_state(str_field(&session.value, "status"))
                        };
                        Ok((status.into(), format!("pi:{}", session.epoch)))
                    } else {
                        Err("Pi target unavailable".into())
                    }
                } else {
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        self.native.idle_observation(&target),
                    )
                    .await
                    .unwrap_or_else(|_| Err("native idle observation timed out".into()))
                };
                (target, observation)
            })
            .buffer_unordered(IDLE_PENDING)
            .collect::<Vec<_>>()
            .await;
        let mut notices = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            for (target, observation) in observations {
                let (status, identity) =
                    observation.unwrap_or_else(|_| ("unavailable".into(), String::new()));
                let status = session_state(&status);
                if !settled(
                    &mut state.idle_stable,
                    &target,
                    &identity,
                    status,
                    self.clock.now_ms(),
                ) {
                    continue;
                }
                let ids: Vec<_> = state
                    .subscriptions
                    .iter()
                    .filter(|(_, record)| {
                        record.status == "subscribed"
                            && record.backend == "local"
                            && record.target == target
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    if let Some(record) =
                        terminal(&mut state.subscriptions, &id, status, self.clock.now_ms())
                    {
                        notices.push((id, record));
                    }
                }
            }
            let ids: Vec<_> = state
                .subscriptions
                .iter()
                .filter(|(_, record)| {
                    record.status == "subscribed"
                        && self.clock.now_ms().saturating_sub(record.created) >= IDLE_TTL
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                if let Some(record) = terminal(
                    &mut state.subscriptions,
                    &id,
                    "expired",
                    self.clock.now_ms(),
                ) {
                    notices.push((id, record));
                }
            }
        }
        futures_util::stream::iter(notices)
            .for_each_concurrent(IDLE_PENDING, |(id, record)| async move {
                self.dispatch_idle(&id, record).await;
            })
            .await;
    }
    pub(super) async fn maintenance(self: &Arc<Self>) {
        let mut stop = self.closing.subscribe();
        let mut forwards = tokio::task::JoinSet::new();
        loop {
            while forwards.try_join_next().is_some() {}
            if *stop.borrow() {
                break;
            }
            tokio::select! { _ = stop.changed() => break, _ = self.clock.sleep("intercom_maintenance", Duration::from_millis(250)) => {} }
            let ended = {
                let state = self.state.lock().unwrap();
                state
                    .subscriptions
                    .values()
                    .filter(|record| {
                        record.status == "subscribed"
                            && record.epoch.is_some_and(|epoch| {
                                state
                                    .sessions
                                    .get(&record.watcher)
                                    .is_none_or(|session| session.epoch != epoch)
                            })
                    })
                    .map(|record| (record.watcher.clone(), record.epoch.unwrap()))
                    .collect::<HashSet<_>>()
            };
            for (watcher, epoch) in ended {
                self.cancel_idle_watcher(&watcher, epoch).await;
            }
            self.refresh_idle().await;
            self.native.maintain_idle(self).await;
            let retries = {
                let mut state = self.state.lock().unwrap();
                let now = self.clock.now_ms();
                state.prune_deliveries(now);
                state.subscriptions.retain(|_, record| {
                    record.status == "subscribed"
                        || record.admitting
                        || now.saturating_sub(record.updated_at) <= IDLE_TTL
                });
                let due = state
                    .messages
                    .iter()
                    .filter(|(id, delivery)| {
                        state.active(id) && delivery.route_terminal() && delivery.retry_due(now)
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                due.into_iter()
                    .filter_map(|id| {
                        let delivery = state.messages.get_mut(&id)?;
                        delivery.forwarding = true;
                        Some((id, delivery.clone()))
                    })
                    .collect::<Vec<_>>()
            };
            for (id, delivery) in retries {
                let runtime = self.clone();
                forwards.spawn(async move { runtime.retry_receipt(&id, delivery).await });
            }
        }
    }
    pub(super) async fn finish_idle(&self, status: &str) {
        let notices = {
            let mut state = self.state.lock().unwrap();
            let ids: Vec<_> = state
                .subscriptions
                .iter()
                .filter(|(_, record)| record.status == "subscribed")
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    terminal(&mut state.subscriptions, &id, status, self.clock.now_ms())
                        .map(|record| (id, record))
                })
                .collect::<Vec<_>>()
        };
        let notifications = futures_util::stream::iter(notices).for_each_concurrent(
            None,
            |(id, record)| async move {
                self.cancel_observer(&id, &record).await;
                self.dispatch_idle(&id, record).await;
            },
        );
        let _ = tokio::time::timeout(IDLE_FLUSH, notifications).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(watcher: &str, target: &str) -> Idle {
        Idle {
            watcher: watcher.into(),
            target: target.into(),
            mode: "query_only".into(),
            status: "subscribed".into(),
            updated_at: 0,
            created: 0,
            epoch: None,
            backend: "local".into(),
            remote_origin: String::new(),
            remote_machine: String::new(),
            native: false,
            source_identity: String::new(),
            admitting: false,
            superseded: false,
        }
    }
    #[tokio::test]
    async fn unknown_native_child_cancel_remains_an_idempotent_noop() {
        let native = native::Native::new();
        let child = uuid::Uuid::new_v4().to_string();
        native.cancel_idle(&child).await.unwrap();
        native.cancel_idle(&child).await.unwrap();
        assert!(native.list().await.is_empty());
    }
    fn runtime() -> Runtime {
        let (stop, _) = watch::channel(false);
        let (closing, _) = watch::channel(false);
        Runtime {
            cfg: Config::default(),
            clock: crate::clock::Clock::default(),
            identity: Identity::default(),
            machine: "idle-wire-fixture".into(),
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
                supervised: false,
                incoming: Default::default(),
                subscriptions: HashMap::new(),
                idle_stable: HashMap::new(),
                next_epoch: 0,
                next_conn: 0,
                events: VecDeque::new(),
                seq: 0,
                remote: Vec::new(),
                relay_tx: None,
            relay_error: String::new(),
            relay_link: Default::default(),
                control_outbox: VecDeque::new(),
                port_conns: HashSet::new(),
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
    #[tokio::test]
    async fn unknown_wire_cancel_returns_correlated_error_while_known_retries_preserve_terminal_state(
    ) {
        let runtime = runtime();
        let known = uuid::Uuid::new_v4().to_string();
        let unknown = uuid::Uuid::new_v4().to_string();
        let mut admitted = record("codex/remote-watcher", "pi/target");
        admitted.remote_origin = "machine-A".into();
        admitted.created = runtime.clock.now_ms();
        admitted.updated_at = admitted.created;
        runtime
            .state
            .lock()
            .unwrap()
            .subscriptions
            .insert(known.clone(), admitted);
        let mut request = json!({"type":"idle_cancel","id":"unknown-cancel-request","origin_machine":"machine-A", "subscription_id":unknown,"watcher":"codex/remote-watcher","session_id":"pi/target"});
        let response = runtime.idle_wire(request.clone()).await.unwrap();
        assert_eq!(response["type"], "error");
        assert_eq!(response["id"], request["id"]);
        assert_eq!(response["code"], "unavailable");
        assert_eq!(response["message"], "uncorrelated idle cancellation");
        {
            let state = runtime.state.lock().unwrap();
            assert_eq!(state.subscriptions.len(), 1);
            assert!(!state.subscriptions.contains_key(&unknown));
            assert_eq!(state.subscriptions[&known].status, "subscribed");
        }
        request["subscription_id"] = json!(known);
        request["id"] = json!("known-cancel-request");
        assert_eq!(
            runtime.idle_wire(request.clone()).await.unwrap()["type"],
            "idle_cancel"
        );
        assert_eq!(
            runtime.state.lock().unwrap().subscriptions[&known].status,
            "unavailable"
        );
        request["id"] = json!("known-cancel-retry");
        let retry = runtime.idle_wire(request.clone()).await.unwrap();
        assert_eq!(retry["type"], "idle_cancel");
        assert_eq!(retry["id"], request["id"]);
        assert_eq!(
            runtime.state.lock().unwrap().subscriptions[&known].status,
            "unavailable"
        );
        request["origin_machine"] = json!("machine-B");
        assert_eq!(
            runtime.idle_wire(request.clone()).await.unwrap()["type"],
            "error"
        );
        assert_eq!(
            runtime.state.lock().unwrap().subscriptions[&known].status,
            "unavailable"
        );
        runtime
            .state
            .lock()
            .unwrap()
            .subscriptions
            .get_mut(&known)
            .unwrap()
            .status = "idle".into();
        request["origin_machine"] = json!("machine-A");
        assert_eq!(
            runtime.idle_wire(request).await.unwrap()["type"],
            "idle_cancel"
        );
        assert_eq!(
            runtime.state.lock().unwrap().subscriptions[&known].status,
            "idle"
        );
    }
    #[tokio::test]
    async fn shutdown_flush_waits_for_a_slow_idle_notice_write() {
        let runtime = runtime();
        let (tx, mut rx) = mpsc::channel(4);
        {
            let mut state = runtime.state.lock().unwrap();
            state.sessions.insert(
                "pi/watcher".into(),
                Session {
                    value: json!({"id":"pi/watcher","harness":"pi"}),
                    owner: 1,
                    epoch: 1,
                    tx,
                    disconnect: None,
                    caps: HashSet::new(),
                    idle_since: None,
                },
            );
            let mut subscription = record("pi/watcher", "pi/target");
            subscription.mode = "pi_push".into();
            subscription.epoch = Some(1);
            subscription.created = runtime.clock.now_ms();
            state.subscriptions.insert("flushed".into(), subscription);
        }
        let writer = async {
            let frame = rx.recv().await.unwrap();
            assert_eq!(frame.value["status"], "unavailable");
            tokio::time::sleep(Duration::from_millis(600)).await;
            frame.written.unwrap().send(Ok(())).is_ok()
        };
        let (_, waited) = tokio::join!(runtime.finish_idle("unavailable"), writer);
        assert!(
            waited,
            "shutdown must not abandon an idle notice the watcher is still writing"
        );
    }
    #[test]
    fn target_resolution_prefers_id_over_a_conflicting_name() {
        let rows = vec![
            json!({"id":"pi/actual","name":"other"}),
            json!({"id":"pi/other","name":"pi/actual"}),
        ];
        assert_eq!(target_row(&rows, "pi/actual").unwrap()["id"], "pi/actual");
        let rows = vec![
            json!({"id":"pi/actual","name":"actual"}),
            json!({"id":"pi/other","name":"pi/actual"}),
        ];
        assert_eq!(target_row(&rows, "pi/actual").unwrap()["id"], "pi/actual");
    }
    #[test]
    fn target_resolution_accepts_exact_slash_names_but_not_partial_names() {
        let rows = vec![json!({"id":"pi/actual","name":"project/target"})];
        assert_eq!(
            target_row(&rows, "project/target").unwrap()["id"],
            "pi/actual"
        );
        assert!(target_row(&rows, "project").is_err());
    }
    #[test]
    fn expired_remote_watch_releases_its_target_child() {
        let mut rec = record("w", "t");
        rec.backend = "remote".into();
        rec.status = "expired".into();
        assert!(terminal_needs_cancel(&rec));
        rec.status = "idle".into();
        assert!(!terminal_needs_cancel(&rec));
        rec.backend = "native".into();
        assert!(terminal_needs_cancel(&rec));
    }
    #[test]
    fn replay_is_atomic_and_conflicting_provenance_is_rejected() {
        let mut records = HashMap::new();
        let mut stable = HashMap::new();
        assert!(
            admit(&mut records, &mut stable, "id", record("w", "t"), 0)
                .unwrap()
                .admitted
        );
        assert!(
            !admit(&mut records, &mut stable, "id", record("w", "t"), 1)
                .unwrap()
                .admitted
        );
        let mut other = record("w", "t");
        other.remote_origin = "other-machine".into();
        assert!(admit(&mut records, &mut stable, "id", other, 2).is_err());
        assert_eq!(records.len(), 1);
    }
    #[test]
    fn native_refresh_reuses_one_slot_only_with_same_endpoint_lifetime() {
        let mut records = HashMap::new();
        let mut stable = HashMap::new();
        let mut rec = record("w", "t");
        rec.native = true;
        rec.source_identity = "source+caller+acceptor-A".into();
        for i in 0..40 {
            let result = admit(&mut records, &mut stable, &i.to_string(), rec.clone(), i).unwrap();
            assert_eq!(result.previous.is_some(), i > 0);
        }
        assert_eq!(
            records
                .values()
                .filter(|r| r.status == "subscribed")
                .count(),
            1
        );
        assert!(records["0"].superseded);
        rec.source_identity = "source+caller+acceptor-B".into();
        assert!(admit(&mut records, &mut stable, "different", rec, 41)
            .unwrap()
            .previous
            .is_none());
        assert_eq!(
            records
                .values()
                .filter(|r| r.status == "subscribed")
                .count(),
            2
        );
    }
    #[test]
    fn watcher_and_global_capacity_are_separate_and_terminal_slots_reclaim() {
        let mut records = HashMap::new();
        let mut stable = HashMap::new();
        for i in 0..32 {
            admit(
                &mut records,
                &mut stable,
                &i.to_string(),
                record(&format!("w{}", i / 4), "t"),
                0,
            )
            .unwrap();
        }
        assert!(admit(&mut records, &mut stable, "extra", record("new", "t"), 0).is_err());
        terminal(&mut records, "0", "idle", 1);
        admit(
            &mut records,
            &mut stable,
            "replacement",
            record("w0", "t"),
            1,
        )
        .unwrap();
        terminal(&mut records, "1", "idle", 1);
        assert!(admit(&mut records, &mut stable, "fifth", record("w1", "t"), 1).is_err());
    }
    #[test]
    fn admission_pin_prevents_history_eviction_until_ack_finishes() {
        let mut records = HashMap::new();
        let mut stable = HashMap::new();
        let mut rec = record("w", "t");
        rec.native = true;
        rec.source_identity = "same".into();
        rec.admitting = true;
        for i in 0..1024 {
            admit(
                &mut records,
                &mut stable,
                &format!("{i:04}"),
                rec.clone(),
                i,
            )
            .unwrap();
        }
        assert!(admit(&mut records, &mut stable, "overflow", rec.clone(), 1024).is_err());
        assert_eq!(records["0000"].status, "cancelled");
        records.get_mut("0000").unwrap().admitting = false;
        admit(&mut records, &mut stable, "new", rec, 1025).unwrap();
        assert!(!records.contains_key("0000"));
    }
    #[test]
    fn expiration_wins_terminal_races_and_never_resurrects() {
        let mut records = HashMap::from([("id".into(), record("w", "t"))]);
        assert_eq!(
            terminal(&mut records, "id", "idle", IDLE_TTL)
                .unwrap()
                .status,
            "expired"
        );
        assert!(terminal(&mut records, "id", "exited", IDLE_TTL + 1).is_none());
    }
    #[test]
    fn settled_idle_is_reset_by_busy_disconnect_and_owner_replacement() {
        let mut stable = HashMap::new();
        assert!(!settled(&mut stable, "t", "epoch-A", "idle", 0));
        assert!(!settled(&mut stable, "t", "epoch-A", "idle", 749));
        assert!(settled(&mut stable, "t", "epoch-A", "idle", 750));
        assert!(!settled(&mut stable, "t", "epoch-B", "idle", 751));
        assert!(!settled(&mut stable, "t", "epoch-B", "unavailable", 1000));
        assert!(!settled(&mut stable, "t", "epoch-B", "idle", 2000));
        assert!(settled(&mut stable, "t", "epoch-B", "idle", 2750));
        assert!(settled(&mut stable, "t", "epoch-B", "exited", 2751));
    }
    #[test]
    fn nonterminal_observations_never_mean_exit() {
        for status in ["offline", "notLoaded", "unknown", ""] {
            assert_eq!(session_state(status), "unavailable");
        }
        assert_eq!(session_state("systemError"), "waiting");
        assert_eq!(session_state("exited"), "exited");
    }
    #[test]
    fn authenticated_notice_requires_origin_watcher_target_and_backend() {
        let mut rec = record("w", "t");
        rec.backend = "remote".into();
        rec.remote_machine = "machine-B".into();
        let frame = json!({"origin_machine":"machine-B","watcher":"w","from":"t"});
        assert!(notice_correlated(&rec, &frame));
        for field in ["origin_machine", "watcher", "from"] {
            let mut forged = frame.clone();
            forged[field] = json!("forged");
            assert!(!notice_correlated(&rec, &forged));
        }
        rec.backend = "local".into();
        assert!(!notice_correlated(&rec, &frame));
    }
    #[test]
    fn cancellation_cannot_revoke_a_sibling_or_another_origin() {
        let mut rec = record("w", "t");
        rec.remote_origin = "machine-A".into();
        let frame = json!({"origin_machine":"machine-A","watcher":"w","session_id":"t"});
        assert!(cancel_correlated(&rec, &frame));
        for field in ["origin_machine", "watcher", "session_id"] {
            let mut forged = frame.clone();
            forged[field] = json!("other");
            assert!(!cancel_correlated(&rec, &forged));
        }
    }
    #[test]
    fn ack_correlates_type_uuid_and_accepts_only_contract_terminal_states() {
        let base = json!({"type":"idle_subscribe","subscription_id":"id","status":"subscribed"});
        assert_eq!(remote_ack(&base, "id").unwrap(), None);
        for status in ["idle", "exited", "unavailable"] {
            let mut ack = base.clone();
            ack["status"] = json!(status);
            assert_eq!(remote_ack(&ack, "id").unwrap(), Some(status));
        }
        for status in ["expired", "busy", "waiting", "cancelled", ""] {
            let mut ack = base.clone();
            ack["status"] = json!(status);
            assert!(remote_ack(&ack, "id").is_err());
        }
        let mut ack = base.clone();
        ack["type"] = json!("idle_cancel");
        assert!(remote_ack(&ack, "id").is_err());
        assert!(remote_ack(&base, "another-id").is_err());
    }
}
