use super::*;

#[test]
fn non_ui_command_families_have_real_help_without_starting_daemon() {
    let fixture = Fixture::new();
    for command in [
        "status",
        "agents",
        "send",
        "notify-idle",
        "idle-status",
        "enroll",
        "device",
        "listen",
        "inbox",
        "events",
        "message",
        "session",
        "workspace",
        "codex",
        "service",
    ] {
        let output = fixture.run(&[command, "--help"]);
        assert_cli_ok(&output);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(command),
            "wrong help for {command}"
        );
        assert!(
            !fixture.sock.exists(),
            "help for {command} unexpectedly started intercom"
        );
    }
}

#[test]
fn inspection_without_discovery_fails_without_starting_a_separate_service() {
    let fixture = Fixture::new();
    for args in [vec!["events"], vec!["message", "missing-message"]] {
        let output = fixture.run(&args);
        assert!(
            !output.status.success(),
            "unwired inspection unexpectedly fabricated history"
        );
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            error.contains("inspect") || error.contains("discovery"),
            "wrong inspection failure: {error}"
        );
    }
    assert!(!fixture.sock.exists());
}

#[test]
fn empty_codex_hook_is_a_side_effect_free_noop() {
    let fixture = Fixture::new();
    let output = fixture.run(&["codex", "hook"]);
    assert_cli_ok(&output);
    assert!(
        output.stdout.is_empty(),
        "empty hook must not fabricate session context"
    );
    assert!(!fixture.sock.exists());
    assert!(
        !fixture.root.join("state/device.json").exists(),
        "empty hook must not create device identity"
    );
}

#[test]
fn absent_codex_host_never_launches_an_app_server_or_claims_idle_threads() {
    let mut fixture = Fixture::new();
    let bin = fixture.root.join("bin");
    fs::create_dir(&bin).unwrap();
    let codex = bin.join("codex");
    let marker = fixture.root.join("codex-launched");
    fs::write(
        &codex,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'codex fixture\\n'; exit 0; fi\nprintf launched > '{}'\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(codex, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        common::tool_free_path().to_str().unwrap()
    );
    fixture.start_with(&[], &[("PATH", &path)]);
    assert!(!fixture.sessions().iter().any(|s| s["harness"] == "codex"));
    assert!(
        !marker.exists(),
        "discovery must not launch codex/app-server"
    );
    let mut peer = fixture.connect();
    let failed =
        peer.call(json!({"op":"send","to":"codex/missing","body":"must not launch a host"}));
    assert_ne!(failed["ok"], true);
    assert!(!marker.exists(), "send must not launch codex/app-server");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn service_print_keeps_original_macos_only_boundary_without_side_effects() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.root.join("client.toml")).unwrap();
    let output = fixture.run(&["service", "print"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("macOS"));
    assert_eq!(fs::read(fixture.root.join("client.toml")).unwrap(), before);
    assert!(!fixture.sock.exists());
    assert!(!fixture.root.join("state/device.json").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn service_print_preserves_selected_manager_home_and_safe_environment_without_saving_token() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.root.join("client.toml")).unwrap();
    let secret = "print-only-secret<&>";
    let socket = fixture.root.join("existing-codex.sock");
    let output = fixture.run_env(
        &["service", "print", "--save-token", "--token", secret],
        &[
            ("HTTP_PROXY", "http://proxy.invalid:1234"),
            ("CODEX_APP_SERVER_SOCK", socket.to_str().unwrap()),
        ],
    );
    assert_cli_ok(&output);
    let plist = String::from_utf8_lossy(&output.stdout);
    assert!(plist.contains("<string>--home</string>"));
    assert!(plist.contains(&format!(
        "<string>{}</string>",
        fixture.root.join("manager").display()
    )));
    assert!(plist.contains(&fixture.root.join("manager/config.json").display().to_string()));
    assert!(plist.contains("HTTP_PROXY") && plist.contains("http://proxy.invalid:1234"));
    assert!(plist.contains("CODEX_APP_SERVER_SOCK") && plist.contains(socket.to_str().unwrap()));
    assert!(!plist.contains(secret) && !plist.contains("print-only-secret&lt;&amp;"));
    assert_eq!(
        fs::read(fixture.root.join("client.toml")).unwrap(),
        before,
        "print saved a preview token"
    );
    assert!(!fixture
        .root
        .join("Library/LaunchAgents/com.pi-famulus.client.plist")
        .exists());
    assert!(!fixture.sock.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn service_print_refuses_token_in_carried_environment_without_echoing_it() {
    let fixture = Fixture::new();
    let secret = "hidden-token<&>";
    let proxy = format!("http://user:{secret}@proxy.invalid");
    let output = fixture.run_env(
        &["service", "print", "--save-token", "--token", secret],
        &[("HTTP_PROXY", &proxy)],
    );
    assert!(!output.status.success());
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !all.contains(secret) && !all.contains("hidden-token&lt;&amp;"),
        "service rejection disclosed the token"
    );
    assert!(!fixture.sock.exists());
}

fn hook(key: &str, plugin: &str, hash: &str, status: &str) -> Value {
    json!({"key":key,"pluginId":plugin,"currentHash":hash,"trustStatus":status})
}

#[test]
fn codex_setup_trusts_only_matching_untrusted_hashes_and_deduplicates_keys() {
    let fixture = Fixture::new();
    let host = super::native::CodexPeer::hooks(
        &fixture,
        json!([
            hook("missing", "intercom@pi-famulus", "hash-A", "untrusted"),
            hook("missing", "intercom@pi-famulus", "hash-B", "untrusted"),
            hook("trusted", "intercom@pi-famulus", "hash-C", "trusted"),
            hook(
                "future",
                "intercom@pi-famulus",
                "hash-D",
                "unknown-future-state"
            ),
            hook("foreign", "other@plugin", "hash-E", "untrusted"),
            hook("empty", "intercom@pi-famulus", "", "untrusted")
        ]),
    );
    let output = fixture.run_env(
        &["codex", "setup"],
        &[("CODEX_APP_SERVER_SOCK", host.path.to_str().unwrap())],
    );
    assert_cli_ok(&output);
    let request = host.request("config/batchWrite");
    assert_eq!(
        request["params"],
        json!({"edits":[{"keyPath":"hooks.state","value":{"missing":{"trusted_hash":"hash-A"}},"mergeStrategy":"upsert"}],"reloadUserConfig":true})
    );
    assert!(
        !fixture.sock.exists(),
        "setup must attach to the existing host without launching manager"
    );
}

#[test]
fn codex_setup_modified_hook_requires_force_and_only_then_writes_trust() {
    let fixture = Fixture::new();
    let host = super::native::CodexPeer::hooks(
        &fixture,
        json!([hook(
            "modified",
            "intercom@pi-famulus",
            "new-hash",
            "modified"
        )]),
    );
    let environment = [("CODEX_APP_SERVER_SOCK", host.path.to_str().unwrap())];
    let refused = fixture.run_env(&["codex", "setup"], &environment);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--force"));
    host.assert_passive_catalog();
    let accepted = fixture.run_env(&["codex", "setup", "--force"], &environment);
    assert_cli_ok(&accepted);
    let request = host.request("config/batchWrite");
    assert_eq!(
        request["params"]["edits"][0]["value"],
        json!({"modified":{"trusted_hash":"new-hash"}})
    );
}

#[test]
fn daemon_never_seeds_codex_hook_trust_without_explicit_setup() {
    let mut fixture = Fixture::new();
    let host = super::native::CodexPeer::hooks(
        &fixture,
        json!([
            hook(
                "modified",
                "intercom@pi-famulus",
                "new-hash",
                "modified"
            ),
            hook("missing", "intercom@pi-famulus", "hash-A", "untrusted")
        ]),
    );
    fixture.start_with(
        &[],
        &[("CODEX_APP_SERVER_SOCK", host.path.to_str().unwrap())],
    );
    fixture.wait_session("codex/thread-A");
    host.assert_passive_catalog();
    assert!(!fixture.root.join("codex/config.toml").exists());
}

fn status_tool(fixture: &Fixture, name: &str, version: &str) {
    let directory = fixture.root.join("status-bin");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf 'probe\\n' >> '{}'\nprintf '%s\\n' '{}'\n",
            fixture.root.join(format!("{name}-probes")).display(),
            version
        ),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn status_report(fixture: &Fixture, environment: &[(&str, &str)]) -> Value {
    let output = fixture.run_env(&["status", "--json"], environment);
    assert_cli_ok(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn online_status_caches_startup_tool_versions_without_reprobing_changed_binaries() {
    let mut fixture = Fixture::new();
    for name in ["claude", "pi", "herdr", "codex"] {
        status_tool(&fixture, name, &format!("{name}-v1"));
    }
    let path = fixture.root.join("status-bin");
    let environment = [("PATH", path.to_str().unwrap())];
    fixture.start_with(&[], &environment);
    let first = status_report(&fixture, &environment);
    for name in ["claude", "pi", "codex"] {
        assert_eq!(first[name], format!("{name}-v1"));
    }
    for name in ["claude", "pi", "herdr", "codex"] {
        assert_eq!(
            fs::read_to_string(fixture.root.join(format!("{name}-probes")))
                .unwrap()
                .lines()
                .count(),
            1
        );
        status_tool(&fixture, name, &format!("{name}-v2"));
    }
    let second = status_report(&fixture, &environment);
    for name in ["claude", "pi", "codex"] {
        assert_eq!(
            second[name],
            format!("{name}-v1"),
            "online status must use the startup snapshot"
        );
    }
    for name in ["claude", "pi", "herdr", "codex"] {
        assert_eq!(
            fs::read_to_string(fixture.root.join(format!("{name}-probes")))
                .unwrap()
                .lines()
                .count(),
            1,
            "online status re-executed {name}"
        );
    }
}

#[test]
fn online_status_mcp_url_uses_the_bound_ephemeral_listener_and_accepts_real_posts() {
    let mut fixture = Fixture::new();
    let path = fixture.root.join("empty-bin");
    fs::create_dir(&path).unwrap();
    let environment = [("PATH", path.to_str().unwrap())];
    fixture.start_with(&["--mcp-listen", "127.0.0.1:0"], &environment);
    let deadline = Instant::now() + TIMEOUT;
    let report = loop {
        let report = status_report(&fixture, &environment);
        if report["mcp_status"] == "running" {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "MCP listener never became ready: {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(report["mcp_listen"], "127.0.0.1:0");
    let url = reqwest::Url::parse(report["mcp_url"].as_str().unwrap()).unwrap();
    assert_eq!(url.path(), "/mcp");
    assert_ne!(
        url.port_or_known_default(),
        Some(0),
        "status advertised the configured port instead of the bound listener"
    );
    let address = format!(
        "{}:{}",
        url.host_str().unwrap(),
        url.port_or_known_default().unwrap()
    )
    .parse()
    .unwrap();
    let response = super::network::mcp(address, &fixture.mcp_token(), "tools/list", json!({}));
    assert!(response["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "list_sessions"));
}

#[test]
fn offline_status_reprobes_tools_without_creating_daemon_state() {
    let fixture = Fixture::new();
    status_tool(&fixture, "pi", "pi-v1");
    let path = fixture.root.join("status-bin");
    let environment = [("PATH", path.to_str().unwrap())];
    assert_eq!(status_report(&fixture, &environment)["pi"], "pi-v1");
    status_tool(&fixture, "pi", "pi-v2");
    assert_eq!(status_report(&fixture, &environment)["pi"], "pi-v2");
    assert_eq!(
        fs::read_to_string(fixture.root.join("pi-probes"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(!fixture.sock.exists());
    assert!(!fixture.root.join("state").exists());
}

#[test]
fn startup_version_probe_does_not_block_manager_and_is_killed_on_shutdown() {
    let mut fixture = Fixture::new();
    let directory = fixture.root.join("status-bin");
    fs::create_dir(&directory).unwrap();
    let tool = directory.join("claude");
    let marker = fixture.root.join("probe-pid");
    fs::write(
        &tool,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(tool, fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    fixture.start_with(&[], &[("PATH", directory.to_str().unwrap())]);
    assert!(fixture.sessions().is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "startup probe blocked the running manager"
    );
    let deadline = Instant::now() + TIMEOUT;
    let pid: u32 = loop {
        if let Ok(value) = fs::read_to_string(&marker) {
            if let Ok(pid) = value.parse() {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "startup never launched version probe"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert!(common::pid_alive(pid));
    drop(fixture);
    let deadline = Instant::now() + Duration::from_secs(1);
    while common::pid_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "startup child survived daemon shutdown: {pid}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

struct StatusCodex {
    path: PathBuf,
    hooks: std::sync::Arc<std::sync::Mutex<Value>>,
    calls: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    reachable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl StatusCodex {
    fn new(fixture: &Fixture, hooks: Value) -> Self {
        let path = fixture.root.join("status-codex.sock");
        let listener = common::bind_private_unix(&path);
        listener.set_nonblocking(true).unwrap();
        let hooks = std::sync::Arc::new(std::sync::Mutex::new(hooks));
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reachable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (state, history, live, stopping) = (
            hooks.clone(),
            calls.clone(),
            reachable.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let stream = match listener.accept() {
                    // BSD-derived systems inherit O_NONBLOCK from the listener.
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                if !live.load(Ordering::Acquire) {
                    continue;
                }
                let (state, history, stopping) = (state.clone(), history.clone(), stopping.clone());
                clients.push(thread::spawn(move || {
                    // A short read timeout during the upgrade interrupts a
                    // request that arrives late; poll briefly only afterwards.
                    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                    let Ok(mut ws) = tokio_tungstenite::tungstenite::accept(stream) else { return; };
                    ws.get_mut().set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                    while !stopping.load(Ordering::Acquire) {
                        let message = match ws.read() {
                            Ok(message) => message,
                            Err(tokio_tungstenite::tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                            Err(_) => return,
                        };
                        if !message.is_text() { continue; }
                        let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                        history.lock().unwrap().push(request.clone());
                        if request.get("id").is_none() { continue; }
                        let result = match request["method"].as_str().unwrap_or_default() {
                            "initialize" => json!({}),
                            "thread/list" | "thread/loaded/list" => json!({"data":[],"nextCursor":null}),
                            "hooks/list" => json!({"data":[{"hooks":state.lock().unwrap().clone()}],"errors":[]}),
                            method => panic!("status performed unexpected operation: {method}"),
                        };
                        if ws.send(tokio_tungstenite::tungstenite::Message::text(json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string())).is_err() { return; }
                    }
                }));
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            path,
            hooks,
            calls,
            reachable,
            stop,
            worker: Some(worker),
        }
    }
    fn wait_attached(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while !self
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["method"] == "thread/list")
        {
            assert!(
                Instant::now() < deadline,
                "daemon never attached to the existing Codex host"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for StatusCodex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn online_status_uses_live_codex_reachability_and_readonly_hook_trust() {
    let mut fixture = Fixture::new();
    status_tool(&fixture, "codex", "codex-v1");
    let host = StatusCodex::new(
        &fixture,
        json!([hook(
            "pending",
            "intercom@pi-famulus",
            "hash-A",
            "modified"
        )]),
    );
    let path = fixture.root.join("status-bin");
    let environment = [
        ("PATH", path.to_str().unwrap()),
        ("CODEX_APP_SERVER_SOCK", host.path.to_str().unwrap()),
    ];
    fixture.start_with(&[], &environment);
    host.wait_attached();
    let first = status_report(&fixture, &environment);
    assert_eq!(first["codex_app_server"], "external listener");
    assert_eq!(
        first["codex_hooks"],
        "untrusted; run pi-famulus intercom codex setup"
    );
    *host.hooks.lock().unwrap() = json!([
        hook("duplicate", "intercom@pi-famulus", "hash-A", "trusted"),
        hook(
            "duplicate",
            "intercom@pi-famulus",
            "hash-B",
            "untrusted"
        ),
        hook("foreign", "other@plugin", "hash-C", "untrusted"),
        hook("empty", "intercom@pi-famulus", "", "untrusted"),
        hook(
            "future",
            "intercom@pi-famulus",
            "hash-D",
            "unknown-future-state"
        )
    ]);
    host.reachable.store(false, Ordering::Release);
    let second = status_report(&fixture, &environment);
    assert_eq!(second["codex_app_server"], "not attached");
    assert!(second.get("codex_hooks").is_none());
    assert!(!fixture.root.join("codex/config.toml").exists());
    assert!(!host
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|request| request["method"] == "config/batchWrite"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("codex-probes"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn online_status_observes_live_herdr_even_without_a_cached_binary() {
    let mut fixture = Fixture::new();
    let path = fixture.root.join("empty-bin");
    fs::create_dir(&path).unwrap();
    let socket = fixture.root.join("status-herdr.sock");
    let listener = common::bind_private_unix(&socket);
    listener.set_nonblocking(true).unwrap();
    let version = std::sync::Arc::new(AtomicU64::new(1));
    let live_version = version.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = stop.clone();
    let worker = thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            let stream = match listener.accept() {
                // BSD-derived systems inherit O_NONBLOCK from the listener.
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = if request["method"] == "ping" {
                json!({"version":format!("live-herdr-v{}", live_version.load(Ordering::Acquire)),"protocol":7})
            } else {
                json!({"panes":[],"workspaces":[]})
            };
            writeln!(
                reader.get_mut(),
                "{}",
                json!({"id":request["id"],"result":result})
            )
            .unwrap();
        }
    });
    let environment = [
        ("PATH", path.to_str().unwrap()),
        ("HERDR_SOCKET_PATH", socket.to_str().unwrap()),
    ];
    fixture.start_with(&[], &environment);
    let first = status_report(&fixture, &environment);
    version.store(2, Ordering::Release);
    let second = status_report(&fixture, &environment);
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert_eq!(first["herdr"], "live-herdr-v1");
    assert_eq!(first["herdr_protocol"], 7);
    assert_eq!(second["herdr"], "live-herdr-v2");
    assert!(first.get("herdr_path").is_none());
    assert_eq!(first["codex"], "not installed");
    assert!(first.get("codex_app_server").is_none());
    let disconnected = status_report(&fixture, &environment);
    assert_eq!(disconnected["herdr"], "not installed");
    assert!(disconnected.get("herdr_protocol").is_none());
}

#[test]
fn online_status_failed_mcp_bind_keeps_daemon_running_with_an_empty_url() {
    let mut fixture = Fixture::new();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap().to_string();
    let path = fixture.root.join("empty-bin");
    fs::create_dir(&path).unwrap();
    let environment = [("PATH", path.to_str().unwrap())];
    fixture.start_with(&["--mcp-listen", &address], &environment);
    let deadline = Instant::now() + TIMEOUT;
    let report = loop {
        let report = status_report(&fixture, &environment);
        if report["mcp_status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
        {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "failed bind never reported degradation"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(report["daemon"], "running");
    assert_eq!(report["mcp_url"], "");
    assert_eq!(report["mcp_listen"], address);
    assert!(fixture.sessions().is_empty());
}

#[test]
fn online_status_prefers_connected_desktop_even_with_an_empty_catalog() {
    use std::io::Read;
    let mut fixture = Fixture::new();
    status_tool(&fixture, "codex", "codex-v1");
    let host = StatusCodex::new(&fixture, json!([]));
    let directory = fixture.root.join("status-desktop");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.join("ipc.sock");
    let listener = common::bind_private_unix(&socket);
    listener.set_nonblocking(true).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = stop.clone();
    let (ready, initialized) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            let mut stream = match listener.accept() {
                // BSD-derived systems inherit O_NONBLOCK from the listener.
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => return,
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            while !stopping.load(Ordering::Acquire) {
                // Poll only for a frame's first byte: the daemon writes the
                // length and the body separately, and a stall between them
                // must not unframe the stream.
                let mut header = [0; 4];
                match stream.read(&mut header[..1]) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue
                    }
                    Err(_) => break,
                }
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                stream.read_exact(&mut header[1..]).unwrap();
                let mut bytes = vec![0; u32::from_le_bytes(header) as usize];
                stream.read_exact(&mut bytes).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(request["method"], "initialize");
                let response = serde_json::to_vec(&json!({"type":"response","requestId":request["requestId"],"resultType":"success","method":"initialize","handledByClientId":"status-owner","result":{"clientId":"status-follower"}})).unwrap();
                stream
                    .write_all(&(response.len() as u32).to_le_bytes())
                    .unwrap();
                stream.write_all(&response).unwrap();
                ready.send(()).unwrap();
            }
        }
    });
    let path = fixture.root.join("status-bin");
    let environment = [
        ("PATH", path.to_str().unwrap()),
        ("CODEX_APP_SERVER_SOCK", host.path.to_str().unwrap()),
        ("CODEX_DESKTOP_IPC_SOCK", socket.to_str().unwrap()),
    ];
    fixture.start_with(&[], &environment);
    initialized.recv_timeout(TIMEOUT).unwrap();
    host.wait_attached();
    assert!(fixture.sessions().is_empty());
    let deadline = Instant::now() + TIMEOUT;
    let attached = loop {
        let report = status_report(&fixture, &environment);
        if report["codex_app_server"] == "Desktop IPC attached" {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "connected Desktop was not reflected in status: {report}"
        );
        thread::sleep(Duration::from_millis(5));
    };
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert_eq!(attached["codex_app_server"], "Desktop IPC attached");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let report = status_report(&fixture, &environment);
        if report["codex_app_server"] == "external listener" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "disconnected Desktop retained an attached status: {report}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
