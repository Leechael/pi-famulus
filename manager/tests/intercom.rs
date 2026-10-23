//! Intercom contracts exercise the installed Rust binary over the legacy Pi JSONL socket.
#![cfg(unix)]

#[path = "intercom_contracts/cli.rs"]
mod cli_contracts;
#[cfg(feature = "test-clock")]
#[path = "intercom_contracts/clock.rs"]
mod clock_contracts;
#[path = "intercom_contracts/codex_state.rs"]
mod codex_state;
mod common;
#[path = "intercom_contracts/desktop.rs"]
mod desktop;
#[path = "intercom_contracts/dynamic.rs"]
mod dynamic;
#[path = "intercom_contracts/herdr.rs"]
mod herdr;
#[path = "intercom_contracts/mcp_transport.rs"]
mod mcp_transport;
#[path = "intercom_contracts/native.rs"]
mod native;
#[path = "intercom_contracts/network.rs"]
mod network;
#[path = "intercom_contracts/wss.rs"]
mod wss;

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pi-famulus");
const TIMEOUT: Duration = Duration::from_secs(8);
static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    binary: PathBuf,
    root: PathBuf,
    sock: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fic-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("config")).unwrap();
        fs::create_dir_all(root.join("claude/sessions")).unwrap();
        fs::create_dir_all(root.join("codex")).unwrap();
        let sock = root.join("i.sock");
        let fixture = Self {
            binary: PathBuf::from(BIN),
            root,
            sock,
            child: None,
        };
        fixture.config("");
        fixture
    }

    fn config(&self, extra: &str) {
        fs::write(
            self.root.join("client.toml"),
            format!(
                "local_only = true\nname = 'fixture-host'\nsock = '{}'\nstate_dir = '{}'\nmcp_listen = ''\n{extra}",
                self.sock.display(),
                self.root.join("state").display()
            ),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        for key in [
            "AGENT_INTERCOM_SERVER",
            "AGENT_INTERCOM_TOKEN",
            "AGENT_INTERCOM_SOCK",
            "AGENT_INTERCOM_NAME",
            "AGENT_INTERCOM_MCP_LISTEN",
            "AGENT_INTERCOM_MCP_TOKEN",
            "AGENT_INTERCOM_REMOTE_MESSAGES",
            "XDG_RUNTIME_DIR",
            "CODEX_THREAD_ID",
            "CODEX_APP_SERVER_SOCK",
            "CODEX_APP_SERVER_SOCKET",
            "CODEX_DESKTOP_IPC_SOCK",
            "CODEX_IPC_SOCK",
            "HERDR_SESSION",
        ] {
            command.env_remove(key);
        }
        command
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .env("HERDR_SOCKET_PATH", self.root.join("absent-herdr.sock"))
            .arg("--home")
            .arg(self.root.join("manager"))
            .arg("intercom")
            .arg("--config")
            .arg(self.root.join("client.toml"));
        command
    }

    fn start(&mut self) {
        self.start_with(&[], &[]);
    }

    fn start_with(&mut self, args: &[&str], env: &[(&str, &str)]) {
        let log = fs::File::create(self.root.join("serve.log")).unwrap();
        let mut command = self.command();
        command
            .args(["serve", "--local-only"])
            .args(args)
            .envs(env.iter().copied());
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if UnixStream::connect(&self.sock).is_ok() {
                return;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("intercom serve exited {status}: {}", self.log());
            }
            assert!(
                Instant::now() < deadline,
                "intercom socket never ready: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("serve.log")).unwrap_or_default()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.run_at(args, env, &self.root)
    }

    fn run_at(&self, args: &[&str], env: &[(&str, &str)], cwd: &std::path::Path) -> Output {
        let mut child = self
            .command()
            .current_dir(cwd)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "CLI {args:?} timed out: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn connect(&self) -> Peer {
        Peer::new(UnixStream::connect(&self.sock).unwrap())
    }

    fn plugin(&self, id: &str, status: &str) -> Peer {
        let mut peer = self.connect();
        peer.register(id, status, &["read_receipts", "idle_state_v1"]);
        peer
    }

    fn wait_session(&self, id: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.sessions().iter().any(|s| s["id"] == id) {
            assert!(
                Instant::now() < deadline,
                "session {id} never became visible: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn sessions(&self) -> Vec<Value> {
        let response = self.connect().ok(json!({"op": "list"}));
        response["sessions"].as_array().cloned().unwrap_or_default()
    }

    fn wait_absent(&self, id: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while self.sessions().iter().any(|row| row["id"] == id) {
            assert!(Instant::now() < deadline, "session {id} remains published");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut stream) = UnixStream::connect(self.root.join("manager/manager.sock")) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            for request in [
                json!({"type":"hello","client_kind":"cli","id":"cleanup-hello"}),
                json!({"type":"shutdown","id":"cleanup-shutdown"}),
            ] {
                if let Ok(raw) = serde_json::to_vec(&request) {
                    let _ = stream.write_all(&(raw.len() as u32).to_be_bytes());
                    let _ = stream.write_all(&raw);
                }
            }
        }
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Peer {
    reader: BufReader<UnixStream>,
    pushed: VecDeque<Value>,
    sequence: u64,
}

impl Peer {
    fn new(stream: UnixStream) -> Self {
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        Self {
            reader: BufReader::new(stream),
            pushed: VecDeque::new(),
            sequence: 0,
        }
    }

    fn write(&mut self, value: &Value) {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        self.reader.get_mut().write_all(&bytes).unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .expect("read JSONL response before deadline");
        assert!(n > 0, "intercom closed socket before response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSONL: {e}: {line:?}"))
    }

    fn call(&mut self, mut request: Value) -> Value {
        self.sequence += 1;
        let id = format!("request-{}", self.sequence);
        request["id"] = json!(id);
        self.write(&request);
        loop {
            let response = self.read();
            if response["id"] == id {
                return response;
            }
            assert!(
                response["type"].is_string(),
                "uncorrelated reply: {response}"
            );
            self.pushed.push_back(response);
        }
    }

    fn ok(&mut self, request: Value) -> Value {
        let response = self.call(request);
        assert_eq!(response["ok"], true, "RPC failed: {response}");
        response
    }

    fn reject(&mut self, request: Value, fragment: &str) -> Value {
        let response = self.call(request);
        assert_ne!(response["ok"], true, "unexpected RPC success: {response}");
        assert!(
            response["error"]
                .as_str()
                .is_some_and(|s| s.contains(fragment)),
            "wrong error, expected {fragment:?}: {response}"
        );
        response
    }

    fn event(&mut self, kind: &str) -> Value {
        let event = self.pushed.pop_front().unwrap_or_else(|| self.read());
        assert_eq!(event["type"], kind, "unexpected pushed frame: {event}");
        event
    }

    fn no_event(&mut self) {
        assert!(
            self.pushed.is_empty(),
            "unexpected buffered events: {:?}",
            self.pushed
        );
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            result => panic!("unexpected socket activity: {result:?}, {line:?}"),
        }
        self.reader
            .get_mut()
            .set_read_timeout(Some(TIMEOUT))
            .unwrap();
    }

    fn register(&mut self, id: &str, status: &str, caps: &[&str]) -> Value {
        let response = self.ok(json!({
            "op": "register", "session": {
                "id": id, "name": format!("name-{id}"), "cwd": "/fixture/project", "pid": std::process::id(),
                "status": status, "caps": caps, "updated_at": 123456789
            }
        }));
        let canonical = if id.starts_with("pi/") {
            id.to_string()
        } else {
            format!("pi/{id}")
        };
        assert_eq!(response["session_id"], canonical);
        response
    }
}

fn assert_cli_ok(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn existing_state_directory_and_symlink_target_permissions_are_preserved() {
    for symlink in [false, true] {
        let mut fixture = Fixture::new();
        let target = if symlink {
            fixture.root.join("existing-state-target")
        } else {
            fixture.root.join("state")
        };
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).unwrap();
        if symlink {
            std::os::unix::fs::symlink(&target, fixture.root.join("state")).unwrap();
        }
        let output = fixture.run(&["device"]);
        assert_cli_ok(&output);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o750,
            "device chmodded existing state directory (symlink={symlink})"
        );
        assert_eq!(
            fs::metadata(target.join("device.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fixture.start();
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o750,
            "serve chmodded existing state directory (symlink={symlink})"
        );
        if symlink {
            assert!(fs::symlink_metadata(fixture.root.join("state"))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }
}

#[test]
fn local_only_toml_starts_without_token_and_creates_private_stable_identity() {
    let mut fixture = Fixture::new();
    fixture.start();
    assert!(fixture.sessions().is_empty());
    let path = fixture.root.join("state/device.json");
    let first: Value =
        serde_json::from_slice(&fs::read(&path).expect("device identity created")).unwrap();
    assert!(first["device_id"].as_str().is_some_and(|id| id.len() == 36));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&fixture.sock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(fixture.root.join("state"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let mut daemon = fixture.child.take().unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    fixture.start();
    let second: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(first["device_id"], second["device_id"]);
}

#[test]
fn socket_cli_overrides_environment_which_overrides_toml() {
    let mut fixture = Fixture::new();
    let configured = fixture.sock.clone();
    fixture.sock = fixture.root.join("env.sock");
    let environment = fixture.sock.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("AGENT_INTERCOM_SOCK", &environment)]);
    assert!(
        !configured.exists(),
        "TOML socket unexpectedly bound despite env override"
    );
    let mut daemon = fixture.child.take().unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    fixture.sock = fixture.root.join("cli.sock");
    let cli = fixture.sock.to_str().unwrap().to_string();
    fixture.start_with(&["--sock", &cli], &[("AGENT_INTERCOM_SOCK", &environment)]);
    assert!(fixture.connect().ok(json!({"op":"list"}))["sessions"].is_array());
}

#[test]
fn malformed_config_and_invalid_remote_policy_fail_without_socket() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("client.toml"), "local_only = [broken\n").unwrap();
    let output = fixture.run(&["serve", "--local-only"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        error.contains("toml") || error.contains("config"),
        "wrong config error: {error}"
    );
    assert!(!fixture.sock.exists());
    fixture.config("remote_messages = 'silently-allow'\n");
    let output = fixture.run(&["serve", "--local-only"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("remote_messages"),
        "wrong policy error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.sock.exists());
}

#[test]
fn registration_refresh_switch_and_disconnect_keep_directory_live() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut plugin = fixture.plugin("one", "busy");
    let rows = fixture.sessions();
    let row = rows
        .iter()
        .find(|row| row["id"] == "pi/one")
        .expect("registered Pi visible");
    assert_eq!(row["name"], "name-one");
    assert_eq!(row["harness"], "pi");
    assert_eq!(row["cwd"], "/fixture/project");
    assert_eq!(row["status"], "busy");
    assert_eq!(row["updated_at"], 123456789);
    assert_eq!(row["notify_idle"], true);
    plugin.register("pi/one", "waiting", &["read_receipts", "idle_state_v1"]);
    assert_eq!(
        fixture
            .sessions()
            .iter()
            .filter(|s| s["id"] == "pi/one")
            .count(),
        1
    );
    plugin.register("two", "idle", &["read_receipts", "idle_state_v1"]);
    fixture.wait_absent("pi/one");
    assert!(fixture.sessions().iter().any(|s| s["id"] == "pi/two"));
    drop(plugin);
    fixture.wait_absent("pi/two");
}

#[test]
fn replacement_owner_survives_old_unregister_and_rejects_old_send() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut old = fixture.plugin("owner", "busy");
    let mut replacement = fixture.plugin("owner", "idle");
    let mut target = fixture.plugin("target", "busy");
    old.reject(
        json!({"op":"send","to":"pi/target","body":"must not arrive"}),
        "ownership",
    );
    target.no_event();
    old.ok(json!({"op":"unregister"}));
    assert!(fixture.sessions().iter().any(|s| s["id"] == "pi/owner"));
    let sent = replacement.ok(json!({"op":"send","to":"pi/target","body":"current owner"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn local_send_and_read_receipt_preserve_identity_and_body() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent = sender
        .ok(json!({"op":"send","to":"pi/target","body":"hello\n世界","reply_to":"pi/sender"}));
    assert_eq!(sent["status"], "delivered");
    let mid = sent["message_id"].as_str().expect("message id");
    assert!(!mid.is_empty());
    let delivered = target.event("deliver");
    assert_eq!(delivered["message_id"], mid);
    assert_eq!(delivered["session_id"], "pi/target");
    assert_eq!(delivered["from"], "pi/sender");
    assert_eq!(delivered["body"], "hello\n世界");
    assert_eq!(delivered["reply_to"], "pi/sender");
    target.ok(json!({"op":"receipt","message_id":mid,"status":"read"}));
    let receipt = sender.event("receipt");
    assert_eq!(receipt["message_id"], mid);
    assert_eq!(receipt["from"], "pi/target");
    assert_eq!(receipt["status"], "read");
    target.ok(json!({"op":"receipt","message_id":mid,"status":"read"}));
    sender.no_event();
}

#[test]
fn forged_cross_session_receipt_does_not_consume_the_real_target_route() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let mut stranger = fixture.plugin("stranger", "busy");
    let sent = sender.ok(json!({"op":"send","to":"pi/target","body":"private"}));
    let mid = sent["message_id"].clone();
    target.event("deliver");
    stranger.ok(json!({"op":"receipt","message_id":mid,"status":"read"}));
    sender.no_event();
    target.ok(json!({"op":"receipt","message_id":mid,"status":"read"}));
    assert_eq!(sender.event("receipt")["message_id"], mid);
}

#[test]
fn receipt_validation_and_unknown_operations_leave_connection_usable() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut unbound = fixture.connect();
    unbound.reject(
        json!({"op":"receipt","message_id":"missing","status":"read"}),
        "registered",
    );
    unbound.reject(json!({"op":"register"}), "session.id");
    unbound.reject(json!({"op":"not-an-operation"}), "unknown");
    unbound.ok(json!({"op":"list"}));
    unbound.register("target", "busy", &["read_receipts"]);
    unbound.reject(
        json!({"op":"receipt","message_id":"missing","status":"delivered"}),
        "read or dropped",
    );
    unbound.reject(json!({"op":"receipt","status":"read"}), "message_id");
    unbound.ok(json!({"op":"list"}));
}

#[test]
fn local_unknown_and_ambiguous_cli_targets_never_deliver() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut a = fixture.plugin("same-a", "busy");
    let mut b = fixture.plugin("same-b", "busy");
    let unknown = fixture.run(&["send", "pi/missing", "must not deliver"]);
    assert!(!unknown.status.success());
    let ambiguous = fixture.run(&["send", "same", "must not deliver"]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"));
    a.no_event();
    b.no_event();
}

#[test]
fn cli_exact_send_and_agents_use_the_same_local_directory() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut target = fixture.plugin("cli-target", "busy");
    let agents = fixture.run(&["agents", "--json"]);
    assert_cli_ok(&agents);
    let rows: Value = serde_json::from_slice(&agents.stdout).expect("agents --json");
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == "pi/cli-target"));
    let sent = fixture.run(&["send", "pi/cli-target", "from CLI"]);
    assert_cli_ok(&sent);
    assert_eq!(target.event("deliver")["body"], "from CLI");
}

#[test]
fn idle_is_one_shot_bound_to_watcher_and_not_a_message_receipt() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut watcher = fixture.plugin("watcher", "busy");
    let mut target = fixture.plugin("target", "busy");
    let mut stranger = fixture.plugin("stranger", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"pi/target","as":"pi/stranger"}));
    assert_eq!(subscribed["status"], "subscribed");
    assert!(!subscribed["delivery_mode"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    let id = subscribed["subscription_id"].clone();
    assert!(id.as_str().is_some_and(|s| !s.is_empty()));
    target.register("target", "waiting", &["read_receipts", "idle_state_v1"]);
    watcher.no_event();
    target.register("target", "idle", &["read_receipts", "idle_state_v1"]);
    let notice = watcher.event("idle_notice");
    assert_eq!(notice["subscription_id"], id);
    assert_eq!(notice["status"], "idle");
    assert_eq!(notice["session_id"], "pi/watcher");
    assert_eq!(notice["from"], "pi/target");
    assert!(notice.get("body").is_none_or(|v| v == ""));
    let status = watcher.ok(json!({"op":"idle_status","subscription_id":id}));
    assert_eq!(status["status"], "idle");
    stranger.reject(
        json!({"op":"idle_status","subscription_id":id}),
        "another session",
    );
    target.register("target", "busy", &["read_receipts", "idle_state_v1"]);
    target.register("target", "idle", &["read_receipts", "idle_state_v1"]);
    watcher.no_event();
}

#[test]
fn idle_requires_capability_and_explicit_local_watcher() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut watcher = fixture.plugin("watcher", "busy");
    let mut old = fixture.connect();
    old.register("legacy", "idle", &["read_receipts"]);
    watcher.reject(json!({"op":"subscribe_idle","to":"pi/legacy"}), "upgrade");
    fixture
        .connect()
        .reject(json!({"op":"subscribe_idle","to":"pi/watcher"}), "watcher");
    watcher.reject(
        json!({"op":"idle_status","subscription_id":"missing"}),
        "unknown",
    );
}

#[test]
fn idle_explicit_unregister_is_exit_but_transport_loss_is_not() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut watcher = fixture.plugin("watcher", "busy");
    let mut target = fixture.plugin("target", "busy");
    let subscription = watcher.ok(json!({"op":"subscribe_idle","to":"pi/target"}));
    drop(target);
    fixture.wait_absent("pi/target");
    watcher.no_event();
    let status =
        watcher.ok(json!({"op":"idle_status","subscription_id":subscription["subscription_id"]}));
    assert_eq!(status["status"], "subscribed");
    target = fixture.plugin("target", "busy");
    target.ok(json!({"op":"unregister"}));
    assert_eq!(watcher.event("idle_notice")["status"], "exited");
}

#[test]
fn watcher_replacement_cancels_old_subscription_without_notifying_new_owner() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut watcher = fixture.plugin("watcher", "busy");
    let mut target = fixture.plugin("target", "busy");
    let subscription = watcher.ok(json!({"op":"subscribe_idle","to":"pi/target"}));
    let mut replacement = fixture.plugin("watcher", "busy");
    target.register("target", "idle", &["read_receipts", "idle_state_v1"]);
    let status = fixture
        .connect()
        .ok(json!({"op":"idle_status","subscription_id":subscription["subscription_id"]}));
    assert_ne!(status["status"], "subscribed");
    replacement.no_event();
}

#[test]
fn idle_per_watcher_capacity_rejects_fifth_without_evicting_first() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut watcher = fixture.plugin("watcher", "busy");
    let _target = fixture.plugin("target", "busy");
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(
            watcher.ok(json!({"op":"subscribe_idle","to":"pi/target"}))["subscription_id"].clone(),
        );
    }
    watcher.reject(json!({"op":"subscribe_idle","to":"pi/target"}), "capacity");
    for id in ids {
        assert_eq!(
            watcher.ok(json!({"op":"idle_status","subscription_id":id}))["status"],
            "subscribed"
        );
    }
}

#[test]
fn publication_unpublish_restore_requires_fresh_registration() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut target = fixture.plugin("target", "busy");
    // macOS rejects SO_RCVTIMEO with EINVAL once the peer has shut down.
    target
        .reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut admin = fixture.connect();
    admin.ok(json!({"op":"session_unpublish","args":["pi/target"]}));
    fixture.wait_absent("pi/target");
    let mut line = String::new();
    assert_eq!(
        target
            .reader
            .read_line(&mut line)
            .expect("unpublish must close the idle owner connection"),
        0,
        "unpublish emitted a frame instead of EOF: {line}"
    );
    let rows = admin.ok(json!({"op":"session_registrations","all":true}));
    assert!(rows["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == "pi/target" && s["status"] == "unpublished"));
    let mut blocked = fixture.connect();
    blocked.reject(
        json!({"op":"register","session":{"id":"target"}}),
        "unpublished",
    );
    admin.ok(json!({"op":"session_restore","args":["pi/target"]}));
    assert!(!fixture.sessions().iter().any(|s| s["id"] == "pi/target"));
    let rows = admin.ok(json!({"op":"session_registrations","all":true}));
    assert!(rows["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == "pi/target" && s["status"] == "restoring"));
    drop(target);
    let mut target = fixture.plugin("target", "idle");
    assert!(fixture.sessions().iter().any(|s| s["id"] == "pi/target"));
    target.ok(json!({"op":"unregister"}));
}

#[test]
fn publication_restoring_rejects_inbound_until_fresh_registration() {
    let mut fixture = Fixture::new();
    fixture.start();
    let target = fixture.plugin("target", "idle");
    let mut admin = fixture.connect();
    let mut inbound = fixture.connect();
    let request =
        json!({"op":"inbound","session":{"id":"target"},"body":"restoring must not forward"});
    admin.ok(json!({"op":"session_unpublish","args":["pi/target"]}));
    inbound.reject(request.clone(), "unpublished");
    admin.ok(json!({"op":"session_restore","args":["pi/target"]}));
    inbound.reject(request.clone(), "unpublished");
    drop(target);
    let _fresh = fixture.plugin("target", "idle");
    inbound.ok(request);
}

#[test]
fn publication_batch_validation_is_atomic_local_and_unbound_only() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut plugin = fixture.plugin("keep", "busy");
    plugin.reject(
        json!({"op":"session_unpublish","args":["pi/keep"]}),
        "unregistered",
    );
    let mut admin = fixture.connect();
    admin.reject(
        json!({"op":"session_unpublish","machine":"remote","args":["pi/keep"]}),
        "local-only",
    );
    admin.reject(
        json!({"op":"session_unpublish","args":["pi/keep","pi/missing"]}),
        "unknown",
    );
    assert!(fixture.sessions().iter().any(|s| s["id"] == "pi/keep"));
    admin.reject(json!({"op":"session_unpublish","args":["keep"]}), "exact");
    admin.reject(json!({"op":"session_unpublish","args":[]}), "1..");
    assert!(fixture.sessions().iter().any(|s| s["id"] == "pi/keep"));
}

#[test]
fn jsonl_fragmentation_pipelining_and_oversize_do_not_break_other_clients() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut peer = fixture.connect();
    let stream = peer.reader.get_mut();
    stream.write_all(b"{\"id\":\"split\",\"op\":").unwrap();
    stream
        .write_all(b"\"list\"}\n{\"id\":\"second\",\"op\":\"list\"}\n")
        .unwrap();
    assert_eq!(peer.read()["id"], "split");
    assert_eq!(peer.read()["id"], "second");
    let mut oversized = fixture.connect();
    let bytes = vec![b'x'; (1 << 20) + 1];
    let _ = oversized.reader.get_mut().write_all(&bytes);
    let _ = oversized.reader.get_mut().write_all(b"\n");
    assert!(fixture.connect().ok(json!({"op":"list"}))["sessions"].is_array());
}

#[test]
fn live_socket_is_never_stolen_by_a_second_manager_home() {
    let mut fixture = Fixture::new();
    fixture.start();
    let other = Fixture::new();
    let output = other.run(&[
        "serve",
        "--local-only",
        "--sock",
        fixture.sock.to_str().unwrap(),
    ]);
    assert!(
        !output.status.success(),
        "second serve unexpectedly succeeded"
    );
    assert!(fixture.connect().ok(json!({"op":"list"}))["sessions"].is_array());
}

#[test]
fn intercom_serve_enables_existing_manager_without_losing_its_active_task() {
    struct EnableProcess(Child);
    impl Drop for EnableProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut fixture = Fixture::new();
    let log = fs::File::create(fixture.root.join("serve.log")).unwrap();
    fixture.child = Some(
        Command::new(BIN)
            .arg("--home")
            .arg(fixture.root.join("manager"))
            .arg("daemon")
            .env("HOME", &fixture.root)
            .env("XDG_CONFIG_HOME", fixture.root.join("config"))
            .env("CODEX_HOME", fixture.root.join("codex"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let pid = fixture.child.as_ref().unwrap().id();
    let deadline = Instant::now() + TIMEOUT;
    let stream = loop {
        if let Ok(stream) = UnixStream::connect(fixture.root.join("manager/manager.sock")) {
            break stream;
        }
        assert!(
            Instant::now() < deadline,
            "ordinary manager failed to listen: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(10));
    };
    let mut manager = common::Conn::new(stream);
    assert_eq!(manager.hello_ext("already-running-client")["pid"], pid);
    let task=manager.request_ok(json!({"type":"start","kind":"shell","command":"printf task-alive; sleep 30","cwd":fixture.root,"env":{},"run_in_background":true}));
    let task_id = task["task_id"].clone();
    assert!(
        !fixture.sock.exists(),
        "ordinary manager enabled intercom before request"
    );
    let log_path = fixture.root.join("enable.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut enabling = EnableProcess(
        fixture
            .command()
            .args(["serve", "--local-only"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if UnixStream::connect(&fixture.sock).is_ok() {
            break;
        }
        if let Some(status) = enabling.0.try_wait().unwrap() {
            let listed = manager.request_ok(json!({"type":"list"}));
            assert!(
                listed["tasks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["task_id"] == task_id && t["status"] == "running"),
                "intercom activation killed existing work: {listed}"
            );
            panic!(
                "intercom serve returned {status} without enabling the already-running manager: {}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "intercom activation never became ready: {}",
            fs::read_to_string(&log_path).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
    let output = fixture.run(&["status", "--json"]);
    assert_cli_ok(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["pid"], pid, "activation created a second daemon");
    let mut sender = fixture.plugin("attached-sender", "busy");
    let mut target = fixture.plugin("attached-target", "busy");
    let sent =
        sender.ok(json!({"op":"send","to":"pi/attached-target","body":"same existing manager"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    let listed = manager.request_ok(json!({"type":"list"}));
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["task_id"] == task_id && t["status"] == "running"),
        "activation disrupted existing work: {listed}"
    );
    manager.request_ok(json!({"type":"stop","task_id":task_id}));
}

#[test]
fn intercom_and_famulus_tasks_share_foreground_manager_pid_and_shutdown() {
    let mut fixture = Fixture::new();
    fixture.start();
    let serve_pid = fixture.child.as_ref().unwrap().id();
    let mut manager = common::Conn::new(
        UnixStream::connect(fixture.root.join("manager/manager.sock"))
            .expect("existing manager framed socket"),
    );
    let hello = manager.hello_ext("coexist-session");
    assert_eq!(
        hello["ok"], true,
        "existing manager protocol failed: {hello}"
    );
    assert_eq!(
        hello["pid"], serve_pid,
        "intercom serve must itself be the manager, not a wrapper"
    );
    let status = fixture.run(&["status", "--json"]);
    assert_cli_ok(&status);
    let report: Value = serde_json::from_slice(&status.stdout).expect("intercom status JSON");
    assert_eq!(
        report["pid"], serve_pid,
        "intercom and framed manager must report one PID"
    );
    let task = manager.request_ok(json!({"type":"start","kind":"shell","command":"printf coexist-ok; sleep 30","cwd":fixture.root,"env":{},"run_in_background":true}));
    let task_id = task["task_id"].as_str().unwrap();
    let mut sender = fixture.plugin("coexist-sender", "busy");
    let mut target = fixture.plugin("coexist-target", "busy");
    let sent = sender.ok(json!({"op":"send","to":"pi/coexist-target","body":"task still running"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    let listed = manager.request_ok(json!({"type":"list"}));
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["task_id"] == task_id && row["status"] == "running"),
        "intercom disrupted existing task: {listed}"
    );
    manager.request_ok(json!({"type":"stop","task_id":task_id}));
    let mut control =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    control.hello_cli();
    control.request_ok(json!({"type":"shutdown"}));
    let deadline = Instant::now() + TIMEOUT;
    while fixture
        .child
        .as_mut()
        .unwrap()
        .try_wait()
        .unwrap()
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "shared manager failed to shut down"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        UnixStream::connect(&fixture.sock).is_err(),
        "Pi listener survived manager shutdown"
    );
}
