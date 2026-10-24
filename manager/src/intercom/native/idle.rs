use super::*;
use std::sync::Arc;
#[cfg(unix)]
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

const TTL: u64 = 12 * 3600 * 1000;
const MAX_ACTIVE: usize = 32;
const MAX_REPLAY: usize = 1024;
#[cfg(unix)]
const DEBT_SCAN_MS: u64 = 5000;

type Notices = Vec<(String, String)>;
type Ready = watch::Receiver<Option<Result<(), String>>>;

pub(super) struct IdleState {
    running: bool,
    epoch: u64,
    requests: HashMap<String, Request>,
    slots: HashMap<String, Slot>,
    tasks: Vec<OwnedTask>,
    connections: Arc<tokio::sync::Semaphore>,
    #[cfg(unix)]
    debt_scan_at: u64,
}
impl Default for IdleState {
    fn default() -> Self {
        Self {
            running: false,
            epoch: 0,
            requests: HashMap::new(),
            slots: HashMap::new(),
            tasks: Vec::new(),
            connections: Arc::new(tokio::sync::Semaphore::new(256)),
            #[cfg(unix)]
            debt_scan_at: 0,
        }
    }
}
struct OwnedTask {
    physical: bool,
    task: tokio::task::JoinHandle<()>,
}
#[derive(Clone)]
struct Request {
    target: String,
    origin: String,
    created: u64,
    slot: Option<String>,
    error: Option<String>,
}
#[derive(Clone, Debug, PartialEq)]
struct Generation {
    socket: String,
    pid: i64,
    started: Value,
    process: String,
    dev: Value,
    ino: Value,
}
impl Generation {
    fn from_endpoint(endpoint: &Value) -> Self {
        Self {
            socket: str_field(endpoint, "_socket").into(),
            pid: endpoint["pid"].as_i64().unwrap_or(0),
            started: endpoint["_started_at"].clone(),
            process: str_field(endpoint, "_proc_start").into(),
            dev: endpoint["_socket_dev"].clone(),
            ino: endpoint["_socket_ino"].clone(),
        }
    }
}
struct Slot {
    serial: String,
    epoch: u64,
    endpoint: Value,
    generation: Generation,
    id: String,
    deadline: u64,
    children: HashSet<String>,
    logical: bool,
    sending: bool,
    ready: watch::Sender<Option<Result<(), String>>>,
    cancel: watch::Sender<bool>,
    done: watch::Sender<bool>,
    debt: Option<String>,
    error: Option<String>,
    early: Option<String>,
    #[cfg(unix)]
    listener: Option<ReplyListener>,
    timer: Option<tokio::task::JoinHandle<()>>,
    timer_seq: u64,
}
impl Slot {
    fn physical_live(&self) -> bool {
        !*self.done.borrow() || self.debt.is_some()
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }
}
pub(super) struct Return {
    pub endpoint: Value,
    pub reply: Value,
    pub original: String,
    target: String,
    source_identity: String,
    admitting: bool,
}
fn control_id(original: &str, _identity: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(original).map_err(|_| "native control requires a UUID".to_string())?;
    Ok(original.into())
}
fn install_return(routes: &mut HashMap<String, Return>, id: &str, route: Return) -> bool {
    if let Some(existing) = routes.get(id) {
        return existing.source_identity == route.source_identity
            && existing.target == route.target
            && existing.original == route.original;
    }
    if routes.len() >= MAX_REPLAY {
        return false;
    }
    routes.insert(id.into(), route);
    true
}
fn native_state(state: &str) -> &str {
    match state {
        "idle" | "exited" | "unavailable" => state,
        _ => "unavailable",
    }
}
impl IdleState {
    fn prune(&mut self, now: u64) {
        self.requests.retain(|_, request| {
            request.slot.is_some() || now.saturating_sub(request.created) < TTL
        });
        while self.requests.len() > MAX_REPLAY {
            let oldest = self
                .requests
                .iter()
                .filter(|(_, request)| request.slot.is_none())
                .map(|(id, request)| (request.created, id.clone()))
                .min();
            let Some((_, id)) = oldest else {
                break;
            };
            self.requests.remove(&id);
        }
        self.tasks.retain(|task| !task.task.is_finished());
    }
    fn replay(&self, target: &str, origin: &str, id: &str) -> Option<Step> {
        let request = self.requests.get(id)?;
        if request.target != target || request.origin != origin {
            return Some(Step::Replay(Err("conflicting native idle UUID".into())));
        }
        if let Some(slot) = request
            .slot
            .as_ref()
            .and_then(|serial| self.slots.get(target).filter(|slot| &slot.serial == serial))
        {
            if slot.sending {
                return Some(Step::Ready(slot.ready.subscribe()));
            }
        }
        Some(Step::Replay(request.error.clone().map_or(Ok(()), Err)))
    }
    fn expire_children(&mut self, target: &str, serial: &str, now: u64) {
        let Some(slot) = self
            .slots
            .get_mut(target)
            .filter(|slot| slot.serial == serial && slot.logical)
        else {
            return;
        };
        slot.children.retain(|child| {
            let Some(request) = self.requests.get_mut(child) else {
                return false;
            };
            if now.saturating_sub(request.created) >= TTL {
                request.slot = None;
                false
            } else {
                true
            }
        });
    }
    fn retire(
        &mut self,
        target: &str,
        serial: &str,
        status: Option<&str>,
        error: Option<String>,
    ) -> Notices {
        let Some(mut slot) = self.slots.remove(target) else {
            return Vec::new();
        };
        if slot.serial != serial || !slot.logical {
            self.slots.insert(target.into(), slot);
            return Vec::new();
        }
        slot.logical = false;
        slot.error = error.clone();
        slot.cancel.send_replace(true);
        if slot.sending {
            slot.sending = false;
            slot.ready
                .send_replace(Some(error.clone().map_or(Ok(()), Err)));
        }
        let mut notices = Vec::new();
        for id in slot.children.drain() {
            if let Some(request) = self
                .requests
                .get_mut(&id)
                .filter(|request| request.slot.as_deref() == Some(serial))
            {
                request.slot = None;
                request.error = error.clone();
                if let Some(status) = status {
                    notices.push((id, status.into()));
                }
            }
        }
        #[cfg(unix)]
        slot.listener.take();
        if let Some(timer) = slot.timer.take() {
            timer.abort();
        }
        if slot.physical_live() {
            self.slots.insert(target.into(), slot);
        }
        notices.sort_by(|a, b| a.0.cmp(&b.0));
        notices
    }
    fn cancel(&mut self, child: &str) {
        let Some(request) = self.requests.get_mut(child) else {
            return;
        };
        let Some(serial) = request.slot.take() else {
            return;
        };
        let target = request.target.clone();
        let last = self
            .slots
            .get_mut(&target)
            .filter(|slot| slot.serial == serial)
            .is_some_and(|slot| {
                slot.children.remove(child);
                slot.children.is_empty()
            });
        if last {
            self.retire(
                &target,
                &serial,
                None,
                Some("native idle subscription cancelled".into()),
            );
        }
    }
}
enum Step {
    Replay(Result<(), String>),
    Ready(Ready),
    Physical(watch::Receiver<bool>),
    Retired(Notices),
    Tick(String, u64, u64),
    Attached(String),
    Start(SendPlan, Ready),
}
struct SendPlan {
    target: String,
    serial: String,
    epoch: u64,
    wire: String,
    endpoint: Value,
    from: String,
    cancel: watch::Receiver<bool>,
    renewal: bool,
}
struct SendWait {
    owner: Weak<super::super::Runtime>,
    child: String,
    armed: bool,
}
impl Drop for SendWait {
    fn drop(&mut self) {
        if self.armed {
            if let Some(runtime) = self.owner.upgrade() {
                runtime
                    .native
                    .idle_state
                    .lock()
                    .unwrap()
                    .cancel(&self.child);
            }
        }
    }
}
async fn wait_ready(mut ready: Ready) -> Result<(), String> {
    loop {
        if let Some(result) = ready.borrow().clone() {
            return result;
        }
        ready
            .changed()
            .await
            .map_err(|_| "native idle adapter closed".to_string())?;
    }
}
async fn wait_physical(mut done: watch::Receiver<bool>) -> Result<(), String> {
    loop {
        if *done.borrow() {
            return Ok(());
        }
        done.changed()
            .await
            .map_err(|_| "native idle adapter closed".to_string())?;
    }
}
async fn emit(runtime: &Shared, notices: Notices) {
    futures_util::stream::iter(notices)
        .for_each_concurrent(MAX_ACTIVE, |(id, status)| async move {
            runtime.complete_idle(&id, &status).await;
        })
        .await;
}
impl Native {
    pub(super) fn start_idle_adapter(&self) {
        self.stop_idle_state();
        self.idle_state.lock().unwrap().running = true;
    }
    fn stop_idle_state(&self) {
        let mut state = self.idle_state.lock().unwrap();
        state.running = false;
        state.epoch += 1;
        let slots: Vec<_> = state
            .slots
            .iter()
            .map(|(target, slot)| (target.clone(), slot.serial.clone()))
            .collect();
        for (target, serial) in slots {
            state.retire(
                &target,
                &serial,
                None,
                Some("native idle adapter closed".into()),
            );
        }
        // Physical sends retain their drain/fence even after logical shutdown.
        // Application shutdown outcomes belong to the coordinator, not Close.
        for task in state.tasks.iter().filter(|task| !task.physical) {
            task.task.abort();
        }
        state
            .tasks
            .retain(|task| task.physical && !task.task.is_finished());
    }
    pub(super) async fn close_idle(&self) {
        self.stop_idle_state();
        self.idle_returns.lock().unwrap().clear();
    }
    fn track_idle_task(&self, physical: bool, task: tokio::task::JoinHandle<()>) {
        let mut state = self.idle_state.lock().unwrap();
        state.tasks.retain(|task| !task.task.is_finished());
        if !physical && !state.running {
            task.abort();
            return;
        }
        state.tasks.push(OwnedTask { physical, task });
    }
    fn idle_now(&self, fallback: u64) -> u64 {
        self.owner
            .lock()
            .unwrap()
            .upgrade()
            .map_or(fallback, |runtime| runtime.clock.now_ms())
    }
    pub async fn cancel_idle(&self, child: &str) -> Result<(), String> {
        self.idle_state.lock().unwrap().cancel(child);
        Ok(())
    }
    #[cfg(unix)]
    fn fresh_idle_endpoint(&self, snapshot: &Value) -> Result<Value, String> {
        let target = str_field(snapshot, "id");
        let registry = PathBuf::from(str_field(snapshot, "_registry"));
        let directory = registry
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| super::super::config::home().join(".claude/sessions"));
        let mut entries = std::fs::read_dir(directory)
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .take(4096)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort();
        let mut candidates = Vec::new();
        for path in entries {
            let Ok(entry) = read_claude_entry(&path) else {
                continue;
            };
            if is_intercom_entry(&entry) {
                continue;
            }
            let Ok(mut row) = endpoint_row(&entry, &path) else {
                continue;
            };
            if str_field(&row, "id") != target {
                continue;
            }
            row["_started_at"] = entry["startedAt"].clone();
            candidates.push(row);
        }
        // Reject ambiguity instead of picking a winner: two live processes
        // publishing one session id (resume race or stale registry) make the
        // intended target unknowable, matching discovery-time duplicate
        // rejection and remote-source policy verification.
        if candidates.len() > 1 {
            return Err(format!(
                "ambiguous native idle target {target:?}: multiple live processes publish this session id"
            ));
        }
        let row = candidates
            .pop()
            .ok_or_else(|| format!("unknown native idle target {target:?}"))?;
        if row["notify_idle"] != true {
            return Err("Claude target does not advertise idle notices; upgrade the target".into());
        }
        let mut rows = self.rows.lock().unwrap();
        rows.retain(|old| str_field(old, "id") != target);
        rows.push(row.clone());
        Ok(row)
    }
    pub async fn subscribe_native(
        &self,
        snapshot: &Value,
        child: &str,
        now: u64,
    ) -> Result<(), String> {
        #[cfg(not(unix))]
        {
            let _ = (snapshot, child, now);
            Err("native idle supports Linux/macOS only".into())
        }
        #[cfg(unix)]
        {
            uuid::Uuid::parse_str(child)
                .map_err(|_| "native idle subscription requires UUID".to_string())?;
            let runtime = self
                .owner
                .lock()
                .unwrap()
                .upgrade()
                .ok_or("native idle adapter not running")?;
            let target = str_field(snapshot, "id");
            let origin = runtime
                .state
                .lock()
                .unwrap()
                .subscriptions
                .get(child)
                .map(|record| record.watcher.clone())
                .ok_or("unknown idle child")?;
            if origin.is_empty() || origin == target {
                return Err("native idle requires distinct watcher and target".into());
            }
            loop {
                let replay = {
                    let mut state = self.idle_state.lock().unwrap();
                    state.prune(self.idle_now(now));
                    if !state.running {
                        return Err("native idle adapter not running".into());
                    }
                    state.replay(target, &origin, child)
                };
                if let Some(step) = replay {
                    match step {
                        Step::Replay(result) => return result,
                        Step::Ready(ready) => {
                            wait_ready(ready).await?;
                            continue;
                        }
                        _ => unreachable!(),
                    }
                }
                let endpoint = self.fresh_idle_endpoint(snapshot)?;
                let generation = Generation::from_endpoint(&endpoint);
                let now = self.idle_now(now);
                let step = {
                    let mut state = self.idle_state.lock().unwrap();
                    if !state.running {
                        return Err("native idle adapter not running".into());
                    }
                    state.prune(now);
                    if let Some(replay) = state.replay(target, &origin, child) {
                        replay
                    } else {
                        self.admit_native_child(
                            &mut state, target, &origin, child, endpoint, generation, now,
                        )?
                    }
                };
                match step {
                    Step::Replay(result) => return result,
                    Step::Ready(ready) => {
                        wait_ready(ready).await?;
                    }
                    Step::Physical(done) => {
                        wait_physical(done).await?;
                    }
                    Step::Retired(notices) => {
                        emit(&runtime, notices).await;
                    }
                    Step::Tick(serial, epoch, seq) => {
                        self.tick_native_slot(target, &serial, epoch, seq)
                    }
                    Step::Attached(serial) => {
                        self.arm_native_timer(target, &serial);
                        return Ok(());
                    }
                    Step::Start(plan, ready) => {
                        let mut guard = SendWait {
                            owner: Arc::downgrade(&runtime),
                            child: child.into(),
                            armed: true,
                        };
                        self.spawn_native_send(plan);
                        let result = wait_ready(ready).await;
                        guard.armed = false;
                        return result;
                    }
                }
            }
        }
    }
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn admit_native_child(
        &self,
        state: &mut IdleState,
        target: &str,
        origin: &str,
        child: &str,
        endpoint: Value,
        generation: Generation,
        now: u64,
    ) -> Result<Step, String> {
        if let Some(slot) = state.slots.get(target).filter(|slot| slot.physical_live()) {
            if let Some(error) = &slot.debt {
                if slot.generation == generation {
                    return Err(error.clone());
                }
                if !slot.logical {
                    state.slots.remove(target);
                } else if let Some(slot) = state.slots.get_mut(target) {
                    slot.debt = None;
                }
            } else {
                return Ok(Step::Physical(slot.done.subscribe()));
            }
        }
        if let Some(slot) = state.slots.get(target).filter(|slot| slot.logical) {
            if slot.sending {
                return Ok(Step::Ready(slot.ready.subscribe()));
            }
            if slot.generation != generation {
                let serial = slot.serial.clone();
                return Ok(Step::Retired(state.retire(
                    target,
                    &serial,
                    Some("unavailable"),
                    None,
                )));
            }
            if now >= slot.deadline {
                return Ok(Step::Tick(slot.serial.clone(), slot.epoch, slot.timer_seq));
            }
        }
        let existing = state.slots.get(target).is_some_and(|slot| slot.logical);
        if state
            .requests
            .values()
            .filter(|request| request.slot.is_some())
            .count()
            >= MAX_ACTIVE
            || (!existing
                && state
                    .slots
                    .values()
                    .filter(|slot| slot.physical_live())
                    .count()
                    >= MAX_ACTIVE)
        {
            return Err("native idle subscription capacity reached".into());
        }
        if let Some(slot) = state.slots.get_mut(target).filter(|slot| slot.logical) {
            slot.children.insert(child.into());
            let serial = slot.serial.clone();
            state.requests.insert(
                child.into(),
                Request {
                    target: target.into(),
                    origin: origin.into(),
                    created: now,
                    slot: Some(serial.clone()),
                    error: None,
                },
            );
            state.prune(now);
            return Ok(Step::Attached(serial));
        }
        let serial = uuid::Uuid::new_v4().to_string();
        let listener = self.bind_idle_listener(
            target,
            &serial,
            state.epoch,
            &endpoint,
            state.connections.clone(),
        )?;
        let from = alias::from_identity(&listener.path);
        let wire = uuid::Uuid::new_v4().to_string();
        let (ready, receiver) = watch::channel(None);
        let (cancel, cancellation) = watch::channel(false);
        let (done, _) = watch::channel(false);
        let plan = SendPlan {
            target: target.into(),
            serial: serial.clone(),
            epoch: state.epoch,
            wire: wire.clone(),
            endpoint: endpoint.clone(),
            from,
            cancel: cancellation,
            renewal: false,
        };
        state.slots.insert(
            target.into(),
            Slot {
                serial: serial.clone(),
                epoch: state.epoch,
                endpoint,
                generation,
                id: wire,
                deadline: now.saturating_add(TTL),
                children: HashSet::from([child.into()]),
                logical: true,
                sending: true,
                ready,
                cancel,
                done,
                debt: None,
                error: None,
                early: None,
                listener: Some(listener),
                timer: None,
                timer_seq: 0,
            },
        );
        state.requests.insert(
            child.into(),
            Request {
                target: target.into(),
                origin: origin.into(),
                created: now,
                slot: Some(serial),
                error: None,
            },
        );
        state.prune(now);
        Ok(Step::Start(plan, receiver))
    }
    fn spawn_idle_notices(&self, notices: Notices) {
        if notices.is_empty() {
            return;
        }
        let owner = self.owner.lock().unwrap().clone();
        let task = tokio::spawn(async move {
            if let Some(runtime) = owner.upgrade() {
                emit(&runtime, notices).await;
            }
        });
        self.track_idle_task(false, task);
    }
    #[cfg(unix)]
    fn spawn_native_send(&self, plan: SendPlan) {
        let owner = self.owner.lock().unwrap().clone();
        let task = tokio::spawn(async move {
            let Some(runtime) = owner.upgrade() else {
                return;
            };
            let frame = json!({"msgV":1,"msg_id":plan.wire,"type":"control","action":"notify_when_idle","from":plan.from});
            let outcome = send_physical_idle(
                &plan.endpoint,
                frame,
                plan.cancel.clone(),
                Duration::from_secs(5),
            )
            .await;
            let notices = runtime.native.finish_native_send(&plan, outcome);
            runtime.native.arm_native_timer(&plan.target, &plan.serial);
            emit(&runtime, notices).await;
        });
        self.track_idle_task(true, task);
    }
    fn finish_native_send(&self, plan: &SendPlan, outcome: SendOutcome) -> Notices {
        let mut state = self.idle_state.lock().unwrap();
        let epoch = state.epoch;
        let Some(slot) = state
            .slots
            .get_mut(&plan.target)
            .filter(|slot| slot.serial == plan.serial && slot.id == plan.wire)
        else {
            return Vec::new();
        };
        slot.done.send_replace(true);
        if outcome.debt {
            slot.debt = outcome.error.clone();
        }
        if !slot.logical {
            if !slot.physical_live() {
                state.slots.remove(&plan.target);
            }
            return Vec::new();
        }
        if slot.epoch != plan.epoch || slot.epoch != epoch {
            return Vec::new();
        }
        if let Some(error) = outcome.error {
            return state.retire(
                &plan.target,
                &plan.serial,
                plan.renewal.then_some("unavailable"),
                Some(error),
            );
        }
        state.expire_children(&plan.target, &plan.serial, self.idle_now(0));
        let slot = state.slots.get_mut(&plan.target).unwrap();
        if slot.children.is_empty() {
            return state.retire(&plan.target, &plan.serial, None, None);
        }
        if let Some(early) = slot.early.take() {
            return state.retire(&plan.target, &plan.serial, Some(native_state(&early)), None);
        }
        slot.sending = false;
        slot.ready.send_replace(Some(Ok(())));
        Vec::new()
    }
    fn arm_native_timer(&self, target: &str, serial: &str) {
        let Some(runtime) = self.owner.lock().unwrap().upgrade() else {
            return;
        };
        let clock = runtime.clock.clone();
        let owner = Arc::downgrade(&runtime);
        let mut state = self.idle_state.lock().unwrap();
        let IdleState {
            slots, requests, ..
        } = &mut *state;
        let Some(slot) = slots
            .get_mut(target)
            .filter(|slot| slot.serial == serial && slot.logical && !slot.sending)
        else {
            return;
        };
        let deadline = slot
            .children
            .iter()
            .filter_map(|child| {
                requests
                    .get(child)
                    .map(|request| request.created.saturating_add(TTL))
            })
            .fold(slot.deadline, u64::min);
        if let Some(timer) = slot.timer.take() {
            timer.abort();
        }
        slot.timer_seq += 1;
        let seq = slot.timer_seq;
        let epoch = slot.epoch;
        let target = target.to_string();
        let serial = serial.to_string();
        let delay = Duration::from_millis(deadline.saturating_sub(clock.now_ms()));
        slot.timer = Some(tokio::spawn(async move {
            clock.sleep("native_idle_expiry", delay).await;
            let Some(runtime) = owner.upgrade() else {
                return;
            };
            {
                let mut state = runtime.native.idle_state.lock().unwrap();
                let Some(slot) = state.slots.get_mut(&target).filter(|slot| {
                    slot.serial == serial && slot.epoch == epoch && slot.timer_seq == seq
                }) else {
                    return;
                };
                slot.timer.take();
            }
            runtime
                .native
                .tick_native_slot(&target, &serial, epoch, seq);
        }));
    }
    fn tick_native_slot(&self, target: &str, serial: &str, epoch: u64, seq: u64) {
        #[cfg(not(unix))]
        {
            let _ = (target, serial, epoch, seq);
        }
        #[cfg(unix)]
        {
            let now = self.idle_now(0);
            let snapshot = {
                let mut state = self.idle_state.lock().unwrap();
                if !state.running || state.epoch != epoch {
                    return;
                }
                let Some(slot) = state.slots.get(target).filter(|slot| {
                    slot.serial == serial && slot.logical && slot.timer_seq == seq && !slot.sending
                }) else {
                    return;
                };
                let snapshot = slot.endpoint.clone();
                state.expire_children(target, serial, now);
                if state.slots[target].children.is_empty() {
                    state.retire(target, serial, None, None);
                    return;
                }
                if now < state.slots[target].deadline {
                    drop(state);
                    self.arm_native_timer(target, serial);
                    return;
                }
                snapshot
            };
            let fresh = self.fresh_idle_endpoint(&snapshot);
            let mut state = self.idle_state.lock().unwrap();
            if !state.running || state.epoch != epoch {
                return;
            }
            let Some(slot) = state.slots.get(target).filter(|slot| {
                slot.serial == serial && slot.logical && slot.timer_seq == seq && !slot.sending
            }) else {
                return;
            };
            if !fresh
                .as_ref()
                .is_ok_and(|endpoint| Generation::from_endpoint(endpoint) == slot.generation)
            {
                state.expire_children(target, serial, self.idle_now(0));
                let notices = state.retire(target, serial, Some("unavailable"), None);
                drop(state);
                self.spawn_idle_notices(notices);
                return;
            }
            let now = self.idle_now(0);
            state.expire_children(target, serial, now);
            let slot = state.slots.get_mut(target).unwrap();
            if slot.children.is_empty() {
                state.retire(target, serial, None, None);
                return;
            }
            slot.id = uuid::Uuid::new_v4().to_string();
            slot.deadline = now.saturating_add(TTL);
            slot.sending = true;
            slot.early = None;
            slot.timer_seq += 1;
            if let Some(timer) = slot.timer.take() {
                timer.abort();
            }
            let (ready, _) = watch::channel(None);
            slot.ready = ready;
            let (cancel, cancellation) = watch::channel(false);
            slot.cancel = cancel;
            let (done, _) = watch::channel(false);
            slot.done = done;
            let from = alias::from_identity(&slot.listener.as_ref().unwrap().path);
            let plan = SendPlan {
                target: target.into(),
                serial: serial.into(),
                epoch,
                wire: slot.id.clone(),
                endpoint: fresh.unwrap(),
                from,
                cancel: cancellation,
                renewal: true,
            };
            drop(state);
            self.spawn_native_send(plan);
        }
    }
    pub async fn maintain_idle(&self, runtime: &super::super::Runtime) {
        let pending = runtime
            .state
            .lock()
            .unwrap()
            .subscriptions
            .iter()
            .filter(|(_, record)| record.status == "subscribed")
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        let due = {
            let mut state = self.idle_state.lock().unwrap();
            let abandoned = state
                .requests
                .iter()
                .filter(|(id, request)| request.slot.is_some() && !pending.contains(*id))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in abandoned {
                state.cancel(&id);
            }
            state.prune(runtime.clock.now_ms());
            state
                .slots
                .iter()
                .filter(|(_, slot)| slot.logical && !slot.sending)
                .map(|(target, slot)| {
                    (
                        target.clone(),
                        slot.serial.clone(),
                        slot.epoch,
                        slot.timer_seq,
                    )
                })
                .collect::<Vec<_>>()
        };
        self.idle_returns
            .lock()
            .unwrap()
            .retain(|id, route| route.admitting || pending.contains(id));
        for (target, serial, epoch, seq) in due {
            self.tick_native_slot(&target, &serial, epoch, seq);
        }
        #[cfg(unix)]
        self.reclaim_idle_debt(runtime.clock.now_ms());
    }
    #[cfg(unix)]
    fn reclaim_idle_debt(&self, now: u64) {
        let debts = {
            let mut state = self.idle_state.lock().unwrap();
            if now < state.debt_scan_at {
                return;
            }
            state.debt_scan_at = now.saturating_add(DEBT_SCAN_MS);
            state
                .slots
                .iter()
                .filter(|(_, slot)| !slot.logical && slot.debt.is_some() && *slot.done.borrow())
                .map(|(target, slot)| (target.clone(), slot.serial.clone(), slot.endpoint.clone()))
                .collect::<Vec<_>>()
        };
        for (target, serial, snapshot) in debts {
            let fresh = self.fresh_idle_endpoint(&snapshot);
            let mut state = self.idle_state.lock().unwrap();
            let gone = state.slots.get(&target).is_some_and(|slot| {
                slot.serial == serial
                    && !slot.logical
                    && slot.debt.is_some()
                    && match &fresh {
                        Ok(endpoint) => Generation::from_endpoint(endpoint) != slot.generation,
                        Err(error) => error.starts_with("unknown native idle target"),
                    }
            });
            if gone {
                state.slots.remove(&target);
            }
        }
    }
    pub async fn idle_notice(&self, id: &str, status: &str) -> Result<(), String> {
        let route = self.owner.lock().unwrap().upgrade().and_then(|runtime| {
            let state = runtime.state.lock().unwrap();
            let record = state.subscriptions.get(id)?;
            if !record.native || record.status != status {
                return None;
            }
            let mut routes = self.idle_returns.lock().unwrap();
            if routes.get(id).is_some_and(|route| {
                route.source_identity == record.source_identity && route.target == record.target
            }) {
                routes.remove(id)
            } else {
                None
            }
        });
        let Some(route) = route else {
            return Ok(());
        };
        if !["idle", "exited", "unavailable"].contains(&status) {
            return Ok(());
        }
        #[cfg(unix)]
        {
            write_control_from(self, &route.endpoint, &route.reply, json!({"msgV":1,"msg_id":uuid::Uuid::new_v4().to_string(),"type":"control","action":"peer_idle_notice","orig_msg_id":route.original,"state":status})).await
        }
        #[cfg(not(unix))]
        {
            let _ = route;
            Err("native idle supports Linux/macOS only".into())
        }
    }
}

#[derive(Debug)]
struct SendOutcome {
    error: Option<String>,
    debt: bool,
}
#[cfg(unix)]
async fn send_physical_idle(
    endpoint: &Value,
    mut frame: Value,
    mut cancel: watch::Receiver<bool>,
    budget: Duration,
) -> SendOutcome {
    let failed = |message: String| SendOutcome {
        error: Some(message),
        debt: false,
    };
    if *cancel.borrow() {
        return failed("native idle send cancelled".into());
    }
    if let Err(error) = validate_endpoint(endpoint, None) {
        return failed(error);
    }
    let connection = tokio::select! {
        _ = cancel.changed() => return failed("native idle send cancelled".into()),
        connection = tokio::time::timeout(budget, tokio::net::UnixStream::connect(str_field(endpoint,"_socket"))) => connection,
    };
    let mut stream = match connection {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => return failed(error.to_string()),
        Err(_) => return failed("native idle connect timed out".into()),
    };
    let peer = match stream.peer_cred() {
        Ok(peer) => peer.pid().map(i64::from),
        Err(error) => return failed(error.to_string()),
    };
    if let Err(error) = validate_endpoint(endpoint, peer) {
        return failed(error);
    }
    frame["msgV"] = json!(1);
    let mut bytes = frame.to_string().into_bytes();
    bytes.push(b'\n');
    let deadline = tokio::time::Instant::now() + budget;
    let mut committed = 0;
    let mut error = None;
    while committed < bytes.len() {
        if *cancel.borrow() {
            error = Some("native idle send cancelled".into());
            break;
        }
        let written = tokio::select! {
            _ = cancel.changed() => { error = Some("native idle send cancelled".into()); break; },
            result = tokio::time::timeout_at(deadline, stream.write(&bytes[committed..])) => result,
        };
        match written {
            Ok(Ok(0)) => {
                error = Some("native idle socket closed while writing".into());
                break;
            }
            Ok(Ok(n)) => committed += n,
            Ok(Err(cause)) => {
                error = Some(cause.to_string());
                break;
            }
            Err(_) => {
                error = Some("native idle write timed out".into());
                break;
            }
        }
    }
    if committed > 0 {
        // Once any bytes may be committed, cancellation only stops writing.
        // Full peer EOF within the original budget is the ordering fence.
        let drain = tokio::time::timeout_at(deadline, async {
            stream.shutdown().await?;
            tokio::io::copy(&mut stream, &mut tokio::io::sink())
                .await
                .map(|_| ())
        })
        .await;
        match drain {
            Ok(Ok(())) => {}
            Ok(Err(cause)) => {
                return SendOutcome {
                    error: Some(format!(
                        "native idle write lacks peer-close ordering proof: {cause}"
                    )),
                    debt: true,
                };
            }
            Err(_) => {
                return SendOutcome {
                    error: Some(
                        "native idle write lacks peer-close ordering proof: timeout".into(),
                    ),
                    debt: true,
                };
            }
        }
    }
    if *cancel.borrow() {
        error = Some("native idle send cancelled".into());
    }
    SendOutcome { error, debt: false }
}

#[cfg(unix)]
struct ReplyListener {
    path: PathBuf,
    identity: (u64, u64),
    task: tokio::task::JoinHandle<()>,
}
#[cfg(unix)]
impl Drop for ReplyListener {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        self.task.abort();
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
#[cfg(unix)]
impl Native {
    fn bind_idle_listener(
        &self,
        target: &str,
        serial: &str,
        epoch: u64,
        endpoint: &Value,
        capacity: Arc<tokio::sync::Semaphore>,
    ) -> Result<ReplyListener, String> {
        use std::os::unix::fs::MetadataExt;
        validate_endpoint(endpoint, None)?;
        let directory = std::path::Path::new(str_field(endpoint, "_socket"))
            .parent()
            .ok_or("native idle target socket has no parent")?;
        let path = directory.join(format!(
            "{}-{}.sock",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        let listener = super::super::localrpc::bind_private(&path)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        let identity = (metadata.dev(), metadata.ino());
        let owner = self.owner.lock().unwrap().clone();
        let target = target.to_string();
        let serial = serial.to_string();
        let generation = Generation::from_endpoint(endpoint);
        let local_capacity = Arc::new(tokio::sync::Semaphore::new(8));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let Ok(global_permit) = capacity.clone().try_acquire_owned() else {
                    continue;
                };
                let Ok(local_permit) = local_capacity.clone().try_acquire_owned() else {
                    continue;
                };
                let Some(peer) = alias::accepted_peer(&stream) else {
                    continue;
                };
                let pid = peer.unwrap_or(0);
                if pid > 0 && pid != generation.pid {
                    continue;
                }
                let process = if pid > 0 {
                    alias::process_start(pid as u32)
                } else {
                    String::new()
                };
                if !process.is_empty()
                    && !generation.process.is_empty()
                    && process != generation.process
                {
                    continue;
                }
                let Some(runtime) = owner.upgrade() else {
                    break;
                };
                let weak = Arc::downgrade(&runtime);
                let target = target.clone();
                let serial = serial.clone();
                let task = tokio::spawn(async move {
                    let _permits = (global_permit, local_permit);
                    read_idle_frames(stream, |frame| {
                        let weak = weak.clone();
                        let target = target.clone();
                        let serial = serial.clone();
                        async move {
                            if let Some(runtime) = weak.upgrade() {
                                let notices =
                                    runtime.native.slot_notice(&target, &serial, epoch, &frame);
                                emit(&runtime, notices).await;
                            }
                        }
                    })
                    .await;
                });
                runtime.native.track_idle_task(false, task);
            }
        });
        Ok(ReplyListener {
            path,
            identity,
            task,
        })
    }
    fn slot_notice(&self, target: &str, serial: &str, epoch: u64, frame: &Value) -> Notices {
        if str_field(frame, "type") != "control" || str_field(frame, "action") != "peer_idle_notice"
        {
            return Vec::new();
        }
        let mut state = self.idle_state.lock().unwrap();
        if !state.running || state.epoch != epoch {
            return Vec::new();
        }
        let Some(slot) = state.slots.get_mut(target).filter(|slot| {
            slot.serial == serial
                && slot.epoch == epoch
                && slot.logical
                && slot.id == str_field(frame, "orig_msg_id")
                && slot.generation.socket == alias::socket_from_identity(str_field(frame, "from"))
        }) else {
            return Vec::new();
        };
        if slot.sending {
            if slot.early.is_none() {
                slot.early = Some(native_state(str_field(frame, "state")).into());
            }
            return Vec::new();
        }
        state.expire_children(target, serial, self.idle_now(0));
        state.retire(
            target,
            serial,
            Some(native_state(str_field(frame, "state"))),
            None,
        )
    }
}
#[cfg(unix)]
async fn read_idle_frames<F, Fut>(stream: tokio::net::UnixStream, mut handler: F)
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut reader = BufReader::new(stream);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let mut bytes = Vec::new();
        let result = tokio::time::timeout_at(
            deadline,
            (&mut reader)
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut bytes),
        )
        .await;
        let Ok(Ok(n)) = result else {
            return;
        };
        if n == 0 {
            return;
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.len() > 1024 * 1024 {
            return;
        }
        if let Ok(frame) = serde_json::from_slice::<Value>(&bytes) {
            if frame.is_object() {
                handler(frame).await;
            }
        }
    }
}
#[cfg(unix)]
pub(super) async fn write_control_from(
    _native: &Native,
    endpoint: &Value,
    reply: &Value,
    mut frame: Value,
) -> Result<(), String> {
    // A pinned outcome uses the original from identity even after its alias
    // retires. Do not reacquire a current alias or transfer it to a successor.
    frame["from"] = json!(alias::from_identity(str_field(reply, "_socket")));
    write_control(endpoint, frame).await
}
#[cfg(unix)]
pub(super) async fn write_control(endpoint: &Value, mut frame: Value) -> Result<(), String> {
    validate_endpoint(endpoint, None)?;
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::UnixStream::connect(str_field(endpoint, "_socket")),
    )
    .await
    .map_err(|_| "native connect timed out".to_string())?
    .map_err(|error| error.to_string())?;
    let peer = stream
        .peer_cred()
        .map_err(|error| error.to_string())?
        .pid()
        .map(i64::from);
    validate_endpoint(endpoint, peer)?;
    frame["msgV"] = json!(1);
    let mut bytes = frame.to_string().into_bytes();
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(&bytes))
        .await
        .map_err(|_| "native write outcome unknown".to_string())?
        .map_err(|error| error.to_string())
}
#[cfg(unix)]
pub(super) async fn notice(_runtime: &Shared, frame: &Value, _peer_pid: Option<i64>) -> bool {
    // Main/alias endpoints are never upstream native idle reply addresses.
    str_field(frame, "action") == "peer_idle_notice"
}
struct ReturnPin<'a> {
    native: &'a Native,
    id: String,
    identity: String,
}
impl Drop for ReturnPin<'_> {
    fn drop(&mut self) {
        if let Some(route) = self
            .native
            .idle_returns
            .lock()
            .unwrap()
            .get_mut(&self.id)
            .filter(|route| route.source_identity == self.identity)
        {
            route.admitting = false;
        }
    }
}
#[cfg(unix)]
pub(super) async fn incoming(
    runtime: &Shared,
    source: &Value,
    frame: &Value,
    target: &str,
    reply_endpoint: &Value,
) {
    let original = str_field(frame, "msg_id");
    let identity = json!([
        source["id"],
        source["pid"],
        source["_started_at"],
        source["_proc_start"],
        source["_socket"],
        source["_socket_dev"],
        source["_socket_ino"],
        reply_endpoint["_generation"],
        reply_endpoint["_socket"],
        reply_endpoint["_socket_dev"],
        reply_endpoint["_socket_ino"]
    ])
    .to_string();
    let Ok(child) = control_id(original, &identity) else {
        return;
    };
    let pending = runtime
        .state
        .lock()
        .unwrap()
        .subscriptions
        .iter()
        .filter(|(_, record)| record.status == "subscribed")
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    let accepted = {
        let mut routes = runtime.native.idle_returns.lock().unwrap();
        routes.retain(|id, route| route.admitting || pending.contains(id));
        install_return(
            &mut routes,
            &child,
            Return {
                endpoint: source.clone(),
                reply: reply_endpoint.clone(),
                original: original.into(),
                target: target.into(),
                source_identity: identity.clone(),
                admitting: true,
            },
        )
    };
    if !accepted {
        let _ = reject_control(&runtime.native, source, reply_endpoint, original).await;
        return;
    }
    let _pin = ReturnPin {
        native: &runtime.native,
        id: child.clone(),
        identity: identity.clone(),
    };
    match runtime
        .start_idle(&child, str_field(source, "id"), target, true, "", &identity)
        .await
    {
        Ok(record) => {
            let status = str_field(&record, "status");
            if status != "subscribed" {
                let _ = runtime.native.idle_notice(&child, status).await;
            }
        }
        Err(_) => {
            // Reject only this request's endpoint, never the route of an
            // existing subscription with the same control UUID.
            let route = {
                let mut routes = runtime.native.idle_returns.lock().unwrap();
                if routes.get(&child).is_some_and(|route| {
                    route.source_identity == identity && route.target == target
                }) {
                    routes.remove(&child)
                } else {
                    None
                }
            };
            if let Some(route) = route {
                let _ = reject_control(
                    &runtime.native,
                    &route.endpoint,
                    &route.reply,
                    &route.original,
                )
                .await;
            }
        }
    }
}

#[cfg(unix)]
async fn reject_control(
    native: &Native,
    source: &Value,
    reply: &Value,
    original: &str,
) -> Result<(), String> {
    write_control_from(native, source, reply, json!({"type":"control","action":"peer_idle_notice","msg_id":uuid::Uuid::new_v4().to_string(),"orig_msg_id":original,"state":"unavailable"})).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_control_preserves_original_uuid_instead_of_hashing_its_identity() {
        let original = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            control_id(&original, "source-lifetime-A").unwrap(),
            original
        );
    }
    #[test]
    fn native_control_rejects_opaque_ids_even_though_user_messages_allow_them() {
        assert!(control_id("opaque user message id", "source-lifetime-A").is_err());
    }
    fn route(identity: &str) -> Return {
        Return {
            endpoint: json!({"id":identity}),
            reply: json!({"_socket":identity}),
            original: "original".into(),
            target: "pi/target".into(),
            source_identity: identity.into(),
            admitting: false,
        }
    }
    #[test]
    fn conflicting_control_uuid_never_acquires_another_sources_return_route() {
        let mut routes = HashMap::new();
        assert!(install_return(&mut routes, "id", route("source-A")));
        assert!(install_return(&mut routes, "id", route("source-A")));
        assert!(!install_return(&mut routes, "id", route("source-B")));
        assert_eq!(routes["id"].source_identity, "source-A");
    }
    #[cfg(unix)]
    struct SocketFixture {
        directory: PathBuf,
        listener: tokio::net::UnixListener,
        endpoint: Value,
    }
    #[cfg(unix)]
    impl SocketFixture {
        fn new() -> Self {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let directory = std::env::temp_dir().join(format!(
                "aic-idle-{}",
                &uuid::Uuid::new_v4().to_string()[..8]
            ));
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.join("peer.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let endpoint = json!({"id":"claude/peer","harness":"claude","pid":std::process::id(),"_socket":path,"_socket_dev":metadata.dev(),"_socket_ino":metadata.ino(),"_proc_start":alias::process_start(std::process::id()),"notify_idle":true});
            Self {
                directory,
                listener,
                endpoint,
            }
        }
    }
    #[cfg(unix)]
    impl Drop for SocketFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[cfg(unix)]
    fn runtime() -> Shared {
        use crate::intercom::{
            config::{Config, Identity},
            Runtime, State,
        };
        let (stop, _) = watch::channel(false);
        let (closing, _) = watch::channel(false);
        let runtime = Arc::new(Runtime {
            cfg: Config {
                local_only: true,
                mcp_listen: String::new(),
                ..Config::default()
            },
            clock: crate::clock::Clock::default(),
            identity: Identity::default(),
            machine: "native-idle-fixture".into(),
            stop,
            closing,
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
                relay_workspaces: HashSet::new(),
                supervised: false,
                incoming: Default::default(),
                subscriptions: HashMap::new(),
                idle_stable: HashMap::new(),
                next_epoch: 0,
                next_conn: 0,
                events: std::collections::VecDeque::new(),
                seq: 0,
                remote: Vec::new(),
                relay_tx: None,
                control_outbox: std::collections::VecDeque::new(),
                relay_message_ids: false,
                relay_generation: 0,
                legacy_active: None,
                pending: HashMap::new(),
                aliases: HashMap::new(),
                mcp_status: String::new(),
                native_status: String::new(),
            }),
        });
        *runtime.native.owner.lock().unwrap() = Arc::downgrade(&runtime);
        runtime.native.start_idle_adapter();
        runtime
    }
    #[cfg(unix)]
    fn child(runtime: &Shared, id: &str, origin: &str, target: &str) {
        runtime.state.lock().unwrap().subscriptions.insert(
            id.into(),
            crate::intercom::Idle {
                watcher: origin.into(),
                target: target.into(),
                mode: "query_only".into(),
                status: "subscribed".into(),
                updated_at: runtime.clock.now_ms(),
                created: runtime.clock.now_ms(),
                epoch: None,
                backend: "native".into(),
                remote_origin: String::new(),
                remote_machine: String::new(),
                native: false,
                source_identity: String::new(),
                admitting: false,
                superseded: false,
            },
        );
    }
    #[cfg(unix)]
    impl SocketFixture {
        fn register(&mut self, name: &str, started: u64) {
            use std::os::unix::fs::PermissionsExt;
            let registry = self.directory.join(format!("{}.json", std::process::id()));
            let entry = json!({"pid":std::process::id(),"sessionId":name,"startedAt":started,"kind":"interactive","status":"busy",
                "messagingSocketPath":self.endpoint["_socket"],"procStart":self.endpoint["_proc_start"],"peerFeatures":["notify_idle"]});
            std::fs::write(&registry, entry.to_string()).unwrap();
            std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o600)).unwrap();
            self.endpoint = endpoint_row(&entry, &registry).unwrap();
            self.endpoint["_started_at"] = json!(started);
        }
        async fn request(&self) -> (Value, BufReader<tokio::net::UnixStream>) {
            let (stream, _) = tokio::time::timeout(Duration::from_secs(2), self.listener.accept())
                .await
                .expect("upstream physical request")
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let frame = serde_json::from_str(&line).unwrap();
            let mut tail = [0u8; 1];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), reader.read(&mut tail))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            (frame, reader)
        }
    }
    #[cfg(unix)]
    struct NativeFixture {
        runtime: Shared,
        peer: SocketFixture,
    }
    #[cfg(unix)]
    impl NativeFixture {
        fn new() -> Self {
            let runtime = runtime();
            let mut peer = SocketFixture::new();
            peer.register("peer", 1);
            runtime
                .native
                .rows
                .lock()
                .unwrap()
                .push(peer.endpoint.clone());
            Self { runtime, peer }
        }
        fn add(&self, origin: &str) -> String {
            let id = uuid::Uuid::new_v4().to_string();
            child(
                &self.runtime,
                &id,
                origin,
                str_field(&self.peer.endpoint, "id"),
            );
            id
        }
        fn subscribe(&self, id: &str) -> tokio::task::JoinHandle<Result<(), String>> {
            let runtime = self.runtime.clone();
            let endpoint = self.peer.endpoint.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                runtime
                    .native
                    .subscribe_native(&endpoint, &id, runtime.clock.now_ms())
                    .await
            })
        }
        async fn proven(&self, id: &str) -> Value {
            let sent = self.subscribe(id);
            let (frame, stream) = self.peer.request().await;
            drop(stream);
            tokio::time::timeout(Duration::from_secs(2), sent)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            frame
        }
    }
    #[cfg(unix)]
    impl Drop for NativeFixture {
        fn drop(&mut self) {
            self.runtime.native.stop_idle_state();
        }
    }
    #[cfg(unix)]
    async fn callback(endpoint: &Value, request: &Value, state: &str) {
        let path = alias::socket_from_identity(str_field(request, "from"));
        let mut stream = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::UnixStream::connect(path),
        )
        .await
        .unwrap()
        .unwrap();
        let frame = json!({"type":"control","action":"peer_idle_notice","from":alias::from_identity(str_field(endpoint,"_socket")),"orig_msg_id":request["msg_id"],"state":state});
        stream
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
    }
    #[cfg(unix)]
    async fn status(runtime: &Shared, id: &str, expected: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if runtime.state.lock().unwrap().subscriptions[id].status == expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn dedicated_reply_socket_is_native_shaped_unregistered_and_fans_out() {
        let fixture = NativeFixture::new();
        let first = fixture.add("pi/w");
        let second = fixture.add("pi/w");
        let request = fixture.proven(&first).await;
        fixture
            .runtime
            .native
            .subscribe_native(
                &fixture.peer.endpoint,
                &second,
                fixture.runtime.clock.now_ms(),
            )
            .await
            .unwrap();
        let path = PathBuf::from(alias::socket_from_identity(str_field(&request, "from")));
        assert_eq!(path.parent(), Some(fixture.peer.directory.as_path()));
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!("{}-", std::process::id())));
        assert!(fixture.runtime.native.base.lock().unwrap().is_none());
        assert_eq!(
            std::fs::read_dir(&fixture.peer.directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json"))
                .count(),
            1
        );
        assert_eq!(
            fixture.runtime.native.idle_state.lock().unwrap().slots["claude/peer"]
                .children
                .len(),
            2
        );
        assert!(request.get("message").is_none());
        assert!(request.get("from_mode").is_none());
        callback(&fixture.peer.endpoint, &request, "idle").await;
        status(&fixture.runtime, &first, "idle").await;
        status(&fixture.runtime, &second, "idle").await;
        assert!(!path.exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn replay_survives_discovery_removal_but_rejects_another_watcher() {
        let fixture = NativeFixture::new();
        let id = fixture.add("pi/w");
        let request = fixture.proven(&id).await;
        std::fs::remove_file(str_field(&fixture.peer.endpoint, "_registry")).unwrap();
        callback(&fixture.peer.endpoint, &request, "exited").await;
        status(&fixture.runtime, &id, "exited").await;
        fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &id, 0)
            .await
            .unwrap();
        fixture
            .runtime
            .state
            .lock()
            .unwrap()
            .subscriptions
            .get_mut(&id)
            .unwrap()
            .watcher = "pi/other".into();
        assert!(fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &id, 0)
            .await
            .unwrap_err()
            .contains("conflicting"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn early_notice_waits_for_proven_transport_before_resolving_children() {
        let fixture = NativeFixture::new();
        let id = fixture.add("pi/w");
        let send = fixture.subscribe(&id);
        let (request, stream) = fixture.peer.request().await;
        callback(&fixture.peer.endpoint, &request, "idle").await;
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&id].status,
            "subscribed"
        );
        assert!(
            fixture.runtime.native.idle_state.lock().unwrap().slots["claude/peer"]
                .early
                .is_some()
        );
        drop(stream);
        send.await.unwrap().unwrap();
        status(&fixture.runtime, &id, "idle").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_preserves_physical_fence_until_predecessor_peer_closes() {
        use std::future::Future;
        let fixture = NativeFixture::new();
        let old = fixture.add("pi/old");
        let send = fixture.subscribe(&old);
        let (old_request, old_stream) = fixture.peer.request().await;
        let serial = fixture.runtime.native.idle_state.lock().unwrap().slots["claude/peer"]
            .serial
            .clone();
        fixture.runtime.native.cancel_idle(&old).await.unwrap();
        assert!(send.await.unwrap().is_err());
        assert!(
            !std::path::Path::new(&alias::socket_from_identity(str_field(
                &old_request,
                "from"
            )))
            .exists()
        );
        let next = fixture.add("pi/new");
        let mut successor = Box::pin(fixture.runtime.native.subscribe_native(
            &fixture.peer.endpoint,
            &next,
            0,
        ));
        assert!(
            futures_util::future::poll_fn(|cx| std::task::Poll::Ready(
                successor.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        {
            let state = fixture.runtime.native.idle_state.lock().unwrap();
            assert_eq!(state.slots["claude/peer"].serial, serial);
            assert!(state.slots["claude/peer"].physical_live());
            assert!(!state.requests.contains_key(&next));
        }
        drop(old_stream);
        let (result, (request, ())) = tokio::join!(successor, async {
            let request = fixture.peer.request().await;
            drop(request.1);
            (request.0, ())
        });
        result.unwrap();
        assert_ne!(request["from"], old_request["from"]);
        callback(&fixture.peer.endpoint, &request, "exited").await;
        status(&fixture.runtime, &next, "exited").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn uncertain_close_blocks_same_generation_but_new_started_at_can_replace_it() {
        let mut fixture = NativeFixture::new();
        let old = fixture.add("pi/old");
        let send = fixture.subscribe(&old);
        let (request, stream) = fixture.peer.request().await;
        callback(&fixture.peer.endpoint, &request, "idle").await;
        let error = tokio::time::timeout(Duration::from_secs(7), send)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.contains("ordering"));
        drop(stream);
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&old].status,
            "subscribed",
            "failed transport must discard its early idle"
        );
        let blocked = fixture.add("pi/blocked");
        assert!(fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &blocked, 0)
            .await
            .unwrap_err()
            .contains("ordering"));
        assert_eq!(
            fixture
                .runtime
                .native
                .idle_state
                .lock()
                .unwrap()
                .slots
                .len(),
            1
        );
        fixture.peer.register("peer", 2);
        let next = fixture.add("pi/new");
        let request = fixture.proven(&next).await;
        callback(&fixture.peer.endpoint, &request, "idle").await;
        status(&fixture.runtime, &next, "idle").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_children_and_cancelled_replay_do_not_refresh_upstream() {
        let fixture = NativeFixture::new();
        let first = fixture.add("pi/w");
        let request = fixture.proven(&first).await;
        for _ in 1..32 {
            let id = fixture.add("pi/w");
            fixture
                .runtime
                .native
                .subscribe_native(&fixture.peer.endpoint, &id, 0)
                .await
                .unwrap();
        }
        let extra = fixture.add("pi/w");
        assert!(fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &extra, 0)
            .await
            .unwrap_err()
            .contains("capacity"));
        fixture.runtime.native.cancel_idle(&first).await.unwrap();
        fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &first, 0)
            .await
            .unwrap();
        let next = fixture.add("pi/w");
        fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &next, 0)
            .await
            .unwrap();
        assert_eq!(
            fixture.runtime.native.idle_state.lock().unwrap().slots["claude/peer"].id,
            request["msg_id"]
        );
        callback(&fixture.peer.endpoint, &request, "idle").await;
        status(&fixture.runtime, &next, "idle").await;
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&first].status,
            "subscribed",
            "cancelled child must not receive a sibling outcome"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn renewal_keeps_listener_but_changes_uuid_and_expires_each_child_independently() {
        let fixture = NativeFixture::new();
        let first = fixture.add("pi/first");
        let old = fixture.proven(&first).await;
        let late = fixture.add("pi/late");
        fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &late, 0)
            .await
            .unwrap();
        let now = fixture.runtime.clock.now_ms();
        let (serial, epoch, seq) = {
            let mut state = fixture.runtime.native.idle_state.lock().unwrap();
            state.requests.get_mut(&first).unwrap().created = now - TTL;
            state.requests.get_mut(&late).unwrap().created = now - 11 * 3600 * 1000;
            let slot = state.slots.get_mut("claude/peer").unwrap();
            slot.deadline = now;
            (slot.serial.clone(), slot.epoch, slot.timer_seq)
        };
        fixture
            .runtime
            .native
            .tick_native_slot("claude/peer", &serial, epoch, seq);
        let (renewed, stream) = fixture.peer.request().await;
        assert_eq!(renewed["from"], old["from"]);
        assert_ne!(renewed["msg_id"], old["msg_id"]);
        callback(&fixture.peer.endpoint, &old, "exited").await;
        callback(&fixture.peer.endpoint, &renewed, "idle").await;
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&late].status,
            "subscribed"
        );
        drop(stream);
        status(&fixture.runtime, &late, "idle").await;
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&first].status,
            "subscribed"
        );
        fixture
            .runtime
            .native
            .tick_native_slot("claude/peer", &serial, epoch, seq);
        assert!(fixture
            .runtime
            .native
            .idle_state
            .lock()
            .unwrap()
            .slots
            .is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn renewal_refreshes_registry_and_rejects_replacement_without_sending() {
        let mut fixture = NativeFixture::new();
        let id = fixture.add("pi/w");
        let old = fixture.proven(&id).await;
        fixture.peer.register("peer", 2);
        let (serial, epoch, seq) = {
            let mut state = fixture.runtime.native.idle_state.lock().unwrap();
            let slot = state.slots.get_mut("claude/peer").unwrap();
            slot.deadline = fixture.runtime.clock.now_ms();
            (slot.serial.clone(), slot.epoch, slot.timer_seq)
        };
        fixture
            .runtime
            .native
            .tick_native_slot("claude/peer", &serial, epoch, seq);
        status(&fixture.runtime, &id, "unavailable").await;
        assert!(
            !std::path::Path::new(&alias::socket_from_identity(str_field(&old, "from"))).exists()
        );
        assert!(fixture
            .runtime
            .native
            .idle_state
            .lock()
            .unwrap()
            .slots
            .is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_unlinks_listeners_without_emitting_and_restart_isolated() {
        let fixture = NativeFixture::new();
        let old = fixture.add("pi/w");
        let request = fixture.proven(&old).await;
        tokio::time::timeout(
            Duration::from_millis(200),
            fixture.runtime.native.close_idle(),
        )
        .await
        .unwrap();
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&old].status,
            "subscribed"
        );
        assert!(
            !std::path::Path::new(&alias::socket_from_identity(str_field(&request, "from")))
                .exists()
        );
        let next = fixture.add("pi/new");
        assert!(fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &next, 0)
            .await
            .is_err());
        fixture.runtime.native.start_idle_adapter();
        let renewed = fixture.proven(&next).await;
        assert_ne!(renewed["from"], request["from"]);
        callback(&fixture.peer.endpoint, &renewed, "idle").await;
        status(&fixture.runtime, &next, "idle").await;
    }
    #[test]
    fn replay_history_prunes_oldest_without_evicting_active_children() {
        let mut state = IdleState::default();
        for i in 0..32 {
            state.requests.insert(
                format!("active-{i}"),
                Request {
                    target: "t".into(),
                    origin: "w".into(),
                    created: 0,
                    slot: Some("live".into()),
                    error: None,
                },
            );
        }
        for i in 0..1100 {
            state.requests.insert(
                format!("done-{i:04}"),
                Request {
                    target: "t".into(),
                    origin: "w".into(),
                    created: i + 1,
                    slot: None,
                    error: None,
                },
            );
            state.prune(1);
        }
        assert_eq!(state.requests.len(), 1024);
        assert_eq!(
            state
                .requests
                .values()
                .filter(|request| request.slot.is_some())
                .count(),
            32
        );
        assert!(!state.requests.contains_key("done-0000"));
        assert!(state.requests.contains_key("done-1099"));
        state.prune(TTL + 2000);
        assert_eq!(state.requests.len(), 32);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dedicated_listener_ignores_user_wrong_source_and_old_uuid_then_normalizes_unknown_state(
    ) {
        let fixture = NativeFixture::new();
        let id = fixture.add("pi/w");
        let request = fixture.proven(&id).await;
        let base_frame = json!({"type":"control","action":"peer_idle_notice","from":alias::from_identity(str_field(&fixture.peer.endpoint,"_socket")),"orig_msg_id":request["msg_id"],"state":"idle"});
        assert!(
            notice(
                &fixture.runtime,
                &base_frame,
                Some(i64::from(std::process::id()))
            )
            .await
        );
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&id].status,
            "subscribed"
        );
        let path = alias::socket_from_identity(str_field(&request, "from"));
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        for (field, value) in [
            ("type", "user"),
            ("action", "rename"),
            ("orig_msg_id", "old-uuid"),
            ("from", "uds:/wrong.sock"),
            ("state", "future-state"),
        ] {
            let mut frame = base_frame.clone();
            frame[field] = json!(value);
            stream
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
        stream.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        status(&fixture.runtime, &id, "unavailable").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn new_subscription_retires_replaced_generation_instead_of_inheriting_it() {
        let mut fixture = NativeFixture::new();
        let old = fixture.add("pi/old");
        let original = fixture.proven(&old).await;
        fixture.peer.register("peer", 2);
        let next = fixture.add("pi/new");
        let fresh = fixture.proven(&next).await;
        status(&fixture.runtime, &old, "unavailable").await;
        assert_ne!(fresh["from"], original["from"]);
        callback(&fixture.peer.endpoint, &fresh, "exited").await;
        status(&fixture.runtime, &next, "exited").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn original_control_uuid_collision_rejects_new_endpoint_without_consuming_old_return() {
        let runtime = runtime();
        let mut first = SocketFixture::new();
        first.register("source-a", 1);
        let mut second = SocketFixture::new();
        second.register("source-b", 1);
        runtime
            .native
            .rows
            .lock()
            .unwrap()
            .extend([first.endpoint.clone(), second.endpoint.clone()]);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        runtime.state.lock().unwrap().sessions.insert(
            "pi/target".into(),
            crate::intercom::Session {
                value: json!({"id":"pi/target","harness":"pi","notify_idle":true,"status":"busy"}),
                owner: 1,
                epoch: 1,
                tx,
                disconnect: None,
                caps: HashSet::from(["idle_state_v1".into()]),
                idle_since: None,
            },
        );
        let id = uuid::Uuid::new_v4().to_string();
        let frame = json!({"type":"control","action":"notify_when_idle","msg_id":id});
        let reply_a = json!({"_socket":"/tmp/retired-alias-a.sock","_generation":"a"});
        let reply_b = json!({"_socket":"/tmp/retired-alias-b.sock","_generation":"b"});
        incoming(&runtime, &first.endpoint, &frame, "pi/target", &reply_a).await;
        assert_eq!(
            runtime.state.lock().unwrap().subscriptions[&id].status,
            "subscribed"
        );
        let ((), (rejected, stream)) = tokio::join!(
            incoming(&runtime, &second.endpoint, &frame, "pi/target", &reply_b),
            second.request()
        );
        drop(stream);
        assert_eq!(rejected["orig_msg_id"], id);
        assert_eq!(rejected["state"], "unavailable");
        assert_eq!(
            rejected["from"],
            alias::from_identity("/tmp/retired-alias-b.sock")
        );
        assert_eq!(
            runtime.native.idle_returns.lock().unwrap()[&id].endpoint["id"],
            "claude/source-a"
        );
        assert_eq!(
            runtime.state.lock().unwrap().subscriptions[&id].status,
            "subscribed"
        );
        let ((), (delivered, stream)) =
            tokio::join!(runtime.complete_idle(&id, "idle"), first.request());
        drop(stream);
        assert_eq!(delivered["state"], "idle");
        assert_eq!(delivered["orig_msg_id"], id);
        assert_eq!(
            delivered["from"],
            alias::from_identity("/tmp/retired-alias-a.sock")
        );
        runtime.native.stop_idle_state();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn reverse_return_stays_pinned_until_start_idle_has_admitted_the_record() {
        let runtime = runtime();
        let mut route = route("source");
        route.admitting = true;
        runtime
            .native
            .idle_returns
            .lock()
            .unwrap()
            .insert("id".into(), route);
        runtime.native.maintain_idle(&runtime).await;
        assert_eq!(runtime.native.idle_returns.lock().unwrap().len(), 1);
        runtime
            .native
            .idle_returns
            .lock()
            .unwrap()
            .get_mut("id")
            .unwrap()
            .admitting = false;
        runtime.native.maintain_idle(&runtime).await;
        assert!(runtime.native.idle_returns.lock().unwrap().is_empty());
        runtime.native.stop_idle_state();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn ordering_debt_has_its_own_thirty_two_physical_slot_bound() {
        let fixture = SocketFixture::new();
        let native = Native::new();
        let mut state = IdleState {
            running: true,
            ..IdleState::default()
        };
        for i in 0..32 {
            let (ready, _) = watch::channel(Some(Ok(())));
            let (cancel, _) = watch::channel(false);
            let (done, _) = watch::channel(true);
            state.slots.insert(
                format!("target-{i}"),
                Slot {
                    serial: i.to_string(),
                    epoch: 0,
                    endpoint: fixture.endpoint.clone(),
                    generation: Generation::from_endpoint(&fixture.endpoint),
                    id: uuid::Uuid::new_v4().to_string(),
                    deadline: TTL,
                    children: HashSet::new(),
                    logical: false,
                    sending: false,
                    ready,
                    cancel,
                    done,
                    debt: Some("ordering proof unavailable".into()),
                    error: None,
                    early: None,
                    listener: None,
                    timer: None,
                    timer_seq: 0,
                },
            );
        }
        assert!(state.requests.is_empty());
        let result = native.admit_native_child(
            &mut state,
            "claude/extra",
            "pi/w",
            &uuid::Uuid::new_v4().to_string(),
            fixture.endpoint.clone(),
            Generation::from_endpoint(&fixture.endpoint),
            0,
        );
        assert!(matches!(result,Err(error) if error.contains("capacity")));
        assert_eq!(state.slots.len(), 32);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordering_debt_slot_is_reclaimed_once_its_subject_generation_is_gone() {
        let runtime = runtime();
        let mut peer = SocketFixture::new();
        peer.register("peer", 1);
        let target = str_field(&peer.endpoint, "id").to_string();
        let (ready, _) = watch::channel(Some(Ok(())));
        let (cancel, _) = watch::channel(false);
        let (done, _) = watch::channel(true);
        runtime.native.idle_state.lock().unwrap().slots.insert(
            target.clone(),
            Slot {
                serial: "debt".into(),
                epoch: 0,
                endpoint: peer.endpoint.clone(),
                generation: Generation::from_endpoint(&peer.endpoint),
                id: uuid::Uuid::new_v4().to_string(),
                deadline: TTL,
                children: HashSet::new(),
                logical: false,
                sending: false,
                ready,
                cancel,
                done,
                debt: Some("ordering proof unavailable".into()),
                error: None,
                early: None,
                listener: None,
                timer: None,
                timer_seq: 0,
            },
        );
        runtime.native.maintain_idle(&runtime).await;
        assert!(
            runtime
                .native
                .idle_state
                .lock()
                .unwrap()
                .slots
                .contains_key(&target),
            "a live subject generation keeps its ordering fence"
        );
        std::fs::remove_file(str_field(&peer.endpoint, "_registry")).unwrap();
        runtime.native.idle_state.lock().unwrap().debt_scan_at = 0;
        runtime.native.maintain_idle(&runtime).await;
        assert!(
            !runtime
                .native
                .idle_state
                .lock()
                .unwrap()
                .slots
                .contains_key(&target),
            "a debt slot whose subject is gone must release its physical capacity"
        );
        runtime.native.stop_idle_state();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn independent_targets_do_not_share_a_physical_send_mutex() {
        let fixture = NativeFixture::new();
        let first = fixture.add("pi/first");
        let first_send = fixture.subscribe(&first);
        let (first_request, first_stream) = fixture.peer.request().await;
        let mut second = SocketFixture::new();
        second.register("second", 1);
        fixture
            .runtime
            .native
            .rows
            .lock()
            .unwrap()
            .push(second.endpoint.clone());
        let next = uuid::Uuid::new_v4().to_string();
        child(&fixture.runtime, &next, "pi/second", "claude/second");
        let runtime = fixture.runtime.clone();
        let endpoint = second.endpoint.clone();
        let id = next.clone();
        let next_send =
            tokio::spawn(async move { runtime.native.subscribe_native(&endpoint, &id, 0).await });
        let (next_request, next_stream) = second.request().await;
        drop(next_stream);
        next_send.await.unwrap().unwrap();
        callback(&second.endpoint, &next_request, "idle").await;
        status(&fixture.runtime, &next, "idle").await;
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&first].status,
            "subscribed"
        );
        assert!(
            !first_send.is_finished(),
            "an independent target must not falsely prove the first peer closed"
        );
        drop(first_stream);
        first_send.await.unwrap().unwrap();
        callback(&fixture.peer.endpoint, &first_request, "exited").await;
        status(&fixture.runtime, &first, "exited").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_initial_subscriber_cancels_writes_without_abandoning_close_proof() {
        use std::future::Future;
        let fixture = NativeFixture::new();
        let id = fixture.add("pi/w");
        let mut subscribe = Box::pin(fixture.runtime.native.subscribe_native(
            &fixture.peer.endpoint,
            &id,
            0,
        ));
        assert!(
            futures_util::future::poll_fn(|cx| std::task::Poll::Ready(
                subscribe.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        let (request, stream) = fixture.peer.request().await;
        let done = fixture.runtime.native.idle_state.lock().unwrap().slots["claude/peer"]
            .done
            .subscribe();
        drop(subscribe);
        {
            let state = fixture.runtime.native.idle_state.lock().unwrap();
            let slot = &state.slots["claude/peer"];
            assert!(!slot.logical);
            assert!(slot.physical_live());
            assert!(*slot.cancel.borrow());
            assert!(state.requests[&id].slot.is_none());
        }
        assert!(
            !std::path::Path::new(&alias::socket_from_identity(str_field(&request, "from")))
                .exists()
        );
        drop(stream);
        tokio::time::timeout(Duration::from_secs(2), wait_physical(done))
            .await
            .unwrap()
            .unwrap();
        assert!(fixture
            .runtime
            .native
            .idle_state
            .lock()
            .unwrap()
            .slots
            .is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_renewal_discards_early_idle_and_explicitly_resolves_late_child_unavailable() {
        let fixture = NativeFixture::new();
        let first = fixture.add("pi/first");
        let old = fixture.proven(&first).await;
        let late = fixture.add("pi/late");
        fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &late, 0)
            .await
            .unwrap();
        let now = fixture.runtime.clock.now_ms();
        let (serial, epoch, seq) = {
            let mut state = fixture.runtime.native.idle_state.lock().unwrap();
            state.requests.get_mut(&first).unwrap().created = now - TTL;
            let slot = state.slots.get_mut("claude/peer").unwrap();
            slot.deadline = now;
            (slot.serial.clone(), slot.epoch, slot.timer_seq)
        };
        fixture
            .runtime
            .native
            .tick_native_slot("claude/peer", &serial, epoch, seq);
        let (request, stream) = fixture.peer.request().await;
        let ready = fixture.runtime.native.idle_state.lock().unwrap().slots["claude/peer"]
            .ready
            .subscribe();
        callback(&fixture.peer.endpoint, &old, "exited").await;
        callback(&fixture.peer.endpoint, &request, "idle").await;
        assert_eq!(
            fixture.runtime.state.lock().unwrap().subscriptions[&late].status,
            "subscribed"
        );
        let error = tokio::time::timeout(Duration::from_secs(7), wait_ready(ready))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("ordering"));
        drop(stream);
        status(&fixture.runtime, &late, "unavailable").await;
        assert!(fixture
            .runtime
            .native
            .subscribe_native(&fixture.peer.endpoint, &late, 0)
            .await
            .unwrap_err()
            .contains("ordering"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn physical_idle_send_waits_for_full_peer_close_not_just_a_successful_write() {
        use tokio::io::AsyncReadExt;
        let fixture = SocketFixture::new();
        let endpoint = fixture.endpoint.clone();
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let send = tokio::spawn(async move {
            send_physical_idle(&endpoint,json!({"type":"control","action":"notify_when_idle","msg_id":uuid::Uuid::new_v4().to_string()}),receiver,Duration::from_secs(1)).await
        });
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), fixture.listener.accept())
            .await
            .expect("physical send never reached the vetted socket")
            .unwrap();
        assert_eq!(
            stream.peer_cred().unwrap().pid().map(|pid| pid as u32),
            Some(std::process::id())
        );
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let mut tail = [0u8; 1];
        assert_eq!(
            reader.read(&mut tail).await.unwrap(),
            0,
            "sender must half-close after its line"
        );
        assert!(
            !send.is_finished(),
            "kernel write/half-close alone is not receiver enqueue ordering proof"
        );
        drop(reader);
        assert!(send.await.unwrap().error.is_none());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn committed_idle_write_without_peer_close_retains_explicit_ordering_debt() {
        use tokio::io::AsyncReadExt;
        let fixture = SocketFixture::new();
        let endpoint = fixture.endpoint.clone();
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let send = tokio::spawn(async move {
            send_physical_idle(
                &endpoint,
                json!({"type":"control","action":"notify_when_idle"}),
                receiver,
                Duration::from_millis(30),
            )
            .await
        });
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), fixture.listener.accept())
            .await
            .expect("physical send never reached the vetted socket")
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let mut tail = [0u8; 1];
        assert_eq!(reader.read(&mut tail).await.unwrap(), 0);
        let outcome = send.await.unwrap();
        assert!(
            outcome.debt,
            "a committed request lacking full peer close must fence its successor"
        );
        assert!(outcome.error.unwrap().contains("ordering"));
    }
}
