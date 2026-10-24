use super::network::{mcp, tool, vacant_address};
use super::*;
use std::net::SocketAddr;

const MINUTE: u64 = 60 * 1000;
const HOUR: u64 = 60 * MINUTE;

fn maintenance_barrier(fixture: &Fixture) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = common::clock_request(
            &fixture.root.join("manager"),
            json!({"type":"clock_status"}),
        );
        if state["pending"].as_array().is_some_and(|timers| {
            timers
                .iter()
                .any(|timer| timer["label"] == "intercom_maintenance" && timer["due_in_ms"] == 250)
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "intercom maintenance did not re-arm on the daemon's manual clock: {state}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn advance(fixture: &Fixture, ms: u64) {
    common::clock_request(
        &fixture.root.join("manager"),
        json!({"type":"clock_advance","ms":ms}),
    );
    if ms >= 250 {
        maintenance_barrier(fixture);
    }
}

const MCP_TOKEN: &str = "clock-secret";

fn start(fixture: &mut Fixture) -> SocketAddr {
    let address = vacant_address();
    let listen = address.to_string();
    fixture.start_with(
        &["--mcp-listen", &listen, "--mcp-token", MCP_TOKEN],
        &[("PI_FAMULUS_TEST_CLOCK", "manual")],
    );
    maintenance_barrier(fixture);
    address
}

fn message_status(address: SocketAddr, id: &Value) -> Value {
    tool(address, MCP_TOKEN, "message_status", json!({"message_id":id}))
}

fn unknown_message(address: SocketAddr, id: &Value) {
    let result = mcp(
        address,
        MCP_TOKEN,
        "tools/call",
        json!({"name":"message_status","arguments":{"message_id":id}}),
    );
    assert_eq!(
        result["isError"], true,
        "expired message history remained visible: {result}"
    );
    assert!(
        result.to_string().contains("unknown message"),
        "wrong lookup failure: {result}"
    );
}

#[test]
fn ordinary_history_expires_after_ten_minutes_and_status_reads_do_not_renew_it() {
    let mut fixture = Fixture::new();
    let address = start(&mut fixture);
    let mut sender = fixture.plugin("sender", "busy");
    let mut target = fixture.plugin("target", "busy");
    let sent = sender.ok(json!({"op":"send","to":"pi/target","body":"unread ordinary route"}));
    target.event("deliver");
    advance(&fixture, 5 * MINUTE);
    assert_eq!(
        message_status(address, &sent["message_id"])["status"],
        "delivered"
    );
    advance(&fixture, 5 * MINUTE + 1);
    unknown_message(address, &sent["message_id"]);
}

#[test]
fn native_held_route_survives_eleven_minutes_then_terminal_history_ages_normally() {
    let mut fixture = Fixture::new();
    let native = native::ClaudePeer::new(&fixture);
    let address = start(&mut fixture);
    fixture.wait_session("claude/native-fixture");
    let mut sender = fixture.plugin("sender", "busy");
    let sent =
        sender.ok(json!({"op":"send","to":"claude/native-fixture","body":"long approval wait"}));
    let user = native.frame();
    native.receipt(&user, "held");
    assert_eq!(sender.event("receipt")["status"], "held");
    advance(&fixture, 11 * MINUTE);
    assert_eq!(
        message_status(address, &sent["message_id"])["status"],
        "held"
    );
    native.receipt(&user, "denied");
    let terminal = sender.event("receipt");
    assert_eq!(terminal["status"], "denied");
    assert_eq!(terminal["message_id"], sent["message_id"]);
    advance(&fixture, 5 * MINUTE);
    assert_eq!(
        message_status(address, &sent["message_id"])["status"],
        "denied"
    );
    advance(&fixture, 5 * MINUTE + 1);
    unknown_message(address, &sent["message_id"]);
}

#[test]
fn pending_idle_expires_at_twelve_hours_and_terminal_reads_do_not_renew_tombstone() {
    let mut fixture = Fixture::new();
    start(&mut fixture);
    let mut watcher = fixture.plugin("watcher", "busy");
    let _target = fixture.plugin("target", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"pi/target"}));
    let id = subscribed["subscription_id"].clone();
    advance(&fixture, 6 * HOUR);
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":id}))["status"],
        "subscribed"
    );
    advance(&fixture, 6 * HOUR + 1);
    let notice = watcher.event("idle_notice");
    assert_eq!(notice["subscription_id"], id);
    assert_eq!(notice["status"], "expired");
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":id}))["status"],
        "expired"
    );
    advance(&fixture, 6 * HOUR);
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":id}))["status"],
        "expired"
    );
    advance(&fixture, 6 * HOUR + 1);
    watcher.reject(json!({"op":"idle_status","subscription_id":id}), "unknown");
}

#[test]
fn idle_settle_uses_shared_manual_clock_and_waiting_resets_the_candidate() {
    let mut fixture = Fixture::new();
    start(&mut fixture);
    let mut watcher = fixture.plugin("watcher", "busy");
    let mut target = fixture.plugin("target", "busy");
    let subscribed = watcher.ok(json!({"op":"subscribe_idle","to":"pi/target"}));
    target.register("target", "idle", &["read_receipts", "idle_state_v1"]);
    advance(&fixture, 500);
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
            ["status"],
        "subscribed"
    );
    target.register("target", "waiting", &["read_receipts", "idle_state_v1"]);
    advance(&fixture, 250);
    target.register("target", "idle", &["read_receipts", "idle_state_v1"]);
    advance(&fixture, 250);
    maintenance_barrier(&fixture);
    advance(&fixture, 749);
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
            ["status"],
        "subscribed",
        "idle candidate survived waiting or fired before 750ms"
    );
    advance(&fixture, 251);
    assert_eq!(
        watcher.ok(json!({"op":"idle_status","subscription_id":subscribed["subscription_id"]}))
            ["status"],
        "idle",
        "750ms settle used wall time rather than the shared manual clock"
    );
    assert_eq!(
        watcher.event("idle_notice")["subscription_id"],
        subscribed["subscription_id"]
    );
}
