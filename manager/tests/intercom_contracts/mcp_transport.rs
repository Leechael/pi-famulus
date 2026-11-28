use super::*;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};

const MODERN: &str = "2026-07-28";
const TOKEN: &str = "transport-secret";

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    fn json(&self) -> Value {
        if self
            .header("content-type")
            .is_some_and(|v| v.contains("text/event-stream"))
        {
            let text = String::from_utf8_lossy(&self.body);
            let data = text
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim))
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&data).unwrap_or_else(|e| panic!("invalid SSE JSON: {e}: {text}"))
        } else {
            serde_json::from_slice(&self.body).unwrap_or_else(|e| {
                panic!(
                    "invalid response JSON: {e}: {:?}",
                    String::from_utf8_lossy(&self.body)
                )
            })
        }
    }
}

fn read_response(reader: &mut BufReader<TcpStream>) -> Response {
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).unwrap() > 0,
        "HTTP closed before status"
    );
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "HTTP closed inside headers"
        );
        if line == "\r\n" {
            break;
        }
        let (key, value) = line
            .trim_end()
            .split_once(':')
            .expect("valid response header");
        headers.push((key.to_string(), value.trim().to_string()));
    }
    let mut response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    if let Some(length) = response
        .header("content-length")
        .map(|v| v.parse::<usize>().unwrap())
    {
        response.body.resize(length, 0);
        reader.read_exact(&mut response.body).unwrap();
    } else if response
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
            if size == 0 {
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                break;
            }
            let offset = response.body.len();
            response.body.resize(offset + size, 0);
            reader.read_exact(&mut response.body[offset..]).unwrap();
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).unwrap();
            assert_eq!(&crlf, b"\r\n");
        }
    } else if !matches!(status, 202 | 204 | 304) {
        reader.read_to_end(&mut response.body).unwrap();
    }
    response
}

fn connect(address: SocketAddr) -> BufReader<TcpStream> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Ok(stream) = TcpStream::connect(address) {
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            return BufReader::new(stream);
        }
        assert!(Instant::now() < deadline, "MCP HTTP listener unavailable");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wire(
    address: SocketAddr,
    method: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    keep_alive: bool,
) -> Vec<u8> {
    let has = |name: &str| {
        headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    };
    let mut head = format!(
        "{method} /mcp HTTP/1.1\r\nHost: {address}\r\nConnection: {}\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    );
    if !has("Content-Type") {
        head.push_str("Content-Type: application/json\r\n");
    }
    if !has("Accept") {
        head.push_str("Accept: application/json, text/event-stream\r\n");
    }
    if !has("Authorization") {
        head.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
    }
    if !has("Content-Length") && !has("Transfer-Encoding") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn request(address: SocketAddr, method: &str, headers: &[(&str, &str)], body: &[u8]) -> Response {
    let mut reader = connect(address);
    reader
        .get_mut()
        .write_all(&wire(address, method, headers, body, false))
        .unwrap();
    read_response(&mut reader)
}

fn modern(method: &str, mut params: Value) -> Vec<u8> {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":MODERN,"io.modelcontextprotocol/clientInfo":{"name":"transport-fixture","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}});
    serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).unwrap()
}

/// The daemon binds an ephemeral port and the test reads it back once the
/// listener runs: a port picked and released here could be taken by another
/// test before the daemon binds it, and nothing waited for the listener.
fn start() -> (Fixture, SocketAddr) {
    start_env(Fixture::new(), &[])
}

fn start_env(mut fixture: Fixture, env: &[(&str, &str)]) -> (Fixture, SocketAddr) {
    fixture.start_with(&["--mcp-listen", "127.0.0.1:0", "--mcp-token", TOKEN], env);
    let deadline = Instant::now() + TIMEOUT;
    let url = loop {
        let output = fixture.run(&["status", "--json"]);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
        if status["mcp_status"] == "running" {
            break reqwest::Url::parse(status["mcp_url"].as_str().unwrap()).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "MCP listener never became ready: {status} {}",
            fixture.log()
        );
        thread::sleep(Duration::from_millis(10));
    };
    let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap())
        .parse()
        .unwrap();
    (fixture, address)
}

#[test]
fn legacy_initialization_is_stateless_and_notifications_return_empty_202() {
    let (_fixture, address) = start();
    for (offered, version) in [
        ("2025-06-18", "2025-06-18"),
        ("2025-11-25", "2025-11-25"),
        ("2099-01-01", "2025-11-25"),
    ] {
        let body=serde_json::to_vec(&json!({"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":offered,"clientInfo":{"name":"legacy-fixture","version":"1"},"capabilities":{}}})).unwrap();
        let initialized = request(address, "POST", &[], &body);
        assert_eq!(initialized.status, 200);
        assert_eq!(initialized.json()["result"]["protocolVersion"], version);
        assert!(
            initialized.header("mcp-session-id").is_none(),
            "stateless Go oracle never mints transport sessions"
        );
        let notification = request(
            address,
            "POST",
            &[("MCP-Protocol-Version", version)],
            br#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        );
        assert_eq!(notification.status, 202);
        assert!(notification.body.is_empty());
        let listed = request(
            address,
            "POST",
            &[
                ("MCP-Protocol-Version", version),
                ("Mcp-Session-Id", "ignored-in-stateless-mode"),
            ],
            br#"{"jsonrpc":"2.0","id":8,"method":"tools/list","params":{}}"#,
        );
        assert_eq!(listed.status, 200);
        assert!(listed.json()["result"]["tools"].is_array());
    }
}

#[test]
fn modern_stateless_method_errors_and_http_methods_have_distinct_statuses() {
    let (_fixture, address) = start();
    let listed = request(
        address,
        "POST",
        &[
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/list"),
        ],
        &modern("tools/list", json!({})),
    );
    assert_eq!(listed.status, 200);
    let result = listed.json();
    assert!(result["result"]["tools"].is_array());
    assert_eq!(result["result"]["resultType"], "complete");
    assert!(result["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].is_string());
    let send = result["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "send")
        .unwrap();
    assert_eq!(send["inputSchema"]["required"], json!(["to", "message"]));
    assert!(listed.header("mcp-session-id").is_none());
    let missing = request(
        address,
        "POST",
        &[
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "intercom/nonexistent"),
        ],
        &modern("intercom/nonexistent", json!({})),
    );
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["error"]["code"], -32601);
    for (method, key) in [
        ("resources/list", "resources"),
        ("resources/templates/list", "resourceTemplates"),
        ("prompts/list", "prompts"),
    ] {
        let response = request(
            address,
            "POST",
            &[("MCP-Protocol-Version", MODERN), ("Mcp-Method", method)],
            &modern(method, json!({})),
        );
        assert_eq!(response.status, 200);
        assert_eq!(response.json()["result"][key], json!([]));
    }
    for method in ["GET", "DELETE", "PUT"] {
        assert_eq!(
            request(address, method, &[("MCP-Protocol-Version", MODERN)], b"").status,
            405
        );
    }
}

#[test]
fn modern_header_body_mismatches_and_unsupported_versions_are_protocol_errors() {
    let (_fixture, address) = start();
    let body = modern("tools/list", json!({}));
    for headers in [
        vec![
            ("MCP-Protocol-Version", "2025-11-25"),
            ("Mcp-Method", "tools/list"),
        ],
        vec![
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
        ],
    ] {
        let rejected = request(address, "POST", &headers, &body);
        assert_eq!(rejected.status, 400);
        assert_eq!(rejected.json()["error"]["code"], -32020);
    }
    let mut future: Value = serde_json::from_slice(&body).unwrap();
    future["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let rejected = request(
        address,
        "POST",
        &[
            ("MCP-Protocol-Version", "2099-01-01"),
            ("Mcp-Method", "tools/list"),
        ],
        &serde_json::to_vec(&future).unwrap(),
    );
    assert_eq!(rejected.status, 400);
    let error = rejected.json();
    assert_eq!(error["error"]["code"], -32022);
    assert!(error["error"]["data"]["supported"]
        .as_array()
        .is_some_and(|versions| !versions.is_empty()));
}

#[test]
fn valid_chunked_and_identical_duplicate_lengths_each_inject_exactly_once() {
    let (fixture, address) = start();
    let mut target = fixture.plugin("target", "busy");
    let _source = fixture.plugin("source", "busy");
    let body = modern(
        "tools/call",
        json!({"name":"send","arguments":{"from":"pi/source","to":"pi/target","message":"HTTP framed body"}}),
    );
    let length = body.len().to_string();
    let response = request(
        address,
        "POST",
        &[
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "send"),
            ("Content-Length", &length),
            ("Content-Length", &length),
        ],
        &body,
    );
    assert_eq!(response.status, 200);
    assert_ne!(response.json()["result"]["isError"], true);
    target.event("deliver");
    target.no_event();
    let split = body.len() / 2;
    let chunked = format!(
        "{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
        split,
        String::from_utf8_lossy(&body[..split]),
        body.len() - split,
        String::from_utf8_lossy(&body[split..])
    );
    let response = request(
        address,
        "POST",
        &[
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "send"),
            ("Transfer-Encoding", "chunked"),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
        chunked.as_bytes(),
    );
    assert_eq!(response.status, 200);
    assert_ne!(response.json()["result"]["isError"], true);
    target.event("deliver");
    target.no_event();
}

#[test]
fn conflicting_lengths_and_malformed_header_names_reject_without_dispatch() {
    let (fixture, address) = start();
    let mut target = fixture.plugin("target", "busy");
    let _source = fixture.plugin("source", "busy");
    let body = modern(
        "tools/call",
        json!({"name":"send","arguments":{"from":"pi/source","to":"pi/target","message":"must not dispatch"}}),
    );
    let length = body.len().to_string();
    let other = (body.len() + 1).to_string();
    let rejected = request(
        address,
        "POST",
        &[("Content-Length", &length), ("Content-Length", &other)],
        &body,
    );
    assert_eq!(rejected.status, 400);
    let rejected = request(address, "POST", &[("Bad Header", "illegal-name")], &body);
    assert_eq!(rejected.status, 400);
    target.no_event();
    assert_eq!(
        request(
            address,
            "POST",
            &[
                ("MCP-Protocol-Version", MODERN),
                ("Mcp-Method", "tools/list")
            ],
            &modern("tools/list", json!({}))
        )
        .status,
        200
    );
}

#[test]
fn duplicate_auth_and_version_headers_cannot_override_a_rejecting_first_value() {
    let (fixture, address) = start();
    let mut target = fixture.plugin("target", "busy");
    let _source = fixture.plugin("source", "busy");
    let body = modern(
        "tools/call",
        json!({"name":"send","arguments":{"from":"pi/source","to":"pi/target","message":"must not dispatch"}}),
    );
    let rejected = request(
        address,
        "POST",
        &[
            ("Authorization", "Bearer wrong"),
            ("Authorization", "Bearer transport-secret"),
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "send"),
        ],
        &body,
    );
    assert_eq!(rejected.status, 401);
    let rejected = request(
        address,
        "POST",
        &[
            ("Authorization", "Bearer transport-secret"),
            ("MCP-Protocol-Version", "1900-01-01"),
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "send"),
        ],
        &body,
    );
    assert!((400..500).contains(&rejected.status));
    let rejected = request(
        address,
        "POST",
        &[
            ("Authorization", ""),
            ("MCP-Protocol-Version", "1900-01-01"),
        ],
        &body,
    );
    assert_eq!(
        rejected.status, 401,
        "version handling ran before authentication"
    );
    let rejected = request(
        address,
        "POST",
        &[
            ("Authorization", "Bearer transport-secret"),
            ("Origin", "https://evil.example"),
            ("MCP-Protocol-Version", "1900-01-01"),
        ],
        &body,
    );
    assert_eq!(
        rejected.status, 403,
        "version handling ran before origin policy"
    );
    target.no_event();
}

#[test]
fn malformed_json_and_http_content_negotiation_never_produce_successful_tools() {
    let (_fixture, address) = start();
    let rejected = request(address, "POST", &[], br#"{"jsonrpc":"2.0","id":1,"#);
    assert!(
        (400..500).contains(&rejected.status)
            || (rejected.status == 200 && rejected.json()["error"]["code"] == -32700)
    );
    let body = modern("tools/list", json!({}));
    assert_eq!(
        request(
            address,
            "POST",
            &[
                ("MCP-Protocol-Version", MODERN),
                ("Mcp-Method", "tools/list"),
                ("Last-Event-ID", "event-1")
            ],
            &body
        )
        .status,
        400
    );
    assert_eq!(
        request(
            address,
            "POST",
            &[
                ("MCP-Protocol-Version", MODERN),
                ("Mcp-Method", "tools/list"),
                ("Content-Type", "text/plain")
            ],
            &body
        )
        .status,
        415
    );
    assert_eq!(
        request(
            address,
            "POST",
            &[
                ("MCP-Protocol-Version", MODERN),
                ("Mcp-Method", "tools/list"),
                ("Accept", "text/plain")
            ],
            &body
        )
        .status,
        400
    );
}

#[test]
fn mcp_tool_discovery_explains_the_local_intercom_identity_requirement() {
    let (_fixture, address) = start();
    let response = request(
        address,
        "POST",
        &[
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/list"),
        ],
        &modern("tools/list", json!({})),
    );
    assert_eq!(response.status, 200);
    let result = response.json();
    for name in ["send", "notify_when_idle"] {
        let tool = result["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        let description = tool["inputSchema"]["properties"]["from"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("registered local intercom session")
                && description.contains("list_sessions"),
            "{name}: {description}"
        );
    }
}

#[test]
fn missing_mcp_sender_points_to_a_registered_session_for_any_client() {
    let (fixture, address) = start();
    let mut target = fixture.plugin("target", "busy");
    for (name, args) in [
        (
            "send",
            json!({"to":"pi/target","message":"must not dispatch"}),
        ),
        ("notify_when_idle", json!({"to":"pi/target","from":""})),
        ("notify_when_idle", json!({"to":"pi/target"})),
    ] {
        let error = call(address, name, args).unwrap_err();
        assert!(error.contains("from required"), "{name}: {error}");
        assert!(
            error.contains("registered local intercom session") && error.contains("list_sessions"),
            "{name}: {error}"
        );
    }
    target.no_event();
}

#[test]
fn notify_when_idle_enforces_the_same_local_sender_policy_as_send() {
    let (fixture, address) = start();
    let _target = fixture.plugin("target", "busy");
    let _source = fixture.plugin("source", "busy");
    let body = |from: &str| {
        modern(
            "tools/call",
            json!({"name":"notify_when_idle","arguments":{"from":from,"to":"pi/target"}}),
        )
    };
    let headers = [
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "notify_when_idle"),
    ];
    // A non-local `from` is rejected exactly like `send`'s local_sender gate:
    // start_idle refuses any watcher that is not a live local agent session.
    let rejected = request(address, "POST", &headers, &body("webhook/foreign"));
    assert_eq!(rejected.status, 200);
    let result = rejected.json();
    assert_eq!(result["result"]["isError"], true);
    assert!(
        result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not a local session"),
        "wrong rejection: {result}"
    );
    let rejected = request(address, "POST", &headers, &body("pi/missing"));
    assert_eq!(rejected.json()["result"]["isError"], true);
    // A real local session subscribes successfully, proving the gate is the
    // local-agent check rather than a blanket refusal.
    let subscribed = request(address, "POST", &headers, &body("pi/source"));
    assert_eq!(subscribed.status, 200);
    let result = subscribed.json();
    assert_ne!(result["result"]["isError"], true, "{result}");
    assert!(!result["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("not a local session"));
    let payload: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["status"], "subscribed");
    assert_eq!(payload["target"], "pi/target");
    assert!(!payload["subscriptionId"].as_str().unwrap().is_empty());
}

#[test]
fn http_keep_alive_handles_two_distinct_requests_on_one_connection() {
    let (_fixture, address) = start();
    let mut reader = connect(address);
    let body = modern("tools/list", json!({}));
    let headers = [
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", "tools/list"),
    ];
    reader
        .get_mut()
        .write_all(&wire(address, "POST", &headers, &body, true))
        .unwrap();
    let first = read_response(&mut reader);
    assert_eq!(first.status, 200);
    assert!(first.json()["result"]["tools"].is_array());
    reader
        .get_mut()
        .write_all(&wire(address, "POST", &headers, &body, false))
        .unwrap();
    let second = read_response(&mut reader);
    assert_eq!(second.status, 200);
    assert!(second.json()["result"]["tools"].is_array());
}

/// A pi session registered over the manager connection, as the pi-famulus
/// extension does: the daemon stamps the hello's `pi_pid` on the row. The
/// session object is the extension's own (`name`, `status`, extra fields).
fn manager_pi(
    fixture: &Fixture,
    session: &str,
    pi_pid: u32,
    mut object: Value,
) -> crate::common::Conn {
    let mut conn = crate::common::Conn::new(
        UnixStream::connect(fixture.root.join("manager/manager.sock")).unwrap(),
    );
    let hello = conn.hello(json!({
        "type": "hello",
        "client_kind": "extension",
        "session_id": session,
        "pi_pid": pi_pid,
        "protocol": 7,
    }));
    assert_eq!(hello["ok"], true, "pi hello: {hello}");
    object["id"] = json!(session);
    let registered =
        conn.request(json!({"type":"intercom","op":{"op":"register","session":object}}));
    assert_eq!(registered["ok"], true, "register {session}: {registered}");
    conn
}

/// `session_status` over MCP: `Ok` with the parsed payload, or `Err` with the
/// tool error text.
fn session_status(address: SocketAddr, session: &str) -> Result<Value, String> {
    call(address, "session_status", json!({ "session": session }))
}

/// One MCP tool call, allowed to take as long as `start_session` waits for
/// the agent to register (10s): `Ok` with the parsed payload, or `Err` with
/// the tool error text.
fn call(address: SocketAddr, name: &str, arguments: Value) -> Result<Value, String> {
    let mut reader = connect(address);
    reader
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = modern("tools/call", json!({"name":name,"arguments":arguments}));
    let headers = [
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", name),
    ];
    reader
        .get_mut()
        .write_all(&wire(address, "POST", &headers, &body, false))
        .unwrap();
    let response = read_response(&mut reader);
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    let result = response.json()["result"].clone();
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if result["isError"] == true {
        Err(text)
    } else {
        Ok(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {result}")))
    }
}

fn list_sessions(address: SocketAddr) -> Value {
    super::network::tool(address, TOKEN, "list_sessions", json!({}))
}

#[test]
fn session_status_reports_the_process_not_the_registry_status() {
    let (fixture, address) = start();
    // The stand-in pi process is this test's own child; only it is ever
    // signalled, and only after the assertions that need it alive (or when
    // a failed assertion unwinds).
    struct Standin(std::process::Child);
    impl Drop for Standin {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut standin = Standin(crate::common::command("sleep").arg("60").spawn().unwrap());
    let pid = standin.0.id();
    // The pane id is reported when the row has one. (No herdr runs in the
    // fixture, so nothing rewrites this row's status.)
    let _paned = manager_pi(
        &fixture,
        "paned-session",
        std::process::id(),
        json!({"name":"paned","herdr_pane_id":"pane-7"}),
    );
    let _alive = manager_pi(
        &fixture,
        "alive-session",
        pid,
        json!({"name":"alive-name","status":"busy"}),
    );
    // A row claiming another machine never has its pid checked here, even
    // when that pid is a live local process.
    let _elsewhere = manager_pi(
        &fixture,
        "elsewhere-session",
        pid,
        json!({"name":"elsewhere","machine_id":"machine-elsewhere"}),
    );
    let _init = manager_pi(&fixture, "init-session", 1, json!({"name":"init"}));
    let _twin_a = manager_pi(
        &fixture,
        "twin-a",
        std::process::id(),
        json!({"name":"twin"}),
    );
    let _twin_b = manager_pi(
        &fixture,
        "twin-b",
        std::process::id(),
        json!({"name":"twin"}),
    );
    // A client-socket registration's pid is self-declared, never vouched.
    let mut client = fixture.connect();
    client.ok(json!({"op":"register","session":{"id":"client-session","name":"client","status":"idle","pid":pid}}));
    let listed = list_sessions(address);

    let by_name = session_status(address, "alive-name").unwrap();
    assert_eq!(by_name["id"], "pi/alive-session", "{by_name}");
    assert_eq!(by_name["name"], "alive-name");
    assert_eq!(by_name["harness"], "pi");
    assert_eq!(by_name["state"], "alive", "{by_name}");
    assert_eq!(by_name["pid"], pid);
    assert_eq!(by_name["advisory_status"], "busy");
    assert!(by_name.get("herdr_pane_id").is_none(), "{by_name}");
    assert_eq!(
        session_status(address, "pi/alive-session").unwrap(),
        by_name
    );
    let paned = session_status(address, "paned").unwrap();
    assert_eq!(paned["herdr_pane_id"], "pane-7", "{paned}");
    assert_eq!(paned["state"], "alive", "{paned}");

    for session in [
        "pi/elsewhere-session",
        "pi/init-session",
        "pi/client-session",
    ] {
        let status = session_status(address, session).unwrap();
        assert_eq!(status["state"], "unknown", "{status}");
        assert!(status.get("pid").is_none(), "{status}");
    }
    for (session, error) in [
        ("pi/nope", "unknown session"),
        ("nope", "unknown session"),
        ("twin", "ambiguous"),
    ] {
        let text = session_status(address, session).unwrap_err();
        assert!(text.contains(error), "{session}: {text}");
    }

    // No side effects, and the public listing still hides pids and
    // `_`-prefixed internal fields.
    assert_eq!(list_sessions(address), listed);
    for row in listed.as_array().unwrap() {
        let keys: Vec<_> = row.as_object().unwrap().keys().collect();
        assert!(
            keys.iter()
                .all(|key| *key != "pid" && !key.starts_with('_')),
            "list_sessions leaked {keys:?}"
        );
    }

    // The process dies while its connection stays open: the row is still
    // listed and still says busy, but the session is exited.
    standin.0.kill().unwrap();
    standin.0.wait().unwrap();
    let exited = session_status(address, "pi/alive-session").unwrap();
    assert_eq!(exited["state"], "exited", "{exited}");
    assert_eq!(exited["advisory_status"], "busy");
    assert!(list_sessions(address)
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == "pi/alive-session"));
}

/// The MCP fixture with the stub Herdr as its only Herdr. The stub answers
/// every `workspace.create` / `tab.create` with pane `pane-new`, which it
/// lists as hosting the pi session `pi-created`.
fn herdr_mcp() -> (Fixture, SocketAddr, super::herdr::Herdr) {
    let fixture = Fixture::new();
    let herdr = super::herdr::Herdr::new(&fixture);
    let path = herdr.path.to_str().unwrap().to_string();
    let (fixture, address) = start_env(fixture, &[("HERDR_SOCKET_PATH", &path)]);
    (fixture, address, herdr)
}

fn cwd(fixture: &Fixture) -> String {
    fixture.root.to_str().unwrap().to_string()
}

/// Every herdr request a refused start must not make: the mutations, and the
/// `ping` probe too (validation comes before any herdr request).
const MUTATIONS: [&str; 6] = [
    "ping",
    "agent.start",
    "workspace.create",
    "tab.create",
    "agent.send_keys",
    "pane.close",
];

#[test]
fn start_session_refuses_bad_input_before_any_herdr_mutation() {
    let (fixture, address, herdr) = herdr_mcp();
    let cwd = cwd(&fixture);
    herdr.drain();
    for (arguments, error) in [
        (
            json!({"cwd":cwd,"name":"w","model":"--dangerously-skip-permissions"}),
            "model",
        ),
        (
            json!({"cwd":cwd,"name":"w","model":"sonnet; rm -rf ~"}),
            "model",
        ),
        (json!({"cwd":"relative/dir","name":"w"}), "absolute"),
        (
            json!({"cwd":format!("{cwd}/missing"),"name":"w"}),
            "absolute",
        ),
        (json!({"cwd":cwd,"name":"-w"}), "'-'"),
        (json!({"cwd":cwd,"name":""}), "name"),
        (json!({"cwd":cwd,"name":"a;b"}), "name"),
        (json!({"cwd":cwd,"name":"a\u{1b}[2Jb"}), "name"),
        (json!({"cwd":cwd,"name":"tab\there"}), "name"),
        (json!({"cwd":cwd,"name":"naïve"}), "name"),
        (json!({"cwd":cwd,"name":"$(id)"}), "name"),
        (json!({"cwd":cwd,"name":"x".repeat(65)}), "name"),
        // Names herdr 0.9.5 refuses with `invalid_agent_name`.
        (json!({"cwd":cwd,"name":"Upper"}), "lowercase"),
        (json!({"cwd":cwd,"name":"two words"}), "lowercase"),
        (json!({"cwd":cwd,"name":"a.b"}), "lowercase"),
        (json!({"cwd":cwd,"name":"x".repeat(33)}), "32"),
        (
            json!({"cwd":cwd,"name":"1abc"}),
            "start with a lowercase letter",
        ),
        (
            json!({"cwd":cwd,"name":"_x"}),
            "start with a lowercase letter",
        ),
        (json!({"name":"w"}), "cwd"),
        (
            json!({"cwd":cwd,"name":"w","args":"--x"}),
            "additional property",
        ),
    ] {
        let text = call(address, "start_session", arguments.clone()).unwrap_err();
        assert!(text.contains(error), "{arguments}: {text}");
    }
    super::herdr::assert_no_herdr_method(&herdr, &MUTATIONS);
}

#[test]
fn start_session_without_herdr_fails_cleanly_and_records_nothing() {
    let fixture = Fixture::new();
    let absent = fixture.root.join("no-herdr.sock");
    let (fixture, address) = start_env(fixture, &[("HERDR_SOCKET_PATH", absent.to_str().unwrap())]);
    let text = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"worker"}),
    )
    .unwrap_err();
    assert!(text.contains("not implemented"), "{text}");
    assert_eq!(
        text.matches("not available").count(),
        1,
        "the error repeats itself: {text}"
    );
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("not started by this daemon"), "{text}");
    let text = session_status(address, "pane-new").unwrap_err();
    assert!(text.contains("unknown session"), "{text}");
}

/// A stand-in pi process this test spawned itself, killed and reaped when it
/// is dropped (also when an assertion fails).
struct Standin(std::process::Child);
impl Standin {
    fn spawn() -> Self {
        Standin(crate::common::command("sleep").arg("60").spawn().unwrap())
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn die(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Standin {
    fn drop(&mut self) {
        self.die();
    }
}

/// `stop_session` against an agent that exits when herdr closes its pane, as
/// a real one does: answers the ctrl+c and pane.close params and the result.
fn stop_while_the_agent_dies(
    address: SocketAddr,
    herdr: &super::herdr::Herdr,
    session: &str,
    standin: &mut Standin,
) -> (Value, Value, Result<Value, String>) {
    thread::scope(|scope| {
        let stop =
            scope.spawn(move || call(address, "stop_session", json!({ "session": session })));
        let keys = herdr.request("agent.send_keys")["params"].clone();
        let close = herdr.request("pane.close")["params"].clone();
        standin.die();
        (keys, close, stop.join().unwrap())
    })
}

#[test]
fn started_session_is_alive_once_registered_and_stop_closes_only_its_own_pane() {
    let (fixture, address, herdr) = herdr_mcp();
    let cwd = cwd(&fixture);
    let mut standin = Standin::spawn();
    let me = standin.pid();
    let fixture_ref = &fixture;
    // The agent registers a moment after Herdr lists its pane, as pi does.
    let (started, registration) = thread::scope(|scope| {
        let late = scope.spawn(move || {
            thread::sleep(Duration::from_millis(800));
            manager_pi(
                fixture_ref,
                "pi-created",
                me,
                json!({"name":"worker","herdr_pane_id":"pane-new"}),
            )
        });
        let started = call(
            address,
            "start_session",
            json!({"cwd":cwd,"name":"worker","model":"sonnet-4.5"}),
        );
        (started, late.join().unwrap())
    });
    let started = started.unwrap();
    let start = herdr.request("agent.start")["params"].clone();
    assert_eq!(start["kind"], "pi", "{start}");
    assert_eq!(
        start["args"],
        json!(["--name", "worker", "--model", "sonnet-4.5"]),
        "{start}"
    );
    assert_eq!(started["id"], "pi/pi-created", "{started}");
    assert_eq!(started["name"], "worker");
    assert_eq!(started["pane_id"], "pane-new");
    assert_eq!(started["host"], "herdr");
    assert_eq!(started["state"], "alive", "{started}");
    assert_eq!(started["pid"], me);
    let by_pane = session_status(address, "pane-new").unwrap();
    assert_eq!(by_pane["state"], "alive", "{by_pane}");
    assert_eq!(by_pane["id"], "pi/pi-created", "{by_pane}");
    assert_eq!(session_status(address, "pi/pi-created").unwrap(), by_pane);

    // A live session this daemon did not start is never stopped.
    let _foreign = manager_pi(
        &fixture,
        "foreign",
        me,
        json!({"name":"foreign","herdr_pane_id":"pane-one"}),
    );
    for session in ["pi/foreign", "pane-one"] {
        let text = call(address, "stop_session", json!({ "session": session })).unwrap_err();
        assert!(text.contains("not started by this daemon"), "{text}");
    }
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);

    let (keys, close, stopped) =
        stop_while_the_agent_dies(address, &herdr, "pi/pi-created", &mut standin);
    let stopped = stopped.unwrap();
    assert_eq!(keys, json!({"target":"pane-new","keys":["ctrl+c"]}));
    assert_eq!(close["pane_id"], "pane-new");
    assert_eq!(stopped["state"], "exited", "{stopped}");
    assert_eq!(stopped["already_exited"], false, "{stopped}");
    assert_eq!(
        session_status(address, "pi/pi-created").unwrap()["state"],
        "exited"
    );

    // The agent's registration is gone and another agent now has the pane:
    // a second stop succeeds and touches nothing.
    drop(registration);
    herdr.set_panes(vec![json!({"pane_id":"pane-new","workspace_id":"ws-new","agent":"pi","agent_status":"busy","agent_session":{"agent":"pi","kind":"session_id","value":"other-agent"}})]);
    let again = call(address, "stop_session", json!({"session":"pi/pi-created"})).unwrap();
    assert_eq!(again["state"], "exited", "{again}");
    assert_eq!(again["already_exited"], true, "{again}");
    assert_eq!(again["pane"], "left_open", "{again}");
    assert_eq!(again["pane_id"], "pane-new", "{again}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
    assert_eq!(
        session_status(address, "pane-new").unwrap()["state"],
        "exited"
    );

    // The first stop closed its pane: a pane listed under that id later (a
    // reused id, here an idle shell) is someone else's and stays open.
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"}),
    ]);
    let last = call(address, "stop_session", json!({"session":"pane-new"})).unwrap();
    assert_eq!(last["already_exited"], true, "{last}");
    assert_eq!(last["pane"], "left_open", "{last}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

/// The two ways a session that never registered ends: stopped while still
/// `starting` (by its pane), and its agent gone with the pane left as an
/// idle shell, which a later stop closes, and never a pane hosting an agent.
#[test]
fn an_unregistered_session_stops_by_its_pane_or_is_exited_once_its_pane_is_a_shell() {
    let (fixture, address, herdr) = herdr_mcp();
    let started = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"first"}),
    )
    .unwrap();
    assert_eq!(started["state"], "starting", "{started}");
    let stopped = call(address, "stop_session", json!({"session":"pane-new"})).unwrap();
    assert_eq!(
        herdr.request("agent.send_keys")["params"],
        json!({"target":"pane-new","keys":["ctrl+c"]})
    );
    assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-new");
    assert_eq!(stopped["state"], "exited", "{stopped}");
    assert_eq!(stopped["already_exited"], false, "{stopped}");

    // The stub hands out `pane-new` again for the second session.
    let started = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"second"}),
    )
    .unwrap();
    assert_eq!(started["state"], "starting", "{started}");
    let shell = json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"});
    let other = json!({"pane_id":"pane-new","workspace_id":"ws-new","agent":"claude","agent_status":"idle"});
    // Past the start deadline its pane hosts no agent and runs nothing: the
    // agent is gone.
    herdr.set_panes(vec![shell.clone()]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "exited", "{status}");
    assert_eq!(status["name"], "second", "{status}");
    // Stopping an exited session touches nothing, not even an idle shell
    // under its pane id (that id may name someone else's pane by now); the
    // caller gets the pane id.
    herdr.drain();
    let kept = call(address, "stop_session", json!({"session":"pane-new"})).unwrap();
    assert_eq!(kept["already_exited"], true, "{kept}");
    assert_eq!(kept["pane"], "left_open", "{kept}");
    assert_eq!(kept["pane_id"], "pane-new", "{kept}");
    super::herdr::assert_no_herdr_method(&herdr, &MUTATIONS);
    // No agent, but something runs in the pane: no evidence of an exit, and
    // nothing a stop may act on.
    herdr.set_foreground(vec![json!({"pid":4343,"name":"vim"})]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "unknown", "{status}");
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
    // Another agent in the pane: never ours.
    herdr.set_foreground(vec![]);
    herdr.set_panes(vec![other]);
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

#[test]
fn a_session_that_has_not_registered_is_starting_then_alive_under_either_handle() {
    let (fixture, address, herdr) = herdr_mcp();
    // No registration within the start deadline (10s): still a success, with
    // the pane id as the handle.
    let started = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"slow"}),
    )
    .unwrap();
    assert_eq!(
        herdr.request("agent.start")["params"]["args"],
        json!(["--name", "slow"])
    );
    assert_eq!(started["state"], "starting", "{started}");
    assert_eq!(started["pane_id"], "pane-new", "{started}");
    assert_eq!(started["host"], "herdr");
    assert!(started.get("id").is_none(), "{started}");
    assert_eq!(
        session_status(address, "pane-new").unwrap()["state"],
        "starting"
    );

    let registration = manager_pi(
        &fixture,
        "pi-created",
        std::process::id(),
        json!({"name":"slow","herdr_pane_id":"pane-new"}),
    );
    let by_pane = session_status(address, "pane-new").unwrap();
    assert_eq!(by_pane["state"], "alive", "{by_pane}");
    assert_eq!(by_pane["id"], "pi/pi-created", "{by_pane}");
    assert_eq!(by_pane["pane_id"], "pane-new", "{by_pane}");
    assert_eq!(session_status(address, "pi/pi-created").unwrap(), by_pane);

    // Its registration drops while the process runs and the pane still
    // hosts the agent (a reconnect gap, an unpublish): alive, flagged as
    // unreachable over messaging, under either handle.
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    for handle in ["pi/pi-created", "pane-new"] {
        let status = session_status(address, handle).unwrap();
        assert_eq!(status["state"], "alive", "{handle}: {status}");
        assert_eq!(status["advisory_status"], "unregistered", "{status}");
        assert_eq!(status["pid"], std::process::id(), "{status}");
    }
    // ...and stopping it really acts: keys to its pane, pane closed. Its
    // process (this test) outlives the stop, so the result says it is still
    // running rather than claiming it exited.
    let stopped = call(address, "stop_session", json!({"session":"pi/pi-created"})).unwrap();
    assert_eq!(
        herdr.request("agent.send_keys")["params"],
        json!({"target":"pane-new","keys":["ctrl+c"]})
    );
    assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-new");
    assert_eq!(stopped["already_exited"], false, "{stopped}");
    assert_eq!(stopped["state"], "alive", "{stopped}");
    assert_eq!(stopped["pid"], std::process::id(), "{stopped}");
    assert_eq!(
        session_status(address, "pane-new").unwrap()["state"],
        "alive"
    );
    // A second stop does not act again on the pane the first one closed
    // (its id may name another pane now): it only reports the process.
    let again = call(address, "stop_session", json!({"session":"pi/pi-created"})).unwrap();
    assert_eq!(again["still_running"], true, "{again}");
    assert_eq!(again["pid"], std::process::id(), "{again}");
    super::herdr::assert_no_herdr_method(&herdr, &MUTATIONS);
}

/// The pid decides when one is known: a pane that shows no agent does not
/// end a session whose process runs, and the pid of the session's current
/// process is adopted when pi comes back under the same id.
#[test]
fn a_known_pid_outweighs_the_pane_and_follows_the_sessions_current_process() {
    let (fixture, address, herdr) = herdr_mcp();
    let mut first = Standin::spawn();
    let first_pid = first.pid();
    let fixture_ref = &fixture;
    let (started, registration) = thread::scope(|scope| {
        let late = scope.spawn(move || {
            thread::sleep(Duration::from_millis(800));
            manager_pi(
                fixture_ref,
                "pi-created",
                first_pid,
                json!({"name":"resumed","herdr_pane_id":"pane-new"}),
            )
        });
        let started = call(
            address,
            "start_session",
            json!({"cwd":cwd(&fixture),"name":"resumed"}),
        );
        (started, late.join().unwrap())
    });
    assert_eq!(started.unwrap()["pid"], first_pid);
    // pi exits and is resumed in the same pane under the same session id.
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    first.die();
    // Seen exited (its known pid is dead)...
    assert_eq!(
        session_status(address, "pane-new").unwrap()["state"],
        "exited"
    );
    // ...and alive again when pi resumes under the same session id.
    let second = Standin::spawn();
    let resumed = manager_pi(
        &fixture,
        "pi-created",
        second.pid(),
        json!({"name":"resumed","herdr_pane_id":"pane-new"}),
    );
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "alive", "{status}");
    assert_eq!(status["pid"], second.pid(), "{status}");
    drop(resumed);
    wait_unlisted(address, "pi/pi-created");
    // herdr stops detecting the agent, but its process runs: still alive.
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"}),
    ]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "alive", "{status}");
    assert_eq!(status["pid"], second.pid(), "{status}");
}

/// The pane id is not the agent's identity. A pane that now hosts an agent
/// this record cannot identify as its own is never stopped or closed, and
/// a row from another machine that claims the pane is not this session.
#[test]
fn a_pane_that_hosts_another_agent_is_never_acted_on() {
    let (fixture, address, herdr) = herdr_mcp();
    // Started; its pi has not registered within the deadline.
    let started = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"gone"}),
    )
    .unwrap();
    assert_eq!(started["state"], "starting", "{started}");
    let _intruder = manager_pi(
        &fixture,
        "intruder",
        std::process::id(),
        json!({"name":"intruder","machine_id":"machine-elsewhere","herdr_pane_id":"pane-new"}),
    );
    let text = call(address, "stop_session", json!({"session":"intruder"})).unwrap_err();
    assert!(text.contains("not started by this daemon"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);

    // Its pi died before registering and the user started claude there.
    herdr.set_panes(vec![json!({"pane_id":"pane-new","workspace_id":"ws-new","agent":"claude","agent_status":"idle","agent_session":{"source":"native","agent":"claude","kind":"id","value":"0f8f6c2e-claude"}})]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "unknown", "{status}");
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);

    // Or: its pi registered and exited before any call saw it (so no pid),
    // and another pi now runs in the pane.
    let mut flash = fixture.connect();
    flash.ok(json!({"op":"register","session":{"id":"pi-created","name":"gone","status":"busy","herdr_pane_id":"pane-new"}}));
    drop(flash);
    wait_unlisted(address, "pi/pi-created");
    herdr.set_panes(vec![json!({"pane_id":"pane-new","workspace_id":"ws-new","agent":"pi","agent_status":"busy","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"someone-else"}})]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "unknown", "{status}");
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

/// The client gives up while start_session waits for the agent: the session
/// herdr created is still recorded, so it can be found and stopped.
#[test]
fn a_start_whose_client_went_away_is_still_recorded() {
    let (fixture, address, herdr) = herdr_mcp();
    let mut reader = connect(address);
    let body = modern(
        "tools/call",
        json!({"name":"start_session","arguments":{"cwd":cwd(&fixture),"name":"orphan"}}),
    );
    let headers = [
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "start_session"),
    ];
    reader
        .get_mut()
        .write_all(&wire(address, "POST", &headers, &body, false))
        .unwrap();
    herdr.request("agent.start");
    reader.get_mut().shutdown(std::net::Shutdown::Both).unwrap();
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let status = session_status(address, "pane-new").unwrap();
        if status["host"] == "herdr" {
            assert_eq!(status["name"], "orphan", "{status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the abandoned start was never recorded: {status}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Two stops of one session at once: one stops it, the other succeeds
/// without a second ctrl+c or pane.close.
#[test]
fn overlapping_stops_dispatch_once() {
    let (fixture, address, herdr) = herdr_mcp();
    let fixture_ref = &fixture;
    let (started, _registration) = thread::scope(|scope| {
        let late = scope.spawn(move || {
            thread::sleep(Duration::from_millis(800));
            let mut plugin = fixture_ref.connect();
            plugin.ok(json!({"op":"register","session":{"id":"pi-created","name":"twice","status":"busy","herdr_pane_id":"pane-new"}}));
            plugin
        });
        let started = call(
            address,
            "start_session",
            json!({"cwd":cwd(&fixture),"name":"twice"}),
        );
        (started, late.join().unwrap())
    });
    assert_eq!(started.unwrap()["state"], "alive");
    // The agent takes a while to exit after ctrl+c.
    herdr.set_foreground(vec![json!({"pid":4242,"name":"pi"})]);
    thread::scope(|scope| {
        let first = scope.spawn(|| call(address, "stop_session", json!({"session":"pane-new"})));
        assert_eq!(
            herdr.request("agent.send_keys")["params"],
            json!({"target":"pane-new","keys":["ctrl+c"]})
        );
        let second = call(address, "stop_session", json!({"session":"pane-new"})).unwrap();
        assert_eq!(second["stop_in_progress"], true, "{second}");
        herdr.set_foreground(vec![]);
        let first = first.join().unwrap().unwrap();
        assert_eq!(first["state"], "exited", "{first}");
    });
    assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-new");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

/// How long a stop waits for herdr to identify a registered agent that runs
/// in its pane's foreground (the daemon's `IDENTIFY_WAIT`).
const IDENTIFY_WAIT: Duration = Duration::from_secs(5);

/// The pane `start_session` created, as herdr lists it before it identifies
/// the agent: no `agent`, no `agent_session`.
fn unidentified_pane() -> Value {
    json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"})
}

/// The same pane once herdr identified its agent as `pi-created`.
fn identified_pane() -> Value {
    json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new","agent":"pi","agent_status":"idle","agent_session":{"source":"native","agent":"pi","kind":"session_id","value":"pi-created"}})
}

/// Start a session whose pi (`pid`, in the pane's foreground) registers
/// 300ms into the start, while herdr does not identify it yet.
fn start_unidentified(
    fixture: &Fixture,
    address: SocketAddr,
    herdr: &super::herdr::Herdr,
    pid: u32,
) -> (Value, crate::common::Conn) {
    herdr.set_panes(vec![unidentified_pane()]);
    herdr.set_foreground(vec![json!({"pid":pid,"name":"pi"})]);
    let (started, registration) = thread::scope(|scope| {
        let late = scope.spawn(move || {
            thread::sleep(Duration::from_millis(300));
            manager_pi(
                fixture,
                "pi-created",
                pid,
                json!({"name":"late","herdr_pane_id":"pane-new"}),
            )
        });
        let started = call(
            address,
            "start_session",
            json!({"cwd":cwd(fixture),"name":"late"}),
        );
        (started, late.join().unwrap())
    });
    let started = started.unwrap();
    assert_eq!(started["state"], "alive", "{started}");
    assert_eq!(started["id"], "pi/pi-created", "{started}");
    (started, registration)
}

/// herdr identifies a started pi only once that pi reports to it, which can
/// come after the pi registered with the daemon and start_session returned.
/// A stop in that gap waits for herdr's identification instead of acting
/// without it (or refusing a session it just reported alive), then stops it.
#[test]
fn a_stop_waits_for_herdr_to_identify_the_started_agent() {
    let (fixture, address, herdr) = herdr_mcp();
    let mut standin = Standin::spawn();
    let me = standin.pid();
    let (_started, _registration) = start_unidentified(&fixture, address, &herdr, me);
    herdr.drain();
    // herdr identifies the agent 1.5s after start_session returned.
    herdr.set_panes_after(Duration::from_millis(1500), vec![identified_pane()]);
    let began = Instant::now();
    thread::scope(|scope| {
        let stop =
            scope.spawn(|| call(address, "stop_session", json!({"session":"pi/pi-created"})));
        let keys = loop {
            if stop.is_finished() {
                panic!(
                    "stop_session answered before sending any key: {:?}",
                    stop.join().unwrap()
                );
            }
            match herdr.requests.recv_timeout(Duration::from_millis(20)) {
                Ok(request) if request["method"] == "agent.send_keys" => {
                    break request["params"].clone()
                }
                Ok(request) => assert_ne!(request["method"], "pane.close", "{request}"),
                Err(_) => {}
            }
        };
        // No key before herdr identified the agent as this session.
        assert!(
            began.elapsed() >= Duration::from_millis(1500),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(keys, json!({"target":"pane-new","keys":["ctrl+c"]}));
        // pi exits on the ctrl+c: the shell has the terminal, herdr lists no agent.
        standin.die();
        herdr.set_foreground(vec![]);
        herdr.set_panes(vec![unidentified_pane()]);
        assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-new");
        let stopped = stop.join().unwrap().unwrap();
        assert_eq!(stopped["state"], "exited", "{stopped}");
        assert_eq!(stopped["pane"], "closed", "{stopped}");
    });
}

/// herdr never identifies the agent in the pane: the stop waits at most
/// IDENTIFY_WAIT, then refuses, sending nothing and closing nothing. A pane
/// whose foreground is not the session's pi (the user's command) is refused
/// at once, without that wait.
#[test]
fn a_stop_that_herdr_never_identifies_refuses_after_the_bound_and_sends_nothing() {
    let (fixture, address, herdr) = herdr_mcp();
    let standin = Standin::spawn();
    let (_started, _registration) = start_unidentified(&fixture, address, &herdr, standin.pid());
    herdr.drain();
    let began = Instant::now();
    let text = call(address, "stop_session", json!({"session":"pi/pi-created"})).unwrap_err();
    let waited = began.elapsed();
    assert!(
        text.contains("nothing was sent") && text.contains("pane-new"),
        "{text}"
    );
    assert!(text.contains("not identified"), "{text}");
    assert!(
        waited >= IDENTIFY_WAIT && waited < IDENTIFY_WAIT + Duration::from_secs(3),
        "waited {waited:?}"
    );
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);

    // The user's command, not the session's pi, has the terminal.
    herdr.set_foreground(vec![json!({"pid":4343,"name":"vim"})]);
    let began = Instant::now();
    let text = call(address, "stop_session", json!({"session":"pi/pi-created"})).unwrap_err();
    assert!(
        text.contains("nothing was sent") && text.contains("pane-new"),
        "{text}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

fn wait_unlisted(address: SocketAddr, id: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while list_sessions(address)
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == id)
    {
        assert!(Instant::now() < deadline, "{id} stayed registered");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A recorded session ends on evidence about its process: once the vouched
/// pid dies it is exited, though its pane still lists an agent.
#[test]
fn a_recorded_session_is_exited_when_its_vouched_pid_dies_without_its_row() {
    let (fixture, address, _herdr) = herdr_mcp();
    /// This test's own stand-in pi, killed and reaped even if it fails.
    struct Standin(std::process::Child);
    impl Drop for Standin {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut standin = Standin(crate::common::command("sleep").arg("60").spawn().unwrap());
    let pid = standin.0.id();
    let fixture_ref = &fixture;
    let (started, registration) = thread::scope(|scope| {
        let late = scope.spawn(move || {
            thread::sleep(Duration::from_millis(800));
            manager_pi(
                fixture_ref,
                "pi-created",
                pid,
                json!({"name":"mortal","herdr_pane_id":"pane-new"}),
            )
        });
        let started = call(
            address,
            "start_session",
            json!({"cwd":cwd(&fixture),"name":"mortal"}),
        );
        (started, late.join().unwrap())
    });
    let started = started.unwrap();
    assert_eq!(started["pid"], pid, "{started}");
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    assert_eq!(
        session_status(address, "pane-new").unwrap()["state"],
        "alive"
    );
    // Only this test's own child is signalled.
    standin.0.kill().unwrap();
    standin.0.wait().unwrap();
    let status = session_status(address, "pi/pi-created").unwrap();
    assert_eq!(status["state"], "exited", "{status}");
}

/// A start whose agent fails after herdr created the pane closes that pane,
/// says so, and records nothing.
#[test]
fn a_failed_agent_start_closes_its_pane_and_records_nothing() {
    let (fixture, address, herdr) = herdr_mcp();
    herdr.fail(&["agent.start"]);
    let text = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"broken"}),
    )
    .unwrap_err();
    assert!(text.contains("closed pane pane-new"), "{text}");
    assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-new");
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("not started by this daemon"), "{text}");
    herdr.fail(&["agent.start", "pane.close"]);
    let text = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"broken"}),
    )
    .unwrap_err();
    assert!(text.contains("pane pane-new is left open"), "{text}");
}

/// Start a session whose pi registers over the client socket (so no pid is
/// vouched) while start_session waits.
fn start_registered_without_pid(
    fixture: &Fixture,
    address: SocketAddr,
    name: &str,
) -> (Value, Peer) {
    thread::scope(|scope| {
        let late = scope.spawn(move || {
            thread::sleep(Duration::from_millis(800));
            let mut plugin = fixture.connect();
            plugin.ok(json!({"op":"register","session":{"id":"pi-created","name":name,"status":"busy","herdr_pane_id":"pane-new"}}));
            plugin
        });
        let started = call(
            address,
            "start_session",
            json!({"cwd":cwd(fixture),"name":name}),
        )
        .unwrap();
        (started, late.join().unwrap())
    })
}

/// A registration that only claims the pane (its `herdr_pane_id` is an
/// environment variable of whatever registered) is not the agent herdr runs
/// there: with no vouched pid and a pi herdr has not identified, the session
/// is `unknown` and a stop sends nothing and closes nothing.
#[test]
fn a_registration_that_only_claims_the_pane_authorises_no_stop() {
    let (fixture, address, herdr) = herdr_mcp();
    let (started, _registration) = start_registered_without_pid(&fixture, address, "claimed");
    assert_eq!(started["state"], "alive", "{started}");
    // Our pi was suspended; the user started a fresh pi herdr has not
    // identified yet. Our registration still claims the pane.
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","agent":"pi","agent_status":"busy"}),
    ]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "unknown", "{status}");
    let text = call(address, "stop_session", json!({"session":"pi/pi-created"})).unwrap_err();
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

/// A pane with no agent but a foreground process is not evidence that a
/// session without a known pid exited.
#[test]
fn a_busy_pane_without_an_agent_is_not_an_exit() {
    let (fixture, address, herdr) = herdr_mcp();
    let (_started, registration) = start_registered_without_pid(&fixture, address, "busy");
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"}),
    ]);
    herdr.set_foreground(vec![json!({"pid":4343,"name":"make"})]);
    let status = session_status(address, "pane-new").unwrap();
    assert_eq!(status["state"], "unknown", "{status}");
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

/// A row from another machine carrying the new pane's id is never taken
/// for the new session's registration.
#[test]
fn start_session_ignores_a_remote_row_claiming_the_new_pane() {
    let (fixture, address, _herdr) = herdr_mcp();
    let _intruder = manager_pi(
        &fixture,
        "intruder",
        std::process::id(),
        json!({"name":"intruder","machine_id":"machine-elsewhere","herdr_pane_id":"pane-new"}),
    );
    let started = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"local"}),
    )
    .unwrap();
    assert!(started.get("id").is_none(), "{started}");
    assert_eq!(started["state"], "starting", "{started}");
    let text = call(address, "stop_session", json!({"session":"pi/intruder"})).unwrap_err();
    assert!(text.contains("not started by this daemon"), "{text}");
}

/// The MCP stop never leaves this machine: when its pane-id target resolves
/// to another machine's row, it is still judged against this machine's herdr
/// (here: nothing of ours in the pane, so refused) and never relayed.
#[test]
fn an_mcp_stop_is_never_relayed_to_another_machine() {
    let (fixture, address, herdr) = herdr_mcp();
    let started = call(
        address,
        "start_session",
        json!({"cwd":cwd(&fixture),"name":"local"}),
    )
    .unwrap();
    assert_eq!(started["state"], "starting", "{started}");
    // Locally the pane now shows no agent (so no placeholder row carries its
    // id) while something runs in it; another machine has a row that the
    // pane id resolves to.
    herdr.set_panes(vec![
        json!({"pane_id":"pane-new","workspace_id":"ws-new","cwd":"/fixture/new"}),
    ]);
    herdr.set_foreground(vec![json!({"pid":4343,"name":"make"})]);
    let _remote = manager_pi(
        &fixture,
        "remote-pane",
        std::process::id(),
        json!({"name":"pane-new","machine_id":"machine-elsewhere","herdr_pane_id":"pane-new"}),
    );
    let text = call(address, "stop_session", json!({"session":"pane-new"})).unwrap_err();
    assert!(!text.contains("relay"), "the stop was relayed: {text}");
    assert!(text.contains("pane-new"), "{text}");
    super::herdr::assert_no_herdr_method(&herdr, &["agent.send_keys", "pane.close"]);
}

/// The name given to start_session stays a handle for its record once the
/// registration row is gone, for status and stop alike.
#[test]
fn the_start_name_stays_a_handle_after_the_row_is_gone() {
    let (fixture, address, herdr) = herdr_mcp();
    let (started, registration) = start_registered_without_pid(&fixture, address, "keeper");
    assert_eq!(started["id"], "pi/pi-created", "{started}");
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    let status = session_status(address, "keeper").unwrap();
    assert_eq!(
        status["host"], "herdr",
        "not the recorded session: {status}"
    );
    assert_eq!(status["id"], "pi/pi-created", "{status}");
    let stopped = call(address, "stop_session", json!({"session":"keeper"})).unwrap();
    assert_eq!(stopped["already_exited"], false, "{stopped}");
    assert_eq!(herdr.request("pane.close")["params"]["pane_id"], "pane-new");
}

/// A start name that names more than one session is refused as ambiguous:
/// two records, or a record and another session's live row.
#[test]
fn a_start_name_that_names_more_than_one_session_is_ambiguous() {
    let (fixture, address, _herdr) = herdr_mcp();
    let (_, registration) = start_registered_without_pid(&fixture, address, "solo");
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    // Another session that happens to carry the same name.
    let mut other = fixture.connect();
    other.ok(json!({"op":"register","session":{"id":"other","name":"solo","status":"idle"}}));
    for tool in ["session_status", "stop_session"] {
        let text = call(address, tool, json!({"session":"solo"})).unwrap_err();
        assert!(text.contains("ambiguous"), "{tool}: {text}");
    }
    drop(other);
    wait_unlisted(address, "pi/other");
    // A second session started under the same name.
    let (_, registration) = start_registered_without_pid(&fixture, address, "solo");
    drop(registration);
    wait_unlisted(address, "pi/pi-created");
    for tool in ["session_status", "stop_session"] {
        let text = call(address, tool, json!({"session":"solo"})).unwrap_err();
        assert!(text.contains("ambiguous"), "{tool}: {text}");
    }
}
