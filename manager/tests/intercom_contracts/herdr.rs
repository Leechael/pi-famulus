use super::*;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

struct Herdr {
    path: PathBuf,
    requests: mpsc::Receiver<Value>,
    panes: Arc<std::sync::Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Herdr {
    fn new(fixture: &Fixture) -> Self {
        let path = fixture.root.join("herdr.sock");
        let listener = common::bind_private_unix(&path);
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (tx, requests) = mpsc::channel();
        let panes = Arc::new(std::sync::Mutex::new(vec![
            json!({"pane_id":"pane-one","workspace_id":"ws-one","cwd":"/fixture/project","label":"Live Pi","agent":"pi","agent_status":"busy","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-pane"}}),
            json!({"pane_id":"pane-new","workspace_id":"ws-one","cwd":"/fixture/new","label":"Created Pi","agent":"pi","agent_status":"idle","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-created"}}),
        ]));
        let listed_panes = panes.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
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
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut line = String::new();
                if BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap_or(0)
                    == 0
                {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let _ = tx.send(request.clone());
                let result = match request["method"].as_str().unwrap_or_default() {
                    "ping" => json!({"type":"pong","version":"fixture","protocol":5}),
                    "workspace.list" => {
                        json!({"type":"workspace_list","workspaces":[{"workspace_id":"ws-one","label":"Project"}]})
                    }
                    "pane.list" => {
                        json!({"type":"pane_list","panes":*listed_panes.lock().unwrap()})
                    }
                    "workspace.create" => {
                        json!({"type":"workspace_created","workspace":{"workspace_id":"ws-new","label":"New workspace"},"root_pane":{"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"}})
                    }
                    "tab.create" => {
                        json!({"type":"tab_created","root_pane":{"pane_id":"pane-new","workspace_id":"ws-one","cwd":"/fixture/new"}})
                    }
                    "agent.read" => json!({"type":"pane_read","read":{"text":"pane output\n世界"}}),
                    "pane.process_info" => {
                        json!({"type":"process_info","process_info":{"pane_id":"pane-one","foreground_processes":[]}})
                    }
                    "workspace.close" | "pane.close" | "agent.start" | "agent.prompt"
                    | "agent.send_keys" | "agent.wait" => json!({"type":"ok"}),
                    other => {
                        let response = json!({"id":request["id"],"error":{"code":"not_found","message":format!("unexpected method {other}")}});
                        let _ = stream.write_all(format!("{response}\n").as_bytes());
                        continue;
                    }
                };
                let response = json!({"id":request["id"],"result":result});
                let _ = stream.write_all(format!("{response}\n").as_bytes());
            }
        });
        Self {
            path,
            requests,
            panes,
            stop,
            worker: Some(worker),
        }
    }

    fn set_panes(&self, panes: Vec<Value>) {
        *self.panes.lock().unwrap() = panes;
    }

    fn request(&self, method: &str) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let request = self
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("Herdr method before deadline");
            if request["method"] == method {
                return request;
            }
        }
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn pane_plugin(fixture: &Fixture, id: &str, pane: &str) -> Peer {
    let mut plugin = fixture.connect();
    plugin.ok(json!({"op":"register","session":{"id":id,"name":id,"cwd":"/fixture/project","status":"busy","pid":std::process::id(),"herdr_pane_id":pane,"caps":["read_receipts","idle_state_v1"]}}));
    plugin
}

#[test]
fn herdr_overlay_marks_stale_and_missing_pi_offline_without_replacing_provider_status() {
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    herdr.set_panes(vec![
        json!({"pane_id":"w1:p1","workspace_id":"w1","agent":"pi","agent_status":"working","agent_session":{"agent":"pi","kind":"id","value":"new"}}),
        json!({"pane_id":"w1:p2","workspace_id":"w1","agent":"pi","agent_status":"working","agent_session":{"agent":"pi","kind":"path","value":"/tmp/2026-10-21T00-00-00Z_current.jsonl"}}),
        json!({"pane_id":"w1:p3","workspace_id":"w1","agent":"pi","agent_status":"working"}),
    ]);
    let path = herdr.path.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("HERDR_SOCKET_PATH", &path)]);
    let _old = pane_plugin(&fixture, "old", "w1:p1");
    let _gone = pane_plugin(&fixture, "gone", "w1:p-gone");
    let _current = pane_plugin(&fixture, "current", "w1:p2");
    let _unidentified = pane_plugin(&fixture, "unidentified", "w1:p3");
    let _terminal = fixture.plugin("terminal", "idle");
    let rows = fixture.sessions();
    let row = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(
        row("pi/old")["status"],
        "offline",
        "stale registration: {rows:?}"
    );
    assert_eq!(row("pi/gone")["status"], "offline");
    assert_eq!(row("pi/current")["status"], "busy");
    assert_eq!(row("pi/current")["herdr_status"], "working");
    assert_eq!(row("pi/current")["herdr_workspace_id"], "w1");
    assert_eq!(row("pi/unidentified")["status"], "busy");
    assert_eq!(row("pi/terminal")["status"], "idle");

    let _new = pane_plugin(&fixture, "new", "w1:p1");
    let rows = fixture.sessions();
    assert_eq!(
        rows.iter().find(|r| r["id"] == "pi/old").unwrap()["status"],
        "offline"
    );
    let current = rows.iter().find(|r| r["id"] == "pi/new").unwrap();
    assert_eq!(current["status"], "busy");
    assert_eq!(current["herdr_status"], "working");

    herdr.set_panes(vec![]);
    assert_eq!(
        fixture
            .sessions()
            .iter()
            .find(|r| r["id"] == "pi/new")
            .unwrap()["status"],
        "offline"
    );
}

#[test]
fn herdr_overlay_lists_unbound_pi_and_identified_offline_codex_with_machine_identity() {
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    let thread = "01a0a6c0-8869-7932-a922-c67a0a8a56a1";
    let native = super::native::CodexPeer::owned_catalog(&fixture);
    let native_path = native.path.to_str().unwrap().to_string();
    herdr.set_panes(vec![
        json!({"pane_id":"w1:p1","workspace_id":"w1","label":"Unregistered Pi","cwd":"/fixture/pi","agent":"pi","agent_status":"working","agent_session":{"agent":"pi","kind":"id","value":"unregistered"}}),
        json!({"pane_id":"w1:p2","workspace_id":"w1","foreground_cwd":"/fixture/codex","agent":"codex","agent_status":"working","agent_session":{"agent":"codex","kind":"path","value":format!("/tmp/rollout-2026-09-16T04-27-10-{thread}.jsonl")}}),
        json!({"pane_id":"w1:p3","workspace_id":"w1","agent":"codex","agent_status":"idle","agent_session":{"agent":"codex","kind":"id","value":thread}}),
        json!({"pane_id":"w1:p4","workspace_id":"w1","agent":"codex","agent_status":"idle"}),
        json!({"pane_id":"w1:p5","workspace_id":"w1","cwd":"/fixture/shell"}),
        json!({"pane_id":"w1:p6","workspace_id":"w1","agent":"codex","agent_status":"working","agent_session":{"agent":"codex","kind":"id","value":"desktop-A"}}),
        json!({"pane_id":"w1:p7","workspace_id":"w1","agent":"codex","agent_status":"working","agent_session":{"agent":"codex","kind":"id","value":"desktop-A"}}),
    ]);
    let path = herdr.path.to_str().unwrap().to_string();
    fixture.start_with(
        &[],
        &[
            ("HERDR_SOCKET_PATH", &path),
            ("CODEX_APP_SERVER_SOCK", &native_path),
        ],
    );
    // The herdr overlay publishes a provisional codex/desktop-A row (no
    // name) before the native provider finishes its first sync; wait for
    // the enriched native row so the listing below observes discovery.
    fixture.wait_session_field("codex/desktop-A", "name", "Codex fixture");
    let _plugin = fixture.plugin("local", "idle");
    let mut cli = fixture.connect();
    let listed = cli.ok(json!({"op":"list","all":true}));
    let rows = listed["sessions"].as_array().unwrap();
    let pi = rows
        .iter()
        .find(|r| r["id"] == "w1:p1")
        .expect("unbound Pi pane in list all");
    assert_eq!(pi["name"], "Unregistered Pi");
    assert_eq!(pi["harness"], "pi");
    assert_eq!(pi["cwd"], "/fixture/pi");
    assert_eq!(pi["herdr_pane_id"], "w1:p1");
    assert_eq!(pi["herdr_workspace_id"], "w1");
    assert_eq!(pi["herdr_status"], "working");
    assert_eq!(pi["all"], true);
    assert_eq!(pi["machine_name"], "fixture-host");
    assert!(pi["machine_id"].as_str().is_some_and(|s| !s.is_empty()));
    let id = format!("codex/{thread}");
    let codex = rows.iter().filter(|r| r["id"] == id).collect::<Vec<_>>();
    assert_eq!(
        codex.len(),
        1,
        "same thread in two panes must not duplicate: {rows:?}"
    );
    assert_eq!(codex[0]["status"], "offline");
    assert_eq!(codex[0]["harness"], "codex");
    assert_eq!(codex[0]["cwd"], "/fixture/codex");
    assert_eq!(codex[0]["herdr_pane_id"], "w1:p2");
    assert_eq!(codex[0]["herdr_status"], "working");
    assert_eq!(codex[0]["machine_id"], pi["machine_id"]);
    assert_eq!(codex[0]["machine_name"], pi["machine_name"]);
    let loaded = rows
        .iter()
        .filter(|r| r["id"] == "codex/desktop-A")
        .collect::<Vec<_>>();
    assert_eq!(
        loaded.len(),
        1,
        "loaded thread must not duplicate across panes"
    );
    assert_eq!(loaded[0]["name"], "Codex fixture");
    assert_eq!(
        loaded[0]["status"], "idle",
        "native provider status must win"
    );
    assert_eq!(loaded[0]["herdr_status"], "working");
    assert_eq!(
        rows.len(),
        4,
        "unidentified Codex and shell panes are not rows: {rows:?}"
    );
    let normal = fixture.sessions();
    assert!(!normal.iter().any(|r| r["id"] == "w1:p1"));
    assert!(normal
        .iter()
        .any(|r| r["id"] == id && r["status"] == "offline"));
    cli.ok(json!({"op":"agent_prompt","to":id,"body":"offline thread pane prompt"}));
    assert_eq!(herdr.request("agent.prompt")["params"]["target"], "w1:p2");
}

#[test]
fn herdr_workspace_list_create_and_close_map_protocol_fields_without_ui() {
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    let path = herdr.path.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("HERDR_SOCKET_PATH", &path)]);
    let mut cli = fixture.connect();
    let list = cli.ok(json!({"op":"workspace_list"}));
    assert!(
        list["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == "ws-one"
                && w["name"] == "Project"
                && w["cwd"] == "/fixture/project"),
        "workspace projection lost fields: {list}"
    );
    let created =
        cli.ok(json!({"op":"workspace_create","name":"New workspace","cwd":"/fixture/new"}));
    assert_eq!(created["pane_id"], "pane-new");
    assert!(created["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["id"] == "ws-new"));
    let request = herdr.request("workspace.create");
    assert_eq!(request["params"]["label"], "New workspace");
    assert_eq!(request["params"]["cwd"], "/fixture/new");
    cli.ok(json!({"op":"workspace_close","to":"ws-new"}));
    assert_eq!(
        herdr.request("workspace.close")["params"]["workspace_id"],
        "ws-new"
    );
}

#[test]
fn herdr_session_create_prompt_keys_read_wait_and_close_use_native_methods() {
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    let path = herdr.path.to_str().unwrap().to_string();
    fixture.start_with(&[], &[("HERDR_SOCKET_PATH", &path)]);
    let _pane = pane_plugin(&fixture, "pi-pane", "pane-one");
    let _created = pane_plugin(&fixture, "pi-created", "pane-new");
    let mut cli = fixture.connect();
    let created=cli.ok(json!({"op":"session_create","name":"Created Pi","cwd":"/fixture/new","agent":"pi","workspace":"ws-one","args":["--offline"]}));
    assert_eq!(created["pane_id"], "pane-new");
    assert_eq!(created["session_id"], "pi/pi-created");
    let started = herdr.request("agent.start");
    assert_eq!(started["params"]["kind"], "pi");
    assert_eq!(started["params"]["args"], json!(["--offline"]));
    cli.ok(json!({"op":"agent_prompt","to":"pi/pi-pane","body":"pane prompt","wait":true,"until":"idle","timeout_ms":1234}));
    let prompt = herdr.request("agent.prompt");
    assert_eq!(prompt["params"]["target"], "pane-one");
    assert_eq!(prompt["params"]["text"], "pane prompt");
    assert_eq!(prompt["params"]["wait"]["until"], json!(["idle"]));
    assert_eq!(prompt["params"]["wait"]["timeout_ms"], 1234);
    cli.ok(json!({"op":"agent_keys","to":"pi/pi-pane","keys":["Ctrl+C","Enter"]}));
    assert_eq!(
        herdr.request("agent.send_keys")["params"]["keys"],
        json!(["Ctrl+C", "Enter"])
    );
    let read = cli.ok(json!({"op":"agent_read","to":"pi/pi-pane","source":"visible","lines":12}));
    assert_eq!(read["text"], "pane output\n世界");
    let request = herdr.request("agent.read");
    assert_eq!(request["params"]["target"], "pane-one");
    assert_eq!(request["params"]["source"], "visible");
    assert_eq!(request["params"]["lines"], 12);
    assert_eq!(request["params"]["strip_ansi"], true);
    cli.ok(json!({"op":"agent_wait","to":"pi/pi-pane","until":"blocked","timeout_ms":2345}));
    let wait = herdr.request("agent.wait");
    assert_eq!(wait["params"]["until"], json!(["blocked"]));
    assert_eq!(wait["params"]["timeout_ms"], 2345);
    cli.ok(json!({"op":"session_close","to":"pi/pi-pane"}));
    assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-one");
}

#[test]
fn relay_peer_drives_only_local_agent_panes_and_starts_agents_without_permission_bypass() {
    use super::network::{relay_basics, start_relay_options_env, WireRelay};
    let relay = WireRelay::new(relay_basics);
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    herdr.set_panes(vec![
        json!({"pane_id":"pane-one","workspace_id":"ws-one","cwd":"/fixture/project","label":"Live Pi","agent":"pi","agent_status":"busy","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-pane"}}),
        json!({"pane_id":"ws-one:p9","workspace_id":"ws-one","cwd":"/fixture/shell"}),
        json!({"pane_id":"pane-two","workspace_id":"ws-agents","cwd":"/fixture/agents","label":"Agent only","agent":"pi","agent_status":"idle","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-agents"}}),
    ]);
    let path = herdr.path.to_str().unwrap().to_string();
    start_relay_options_env(
        &mut fixture,
        &relay,
        "",
        &[],
        &[("HERDR_SOCKET_PATH", &path)],
    );
    relay.request("register");
    fixture.wait_session("pi/remote");
    let _pane = pane_plugin(&fixture, "pi-pane", "pane-one");
    let _created = pane_plugin(&fixture, "pi-created", "pane-new");
    let call = |frame: Value| {
        let id = uuid::Uuid::new_v4().to_string();
        let mut frame = frame;
        frame["id"] = json!(id);
        relay.push(frame);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let reply = relay
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("relay reply");
            if reply["id"] == id {
                return reply;
            }
        }
    };
    let shell = call(json!({"type":"agent_keys","session_id":"ws-one:p9","keys":["c","Enter"]}));
    assert_eq!(
        shell["type"], "error",
        "relay typed into a shell pane: {shell}"
    );
    let agent = call(json!({"type":"agent_keys","session_id":"pi/pi-pane","keys":["Enter"]}));
    assert_ne!(agent["type"], "error", "{agent}");
    assert_eq!(
        herdr.request("agent.send_keys")["params"]["target"],
        "pane-one",
        "the refused shell-pane keys reached Herdr"
    );
    let cwd = fixture.root.to_str().unwrap().to_string();
    for (args, cwd) in [
        (json!(["--dangerously-skip-permissions"]), cwd.as_str()),
        (
            json!(["--permission-mode", "bypassPermissions"]),
            cwd.as_str(),
        ),
        (json!(["--permission-mode=bypassPermissions"]), cwd.as_str()),
        (
            json!(["--model", "--dangerously-skip-permissions"]),
            cwd.as_str(),
        ),
        (json!("--offline"), cwd.as_str()),
        (json!(["--offline"]), "/fixture/does-not-exist"),
        (json!(["--offline"]), "relative/dir"),
    ] {
        let reply = call(
            json!({"type":"session_create","name":"Created Pi","cwd":cwd,"agent":"claude","workspace_id":"ws-one","origin_machine":"other-device","args":args}),
        );
        assert_eq!(reply["type"], "error", "{args} in {cwd}: {reply}");
    }
    let anonymous = call(
        json!({"type":"session_create","name":"Created Pi","cwd":cwd,"agent":"pi","args":["--offline"]}),
    );
    assert_eq!(
        anonymous["type"], "error",
        "relay created a workspace without origin provenance: {anonymous}"
    );
    let foreign = call(
        json!({"type":"session_create","name":"Created Pi","cwd":cwd,"agent":"pi","workspace_id":"ws-one","origin_machine":"other-device","args":["--offline"]}),
    );
    assert_eq!(
        foreign["type"], "error",
        "relay added a tab to a workspace it did not create: {foreign}"
    );
    let created = call(
        json!({"type":"session_create","name":"Created Pi","cwd":cwd,"agent":"pi","origin_machine":"other-device","args":["--offline","--model","sonnet"]}),
    );
    assert_ne!(created["type"], "error", "{created}");
    assert_eq!(
        herdr.request("workspace.create")["params"]["cwd"],
        json!(cwd)
    );
    let started = herdr.request("agent.start");
    assert_eq!(
        started["params"]["args"],
        json!(["--offline", "--model", "sonnet"]),
        "a refused session_create reached agent.start"
    );
    for foreign in ["ws-one", "ws-agents"] {
        let closed = call(
            json!({"type":"workspace_close","workspace_id":foreign,"origin_machine":"other-device"}),
        );
        assert_eq!(
            closed["type"], "error",
            "relay closed {foreign}, which it did not create: {closed}"
        );
    }
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new","agent":"pi","agent_status":"idle","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-created"}}),
        json!({"pane_id":"ws-new:p2","workspace_id":"ws-new","cwd":"/fixture/new"}),
    ]);
    let shell = call(
        json!({"type":"workspace_close","workspace_id":"ws-new","origin_machine":"other-device"}),
    );
    assert_eq!(
        shell["type"], "error",
        "relay closed its workspace while the user had a shell in it: {shell}"
    );
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new","agent":"pi","agent_status":"idle","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-created"}}),
    ]);
    let own = call(
        json!({"type":"workspace_close","workspace_id":"ws-new","origin_machine":"other-device"}),
    );
    assert_ne!(own["type"], "error", "{own}");
    assert_eq!(
        herdr.request("workspace.close")["params"]["workspace_id"],
        "ws-new",
        "a refused workspace close reached Herdr"
    );
}

#[test]
fn relay_pane_ops_revalidate_pane_ownership_after_reassignment() {
    use super::network::{relay_basics, start_relay_options_env, WireRelay};
    let relay = WireRelay::new(relay_basics);
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    herdr.set_panes(vec![json!({
        "pane_id":"pane-one","workspace_id":"ws-one","cwd":"/fixture/project","label":"Live Pi",
        "agent":"pi","agent_status":"busy",
        "agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-pane"}
    })]);
    let path = herdr.path.to_str().unwrap().to_string();
    start_relay_options_env(
        &mut fixture,
        &relay,
        "",
        &[],
        &[("HERDR_SOCKET_PATH", &path)],
    );
    relay.request("register");
    fixture.wait_session("pi/remote");
    let _pane = pane_plugin(&fixture, "pi-pane", "pane-one");
    let call = |frame: Value| {
        let id = uuid::Uuid::new_v4().to_string();
        let mut frame = frame;
        frame["id"] = json!(id);
        relay.push(frame);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let reply = relay
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("relay reply");
            if reply["id"] == id {
                return reply;
            }
        }
    };
    // Assert no request with the given Herdr method arrives within the window.
    let assert_no_herdr_method = |method: &str| {
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Ok(request) = herdr
            .requests
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            assert_ne!(
                request["method"], method,
                "a refused relay operation reached Herdr as {method}: {request}"
            );
        }
    };
    let keys = call(json!({"type":"agent_keys","session_id":"pi/pi-pane","keys":["Enter"]}));
    assert_ne!(keys["type"], "error", "{keys}");
    assert_eq!(
        herdr.request("agent.send_keys")["params"]["target"],
        "pane-one"
    );

    // The user reassigns the pane to a shell; the stale directory row must
    // not keep accepting relay input.
    herdr.set_panes(vec![json!({
        "pane_id":"pane-one","workspace_id":"ws-one","cwd":"/fixture/shell"
    })]);
    let shell = call(json!({"type":"agent_keys","session_id":"pi/pi-pane","keys":["c"]}));
    assert_eq!(
        shell["type"], "error",
        "relay typed into a reassigned shell pane: {shell}"
    );
    assert_no_herdr_method("agent.send_keys");

    // The pane now hosts a different agent session; the old session's relay
    // operations must be refused too.
    herdr.set_panes(vec![json!({
        "pane_id":"pane-one","workspace_id":"ws-one","agent":"pi","agent_status":"busy",
        "agent_session":{"agent":"pi","kind":"session_id","value":"other-agent"}
    })]);
    let stolen = call(json!({"type":"agent_read","session_id":"pi/pi-pane"}));
    assert_eq!(
        stolen["type"], "error",
        "relay read a pane reassigned to another agent: {stolen}"
    );
    assert_no_herdr_method("agent.read");
    let prompt = call(json!({"type":"agent_prompt","session_id":"pi/pi-pane","body":"hi"}));
    assert_eq!(prompt["type"], "error", "{prompt}");
    assert_no_herdr_method("agent.prompt");
    let closed = call(json!({"type":"session_close","session_id":"pi/pi-pane"}));
    assert_eq!(
        closed["type"], "error",
        "relay closed a pane it no longer owns: {closed}"
    );
    assert_no_herdr_method("pane.close");
}

#[test]
fn relay_workspace_ownership_is_scoped_to_the_creating_origin() {
    use super::network::{relay_basics, start_relay_options_env, WireRelay};
    let relay = WireRelay::new(relay_basics);
    let mut fixture = Fixture::new();
    let herdr = Herdr::new(&fixture);
    let path = herdr.path.to_str().unwrap().to_string();
    start_relay_options_env(
        &mut fixture,
        &relay,
        "",
        &[],
        &[("HERDR_SOCKET_PATH", &path)],
    );
    relay.request("register");
    fixture.wait_session("pi/remote");
    let call = |frame: Value| {
        let id = uuid::Uuid::new_v4().to_string();
        let mut frame = frame;
        frame["id"] = json!(id);
        relay.push(frame);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let reply = relay
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("relay reply");
            if reply["id"] == id {
                return reply;
            }
        }
    };
    let assert_no_herdr_method = |method: &str| {
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Ok(request) = herdr
            .requests
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            assert_ne!(
                request["method"], method,
                "a refused relay operation reached Herdr as {method}: {request}"
            );
        }
    };
    let cwd = fixture.root.to_str().unwrap().to_string();
    // No provenance, no workspace: refused before Herdr is asked to create.
    let anonymous = call(json!({"type":"workspace_create","name":"Anon","cwd":cwd}));
    assert_eq!(anonymous["type"], "error", "{anonymous}");
    assert_no_herdr_method("workspace.create");
    // machine-A creates the workspace.
    let created = call(
        json!({"type":"workspace_create","name":"Project","cwd":cwd,"origin_machine":"machine-A"}),
    );
    assert_ne!(created["type"], "error", "{created}");
    assert_eq!(
        herdr.request("workspace.create")["params"]["cwd"],
        json!(cwd)
    );
    // machine-B cannot extend machine-A's workspace...
    let foreign = call(json!({
        "type":"session_create","name":"Intruder","cwd":cwd,"agent":"pi",
        "workspace_id":"ws-new","origin_machine":"machine-B"
    }));
    assert_eq!(
        foreign["type"], "error",
        "another origin extended machine-A's workspace: {foreign}"
    );
    assert_no_herdr_method("tab.create");
    // ...and machine-A can.
    let own = call(json!({
        "type":"session_create","name":"Member","cwd":cwd,"agent":"pi",
        "workspace_id":"ws-new","origin_machine":"machine-A"
    }));
    assert_ne!(own["type"], "error", "{own}");
    assert_eq!(
        herdr.request("tab.create")["params"]["workspace_id"],
        "ws-new"
    );
    // machine-B cannot close it either; a frame without provenance is refused
    // as well; only machine-A can close it.
    let foreign_close = call(json!({
        "type":"workspace_close","workspace_id":"ws-new","origin_machine":"machine-B"
    }));
    assert_eq!(
        foreign_close["type"], "error",
        "another origin closed machine-A's workspace: {foreign_close}"
    );
    assert_no_herdr_method("workspace.close");
    let anonymous_close = call(json!({"type":"workspace_close","workspace_id":"ws-new"}));
    assert_eq!(anonymous_close["type"], "error", "{anonymous_close}");
    herdr.set_panes(vec![json!({
        "pane_id":"pane-new","workspace_id":"ws-new","cwd":cwd,"agent":"pi","agent_status":"idle",
        "agent_session":{"agent":"pi","kind":"session_id","value":"pi-created"}
    })]);
    let own_close = call(json!({
        "type":"workspace_close","workspace_id":"ws-new","origin_machine":"machine-A"
    }));
    assert_ne!(own_close["type"], "error", "{own_close}");
    assert_eq!(
        herdr.request("workspace.close")["params"]["workspace_id"],
        "ws-new"
    );
}

/// A two-peer relay: registers each connection's machine id and routes
/// machine-addressed operation frames to that peer's connection, then routes
/// the response back to the requester. This drives the real daemon-to-daemon
/// forwarding path (no hand-crafted relay frames).
struct RoutingRelay {
    address: SocketAddr,
    requests: mpsc::Receiver<Value>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RoutingRelay {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (tx, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let registry = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
                String,
                mpsc::Sender<Value>,
            >::new()));
            let reply_routes = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
                Value,
                mpsc::Sender<Value>,
            >::new()));
            while !stopped.load(Ordering::Relaxed) {
                let (stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                let tx = tx.clone();
                let stopped = stopped.clone();
                let registry = registry.clone();
                let reply_routes = reply_routes.clone();
                thread::spawn(move || {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_nodelay(true).unwrap();
                    // Poll briefly only after the upgrade: a short timeout
                    // during it interrupts a request that arrives late.
                    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let Ok(mut ws) = tokio_tungstenite::tungstenite::accept(stream) else {
                        return;
                    };
                    ws.get_mut()
                        .set_read_timeout(Some(Duration::from_millis(2)))
                        .unwrap();
                    let (outgoing, outbound) = mpsc::channel::<Value>();
                    let mut own_machine = String::new();
                    while !stopped.load(Ordering::Relaxed) {
                        // Forward queued outbound frames to this peer.
                        while let Ok(frame) = outbound.try_recv() {
                            if ws
                                .send(tokio_tungstenite::tungstenite::Message::text(
                                    frame.to_string(),
                                ))
                                .is_err()
                            {
                                return;
                            }
                        }
                        let frame = match ws.read() {
                            Ok(frame) => frame,
                            Err(tokio_tungstenite::tungstenite::Error::Io(error))
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue
                            }
                            Err(_) => break,
                        };
                        if !frame.is_text() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(frame.to_text().unwrap())
                        else {
                            continue;
                        };
                        let _ = tx.send(value.clone());
                        let kind = value["type"].as_str().unwrap_or_default();
                        if kind == "register" {
                            own_machine = value["machine_id"].as_str().unwrap_or("").to_string();
                            registry
                                .lock()
                                .unwrap()
                                .insert(own_machine.clone(), outgoing.clone());
                            let welcome = json!({"type":"welcome","machine_id":value["device_id"],"send_message_id_v1":true});
                            let _ = ws.send(tokio_tungstenite::tungstenite::Message::text(
                                welcome.to_string(),
                            ));
                            continue;
                        }
                        if kind == "snapshot" {
                            continue;
                        }
                        if kind == "list" {
                            let reply = json!({"type":"list","id":value["id"],"sessions":[]});
                            let _ = ws.send(tokio_tungstenite::tungstenite::Message::text(
                                reply.to_string(),
                            ));
                            continue;
                        }
                        // An operation addressed to a machine routes to that
                        // peer; anything else with a tracked id is the reply.
                        let target = value["machine_id"].as_str().unwrap_or("");
                        let routed = if !target.is_empty() && target != own_machine {
                            let sender = registry.lock().unwrap().get(target).cloned();
                            if let Some(sender) = sender {
                                let _ = sender.send(value.clone());
                                reply_routes
                                    .lock()
                                    .unwrap()
                                    .insert(value["id"].clone(), outgoing.clone());
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if !routed {
                            if let Some(sender) = reply_routes.lock().unwrap().remove(&value["id"])
                            {
                                let _ = sender.send(value);
                            }
                        }
                    }
                });
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
    /// Wait until both peers registered; returns both machine ids in arrival
    /// order.
    fn wait_machines(&self) -> Vec<String> {
        let mut machines = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        while machines.len() < 2 {
            let frame = self
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("both peers register");
            if frame["type"] == "register" {
                machines.push(frame["machine_id"].as_str().unwrap().to_string());
            }
        }
        machines
    }
}

impl Drop for RoutingRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Drives a remote workspace create/extend/close through the real
/// daemon-to-daemon forwarding path. The initiating daemon stamps
/// `origin_machine` on the forwarded frame; without that stamp the target
/// refuses the operation for lacking provenance.
#[test]
fn cross_machine_workspace_ops_forward_end_to_end_with_origin_provenance() {
    let relay = RoutingRelay::new();
    let mut fixture_a = Fixture::new();
    let mut fixture_b = Fixture::new();
    let herdr_b = Herdr::new(&fixture_b);
    // Fixture B is the target: distinct machine name, relay instead of
    // local-only, and the Herdr fixture socket its daemon talks to.
    std::fs::write(
        fixture_b.root.join("client.toml"),
        format!(
            "local_only = false\nname = 'fixture-b'\nsock = '{}'\nstate_dir = '{}'\nmcp_listen = ''\nserver = '{}'\ntoken = 'fixture-token'\n",
            fixture_b.sock.display(),
            fixture_b.root.join("state").display(),
            relay.url()
        ),
    )
    .unwrap();
    let herdr_b_path = herdr_b.path.to_str().unwrap().to_string();
    fixture_b.spawn_serve(&[], &[("HERDR_SOCKET_PATH", &herdr_b_path)]);
    fixture_b.wait_supervised();
    // Fixture A is the initiator on the same relay.
    let config = std::fs::read_to_string(fixture_a.root.join("client.toml"))
        .unwrap()
        .replace("local_only = true", "local_only = false");
    std::fs::write(
        fixture_a.root.join("client.toml"),
        format!(
            "{config}\nserver='{}'\ntoken='fixture-token'\n",
            relay.url()
        ),
    )
    .unwrap();
    fixture_a.spawn_serve(&[], &[]);
    fixture_a.wait_supervised();
    let machines = relay.wait_machines();
    // The relay records a register frame before the peer has processed the
    // reply that makes its own relay link usable; requests sent in that gap
    // fail with "not connected to relay". Wait for each daemon to report it.
    for fixture in [&fixture_a, &fixture_b] {
        let mut peer = fixture.connect();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let status = peer.ok(json!({"op":"status"}));
            if status["report"]["relay"] == "connected" {
                break;
            }
            assert!(Instant::now() < deadline, "relay never connected: {status}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Fixture A keeps the default 'fixture-host' name; fixture B is the peer.
    let machine_b = machines
        .iter()
        .find(|machine| !machine.starts_with("fixture-host@"))
        .cloned()
        .expect("fixture-b's machine id");

    let cwd = fixture_b.root.to_str().unwrap().to_string();
    let mut cli = fixture_a.connect();
    // workspace_create crosses A -> relay -> B with origin_machine stamped by A.
    let created = cli.ok(json!({
        "op":"workspace_create","name":"Remote project","cwd":cwd,"machine":machine_b
    }));
    assert!(
        created.get("error").is_none(),
        "remote workspace_create refused: {created}"
    );
    assert_eq!(created["pane_id"], "pane-new");
    assert!(created["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["id"] == "ws-new" && w["cwd"] == cwd));
    assert_eq!(
        herdr_b.request("workspace.create")["params"]["cwd"],
        json!(cwd)
    );
    // session_create into that workspace extends it under the same origin.
    let member = cli.ok(json!({
        "op":"session_create","name":"Member","cwd":cwd,"agent":"pi",
        "workspace":"ws-new","args":["--offline"],"machine":machine_b
    }));
    assert!(
        member.get("error").is_none(),
        "remote session_create refused: {member}"
    );
    assert_eq!(
        herdr_b.request("tab.create")["params"]["workspace_id"],
        "ws-new"
    );
    // workspace_close tears it down under the same origin.
    herdr_b.set_panes(vec![json!({
        "pane_id":"pane-new","workspace_id":"ws-new","cwd":cwd,"agent":"pi","agent_status":"idle",
        "agent_session":{"agent":"pi","kind":"session_id","value":"pi-created"}
    })]);
    let closed = cli.ok(json!({"op":"workspace_close","to":"ws-new","machine":machine_b}));
    assert!(
        closed.get("error").is_none(),
        "remote workspace_close refused: {closed}"
    );
    assert_eq!(
        herdr_b.request("workspace.close")["params"]["workspace_id"],
        "ws-new"
    );
}
