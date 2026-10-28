//! Cross-session messaging over the manager connection (protocol 7 `intercom`).
//!
//! The pi-famulus extension serves the pi-famulus client protocol on its
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

/// Ends `serve` and waits until the manager has demoted the bridge. `serve`
/// exiting does not mean the manager has handled its lease connection
/// closing; a `supervise` before that finds the bridge still supervised and
/// never restarts it.
fn release_lease_and_wait_demoted(fixture: &mut Fixture) {
    fixture.release_lease();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let report = fixture.connect().ok(json!({"op":"status"}))["report"].clone();
        if report["supervised"] == false {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "bridge stayed supervised after the lease ended: {report}"
        );
        thread::sleep(Duration::from_millis(10));
    }
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

    // Cap bob's receive buffer: the deliver cannot hide in the socket, so
    // the daemon's write blocks until the test drains it. 1 MiB is big
    // enough to prove ordering yet small enough to drain within the
    // daemon's 5s push write-ack budget even under heavy load (16 round
    // trips through the capped buffer). If the implementation answered
    // alice before the deliver was fully written, her small response
    // would become readable while bob still owed bytes.
    bob.stream.set_recv_buffer_size(64 * 1024).unwrap();
    let body = "x".repeat(1024 * 1024);
    alice.send(&json!({"v":1,"id":"send-1","type":"intercom","op":{"op":"send","to":"pi/bob-session","body":body,"reply_to":"pi/alice-session"}}));

    // A dedicated reader thread drains bob at kernel speed; interleaving
    // the drain with per-iteration polls paces it at the poll interval
    // (the daemon blocks on bob's capped buffer) and the daemon's 5s push
    // write-ack budget expires. The reader also learns the exact frame
    // size from the stream prefix. The main thread watches alice: her
    // response may not become readable before bob holds half the body
    // (kernel buffers hide at most a few hundred KB — the daemon's send
    // buffer is clamped to the peer's small receive buffer — and an
    // implementation that answers before the deliver write trips this at
    // near zero, which the early-ack sabotage proves red).
    let threshold = body.len() / 2;
    let got = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let want = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reader = {
        let mut stream = bob.stream.try_clone().unwrap();
        let got = got.clone();
        let want = want.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut prefix: Vec<u8> = Vec::new();
            let mut total = 0usize;
            loop {
                match stream.read_timeout(&mut buf, Duration::from_millis(500)) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if want.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                            prefix.extend_from_slice(&buf[..n.min(4 - prefix.len())]);
                            if prefix.len() >= 4 {
                                want.store(
                                    u32::from_be_bytes([
                                        prefix[0], prefix[1], prefix[2], prefix[3],
                                    ]) as usize
                                        + 4,
                                    std::sync::atomic::Ordering::SeqCst,
                                );
                            }
                        }
                        total += n;
                        got.store(total, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let w = want.load(std::sync::atomic::Ordering::SeqCst);
        if w != 0 && got.load(std::sync::atomic::Ordering::SeqCst) >= w {
            break;
        }
        let mut one = [0u8; 1];
        match alice.stream.peek_timeout(&mut one, Duration::from_millis(10)) {
            Ok(0) => panic!("alice's connection closed before answering"),
            Ok(_) => {
                // The response is readable before the frame completed:
                // only legitimate once bob holds (nearly) everything.
                let held = got.load(std::sync::atomic::Ordering::SeqCst);
                assert!(
                    held >= threshold,
                    "alice's send response became readable while bob held only {held}/{threshold} deliver bytes"
                );
                break;
            }
            Err(_) => {}
        }
        assert!(Instant::now() < deadline, "deliver frame never completed");
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = reader.join();
    let response = alice.wait_id("send-1", TIMEOUT).expect("send response");
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["status"], "delivered");
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
    release_lease_and_wait_demoted(&mut fixture);
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
fn a_slow_op_on_one_connection_does_not_block_other_sessions_or_close() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let mut bob = pi_conn(&fixture, "bob-session");
    register(&mut alice, "alice-session", "alice");
    register(&mut bob, "bob-session", "bob");

    // Park alice: she sends a large message to bob, whose receive buffer is
    // capped and never drained. The daemon's deliver write blocks on the
    // socket, and alice's send op waits on the frame's written-ack.
    bob.stream.set_recv_buffer_size(8192).unwrap();
    let body = "x".repeat(3 * 1024 * 1024);
    alice.send(&json!({"v":1,"id":"blocked","type":"intercom","op":{"op":"send","to":"pi/bob-session","body":body,"reply_to":"pi/alice-session"}}));
    thread::sleep(Duration::from_millis(300));
    // The op must really be blocked: no answer yet.
    let mut probe = [0u8; 1];
    let readable = alice
        .stream
        .peek_timeout(&mut probe, Duration::from_millis(50))
        .unwrap_or(0);
    assert_eq!(readable, 0, "alice's blocked op answered already");

    // Meanwhile a third party messaging bob's OTHER direction and a legacy
    // peer still work: carol registers and messages a legacy peer without
    // waiting on alice (her op answers in well under the write-ack timeout
    // alice is parked behind).
    let mut carol = pi_conn(&fixture, "carol-session");
    // Well under alice's 5s push write-ack timeout: the broken
    // global-lock version answers only after her op finishes (~5s), so
    // this stays red there; 4s leaves slow-runner headroom for the fixed
    // per-connection path.
    let registration = carol.try_request(
        json!({"type":"intercom","op":{"op":"register","session":{"id":"carol-session","name":"carol","caps":["read_receipts","idle_state_v1"]}}}),
        Duration::from_secs(4),
    );
    let registration = registration.expect("carol's register waited on alice's blocked op");
    assert_eq!(registration["ok"], true, "{registration}");
    let mut legacy = fixture.plugin("legacy-peer", "idle");
    let sent = legacy.ok(json!({"op":"send","to":"pi/carol-session","body":"while alice is stuck"}));
    assert_eq!(sent["status"], "delivered");

    // Alice's connection drops: her close must not wait out her blocked
    // op's 5s push write-ack. 4s keeps red on the global-lock design with
    // slow-runner headroom for the cancellation path.
    drop(alice);
    let deadline = Instant::now() + Duration::from_secs(4);
    while fixture.sessions().iter().any(|row| row["id"] == "pi/alice-session") {
        assert!(
            Instant::now() < deadline,
            "alice's close waited on her blocked op"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn dropping_the_old_connection_keeps_the_rebound_sessions_registration() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut first = pi_conn(&fixture, "alice-session");
    register(&mut first, "alice-session", "alice-v1");
    fixture.wait_session_field("pi/alice-session", "name", "alice-v1");

    // The same pi session rebinds on a newer connection (resume/reconnect).
    let mut second = pi_conn(&fixture, "alice-session");
    register(&mut second, "alice-session", "alice-v2");
    fixture.wait_session_field("pi/alice-session", "name", "alice-v2");

    // The old connection dies; it must not take the rebound session with it.
    drop(first);
    fixture.wait_session_field("pi/alice-session", "name", "alice-v2");

    // The new connection still owns the session end to end.
    let mut legacy = fixture.plugin("bob-legacy", "idle");
    let sent = legacy.ok(json!({"op":"send","to":"pi/alice-session","body":"after rebind"}));
    let deliver = intercom_frame(&mut second, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}

#[test]
fn a_failed_upgrade_leaves_the_ports_alive() {
    let mut fixture = Fixture::new();
    let copy = fixture.root.join("bin/pi-famulus");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    common::replace_binary(&copy, Path::new(BIN));
    fixture.binary = copy.clone();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");

    // Corrupt the upgrade target: the preflight rejects it, no handover.
    // Rename over the file: writing a running executable fails with
    // ETXTBSY on Linux.
    let corrupt = fixture.root.join("bin/pi-famulus.corrupt");
    fs::write(&corrupt, b"#!/bin/sh\necho not a pi-famulus\n").unwrap();
    fs::rename(&corrupt, &copy).unwrap();
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    let accepted = manager.request(json!({"type":"upgrade"}));
    assert_eq!(accepted["ok"], true, "upgrade request accepted: {accepted}");

    // The daemon is unchanged and the port still serves: no reset, no
    // re-registration needed.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let status = manager.request_ok(json!({"type":"status"}));
        if status["last_upgrade"].is_object() {
            assert_eq!(status["last_upgrade"]["ok"], false, "upgrade must be recorded as failed: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "no failed-upgrade record: {status}");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !alice.events.iter().any(|event| event["event"] == "intercom_reset"),
        "a failed upgrade must not reset ports"
    );
    register(&mut alice, "alice-session", "alice-again");
    let mut legacy = fixture.plugin("bob-legacy", "idle");
    let sent = legacy.ok(json!({"op":"send","to":"pi/alice-session","body":"still serving"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}

#[test]
fn a_bridge_reset_finishes_the_in_flight_op_and_answers_queued_ops() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let mut carol = pi_conn(&fixture, "carol-session");
    register(&mut alice, "alice-session", "alice");
    register(&mut carol, "carol-session", "carol");

    // Park alice: her send's deliver blocks on carol's capped, undrained
    // socket, so the op stays in flight across the reset.
    carol.stream.set_recv_buffer_size(8192).unwrap();
    let body = "x".repeat(3 * 1024 * 1024);
    alice.send(&json!({"v":1,"id":"s1","type":"intercom","op":{"op":"send","to":"pi/carol-session","body":body,"reply_to":"pi/alice-session"}}));
    thread::sleep(Duration::from_millis(300));

    // Two ops queue behind the blocked send (registers, reapplied in
    // order once the bridge is back).
    let op = |id: &str, status: &str| {
        let inner = json!({"op":"register","session":{"id":"alice-session","name":"alice","status":status,"caps":["read_receipts","idle_state_v1"]}}).to_string();
        let body = json!({"v":1,"id":id,"type":"intercom","op":serde_json::from_str::<Value>(&inner).unwrap()}).to_string();
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body.as_bytes());
        frame
    };
    alice.stream.write_all(&[op("r2", "busy"), op("r3", "idle")].concat()).unwrap();

    // Restart the bridge (config change on promotion).
    release_lease_and_wait_demoted(&mut fixture);
    fixture.config("remote_messages = 'refuse'\n");
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    manager.request_ok(json!({"type":"supervise"}));

    // The in-flight send finishes (the old runtime's write-ack times out
    // against the undrained carol) and is answered on the same connection.
    let send = alice
        .wait_id("s1", Duration::from_secs(20))
        .expect("in-flight send answered");
    assert_eq!(send["ok"], true, "{send}");
    // Then every queued op gets a deterministic, ordered reset answer.
    let r2 = alice
        .wait_id("r2", Duration::from_secs(15))
        .expect("queued op r2 answered promptly");
    assert_eq!(r2["error"]["code"], "E_INTERCOM_RESET", "{r2}");
    let r3 = alice
        .wait_id("r3", Duration::from_secs(15))
        .expect("queued op r3 answered promptly");
    assert_eq!(r3["error"]["code"], "E_INTERCOM_RESET", "{r3}");
    // Wire order across the reset: r2's answer precedes r3's (wait_id
    // reads the socket in order, so reaching both asserts FIFO).

    // One worker served everything: the same connection re-registers and
    // messaging works end to end.
    register(&mut alice, "alice-session", "alice");
    let mut bob = fixture.plugin("bob-legacy", "idle");
    let sent = bob.ok(json!({"op":"send","to":"pi/alice-session","body":"after reset"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}

#[test]
fn unbound_ports_cannot_spoof_other_sessions() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");

    // Before register, only registration and read-only lookups exist.
    let send = intercom(
        &mut alice,
        json!({"op":"send","to":"pi/victim","body":"x","reply_to":"pi/victim"}),
    );
    assert_ne!(send["ok"], true, "unbound send: {send}");
    assert!(
        send["error"]["message"].as_str().is_some_and(|m| m.contains("not registered")),
        "{send}"
    );
    let inbound = intercom(
        &mut alice,
        json!({"op":"inbound","session":{"id":"pi/victim"},"body":"x"}),
    );
    assert_ne!(inbound["ok"], true, "unbound inbound: {inbound}");
    let subscribe = intercom(
        &mut alice,
        json!({"op":"subscribe_idle","as":"pi/victim","to":"pi/target"}),
    );
    assert_ne!(subscribe["ok"], true, "unbound subscribe with as: {subscribe}");
    // Read-only ops work unbound.
    intercom_ok(&mut alice, json!({"op":"list"}));

    // After register, the rest of the client surface stays shut: a pi
    // session speaks for its own bound id only. Neither the extension nor
    // the old plugin uses session_name/message_status.
    register(&mut alice, "alice-session", "alice");
    for op in [
        json!({"op":"inbound","session":{"id":"pi/victim"},"body":"x"}),
        json!({"op":"session_unpublish","args":["pi/alice-session"]}),
        json!({"op":"workspace_list"}),
        json!({"op":"subscribe_idle","as":"pi/victim","to":"pi/target"}),
        json!({"op":"session_name","to":"pi/victim","name":"hijacked"}),
        json!({"op":"message_status","message_id":"whatever"}),
    ] {
        let refused = intercom(&mut alice, op.clone());
        assert_ne!(refused["ok"], true, "spoofing op accepted: {refused}");
        assert!(
            refused["error"]["message"].as_str().is_some_and(|m| m.contains("not available")),
            "{refused}"
        );
    }
}

#[test]
fn intercom_ops_on_one_connection_apply_in_wire_order() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let op = |id: &str, status: &str| {
        let body = json!({"v":1,"id":id,"type":"intercom","op":{"op":"register","session":{"id":"alice-session","name":"alice","status":status,"caps":["read_receipts","idle_state_v1"]}}}).to_string();
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body.as_bytes());
        frame
    };
    // Pipelined back to back; the worker must apply them in arrival order.
    alice.stream
        .write_all(&[op("r1", "idle"), op("r2", "busy"), op("r3", "idle")].concat())
        .unwrap();
    for id in ["r1", "r2", "r3"] {
        let response = alice.wait_id(id, TIMEOUT).expect("register {id} answered");
        assert_eq!(response["ok"], true, "register {id}: {response}");
    }
    // The last write in wire order wins.
    fixture.wait_session_field("pi/alice-session", "status", "idle");
}

#[test]
fn a_remote_unpublish_reaches_the_extension_as_a_reset() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");

    // The local CLI unpublishes the session over client.sock.
    let mut cli = fixture.connect();
    cli.ok(json!({
        "op":"session_unpublish",
        "args":["pi/alice-session"]
    }));
    let reset = alice
        .wait_event(TIMEOUT, |event| event["event"] == "intercom_reset")
        .unwrap_or_else(|| panic!("no intercom_reset after unpublish; events: {:?}", alice.events));
    assert_eq!(reset["event"], "intercom_reset");
    // The extension learns the truth on its next op: re-registering is
    // refused while unpublished, with the restore hint.
    let refused = intercom(
        &mut alice,
        json!({"op":"register","session":{"id":"alice-session","name":"alice"}}),
    );
    assert_ne!(refused["ok"], true, "re-register while unpublished: {refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("unpublished")),
        "{refused}"
    );
}

/// A manager connection whose hello carries a chosen pi_pid (a stand-in
/// process the test can kill).
fn pi_conn_as(fixture: &Fixture, session: &str, pi_pid: u32) -> common::Conn {
    let mut conn = common::Conn::new(
        UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap(),
    );
    let hello = conn.hello(json!({
        "type": "hello",
        "client_kind": "extension",
        "session_id": session,
        "pi_pid": pi_pid,
        "protocol": 7,
    }));
    assert_eq!(hello["ok"], true, "pi hello: {hello}");
    conn
}

#[test]
fn an_unpublish_during_an_in_flight_op_still_reaches_the_extension() {
    let mut fixture = Fixture::new();
    fixture.start();
    let mut alice = pi_conn(&fixture, "alice-session");
    let mut carol = pi_conn(&fixture, "carol-session");
    register(&mut alice, "alice-session", "alice");
    register(&mut carol, "carol-session", "carol");

    // Alice's send blocks on carol's capped, undrained socket: the op is
    // in flight when the session is unpublished.
    carol.stream.set_recv_buffer_size(8192).unwrap();
    let body = "x".repeat(3 * 1024 * 1024);
    alice.send(&json!({"v":1,"id":"s1","type":"intercom","op":{"op":"send","to":"pi/carol-session","body":body,"reply_to":"pi/alice-session"}}));
    thread::sleep(Duration::from_millis(300));
    let mut cli = fixture.connect();
    cli.ok(json!({"op":"session_unpublish","args":["pi/alice-session"]}));

    // The in-flight op ends with the ownership error...
    let send = alice
        .wait_id("s1", Duration::from_secs(20))
        .expect("the in-flight op is answered");
    assert_ne!(send["ok"], true, "{send}");
    assert!(
        send["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("ownership ended")),
        "{send}"
    );
    // ...and the unpublish still reaches the extension as a reset (the
    // in-op select consumed the watch notification; the outer loop must
    // re-check the value).
    let reset = alice
        .wait_event(TIMEOUT, |event| event["event"] == "intercom_reset")
        .unwrap_or_else(|| panic!("no intercom_reset after the mid-op unpublish; events: {:?}", alice.events));
    assert_eq!(reset["event"], "intercom_reset");
}

#[test]
fn a_dead_pi_process_settles_unread_deliveries_as_dropped() {
    let mut fixture = Fixture::new();
    fixture.start();
    // The pi process is a real child the test kills: the daemon's crash
    // signal (dead pi_pid) is what makes a close a "session gone".
    let mut standin = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let mut alice = pi_conn_as(&fixture, "alice-session", standin.id());
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");

    // A legacy peer sends a message alice never reads; then her connection
    // dies for good along with the pi process. The sender must get a
    // terminal dropped receipt.
    let mut legacy = fixture.plugin("bob-legacy", "idle");
    let sent = legacy.ok(json!({"op":"send","to":"pi/alice-session","body":"unread"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
    // The pi process dies first, then its connection goes.
    standin.kill().unwrap();
    standin.wait().unwrap();
    drop(alice);
    let receipt = legacy.event("receipt");
    assert_eq!(receipt["message_id"], sent["message_id"]);
    assert_eq!(receipt["status"], "dropped");
}

#[test]
fn a_reconnect_gap_does_not_settle_unread_deliveries() {
    let mut fixture = Fixture::new();
    fixture.start();
    // pi_pid is this (alive) test process: a plain disconnect with a gap
    // must NOT settle unread deliveries as dropped.
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");
    let mut legacy = fixture.plugin("bob-legacy", "idle");
    let sent = legacy.ok(json!({"op":"send","to":"pi/alice-session","body":"unread"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
    drop(alice);

    // The gap: no dropped receipt even though the connection is gone.
    legacy.no_event_for(Duration::from_millis(700));

    // The same session reconnects and re-registers; its late read receipt
    // for the pre-gap delivery is accepted, not settled over.
    let mut alice = pi_conn(&fixture, "alice-session");
    register(&mut alice, "alice-session", "alice");
    fixture.wait_session("pi/alice-session");
    intercom_ok(
        &mut alice,
        json!({"op":"receipt","message_id":sent["message_id"],"status":"read"}),
    );
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
    drop(bob);

    // Only the post-exec image can answer with a higher generation, so this
    // is the deterministic new-image signal. The old image's client.sock
    // peers see a refusal window while the old bridge dies and the new one
    // rebinds — pre-existing product behavior on main (the serve loop shuts
    // the bridge down before handover::perform), so legacy clients must
    // retry; the test therefore never treats a bare connect as readiness.
    let generation = manager_status_generation(&fixture)
        .expect("read the current generation before requesting an upgrade");
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    let upgraded = manager.request_ok(json!({"type":"upgrade"}));
    assert!(upgraded["ok"] == true, "upgrade request: {upgraded}");
    drop(manager); // it dies with the exec

    let deadline = Instant::now() + Duration::from_secs(60);
    while !manager_status_generation(&fixture).is_some_and(|g| g > generation) {
        assert!(
            Instant::now() < deadline,
            "the new image never appeared: {}",
            fixture.manager_log()
        );
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

    // The bridge restarts under the new image: a legacy peer gets a full
    // register round-trip (never a bare connect) before it is used.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut bob = loop {
        if let Some(peer) = bridge_peer(&fixture) {
            break peer;
        }
        assert!(
            Instant::now() < deadline,
            "bridge did not come back: {}",
            fixture.manager_log()
        );
        thread::sleep(Duration::from_millis(25));
    };
    let sent = bob.ok(json!({"op":"send","to":"pi/alice-session","body":"after upgrade"}));
    let deliver = intercom_frame(&mut alice, |frame| frame["type"] == "deliver");
    assert_eq!(deliver["message_id"], sent["message_id"]);
}

/// The manager's `status.generation` via a fresh connection, or None while
/// the daemon is mid-exec. Only the post-upgrade image answers with a
/// higher generation than before the request.
fn manager_status_generation(fixture: &Fixture) -> Option<u64> {
    let conn =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).ok()?);
    manager_connection_generation(conn)
}

fn manager_connection_generation(mut conn: common::Conn) -> Option<u64> {
    // hello first: the daemon refuses non-hello first frames, and during
    // the exec hellos are rejected until the new image serves them.
    let hello = conn.try_request(
        json!({"type":"hello","client_kind":"cli"}),
        Duration::from_millis(1500),
    )?;
    if hello["ok"] != true {
        return None;
    }
    conn.try_request(json!({"type":"status"}), Duration::from_millis(1500))?["generation"].as_u64()
}

#[test]
fn optional_request_records_a_failed_write_without_panicking() {
    let (stream, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let mut conn = common::Conn::new(stream);
    assert_eq!(conn.try_request(json!({"type":"status"}), TIMEOUT), None);
    assert!(conn.closed);
    assert!(
        conn.close_reason.contains("write error"),
        "{}",
        conn.close_reason
    );
    assert_eq!(conn.try_request(json!({"type":"status"}), TIMEOUT), None);
}

#[test]
#[should_panic(expected = "no response")]
fn required_request_still_rejects_a_failed_write() {
    let (stream, peer) = UnixStream::pair().unwrap();
    drop(peer);
    common::Conn::new(stream).request(json!({"type":"status"}));
}

#[test]
fn generation_probe_tolerates_a_peer_closed_before_hello() {
    let (stream, peer) = UnixStream::pair().unwrap();
    drop(peer);
    assert_eq!(
        manager_connection_generation(common::Conn::new(stream)),
        None
    );
}

#[test]
fn generation_probe_tolerates_a_peer_closed_before_status() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        use std::io::Read;
        let mut prefix = [0; 4];
        peer.read_exact(&mut prefix).unwrap();
        let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
        peer.read_exact(&mut body).unwrap();
        let hello: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(hello["type"], "hello");
        // Refuse the next write before releasing the hello response, so the
        // client's status write deterministically gets BrokenPipe.
        peer.shutdown(std::net::Shutdown::Read).unwrap();
        common::Conn::new(peer).send(&json!({"id":hello["id"],"ok":true}));
    });
    assert_eq!(
        manager_connection_generation(common::Conn::new(stream)),
        None
    );
    server.join().unwrap();
}

/// A fresh legacy peer that has JUST completed a register round-trip, or
/// None while the bridge socket is unbound/refused. Readiness is the
/// round-trip, never the connect: during an in-place upgrade the old
/// image's listener can still accept (and then die mid-request), and the
/// file can linger unbound between images.
fn bridge_peer(fixture: &Fixture) -> Option<Peer> {
    use std::io::{BufRead, Write as _};
    let mut stream = UnixStream::connect(&fixture.sock).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
    let request =
        json!({"id":"probe","op":"register","session":{"id":"probe-peer","name":"probe"}})
            .to_string()
            + "\n";
    stream.write_all(request.as_bytes()).ok()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
        return None;
    }
    let response: Value = serde_json::from_str(&line).ok()?;
    if response["id"] != "probe" || response["ok"] != true {
        return None;
    }
    Some(Peer::new(reader.into_inner()))
}
