//! Black-box contracts for a daemon whose spawner went away and for the
//! relay controls the extension drives: stdio redirect, reconnect-now,
//! `retry_at_ms` and `device_id` in `status`.
#![cfg(unix)]

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const S: fn(u64) -> Duration = Duration::from_secs;

/// A relay that answers every upgrade with HTTP 401, which parks the
/// daemon's reconnect loop for the 300s auth backoff. Counts the attempts.
fn rejecting_relay() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counted.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    (format!("http://{address}"), attempts)
}

/// A real-time home whose manager config enables messaging toward `server`.
fn messaging_home(name: &str, messaging: Value) -> Home {
    let home = Home::new_real(name);
    fs::set_permissions(&home.path, fs::Permissions::from_mode(0o700)).unwrap();
    let state = home.path.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let mut messaging = messaging;
    messaging["enabled"] = json!(true);
    messaging["sock"] = json!(home.path.join("i.sock"));
    messaging["stateDir"] = json!(state);
    fs::write(
        home.path.join("config.json"),
        serde_json::to_vec(&json!({ "messaging": messaging })).unwrap(),
    )
    .unwrap();
    home
}

fn command(home: &Home) -> std::process::Command {
    let mut command = famulus_command(BIN);
    command
        .env_remove("PI_FAMULUS_NO_INTERCOM")
        .env("PI_FAMULUS_TEST_OWNER", test_owner())
        .env("HOME", &home.path)
        .env("XDG_CONFIG_HOME", home.path.join("config"))
        .env("HERDR_SOCKET_PATH", home.path.join("absent-herdr.sock"))
        .arg("--home")
        .arg(&home.path)
        .arg("daemon")
        .stdin(Stdio::null());
    command
}

fn status_conn(home: &Home) -> Conn {
    let mut conn = home.connect();
    conn.hello_ext("status-session");
    conn
}

fn messaging_status(conn: &mut Conn) -> Value {
    conn.request_ok(json!({"type":"status"}))["messaging"].clone()
}

fn relay_log_lines(home: &Home) -> usize {
    fs::read_to_string(home.path.join("manager.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("intercom relay:"))
        .count()
}

/// The extension spawns the daemon with pipes it reads; when that pi exits
/// the peers vanish. The daemon must keep working and log to manager.log.
#[test]
fn daemon_whose_spawner_pipes_are_gone_keeps_relaying_and_logs_to_manager_log() {
    let (server, attempts) = rejecting_relay();
    let home = messaging_home("dead-spawner", json!({"server": server, "token": "bootstrap"}));
    let (out_child, out_parent) = UnixStream::pair().unwrap();
    let (err_child, err_parent) = UnixStream::pair().unwrap();
    let mut spawn = command(&home);
    spawn.stdout(Stdio::from(std::os::fd::OwnedFd::from(out_child)));
    spawn.stderr(Stdio::from(std::os::fd::OwnedFd::from(err_child)));
    let daemon = home.adopt(spawn.spawn().expect("spawn daemon"));
    let mut conn = status_conn(&home);
    assert!(
        poll_true(S(5), || attempts.load(Ordering::SeqCst) == 1 && relay_log_lines(&home) == 1),
        "first relay failure must be logged to manager.log, not the spawner's pipe: {}",
        fs::read_to_string(home.path.join("manager.log")).unwrap_or_default()
    );
    // The spawning pi exits: both pipe peers disappear.
    drop(out_parent);
    drop(err_parent);
    // A new failure after that point must still be logged and not kill relaying.
    conn.request_ok(json!({"type":"messaging_reconnect"}));
    assert!(
        poll_true(S(5), || attempts.load(Ordering::SeqCst) == 2 && relay_log_lines(&home) == 2),
        "a relay failure after the spawner left was lost: {}",
        fs::read_to_string(home.path.join("manager.log")).unwrap_or_default()
    );
    assert!(pid_alive(daemon.id()), "daemon died after its spawner's pipes closed");
    assert_eq!(messaging_status(&mut conn)["state"], "disconnected");
}

/// Output printed before the daemon commits to serving still reaches the
/// spawner: the extension reads it until the socket is ready.
#[test]
fn contention_line_still_reaches_the_spawners_pipe() {
    let home = Home::new_real("contention");
    let _first = home.start_daemon();
    // A daemon with no connections exits after 5s; on a loaded runner the
    // second spawn can land after that and become the daemon itself.
    let _keepalive = status_conn(&home);
    let mut second = command(&home);
    second.env("PI_FAMULUS_NO_INTERCOM", "1");
    let output = second
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("pi-famulus already running"),
        "{output:?}"
    );
}

#[test]
fn status_reports_next_retry_and_reconnect_now_skips_the_backoff() {
    let (server, attempts) = rejecting_relay();
    let home = messaging_home("retry-at", json!({"server": server, "token": "bootstrap"}));
    let _daemon = home.adopt(command(&home).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let mut conn = status_conn(&home);
    assert!(poll_true(S(5), || attempts.load(Ordering::SeqCst) == 1));
    let status = poll_until(S(5), || {
        let status = messaging_status(&mut conn);
        status["retry_at_ms"].is_u64().then_some(status)
    })
    .expect("a disconnected relay reports its next retry");
    assert_eq!(status["state"], "disconnected", "{status}");
    let now = proto_now_ms();
    let retry_at = status["retry_at_ms"].as_u64().unwrap();
    assert!(
        retry_at > now + 250_000 && retry_at <= now + 301_000,
        "auth failure backs off ~300s: retry_at={retry_at} now={now}"
    );
    let report = conn.request_ok(json!({"type":"messaging_reconnect"}));
    assert!(report.get("error").is_none(), "{report}");
    assert!(
        poll_true(S(5), || attempts.load(Ordering::SeqCst) == 2),
        "reconnect-now did not attempt before the backoff elapsed"
    );
}

fn proto_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn status_reports_the_enrolled_device_id_only_for_the_configured_server() {
    let (server, _attempts) = rejecting_relay();
    for (identity_server, expected) in [(server.clone(), "0b6c1a52-8f0e-4a4e-9a52-3f4c7f1d2e11"), ("http://other.invalid".to_string(), "")] {
        let home = messaging_home("device-id", json!({"server": server, "token": "bootstrap"}));
        let device = json!({"device_id":"0b6c1a52-8f0e-4a4e-9a52-3f4c7f1d2e11","credential":"enrolled","server":identity_server});
        let path = home.path.join("state/device.json");
        fs::write(&path, serde_json::to_vec(&device).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let _daemon = home.adopt(command(&home).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let mut conn = status_conn(&home);
        let status = poll_until(S(5), || {
            let status = messaging_status(&mut conn);
            (status["state"] != "unavailable").then_some(status)
        })
        .unwrap_or_else(|| panic!("{}: {}", identity_server, messaging_status(&mut conn)));
        assert_eq!(
            status.get("device_id").and_then(Value::as_str).unwrap_or(""),
            expected,
            "{status}"
        );
    }
}

#[test]
fn reconnect_without_a_relay_is_refused_with_a_reason() {
    let cases = [
        ("disabled", json!({"enabled": false}), "E_INTERCOM_DISABLED"),
        ("local-only", json!({"localOnly": true}), "E_INTERCOM_DISABLED"),
    ];
    for (name, messaging, code) in cases {
        let home = messaging_home(name, json!({}));
        let mut config: Value = serde_json::from_slice(&fs::read(home.path.join("config.json")).unwrap()).unwrap();
        for (key, value) in messaging.as_object().unwrap() {
            config["messaging"][key] = value.clone();
        }
        fs::write(home.path.join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
        let _daemon = home.adopt(command(&home).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let mut conn = status_conn(&home);
        let response = conn.request(json!({"type":"messaging_reconnect"}));
        assert_eq!(response["ok"], false, "{name}: {response}");
        assert_eq!(response["error"]["code"], code, "{name}: {response}");
        assert!(
            response["error"]["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{name}: {response}"
        );
    }
}
