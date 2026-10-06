mod common;

use common::*;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;

fn install_layout(home: &Home, package: &str) -> std::path::PathBuf {
    let bin_dir = home.path.join("node_modules").join(package).join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let binary = bin_dir.join("pi-famulus");
    replace_binary(&binary, Path::new(BIN));
    binary
}

fn start_shell(home: &Home) {
    let mut client = home.connect();
    client.request_ok(json!({"type":"hello","client_kind":"extension","session_id":"retired-test",
        "pi_pid":std::process::id(),"cwd":"/tmp","protocol":2}));
    let (task, _) = client.start_with(json!({"type":"start","kind":"shell","command":"printf survived",
        "cwd":"/tmp","env":{"PATH":PATH_ENV}}));
    let done = client.request_ok(json!({"type":"wait","task_id":task,"budget_ms":10000}));
    assert_eq!(done["done"], true);
    let output = client.request_ok(json!({"type":"output","task_id":task,"cursor":0,"max_bytes":1024}));
    assert_eq!(output["chunk"], "survived");
}

fn assert_upgrade(home: &Home) {
    let out = home.cli(&["upgrade"], Duration::from_secs(40));
    assert!(
        out.status.success(),
        "upgrade failed: {} {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("upgraded in place"),
        "upgrade did not hand over: {}",
        out.stdout
    );
}

/// Stable-layout startup followed by npm retirement, deletion and reinstall.
#[cfg(target_os = "linux")]
#[test]
fn stable_install_survives_rename_delete_and_reinstall() {
    let home = Home::new("stable-retirement");
    let stable = install_layout(&home, "pi-famulus-linux-x64");
    let daemon = home.start_daemon_from(&stable, &[]);
    let pid = home.pidfile_pid().unwrap();
    let retired = home.path.join("node_modules/.pi-famulus-linux-x64-AWM9wakS");
    fs::rename(stable.parent().unwrap().parent().unwrap(), &retired).unwrap();
    fs::remove_dir_all(&retired).unwrap();
    let link = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    assert!(link.to_string_lossy().ends_with(" (deleted)"), "daemon exe was not deleted: {link:?}");
    // npm installs the new stable package before the next upgrade attempt.
    let _new = install_layout(&home, "pi-famulus-linux-x64");
    start_shell(&home);
    assert_upgrade(&home);
    drop(daemon);
}

/// A daemon initially launched from npm's already-retired package layout must
/// map <retired>/bin/pi-famulus to the stable sibling's bin/pi-famulus.
#[cfg(target_os = "linux")]
#[test]
fn retired_layout_startup_maps_bin_executable_to_stable_sibling() {
    let home = Home::new("retired-layout");
    let stable = install_layout(&home, "pi-famulus-linux-x64");
    let retired = install_layout(&home, ".pi-famulus-linux-x64-AWM9wakS");
    let daemon = home.start_daemon_from(&retired, &[]);
    let pid = home.pidfile_pid().unwrap();
    fs::remove_dir_all(retired.parent().unwrap().parent().unwrap()).unwrap();
    let link = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    assert!(link.to_string_lossy().ends_with(" (deleted)"), "daemon exe was not deleted: {link:?}");
    start_shell(&home);
    assert_upgrade(&home);
    assert!(stable.is_file());
    drop(daemon);
}

/// A daemon started before npm creates the stable sibling must recover its
/// executable path after npm deletes the retired directory.
#[cfg(target_os = "linux")]
#[test]
fn daemon_started_mid_reinstall_re_resolves_its_executable() {
    let home = Home::new("mid-reinstall");
    let retired = install_layout(&home, ".pi-famulus-linux-x64-AWM9wakS");
    let daemon = home.start_daemon_from(&retired, &[]);
    fs::remove_dir_all(retired.parent().unwrap().parent().unwrap()).unwrap();
    let stable = install_layout(&home, "pi-famulus-linux-x64");
    start_shell(&home);
    assert_upgrade(&home);
    assert!(stable.is_file());
    drop(daemon);
}
