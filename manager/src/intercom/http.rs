use super::{config::Config, str_field, Shared};
use hmac::{Hmac, Mac};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::http::{HeaderMap, Method, Request, Response};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use sha2::Sha256;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use std::{convert::Infallible, sync::Arc};

const MODERN: &str = "2026-07-28";
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_INFO: &str = "io.modelcontextprotocol/clientInfo";
const VERSIONS: [&str; 5] = [
    MODERN,
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
// Nothing larger than one relay frame can be delivered remotely.
const MAX_BODY: usize = super::relay::MAX_FRAME;
type Body = UnsyncBoxBody<Bytes, Infallible>;
type Reply = Response<Body>;

pub async fn start(rt: Shared) -> Result<tokio::task::JoinHandle<()>, String> {
    let listener = tokio::net::TcpListener::bind(&rt.cfg.mcp_listen)
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    if rt.cfg.mcp_token.trim().is_empty() {
        return Err("refusing unauthenticated MCP; no bearer token configured".into());
    }
    if !address.ip().is_loopback() && !rt.cfg.mcp_allow_non_loopback {
        return Err(format!(
            "refusing plain-HTTP MCP on non-loopback address {address}; set mcp_allow_non_loopback"
        ));
    }
    *rt.mcp_url.lock().unwrap() = format!("http://{address}/mcp");
    eprintln!("intercom MCP http://{address}/mcp");
    Ok(tokio::spawn(async move {
        let mut stop = rt.stop.subscribe();
        let permits = Arc::new(tokio::sync::Semaphore::new(128));
        let throttle = Arc::new(AuthThrottle::default());
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else { continue };
                    let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                    let Ok(local) = stream.local_addr() else { continue };
                    let rt = rt.clone();
                    let throttle = throttle.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        let service = service_fn(move |req| {
                            let rt = rt.clone();
                            let throttle = throttle.clone();
                            async move {
                                let reply = handle(rt, &throttle, local, peer.ip(), req).await;
                                Ok::<_, Infallible>(reply)
                            }
                        });
                        // Hyper owns HTTP framing, duplicate Content-Length validation,
                        // chunked decoding and persistent-connection request boundaries.
                        let _ = http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(Duration::from_secs(5))
                            .max_headers(100)
                            .max_buf_size(32 * 1024)
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }))
}

fn response(status: u16, kind: &'static str, body: impl Into<Bytes>) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", kind)
        .header("cache-control", "no-cache, no-transform")
        .body(Full::new(body.into()).boxed_unsync())
        .unwrap()
}
fn text(status: u16, message: impl Into<String>) -> Reply {
    response(
        status,
        "text/plain; charset=utf-8",
        format!("{}\n", message.into()),
    )
}
fn rpc_error(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut error = json!({"code":code,"message":message.into()});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc":"2.0","id":id,"error":error})
}
fn json_reply(status: u16, value: Value) -> Reply {
    response(status, "application/json", value.to_string())
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    // Go Header.Get returns the first field, never a last-wins merge.
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}
fn supported_protocol(version: &str) -> bool {
    VERSIONS.contains(&version)
}
fn bearer_ok(got: &str, want: &str) -> bool {
    let Some(prefix) = got.get(..7) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case("Bearer ") {
        return false;
    }
    let got = got[7..].trim();
    if got.len() != want.len() {
        return false;
    }
    // Mac::verify_slice compares the fixed-size authentication tags in constant
    // time, without bringing in a second crypto comparison dependency.
    let mut expected = Hmac::<Sha256>::new_from_slice(b"intercom bearer comparison").unwrap();
    expected.update(want.as_bytes());
    let mut actual = Hmac::<Sha256>::new_from_slice(b"intercom bearer comparison").unwrap();
    actual.update(got.as_bytes());
    actual
        .verify_slice(&expected.finalize().into_bytes())
        .is_ok()
}
fn origin_allowed(origin: &str) -> bool {
    origin.is_empty()
        || reqwest::Url::parse(origin).is_ok_and(|url| {
            matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
            )
        })
}
fn localhost_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.split_once(']').map(|p| p.0))
        .unwrap_or_else(|| host.split(':').next().unwrap_or(""));
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
fn accepts(headers: &HeaderMap) -> bool {
    let mut json = false;
    let mut sse = false;
    for value in headers.get_all("accept") {
        if let Ok(value) = value.to_str() {
            for media in value.split(',') {
                match media
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "application/json" | "application/*" => json = true,
                    "text/event-stream" | "text/*" => sse = true,
                    "*/*" => {
                        json = true;
                        sse = true;
                    }
                    _ => {}
                }
            }
        }
    }
    json && sse
}

const FREE_AUTH_FAILURES: u32 = 5;
const MAX_AUTH_LOCKOUT: Duration = Duration::from_secs(60);
const AUTH_FAILURE_MEMORY: Duration = Duration::from_secs(600);
/// Per-peer failed-bearer backoff: after a few wrong tokens a peer is refused
/// without a token check for a doubling interval, capped at a minute. Loopback
/// peers share one address across local users, so throttling them would let
/// one user lock out another's clients; a 256-bit token needs no throttle there.
#[derive(Default)]
struct AuthThrottle {
    failures: std::sync::Mutex<std::collections::HashMap<IpAddr, (u32, Instant)>>,
}
impl AuthThrottle {
    fn lockout(failures: u32) -> Duration {
        if failures < FREE_AUTH_FAILURES {
            return Duration::ZERO;
        }
        Duration::from_secs(1u64 << (failures - FREE_AUTH_FAILURES).min(6)).min(MAX_AUTH_LOCKOUT)
    }
    fn retry_after(&self, peer: IpAddr, now: Instant) -> Option<Duration> {
        if peer.is_loopback() {
            return None;
        }
        let failures = self.failures.lock().unwrap();
        let (count, last) = failures.get(&peer)?;
        let until = *last + Self::lockout(*count);
        (until > now).then(|| until - now)
    }
    fn failed(&self, peer: IpAddr, now: Instant) {
        if peer.is_loopback() {
            return;
        }
        let mut failures = self.failures.lock().unwrap();
        if failures.len() >= 4096 {
            failures.retain(|_, (_, last)| {
                now.saturating_duration_since(*last) < AUTH_FAILURE_MEMORY
            });
        }
        let entry = failures.entry(peer).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= AUTH_FAILURE_MEMORY {
            entry.0 = 0;
        }
        *entry = (entry.0.saturating_add(1), now);
    }
    fn succeeded(&self, peer: IpAddr) {
        self.failures.lock().unwrap().remove(&peer);
    }
}

async fn handle(
    rt: Shared,
    throttle: &AuthThrottle,
    local: SocketAddr,
    peer: IpAddr,
    req: Request<Incoming>,
) -> Reply {
    if req.uri().path() != "/mcp" && !req.uri().path().starts_with("/mcp/") {
        return text(404, "404 page not found");
    }
    if let Some(wait) = throttle.retry_after(peer, Instant::now()) {
        let mut reply = text(429, "too many failed authorization attempts");
        let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        reply
            .headers_mut()
            .insert("retry-after", seconds.to_string().parse().unwrap());
        return reply;
    }
    if !bearer_ok(header(req.headers(), "authorization"), rt.cfg.mcp_token.trim()) {
        throttle.failed(peer, Instant::now());
        return json_reply(401, rpc_error(Value::Null, -32000, "unauthorized", None));
    }
    throttle.succeeded(peer);
    if !origin_allowed(header(req.headers(), "origin")) {
        return text(403, "forbidden origin");
    }
    if local.ip().is_loopback() && !localhost_host(header(req.headers(), "host")) {
        return text(403, "Forbidden: invalid Host header");
    }
    if req.method() != Method::POST {
        let mut reply = text(405, "Method Not Allowed");
        reply.headers_mut().insert("allow", "POST".parse().unwrap());
        return reply;
    }
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;
    let raw = match tokio::time::timeout(
        Duration::from_secs(30),
        Limited::new(body, MAX_BODY).collect(),
    )
    .await
    {
        Ok(Ok(body)) => body.to_bytes(),
        Ok(Err(error)) => {
            let oversized = error.is::<http_body_util::LengthLimitError>();
            let mut reply = text(
                if oversized { 413 } else { 400 },
                if oversized {
                    format!("request body exceeds {MAX_BODY} bytes")
                } else {
                    "failed to read body".into()
                },
            );
            reply
                .headers_mut()
                .insert("connection", "close".parse().unwrap());
            return reply;
        }
        Err(_) => {
            let mut reply = text(408, "request body timeout");
            reply
                .headers_mut()
                .insert("connection", "close".parse().unwrap());
            return reply;
        }
    };
    let version_header = header(headers, "mcp-protocol-version");
    let parsed = serde_json::from_slice::<Value>(&raw);
    let id = parsed
        .as_ref()
        .ok()
        .and_then(|v| v.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    // The Go wrapper intercepts unsupported versions before the SDK's
    // content negotiation and request parsing.
    if !version_header.is_empty() && !supported_protocol(version_header) {
        return json_reply(
            400,
            rpc_error(
                id,
                -32022,
                format!("protocol version {version_header:?} is not supported by this server"),
                Some(json!({"supported":VERSIONS,"requested":version_header})),
            ),
        );
    }
    if !header(headers, "content-type")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return text(415, "Content-Type must be 'application/json'");
    }
    if !accepts(headers) {
        return text(
            400,
            "Accept must contain both 'application/json' and 'text/event-stream'",
        );
    }
    if headers.contains_key("last-event-id") {
        return text(400, "can't send Last-Event-ID for POST request");
    }
    let value = match parsed {
        Ok(value) => value,
        Err(_) => return text(400, "malformed payload: invalid JSON"),
    };
    let version = if version_header.is_empty() {
        "2025-03-26"
    } else {
        version_header
    };
    let batch = value.is_array();
    if batch && version >= "2025-06-18" {
        return text(
            400,
            "JSON-RPC batching is not supported in 2025-06-18 and later",
        );
    }
    let messages: Vec<&Value> = if let Some(array) = value.as_array() {
        if array.is_empty() {
            return text(400, "empty JSON-RPC batch");
        }
        array.iter().collect()
    } else {
        vec![&value]
    };
    let mut ids = std::collections::HashSet::new();
    for message in &messages {
        if let Err(reply) = preflight(message, version, headers, !batch) {
            return reply;
        }
        if message.get("method").is_some()
            && !message["id"].is_null()
            && !ids.insert(message["id"].to_string())
        {
            return json_reply(
                400,
                rpc_error(
                    message["id"].clone(),
                    -32600,
                    "duplicate in-flight request ID",
                    None,
                ),
            );
        }
    }
    if !batch && str_field(&value, "method") == "subscriptions/listen" {
        return listen(rt, &value, version >= MODERN);
    }
    let mut replies = Vec::new();
    let mut status = 200;
    for message in messages {
        if message.get("method").is_none() || message["id"].is_null() {
            continue;
        }
        let reply = rpc(&rt, message, version >= MODERN).await;
        let code = reply["error"]["code"].as_i64().unwrap_or(0);
        if code == -32601 {
            status = 404;
        } else if version >= MODERN && matches!(code, -32602 | -32021 | -32022) {
            status = 400;
        }
        replies.push(reply);
    }
    if replies.is_empty() {
        return response(202, "application/json", Bytes::new());
    }
    // JSON responses are a standard Streamable HTTP response shape. Unlike
    // SSE, they allow the Go wrapper's method-not-found status correction.
    json_reply(
        status,
        if batch {
            json!(replies)
        } else {
            replies.remove(0)
        },
    )
}

#[allow(clippy::result_large_err)]
fn preflight(req: &Value, version: &str, headers: &HeaderMap, single: bool) -> Result<(), Reply> {
    if !req.is_object()
        || str_field(req, "jsonrpc") != "2.0"
        || req
            .get("id")
            .is_some_and(|id| !id.is_null() && !id.is_string() && id.as_i64().is_none())
    {
        return Err(text(400, "malformed JSON-RPC message"));
    }
    let Some(method) = req.get("method") else {
        if req.get("id").is_none() || (req.get("result").is_some() == req.get("error").is_some()) {
            return Err(text(400, "malformed JSON-RPC response"));
        }
        return Ok(());
    };
    let Some(method) = method.as_str() else {
        return Err(text(400, "invalid JSON-RPC method"));
    };
    let call = !req["id"].is_null();
    let (notification, required_params) = match method {
        "initialize"
        | "tools/call"
        | "prompts/get"
        | "resources/read"
        | "logging/setLevel"
        | "resources/subscribe"
        | "resources/unsubscribe"
        | "subscriptions/listen" => (false, true),
        "ping"
        | "tools/list"
        | "server/discover"
        | "prompts/list"
        | "resources/list"
        | "resources/templates/list" => (false, false),
        "notifications/initialized"
        | "notifications/cancelled"
        | "notifications/roots/list_changed" => (true, false),
        "notifications/progress" => (true, true),
        _ => {
            return Err(if version >= MODERN && call {
                json_reply(
                    404,
                    rpc_error(
                        req["id"].clone(),
                        -32601,
                        format!("method not found: {method:?} unsupported"),
                        None,
                    ),
                )
            } else {
                text(400, format!("method not found: {method:?} unsupported"))
            })
        }
    };
    if notification == call {
        return Err(text(400, "unexpected or missing JSON-RPC id"));
    }
    if required_params && req.get("params").is_none() {
        return Err(text(400, "missing required params"));
    }
    let meta_version = str_field(&req["params"]["_meta"], META_VERSION);
    if (call && version >= MODERN) || !meta_version.is_empty() {
        let header_version = header(headers, "mcp-protocol-version");
        let (code, error) = if header_version.is_empty() {
            (-32020, format!("MCP-Protocol-Version header is required for requests carrying {META_VERSION:?}"))
        } else if meta_version.is_empty() {
            (
                -32602,
                format!("missing or invalid _meta field {META_VERSION:?}"),
            )
        } else if header_version != meta_version {
            (-32020, format!("MCP-Protocol-Version header {header_version:?} does not match request {META_VERSION} {meta_version:?}"))
        } else {
            (0, String::new())
        };
        if code != 0 {
            return Err(json_reply(
                400,
                rpc_error(req["id"].clone(), code, error, None),
            ));
        }
    }
    if single && version >= MODERN {
        let method_header = header(headers, "mcp-method");
        if method_header.is_empty() || method_header != method {
            return Err(json_reply(
                400,
                rpc_error(
                    req["id"].clone(),
                    -32020,
                    "missing or mismatched Mcp-Method header",
                    None,
                ),
            ));
        }
        if matches!(method, "tools/call" | "prompts/get" | "resources/read") {
            let name = str_field(
                &req["params"],
                if method == "resources/read" {
                    "uri"
                } else {
                    "name"
                },
            );
            let name_header = header(headers, "mcp-name");
            if name_header.is_empty() || name_header != name {
                return Err(json_reply(
                    400,
                    rpc_error(
                        req["id"].clone(),
                        -32020,
                        "missing or mismatched Mcp-Name header",
                        None,
                    ),
                ));
            }
        }
        // These tools have no x-mcp-header annotations. Extra Mcp-Param-*
        // fields therefore have no defined bindings and are ignored by Go.
    }
    Ok(())
}

fn server_info() -> Value {
    json!({"name":"agent-intercom","version":"0.1.0"})
}
fn capabilities() -> Value {
    json!({"logging":{},"tools":{"listChanged":true}})
}
fn modern_result(result: &mut Value) {
    result["_meta"]["io.modelcontextprotocol/serverInfo"] = server_info();
    result["resultType"] = json!("complete");
}
fn invalid_params(req: &Value, message: impl Into<String>) -> Value {
    rpc_error(req["id"].clone(), -32602, message, None)
}
async fn rpc(rt: &Shared, req: &Value, modern: bool) -> Value {
    let method = str_field(req, "method");
    let params = &req["params"];
    let meta = &params["_meta"];
    if modern {
        if !meta[META_CAPS].is_object() {
            return invalid_params(req, format!("missing or invalid _meta field {META_CAPS:?}"));
        }
        if !meta[META_INFO].is_null() && !meta[META_INFO].is_object() {
            return invalid_params(req, format!("invalid _meta field {META_INFO:?}"));
        }
        if matches!(
            method,
            "initialize"
                | "ping"
                | "logging/setLevel"
                | "resources/subscribe"
                | "resources/unsubscribe"
        ) {
            return rpc_error(
                req["id"].clone(),
                -32601,
                format!("{method:?} is not supported in the new protocol"),
                None,
            );
        }
    }
    if !params.is_null() && !params.is_object() {
        return invalid_params(req, "params must be an object");
    }
    let mut result = match method {
        "initialize" => {
            if !params.is_object() {
                return invalid_params(req, "missing required params");
            }
            if params
                .get("protocolVersion")
                .is_some_and(|v| !v.is_string())
                || params
                    .get("clientInfo")
                    .is_some_and(|v| !v.is_null() && !v.is_object())
                || params
                    .get("capabilities")
                    .is_some_and(|v| !v.is_null() && !v.is_object())
            {
                return invalid_params(req, "invalid initialize parameters");
            }
            let requested = str_field(params, "protocolVersion");
            json!({"protocolVersion":if supported_protocol(requested) && requested < MODERN {requested}else{"2025-11-25"},
                "capabilities":capabilities(),"serverInfo":server_info()})
        }
        "server/discover" => {
            if !modern {
                return rpc_error(
                    req["id"].clone(),
                    -32601,
                    "server/discover is only supported in protocol version >= 2026-07-28",
                    None,
                );
            }
            json!({"supportedVersions":VERSIONS,"capabilities":capabilities(),"ttlMs":0,"cacheScope":"public"})
        }
        "ping" => json!({}),
        "tools/list" | "prompts/list" | "resources/list" | "resources/templates/list" => {
            if params.get("cursor").is_some_and(|v| !v.is_string()) {
                return invalid_params(req, "cursor must be a string");
            }
            if !str_field(params, "cursor").is_empty() {
                return invalid_params(req, "invalid cursor");
            }
            match method {
                "tools/list" => json!({"tools":tools(),"ttlMs":0,"cacheScope":"public"}),
                "prompts/list" => json!({"prompts":[],"ttlMs":0,"cacheScope":"public"}),
                "resources/list" => json!({"resources":[],"ttlMs":0,"cacheScope":"public"}),
                _ => json!({"resourceTemplates":[],"ttlMs":0,"cacheScope":"public"}),
            }
        }
        "tools/call" => {
            let name = str_field(params, "name");
            if !params["name"].is_string() {
                return invalid_params(req, "name must be a string");
            }
            let Some(tool) = tools().into_iter().find(|tool| tool["name"] == name) else {
                return invalid_params(req, format!("unknown tool {name:?}"));
            };
            let empty = json!({});
            let args = params.get("arguments").unwrap_or(&empty);
            if let Err(error) = validate_arguments(&tool, args) {
                return tool_response(req, Err(error), modern);
            }
            return tool_response(req, call_tool(rt, name, args).await, modern);
        }
        "prompts/get" => {
            return invalid_params(
                req,
                format!("unknown prompt {:?}", str_field(params, "name")),
            )
        }
        "resources/read" => {
            return rpc_error(
                req["id"].clone(),
                -32002,
                format!("resource not found: {:?}", str_field(params, "uri")),
                None,
            )
        }
        "resources/subscribe" | "resources/unsubscribe" => {
            return rpc_error(
                req["id"].clone(),
                -32601,
                "server does not support resource subscriptions",
                None,
            )
        }
        "logging/setLevel" => {
            if ![
                "debug",
                "info",
                "notice",
                "warning",
                "error",
                "critical",
                "alert",
                "emergency",
            ]
            .contains(&str_field(params, "level"))
            {
                return invalid_params(req, "invalid log level");
            }
            json!({})
        }
        _ => return rpc_error(req["id"].clone(), -32601, "method not found", None),
    };
    if modern {
        modern_result(&mut result);
    }
    json!({"jsonrpc":"2.0","id":req["id"],"result":result})
}

fn validate_arguments(tool: &Value, args: &Value) -> Result<(), String> {
    let Some(args) = args.as_object() else {
        return Err("validating arguments: expected object".into());
    };
    let schema = &tool["inputSchema"];
    for field in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !args.contains_key(field) {
            return Err(format!(
                "validating arguments: missing required property {field:?}"
            ));
        }
    }
    let properties = schema["properties"].as_object().unwrap();
    for (key, value) in args {
        if !properties.contains_key(key) {
            return Err(format!(
                "validating arguments: additional property {key:?} is not allowed"
            ));
        }
        if !value.is_string() {
            return Err(format!(
                "validating arguments: property {key:?} must be a string"
            ));
        }
    }
    Ok(())
}
fn tool_response(req: &Value, result: Result<Value, String>, modern: bool) -> Value {
    let mut result = match result {
        Ok(value) => {
            let failed = !str_field(&value, "messageId").is_empty()
                && matches!(str_field(&value, "status"), "unknown" | "unavailable")
                && !str_field(&value, "error").is_empty();
            let mut result = json!({"content":[{"type":"text","text":value.to_string()}]});
            if failed {
                result["isError"] = json!(true);
            }
            result
        }
        Err(error) => json!({"isError":true,"content":[{"type":"text","text":error}]}),
    };
    if modern {
        modern_result(&mut result);
    }
    json!({"jsonrpc":"2.0","id":req["id"],"result":result})
}

async fn resolve_to(rt: &Shared, to: &str) -> Result<String, String> {
    let to = to.trim();
    if to.is_empty() {
        return Err("to required".into());
    }
    if to.contains('/') {
        return Ok(to.into());
    }
    let rows = rt.directory().await;
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| str_field(row, "id") == to || str_field(row, "name") == to)
        .collect();
    match matches.as_slice() {
        [] => Err(format!("unknown session {to:?}")),
        [row] => Ok(str_field(row, "id").into()),
        _ => Err(format!("session {to:?} is ambiguous")),
    }
}
// deferred: binding `from` to a caller credential | impact: any holder of the one MCP bearer
// token (this user's MCP clients) can speak as any live local agent session, never as a
// remote, webhook or unknown id | trigger: MCP clients can present a per-session credential
async fn local_sender(rt: &Shared, from: &str) -> Result<(), String> {
    let rows = rt.directory().await;
    if rows
        .iter()
        .any(|row| str_field(row, "id") == from && rt.local_agent(row))
    {
        Ok(())
    } else {
        Err(format!("from {from:?} is not a local intercom session"))
    }
}
async fn call_tool(rt: &Shared, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "list_sessions" => {
            let query = str_field(args, "query").to_lowercase();
            let rows: Vec<_> = rt
                .directory()
                .await
                .into_iter()
                .filter(|row| {
                    query.is_empty()
                        || ["id", "name", "machine_id", "machine_name", "cwd"]
                            .iter()
                            .any(|key| str_field(row, key).to_lowercase().contains(&query))
                })
                .map(super::public_row)
                .collect();
            Ok(json!(rows))
        }
        "send" => {
            let from = str_field(args, "from").trim();
            if from.is_empty() {
                return Err(
                    "from required: pass your intercom session id (codex/<threadId>)".into(),
                );
            }
            local_sender(rt, from).await?;
            let to = resolve_to(rt, str_field(args, "to")).await?;
            let value = rt
                .send("", &to, str_field(args, "message"), from, "")
                .await?;
            let status = match str_field(&value, "status") {
                "" => "accepted",
                status => status,
            };
            let mut result = json!({"messageId":value["message_id"],"status":status});
            if !str_field(&value, "error").is_empty() {
                result["error"] = value["error"].clone();
            }
            Ok(result)
        }
        "message_status" => {
            let id = str_field(args, "message_id").trim();
            if id.is_empty() {
                return Err("message_id required".into());
            }
            let mut state = rt.state.lock().unwrap();
            state.prune_deliveries(rt.clock.now_ms());
            state.messages.get(id).filter(|_| state.history.contains(id)).map(|delivery| {
                json!({"messageId":id,"status":delivery.status,"updatedAt":rfc3339(delivery.updated_at)})
            }).ok_or_else(|| "unknown message id".into())
        }
        "notify_when_idle" => {
            let from = str_field(args, "from").trim();
            if from.is_empty() {
                return Err(
                    "from required: pass your intercom session id (codex/<threadId>)".into(),
                );
            }
            let to = resolve_to(rt, str_field(args, "to")).await?;
            let value = rt.subscribe(from, &to).await?;
            Ok(
                json!({"subscriptionId":value["subscription_id"],"status":value["status"],"delivery_mode":value["delivery_mode"],"target":to}),
            )
        }
        "idle_status" => {
            let id = str_field(args, "subscription_id").trim();
            if id.is_empty() {
                return Err("subscription_id required".into());
            }
            let state = rt.state.lock().unwrap();
            state.subscriptions.get(id).map(|subscription| json!({
                "subscriptionId":id,"status":subscription.status,"target":subscription.target,"delivery_mode":subscription.mode
            })).ok_or_else(|| "unknown subscription id".into())
        }
        "name_session" => {
            let row = rt.resolve(str_field(args, "session")).await?;
            let name = str_field(args, "name");
            if name.is_empty() {
                return Err("name required".into());
            }
            rt.native.rename(&row, name).await?;
            rt.state
                .lock()
                .unwrap()
                .aliases
                .insert(str_field(&row, "id").into(), name.into());
            Ok(json!({"ok":true}))
        }
        _ => Err("unknown tool".into()),
    }
}

// Gregorian civil-date conversion from Unix days, kept local to the one MCP
// wire field that uses RFC3339 rather than intercom's millisecond timestamps.
fn rfc3339(ms: u64) -> String {
    let seconds = ms / 1000;
    let z = (seconds / 86400) as i64 + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        ms % 1000
    )
}

fn listen(rt: Shared, req: &Value, modern: bool) -> Reply {
    let params = &req["params"];
    if !params["notifications"].is_object() {
        return json_reply(
            if modern { 400 } else { 200 },
            invalid_params(req, "missing required notifications field"),
        );
    }
    if modern && !params["_meta"][META_CAPS].is_object() {
        return json_reply(
            400,
            invalid_params(req, format!("missing or invalid _meta field {META_CAPS:?}")),
        );
    }
    let active = params["notifications"]["toolsListChanged"]
        .as_bool()
        .unwrap_or(false);
    let id = req["id"].clone();
    let agreed = if active {
        json!({"toolsListChanged":true})
    } else {
        json!({})
    };
    let ack = json!({"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged",
        "params":{"_meta":{"io.modelcontextprotocol/subscriptionId":id},"notifications":agreed}});
    let mut result = json!({"_meta":{"io.modelcontextprotocol/subscriptionId":id}});
    if modern {
        modern_result(&mut result);
    }
    let done = json!({"jsonrpc":"2.0","id":id,"result":result});
    let initial = format!("event: message\ndata: {ack}\n\n");
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(2);
    let mut stop = rt.stop.subscribe();
    tokio::spawn(async move {
        if tx.send(initial.into()).await.is_err() {
            return;
        }
        if active {
            tokio::select! { _ = stop.changed() => {}, _ = tx.closed() => return }
        }
        let _ = tx
            .send(format!("event: message\ndata: {done}\n\n").into())
            .await;
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(Frame::data(bytes)), rx))
    });
    response_stream(StreamBody::new(stream).boxed_unsync())
}
fn response_stream(body: Body) -> Reply {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache, no-transform")
        .body(body)
        .unwrap()
}
fn tools() -> Vec<Value> {
    type ToolDefinition = (
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str, bool)],
    );
    let definitions: [ToolDefinition; 6] = [
        ("send", "Send a message to another agent session over intercom", &[
            ("to", "target session name or id", true),
            ("message", "message text", true),
            ("from", "sender intercom session id (required)", false),
        ]),
        ("list_sessions", "List agent sessions visible over intercom", &[
            ("query", "optional filter over id, name, machine, cwd", false),
        ]),
        ("name_session", "Assign a display name to a session", &[
            ("session", "session id or name", true),
            ("name", "display name to assign", true),
        ]),
        ("message_status", "Look up the latest delivery status of a message id (accepted, queued, unavailable, delivered, unknown, read, held, denied, refused, expired, dropped)", &[
            ("message_id", "message id returned by send", true),
        ]),
        ("notify_when_idle", "Subscribe once to an agent becoming idle. Returns delivery_mode: pi_push for an upgraded Pi watcher, otherwise query_only (use idle_status); does not inject a turn or native notification into Codex or MCP Claude.", &[
            ("from", "watcher intercom session id (e.g. codex/<threadId>)", true),
            ("to", "target session name or id", true),
        ]),
        ("idle_status", "Look up the latest status of an idle notification subscription", &[
            ("subscription_id", "subscription id returned by notify_when_idle", true),
        ]),
    ];
    let mut tools: Vec<_> = definitions
        .into_iter()
        .map(|(name, description, fields)| {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for &(field, description, needed) in fields {
                properties.insert(
                    field.into(),
                    json!({"type":"string","description":description}),
                );
                if needed {
                    required.push(field);
                }
            }
            let mut schema =
                json!({"type":"object","properties":properties,"additionalProperties":false});
            if !required.is_empty() {
                schema["required"] = json!(required);
            }
            json!({"name":name,"description":description,"inputSchema":schema})
        })
        .collect();
    tools.sort_by(|a, b| str_field(a, "name").cmp(str_field(b, "name")));
    tools
}
fn inspect_config(c: &Config) -> Result<(String, String), String> {
    let bytes = std::fs::read(c.state_dir.join("inspect.json"))
        .map_err(|e| format!("inspection discovery: {e}"))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let url = str_field(&v, "url");
    let token = str_field(&v, "token");
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    if !parsed
        .host_str()
        .is_some_and(|host| localhost_host(&format!("{host}:0")))
        || parsed.scheme() != "http"
        || token.is_empty()
    {
        return Err("invalid local inspection capability".into());
    }
    Ok((url.into(), token.into()))
}
pub async fn inspect_message(c: &Config, id: &str) -> Result<(), String> {
    let (url, token) = inspect_config(c)?;
    let mut url = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|_| "invalid URL")?
        .extend(["v1", "messages", id]);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let v: Value = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    super::cli::print_json(&v);
    Ok(())
}
pub async fn inspect_events(c: &Config, mut since: u64, follow: bool) -> Result<(), String> {
    loop {
        let (url, token) = inspect_config(c)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let v: Value = client
            .get(format!("{url}/v1/events"))
            .query(&[("since", since.to_string()), ("limit", "1000".into())])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        for event in v.as_array().into_iter().flatten() {
            let seq = event["seq"].as_u64().ok_or("invalid inspection sequence")?;
            if seq <= since {
                return Err("inspection sequence did not increase".into());
            }
            since = seq;
            super::cli::print_json(event);
        }
        if !follow {
            return Ok(());
        }
        tokio::select! {_=tokio::signal::ctrl_c()=>return Ok(()),_=tokio::time::sleep(Duration::from_millis(250))=>{}}
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn modern_request(method: &str) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"_meta":{
            META_VERSION:MODERN, META_CAPS:{}
        }}})
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_but_token_is_not() {
        assert!(bearer_ok("bEaReR   secret  ", "secret"));
        assert!(!bearer_ok("Bearer SECRET", "secret"));
        assert!(!bearer_ok("Bearer secrex", "secret"));
        assert!(!bearer_ok("Basic secret", "secret"));
        assert!(!bearer_ok("\u{80}\u{80}\u{80}\u{80}", "secret"));
    }

    #[test]
    fn repeated_bad_tokens_lock_out_only_that_peer_with_growing_backoff() {
        let throttle = AuthThrottle::default();
        let attacker: IpAddr = "192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        let start = Instant::now();
        for attempt in 0..FREE_AUTH_FAILURES {
            assert_eq!(throttle.retry_after(attacker, start), None, "{attempt}");
            throttle.failed(attacker, start);
        }
        assert_eq!(
            throttle.retry_after(attacker, start),
            Some(Duration::from_secs(1))
        );
        assert_eq!(throttle.retry_after(other, start), None);
        let later = start + Duration::from_secs(1);
        assert_eq!(throttle.retry_after(attacker, later), None);
        throttle.failed(attacker, later);
        assert_eq!(
            throttle.retry_after(attacker, later),
            Some(Duration::from_secs(2))
        );
        for _ in 0..20 {
            throttle.failed(attacker, later);
        }
        assert_eq!(throttle.retry_after(attacker, later), Some(MAX_AUTH_LOCKOUT));
        let forgotten = later + AUTH_FAILURE_MEMORY;
        throttle.failed(attacker, forgotten);
        assert_eq!(throttle.retry_after(attacker, forgotten), None);
        throttle.succeeded(attacker);
        assert!(throttle.failures.lock().unwrap().is_empty());
    }

    #[test]
    fn loopback_peers_are_never_locked_out_by_another_local_user() {
        let throttle = AuthThrottle::default();
        let start = Instant::now();
        for peer in ["127.0.0.1", "::1"] {
            let peer: IpAddr = peer.parse().unwrap();
            for _ in 0..FREE_AUTH_FAILURES * 4 {
                throttle.failed(peer, start);
            }
            assert_eq!(throttle.retry_after(peer, start), None);
        }
    }

    #[test]
    fn duplicate_headers_use_first_value() {
        let mut headers = HeaderMap::new();
        headers.append("authorization", "Bearer wrong".parse().unwrap());
        headers.append("authorization", "Bearer secret".parse().unwrap());
        assert!(!bearer_ok(header(&headers, "authorization"), "secret"));
        headers.append("mcp-protocol-version", "1900-01-01".parse().unwrap());
        headers.append("mcp-protocol-version", MODERN.parse().unwrap());
        assert!(!supported_protocol(header(
            &headers,
            "mcp-protocol-version"
        )));
    }

    #[test]
    fn accepts_requires_json_and_stream_and_allows_go_wildcards() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "application/json".parse().unwrap());
        assert!(!accepts(&headers));
        headers.append(
            "accept",
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        assert!(accepts(&headers));
        headers.insert("accept", "*/*".parse().unwrap());
        assert!(accepts(&headers));
    }

    #[test]
    fn local_origin_and_host_policy_covers_ipv6() {
        for origin in [
            "",
            "http://localhost:3000",
            "http://127.0.0.1",
            "http://[::1]:1234",
        ] {
            assert!(origin_allowed(origin), "{origin}");
        }
        for origin in [
            "null",
            "http://evil.test",
            "http://localhost.evil.test",
            "http://127.0.0.2",
        ] {
            assert!(!origin_allowed(origin), "{origin}");
        }
        assert!(localhost_host("[::1]:8080"));
        assert!(localhost_host("127.0.0.2:8080"));
        assert!(!localhost_host("localhost.evil.test:8080"));
    }

    #[tokio::test]
    async fn modern_headers_and_meta_must_match() {
        let req = modern_request("tools/list");
        let mut headers = HeaderMap::new();
        headers.insert("mcp-protocol-version", MODERN.parse().unwrap());
        let reply = preflight(&req, MODERN, &headers, true).err().unwrap();
        assert_eq!(reply.status(), 400);
        let body = reply.into_body().collect().await.unwrap().to_bytes();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["code"], -32020);
        headers.insert("mcp-method", "tools/list".parse().unwrap());
        assert!(preflight(&req, MODERN, &headers, true).is_ok());
        headers.insert("mcp-protocol-version", "2025-11-25".parse().unwrap());
        assert!(preflight(&req, "2025-11-25", &headers, true).is_err());
        headers.remove("mcp-protocol-version");
        assert!(preflight(&req, "2025-03-26", &headers, true).is_err());
    }

    #[test]
    fn notifications_require_no_id_and_calls_require_id() {
        let headers = HeaderMap::new();
        let notification = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(preflight(&notification, "2025-06-18", &headers, true).is_ok());
        let call = json!({"jsonrpc":"2.0","id":1,"method":"notifications/initialized"});
        assert!(preflight(&call, "2025-06-18", &headers, true).is_err());
        let missing_id = json!({"jsonrpc":"2.0","method":"tools/list"});
        assert!(preflight(&missing_id, "2025-06-18", &headers, true).is_err());
    }

    #[test]
    fn status_timestamps_use_rfc3339_milliseconds() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(951827696123), "2000-02-29T12:34:56.123Z");
        assert_eq!(rfc3339(1791964800000), "2026-10-14T08:00:00.000Z");
    }

    #[test]
    fn tool_schema_checks_types_required_and_unknown_arguments() {
        let tools = tools();
        let send = tools.iter().find(|t| t["name"] == "send").unwrap();
        assert!(validate_arguments(send, &json!({"to":"pi/target","message":"hi"})).is_ok());
        for args in [
            json!({"to":"pi/target"}),
            json!({"to":1,"message":"hi"}),
            json!({"to":"a","message":"hi","unknown":"x"}),
            Value::Null,
        ] {
            assert!(validate_arguments(send, &args).is_err());
        }
    }

    #[test]
    fn modern_tool_errors_remain_tool_results_with_server_identity() {
        let req = modern_request("tools/call");
        let reply = tool_response(
            &req,
            Ok(json!({"messageId":"m","status":"unknown","error":"lost reply"})),
            true,
        );
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["resultType"], "complete");
        assert_eq!(
            reply["result"]["_meta"]["io.modelcontextprotocol/serverInfo"],
            server_info()
        );
        assert!(reply.get("error").is_none());
    }

    #[test]
    fn send_schema_matches_go_optional_from() {
        let send = tools()
            .into_iter()
            .find(|tool| tool["name"] == "send")
            .unwrap();
        assert_eq!(send["inputSchema"]["required"], json!(["to", "message"]));
        assert_eq!(send["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            send["description"],
            "Send a message to another agent session over intercom"
        );
    }

    #[test]
    fn inspection_discovery_accepts_bracketed_ipv6_loopback_only() {
        let dir = std::env::temp_dir().join(format!("intercom-inspect-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let cfg = Config {
            state_dir: dir.clone(),
            ..Config::default()
        };
        let check = |url: &str| {
            std::fs::write(
                dir.join("inspect.json"),
                json!({"url":url,"token":"t"}).to_string(),
            )
            .unwrap();
            inspect_config(&cfg)
        };
        for url in [
            "http://[::1]:4100",
            "http://127.0.0.1:4100",
            "http://localhost:4100",
        ] {
            assert_eq!(check(url).unwrap().0, url);
        }
        for url in [
            "http://[2001:db8::1]:4100",
            "http://192.0.2.1:4100",
            "https://[::1]:4100",
        ] {
            assert!(check(url).is_err(), "{url}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
