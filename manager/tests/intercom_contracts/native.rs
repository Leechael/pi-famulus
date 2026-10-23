use super::*;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

pub(super) struct ClaudePeer {
    process: SourceProcess,
    pub(super) path: PathBuf,
    socket_identity: (u64, u64),
    frames: mpsc::Receiver<Value>,
    input: std::sync::Mutex<std::process::ChildStdin>,
    worker: Option<thread::JoinHandle<()>>,
}

const CLAUDE_PEER: &str = r#"
import json, os, select, socket, sys
listener = socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[1])
os.chmod(sys.argv[1], 0o600)
listener.listen(16)
print(json.dumps({'fixture_ready': True}), flush=True)
while True:
    ready, _, _ = select.select([listener, sys.stdin], [], [], 0.1)
    if sys.stdin in ready:
        line = sys.stdin.readline()
        if not line:
            break
        command = json.loads(line)
        with socket.socket(socket.AF_UNIX) as outgoing:
            outgoing.settimeout(2)
            outgoing.connect(command['send_to'])
            outgoing.sendall((json.dumps(command['frame'])+'\n').encode())
    if listener in ready:
        stream, _ = listener.accept()
        with stream:
            stream.settimeout(1)
            data = b''
            try:
                while b'\n' not in data:
                    chunk = stream.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                if data:
                    print(data.split(b'\n')[0].decode(), flush=True)
            except TimeoutError:
                pass
"#;

struct SourceProcess(Child);

impl Drop for SourceProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl ClaudePeer {
    pub(super) fn new(fixture: &Fixture) -> Self {
        Self::with_flags(fixture, &[])
    }

    pub(super) fn with_flags(fixture: &Fixture, flags: &[&str]) -> Self {
        Self::namespace_flags(fixture, "native", "native-fixture", flags)
    }

    fn namespace_flags(
        fixture: &Fixture,
        directory: &str,
        session_id: &str,
        flags: &[&str],
    ) -> Self {
        Self::socket_namespace_flags(fixture, directory, session_id, "claude.sock", flags)
    }

    pub(super) fn socket_namespace_flags(
        fixture: &Fixture,
        directory: &str,
        session_id: &str,
        socket_name: &str,
        flags: &[&str],
    ) -> Self {
        let dir = fixture.root.join(directory);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join(socket_name);
        let mut process = SourceProcess(
            Command::new("python3")
                .args(["-u", "-c", CLAUDE_PEER])
                .arg(&path)
                .args(flags)
                .current_dir(&fixture.root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .env("HOME", &fixture.root)
                .env("CLAUDE_CONFIG_DIR", fixture.root.join(".claude"))
                .env_remove("CLAUDE_CODE_ENTRYPOINT")
                .spawn()
                .unwrap(),
        );
        let input = process.0.stdin.take().unwrap();
        let output = process.0.stdout.take().unwrap();
        let (tx, frames) = mpsc::channel();
        let worker = thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(frame);
                }
            }
        });
        assert_eq!(
            frames
                .recv_timeout(TIMEOUT)
                .expect("native fixture readiness")["fixture_ready"],
            true
        );
        let pid = process.0.id();
        #[cfg(target_os = "linux")]
        let proc_start = {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            stat[stat.rfind(')').unwrap() + 1..]
                .split_whitespace()
                .nth(19)
                .unwrap()
                .to_string()
        };
        #[cfg(not(target_os = "linux"))]
        let proc_start = String::from_utf8(
            Command::new("ps")
                .args(["-o", "lstart=", "-p", &pid.to_string()])
                .env("LC_ALL", "C")
                .env("TZ", "UTC")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let registry = fixture.root.join(".claude/sessions");
        fs::create_dir_all(&registry).unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
        let file = registry.join(format!("{pid}.json"));
        fs::write(
            &file,
            serde_json::to_vec(&json!({
                "pid":pid, "procStart":proc_start, "sessionId":session_id, "cwd":fixture.root,
                "kind":"interactive", "entrypoint":"cli", "version":"2.1.302", "peerProtocol":1,
                "peerFeatures":["notify_idle"], "messagingSocketPath":path,
                "name":"Claude fixture", "status":"busy", "updatedAt":123456789
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let socket_identity = (metadata.dev(), metadata.ino());
        Self {
            process,
            path,
            socket_identity,
            frames,
            input: std::sync::Mutex::new(input),
            worker: Some(worker),
        }
    }

    pub(super) fn frame(&self) -> Value {
        self.frame_for("unlabelled")
    }

    pub(super) fn frame_for(&self, phase: &str) -> Value {
        assert!(common::pid_alive(self.process.0.id()));
        self.frames
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|error| panic!("native Claude frame {phase}: {error:?}"))
    }

    pub(super) fn no_frame(&self) {
        match self.frames.recv_timeout(Duration::from_millis(150)) {
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            other => panic!("unsupported/unexpected native wire frame: {other:?}"),
        }
    }

    pub(super) fn remove_registry(&self, fixture: &Fixture) {
        fs::remove_file(
            fixture
                .root
                .join(".claude/sessions")
                .join(format!("{}.json", self.process.0.id())),
        )
        .unwrap();
    }

    pub(super) fn send_control(&self, destination: &str, frame: Value) {
        self.input
            .lock()
            .unwrap()
            .write_all(format!("{}\n", json!({"send_to":destination,"frame":frame})).as_bytes())
            .unwrap();
    }

    pub(super) fn is_alive(&self) -> bool {
        common::pid_alive(self.process.0.id())
    }

    pub(super) fn receipt(&self, user: &Value, status: &str) {
        let destination = user["from"]
            .as_str()
            .expect("native return socket")
            .strip_prefix("uds:")
            .unwrap();
        let frame = json!({"type":"control","action":"peer_message_status","from":format!("uds:{}",self.path.display()),"orig_msg_id":user["msg_id"],"status":status});
        self.send_control(destination, frame);
    }
}

impl Drop for ClaudePeer {
    fn drop(&mut self) {
        let _ = self.process.0.kill();
        let _ = self.process.0.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.socket_identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[path = "native_extra.rs"]
mod extra;

pub(super) fn alias_record(fixture: &Fixture, id: &str) -> Value {
    extra::alias_record(fixture, id)
}

pub(super) fn native_user(peer: &ClaudePeer, id: &str, body: &str) -> Value {
    json!({"msg_v":1,"type":"user","from":format!("uds:{}",peer.path.display()),"from_name":"native","from_mode":"prompting","msg_id":id,"message":{"role":"user","content":body}})
}

fn await_session(fixture: &Fixture, id: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !fixture.sessions().iter().any(|s| s["id"] == id) {
        assert!(
            Instant::now() < deadline,
            "{id} not discovered: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn claude_native_delivery_waits_for_native_held_and_terminal_receipts() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    fixture.start();
    await_session(&fixture, "claude/native-fixture");
    let mut sender = fixture.plugin("sender", "busy");
    let sent = sender.ok(json!({"op":"send","to":"claude/native-fixture","body":"native body","from_mode":"prompting"}));
    assert_eq!(
        sent["status"], "accepted",
        "native delivery must not synthesize delivered"
    );
    let user = native.frame();
    assert_eq!(user["type"], "user");
    assert_eq!(user["msg_id"], sent["message_id"]);
    let content = user["message"]["content"].as_str().unwrap();
    assert!(content.contains("native body"));
    assert!(content.contains("pi/sender"));
    assert!(content.contains("from-mode=\"prompting\""));
    native.receipt(&user, "held");
    let held = sender.event("receipt");
    assert_eq!(held["status"], "held");
    assert_eq!(held["message_id"], sent["message_id"]);
    native.receipt(&user, "denied");
    let denied = sender.event("receipt");
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["message_id"], sent["message_id"]);
    sender.no_event();
}

#[test]
fn native_aliases_for_two_pi_sessions_with_one_source_pid_have_distinct_live_processes() {
    let mut fixture = Fixture::new();
    fixture.start();
    let _one = fixture.plugin("one", "busy");
    let _two = fixture.plugin("two", "busy");
    let directory = fixture.root.join(".claude/sessions");
    let deadline = Instant::now() + TIMEOUT;
    let entries = loop {
        let entries: Vec<Value> = fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .filter(|entry| entry["sessionId"] == "pi/one" || entry["sessionId"] == "pi/two")
            .collect();
        if entries.len() == 2 {
            break entries;
        }
        assert!(
            Instant::now() < deadline,
            "two Pi native aliases not published: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(
        entries[0]["pid"], entries[1]["pid"],
        "one native PID cannot publish two identities"
    );
    for entry in &entries {
        let pid = entry["pid"].as_u64().unwrap() as u32;
        assert_ne!(
            pid,
            std::process::id(),
            "idle-capable alias needs its own real listener process"
        );
        assert!(
            common::pid_alive(pid),
            "published alias process is not alive: {entry}"
        );
        let path = PathBuf::from(entry["messagingSocketPath"].as_str().unwrap());
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), pid.to_string());
        assert!(
            UnixStream::connect(path).is_ok(),
            "published native alias must accept"
        );
    }
}

#[test]
fn native_terminal_receipt_retries_after_pi_origin_reconnect() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    let address = super::network::vacant_address();
    let listen = address.to_string();
    fixture.start_with(&["--mcp-listen", &listen], &[]);
    await_session(&fixture, "claude/native-fixture");
    let mut sender = fixture.plugin("sender", "busy");
    let sent =
        sender.ok(json!({"op":"send","to":"claude/native-fixture","body":"retain return route"}));
    let user = native.frame();
    native.receipt(&user, "held");
    assert_eq!(sender.event("receipt")["status"], "held");
    drop(sender);
    fixture.wait_absent("pi/sender");
    native.receipt(&user, "denied");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let status = super::network::tool(
            address,
            &fixture.mcp_token(),
            "message_status",
            json!({"message_id":sent["message_id"]}),
        );
        if status["status"] == "denied" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native terminal not observed before origin reconnect"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let mut sender = fixture.plugin("sender", "busy");
    native.receipt(&user, "denied");
    let terminal = sender.event("receipt");
    assert_eq!(terminal["message_id"], sent["message_id"]);
    assert_eq!(terminal["status"], "denied");
    native.receipt(&user, "denied");
    sender.no_event();
}

#[test]
fn exec_upgrade_restarts_the_bridge_from_its_configuration_and_reaps_predecessor_aliases() {
    let mut fixture = Fixture::new();
    let binary = fixture.root.join("pi-famulus");
    fs::copy(BIN, &binary).unwrap();
    fixture.binary = binary;
    fixture.start();
    let pid = fixture.daemon_pid();
    let _plugin = fixture.plugin("before-upgrade", "busy");
    let directory = fixture.root.join(".claude/sessions");
    let deadline = Instant::now() + TIMEOUT;
    let old_alias = loop {
        let aliases: Vec<Value> = fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .filter(|entry| entry["sessionId"] == "pi/before-upgrade")
            .collect();
        if let Some(alias) = aliases.into_iter().next() {
            break alias;
        }
        assert!(
            Instant::now() < deadline,
            "alias before upgrade not published"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let output = common::run_cli(
        &fixture.root.join("manager"),
        &["upgrade"],
        Duration::from_secs(30),
    );
    assert!(
        output.status.success(),
        "upgrade failed: {} {}",
        output.stdout,
        output.stderr
    );
    assert!(
        output.stdout.contains("upgraded in place"),
        "upgrade was not executed: {}",
        output.stdout
    );
    assert_eq!(fixture.daemon_pid(), pid);
    let old_pid = old_alias["pid"].as_u64().unwrap() as u32;
    let deadline = Instant::now() + TIMEOUT;
    while common::pid_alive(old_pid) {
        assert!(
            Instant::now() < deadline,
            "old alias process survived exec upgrade: {old_alias}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + TIMEOUT;
    while UnixStream::connect(&fixture.sock).is_err() {
        assert!(
            Instant::now() < deadline,
            "intercom socket did not return after exec upgrade: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let mut sender = fixture.plugin("post-upgrade-sender", "busy");
    let mut target = fixture.plugin("post-upgrade-target", "busy");
    let sent =
        sender.ok(json!({"op":"send","to":"pi/post-upgrade-target","body":"upgrade kept config"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    let status = fixture.run(&["status", "--json"]);
    assert_cli_ok(&status);
    let report: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["pid"], pid);
    let mut hostname = [0 as libc::c_char; 256];
    assert_eq!(
        unsafe { libc::gethostname(hostname.as_mut_ptr(), hostname.len()) },
        0
    );
    let host = unsafe { std::ffi::CStr::from_ptr(hostname.as_ptr()) }
        .to_str()
        .unwrap();
    assert_eq!(report["machine"], format!("fixture-host@{host}"));
}

#[test]
fn exec_upgrade_keeps_restored_tasks_when_intercom_cannot_restart() {
    let mut fixture = Fixture::new();
    let binary = fixture.root.join("pi-famulus");
    fs::copy(BIN, &binary).unwrap();
    fixture.binary = binary;
    fixture.start();
    let pid = fixture.daemon_pid();
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_ext("restore-degraded");
    let task = manager.request_ok(json!({"type":"start","kind":"shell","command":"sleep 60","cwd":fixture.root,"env":{},"run_in_background":true}));
    let task_id = task["task_id"].clone();
    let task_pid = task["pid"].as_u64().unwrap() as u32;
    // The new image reloads the device identity; an unreadable one fails
    // intercom startup there and only there.
    fs::write(fixture.root.join("state/device.json"), b"not json").unwrap();
    let output = common::run_cli(
        &fixture.root.join("manager"),
        &["upgrade"],
        Duration::from_secs(30),
    );
    assert!(
        output.status.success() && output.stdout.contains("upgraded in place"),
        "upgrade with failing intercom restart: {} {} {}",
        output.stdout,
        output.stderr,
        fixture.log()
    );
    assert!(
        common::pid_alive(pid),
        "restored daemon exited: {}",
        fixture.log()
    );
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    assert_eq!(manager.hello_ext("restore-degraded")["pid"], pid);
    assert!(
        common::pid_alive(task_pid),
        "restored task process was killed"
    );
    let listed = manager.request_ok(json!({"type":"list"}));
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["task_id"] == task_id && t["status"] == "running"),
        "restored task did not survive the intercom failure: {listed}"
    );
    manager.request_ok(json!({"type":"stop","task_id":task_id}));
}

pub(super) struct CodexPeer {
    pub(super) path: PathBuf,
    requests: mpsc::Receiver<Value>,
    busy: Arc<AtomicBool>,
    scenario: Arc<std::sync::atomic::AtomicU8>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CodexPeer {
    fn new(fixture: &Fixture, busy_turn: bool) -> Self {
        Self::with_thread(fixture, busy_turn, "thread-A", true, json!([]))
    }

    pub(super) fn catalog(fixture: &Fixture, busy_turn: bool) -> Self {
        Self::with_thread(fixture, busy_turn, "desktop-A", false, json!([]))
    }

    pub(super) fn owned_catalog(fixture: &Fixture) -> Self {
        Self::with_thread(fixture, false, "desktop-A", true, json!([]))
    }

    pub(super) fn fail_loaded_queries(&self) {
        self.scenario.store(1, Ordering::Relaxed);
    }
    pub(super) fn lose_mutation_reply(&self) {
        self.scenario.store(2, Ordering::Relaxed);
    }

    pub(super) fn hooks(fixture: &Fixture, hooks: Value) -> Self {
        Self::with_thread(fixture, false, "thread-A", true, hooks)
    }

    fn with_thread(
        fixture: &Fixture,
        busy_turn: bool,
        thread_id: &str,
        loaded: bool,
        hooks: Value,
    ) -> Self {
        let thread_id = thread_id.to_string();
        let path = fixture.root.join("codex-host.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let busy = Arc::new(AtomicBool::new(busy_turn));
        let active = busy.clone();
        let scenario = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let behavior = scenario.clone();
        let (tx, requests) = mpsc::channel();
        let worker = thread::spawn(move || {
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
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let Ok(mut ws) = tokio_tungstenite::tungstenite::accept(stream) else {
                    continue;
                };
                while !stopped.load(Ordering::Relaxed) {
                    let message = match ws.read() {
                        Ok(message) => message,
                        Err(tokio_tungstenite::tungstenite::Error::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    };
                    if !message.is_text() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<Value>(message.to_text().unwrap())
                    else {
                        continue;
                    };
                    let _ = tx.send(request.clone());
                    let method = request["method"].as_str().unwrap_or_default();
                    if request.get("id").is_none() {
                        continue;
                    }
                    if method == "thread/loaded/list" && behavior.load(Ordering::Relaxed) == 1 {
                        let error = json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":"fixture older host lacks thread/loaded/list"}});
                        if ws
                            .send(tokio_tungstenite::tungstenite::Message::text(
                                error.to_string(),
                            ))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    if matches!(method, "turn/start" | "turn/steer")
                        && behavior.load(Ordering::Relaxed) == 2
                    {
                        break;
                    }
                    let result = match method {
                        "initialize" => {
                            json!({"userAgent":"codex-fixture","platformFamily":"unix","platformOs":"linux"})
                        }
                        "thread/list" => {
                            json!({"data":[{"id":thread_id,"name":"Codex fixture","cwd":"/fixture/codex","status":if active.load(Ordering::Relaxed){"active"}else{"idle"}}],"nextCursor":null})
                        }
                        "hooks/list" => json!({"data":[{"hooks":hooks}],"errors":[]}),
                        "config/batchWrite" => json!({}),
                        "thread/loaded/list" => {
                            json!({"data":if loaded{json!([thread_id])}else{json!([])},"nextCursor":null})
                        }
                        "thread/read" => {
                            json!({"thread":{"id":thread_id,"name":"Codex fixture","cwd":"/fixture/codex","status":if active.load(Ordering::Relaxed){"active"}else{"idle"},"turns":if active.load(Ordering::Relaxed){json!([{"id":"turn-A","status":"inProgress"}])}else{json!([])}}})
                        }
                        "turn/start" | "turn/steer" => {
                            // Real host can notify before replying to the send request.
                            let notice = json!({"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":"turn-A","status":"completed"}}});
                            if ws
                                .send(tokio_tungstenite::tungstenite::Message::text(
                                    notice.to_string(),
                                ))
                                .is_err()
                            {
                                break;
                            }
                            if method == "turn/steer" {
                                json!({"turnId":"turn-A"})
                            } else {
                                json!({"turn":{"id":"turn-A"}})
                            }
                        }
                        _ => {
                            let error = json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":format!("unexpected fixture method {method}")}});
                            if ws
                                .send(tokio_tungstenite::tungstenite::Message::text(
                                    error.to_string(),
                                ))
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };
                    let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
                    if ws
                        .send(tokio_tungstenite::tungstenite::Message::text(
                            response.to_string(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            path,
            requests,
            busy,
            scenario,
            stop,
            worker: Some(worker),
        }
    }

    pub(super) fn assert_passive_catalog(&self) {
        for request in self.requests.try_iter() {
            assert!(
                !matches!(
                    request["method"].as_str(),
                    Some("thread/resume" | "turn/start" | "turn/steer" | "config/batchWrite")
                ),
                "unloaded app-server catalog was mutated: {request}"
            );
        }
    }

    pub(super) fn request(&self, method: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let request = self
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("Codex method before deadline");
            assert_ne!(
                request["method"], "thread/resume",
                "discovery/send must not load another owner's thread"
            );
            if request["method"] == method {
                return request;
            }
        }
    }
}

impl Drop for CodexPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
fn codex_existing_uds_host_initializes_lists_and_tracks_early_turn_completion() {
    let mut fixture = Fixture::new();
    let codex = CodexPeer::new(&fixture, false);
    let path = codex.path.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("CODEX_APP_SERVER_SOCK", &path)]);
    await_session(&fixture, "codex/thread-A");
    let initialize = codex.request("initialize");
    assert!(initialize["params"]["clientInfo"].is_object());
    codex.request("initialized");
    let mut sender = fixture.plugin("sender", "busy");
    let sent = sender.ok(json!({"op":"send","to":"codex/thread-A","body":"Codex message"}));
    assert_eq!(sent["status"], "delivered");
    let started = codex.request("turn/start");
    assert_eq!(started["params"]["threadId"], "thread-A");
    assert!(started["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Codex message"));
    let read = sender.event("receipt");
    assert_eq!(read["status"], "read");
    assert_eq!(read["message_id"], sent["message_id"]);
}

#[test]
fn codex_busy_thread_steers_exact_active_turn_without_resume_or_second_start() {
    let mut fixture = Fixture::new();
    let codex = CodexPeer::new(&fixture, true);
    let path = codex.path.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("CODEX_APP_SERVER_SOCK", &path)]);
    await_session(&fixture, "codex/thread-A");
    let mut sender = fixture.plugin("sender", "busy");
    let sent = sender.ok(json!({"op":"send","to":"codex/thread-A","body":"steer this turn"}));
    assert_eq!(sent["status"], "delivered");
    let steer = codex.request("turn/steer");
    assert_eq!(steer["params"]["expectedTurnId"], "turn-A");
    assert_eq!(steer["params"]["threadId"], "thread-A");
    assert_eq!(sender.event("receipt")["message_id"], sent["message_id"]);
    codex.busy.store(false, Ordering::Relaxed);
    let pending: Vec<Value> = codex.requests.try_iter().collect();
    assert!(!pending
        .iter()
        .any(|r| r["method"] == "turn/start" || r["method"] == "thread/resume"));
}
