use super::*;
use std::net::TcpListener;

fn plain(fixture: &mut Fixture, manual: bool) -> common::Conn {
    let log = fs::File::create(fixture.root.join("serve.log")).unwrap();
    fixture.child = Some(
        Command::new(BIN)
            .arg("--home")
            .arg(fixture.root.join("manager"))
            .arg("daemon")
            .current_dir(&fixture.root)
            .env("HOME", &fixture.root)
            .env("XDG_CONFIG_HOME", fixture.root.join("config"))
            .env("CODEX_HOME", fixture.root.join("codex"))
            .env(
                "PI_FAMULUS_TEST_CLOCK",
                if manual { "manual" } else { "real" },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + TIMEOUT;
    let stream = loop {
        if let Ok(stream) = UnixStream::connect(fixture.root.join("manager/manager.sock")) {
            break stream;
        }
        assert!(
            Instant::now() < deadline,
            "ordinary manager never became ready: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(10));
    };
    let mut manager = common::Conn::new(stream);
    manager.hello_cli();
    manager
}

fn config(fixture: &Fixture) -> Value {
    json!({"local_only":true,"name":"fixture-host","sock":fixture.sock,"state_dir":fixture.root.join("state"),"mcp_listen":"","mcp_token":"","remote_messages":"refuse"})
}

fn enable(manager: &mut common::Conn, config: &Value) -> Value {
    manager.request(json!({"type":"intercom_enable","config":config}))
}

#[test]
fn relative_cli_socket_and_state_resolve_in_callers_cwd_not_existing_managers_cwd() {
    let mut fixture = Fixture::new();
    let _manager = plain(&mut fixture, false);
    let caller = fixture.root.join("caller-cwd");
    fs::create_dir(&caller).unwrap();
    fs::set_permissions(&caller, fs::Permissions::from_mode(0o700)).unwrap();
    let args = [
        "serve",
        "--local-only",
        "--sock",
        "relative.sock",
        "--state-dir",
        "relative-state",
    ];
    let first = fixture.run_at(&args, &[], &caller);
    assert_cli_ok(&first);
    let expected = caller.join("relative.sock");
    assert!(
        UnixStream::connect(&expected).is_ok(),
        "dynamic enable interpreted relative sock against daemon cwd instead of CLI cwd: {}",
        fixture.log()
    );
    assert!(caller.join("relative-state/device.json").exists());
    assert!(!fixture.root.join("relative.sock").exists());
    assert!(!fixture.root.join("relative-state").exists());
    let before = fs::metadata(&expected).unwrap();
    let repeated = fixture.run_at(&args, &[], &caller);
    assert_cli_ok(&repeated);
    use std::os::unix::fs::MetadataExt;
    let after = fs::metadata(&expected).unwrap();
    assert_eq!(
        (before.dev(), before.ino()),
        (after.dev(), after.ino()),
        "same caller-relative config rebound runtime"
    );
    let mut sender = Peer::new(UnixStream::connect(&expected).unwrap());
    sender.register(
        "relative-sender",
        "busy",
        &["read_receipts", "idle_state_v1"],
    );
    let mut target = Peer::new(UnixStream::connect(&expected).unwrap());
    target.register(
        "relative-target",
        "busy",
        &["read_receipts", "idle_state_v1"],
    );
    let sent = sender.ok(
        json!({"op":"send","to":"pi/relative-target","body":"relative paths stay with caller"}),
    );
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn extension_cannot_enable_intercom_but_cli_can_after_the_rejection() {
    let mut fixture = Fixture::new();
    let mut cli = plain(&mut fixture, false);
    let mut extension =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    extension.hello_ext("unprivileged-extension");
    let rejected = enable(&mut extension, &config(&fixture));
    assert_ne!(rejected["ok"], true);
    assert!(rejected.to_string().contains("cli-only"));
    assert!(!fixture.sock.exists());
    assert_eq!(enable(&mut cli, &config(&fixture))["ok"], true);
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent =
        sender.ok(json!({"op":"send","to":"pi/target","body":"authorized after denied request"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn identical_enable_is_idempotent_without_rebinding_or_losing_registered_peers() {
    use std::os::unix::fs::MetadataExt;
    let mut fixture = Fixture::new();
    let mut manager = plain(&mut fixture, false);
    let cfg = config(&fixture);
    assert_eq!(enable(&mut manager, &cfg)["ok"], true);
    let original = fs::metadata(&fixture.sock).unwrap();
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    assert_eq!(enable(&mut manager, &cfg)["ok"], true);
    let current = fs::metadata(&fixture.sock).unwrap();
    assert_eq!(
        (current.dev(), current.ino()),
        (original.dev(), original.ino())
    );
    let sent = sender.ok(json!({"op":"send","to":"pi/target","body":"idempotent"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn different_enable_configuration_is_rejected_without_altering_the_live_runtime() {
    let mut fixture = Fixture::new();
    let mut manager = plain(&mut fixture, false);
    let cfg = config(&fixture);
    assert_eq!(enable(&mut manager, &cfg)["ok"], true);
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let other_sock = fixture.root.join("other.sock");
    let mut other = cfg;
    other["sock"] = json!(other_sock);
    let rejected = enable(&mut manager, &other);
    assert_ne!(rejected["ok"], true);
    assert!(rejected.to_string().contains("differently"));
    assert!(!other_sock.exists());
    let sent = sender.ok(
        json!({"op":"send","to":"pi/target","body":"original configuration still owns runtime"}),
    );
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn fatal_local_socket_bind_preserves_owned_file_and_allows_same_configuration_retry() {
    let mut fixture = Fixture::new();
    let mut manager = plain(&mut fixture, false);
    fs::write(&fixture.sock, b"owned sentinel").unwrap();
    let cfg = config(&fixture);
    let rejected = enable(&mut manager, &cfg);
    assert_ne!(rejected["ok"], true);
    assert_eq!(
        fs::read(&fixture.sock).unwrap(),
        b"owned sentinel",
        "failed enable replaced an owned non-socket file"
    );
    assert_eq!(manager.request_ok(json!({"type":"status"}))["ok"], true);
    fs::remove_file(&fixture.sock).unwrap();
    assert_eq!(
        enable(&mut manager, &cfg)["ok"],
        true,
        "failed enable poisoned configuration or leaked listeners"
    );
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent = sender.ok(json!({"op":"send","to":"pi/target","body":"successful retry"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn fatal_identity_directory_error_does_not_publish_socket_or_consume_configuration() {
    let mut fixture = Fixture::new();
    let mut manager = plain(&mut fixture, false);
    let blocked = fixture.root.join("blocked-state");
    fs::write(&blocked, b"owned identity conflict").unwrap();
    let mut cfg = config(&fixture);
    cfg["state_dir"] = json!(blocked);
    let rejected = enable(&mut manager, &cfg);
    assert_ne!(rejected["ok"], true);
    assert!(!fixture.sock.exists());
    assert_eq!(fs::read(&blocked).unwrap(), b"owned identity conflict");
    assert_eq!(
        fs::read_dir(fixture.root.join(".claude/sessions"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "failed identity startup published alias residue"
    );
    fs::remove_file(&blocked).unwrap();
    assert_eq!(
        enable(&mut manager, &cfg)["ok"],
        true,
        "failed identity load consumed CONFIG"
    );
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent =
        sender.ok(json!({"op":"send","to":"pi/target","body":"retry after identity failure"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
}

#[test]
fn mcp_bind_failure_keeps_local_jsonl_working_and_reports_error_without_failed_activation() {
    let mut fixture = Fixture::new();
    let mut manager = plain(&mut fixture, false);
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = config(&fixture);
    cfg["mcp_listen"] = json!(occupied.local_addr().unwrap().to_string());
    assert_eq!(
        enable(&mut manager, &cfg)["ok"],
        true,
        "Go preserves local RPC when MCP listener cannot start"
    );
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent = sender
        .ok(json!({"op":"send","to":"pi/target","body":"degraded MCP keeps local transport"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let log = fixture.log().to_lowercase();
        if log.contains("mcp") && (log.contains("error") || log.contains("in use")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "MCP listener failure was not observable: {log}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    manager.request_ok(json!({"type":"shutdown"}));
    let deadline = Instant::now() + Duration::from_secs(4);
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
            "degraded MCP activation could not shut down"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!fixture.sock.exists());
}

#[test]
fn shutdown_is_bounded_with_32_idle_notices_and_backpressured_nonreading_watchers() {
    use std::io::Read;
    let mut fixture = Fixture::new();
    fixture.start();
    let _target = fixture.plugin("busy-target", "busy");
    let mut watchers = Vec::new();
    for index in 0..8 {
        let mut watcher = fixture.plugin(&format!("slow-{index}"), "busy");
        for _ in 0..4 {
            watcher.ok(json!({"op":"subscribe_idle","to":"pi/busy-target"}));
        }
        watchers.push(watcher);
    }
    let body = "x".repeat(900_000);
    for (index, watcher) in watchers.iter_mut().enumerate() {
        watcher.write(&json!({"id":format!("block-{index}"),"op":"send","to":format!("pi/slow-{index}"),"body":body}));
        let mut first = [0u8; 1];
        watcher
            .reader
            .read_exact(&mut first)
            .expect("backpressure barrier: large delivery entered socket");
        assert_eq!(first[0], b'{');
    }
    let mut control =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    control.hello_cli();
    let started = Instant::now();
    control.request_ok(json!({"type":"shutdown"}));
    let deadline = started + Duration::from_secs(4);
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
            "shutdown waited for slow-reader idle notice writes instead of cancelling them"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !fixture.sock.exists(),
        "bounded shutdown leaked the intercom listener"
    );
}

#[cfg(feature = "test-clock")]
#[test]
fn failed_enable_does_not_pin_ordinary_manager_past_its_idle_shutdown() {
    let mut fixture = Fixture::new();
    let mut manager = plain(&mut fixture, true);
    fs::write(&fixture.sock, b"owned sentinel").unwrap();
    let cfg = config(&fixture);
    assert_ne!(enable(&mut manager, &cfg)["ok"], true);
    assert_eq!(fs::read(&fixture.sock).unwrap(), b"owned sentinel");
    fs::remove_file(&fixture.sock).unwrap();
    drop(manager);
    let home = fixture.root.join("manager");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let clock = common::clock_request(&home, json!({"type":"clock_status"}));
        if clock["pending"].as_array().is_some_and(|timers| {
            timers
                .iter()
                .any(|t| t["label"] == "idle" && t["due_in_ms"] == 5000)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "failed enable suppressed ordinary daemon idle: {clock}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    common::clock_request(&home, json!({"type":"clock_advance","ms":5001}));
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
            "failed enable permanently pinned ordinary manager"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!home.join("manager.sock").exists());
}
