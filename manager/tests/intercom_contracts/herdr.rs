use super::*;
use std::os::unix::net::UnixListener;
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
        let listener = UnixListener::bind(&path).unwrap();
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
    fixture.wait_session("codex/desktop-A");
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
    start_relay_options_env(&mut fixture, &relay, "", &[], &[("HERDR_SOCKET_PATH", &path)]);
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
    assert_eq!(shell["type"], "error", "relay typed into a shell pane: {shell}");
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
        (json!(["--permission-mode", "bypassPermissions"]), cwd.as_str()),
        (json!(["--permission-mode=bypassPermissions"]), cwd.as_str()),
        (json!(["--model", "--dangerously-skip-permissions"]), cwd.as_str()),
        (json!("--offline"), cwd.as_str()),
        (json!(["--offline"]), "/fixture/does-not-exist"),
        (json!(["--offline"]), "relative/dir"),
    ] {
        let reply = call(json!({"type":"session_create","name":"Created Pi","cwd":cwd,"agent":"claude","workspace_id":"ws-one","args":args}));
        assert_eq!(reply["type"], "error", "{args} in {cwd}: {reply}");
    }
    let created = call(json!({"type":"session_create","name":"Created Pi","cwd":cwd,"agent":"pi","workspace_id":"ws-one","args":["--offline","--model","sonnet"]}));
    assert_ne!(created["type"], "error", "{created}");
    assert_eq!(herdr.request("tab.create")["params"]["cwd"], json!(cwd));
    let started = herdr.request("agent.start");
    assert_eq!(
        started["params"]["args"],
        json!(["--offline", "--model", "sonnet"]),
        "a refused session_create reached agent.start"
    );
    let shell_workspace = call(json!({"type":"workspace_close","workspace_id":"ws-one"}));
    assert_eq!(
        shell_workspace["type"], "error",
        "relay closed a workspace holding a shell pane: {shell_workspace}"
    );
    let agents = call(json!({"type":"workspace_close","workspace_id":"ws-agents"}));
    assert_ne!(agents["type"], "error", "{agents}");
    assert_eq!(
        herdr.request("workspace.close")["params"]["workspace_id"],
        "ws-agents",
        "the refused shell workspace close reached Herdr"
    );
}
