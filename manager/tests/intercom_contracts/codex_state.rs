use super::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};

enum Command {
    Push(Value),
    Disconnect,
}
struct Model {
    reads: HashMap<String, Value>,
    turns: Vec<(u64, String, String)>,
    fail_mutation: bool,
}
struct Host {
    path: PathBuf,
    model: Arc<Mutex<Model>>,
    links: Arc<Mutex<BTreeMap<u64, mpsc::Sender<Command>>>>,
    log: Arc<Mutex<Vec<(u64, Value)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
fn row(id: &str) -> Value {
    json!({"id":id,"name":id,"cwd":"/fixture/codex-state","status":{"type":"idle"}})
}
fn fresh(id: &str, status: &str, turns: Value) -> Value {
    json!({"thread":{"id":id,"status":{"type":status},"turns":turns}})
}
impl Host {
    fn new(fixture: &Fixture) -> Self {
        let path = fixture.root.join("codex-state.sock");
        let listener = common::bind_private_unix(&path);
        listener.set_nonblocking(true).unwrap();
        let model = Arc::new(Mutex::new(Model {
            reads: [
                (
                    "thread-first".into(),
                    fresh("thread-first", "idle", json!([])),
                ),
                (
                    "thread-second".into(),
                    fresh("thread-second", "idle", json!([])),
                ),
            ]
            .into_iter()
            .collect(),
            turns: Vec::new(),
            fail_mutation: false,
        }));
        let links = Arc::new(Mutex::new(BTreeMap::<u64, mpsc::Sender<Command>>::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let shared_model = model.clone();
        let shared_links = links.clone();
        let shared_log = log.clone();
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            let sequence = AtomicU64::new(0);
            while !stopped.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    // BSD-derived systems inherit O_NONBLOCK from the listener.
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let id = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                let model = shared_model.clone();
                let links = shared_links.clone();
                let log = shared_log.clone();
                let stop = stopped.clone();
                let (tx, rx) = mpsc::channel();
                workers.push(thread::spawn(move|| {
                    let Ok(mut ws)=tokio_tungstenite::tungstenite::accept(stream)else{return;};
                    links.lock().unwrap().insert(id, tx);
                    'connection: while !stop.load(Ordering::Relaxed) {
                        for command in rx.try_iter(){match command{Command::Push(value)=>{if ws.send(tokio_tungstenite::tungstenite::Message::text(value.to_string())).is_err(){break 'connection;}},Command::Disconnect=>{let _=ws.close(None);break 'connection;}}}
                        let message=match ws.read(){Ok(message)=>message,Err(tokio_tungstenite::tungstenite::Error::Io(e)) if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut)=>continue,Err(_)=>break};
                        if !message.is_text(){continue;}
                        let Ok(request)=serde_json::from_str::<Value>(message.to_text().unwrap())else{continue;};log.lock().unwrap().push((id,request.clone()));
                        if request.get("id").is_none()||request.get("method").is_none(){continue;}
                        let method=request["method"].as_str().unwrap();
                        let result=match method {
                            "initialize"=>json!({"userAgent":"codex-state-fixture","platformFamily":"unix","platformOs":"linux"}),
                            "thread/list"=>if request["params"]["cursor"]=="catalog-2"{json!({"data":[row("thread-second"),row("thread-unloaded")],"nextCursor":null})}else{json!({"data":[row("thread-first")],"nextCursor":"catalog-2"})},
                            "thread/loaded/list"=>if request["params"]["cursor"]=="loaded-2"{json!({"data":["thread-second"],"nextCursor":null})}else{json!({"data":["thread-first"],"nextCursor":"loaded-2"})},
                            "thread/read"=>model.lock().unwrap().reads.get(request["params"]["threadId"].as_str().unwrap_or_default()).cloned().unwrap_or_else(||fresh("thread-unloaded","notLoaded",json!([]))),
                            "turn/start"|"turn/steer"=>{
                                let mut model=model.lock().unwrap();let thread=request["params"]["threadId"].as_str().unwrap().to_string();let turn=if method=="turn/steer"{request["params"]["expectedTurnId"].as_str().unwrap_or("missing-expected-turn").to_string()}else{format!("sent-turn-{}",model.turns.len()+1)};model.turns.push((id,thread,turn.clone()));
                                if model.fail_mutation {let error=json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"fixture internal error after submission"}});let _=ws.send(tokio_tungstenite::tungstenite::Message::text(error.to_string()));continue;}
                                if method=="turn/steer"{json!({"turnId":turn})}else{json!({"turn":{"id":turn}})}
                            }
                            _=>{let error=json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":format!("forbidden or unexpected host method {method}")}});let _=ws.send(tokio_tungstenite::tungstenite::Message::text(error.to_string()));continue;}
                        };
                        let response=json!({"jsonrpc":"2.0","id":request["id"],"result":result});if ws.send(tokio_tungstenite::tungstenite::Message::text(response.to_string())).is_err(){break;}
                    }
                    links.lock().unwrap().remove(&id);
                }));
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        Self {
            path,
            model,
            links,
            log,
            stop,
            worker: Some(worker),
        }
    }
    fn start(&self, fixture: &mut Fixture) {
        fixture.start_with(
            &[],
            &[("CODEX_APP_SERVER_SOCK", self.path.to_str().unwrap())],
        );
        fixture.wait_session("codex/thread-first");
    }
    fn push(&self, value: Value) {
        let links = self.links.lock().unwrap();
        links
            .iter()
            .next_back()
            .expect("live persistent host connection")
            .1
            .send(Command::Push(value))
            .unwrap();
    }
    fn notice(&self, method: &str, params: Value) {
        self.push(json!({"jsonrpc":"2.0","method":method,"params":params}));
    }
    fn last_turn(&self) -> (u64, String, String) {
        self.model
            .lock()
            .unwrap()
            .turns
            .last()
            .expect("accepted native turn")
            .clone()
    }
    fn no_mutation(&self) {
        assert!(
            self.model.lock().unwrap().turns.is_empty(),
            "invalid fresh state still mutated a native turn"
        );
        assert!(
            !self
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|(_, r)| r["method"] == "thread/resume"),
            "adapter resumed an unloaded thread"
        );
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn wait_row(fixture: &Fixture, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(row) = fixture
            .sessions()
            .into_iter()
            .find(|row| row["id"] == id && predicate(row))
        {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "Codex cached row {id} never reflected host event"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn codex_catalog_and_loaded_pages_are_unioned_on_one_persistent_connection() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.start(&mut fixture);
    fixture.wait_session("codex/thread-second");
    fixture.wait_session("codex/thread-unloaded");
    for _ in 0..3 {
        fixture.sessions();
    }
    let log = host.log.lock().unwrap();
    assert!(log
        .iter()
        .any(|(_, r)| r["method"] == "thread/list" && r["params"]["cursor"] == "catalog-2"));
    assert!(log
        .iter()
        .any(|(_, r)| r["method"] == "thread/loaded/list" && r["params"]["cursor"] == "loaded-2"));
    assert_eq!(
        log.iter()
            .filter(|(_, r)| r["method"] == "initialize")
            .count(),
        1,
        "directory reads opened disposable hosts instead of reusing one connection"
    );
}

#[test]
fn codex_notifications_patch_old_page_name_status_and_close_without_full_rescan() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.start(&mut fixture);
    fixture.wait_session("codex/thread-second");
    host.notice(
        "thread/name/updated",
        json!({"threadId":"thread-second","name":"renamed old-page thread"}),
    );
    wait_row(&fixture, "codex/thread-second", |r| {
        r["name"] == "renamed old-page thread"
    });
    host.notice(
        "thread/status/changed",
        json!({"threadId":"thread-second","status":{"type":"active"}}),
    );
    wait_row(&fixture, "codex/thread-second", |r| r["status"] == "active");
    host.notice("thread/closed", json!({"threadId":"thread-second"}));
    fixture.wait_absent("codex/thread-second");
}

#[test]
fn codex_busy_without_active_turn_and_mismatched_fresh_thread_never_mutate() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.model.lock().unwrap().reads.insert(
        "thread-first".into(),
        fresh("thread-first", "active", json!([])),
    );
    host.start(&mut fixture);
    let mut sender = fixture.plugin("sender", "busy");
    assert_ne!(
        sender.call(json!({"op":"send","to":"codex/thread-first","body":"no active turn"}))["ok"],
        true
    );
    host.no_mutation();
    host.model.lock().unwrap().reads.insert(
        "thread-first".into(),
        fresh("wrong-thread", "idle", json!([])),
    );
    assert_ne!(
        sender.call(json!({"op":"send","to":"codex/thread-first","body":"wrong read identity"}))
            ["ok"],
        true
    );
    host.no_mutation();
}

#[test]
fn codex_steers_latest_active_turn_once_and_internal_error_never_retries_mutation() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.model.lock().unwrap().reads.insert("thread-first".into(),fresh("thread-first","active",json!([{"id":"old","status":"completed"},{"id":"active-earlier","status":"inProgress"},{"id":"active-latest","status":"active"}])));
    host.model.lock().unwrap().fail_mutation = true;
    host.start(&mut fixture);
    let mut sender = fixture.plugin("sender", "busy");
    let failed =
        sender.call(json!({"op":"send","to":"codex/thread-first","body":"one steering attempt"}));
    assert_ne!(failed["ok"], true);
    assert_eq!(failed["status"], "unknown");
    let model = host.model.lock().unwrap();
    assert_eq!(model.turns.len(), 1, "uncertain mutation was retried");
    assert_eq!(model.turns[0].2, "active-latest");
    drop(model);
    assert!(!host
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, r)| r["method"] == "turn/start" || r["method"] == "thread/resume"));
}

#[test]
fn codex_failed_interrupted_and_missing_completion_status_never_emit_read() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.start(&mut fixture);
    let mut sender = fixture.plugin("sender", "busy");
    for status in [Some("failed"), Some("interrupted"), None] {
        sender
            .ok(json!({"op":"send","to":"codex/thread-first","body":"completion must be proven"}));
        let (_, thread, turn) = host.last_turn();
        let mut completion = json!({"threadId":thread,"turn":{"id":turn}});
        if let Some(status) = status {
            completion["turn"]["status"] = json!(status);
        }
        host.notice("turn/completed", completion);
        sender.no_event();
    }
    let sent = sender.ok(json!({"op":"send","to":"codex/thread-first","body":"actual completion"}));
    let (_, thread, turn) = host.last_turn();
    host.notice(
        "turn/completed",
        json!({"threadId":thread,"turn":{"id":turn,"status":"completed"}}),
    );
    let receipt = sender.event("receipt");
    assert_eq!(receipt["status"], "read");
    assert_eq!(receipt["message_id"], sent["message_id"]);
}

#[test]
fn codex_new_connection_completion_cannot_read_an_old_generation_route() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.start(&mut fixture);
    let mut sender = fixture.plugin("sender", "busy");
    let old = sender.ok(json!({"op":"send","to":"codex/thread-first","body":"old connection"}));
    let (generation, thread, turn) = host.last_turn();
    host.links
        .lock()
        .unwrap()
        .get(&generation)
        .unwrap()
        .send(Command::Disconnect)
        .unwrap();
    let mut observer = fixture.plugin("reconnect-observer", "busy");
    observer.ok(json!({"op":"subscribe_idle","to":"codex/thread-first"}));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if host
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(id, r)| *id != generation && r["method"] == "initialize")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "persistent host never reconnected for fresh idle observation"
        );
        thread::sleep(Duration::from_millis(10));
    }
    host.notice(
        "turn/completed",
        json!({"threadId":thread,"turn":{"id":turn,"status":"completed"}}),
    );
    sender.no_event();
    let sent = sender.ok(json!({"op":"send","to":"codex/thread-first","body":"new connection"}));
    assert_ne!(sent["message_id"], old["message_id"]);
    let (new_generation, thread, turn) = host.last_turn();
    assert_ne!(new_generation, generation);
    host.notice(
        "turn/completed",
        json!({"threadId":thread,"turn":{"id":turn,"status":"completed"}}),
    );
    assert_eq!(sender.event("receipt")["message_id"], sent["message_id"]);
}

#[test]
fn codex_fresh_busy_blocks_cached_idle_until_a_real_fresh_idle_observation() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.model.lock().unwrap().reads.insert(
        "thread-first".into(),
        fresh(
            "thread-first",
            "active",
            json!([{"id":"active","status":"inProgress"}]),
        ),
    );
    host.start(&mut fixture);
    let mut watcher = fixture.plugin("watcher", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"codex/thread-first"}));
    let deadline = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < deadline {
        assert_eq!(
            watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
                ["status"],
            "subscribed",
            "cached idle overrode fresh active native turn"
        );
        thread::sleep(Duration::from_millis(20));
    }
    host.model.lock().unwrap().reads.insert(
        "thread-first".into(),
        fresh("thread-first", "idle", json!([])),
    );
    host.notice(
        "thread/status/changed",
        json!({"threadId":"thread-first","status":{"type":"idle"}}),
    );
    let notice = watcher.event("idle_notice");
    assert_eq!(notice["status"], "idle");
    assert_eq!(notice["subscription_id"], subscribed["subscription_id"]);
}

#[test]
fn codex_approval_requests_are_passive_needs_user_until_resolved() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.start(&mut fixture);
    let mut watcher = fixture.plugin("approval-watcher", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"codex/thread-first"}));
    host.push(json!({"jsonrpc":"2.0","id":"approval-1","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-first"}}));
    assert!(
        !host
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, r)| r["id"] == "approval-1"),
        "native approval was automatically answered"
    );
    let deadline = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < deadline {
        assert_eq!(
            watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
                ["status"],
            "subscribed",
            "fresh idle thread/read ignored outstanding approval"
        );
        thread::sleep(Duration::from_millis(20));
    }
    host.notice(
        "serverRequest/resolved",
        json!({"threadId":"thread-first","requestId":"approval-1"}),
    );
    wait_row(&fixture, "codex/thread-first", |row| {
        row["status"] == "idle"
    });
    assert_eq!(watcher.event("idle_notice")["status"], "idle");
}

#[test]
fn codex_fresh_read_identity_mismatch_cannot_complete_idle_watch_or_claim_exit() {
    let mut fixture = Fixture::new();
    let host = Host::new(&fixture);
    host.model.lock().unwrap().reads.insert(
        "thread-first".into(),
        fresh("wrong-thread", "idle", json!([])),
    );
    host.start(&mut fixture);
    let mut watcher = fixture.plugin("watcher", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"codex/thread-first"}));
    let deadline = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < deadline {
        assert_eq!(
            watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
                ["status"],
            "subscribed",
            "unavailable identity observation completed an idle watch"
        );
        thread::sleep(Duration::from_millis(20));
    }
    watcher.no_event();
    assert!(
        host.log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, request)| request["method"] == "thread/read"
                && request["params"]["threadId"] == "thread-first"),
        "identity mismatch path never obtained a fresh observation"
    );
    host.model.lock().unwrap().reads.insert(
        "thread-first".into(),
        fresh("thread-first", "idle", json!([])),
    );
    let notice = watcher.event("idle_notice");
    assert_eq!(notice["subscription_id"], subscribed["subscription_id"]);
    assert_eq!(notice["status"], "idle");
}
