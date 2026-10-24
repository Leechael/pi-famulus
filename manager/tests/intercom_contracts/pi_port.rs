//! Cross-session messaging over the manager connection (protocol 7 `intercom`).
//!
//! The pi-famulus extension serves the agent-intercom client protocol on its
//! existing manager socket instead of the legacy client socket: one port per
//! manager connection, push frames as `intercom` events, identity bound to
//! the hello session, and `intercom_reset` when the bridge restarts.

use super::*;
use crate::common;
use std::path::Path;

/// An extension (parent) connection on the manager socket.
fn pi_conn(fixture: &Fixture, session: &str) -> common::Conn {
    let mut conn = common::Conn::new(
        UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap(),
    );
    let hello = conn.hello_ext(session);
    assert_eq!(hello["ok"], true, "pi hello: {hello}");
    conn
}

fn intercom(conn: &mut common::Conn, op: Value) -> Value {
    conn.request(json!({"type":"intercom","op":op}))
}

fn intercom_ok(conn: &mut common::Conn, op: Value) -> Value {
    let response = intercom(conn, op.clone());
    assert_eq!(response["ok"], true, "intercom {op} failed: {response}");
    response
}

fn register(conn: &mut common::Conn, session: &str, name: &str) -> Value {
    intercom_ok(
        conn,
        json!({"op":"register","session":{"id":session,"name":name,"caps":["read_receipts","idle_state_v1"]}}),
    )
}

/// The next `intercom` event frame matching `pred` on a manager connection.
fn intercom_frame(conn: &mut common::Conn, pred: impl Fn(&Value) -> bool) -> Value {
    let event = conn
        .wait_event(TIMEOUT, |event| {
            event["event"] == "intercom" && pred(&event["frame"])
        })
        .unwrap_or_else(|| panic!("no matching intercom frame; events: {:?}", conn.events));
    event["frame"].clone()
}

#[test]
fn register_send_deliver_receipt_between_two_pi_connections() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let mut bob = pi_conn(&fixture, "bob-session");

    // Register is bound to the hello session id, whatever the op asked for.
    let reg = register(&mut alice, "alice-session", "alice");
    assert_eq!(reg["session_id"], "pi/alice-session");
    register(&mut bob, "bob-session", "bob");

    // The directory sees both sessions over either transport.
    let list = intercom_ok(&mut alice, json!({"op":"list"}));
    let rows = list["sessions"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| row["id"] == "pi/bob-session" && row["harness"] == "pi"),
        "pi/bob-session missing from {list}"
    );

    let sent = intercom_ok(
        &mut alice,
        json!({"op":"send","to":"pi/bob-session","body":"ping","reply_to":"pi/alice-session"}),
    );
    assert_eq!(sent["status"], "delivered");

    let deliver = intercom_frame(&mut bob, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["body"], "ping");
    assert_eq!(deliver["from"], "pi/alice-session");
    assert_eq!(deliver["message_id"], sent["message_id"]);

    // Bob's read receipt reaches alice as an intercom frame.
    intercom_ok(
        &mut bob,
        json!({"op":"receipt","message_id":sent["message_id"],"status":"read"}),
    );
    let receipt = intercom_frame(&mut alice, |frame| frame["type"] == "receipt");
    assert_eq!(receipt["status"], "read");
    assert_eq!(receipt["from"], "pi/bob-session");

    // message_status tracks the delivery end to end.
    let status = intercom_ok(
        &mut alice,
        json!({"op":"message_status","message_id":sent["message_id"]}),
    );
    assert_eq!(status["status"], "read");
}

#[test]
fn intercom_exchanges_with_a_client_sock_peer() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    let mut legacy = fixture.plugin("legacy-peer", "idle");

    let sent = intercom_ok(
        &mut alice,
        json!({"op":"send","to":"pi/legacy-peer","body":"over the bridge"}),
    );
    let deliver = legacy.event("deliver");
    assert_eq!(deliver["body"], "over the bridge");
    assert_eq!(deliver["message_id"], sent["message_id"]);

    // The legacy peer answers over client.sock; alice gets an intercom frame.
    legacy.ok(json!({"op":"receipt","message_id":sent["message_id"],"status":"read"}));
    let receipt = intercom_frame(&mut alice, |frame| frame["type"] == "receipt");
    assert_eq!(receipt["status"], "read");

    // And the legacy peer can message the manager port.
    legacy.ok(json!({"op":"send","to":"pi/alice-session","body":"back","reply_to":"pi/legacy-peer"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["body"], "back");
    assert_eq!(deliver["from"], "pi/legacy-peer");
}

#[test]
fn manager_register_rejects_an_identity_mismatch() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let mismatched = intercom(
        &mut alice,
        json!({"op":"register","session":{"id":"someone-else","name":"alice"}}),
    );
    assert_ne!(mismatched["ok"], true, "mismatch accepted: {mismatched}");
    assert!(
        mismatched["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("session id mismatch")),
        "{mismatched}"
    );
    // Even an explicit pi/ id that is not this connection's session is refused.
    let mismatched = intercom(
        &mut alice,
        json!({"op":"register","session":{"id":"pi/other-session","name":"alice"}}),
    );
    assert_ne!(mismatched["ok"], true, "pi/ mismatch accepted: {mismatched}");
    // The right id registers (both raw and pi/-prefixed spellings).
    intercom_ok(
        &mut alice,
        json!({"op":"register","session":{"id":"alice-session","name":"alice"}}),
    );
    let rows = fixture.sessions();
    assert!(
        rows.iter().any(|row| row["id"] == "pi/alice-session"),
        "pi/alice-session missing: {rows:?}"
    );
}

#[test]
fn intercom_rejects_cli_connections() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    let refused = manager.request(json!({"type":"intercom","op":{"op":"list"}}));
    assert_ne!(refused["ok"], true, "cli intercom accepted: {refused}");
    assert_eq!(refused["error"]["code"], "E_FORBIDDEN");
}

#[test]
fn closing_the_manager_connection_cleans_up_the_port() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");
    // Abrupt socket close (a crashed pi), not a polite unregister.
    drop(alice);
    fixture.wait_absent("pi/alice-session");
}

#[test]
fn a_client_socket_cannot_claim_a_manager_port_session() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");

    let mut legacy = fixture.connect();
    let refused = legacy.call(json!({"op":"register","session":{"id":"alice-session","name":"thief","caps":["read_receipts"]}}));
    assert_ne!(refused["ok"], true, "legacy claimed a port session: {refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("served by the pi-famulus extension")),
        "{refused}"
    );
    fixture.wait_session("pi/alice-session");
}

#[test]
fn a_manager_port_takes_over_a_client_socket_session() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut legacy = fixture.plugin("shared-id", "idle");
    fixture.wait_session("pi/shared-id");

    // The extension re-registers the same id over manager.sock and wins.
    let mut alice = pi_conn(&fixture, "shared-id");
    let reg = register(&mut alice, "shared-id", "from-extension");
    assert_eq!(reg["session_id"], "pi/shared-id");
    fixture.wait_session_field("pi/shared-id", "name", "from-extension");

    // The legacy connection no longer owns the session: its bound ops end.
    legacy.reject(
        json!({"op":"send","to":"pi/shared-id","body":"stale"}),
        "session ownership ended",
    );
}

#[test]
fn the_send_response_arrives_only_after_the_deliver_frame_was_written() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let mut bob = pi_conn(&fixture, "bob-session");
    register(&mut alice, "alice-session", "alice");
    register(&mut bob, "bob-session", "bob");

    let sent = intercom_ok(
        &mut alice,
        json!({"op":"send","to":"pi/bob-session","body":"ordered","reply_to":"pi/alice-session"}),
    );
    // The daemon awaited the deliver frame's socket-write ack before
    // answering, so bob's frame is already in his socket buffer now: it must
    // be readable without giving the daemon any more time to schedule.
    let deliver = intercom_frame(&mut bob, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}

#[test]
fn bridge_restart_pushes_intercom_reset_and_the_port_reregisters() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");

    // The bridge restarts when its configuration changed and a lease is
    // (re)applied: drop the lease, change the config, promote again.
    fixture.release_lease();
    fixture.config("remote_messages = 'refuse'\n");
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    let supervised = manager.request_ok(json!({"type":"supervise"}));
    assert!(
        supervised["intercom"].as_str().is_some_and(|status| status != "unavailable"),
        "bridge did not come back: {supervised}"
    );

    let reset = alice
        .wait_event(TIMEOUT, |event| event["event"] == "intercom_reset")
        .unwrap_or_else(|| panic!("no intercom_reset; events: {:?}", alice.events));
    assert_eq!(reset["event"], "intercom_reset");

    // The port belonged to the old runtime; a fresh register on the same
    // manager connection lands on the new bridge.
    register(&mut alice, "alice-session", "alice-again");
    fixture.wait_session_field("pi/alice-session", "name", "alice-again");
    let mut bob = fixture.plugin("bob-legacy", "idle");
    let sent = bob.ok(json!({"op":"send","to":"pi/alice-session","body":"after restart"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}

#[test]
fn a_disabled_bridge_reports_a_structured_error_and_keeps_the_manager_running() {
    let mut fixture = Fixture::new();
    fixture.spawn_serve(&[], &[("PI_FAMULUS_NO_INTERCOM", "1")]);
    // No bridge socket exists at all; wait for the manager socket instead.
    let manager_sock = fixture.root.join("manager/manager.sock");
    let deadline = Instant::now() + TIMEOUT;
    while UnixStream::connect(&manager_sock).is_err() {
        assert!(Instant::now() < deadline, "manager did not start: {}", fixture.log());
        thread::sleep(Duration::from_millis(25));
    }
    let mut alice = pi_conn(&fixture, "alice-session");
    let refused = intercom(
        &mut alice,
        json!({"op":"register","session":{"id":"alice-session","name":"alice"}}),
    );
    assert_ne!(refused["ok"], true, "disabled bridge served intercom: {refused}");
    assert_eq!(refused["error"]["code"], "E_INTERCOM_DISABLED");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("disabled by")),
        "{refused}"
    );
    // The rest of the manager is unaffected.
    alice.request_ok(json!({"type":"status"}));
}

#[test]
fn reregistration_and_pre_upgrade_receipts_survive_an_in_place_upgrade() {
    let mut fixture = Fixture::new();
    // The daemon execs its own path on upgrade: run a private copy.
    let copy = fixture.root.join("bin/pi-famulus");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    common::replace_binary(&copy, Path::new(BIN));
    fixture.binary = copy;
    fixture.start();

    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");

    // A message delivered before the upgrade keeps its id; its read receipt
    // must be accepted after the upgrade the same way it is after any
    // reconnect to a still-running daemon.
    let mut bob = fixture.plugin("bob-legacy", "idle");
    let sent = bob.ok(json!({"op":"send","to":"pi/alice-session","body":"before upgrade"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);

    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    let upgraded = manager.request_ok(json!({"type":"upgrade"}));
    assert!(upgraded["ok"] == true, "upgrade request: {upgraded}");

    // The bridge socket of the old image is gone until the new image's
    // bridge rebinds it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fixture.root.join("state").exists() || UnixStream::connect(&fixture.sock).is_err() {
        assert!(Instant::now() < deadline, "bridge socket did not return: {}", fixture.manager_log());
        thread::sleep(Duration::from_millis(25));
    }

    // The extension reconnects (new connection, same session), re-registers,
    // and the stale receipt is accepted without an error.
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    let receipt = intercom_ok(
        &mut alice,
        json!({"op":"receipt","message_id":sent["message_id"],"status":"read"}),
    );
    assert_eq!(receipt["ok"], true);

    // The bridge restarted under the new image: a legacy peer reconnects and
    // messaging works end to end.
    let mut bob = fixture.plugin("bob-legacy", "idle");
    let sent = bob.ok(json!({"op":"send","to":"pi/alice-session","body":"after upgrade"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}
