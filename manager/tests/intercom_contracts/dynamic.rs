use super::*;
use std::net::TcpListener;

fn manager(fixture: &Fixture) -> common::Conn {
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    manager
}

#[test]
fn only_a_cli_connection_can_hold_a_supervision_lease() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut extension =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    extension.hello_ext("pi-session");
    let refused = extension.request(json!({"type":"supervise"}));
    assert_ne!(refused["ok"], true, "a pi session took a supervision lease: {refused}");
    let granted = manager(&fixture).request_ok(json!({"type":"supervise"}));
    assert_eq!(granted["intercom"], "supervised", "{granted}");
}

#[test]
fn mcp_bind_failure_keeps_the_bridge_working_and_reports_the_error() {
    let mut fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap().to_string();
    fixture.start_with(&["--mcp-listen", &address], &[]);
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent = sender
        .ok(json!({"op":"send","to":"pi/target","body":"degraded MCP keeps local transport"}));
    assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    let output = fixture.run(&["status", "--json"]);
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        status["mcp_status"]
            .as_str()
            .is_some_and(|status| status.starts_with("error")),
        "MCP listener failure was not observable: {status}"
    );
}

#[test]
fn an_unusable_bridge_configuration_leaves_the_manager_running_without_messaging() {
    let mut fixture = Fixture::new();
    fixture.config("remote_messages = 'sometimes'\n");
    fixture.spawn_serve(&[], &[]);
    let deadline = Instant::now() + TIMEOUT;
    while !fixture.log().contains("intercom unavailable") {
        assert!(
            Instant::now() < deadline,
            "serve did not report the unusable bridge: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fixture.log().contains("remote_messages"), "{}", fixture.log());
    assert_eq!(manager(&fixture).request_ok(json!({"type":"status"}))["ok"], true);
    assert!(UnixStream::connect(&fixture.sock).is_err());
}

#[test]
fn shutdown_is_bounded_with_32_idle_notices_and_backpressured_nonreading_watchers() {
    use std::io::Read;
    let mut fixture = Fixture::new();
    fixture.start();
    let pid = fixture.daemon_pid();
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
    let mut control = manager(&fixture);
    fixture.release_lease();
    control.request_ok(json!({"type":"shutdown"}));
    fixture.wait_exit(
        pid,
        Duration::from_secs(4),
        "shutdown waited for slow-reader idle notice writes instead of cancelling them",
    );
    assert!(
        !fixture.sock.exists(),
        "bounded shutdown leaked the intercom listener"
    );
}
