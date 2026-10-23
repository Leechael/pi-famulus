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

fn start() -> (Fixture, SocketAddr) {
    let mut fixture = Fixture::new();
    let address = super::network::vacant_address();
    let listen = address.to_string();
    fixture.start_with(&["--mcp-listen", &listen, "--mcp-token", TOKEN], &[]);
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
        &[("Authorization", ""), ("MCP-Protocol-Version", "1900-01-01")],
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
