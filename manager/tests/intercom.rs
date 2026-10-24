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
#[path = "intercom_contracts/lifecycle.rs"]
mod lifecycle;
#[path = "intercom_contracts/native.rs"]
mod native;
#[path = "intercom_contracts/pi_port.rs"]
mod pi_port;
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
/// Outlasts the 750ms idle settle plus two 250ms maintenance ticks.
const IDLE_QUIET: Duration = Duration::from_millis(1500);
/// Outlasts the daemon's 3s relay snapshot, Desktop reconnect and webhook retry cadence.
const RESEND_QUIET: Duration = Duration::from_millis(3500);
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
        // The daemon refuses a group/world-writable socket directory rather
        // than chmodding it, so the fixture must not depend on the umask.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
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

    /// The manager binary with this fixture's isolated environment. Every
    /// manager it spawns reads `client.toml` as its bridge configuration.
    fn base_command(&self) -> Command {
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
            .env("AGENT_INTERCOM_CONFIG", self.root.join("client.toml"))
            .arg("--home")
            .arg(self.root.join("manager"));
        command
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        command
            .arg("intercom")
            .arg("--config")
            .arg(self.root.join("client.toml"));
        command
    }

    /// `pi-famulus serve`, which spawns this fixture's manager and keeps it
    /// supervised. Former `intercom serve` flags become the environment
    /// overrides the spawned manager reads with `client.toml`.
    fn spawn_serve(&mut self, args: &[&str], env: &[(&str, &str)]) {
        let log = fs::File::create(self.root.join("serve.log")).unwrap();
        let mut command = self.base_command();
        command.arg("serve").envs(env.iter().copied());
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let key = match *flag {
                "--local-only" => continue,
                "--mcp-listen" => "AGENT_INTERCOM_MCP_LISTEN",
                "--mcp-token" => "AGENT_INTERCOM_MCP_TOKEN",
                "--sock" => "AGENT_INTERCOM_SOCK",
                "--server" => "AGENT_INTERCOM_SERVER",
                "--token" => "AGENT_INTERCOM_TOKEN",
                "--name" => "AGENT_INTERCOM_NAME",
                "--remote-messages" => "AGENT_INTERCOM_REMOTE_MESSAGES",
                other => panic!("serve takes no {other}; put it in client.toml"),
            };
            command.env(key, args.next().expect("flag value"));
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap();
        self.child = Some(child);
    }

    /// Waits until `serve` holds its lease and the bridge socket answers.
    fn wait_supervised(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if self.log().contains("pi-famulus serve: supervising")
                && UnixStream::connect(&self.sock).is_ok()
            {
                return;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("serve exited {status}: {}", self.log());
            }
            assert!(
                Instant::now() < deadline,
                "manager never became supervised with a bridge: {}{}",
                self.log(),
                self.manager_log()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn manager_log(&self) -> String {
        fs::read_to_string(self.root.join("manager/manager.log")).unwrap_or_default()
    }

    /// The manager process (not `serve`, which only holds the lease).
    fn daemon_pid(&self) -> u32 {
        let raw = fs::read(self.root.join("manager/manager.pid")).expect("manager.pid");
        serde_json::from_slice::<Value>(&raw).unwrap()["pid"].as_u64().unwrap() as u32
    }

    /// Ends `serve`, releasing its lease; the manager keeps running for
    /// whatever else holds it.
    fn release_lease(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            child.wait().unwrap();
        }
    }

    fn wait_exit(&self, pid: u32, budget: Duration, why: &str) {
        let deadline = Instant::now() + budget;
        while unsafe { libc::kill(pid as i32, 0) } == 0 {
            assert!(Instant::now() < deadline, "{why}: {}", self.manager_log());
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Ends `serve` (releasing the lease), then shuts the manager down and
    /// waits for it to exit.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let pid = fs::read(self.root.join("manager/manager.pid"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|v| v["pid"].as_u64());
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
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe { libc::kill(pid as i32, 0) } == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if unsafe { libc::kill(pid as i32, 0) } == 0 {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    }

    fn start(&mut self) {
        self.start_with(&[], &[]);
    }

    /// The MCP bearer token the daemon generated because none was configured.
    fn mcp_token(&self) -> String {
        fs::read_to_string(self.root.join("state/mcp-token")).unwrap()
    }

    fn start_with(&mut self, args: &[&str], env: &[(&str, &str)]) {
        self.spawn_serve(args, env);
        self.wait_supervised();
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
    /// Like `wait_session`, but the row must also report `field == value`.
    /// The herdr overlay can publish a provisional row for a native session
    /// id before the native provider enriches it, so id presence alone
    /// races discovery.
    fn wait_session_field(&self, id: &str, field: &str, value: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self
            .sessions()
            .iter()
            .any(|s| s["id"] == id && s[field].as_str() == Some(value))
        {
            assert!(
                Instant::now() < deadline,
                "session {id} never reported {field}={value}: {}",
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
        self.stop();
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
        self.no_event_for(Duration::from_millis(150));
    }

    fn no_event_for(&mut self, window: Duration) {
        assert!(
            self.pushed.is_empty(),
            "unexpected buffered events: {:?}",
            self.pushed
        );
        self.reader
            .get_mut()
            .set_read_timeout(Some(window))
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
fn socket_environment_overrides_toml_and_cli_flag_overrides_environment() {
    let mut fixture = Fixture::new();
    let configured = fixture.sock.clone();
    fixture.sock = fixture.root.join("env.sock");
    let environment = fixture.sock.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("AGENT_INTERCOM_SOCK", &environment)]);
    assert!(
        !configured.exists(),
        "TOML socket unexpectedly bound despite env override"
    );
    let missing = fixture.root.join("missing.sock");
    let output = fixture.run_env(
        &["--sock", &environment, "agents", "--json"],
        &[("AGENT_INTERCOM_SOCK", missing.to_str().unwrap())],
    );
    assert!(
        output.status.success(),
        "--sock did not override the environment: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn malformed_config_fails_cli_commands_and_leaves_serve_without_a_bridge() {
    let mut fixture = Fixture::new();
    fs::write(fixture.root.join("client.toml"), "local_only = [broken\n").unwrap();
    let output = fixture.run(&["agents"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        error.contains("toml") || error.contains("config") || error.contains("parse"),
        "wrong config error: {error}"
    );
    fixture.spawn_serve(&[], &[]);
    let deadline = Instant::now() + TIMEOUT;
    while !fixture.log().contains("intercom unavailable") {
        assert!(
            Instant::now() < deadline,
            "serve did not report the unusable configuration: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fixture.log().contains("parse"), "{}", fixture.log());
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
    watcher.no_event_for(IDLE_QUIET);
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
    watcher.no_event_for(IDLE_QUIET);
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
    watcher.no_event_for(IDLE_QUIET);
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
    replacement.no_event_for(IDLE_QUIET);
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
    let _plugin = fixture.plugin("first-home", "busy");
    let mut other = Fixture::new();
    other.spawn_serve(&["--sock", fixture.sock.to_str().unwrap()], &[]);
    let deadline = Instant::now() + TIMEOUT;
    while !other.log().contains("intercom unavailable") {
        assert!(
            Instant::now() < deadline,
            "second manager did not refuse the live socket: {}",
            other.log()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(other.log().contains("in use"), "{}", other.log());
    assert!(fixture
        .sessions()
        .iter()
        .any(|row| row["id"] == "pi/first-home"));
}

const TASK_WATCHER: &str = r#"
import json, signal, socket, sys
signal.signal(signal.SIGTERM, signal.SIG_IGN)
stream = socket.socket(socket.AF_UNIX)
stream.connect(sys.argv[1])
out = open(sys.argv[2], 'a')
for request in [
    {'id': 'register', 'op': 'register', 'session': {'id': 'task-watcher', 'name': 'task watcher', 'cwd': '/fixture/project', 'status': 'busy', 'caps': ['read_receipts', 'idle_state_v1'], 'updated_at': 1}},
    {'id': 'subscribe', 'op': 'subscribe_idle', 'to': 'pi/target'},
]:
    stream.sendall((json.dumps(request) + '\n').encode())
for line in stream.makefile():
    out.write(line)
    out.flush()
"#;


#[test]
fn shutdown_notifies_a_task_hosted_idle_watcher_before_terminating_it() {
    let mut fixture = Fixture::new();
    fixture.start();
    let target = fixture.plugin("target", "busy");
    let script = fixture.root.join("watcher.py");
    fs::write(&script, TASK_WATCHER).unwrap();
    let frames = fixture.root.join("watcher.jsonl");
    let mut manager = common::Conn::new(
        UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap(),
    );
    manager.hello_ext("watcher-host");
    manager.request_ok(json!({"type":"start","kind":"shell","command":format!("exec python3 {} {} {}", script.display(), fixture.sock.display(), frames.display()),"cwd":fixture.root,"env":{},"run_in_background":true}));
    let deadline = Instant::now() + TIMEOUT;
    let subscription = loop {
        let subscribed = fs::read_to_string(&frames)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["id"] == "subscribe");
        if let Some(frame) = subscribed {
            assert_eq!(frame["status"], "subscribed", "{frame}");
            break frame["subscription_id"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "task watcher never subscribed: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(20));
    };
    let pid = fixture.daemon_pid();
    fixture.release_lease();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    fixture.wait_exit(pid, TIMEOUT, "manager did not shut down");
    let notices: Vec<Value> = fs::read_to_string(&frames)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|frame| frame["type"] == "idle_notice")
        .collect();
    assert!(
        notices
            .iter()
            .any(|n| n["subscription_id"] == subscription && n["status"] == "unavailable"),
        "task-hosted watcher was terminated before its watch settled: {notices:?}"
    );
    drop(target);
}

#[test]
fn intercom_and_famulus_tasks_share_foreground_manager_pid_and_shutdown() {
    let mut fixture = Fixture::new();
    fixture.start();
    let serve_pid = fixture.daemon_pid();
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
        "the bridge must run inside the manager, not a separate process"
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
    fixture.release_lease();
    control.request_ok(json!({"type":"shutdown"}));
    fixture.wait_exit(serve_pid, TIMEOUT, "shared manager failed to shut down");
    assert!(
        UnixStream::connect(&fixture.sock).is_err(),
        "Pi listener survived manager shutdown"
    );
}

#[test]
fn shutdown_with_live_task_and_idle_watch_reports_unavailable_and_exits() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_ext("shutdown-live-session");
    let task = manager.request_ok(json!({"type":"start","kind":"shell","command":"sleep 30","cwd":fixture.root,"env":{},"run_in_background":true}));
    let task_id = task["task_id"].as_str().unwrap().to_string();
    let mut watcher = fixture.plugin("shutdown-watcher", "busy");
    let _target = fixture.plugin("shutdown-target", "busy");
    let subscription = watcher.ok(json!({"op":"subscribe_idle","to":"pi/shutdown-target"}));
    assert_eq!(subscription["delivery_mode"], "pi_push");
    let listed = manager.request_ok(json!({"type":"list"}));
    let pid = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["task_id"] == task_id && row["status"] == "running")
        .and_then(|row| row["pid"].as_u64())
        .unwrap_or_else(|| panic!("live task missing before shutdown: {listed}"))
        as u32;
    assert!(common::pid_alive(pid));
    let mut control =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    control.hello_cli();
    let daemon = fixture.daemon_pid();
    fixture.release_lease();
    control.request_ok(json!({"type":"shutdown"}));
    let notice = watcher.event("idle_notice");
    assert_eq!(notice["subscription_id"], subscription["subscription_id"]);
    assert_eq!(notice["status"], "unavailable");
    fixture.wait_exit(
        daemon,
        TIMEOUT,
        "manager hung shutting down live task with an active idle watch",
    );
    assert!(
        common::poll_true(TIMEOUT, || !common::pid_alive(pid)),
        "live task survived manager shutdown"
    );
    assert!(UnixStream::connect(&fixture.sock).is_err());
}

#[test]
fn mixed_version_watchers_share_one_target_without_pushing_to_legacy_plugin() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut modern = fixture.plugin("modern-watcher", "busy");
    let mut legacy = fixture.connect();
    legacy.register("legacy-watcher", "busy", &["read_receipts"]);
    let mut target = fixture.plugin("mixed-target", "busy");
    let pushed = modern.ok(json!({"op":"subscribe_idle","to":"pi/mixed-target"}));
    let polled = legacy.ok(json!({"op":"subscribe_idle","to":"pi/mixed-target"}));
    assert_eq!(pushed["delivery_mode"], "pi_push");
    assert_eq!(polled["delivery_mode"], "query_only");
    target.register("mixed-target", "idle", &["read_receipts", "idle_state_v1"]);
    let notice = modern.event("idle_notice");
    assert_eq!(notice["subscription_id"], pushed["subscription_id"]);
    assert_eq!(notice["status"], "idle");
    assert_eq!(
        legacy.ok(json!({"op":"idle_status","subscription_id":polled["subscription_id"]}))
            ["status"],
        "idle"
    );
    legacy.no_event_for(IDLE_QUIET);
    let sent = legacy.ok(json!({"op":"send","to":"pi/modern-watcher","body":"from legacy"}));
    assert_eq!(modern.event("deliver")["message_id"], sent["message_id"]);
    let reply = modern.ok(json!({"op":"send","to":"pi/legacy-watcher","body":"to legacy"}));
    assert_eq!(legacy.event("deliver")["message_id"], reply["message_id"]);
}
