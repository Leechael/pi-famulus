use super::*;

fn ack(request: &Value) -> Vec<Value> {
    if request["type"] == "idle_subscribe" {
        vec![
            json!({"type":"idle_subscribe","id":request["id"],"subscription_id":request["subscription_id"],"status":"subscribed","rpc_reply":true}),
        ]
    } else {
        relay_basics(request)
    }
}
fn notice(request: &Value, status: &str, id: &str) -> Value {
    json!({"type":"idle_notice","id":id,"origin_machine":request["machine_id"],"subscription_id":request["subscription_id"],"watcher":request["watcher"],"from":request["session_id"],"status":status})
}

#[test]
fn remote_idle_notice_before_ack_keeps_terminal_state_and_exact_subscription_correlation() {
    let relay = WireRelay::new(|request| {
        if request["type"] == "idle_subscribe" {
            let mut messages = vec![notice(request, "idle", "early-idle")];
            messages.extend(ack(request));
            messages
        } else {
            relay_basics(request)
        }
    });
    let mut fixture = Fixture::new();
    start_relay(&mut fixture, &relay, "");
    let mut watcher = fixture.plugin("watcher", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"pi/remote"}));
    let request = relay.request("idle_subscribe");
    assert_eq!(request["machine_id"], "other-device");
    assert_eq!(request["watcher"], "pi/watcher");
    assert_eq!(request["session_id"], "pi/remote");
    assert_eq!(request["subscription_id"], subscribed["subscription_id"]);
    assert_eq!(
        subscribed["status"], "idle",
        "late subscribed ACK regressed the canonical early terminal notice"
    );
    let pushed = watcher.event("idle_notice");
    assert_eq!(pushed["subscription_id"], subscribed["subscription_id"]);
    assert_eq!(pushed["status"], "idle");
    assert_eq!(pushed["from"], "pi/remote");
    assert!(pushed.get("body").is_none_or(|body| body == ""));
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
            ["status"],
        "idle"
    );
}

#[test]
fn remote_idle_auth_target_watcher_and_uuid_mismatch_never_consume_the_live_watch() {
    let relay = WireRelay::new(ack);
    let mut fixture = Fixture::new();
    start_relay(&mut fixture, &relay, "");
    let mut watcher = fixture.plugin("watcher", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"pi/remote"}));
    let request = relay.request("idle_subscribe");
    for (index, key, value) in [
        (0, "origin_machine", json!("")),
        (1, "origin_machine", json!("wrong-machine")),
        (2, "from", json!("pi/wrong-target")),
        (3, "watcher", json!("pi/other-watcher")),
        (4, "subscription_id", json!("unrelated-subscription")),
    ] {
        let id = format!("uncorrelated-{index}");
        let mut rejected = notice(&request, "idle", &id);
        rejected[key] = value;
        relay.outgoing.send(rejected).unwrap();
        if index != 0 {
            let error = relay.request("error");
            assert_eq!(error["id"], id);
        }
    }
    watcher.no_event();
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
            ["status"],
        "subscribed"
    );
    relay
        .outgoing
        .send(notice(&request, "idle", "valid-idle"))
        .unwrap();
    assert_eq!(
        watcher.event("idle_notice")["subscription_id"],
        subscribed["subscription_id"]
    );
}

#[test]
fn remote_idle_watches_survive_reconnect_and_notices_keep_original_subscription_identity() {
    let relay = WireRelay::new(ack);
    let mut fixture = Fixture::new();
    start_relay(&mut fixture, &relay, "");
    let mut watcher = fixture.plugin("watcher", "busy");
    let old = watcher.ok(json!({"op":"subscribe_idle","to":"pi/remote"}));
    let old_request = relay.request("idle_subscribe");
    relay
        .outgoing
        .send(json!({"_fixture_disconnect":true}))
        .unwrap();
    watcher.no_event();
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":old["subscription_id"]}))["status"],
        "subscribed",
        "relay loss must not terminate an already acknowledged remote watch"
    );
    relay.request("register");
    fixture.wait_session("pi/remote");
    let fresh = watcher.ok(json!({"op":"subscribe_idle","to":"pi/remote"}));
    let current = relay.request("idle_subscribe");
    assert_ne!(fresh["subscription_id"], old["subscription_id"]);
    relay
        .outgoing
        .send(notice(&old_request, "idle", "old-after-reconnect"))
        .unwrap();
    let old_notice = watcher.event("idle_notice");
    assert_eq!(old_notice["subscription_id"], old["subscription_id"]);
    assert_eq!(old_notice["status"], "idle");
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":fresh["subscription_id"]}))
            ["status"],
        "subscribed",
        "old remote notice completed a different subscription"
    );
    relay
        .outgoing
        .send(notice(&current, "idle", "fresh-after-reconnect"))
        .unwrap();
    let pushed = watcher.event("idle_notice");
    assert_eq!(pushed["subscription_id"], fresh["subscription_id"]);
    assert_eq!(pushed["status"], "idle");
}

#[test]
fn native_remote_idle_refresh_cancels_previous_transport_watch_and_only_notifies_latest_native_uuid(
) {
    let relay = WireRelay::new(ack);
    let mut fixture = Fixture::new();
    let native = super::super::native::ClaudePeer::new(&fixture);
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    let alias = super::super::native::alias_record(&fixture, "pi/remote");
    let socket = alias["messagingSocketPath"].as_str().unwrap();
    let first = "223bfe8d-0039-4cbd-b1f9-a892a2773d35";
    let latest = "223bfe8d-0039-4cbd-b1f9-a892a2773d36";
    let subscribe = |id: &str| {
        native.send_control(socket,json!({"type":"control","action":"notify_when_idle","from":format!("uds:{}",native.path.display()),"msg_id":id}))
    };
    subscribe(first);
    let original = relay.request("idle_subscribe");
    assert_eq!(original["watcher"], "claude/native-fixture");
    native.no_frame();
    subscribe(latest);
    let cancelled = relay.request("idle_cancel");
    let current = relay.request("idle_subscribe");
    assert_eq!(cancelled["subscription_id"], original["subscription_id"]);
    assert_eq!(cancelled["session_id"], original["session_id"]);
    assert_ne!(cancelled["subscription_id"], current["subscription_id"]);
    assert_eq!(cancelled["watcher"], "claude/native-fixture");
    let stale = notice(&original, "exited", "old-native-observer");
    relay.outgoing.send(stale).unwrap();
    native.no_frame();
    relay
        .outgoing
        .send(notice(&current, "idle", "latest-native-observer"))
        .unwrap();
    let result = native.frame();
    assert_eq!(result["type"], "control");
    assert_eq!(result["action"], "peer_idle_notice");
    assert_eq!(result["orig_msg_id"], latest);
    assert_eq!(result["state"], "idle");
    assert!(result.get("message").is_none_or(Value::is_null));
}

#[test]
fn remote_forwarded_idle_subscribe_and_cancel_require_matching_authenticated_origin() {
    let relay = WireRelay::new(relay_basics);
    let mut fixture = Fixture::new();
    start_relay(&mut fixture, &relay, "");
    let mut target = fixture.plugin("local-target", "busy");
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d37";
    let mut subscribe = json!({"type":"idle_subscribe","id":"unauthenticated-subscribe","subscription_id":id,"watcher":"codex/remote-watcher","session_id":"pi/local-target"});
    relay.outgoing.send(subscribe.clone()).unwrap();
    subscribe["id"] = json!("authenticated-subscribe");
    subscribe["origin_machine"] = json!("authenticated-watcher-machine");
    relay.outgoing.send(subscribe).unwrap();
    let subscribed = relay.request("idle_subscribe");
    assert_eq!(subscribed["id"], "authenticated-subscribe");
    assert_eq!(subscribed["status"], "subscribed");
    let mut cancel = json!({"type":"idle_cancel","id":"wrong-cancel","origin_machine":"another-machine","subscription_id":id,"watcher":"codex/remote-watcher","session_id":"pi/local-target"});
    relay.outgoing.send(cancel.clone()).unwrap();
    assert_eq!(relay.request("error")["id"], "wrong-cancel");
    cancel["id"] = json!("matching-cancel");
    cancel["origin_machine"] = json!("authenticated-watcher-machine");
    relay.outgoing.send(cancel).unwrap();
    assert_eq!(relay.request("idle_cancel")["id"], "matching-cancel");
    let control = "223bfe8d-0039-4cbd-b1f9-a892a2773d39";
    relay
        .outgoing
        .send(json!({"type":"idle_subscribe","id":"control-subscribe","origin_machine":"authenticated-watcher-machine","subscription_id":control,"watcher":"codex/remote-watcher","session_id":"pi/local-target"}))
        .unwrap();
    assert_eq!(relay.request("idle_subscribe")["id"], "control-subscribe");
    target.register("local-target", "idle", &["read_receipts", "idle_state_v1"]);
    let deadline = Instant::now() + TIMEOUT;
    let mut settled_at = None;
    while settled_at.is_none_or(|at: Instant| at.elapsed() < Duration::from_millis(500)) {
        let message = relay.requests.recv_timeout(Duration::from_millis(50)).ok();
        if let Some(message) = message.filter(|message| message["type"] == "idle_notice") {
            assert_ne!(
                message["subscription_id"], id,
                "cancelled remote watch later notified its former owner"
            );
            if message["subscription_id"] == control {
                assert_eq!(message["status"], "idle");
                settled_at = Some(Instant::now());
            }
        }
        assert!(
            settled_at.is_some() || Instant::now() < deadline,
            "uncancelled control watch on the same target never settled"
        );
    }
}

#[test]
fn native_managed_policy_blocks_remote_idle_before_any_subscription_bytes() {
    let relay = WireRelay::new(ack);
    let mut fixture = Fixture::new();
    let native = super::super::native::ClaudePeer::new(&fixture);
    fs::write(
        fixture.root.join(".claude/settings.json"),
        b"{\"isolatePeerMachines\":true}\n",
    )
    .unwrap();
    start_relay(&mut fixture, &relay, "");
    fixture.wait_session("claude/native-fixture");
    let alias = super::super::native::alias_record(&fixture, "pi/remote");
    let id = "223bfe8d-0039-4cbd-b1f9-a892a2773d38";
    native.send_control(alias["messagingSocketPath"].as_str().unwrap(),json!({"type":"control","action":"notify_when_idle","from":format!("uds:{}",native.path.display()),"msg_id":id}));
    let refused = native.frame();
    assert_eq!(refused["action"], "peer_idle_notice");
    assert_eq!(refused["orig_msg_id"], id);
    assert_eq!(refused["state"], "unavailable");
    assert!(
        !relay
            .requests
            .try_iter()
            .any(|r| r["type"] == "idle_subscribe"),
        "managed source policy was checked after remote subscription bytes"
    );
}
