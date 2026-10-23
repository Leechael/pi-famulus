use super::*;
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

/// A self-managed manager as pi spawns it: no lease, only its own env.
fn spawn_self_managed(fixture: &mut Fixture, env: &[(&str, &str)]) {
    let log = fs::File::create(fixture.root.join("daemon.log")).unwrap();
    let child = fixture
        .base_command()
        .arg("daemon")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    fixture.child = Some(child);
}

/// A pi session's connection to the manager; dropping it disconnects.
fn pi_session(fixture: &Fixture, session: &str) -> common::Conn {
    let deadline = Instant::now() + TIMEOUT;
    let stream = loop {
        if let Ok(stream) = UnixStream::connect(fixture.root.join("manager/manager.sock")) {
            break stream;
        }
        assert!(Instant::now() < deadline, "manager never listened: {}", fixture.manager_log());
        thread::sleep(Duration::from_millis(10));
    };
    let mut conn = common::Conn::new(stream);
    conn.hello_ext(session);
    conn
}

fn wait_for(what: &str, fixture: &Fixture, budget: Duration, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + budget;
    while !ready() {
        assert!(Instant::now() < deadline, "{what}: {}", fixture.manager_log());
        thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn mcp_up(address: &str) -> bool {
    TcpStream::connect(address).is_ok()
}

/// A stand-in Codex app-server socket: connections accepted so far, and
/// how many are still open (the adapter has not detached).
struct CodexHost {
    path: String,
    accepted: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
}
fn codex_host(fixture: &Fixture) -> CodexHost {
    use std::io::Read;
    let path = fixture.root.join("codex-app.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (accepted, open) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (seen, live) = (accepted.clone(), open.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            seen.fetch_add(1, Ordering::SeqCst);
            live.fetch_add(1, Ordering::SeqCst);
            let live = live.clone();
            thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
                live.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    CodexHost {
        path: path.to_str().unwrap().to_string(),
        accepted,
        open,
    }
}

#[test]
fn self_managed_manager_bridges_pi_without_mcp_or_codex_and_lives_only_for_pi() {
    let mut fixture = Fixture::new();
    let mcp = network::vacant_address().to_string();
    let codex = codex_host(&fixture);
    spawn_self_managed(
        &mut fixture,
        &[("AGENT_INTERCOM_MCP_LISTEN", &mcp), ("CODEX_APP_SERVER_SOCK", &codex.path)],
    );
    let pi = pi_session(&fixture, "pi-session");
    wait_for("the bridge runs without any lease", &fixture, TIMEOUT, || {
        UnixStream::connect(&fixture.sock).is_ok()
    });
    let pid = fixture.daemon_pid();
    thread::sleep(Duration::from_secs(2));
    assert!(!mcp_up(&mcp), "a self-managed manager served MCP");
    assert_eq!(
        codex.accepted.load(Ordering::SeqCst),
        0,
        "a self-managed manager attached to the Codex app-server"
    );
    drop(pi);
    let mut daemon = fixture.child.take().unwrap();
    assert_eq!(daemon.id(), pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the self-managed manager outlived its last pi session: {}",
            fixture.manager_log()
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        UnixStream::connect(&fixture.sock).is_err(),
        "the bridge outlived its manager"
    );
}

#[test]
fn serve_supervises_until_it_exits_then_the_manager_returns_to_pi_lifetime() {
    let mut fixture = Fixture::new();
    let mcp = network::vacant_address().to_string();
    let codex = codex_host(&fixture);
    let env = [("CODEX_APP_SERVER_SOCK", codex.path.as_str())];
    fixture.start_with(&["--mcp-listen", &mcp], &env);
    let pid = fixture.daemon_pid();
    wait_for("Supervised MCP listens", &fixture, TIMEOUT, || mcp_up(&mcp));
    wait_for("Supervised Codex adapter attaches", &fixture, TIMEOUT, || {
        codex.open.load(Ordering::SeqCst) > 0
    });
    thread::sleep(Duration::from_secs(7));
    assert!(alive(pid), "a supervised manager idled out without pi");
    let pi = pi_session(&fixture, "pi-session");
    let mut serve = fixture.child.take().unwrap();
    unsafe { libc::kill(serve.id() as i32, libc::SIGTERM) };
    serve.wait().unwrap();
    wait_for("MCP stops with the last lease", &fixture, TIMEOUT, || !mcp_up(&mcp));
    wait_for("the Codex adapter detaches with the last lease", &fixture, TIMEOUT, || {
        codex.open.load(Ordering::SeqCst) == 0
    });
    let demoted = codex.accepted.load(Ordering::SeqCst);
    thread::sleep(Duration::from_secs(7));
    assert_eq!(
        codex.accepted.load(Ordering::SeqCst),
        demoted,
        "the Codex adapter kept reconnecting after the last lease ended"
    );
    assert!(alive(pid), "the manager exited while pi still used it");
    assert!(
        UnixStream::connect(&fixture.sock).is_ok(),
        "the bridge stopped with the lease"
    );
    let reattached = codex.accepted.load(Ordering::SeqCst);
    fixture.start_with(&["--mcp-listen", &mcp], &env);
    assert_eq!(fixture.daemon_pid(), pid, "a second lease replaced the manager");
    wait_for("a second lease promotes again", &fixture, TIMEOUT, || {
        mcp_up(&mcp) && codex.accepted.load(Ordering::SeqCst) > reattached
    });
    fixture.release_lease();
    wait_for("the second lease demotes again", &fixture, TIMEOUT, || !mcp_up(&mcp));
    drop(pi);
    wait_for(
        "the manager outlived its last pi after the lease ended",
        &fixture,
        Duration::from_secs(10),
        || !alive(pid),
    );
}

#[test]
fn serve_leases_an_existing_self_managed_manager_and_survives_its_replacement() {
    let mut fixture = Fixture::new();
    let mcp = network::vacant_address().to_string();
    spawn_self_managed(&mut fixture, &[("AGENT_INTERCOM_MCP_LISTEN", &mcp)]);
    let mut pi = pi_session(&fixture, "pi-session");
    let (task, task_pid) = pi.start("sleep 30");
    let first = fixture.daemon_pid();
    let mut daemon = fixture.child.take().unwrap();
    fixture.start_with(&[], &[("AGENT_INTERCOM_MCP_LISTEN", &mcp)]);
    assert_eq!(fixture.daemon_pid(), first, "serve replaced a running manager");
    wait_for("leasing promotes the running manager", &fixture, TIMEOUT, || mcp_up(&mcp));
    let listed = pi.request_ok(json!({"type":"list"}));
    assert!(
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["task_id"] == task && row["status"] == "running"),
        "leasing disturbed a running task: {listed}"
    );
    assert!(alive(task_pid));
    drop(pi);
    let mut manager = common::Conn::new(
        UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap(),
    );
    manager.hello_cli();
    manager.request_ok(json!({"type":"shutdown"}));
    daemon.wait().unwrap();
    wait_for("serve brings a manager back", &fixture, Duration::from_secs(15), || {
        fs::read(fixture.root.join("manager/manager.pid"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|pid| pid["pid"].as_u64())
            .is_some_and(|pid| pid as u32 != first && alive(pid as u32))
            && mcp_up(&mcp)
    });
}

#[test]
fn a_lease_rereads_the_bridge_configuration_a_self_managed_manager_started_with() {
    let mut fixture = Fixture::new();
    spawn_self_managed(&mut fixture, &[]);
    let pi = pi_session(&fixture, "pi-session");
    wait_for("the bridge runs without any lease", &fixture, TIMEOUT, || {
        UnixStream::connect(&fixture.sock).is_ok()
    });
    let machine = |fixture: &Fixture| {
        let output = fixture.run(&["status", "--json"]);
        serde_json::from_slice::<Value>(&output.stdout).unwrap_or_default()["machine"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert!(machine(&fixture).starts_with("fixture-host@"));
    let config = fs::read_to_string(fixture.root.join("client.toml"))
        .unwrap()
        .replace("name = 'fixture-host'", "name = 'enrolled-host'");
    fs::write(fixture.root.join("client.toml"), config).unwrap();
    let mut daemon = fixture.child.take().unwrap();
    fixture.start_with(&[], &[]);
    wait_for("the lease applied the newer configuration", &fixture, TIMEOUT, || {
        machine(&fixture).starts_with("enrolled-host@")
    });
    drop(pi);
    fixture.release_lease();
    let mut manager =
        common::Conn::new(UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap());
    manager.hello_cli();
    manager.request_ok(json!({"type":"shutdown"}));
    daemon.wait().unwrap();
}
