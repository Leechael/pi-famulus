use super::*;

#[test]
fn hosted_relay_uses_only_a_same_server_saved_credential_over_the_bootstrap_token() {
    for same_server in [true, false] {
        let relay = WireRelay::with_transport(|_| vec![], false, true);
        let mut fixture = stable_fixture();
        let state = fixture.root.join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let identity = state.join("device.json");
        fs::write(&identity, serde_json::to_vec(&json!({
            "device_id":uuid::Uuid::new_v4().to_string(),
            "tenant_id":"fixture-tenant",
            "credential":"saved-device-credential",
            "server":if same_server { relay.url() } else { "http://other-server.invalid".into() },
        })).unwrap()).unwrap();
        fs::set_permissions(identity, fs::Permissions::from_mode(0o600)).unwrap();
        start_relay_options_env(&mut fixture, &relay, "", &[], &[]);
        let request = relay.request("_fixture_upgrade_attempt");
        let raw = request["request"].as_str().unwrap().to_ascii_lowercase();
        let credential = if same_server {
            "saved-device-credential"
        } else {
            "fixture-token"
        };
        assert!(
            raw.contains(&format!("authorization: bearer {credential}")),
            "{raw}"
        );
        let unexpected = if same_server {
            "fixture-token"
        } else {
            "saved-device-credential"
        };
        assert!(!raw.contains(unexpected), "{raw}");
    }
}

#[cfg(feature = "test-clock")]
fn start_manual_transport(fixture: &mut Fixture, relay: &WireRelay) {
    start_relay_options_env(
        fixture,
        relay,
        "",
        &[],
        &[("PI_FAMULUS_TEST_CLOCK", "manual")],
    );
}
#[cfg(feature = "test-clock")]
fn advance_transport(fixture: &Fixture, ms: u64) {
    common::clock_request(
        &fixture.root.join("manager"),
        json!({"type":"clock_advance","ms":ms}),
    );
}

#[cfg(feature = "test-clock")]
#[test]
fn relay_ping_is_a_real_websocket_ping_at_twenty_manual_seconds() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    start_manual_transport(&mut fixture, &relay);
    fixture.wait_session("pi/remote");
    advance_transport(&fixture, 20_000);
    relay.request("_fixture_ws_ping");
}

#[cfg(feature = "test-clock")]
#[test]
fn relay_without_inbound_app_frames_or_server_ping_closes_at_sixty_manual_seconds() {
    let relay = WireRelay::with_transport(
        |request| {
            if request["type"] == "register" {
                relay_basics(request)
            } else {
                vec![]
            }
        },
        true,
        false,
    );
    let mut fixture = stable_fixture();
    start_manual_transport(&mut fixture, &relay);
    relay.request("_fixture_silent");
    advance_transport(&fixture, 59_999);
    assert!(
        relay
            .requests
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "relay disconnected before the real sixty-second inbound deadline"
    );
    advance_transport(&fixture, 1);
    relay.request("_fixture_transport_closed");
}

#[cfg(feature = "test-clock")]
fn assert_auth_reconnect_uses_shared_five_minute_clock(http401: bool) {
    let relay = WireRelay::with_transport(
        move |request| {
            if request["type"] == "register" {
                vec![
                    json!({"type":"error","code":"unauthenticated","message":"fixture credential revoked"}),
                ]
            } else {
                vec![]
            }
        },
        false,
        http401,
    );
    let mut fixture = stable_fixture();
    start_manual_transport(&mut fixture, &relay);
    let kind = if http401 {
        "_fixture_upgrade_attempt"
    } else {
        "register"
    };
    let first = relay.request(kind);
    if http401 {
        assert!(
            first["request"]
                .as_str()
                .unwrap()
                .contains("Upgrade: websocket")
                || first["request"]
                    .as_str()
                    .unwrap()
                    .contains("upgrade: websocket")
        );
    }
    let deadline = Instant::now() + Duration::from_millis(900);
    while let Ok(frame) = relay
        .requests
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        assert_ne!(frame["type"],kind,"unauthenticated relay retried on wall time while the manual clock remained at zero; Go requires five minutes");
        if Instant::now() >= deadline {
            break;
        }
    }
    advance_transport(&fixture, 299_999);
    assert!(
        relay
            .requests
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "authentication retry ran before 300 seconds on the shared clock"
    );
    advance_transport(&fixture, 1);
    relay.request(kind);
}

#[cfg(feature = "test-clock")]
#[test]
fn relay_http_401_reconnect_waits_five_minutes_on_shared_clock() {
    assert_auth_reconnect_uses_shared_five_minute_clock(true);
}
#[cfg(feature = "test-clock")]
#[test]
fn relay_welcome_unauthenticated_reconnect_waits_five_minutes_on_shared_clock() {
    assert_auth_reconnect_uses_shared_five_minute_clock(false);
}

#[test]
fn relay_incoming_native_1025_physical_deliveries_do_not_consume_outgoing_native_route_capacity() {
    use std::os::unix::fs::MetadataExt;
    let relay = WireRelay::new(|request| match request["type"].as_str().unwrap_or("") {
        "list" => vec![
            json!({"type":"list","id":request["id"],"sessions":[{"id":"pi/remote","name":"remote fixture","harness":"pi","machine_id":"other-device","status":"busy","notify_idle":true},{"id":"claude/outgoing-remote","name":"outgoing native fixture","harness":"claude","machine_id":"other-device","status":"busy"}]}),
        ],
        "send" => vec![
            json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
        ],
        _ => relay_basics(request),
    });
    let mut fixture = stable_fixture();
    let target = super::super::native::ClaudePeer::new(&fixture);
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    fixture.wait_session("claude/native-fixture");
    fixture.wait_session("claude/outgoing-remote");
    let source = super::super::native::alias_record(&fixture, "pi/remote");
    let source_socket = source["messagingSocketPath"].as_str().unwrap();
    assert!(common::pid_alive(source["pid"].as_u64().unwrap() as u32));
    let source_metadata = fs::symlink_metadata(source_socket).unwrap();
    let source_identity = (source_metadata.dev(), source_metadata.ino());
    assert!(UnixStream::connect(source_socket).is_ok());
    let target_metadata = fs::symlink_metadata(&target.path).unwrap();
    let target_identity = (target_metadata.dev(), target_metadata.ino());
    let _progress = fixture.plugin("incoming-progress", "busy");
    let progress_id = uuid::Uuid::new_v4().to_string();
    relay.outgoing.send(json!({"type":"idle_subscribe","id":"incoming-progress-subscribe","origin_machine":"other-device","subscription_id":progress_id,"watcher":"pi/remote","session_id":"pi/incoming-progress"})).unwrap();
    assert_eq!(relay.request("idle_subscribe")["status"], "subscribed");
    let mut last = None;
    for index in 0..1025u128 {
        let id = uuid::Uuid::from_u128(0x56200000_0000_4000_8000_000000000000 + index).to_string();
        let body = format!("incoming-native-{index}-without-target-receipt");
        relay.outgoing.send(json!({"type":"deliver","session_id":"claude/native-fixture","from":"pi/remote","reply_to":"pi/remote","message_id":id,"body":body})).unwrap();
        let delivered =
            target.frame_for(&format!("incoming native physical body {}/1025", index + 1));
        assert_eq!(delivered["type"], "user");
        assert_eq!(delivered["msg_id"], id);
        assert_eq!(delivered["from"], format!("uds:{source_socket}"));
        assert!(
            delivered["message"]["content"]
                .as_str()
                .unwrap()
                .contains(&body),
            "incoming native body changed"
        );
        let barrier = format!("incoming-native-progress-{index}");
        relay.outgoing.send(json!({"type":"idle_cancel","id":barrier,"origin_machine":"other-device","subscription_id":progress_id,"watcher":"pi/remote","session_id":"pi/incoming-progress"})).unwrap();
        assert_eq!(relay.request("idle_cancel")["id"],barrier,"same authenticated WebSocket must ACK a valid known-watch cancellation after each physical native write");
        last = Some(delivered);
        if (index + 1) % 128 == 0 {
            eprintln!("incoming native quota phase: {} physical bodies and authenticated wire ACKs completed without native receipts",index+1);
        }
    }
    let source_metadata = fs::symlink_metadata(source_socket).unwrap();
    assert_eq!(
        (source_metadata.dev(), source_metadata.ino()),
        source_identity
    );
    let target_metadata = fs::symlink_metadata(&target.path).unwrap();
    assert_eq!(
        (target_metadata.dev(), target_metadata.ino()),
        target_identity
    );
    let mut outgoing = fixture.plugin("separate-outgoing-pool", "busy");
    let sent=outgoing.ok(json!({"op":"send","to":"claude/outgoing-remote","body":"incoming native traffic must not reserve outgoing routes"}));
    let request = relay.request("send");
    assert_eq!(request["message_id"], sent["message_id"]);
    assert_eq!(request["to"], "claude/outgoing-remote");
    let last = last.unwrap();
    target.receipt(&last, "delivered");
    let callback = relay.request("receipt");
    assert_eq!(callback["message_id"], last["msg_id"]);
    assert_eq!(callback["status"], "delivered");
    assert_eq!(callback["from"], "claude/native-fixture");
    assert_eq!(callback["session_id"], "claude/native-fixture");
    assert_eq!(
        callback["reply_to"], last["msg_id"],
        "unknown native callback must preserve Go's original UUID reply_to"
    );
    assert_eq!(
        tool(
            address,
            &fixture.mcp_token(),
            "message_status",
            json!({"message_id":last["msg_id"]})
        )["status"],
        "delivered",
        "native callback without an admitted outgoing route must still record a status fact"
    );
    // The native namespace follows the first live Claude socket. Keep another
    // peer there so removing the target's registry does not move the
    // namespace and respawn the pi/remote alias before the orphan callback.
    let _anchor = super::super::native::ClaudePeer::socket_namespace_flags(
        &fixture,
        "native",
        "namespace-anchor",
        "anchor.sock",
        &[],
    );
    fixture.wait_session("claude/namespace-anchor");
    target.remove_registry(&fixture);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let rows = fixture.sessions();
        if !rows.iter().any(|row| row["id"] == "claude/native-fixture") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "removed native registry entry remained in the actual directory cache"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(target.is_alive());
    let metadata = fs::symlink_metadata(&target.path).unwrap();
    assert_eq!(
        (metadata.dev(), metadata.ino()),
        target_identity,
        "registry removal must not replace the live physical sender endpoint"
    );
    let prior_id = uuid::Uuid::from_u128(0x56200000_0000_4000_8000_000000000000 + 1023).to_string();
    let mut prior = last.clone();
    prior["msg_id"] = json!(prior_id);
    target.receipt(&prior, "delivered");
    let callback = relay.request("receipt");
    // The send-time endpoint cache authenticates the late callback and keeps
    // the real target identity, so a later read can settle the same delivery.
    let verified = "claude/native-fixture";
    assert_eq!(callback["message_id"], prior_id);
    assert_eq!(callback["status"], "delivered");
    assert_eq!(callback["from"], verified);
    assert_eq!(callback["session_id"], verified);
    assert_eq!(callback["reply_to"], prior_id);
    assert_eq!(tool(address,&fixture.mcp_token(),"message_status",json!({"message_id":prior_id}))["status"],"delivered","authenticated socket callback without a rediscoverable native registry row must still record its orphan status fact");
}

#[test]
fn relay_status_report_does_not_reveal_the_mcp_token_location() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    let listen = vacant_address().to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    relay
        .outgoing
        .send(json!({"type":"status","id":"status-fields","origin_machine":"other-device"}))
        .unwrap();
    let reply = relay.request("status");
    assert_eq!(reply["id"], "status-fields");
    assert_eq!(reply["report"]["mcp_status"], "running", "{reply}");
    assert!(
        reply["report"].get("mcp_token_file").is_none(),
        "a relay peer learned where the MCP token lives: {reply}"
    );
}

#[test]
fn late_rpc_reply_is_never_redispatched_as_a_new_forwarded_status_request() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    start_relay(&mut fixture, &relay, "");
    relay.outgoing.send(json!({"type":"status","id":"reachable-status-request","origin_machine":"other-device"})).unwrap();
    assert_eq!(
        relay.request("status")["id"],
        "reachable-status-request",
        "normal forwarded status must reach the real response path"
    );
    relay.outgoing.send(json!({"type":"status","id":"expired-status-rpc-reply","origin_machine":"other-device","rpc_reply":true,"report":{"machine_id":"other-device"}})).unwrap();
    relay.outgoing.send(json!({"type":"status","id":"after-late-reply-barrier","origin_machine":"other-device"})).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let frame = relay
            .requests
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("normal status barrier after the late reply");
        assert_ne!(
            frame["id"], "expired-status-rpc-reply",
            "late rpc_reply=true was re-executed and replied to as a new operation"
        );
        if frame["id"] == "after-late-reply-barrier" {
            assert_eq!(frame["type"], "status");
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_millis(250);
    while let Ok(frame) = relay
        .requests
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        assert_ne!(
            frame["id"], "expired-status-rpc-reply",
            "late reply was re-executed by an asynchronous forwarded worker"
        );
        if Instant::now() >= deadline {
            break;
        }
    }
}

fn assert_native_base_refused(variant: &str) {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    start_relay(&mut fixture, &relay, "");
    let mut first = fixture.plugin("base-first", "busy");
    let mut second = fixture.plugin("base-second", "busy");
    let status = fixture.run(&["status", "--json"]);
    assert_cli_ok(&status);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let pid = status["pid"].as_u64().unwrap();
    let base_registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{pid}.json"));
    let base: Value = serde_json::from_slice(&fs::read(&base_registry).unwrap()).unwrap();
    let socket = base["messagingSocketPath"].as_str().unwrap();
    let directory = PathBuf::from(socket).parent().unwrap().to_path_buf();
    let native = super::super::native::ClaudePeer::socket_namespace_flags(
        &fixture,
        directory.to_str().unwrap(),
        "native-fixture",
        &format!("n{pid}"),
        &[],
    );
    fixture.wait_session("claude/native-fixture");
    let id = format!("native-base-opaque-{variant}");
    let mut frame = super::super::native::native_user(&native, &id, "hub is not an agent");
    if variant == "envelope" {
        frame["from"] = json!("");
        frame["message"]["content"]=json!(format!("<cross-session-message from=\"uds:{}\" from-name=\"native\" from-mode=\"prompting\">\nhub is not an agent\n</cross-session-message>",native.path.display()));
    }
    if variant == "unregistered" {
        native.remove_registry(&fixture);
    }
    native.send_control(socket, frame);
    let refused = native.frame_for(variant);
    assert_eq!(refused["type"], "control");
    assert_eq!(refused["action"], "peer_message_status");
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["orig_msg_id"], id);
    assert_eq!(refused["from"], format!("uds:{socket}"));
    assert_ne!(refused["msg_id"], id);
    assert!(uuid::Uuid::parse_str(refused["msg_id"].as_str().unwrap()).is_ok());
    assert_eq!(refused["reason"],"pi-famulus: the relay peer is this machine's hub, not an agent session, and cannot receive messages. If you were replying to a message from this machine, that sender gave no reply address and cannot be answered — ask for a resend from a named session (ListAgents).");
    first.no_event();
    second.no_event();
    assert!(
        !relay
            .requests
            .try_iter()
            .any(|request| ["send", "inbound"].contains(&request["type"].as_str().unwrap_or(""))),
        "hub user traffic was forwarded instead of refused"
    );
}

#[test]
fn native_base_user_is_refused_from_actual_base_without_relay_or_pi_delivery() {
    assert_native_base_refused("raw");
}
#[test]
fn native_base_uses_envelope_from_when_raw_from_is_empty() {
    assert_native_base_refused("envelope");
}
#[test]
fn native_base_refuses_unregistered_native_source_without_forwarding() {
    assert_native_base_refused("unregistered");
}

#[test]
fn published_pi_rpc_inbound_uses_new_transport_uuid_and_exact_authenticated_source_fields() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "inbound" {
            vec![
                json!({"type":"inbound","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    let mut source = fixture.plugin("inbound-source", "busy");
    source.ok(json!({"op":"inbound","session":{"id":"inbound-source"},"body":"Pi forwarding user content","reply_to":"pi/inbound-source","message_id":"not-a-native-transport-identity","from_mode":"bypass"}));
    let request = relay.request("inbound");
    assert_eq!(request["session_id"], "pi/inbound-source");
    assert_eq!(request["from"], "pi/inbound-source");
    assert_eq!(request["body"], "Pi forwarding user content");
    assert_eq!(request["reply_to"], "pi/inbound-source");
    assert!(
        request["from_mode"].is_null() || request["from_mode"] == "",
        "Pi RPC must not manufacture a native wire permission mode"
    );
    let id = request["message_id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert_ne!(id, "not-a-native-transport-identity");
    assert_eq!(
        tool(
            address,
            &fixture.mcp_token(),
            "message_status",
            json!({"message_id":id})
        )["status"],
        "accepted"
    );
}

#[test]
fn incoming_delivery_dedup_is_4096_entry_lru_and_hot_duplicates_refresh_recency() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    start_relay(&mut fixture, &relay, "");
    let mut target = fixture.plugin("lru-target", "busy");
    let message_id =
        |i: u128| uuid::Uuid::from_u128(0x45200000_0000_4000_8000_000000000000 + i).to_string();
    let frame = |i: u128| json!({"type":"deliver","session_id":"pi/lru-target","from":"pi/remote","reply_to":"pi/remote","message_id":message_id(i),"body":format!("incoming-{i}")});
    for index in 0..=4096u128 {
        if index > 0 {
            relay.outgoing.send(frame(0)).unwrap();
        }
        relay.outgoing.send(frame(index)).unwrap();
        let delivered = target.event("deliver");
        assert_eq!(
            delivered["message_id"],
            message_id(index),
            "successful duplicate was injected twice"
        );
        target.ok(json!({"op":"receipt","message_id":message_id(index),"status":"read"}));
    }
    eprintln!("dedup phase: 4097 unique injections acknowledged; replaying cold ID 1 after hot-ID touches");
    relay.outgoing.send(frame(1)).unwrap();
    assert_eq!(
        target.event("deliver")["message_id"],
        message_id(1),
        "oldest cold ID was retained beyond the real 4096-entry dedup bound"
    );
    target.ok(json!({"op":"receipt","message_id":message_id(1),"status":"read"}));
    relay.outgoing.send(frame(0)).unwrap();
    relay.outgoing.send(frame(4097)).unwrap();
    assert_eq!(
        target.event("deliver")["message_id"],
        message_id(4097),
        "hot ID was evicted despite being touched by every duplicate"
    );
}

#[cfg(feature = "test-clock")]
#[test]
fn uncertain_incoming_injection_late_read_proves_delivery_and_dedup_survives_retry() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options_env(
        &mut fixture,
        &relay,
        "",
        &["--mcp-listen", &listen],
        &[("PI_FAMULUS_TEST_CLOCK", "manual")],
    );
    fixture.wait_session("pi/remote");
    let mut target = fixture.plugin("slow-incoming", "busy");
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d40";
    let body = "z".repeat(900 * 1024);
    let frame = json!({"type":"deliver","session_id":"pi/slow-incoming","from":"pi/remote","reply_to":"pi/remote","message_id":id,"body":body});
    relay.outgoing.send(frame.clone()).unwrap();
    let mut prefix = [0u8; 1];
    target.reader.get_mut().read_exact(&mut prefix).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = common::clock_request(
            &fixture.root.join("manager"),
            json!({"type":"clock_status"}),
        );
        if state["pending"].as_array().is_some_and(|timers| {
            timers
                .iter()
                .any(|timer| timer["label"] == "intercom-write-ack")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "injection write acknowledgement timer never armed: {state}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // Expire only the caller's acknowledgement wait. The transport's physical
    // write deadline stays on real time so it can finish the complete frame.
    advance_transport(&fixture, 5_000);
    let outcome = relay.request("receipt");
    assert_eq!(outcome["message_id"], id);
    let mut line = String::from_utf8(prefix.to_vec()).unwrap();
    target.reader.read_line(&mut line).unwrap();
    let delivered: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(delivered["message_id"], id);
    assert_eq!(delivered["body"].as_str(), Some(body.as_str()));
    target.ok(json!({"op":"receipt","message_id":id,"status":"read"}));
    relay.outgoing.send(frame).unwrap();
    target
        .reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let mut duplicate = [0u8; 1];
    let duplicate = target.reader.get_mut().read(&mut duplicate);
    target
        .reader
        .get_mut()
        .set_read_timeout(Some(TIMEOUT))
        .unwrap();
    assert!(
        matches!(duplicate,Err(ref error) if matches!(error.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut)),
        "retry injected a second body after authenticated late read: {duplicate:?}"
    );
    assert_eq!(
        outcome["status"], "dropped",
        "Go Pi injection failures nack dropped without creating a terminal ledger fact"
    );
    assert_eq!(
        tool(
            address,
            &fixture.mcp_token(),
            "message_status",
            json!({"message_id":id})
        )["status"],
        "read",
        "late authenticated read did not prove an uncertain injection happened"
    );
}

#[test]
fn enroll_and_device_persist_credentials_without_printing_secrets() {
    let endpoint = HttpFixture::new(|request| {
        assert_eq!(request.path, "/v1/enroll");
        (
            201,
            json!({"tenant_id":"tenant-fixture","device_id":request.body["device_id"],"credential":"device-secret-never-print"}),
        )
    });
    let fixture = stable_fixture();
    let url = endpoint.url();
    let output = fixture.run(&[
        "enroll",
        "--server",
        &url,
        "--token",
        "one-time-enrollment-secret",
    ]);
    assert_cli_ok(&output);
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!printed.contains("device-secret-never-print"));
    assert!(!printed.contains("one-time-enrollment-secret"));
    let request = endpoint.request();
    assert_eq!(request.body["token"], "one-time-enrollment-secret");
    assert_eq!(request.body["name"], "fixture-host");
    assert!(request.body["client_version"].is_string());
    let saved: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("state/device.json")).unwrap()).unwrap();
    assert_eq!(saved["device_id"], request.body["device_id"]);
    assert_eq!(saved["credential"], "device-secret-never-print");
    assert_eq!(saved["server"], url);
    let output = fixture.run(&["device"]);
    assert_cli_ok(&output);
    let device: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(device["device_id"], saved["device_id"]);
    assert_eq!(device["credential"], "enrolled");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("device-secret-never-print"));
}

#[test]
fn endpoint_http_missing_endpoint_newline_falls_back_only_to_a_running_device_bound_daemon() {
    let endpoint = HttpFixture::raw(|request| {
        let body = request.body["body"].as_str().unwrap();
        if let Some(status) = body.strip_prefix("negative-") {
            (status.parse().unwrap(), b"endpoint not found\n".to_vec())
        } else if body == "wrong-404" {
            (404, b"unknown session\n".to_vec())
        } else {
            (404, b"endpoint not found\n".to_vec())
        }
    });
    let mut fixture = stable_fixture();
    fixture.start();
    let mut target = fixture.plugin("endpoint-receiver", "busy");
    let url = endpoint.url();
    let config = fixture.root.join("client.toml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("local_only = true", "local_only = false");
    fs::write(&config, format!("{text}\nserver='{url}'\n")).unwrap();
    let identity_path = fixture.root.join("state/device.json");
    let mut identity: Value = serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
    identity["credential"] = json!("matching-device-binding");
    identity["server"] = json!(url);
    fs::write(&identity_path, serde_json::to_vec(&identity).unwrap()).unwrap();
    for status in [400, 401, 403, 500, 503] {
        let body = format!("negative-{status}");
        let output = fixture.run(&[
            "send",
            "pi/endpoint-receiver",
            "--as",
            "hermes",
            "--token",
            "matching-device-binding",
            &body,
        ]);
        assert!(
            !output.status.success(),
            "non-404 HTTP {status} must never use daemon fallback"
        );
        let request = endpoint.requests.recv_timeout(TIMEOUT).unwrap_or_else(|_| {
            panic!(
                "HTTP {status} fixture was never called: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(request.body["body"], body);
        assert_eq!(request.method, "POST");
        target.no_event();
    }
    for (body, token) in [
        ("wrong-404", "matching-device-binding"),
        ("opaque-key-must-not-fallback", "opaque-endpoint-key"),
    ] {
        let output = fixture.run(&[
            "send",
            "pi/endpoint-receiver",
            "--as",
            "hermes",
            "--token",
            token,
            body,
        ]);
        assert!(
            !output.status.success(),
            "wrong 404 body or opaque token must not use daemon fallback"
        );
        let request = endpoint.request();
        assert_eq!(request.body["body"], body);
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {token}").as_str())
        );
        target.no_event();
    }
    let body = "device-bound missing endpoint newline fallback";
    let output = fixture.run(&[
        "send",
        "pi/endpoint-receiver",
        "--as",
        "hermes",
        "--token",
        "matching-device-binding",
        body,
    ]);
    assert_cli_ok(&output);
    let request = endpoint.request();
    assert_eq!(request.body["body"], body);
    let delivered = target.event("deliver");
    assert_eq!(delivered["body"], body);
    assert_eq!(delivered["reply_to"], "listen/hermes");
    target.no_event();
    assert!(
        endpoint
            .requests
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "daemon fallback must not resubmit an HTTP body or open a one-shot WebSocket"
    );
    let missing = fixture.root.join("no-client-daemon.sock");
    let output = fixture.run(&[
        "send",
        "pi/endpoint-receiver",
        "--as",
        "hermes",
        "--token",
        "matching-device-binding",
        "--sock",
        missing.to_str().unwrap(),
        "no-daemon-must-not-one-shot",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("running client daemon"));
    let request = endpoint.request();
    assert_eq!(request.body["body"], "no-daemon-must-not-one-shot");
    target.no_event();
    assert!(
        endpoint
            .requests
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "missing daemon must not open a one-shot WebSocket or send a second body"
    );
}

#[test]
fn endpoint_http_queued_send_and_inbox_preserve_server_status_and_identity() {
    let endpoint = HttpFixture::new(|request| {
        if request.method == "POST" {
            (
                201,
                json!({"message_id":"223bfe8d-0039-4cbd-b1f9-a892a2773d24","status":"queued"}),
            )
        } else if request.path.starts_with("/v1/inbox/") {
            (
                200,
                json!({"id":"223bfe8d-0039-4cbd-b1f9-a892a2773d24","body":"inbox detail","status":"read"}),
            )
        } else {
            (
                200,
                json!({"messages":[{"id":"223bfe8d-0039-4cbd-b1f9-a892a2773d24","body":"inbox detail","status":"delivered"}],"next_cursor":"next-page"}),
            )
        }
    });
    let fixture = stable_fixture();
    let config = fs::read_to_string(fixture.root.join("client.toml"))
        .unwrap()
        .replace("local_only = true", "local_only = false");
    fs::write(fixture.root.join("client.toml"), config).unwrap();
    let url = endpoint.url();
    let sent = fixture.run(&[
        "send",
        "--server",
        &url,
        "--token",
        "endpoint-secret",
        "--as",
        "hermes",
        "pi/offline",
        "queued body",
    ]);
    assert_cli_ok(&sent);
    let output: Value =
        serde_json::from_slice(&sent.stdout).expect("queued endpoint send prints status+ID JSON");
    assert_eq!(output["message_id"], "223bfe8d-0039-4cbd-b1f9-a892a2773d24");
    assert_eq!(output["status"], "queued");
    let request = endpoint.request();
    assert!(request.path.starts_with("/v1/messages?"));
    assert!(request.path.contains("as=hermes"));
    assert_eq!(
        request.headers.get("authorization").unwrap(),
        "Bearer endpoint-secret"
    );
    assert_eq!(request.body["to"], "pi/offline");
    assert_eq!(request.body["body"], "queued body");
    let listed = fixture.run(&[
        "inbox",
        "--server",
        &url,
        "--token",
        "endpoint-secret",
        "--as",
        "hermes",
        "--limit",
        "7",
        "--cursor",
        "cursor-one",
    ]);
    assert_cli_ok(&listed);
    let request = endpoint.request();
    assert_eq!(request.method, "GET");
    assert!(request.path.contains("limit=7") && request.path.contains("cursor=cursor-one"));
    let detail = fixture.run(&[
        "inbox",
        "--server",
        &url,
        "--token",
        "endpoint-secret",
        "--as",
        "hermes",
        "223bfe8d-0039-4cbd-b1f9-a892a2773d24",
    ]);
    assert_cli_ok(&detail);
    assert!(endpoint
        .request()
        .path
        .starts_with("/v1/inbox/223bfe8d-0039-4cbd-b1f9-a892a2773d24"));
    assert!(
        !fixture.sock.exists(),
        "endpoint HTTP commands must not start a local daemon"
    );
}

#[test]
fn listen_announces_endpoint_forwards_signed_payload_and_returns_delivery_receipt() {
    let endpoint = HttpFixture::new(|_| (200, json!({"status":"duplicate"})));
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    let url = relay.url();
    let forward = endpoint.url();
    let log = fs::File::create(fixture.root.join("serve.log")).unwrap();
    fixture.child = Some(
        fixture
            .command()
            .args([
                "listen",
                "--server",
                &url,
                "--token",
                "listen-token",
                "--endpoint",
                "hermes",
                "--forward-to",
                &forward,
                "--secret",
                "hook-secret",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    relay.request("register");
    let snapshot = relay.request("snapshot");
    assert!(snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == "listen/hermes" && s["harness"] == "webhook"));
    relay.push(json!({"type":"deliver","session_id":"listen/hermes","from":"pi/remote","reply_to":"pi/remote","message_id":"223bfe8d-0039-4cbd-b1f9-a892a2773d25","body":"forward body"}));
    let request = endpoint.request();
    assert_eq!(request.body["message"]["body"], "forward body");
    assert_eq!(
        request.body["message"]["id"],
        "223bfe8d-0039-4cbd-b1f9-a892a2773d25"
    );
    assert!(request.headers.contains_key("x-intercom-signature"));
    let receipt = relay.request("receipt");
    assert_eq!(receipt["status"], "delivered");
    assert_eq!(
        receipt["message_id"],
        "223bfe8d-0039-4cbd-b1f9-a892a2773d25"
    );
}

#[test]
fn legacy_relay_non_native_send_keeps_server_id_and_early_held_projection() {
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d27";
    let relay = WireRelay::new(
        move |request| match request["type"].as_str().unwrap_or_default() {
            "register" => vec![json!({"type":"welcome","machine_id":request["device_id"]})],
            "send" => vec![
                json!({"type":"receipt","session_id":request["reply_to"],"from":request["to"],"message_id":id,"status":"held"}),
                json!({"type":"send","id":request["id"],"message_id":id,"status":"accepted"}),
            ],
            _ => relay_basics(request),
        },
    );
    let mut fixture = stable_fixture();
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    let _origin = fixture.plugin("origin", "busy");
    let sent = tool(
        address,
        &fixture.mcp_token(),
        "send",
        json!({"from":"pi/origin","to":"pi/remote","message":"legacy send"}),
    );
    assert_eq!(sent["messageId"], id);
    let request = relay.request("send");
    assert!(
        request.get("message_id").is_none_or(|v| v == ""),
        "legacy relay was sent unsupported caller UUID: {request}"
    );
    let status = tool(
        address,
        &fixture.mcp_token(),
        "message_status",
        json!({"message_id":id}),
    );
    assert_eq!(
        status["status"], "held",
        "ACK overwrote receipt that arrived before it"
    );
}

#[test]
fn legacy_relay_rejects_native_origin_before_body_submission() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "register" {
            vec![json!({"type":"welcome","machine_id":request["device_id"]})]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let _native = super::super::native::ClaudePeer::new(&fixture);
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    fixture.connect().reject(json!({"op":"send","reply_to":"claude/native-fixture","to":"pi/remote","body":"must not escape to legacy relay"}),"upgrade relay");
    assert!(!relay
        .requests
        .try_iter()
        .any(|request| request["type"] == "send"));
}

#[test]
fn native_unverifiable_launch_settings_refuse_before_relay_body_even_when_file_allows() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = stable_fixture();
    let _native = super::super::native::ClaudePeer::with_flags(&fixture, &["--settings", "{}"]);
    fs::write(
        fixture.root.join(".claude/settings.json"),
        b"{\"isolatePeerMachines\":false}\n",
    )
    .unwrap();
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    let refused=fixture.connect().call(json!({"op":"send","reply_to":"claude/native-fixture","to":"pi/remote","body":"unverifiable launch override"}));
    assert_ne!(refused["ok"], true, "source launch settings were ignored");
    assert!(
        refused["error"].as_str().unwrap().contains("--settings"),
        "wrong refusal: {refused}"
    );
    assert!(
        !relay
            .requests
            .try_iter()
            .any(|request| request["type"] == "send"),
        "unverifiable source submitted body before refusal"
    );
}

#[test]
fn native_remote_policy_reloads_source_settings_before_each_relay_body() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "send" {
            vec![
                json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let _native = super::super::native::ClaudePeer::new(&fixture);
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    let mut sender = fixture.connect();
    sender.ok(json!({"op":"send","reply_to":"claude/native-fixture","to":"pi/remote","body":"allowed before settings change"}));
    assert_eq!(
        relay.request("send")["body"],
        "allowed before settings change"
    );
    fs::write(
        fixture.root.join(".claude/settings.json"),
        b"{\"isolatePeerMachines\":true}\n",
    )
    .unwrap();
    let refused=sender.call(json!({"op":"send","reply_to":"claude/native-fixture","to":"pi/remote","body":"blocked after settings change"}));
    assert_ne!(
        refused["ok"], true,
        "stale launch/settings policy authorized remote body"
    );
    assert!(
        refused["error"].as_str().unwrap().contains("refus"),
        "wrong policy outcome: {refused}"
    );
    assert!(
        !relay.requests.try_iter().any(|r| r["type"] == "send"),
        "policy refusal occurred after body submission"
    );
}

#[test]
fn remote_native_capacity_is_independent_and_rejects_before_1025th_relay_body() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "send" {
            vec![
                json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let _native = super::super::native::ClaudePeer::new(&fixture);
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    let mut unbound = fixture.connect();
    for _ in 0..1024 {
        let sent=unbound.ok(json!({"op":"send","reply_to":"claude/native-fixture","to":"pi/remote","body":"remote native pending"}));
        assert_eq!(sent["status"], "accepted");
    }
    unbound.reject(json!({"op":"send","reply_to":"claude/native-fixture","to":"pi/remote","body":"remote overflow must not be sent"}),"capacity");
    let requests: Vec<Value> = relay
        .requests
        .try_iter()
        .filter(|r| r["type"] == "send")
        .collect();
    assert_eq!(
        requests.len(),
        1024,
        "remote active overflow reached relay or previously admitted bodies vanished"
    );
    assert!(!requests
        .iter()
        .any(|r| r["body"] == "remote overflow must not be sent"));
    let mut sender = fixture.plugin("local-sender", "busy");
    let mut target = fixture.plugin("local-target", "busy");
    let local =
        sender.ok(json!({"op":"send","to":"pi/local-target","body":"independent local capacity"}));
    assert_eq!(target.event("deliver")["message_id"], local["message_id"]);
}

#[test]
fn native_remote_dropped_receipts_preserve_normalized_reasons_and_opaque_original_ids() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "send" {
            vec![
                json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let native = super::super::native::ClaudePeer::new(&fixture);
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    let alias = super::super::native::alias_record(&fixture, "pi/remote");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    for (index, reason, expected) in [
        (0, "queue-full", "queue-full"),
        (1, "rate-limited", "rate-limited"),
        (2, "untrusted-arbitrary-reason", ""),
    ] {
        let original = format!("native-opaque-drop-reason-{index}");
        let body = format!("remote drop reason body {index}");
        native.send_control(
            socket,
            super::super::native::native_user(&native, &original, &body),
        );
        let sent = relay.request("send");
        assert_eq!(sent["to"], "pi/remote");
        assert_eq!(sent["reply_to"], "claude/native-fixture");
        assert_eq!(sent["body"], body);
        assert_ne!(sent["message_id"], original);
        assert!(uuid::Uuid::parse_str(sent["message_id"].as_str().unwrap()).is_ok());
        relay.outgoing.send(json!({"type":"receipt","session_id":sent["reply_to"],"from":sent["to"],"message_id":sent["message_id"],"status":"dropped","drop_reason":reason})).unwrap();
        let receipt = native.frame_for(&format!("native dropped reason {reason}"));
        assert_eq!(receipt["action"], "peer_message_status");
        assert_eq!(receipt["status"], "dropped");
        assert_eq!(receipt["orig_msg_id"], original);
        assert_eq!(receipt["from"], format!("uds:{socket}"));
        assert_eq!(
            receipt["drop_reason"].as_str().unwrap_or(""),
            expected,
            "normalized relay drop_reason was lost or an arbitrary reason was forwarded"
        );
        native.no_frame();
    }
}

#[test]
fn native_remote_alias_maps_transport_receipts_to_original_uuid_and_pinned_source_endpoint() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "send" {
            vec![
                json!({"type":"receipt","session_id":request["reply_to"],"from":request["to"],"message_id":request["message_id"],"status":"held"}),
                json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let native = super::super::native::ClaudePeer::new(&fixture);
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    fixture.wait_session("claude/native-fixture");
    let alias = super::super::native::alias_record(&fixture, "pi/remote");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d31";
    native.send_control(
        socket,
        super::super::native::native_user(&native, id, "native remote body"),
    );
    let sent = relay.request("send");
    assert_eq!(sent["to"], "pi/remote");
    assert_eq!(sent["reply_to"], "claude/native-fixture");
    assert_eq!(sent["body"], "native remote body");
    assert!(sent["message_id"].as_str().is_some_and(|id| !id.is_empty()));
    let held = native.frame_for("remote alias early held before relay ACK");
    assert_eq!(held["status"], "held");
    assert_eq!(held["orig_msg_id"], id);
    assert_eq!(held["from"], format!("uds:{socket}"));
    native.remove_registry(&fixture);
    relay.outgoing.send(json!({"type":"receipt","session_id":sent["reply_to"],"from":sent["to"],"message_id":sent["message_id"],"status":"delivered"})).unwrap();
    let delivered = native.frame_for("remote delivered after native source registry withdrawal");
    assert_eq!(delivered["action"], "peer_message_status");
    assert_eq!(delivered["status"], "delivered");
    assert_eq!(
        delivered["orig_msg_id"], id,
        "transport UUID replaced original native UUID"
    );
    assert_eq!(
        delivered["from"],
        format!("uds:{socket}"),
        "receipt used a rediscovered alias instead of admitted endpoint snapshot"
    );
    relay.outgoing.send(json!({"type":"receipt","session_id":sent["reply_to"],"from":sent["to"],"message_id":sent["message_id"],"status":"read"})).unwrap();
    assert_eq!(
        tool(
            address,
            &fixture.mcp_token(),
            "message_status",
            json!({"message_id":sent["message_id"]})
        )["status"],
        "delivered",
        "native relay delivered is terminal; later read is event-only"
    );
    native.no_frame();
}

#[test]
fn native_remote_alias_policy_refuses_before_body_and_directory_withdrawal_retires_alias() {
    let visible = Arc::new(AtomicBool::new(true));
    let directory = visible.clone();
    let relay = WireRelay::new(move |request| {
        if request["type"] == "list" && !directory.load(Ordering::Relaxed) {
            vec![json!({"type":"list","id":request["id"],"sessions":[]})]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let native = super::super::native::ClaudePeer::new(&fixture);
    start_relay(&mut fixture, &relay, "remote_messages='refuse'\n");
    fixture.wait_session("claude/native-fixture");
    let alias = super::super::native::alias_record(&fixture, "pi/remote");
    let socket = PathBuf::from(alias["messagingSocketPath"].as_str().unwrap());
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d32";
    native.send_control(
        socket.to_str().unwrap(),
        super::super::native::native_user(&native, id, "policy must refuse"),
    );
    let refused = native.frame();
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["orig_msg_id"], id);
    assert!(
        !relay
            .requests
            .try_iter()
            .any(|request| request["type"] == "send"),
        "refused native source still sent relay body"
    );
    visible.store(false, Ordering::Relaxed);
    relay
        .outgoing
        .send(json!({"type":"list","id":"directory","sessions":[]}))
        .unwrap();
    fixture.wait_absent("pi/remote");
    let registry = fixture
        .root
        .join(".claude/sessions")
        .join(format!("{}.json", alias["pid"].as_u64().unwrap()));
    let deadline = Instant::now() + TIMEOUT;
    while socket.exists()
        || registry.exists()
        || common::pid_alive(alias["pid"].as_u64().unwrap() as u32)
    {
        fixture.sessions();
        assert!(
            Instant::now() < deadline,
            "withdrawn remote session retained its native alias publication/helper"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn history_capacity_pins_held_and_active_and_rejects_before_local_injection() {
    let first = Arc::new(AtomicBool::new(true));
    let once = first.clone();
    let relay = WireRelay::new(move |request| {
        if request["type"] == "send" {
            let ack = json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"});
            if once.swap(false, Ordering::Relaxed) {
                vec![
                    json!({"type":"receipt","session_id":request["reply_to"],"from":request["to"],"message_id":request["message_id"],"status":"held"}),
                    ack,
                ]
            } else {
                vec![ack]
            }
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let mut first_id = Value::Null;
    for index in 0..4096 {
        let sent = sender.ok(json!({"op":"send","to":"pi/remote","body":"retained pending"}));
        if index == 0 {
            first_id = sent["message_id"].clone();
        }
    }
    let held = sender.event("receipt");
    assert_eq!(held["status"], "held");
    assert_eq!(held["message_id"], first_id);
    let rejected =
        sender.call(json!({"op":"send","to":"pi/target","body":"must never enter target"}));
    assert_ne!(
        rejected["ok"], true,
        "4097th active history admission succeeded: {rejected}"
    );
    assert!(rejected["error"].as_str().unwrap().contains("capacity"));
    target.no_event();
    let status = tool(
        address,
        &fixture.mcp_token(),
        "message_status",
        json!({"message_id":first_id}),
    );
    assert_eq!(
        status["status"], "held",
        "history pressure evicted or downgraded a live hold"
    );
}

#[test]
fn local_active_capacity_rejects_1025th_before_bytes_but_remote_history_still_admits() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "send" {
            vec![
                json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    start_relay(&mut fixture, &relay, "");
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    for _ in 0..1024 {
        let sent = sender.ok(json!({"op":"send","to":"pi/target","body":"awaiting read"}));
        assert_eq!(target.event("deliver")["message_id"], sent["message_id"]);
    }
    sender.reject(
        json!({"op":"send","to":"pi/target","body":"overflow must not arrive"}),
        "capacity",
    );
    target.no_event();
    let remote =
        sender.ok(json!({"op":"send","to":"pi/remote","body":"separate remote admission"}));
    assert_eq!(remote["status"], "accepted");
    assert_eq!(relay.request("send")["message_id"], remote["message_id"]);
}

#[test]
fn relay_terminal_status_is_first_writer_wins_even_after_requeue_and_lower_receipts() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "send" {
            vec![
                json!({"type":"send","id":request["id"],"message_id":request["message_id"],"status":"accepted"}),
            ]
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = stable_fixture();
    let address = vacant_address();
    let listen = address.to_string();
    start_relay_options(&mut fixture, &relay, "", &["--mcp-listen", &listen]);
    let mut sender = fixture.plugin("sender", "busy");
    for terminal in ["denied", "expired", "refused"] {
        let sent = sender.ok(json!({"op":"send","to":"pi/remote","body":"terminal state"}));
        let id = sent["message_id"].clone();
        for state in ["held", terminal, "queued", "delivered", "read"] {
            relay.push(json!({"type":"receipt","session_id":"pi/sender","from":"pi/remote","message_id":id,"status":state}));
            let event = sender.event("receipt");
            assert_eq!(event["message_id"], id);
            assert_eq!(
                event["status"], state,
                "receipt stream remains arrival-ordered; only query projection folds status"
            );
        }
        let status = tool(
            address,
            &fixture.mcp_token(),
            "message_status",
            json!({"message_id":id}),
        );
        assert_eq!(
            status["status"], terminal,
            "later terminal or lower rank rewrote terminal truth"
        );
    }
}
