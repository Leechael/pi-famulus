mod common;

use common::*;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;

#[cfg(target_os = "linux")]
#[test]
fn daemon_survives_retired_package_directory() {
    let home = Home::new("retired-package");
    let modules = home.path.join("node_modules");
    let retired = modules.join(".pi-famulus-linux-x64-random");
    let stable = modules.join("pi-famulus-linux-x64");
    fs::create_dir_all(&retired).unwrap();
    fs::create_dir_all(&stable).unwrap();
    let retired_bin = retired.join("pi-famulus");
    let stable_bin = stable.join("pi-famulus");
    replace_binary(&retired_bin, Path::new(BIN));
    replace_binary(&stable_bin, Path::new(BIN));
    let daemon = home.start_daemon_from(&retired_bin, &[]);

    fs::rename(&retired, modules.join(".pi-famulus-linux-x64-random-retired")).unwrap();

    let mut client = home.connect();
    client.request_ok(json!({"type":"hello","client_kind":"extension","session_id":"retired-test",
        "pi_pid":std::process::id(),"cwd":"/tmp","protocol":2}));
    let (task, _) = client.start_with(json!({"type":"start","kind":"shell","command":"printf survived",
        "cwd":"/tmp","env":{"PATH":PATH_ENV}}));
    let done = client.request_ok(json!({"type":"wait","task_id":task,"budget_ms":10000}));
    assert_eq!(done["done"], true);
    let output = client.request_ok(json!({"type":"output","task_id":task,"cursor":0,"max_bytes":1024}));
    assert_eq!(output["chunk"], "survived");

    // The daemon's saved install path must escape npm's hidden retired-dir name.
    let out = home.cli(&["upgrade"], Duration::from_secs(40));
    assert!(out.status.success(), "upgrade must use the stable package executable: {} {}", out.stdout, out.stderr);
    drop(daemon);
}
