use super::*;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::unix::fs::FileTypeExt;

#[cfg(target_os = "linux")]
fn kernel_process_fields(pid: u32) -> Option<(String, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .collect::<Vec<_>>();
    Some((fields.first()?.to_string(), fields.get(19)?.to_string()))
}

#[cfg(target_os = "linux")]
struct ResumeManager(u32);
#[cfg(target_os = "linux")]
impl Drop for ResumeManager {
    fn drop(&mut self) {
        common::kill_pid(self.0, libc::SIGCONT);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn killed_alias_publication_is_reaped_within_one_second_and_recovers_latest_desired_metadata() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut plugin = fixture.plugin("fast-reap", "busy");
    alias_record(&fixture, "pi/fast-reap");
    let _native = ClaudePeer::new(&fixture);
    fixture.wait_session("claude/native-fixture");
    let marker = format!("reap-marker-{}", uuid::Uuid::new_v4());
    let cwd = fixture.root.join("reap-marker-cwd");
    fs::create_dir(&cwd).unwrap();
    plugin.ok(json!({"op":"register","session":{"id":"fast-reap","name":marker,"cwd":cwd,"pid":std::process::id(),"status":"waiting","caps":["read_receipts","idle_state_v1"]}}));
    let deadline = Instant::now() + TIMEOUT;
    let old = loop {
        let row = alias_record(&fixture, "pi/fast-reap");
        if row["name"] == marker && row["status"] == "waiting" && row["cwd"] == json!(cwd) {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "registry never reflected the fresh reconciliation marker"
        );
        thread::sleep(Duration::from_millis(5));
    };
    let pid = old["pid"].as_u64().unwrap() as u32;
    let (_, start) = kernel_process_fields(pid).expect("old alias must be a real live child");
    assert_eq!(old["procStart"], start);
    let registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{pid}.json"));
    let socket = PathBuf::from(old["messagingSocketPath"].as_str().unwrap());
    common::kill_pid(pid, libc::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut listener_dead = false;
    loop {
        let state = kernel_process_fields(pid);
        listener_dead |= state.as_ref().is_none_or(|(state, _)| state == "Z");
        listener_dead |= UnixStream::connect(&socket).is_err_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        });
        if listener_dead && !registry.exists() && !socket.exists() {
            break;
        }
        assert!(Instant::now()<deadline,"dead alias publication survived the cmd.Wait cleanup window: listener_dead={listener_dead}, registry={}, socket={}, kernel={state:?}",registry.exists(),socket.exists());
        thread::sleep(Duration::from_millis(5));
    }
    let next = alias_record_in_phase(&fixture, "pi/fast-reap", "event-driven child recovery");
    assert_ne!(next["pid"], old["pid"]);
    assert_ne!(next["intercomGeneration"], old["intercomGeneration"]);
    assert_eq!(next["name"], marker);
    assert_eq!(next["status"], "waiting");
    assert_eq!(next["cwd"], json!(cwd));
    assert!(next["peerFeatures"]
        .as_array()
        .is_some_and(|features| features.iter().any(|feature| feature == "notify_idle")));
    assert_eq!(
        next["procStart"],
        kernel_process_fields(next["pid"].as_u64().unwrap() as u32)
            .unwrap()
            .1
    );
    assert!(UnixStream::connect(next["messagingSocketPath"].as_str().unwrap()).is_ok());
}

#[cfg(target_os = "linux")]
#[test]
fn initially_degraded_base_recovers_owned_previous_kernel_incarnation_without_an_in_memory_predecessor(
) {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    let native_registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{}.json", native.process.0.id()));
    let native_bytes = fs::read(&native_registry).unwrap();
    native.remove_registry(&fixture);
    let sessions = fixture.root.join(".claude/sessions");
    fs::remove_dir(&sessions).unwrap();
    fs::write(&sessions, b"initial base setup obstruction").unwrap();
    let dead_socket = fixture.root.join("previous-incarnation.sock");
    let mut old=SourceProcess(Command::new("python3").args(["-u","-c","import os,socket,sys,time; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); os.chmod(sys.argv[1],0o600); s.listen(1); print('ready',flush=True); time.sleep(60)"]).arg(&dead_socket).stdout(Stdio::piped()).spawn().unwrap());
    let mut ready = String::new();
    BufReader::new(old.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    let (_, old_start) = kernel_process_fields(old.0.id()).unwrap();
    old.0.kill().unwrap();
    old.0.wait().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut probe = SourceProcess(
            Command::new("sleep")
                .arg("1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let (_, observed_start) = kernel_process_fields(probe.0.id()).unwrap();
        probe.0.kill().unwrap();
        probe.0.wait().unwrap();
        if observed_start != old_start {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "kernel start-time tick never advanced beyond the old process incarnation"
        );
    }
    assert!(fs::symlink_metadata(&dead_socket)
        .unwrap()
        .file_type()
        .is_socket());
    fixture.start();
    let status = fixture.run(&["status", "--json"]);
    assert_cli_ok(&status);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let pid = status["pid"].as_u64().unwrap() as u32;
    let (_, current_start) = kernel_process_fields(pid).unwrap();
    assert_ne!(
        old_start, current_start,
        "fixture requires independently observed kernel incarnations"
    );
    common::kill_pid(pid, libc::SIGSTOP);
    let resume = ResumeManager(pid);
    let deadline = Instant::now() + TIMEOUT;
    while !kernel_process_fields(pid).is_some_and(|(state, _)| state == "T" || state == "t") {
        assert!(
            Instant::now() < deadline,
            "daemon never stopped for deterministic stale-row setup"
        );
        thread::sleep(Duration::from_millis(2));
    }
    fs::remove_file(&sessions).unwrap();
    fs::create_dir(&sessions).unwrap();
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&native_registry, native_bytes).unwrap();
    fs::set_permissions(&native_registry, fs::Permissions::from_mode(0o600)).unwrap();
    let socket = native.path.parent().unwrap().join(format!("{pid}.sock"));
    fs::rename(&dead_socket, &socket).unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    let registry = sessions.join(format!("{pid}.json"));
    let row = json!({"pid":pid,"sessionId":format!("agent-intercom-{pid}"),"cwd":fixture.root,"startedAt":1,"procStart":old_start,"version":"agent-intercom","peerProtocol":1,"kind":"interactive","entrypoint":"agent-intercom","messagingSocketPath":socket,"name":"previous incarnation","nameSource":"derived","status":"idle","intercomGeneration":generation});
    fs::write(&registry, serde_json::to_vec(&row).unwrap()).unwrap();
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).unwrap();
    drop(resume);
    let deadline = Instant::now() + TIMEOUT;
    let recovered = loop {
        let row: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        if row["procStart"] == current_start && row["intercomGeneration"] != generation {
            break row;
        }
        assert!(Instant::now()<deadline,"base with no in-memory predecessor never reclaimed a proven prior kernel incarnation: {row}");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(recovered["pid"], pid);
    let socket = recovered["messagingSocketPath"].as_str().unwrap();
    assert!(UnixStream::connect(socket).is_ok());
    let id = "recovered-base-opaque";
    native.send_control(
        socket,
        native_user(&native, id, "recovered hub remains a hub"),
    );
    let refused = native.frame_for("recovered base refusal");
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["orig_msg_id"], id);
    assert_eq!(refused["from"], format!("uds:{socket}"));
}

#[test]
fn native_pi_read_before_physical_write_ack_keeps_read_ledger_and_still_acknowledges_delivered() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    let address = super::super::network::vacant_address();
    let listen = address.to_string();
    fixture.start_with(&["--mcp-listen", &listen], &[]);
    let mut target = fixture.plugin("early-read-target", "busy");
    let alias = alias_record(&fixture, "pi/early-read-target");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d41";
    let body = "r".repeat(900 * 1024);
    native.send_control(socket, native_user(&native, id, &body));
    let mut prefix = [0u8; 1];
    target.reader.get_mut().read_exact(&mut prefix).unwrap();
    native.no_frame();
    target.write(
        &json!({"id":"read-before-physical-ack","op":"receipt","message_id":id,"status":"read"}),
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state =
            super::super::network::tool(address, "", "message_status", json!({"message_id":id}));
        if state["status"] == "read" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "read was not recorded while the body writer was still blocked"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let mut line = String::from_utf8(prefix.to_vec()).unwrap();
    target.reader.read_line(&mut line).unwrap();
    let delivered: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(delivered["message_id"], id);
    assert!(
        delivered["body"].as_str() == Some(body.as_str()),
        "native body changed during blocked physical delivery"
    );
    let read_ack = target.read();
    assert_eq!(read_ack["id"], "read-before-physical-ack");
    assert_eq!(read_ack["ok"], true);
    let ack = native.frame_for("Pi delivered after read-before-physical-write-ACK");
    assert_eq!(ack["status"], "delivered");
    assert_eq!(ack["orig_msg_id"], id);
    assert_eq!(ack["from"], format!("uds:{socket}"));
    assert_eq!(
        super::super::network::tool(address, "", "message_status", json!({"message_id":id}))
            ["status"],
        "read"
    );
    native.no_frame();
}

pub(super) fn alias_record(fixture: &Fixture, id: &str) -> Value {
    alias_record_in_phase(fixture, id, "publication")
}

fn alias_record_in_phase(fixture: &Fixture, id: &str, phase: &str) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(entry) = fs::read_dir(fixture.root.join(".claude/sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .find(|entry| entry["sessionId"] == id)
        {
            return entry;
        }
        assert!(
            Instant::now() < deadline,
            "native alias {id} never published during {phase}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn native_idle_request_uses_real_control_and_pinned_endpoint_after_registry_removal() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    fixture.start();
    await_session(&fixture, "claude/native-fixture");
    let mut watcher = fixture.plugin("watcher", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"claude/native-fixture"}));
    let request = native.frame();
    assert_eq!(request["type"], "control");
    assert_eq!(request["action"], "notify_when_idle");
    assert!(request["msg_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(request.get("from_mode").is_none_or(|v| v == ""));
    let destination = request["from"]
        .as_str()
        .unwrap()
        .strip_prefix("uds:")
        .unwrap();
    let registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{}.json", native.process.0.id()));
    fs::remove_file(registry).unwrap();
    native.send_control(destination,json!({"type":"control","action":"peer_idle_notice","from":"uds:/forged.sock","orig_msg_id":request["msg_id"],"state":"idle"}));
    watcher.no_event();
    native.send_control(destination,json!({"type":"control","action":"peer_idle_notice","from":format!("uds:{}",native.path.display()),"orig_msg_id":request["msg_id"],"state":"exited"}));
    let notice = watcher.event("idle_notice");
    assert_eq!(notice["subscription_id"], subscribed["subscription_id"]);
    assert_eq!(notice["status"], "exited");
    assert_eq!(notice["from"], "claude/native-fixture");
}

#[test]
fn native_watcher_observes_pi_alias_with_correlated_peer_idle_notice_not_user_text() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    fixture.start();
    let mut target = fixture.plugin("target", "busy");
    let alias = alias_record(&fixture, "pi/target");
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d26";
    native.send_control(alias["messagingSocketPath"].as_str().unwrap(),json!({"type":"control","action":"notify_when_idle","from":format!("uds:{}",native.path.display()),"msg_id":id}));
    target.register("target", "idle", &["read_receipts", "idle_state_v1"]);
    let notice = native.frame();
    assert_eq!(notice["type"], "control");
    assert_eq!(notice["action"], "peer_idle_notice");
    assert_eq!(notice["orig_msg_id"], id);
    assert_eq!(notice["state"], "idle");
    assert!(notice.get("message").is_none_or(Value::is_null));
}

#[test]
fn native_alias_namespace_co_locates_with_verified_peer_and_revokes_idle_capability() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    fixture.start();
    let mut plugin = fixture.plugin("namespace", "busy");
    let alias = alias_record(&fixture, "pi/namespace");
    let socket = PathBuf::from(alias["messagingSocketPath"].as_str().unwrap());
    assert_eq!(
        socket.parent(),
        native.path.parent(),
        "native reply allowlist requires the selected peer namespace"
    );
    assert!(alias["peerFeatures"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "notify_idle"));
    plugin.register("namespace", "busy", &["read_receipts"]);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let alias = alias_record(&fixture, "pi/namespace");
        if !alias["peerFeatures"]
            .as_array()
            .is_some_and(|features| features.iter().any(|v| v == "notify_idle"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "revoked Pi capability still advertised to native peers"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn crashed_native_alias_is_republished_by_regular_sync_without_user_repair() {
    let mut fixture = Fixture::new();
    fixture.start();
    let _plugin = fixture.plugin("recover", "busy");
    let original = alias_record(&fixture, "pi/recover");
    let pid = original["pid"].as_u64().unwrap() as u32;
    common::kill_pid(pid, libc::SIGKILL);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let replacement = alias_record(&fixture, "pi/recover");
        if replacement["pid"] != original["pid"] {
            let new_pid = replacement["pid"].as_u64().unwrap() as u32;
            assert!(common::pid_alive(new_pid));
            assert!(
                UnixStream::connect(replacement["messagingSocketPath"].as_str().unwrap()).is_ok()
            );
            assert!(
                !fixture
                    .root
                    .join(".claude/sessions")
                    .join(format!("{pid}.json"))
                    .exists(),
                "dead alias registry was leaked"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "regular sync did not recover crashed alias"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn native_aliases_do_not_reintroduce_retired_default_64_process_cap() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    fixture.start();
    let mut plugins: Vec<Peer> = (0..65)
        .map(|i| fixture.plugin(&format!("many-{i}"), "busy"))
        .collect();
    let mut pids = std::collections::BTreeSet::new();
    for i in 0..65 {
        let alias = alias_record(&fixture, &format!("pi/many-{i}"));
        let pid = alias["pid"].as_u64().unwrap() as u32;
        assert!(common::pid_alive(pid), "alias many-{i} is not running");
        assert!(pids.insert(pid), "alias many-{i} shares process {pid}");
    }
    let last = alias_record(&fixture, "pi/many-64");
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d65";
    native.send_control(
        last["messagingSocketPath"].as_str().unwrap(),
        native_user(&native, id, "sixty-fifth alias delivers"),
    );
    let delivered = plugins[64].event("deliver");
    assert_eq!(delivered["message_id"], id);
    assert_eq!(delivered["body"], "sixty-fifth alias delivers");
}

#[test]
fn invalid_upgrade_target_keeps_intercom_connections_and_aliases_live() {
    let mut fixture = Fixture::new();
    let binary = fixture.root.join("pi-famulus");
    fs::copy(BIN, &binary).unwrap();
    fixture.binary = binary.clone();
    fixture.start();
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let alias = alias_record(&fixture, "pi/sender");
    let bad = fixture.root.join("bad-target");
    fs::write(&bad, b"not an executable\n").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(bad, &binary).unwrap();
    let output = common::run_cli(
        &fixture.root.join("manager"),
        &["upgrade"],
        Duration::from_secs(20),
    );
    assert!(
        !output.status.success(),
        "invalid executable was treated as an upgrade"
    );
    assert!(
        output.stderr.contains("upgrade not done"),
        "wrong upgrade outcome: {} {}",
        output.stdout,
        output.stderr
    );
    let sent = sender.ok(json!({"op":"send","to":"pi/target","body":"rollback kept runtime"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    assert_eq!(
        alias_record(&fixture, "pi/sender")["pid"],
        alias["pid"],
        "failed preflight unnecessarily retired working native alias"
    );
    common::replace_binary(&binary, std::path::Path::new(BIN));
}

#[test]
fn native_user_to_local_pi_alias_keeps_uuid_delivered_and_read_is_ledger_only() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    let address = super::super::network::vacant_address();
    let listen = address.to_string();
    fixture.start_with(&["--mcp-listen", &listen], &[]);
    let mut target = fixture.plugin("native-target", "busy");
    let alias = alias_record(&fixture, "pi/native-target");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d34";
    native.send_control(
        socket,
        native_user(&native, id, "native original UUID local delivery"),
    );
    let delivered = target.event("deliver");
    assert_eq!(
        delivered["message_id"], id,
        "native inbound route regenerated a local message ID"
    );
    assert_eq!(delivered["body"], "native original UUID local delivery");
    let receipt = native.frame();
    assert_eq!(receipt["action"], "peer_message_status");
    assert_eq!(receipt["status"], "delivered");
    assert_eq!(receipt["orig_msg_id"], id);
    assert_eq!(receipt["from"], format!("uds:{socket}"));
    target.ok(json!({"op":"receipt","message_id":id,"status":"read"}));
    assert_eq!(
        super::super::network::tool(address, "", "message_status", json!({"message_id":id}))
            ["status"],
        "read"
    );
    native.no_frame();
}

#[test]
fn native_user_message_id_can_be_opaque_non_uuid_and_delivery_preserves_it() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    fixture.start();
    let mut target = fixture.plugin("opaque-target", "busy");
    let alias = alias_record(&fixture, "pi/opaque-target");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    let id = "native-id-opaque";
    native.send_control(socket, native_user(&native, id, "opaque native identity"));
    assert_eq!(target.event("deliver")["message_id"], id);
    let delivered = native.frame();
    assert_eq!(delivered["status"], "delivered");
    assert_eq!(delivered["orig_msg_id"], id);
}

#[test]
fn native_user_to_local_codex_alias_keeps_original_uuid_through_real_host_read_receipt() {
    let mut fixture = Fixture::new();
    let native = ClaudePeer::new(&fixture);
    let host = CodexPeer::new(&fixture, false);
    let address = super::super::network::vacant_address();
    let listen = address.to_string();
    fixture.start_with(
        &["--mcp-listen", &listen],
        &[("CODEX_APP_SERVER_SOCK", host.path.to_str().unwrap())],
    );
    fixture.wait_session("codex/thread-A");
    let alias = alias_record(&fixture, "codex/thread-A");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d30";
    native.send_control(
        socket,
        native_user(&native, id, "native through Codex alias"),
    );
    let request = host.request("turn/start");
    assert_eq!(request["params"]["threadId"], "thread-A");
    assert!(request["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .contains("native through Codex alias"));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let status =
            super::super::network::tool(address, "", "message_status", json!({"message_id":id}));
        if status["status"] == "read" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native Codex completion did not update read ledger"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let receipt =
        native.frame_for("local Codex delivered ACK while canonical ledger is already read");
    assert_eq!(receipt["action"], "peer_message_status");
    assert_eq!(receipt["status"], "delivered");
    assert_eq!(receipt["orig_msg_id"], id);
    assert_eq!(receipt["from"], format!("uds:{socket}"));
    native.no_frame();
}

#[test]
fn failed_namespace_bind_preserves_old_alias_and_revokes_idle_before_retry() {
    let mut fixture = Fixture::new();
    fixture.start();
    let pid = fixture.child.as_ref().unwrap().id();
    let mut plugin = fixture.plugin("migrating", "busy");
    let old = alias_record(&fixture, "pi/migrating");
    let old_socket = PathBuf::from(old["messagingSocketPath"].as_str().unwrap());
    let old_base = old_socket.parent().unwrap().join(format!("{pid}.sock"));
    assert!(
        UnixStream::connect(&old_base).is_ok(),
        "base listener PID must be the manager"
    );
    let sun_path_limit = std::mem::size_of::<libc::sockaddr_un>()
        - std::mem::offset_of!(libc::sockaddr_un, sun_path)
        - 1;
    let directory_name =
        "n".repeat(sun_path_limit - "/1.sock".len() - fixture.root.as_os_str().len() - 1);
    let mut preferred = fixture.root.join(&directory_name);
    let mut native = ClaudePeer::socket_namespace_flags(
        &fixture,
        &directory_name,
        "later-native",
        "1.sock",
        &[],
    );
    assert_eq!(native.path.as_os_str().len(), sun_path_limit);
    assert!(
        preferred.join(format!("{pid}.sock")).as_os_str().len() > sun_path_limit,
        "fixture needs a multi-digit real daemon/child PID"
    );
    fixture.wait_session("claude/later-native");
    struct ResumeAlias(u32);
    impl Drop for ResumeAlias {
        fn drop(&mut self) {
            common::kill_pid(self.0, libc::SIGCONT);
        }
    }
    let old_pid = old["pid"].as_u64().unwrap() as u32;
    common::kill_pid(old_pid, libc::SIGSTOP);
    let resume = ResumeAlias(old_pid);
    plugin.register("migrating", "busy", &["read_receipts"]);
    let deadline = Instant::now() + Duration::from_millis(4500);
    loop {
        let current = alias_record(&fixture, "pi/migrating");
        assert_eq!(current["pid"], old["pid"]);
        assert_eq!(current["intercomGeneration"], old["intercomGeneration"]);
        if !current["peerFeatures"]
            .as_array()
            .is_some_and(|features| features.iter().any(|feature| feature == "notify_idle"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "failed migration kept advertising revoked notify_idle on the working predecessor"
        );
        thread::sleep(Duration::from_millis(20));
    }
    drop(resume);
    native.send_control(
        old_socket.to_str().unwrap(),
        native_user(
            &native,
            "223bfe8d-0039-4cbd-b1f9-a892a2773d33",
            "old endpoint after capability withdrawal",
        ),
    );
    assert_eq!(
        plugin.event("deliver")["body"],
        "old endpoint after capability withdrawal"
    );
    let initial = native.frame();
    assert_eq!(initial["status"], "delivered");
    assert_eq!(
        initial["orig_msg_id"],
        "223bfe8d-0039-4cbd-b1f9-a892a2773d33"
    );
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        fixture.sessions();
        let retained = alias_record(&fixture, "pi/migrating");
        assert_eq!(
            retained["messagingSocketPath"], old["messagingSocketPath"],
            "failed alias child bind retired the still-working predecessor"
        );
        assert!(
            !retained["peerFeatures"]
                .as_array()
                .is_some_and(|features| features.iter().any(|feature| feature == "notify_idle")),
            "resumed predecessor republished a withdrawn idle capability"
        );
        assert!(UnixStream::connect(&old_socket).is_ok());
        thread::sleep(Duration::from_millis(20));
    }
    let shorter = fixture.root.join("later-native");
    fs::rename(&preferred, &shorter).unwrap();
    preferred = shorter;
    native.path = preferred.join("1.sock");
    let registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{}.json", native.process.0.id()));
    let mut row: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
    row["messagingSocketPath"] = json!(native.path);
    fs::write(&registry, serde_json::to_vec(&row).unwrap()).unwrap();
    let blocked = preferred.join(format!("{pid}.sock"));
    let deadline = Instant::now() + TIMEOUT;
    let current = loop {
        fixture.sessions();
        let entry = alias_record(&fixture, "pi/migrating");
        if PathBuf::from(entry["messagingSocketPath"].as_str().unwrap()).parent()
            == Some(preferred.as_path())
        {
            break entry;
        }
        assert!(
            Instant::now() < deadline,
            "native discovery never migrated alias after moving the verified peer to a bindable namespace"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let current_socket = current["messagingSocketPath"].as_str().unwrap();
    assert!(UnixStream::connect(current_socket).is_ok());
    assert!(UnixStream::connect(&blocked).is_ok());
    let deadline = Instant::now() + TIMEOUT;
    while old_socket.exists() || old_base.exists() {
        assert!(
            Instant::now() < deadline,
            "successful migration retained retired namespace endpoints"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let sent = plugin
        .ok(json!({"op":"send","to":"claude/later-native","body":"outbound after migration"}));
    let user = native.frame();
    assert_eq!(user["from"], format!("uds:{current_socket}"));
    native.receipt(&user, "delivered");
    assert_eq!(plugin.event("receipt")["message_id"], sent["message_id"]);
    native.send_control(current_socket,json!({"type":"user","from":format!("uds:{}",native.path.display()),"from_name":"native","from_mode":"prompting","msg_id":"223bfe8d-0039-4cbd-b1f9-a892a2773d29","message":{"role":"user","content":"inbound after migration"}}));
    assert_eq!(plugin.event("deliver")["body"], "inbound after migration");
    fs::remove_file(&blocked).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while UnixStream::connect(&blocked).is_err() {
        fixture.sessions();
        assert!(
            Instant::now() < deadline,
            "base socket unlink was not repaired in its selected namespace"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let report = fixture.run(&["status", "--json"]);
    assert_cli_ok(&report);
    assert_eq!(
        serde_json::from_slice::<Value>(&report.stdout).unwrap()["pid"],
        pid
    );
}

#[test]
fn same_inode_successor_generation_is_not_reclaimed_until_unpublish_and_survives_shutdown() {
    use std::os::unix::fs::MetadataExt;
    struct SuccessorCleanup {
        registry: PathBuf,
        socket: PathBuf,
        identity: (u64, u64),
        bytes: Vec<u8>,
    }
    impl Drop for SuccessorCleanup {
        fn drop(&mut self) {
            if fs::read(&self.registry).ok().as_deref() == Some(self.bytes.as_slice()) {
                let _ = fs::remove_file(&self.registry);
            }
            if fs::symlink_metadata(&self.socket)
                .ok()
                .is_some_and(|m| (m.dev(), m.ino()) == self.identity)
            {
                let _ = fs::remove_file(&self.socket);
            }
        }
    }
    let mut fixture = Fixture::new();
    fixture.start();
    let mut plugin = fixture.plugin("owned", "busy");
    let original = alias_record_in_phase(&fixture, "pi/owned", "initial publication");
    let socket = PathBuf::from(original["messagingSocketPath"].as_str().unwrap());
    let metadata = fs::symlink_metadata(&socket).unwrap();
    let registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{}.json", original["pid"].as_u64().unwrap()));
    let mut successor = original.clone();
    successor["sessionId"] = json!("same-inode-successor");
    successor["name"] = json!("successor-owned");
    successor["intercomGeneration"] = json!("a6f22df7-1be2-497b-b954-34a5e30ca5b0");
    let bytes = serde_json::to_vec(&successor).unwrap();
    fs::write(&registry, &bytes).unwrap();
    let cleanup = SuccessorCleanup {
        registry: registry.clone(),
        socket: socket.clone(),
        identity: (metadata.dev(), metadata.ino()),
        bytes: bytes.clone(),
    };
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        plugin.register("owned", "waiting", &["read_receipts", "idle_state_v1"]);
        fixture.sessions();
        assert_eq!(
            fs::read(&registry).unwrap(),
            bytes,
            "refresh overwrote successor generation"
        );
        assert_eq!(
            fs::read_dir(fixture.root.join(".claude/sessions"))
                .unwrap()
                .flatten()
                .filter_map(|e| fs::read(e.path()).ok())
                .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .filter(|e| e["sessionId"] == "pi/owned")
                .count(),
            0,
            "same active publication reclaimed a tombstoned successor"
        );
        thread::sleep(Duration::from_millis(20));
    }
    plugin.ok(json!({"op":"unregister"}));
    plugin.register("owned", "busy", &["read_receipts", "idle_state_v1"]);
    let renewed = alias_record_in_phase(
        &fixture,
        "pi/owned",
        "renewed publication after explicit unregister/register",
    );
    assert_ne!(renewed["pid"], original["pid"]);
    assert_eq!(fs::read(&registry).unwrap(), bytes);
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
            "successor-aware shutdown did not complete"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(&registry).unwrap(),
        bytes,
        "daemon shutdown deleted successor registration"
    );
    let metadata =
        fs::symlink_metadata(&socket).expect("shutdown deleted same-inode successor socket");
    assert_eq!((metadata.dev(), metadata.ino()), cleanup.identity);
}

#[test]
fn unlinked_alias_socket_without_successor_recovers_with_a_new_live_child_pid() {
    let mut fixture = Fixture::new();
    fixture.start();
    let _plugin = fixture.plugin("unlink", "busy");
    let original = alias_record(&fixture, "pi/unlink");
    let old_pid = original["pid"].as_u64().unwrap() as u32;
    fs::remove_file(original["messagingSocketPath"].as_str().unwrap()).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        fixture.sessions();
        let current = alias_record(&fixture, "pi/unlink");
        if current["pid"] != original["pid"] {
            assert!(common::pid_alive(current["pid"].as_u64().unwrap() as u32));
            assert!(UnixStream::connect(current["messagingSocketPath"].as_str().unwrap()).is_ok());
            assert!(
                !common::pid_alive(old_pid),
                "orphaned alias child survived lost socket recovery"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "socket unlink did not trigger fresh real helper publication"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
