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

/// Counts connections to a stand-in Codex app-server socket.
fn codex_host(fixture: &Fixture) -> (PathBuf, Arc<AtomicUsize>) {
    let path = fixture.root.join("codex-app.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let seen = accepted.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_err() {
                break;
            }
            seen.fetch_add(1, Ordering::SeqCst);
        }
    });
    (path, accepted)
}

#[test]
fn self_managed_manager_bridges_pi_without_mcp_or_codex_and_lives_only_for_pi() {
    let mut fixture = Fixture::new();
    let mcp = network::vacant_address().to_string();
    let (codex, accepted) = codex_host(&fixture);
    let codex = codex.to_str().unwrap().to_string();
    spawn_self_managed(
        &mut fixture,
        &[("AGENT_INTERCOM_MCP_LISTEN", &mcp), ("CODEX_APP_SERVER_SOCK", &codex)],
    );
    let pi = pi_session(&fixture, "pi-session");
    wait_for("the bridge runs without any lease", &fixture, TIMEOUT, || {
        UnixStream::connect(&fixture.sock).is_ok()
    });
    let pid = fixture.daemon_pid();
    thread::sleep(Duration::from_secs(2));
    assert!(!mcp_up(&mcp), "a self-managed manager served MCP");
    assert_eq!(
        accepted.load(Ordering::SeqCst),
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
    let (codex, accepted) = codex_host(&fixture);
    let codex = codex.to_str().unwrap().to_string();
    fixture.start_with(&["--mcp-listen", &mcp], &[("CODEX_APP_SERVER_SOCK", &codex)]);
    let pid = fixture.daemon_pid();
    wait_for("Supervised MCP listens", &fixture, TIMEOUT, || mcp_up(&mcp));
    wait_for("Supervised Codex adapter attaches", &fixture, TIMEOUT, || {
        accepted.load(Ordering::SeqCst) > 0
    });
    thread::sleep(Duration::from_secs(7));
    assert!(alive(pid), "a supervised manager idled out without pi");
    let pi = pi_session(&fixture, "pi-session");
    let mut serve = fixture.child.take().unwrap();
    unsafe { libc::kill(serve.id() as i32, libc::SIGTERM) };
    serve.wait().unwrap();
    wait_for("MCP stops with the last lease", &fixture, TIMEOUT, || !mcp_up(&mcp));
    thread::sleep(Duration::from_secs(7));
    assert!(alive(pid), "the manager exited while pi still used it");
    assert!(
        UnixStream::connect(&fixture.sock).is_ok(),
        "the bridge stopped with the lease"
    );
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
