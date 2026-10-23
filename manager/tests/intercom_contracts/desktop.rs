use super::*;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

struct Desktop {
    cached_busy: bool,
    path: PathBuf,
    requests: mpsc::Receiver<Value>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

fn state_snapshot(busy: bool, revision: u64) -> Value {
    json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner-A","version":11,"targetClientIds":["follower"],"params":{"conversationId":"desktop-A","hostId":"local","change":{"type":"snapshot","revision":revision,"conversationState":{"title":"Desktop fixture","cwd":"/workspace/desktop-A","threadRuntimeStatus":{"type":if busy{"active"}else{"idle"}},"turnHistory":{"kind":"canonical","history":{"entitiesByKey":if busy{json!({"turn-key":{"turnId":"turn-A","status":"inProgress"}})}else{json!({})}}}}}}})
}

fn desktop_write(stream: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    let raw = serde_json::to_vec(value).unwrap();
    stream.write_all(&(raw.len() as u32).to_le_bytes())?;
    stream.write_all(&raw)
}

impl Desktop {
    fn new(
        fixture: &Fixture,
        cached_busy: bool,
        fresh_busy: bool,
        owner_ok: bool,
        drop_after_mutation: bool,
    ) -> Self {
        let directory = fixture.root.join("desktop");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (tx, requests) = mpsc::channel();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
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
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut revision = 1u64;
                let mut live_busy = fresh_busy;
                loop {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut header = [0u8; 4];
                    match stream.read_exact(&mut header) {
                        Ok(()) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(_) => break,
                    }
                    let size = u32::from_le_bytes(header) as usize;
                    if size > 16 * 1024 * 1024 {
                        break;
                    }
                    let mut raw = vec![0; size];
                    if stream.read_exact(&mut raw).is_err() {
                        break;
                    }
                    let Ok(request) = serde_json::from_slice::<Value>(&raw) else {
                        continue;
                    };
                    let _ = tx.send(request.clone());
                    let method = request["method"].as_str().unwrap_or_default();
                    let response = |result: Value| json!({"type":"response","requestId":request["requestId"],"resultType":"success","method":method,"handledByClientId":"owner-A","result":result});
                    match method {
                        "initialize" => {
                            if desktop_write(&mut stream, &response(json!({"clientId":"follower"})))
                                .is_err()
                            {
                                break;
                            }
                            if desktop_write(&mut stream, &state_snapshot(cached_busy, revision))
                                .is_err()
                            {
                                break;
                            }
                        }
                        "thread-owner-discovery" => {
                            let reply = if owner_ok {
                                response(json!(true))
                            } else {
                                json!({"type":"response","requestId":request["requestId"],"resultType":"error","method":method,"error":"fixture owner unavailable"})
                            };
                            if desktop_write(&mut stream, &reply).is_err() {
                                break;
                            }
                        }
                        "thread-stream-following-changed" => {
                            if request["params"]["following"] == true {
                                revision += 1;
                                if desktop_write(&mut stream, &state_snapshot(live_busy, revision))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        "thread-follower-start-turn" | "thread-follower-steer-turn" => {
                            if drop_after_mutation {
                                break;
                            }
                            let base_revision = revision;
                            revision += 1;
                            live_busy = false;
                            let completion = json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner-A","version":11,"targetClientIds":["follower"],"params":{"conversationId":"desktop-A","hostId":"local","change":{"type":"patches","revision":revision,"baseRevision":base_revision,"patches":[{"op":"replace","path":["threadRuntimeStatus","type"],"value":"idle"},{"op":"add","path":["turnHistory","history","entitiesByKey","turn-key"],"value":{"turnId":"turn-A","status":"completed"}}]}}});
                            if desktop_write(&mut stream, &completion).is_err() {
                                break;
                            }
                            if desktop_write(
                                &mut stream,
                                &response(json!({"result":{"id":"turn-A"}})),
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        Self {
            cached_busy,
            path,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn request(&self, method: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let request = self
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("Desktop native method before deadline");
            if request["method"] == method {
                return request;
            }
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn start(fixture: &mut Fixture, desktop: &Desktop) -> super::native::CodexPeer {
    let catalog = super::native::CodexPeer::catalog(fixture, desktop.cached_busy);
    let path = desktop.path.to_str().unwrap().to_string();
    let app_server = catalog.path.to_str().unwrap().to_string();
    fixture.start_with(
        &[],
        &[
            ("CODEX_DESKTOP_IPC_SOCK", &path),
            ("CODEX_APP_SERVER_SOCK", &app_server),
        ],
    );
    fixture.wait_session("codex/desktop-A");
    catalog
}

fn start_shared_scenario(
    fixture: &mut Fixture,
    desktop: &Desktop,
    catalog: &super::native::CodexPeer,
) {
    fixture.start_with(
        &[],
        &[
            ("CODEX_DESKTOP_IPC_SOCK", desktop.path.to_str().unwrap()),
            ("CODEX_APP_SERVER_SOCK", catalog.path.to_str().unwrap()),
        ],
    );
    fixture.wait_session("codex/desktop-A");
    catalog.request("thread/list");
    catalog.request("thread/loaded/list");
    desktop.request("initialize");
    let row = fixture
        .sessions()
        .into_iter()
        .find(|row| row["id"] == "codex/desktop-A")
        .unwrap();
    assert_ne!(
        row["_desktop"], true,
        "real shared thread/list must discover the target before a Desktop owner is trusted"
    );
}

#[test]
fn shared_catalog_disconnect_before_turn_falls_back_to_healthy_desktop_and_idle_observation() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, true, false);
    let catalog = super::native::CodexPeer::catalog(&fixture, false);
    start_shared_scenario(&mut fixture, &desktop, &catalog);
    catalog.assert_passive_catalog();
    drop(catalog);
    let mut sender = fixture.plugin("shared-disconnected", "busy");
    let body = "healthy Desktop owns a real shared-catalog thread after shared disconnect";
    let sent = sender.call(json!({"op":"send","to":"codex/desktop-A","body":body}));
    assert_eq!(
        sent["ok"], true,
        "shared pre-turn ensure failure must still try the healthy Desktop owner: {sent}"
    );
    let request = desktop.request("thread-follower-start-turn");
    assert!(request.to_string().contains(body));
    assert_eq!(request["targetClientId"], "owner-A");
    let read = sender.event("receipt");
    assert_eq!(read["message_id"], sent["message_id"]);
    assert_eq!(read["status"], "read");
    let subscription = sender.ok(json!({"op":"subscribe_idle","to":"codex/desktop-A"}));
    let idle = sender.event("idle_notice");
    assert_eq!(idle["subscription_id"], subscription["subscription_id"]);
    assert_eq!(idle["status"], "idle");
    assert!(
        !desktop.requests.try_iter().any(|request| matches!(
            request["method"].as_str(),
            Some("thread-follower-start-turn" | "thread-follower-steer-turn")
        )),
        "healthy Desktop received a duplicate body"
    );
}

#[test]
fn shared_attach_failure_before_any_send_still_observes_healthy_desktop_idle_without_mutation() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, true, false);
    let catalog = super::native::CodexPeer::catalog(&fixture, false);
    start_shared_scenario(&mut fixture, &desktop, &catalog);
    catalog.assert_passive_catalog();
    drop(catalog);
    let mut watcher = fixture.plugin("shared-idle-only", "busy");
    let subscription = watcher.ok(json!({"op":"subscribe_idle","to":"codex/desktop-A"}));
    desktop.request("thread-owner-discovery");
    let idle = watcher.event("idle_notice");
    assert_eq!(idle["subscription_id"], subscription["subscription_id"]);
    assert_eq!(idle["status"], "idle");
    assert!(
        !desktop.requests.try_iter().any(|request| matches!(
            request["method"].as_str(),
            Some("thread-follower-start-turn" | "thread-follower-steer-turn")
        )),
        "query-only Desktop idle fallback performed a mutation"
    );
}

#[test]
fn shared_loaded_list_unsupported_before_turn_still_uses_healthy_desktop_owner_once() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, true, false);
    let catalog = super::native::CodexPeer::catalog(&fixture, false);
    start_shared_scenario(&mut fixture, &desktop, &catalog);
    catalog.fail_loaded_queries();
    let mut sender = fixture.plugin("shared-unsupported", "busy");
    let body = "shared loaded-list unsupported must not hide a healthy Desktop owner";
    let sent = sender.call(json!({"op":"send","to":"codex/desktop-A","body":body}));
    assert_eq!(
        sent["ok"], true,
        "pre-turn loaded/list failure must still try Desktop: {sent}"
    );
    catalog.request("thread/loaded/list");
    let request = desktop.request("thread-follower-start-turn");
    assert!(request.to_string().contains(body));
    assert_eq!(sender.event("receipt")["message_id"], sent["message_id"]);
    catalog.assert_passive_catalog();
    assert!(
        !desktop.requests.try_iter().any(|request| matches!(
            request["method"].as_str(),
            Some("thread-follower-start-turn" | "thread-follower-steer-turn")
        )),
        "Desktop retried a successful body"
    );
}

#[test]
fn shared_turn_already_written_with_unknown_result_never_retries_on_healthy_desktop() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, true, false);
    let catalog = super::native::CodexPeer::owned_catalog(&fixture);
    start_shared_scenario(&mut fixture, &desktop, &catalog);
    catalog.lose_mutation_reply();
    let mut sender = fixture.plugin("shared-unknown", "busy");
    let sent=sender.call(json!({"op":"send","to":"codex/desktop-A","body":"exactly one app-server mutation, never a second Desktop body"}));
    catalog.request("turn/start");
    assert_ne!(sent["ok"], true);
    assert_eq!(sent["status"], "unknown");
    assert!(
        !desktop.requests.try_iter().any(|request| matches!(
            request["method"].as_str(),
            Some("thread-follower-start-turn" | "thread-follower-steer-turn")
        )),
        "uncertain shared mutation was retried through Desktop"
    );
    catalog.assert_passive_catalog();
}

#[test]
fn desktop_unsolicited_snapshot_from_unknown_owner_does_not_publish_a_thread() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, true, false);
    let path = desktop.path.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("CODEX_DESKTOP_IPC_SOCK", &path)]);
    desktop.request("initialize");
    let deadline = Instant::now() + Duration::from_millis(350);
    loop {
        assert!(
            !fixture
                .sessions()
                .iter()
                .any(|s| s["id"] == "codex/desktop-A"),
            "unowned Desktop broadcast manufactured a trusted session"
        );
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn desktop_fresh_owner_state_overrides_cached_idle_and_steers_with_early_completion() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, true, true, false);
    let _catalog = start(&mut fixture, &desktop);
    let mut sender = fixture.plugin("sender", "busy");
    let sent = sender.ok(json!({"op":"send","to":"codex/desktop-A","body":"Desktop steering"}));
    assert_eq!(sent["status"], "delivered");
    let request = desktop.request("thread-follower-steer-turn");
    assert_eq!(request["version"], 1);
    assert_eq!(request["targetClientId"], "owner-A");
    assert_eq!(request["params"]["conversationId"], "desktop-A");
    assert!(request["params"]["restoreMessage"].is_object());
    assert!(request["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Desktop steering"));
    let receipt = sender.event("receipt");
    assert_eq!(receipt["status"], "read");
    assert_eq!(receipt["message_id"], sent["message_id"]);
    _catalog.assert_passive_catalog();
}

#[test]
fn desktop_fresh_owner_idle_starts_instead_of_steering_cached_busy_turn() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, true, false, true, false);
    let _catalog = start(&mut fixture, &desktop);
    let mut sender = fixture.plugin("sender", "busy");
    let sent = sender.ok(json!({"op":"send","to":"codex/desktop-A","body":"Desktop start"}));
    assert_eq!(sent["status"], "delivered");
    let request = desktop.request("thread-follower-start-turn");
    assert_eq!(request["version"], 2);
    assert_eq!(request["targetClientId"], "owner-A");
    assert_eq!(
        request["params"]["turnStart"]["request"]["threadId"],
        "desktop-A"
    );
    assert_eq!(
        request["params"]["turnStart"]["context"]["inheritThreadSettings"],
        true
    );
    assert_eq!(sender.event("receipt")["message_id"], sent["message_id"]);
    _catalog.assert_passive_catalog();
}

#[test]
fn desktop_owner_discovery_failure_never_mutates_any_thread() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, false, false);
    let _catalog = start(&mut fixture, &desktop);
    let mut sender = fixture.plugin("sender", "busy");
    let response = sender.call(json!({"op":"send","to":"codex/desktop-A","body":"no owner"}));
    assert_ne!(response["ok"], true);
    desktop.request("thread-owner-discovery");
    _catalog.assert_passive_catalog();
    assert!(!desktop
        .requests
        .try_iter()
        .any(|r| r["method"] == "thread-follower-start-turn"
            || r["method"] == "thread-follower-steer-turn"));
}

#[test]
fn desktop_transport_loss_after_mutation_is_unknown_not_retried() {
    let mut fixture = Fixture::new();
    let desktop = Desktop::new(&fixture, false, false, true, true);
    let _catalog = start(&mut fixture, &desktop);
    let mut sender = fixture.plugin("sender", "busy");
    let response =
        sender.call(json!({"op":"send","to":"codex/desktop-A","body":"exactly one attempt"}));
    assert_ne!(response["ok"], true);
    assert_eq!(response["status"], "unknown");
    desktop.request("thread-follower-start-turn");
    _catalog.assert_passive_catalog();
    assert!(!desktop
        .requests
        .try_iter()
        .any(|r| r["method"] == "thread-follower-start-turn"
            || r["method"] == "thread-follower-steer-turn"));
}
