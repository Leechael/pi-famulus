#[cfg(test)]
mod tests;
mod tls;
use super::{
    config::{Config, Identity},
    str_field, Runtime, Shared,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

enum OrderedIncoming {
    Control(Value),
    Reply(Value, oneshot::Sender<Value>),
}
const IDLE_CONTROL_QUEUE: usize = 128;
/// Keeps replies ordered behind earlier idle controls without letting a
/// control backlog reject them: only controls count against the cap, and
/// replies are already bounded by the pending-call table.
struct OrderedQueue {
    tx: mpsc::UnboundedSender<OrderedIncoming>,
    controls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
struct OrderedReceiver {
    rx: mpsc::UnboundedReceiver<OrderedIncoming>,
    controls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
fn ordered_queue() -> (OrderedQueue, OrderedReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let controls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (
        OrderedQueue {
            tx,
            controls: controls.clone(),
        },
        OrderedReceiver { rx, controls },
    )
}
impl OrderedQueue {
    fn control(&self, frame: Value) -> Result<(), ()> {
        use std::sync::atomic::Ordering;
        if self.controls.fetch_add(1, Ordering::SeqCst) >= IDLE_CONTROL_QUEUE {
            self.controls.fetch_sub(1, Ordering::SeqCst);
            return Err(());
        }
        let _ = self.tx.send(OrderedIncoming::Control(frame));
        Ok(())
    }
    fn reply(&self, frame: Value, reply: oneshot::Sender<Value>) {
        let _ = self.tx.send(OrderedIncoming::Reply(frame, reply));
    }
}
impl OrderedReceiver {
    async fn recv(&mut self) -> Option<OrderedIncoming> {
        let frame = self.rx.recv().await?;
        if matches!(frame, OrderedIncoming::Control(_)) {
            self.controls
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        Some(frame)
    }
}
struct PendingCall<'a> {
    runtime: &'a Runtime,
    id: String,
}
impl Drop for PendingCall<'_> {
    fn drop(&mut self) {
        self.runtime.state.lock().unwrap().pending.remove(&self.id);
    }
}
fn is_control(frame: &Value) -> bool {
    matches!(str_field(frame, "type"), "idle_cancel" | "idle_notice")
}
fn park_controls(state: &mut super::State, mut frames: std::collections::VecDeque<Value>) {
    frames.append(&mut state.control_outbox);
    state.control_outbox = frames;
    while state.control_outbox.len() > 256 {
        state.control_outbox.pop_front();
        eprintln!("intercom relay: outbox full; dropped oldest idle control");
    }
}
pub async fn enqueue_control(rt: &Runtime, frame: Value) -> Result<(), String> {
    if !is_control(&frame) {
        return Err("only idle_cancel and idle_notice may be queued".into());
    }
    loop {
        let mut generation = rt.state.lock().unwrap().relay_generation;
        loop {
            let writer = rt.state.lock().unwrap().relay_tx.clone();
            let Some(writer) = writer else { break };
            if writer.send(frame.clone().into()).await.is_ok() {
                // A torn-down session may have parked controls after the current
                // session drained the outbox at setup; flush them through the
                // healthy writer instead of waiting for the next reconnect.
                let parked = std::mem::take(&mut rt.state.lock().unwrap().control_outbox);
                let mut stranded: std::collections::VecDeque<Value> = Default::default();
                for old in parked {
                    if let Err(error) = writer.send(old.into()).await {
                        stranded.push_back(error.0.value);
                    }
                }
                if !stranded.is_empty() {
                    park_controls(&mut rt.state.lock().unwrap(), stranded);
                }
                return Ok(());
            }
            // The send failed on a stale writer: retry once against the writer of
            // a newer connection before parking the frame for a future session.
            let state = rt.state.lock().unwrap();
            if state.relay_generation == generation {
                break;
            }
            generation = state.relay_generation;
        }
        let mut state = rt.state.lock().unwrap();
        // A healthy session may have installed its writer and drained an empty
        // outbox while this task was descheduled. Never park behind that final
        // drain: retry against the new generation while holding the state lock
        // long enough to make the park-vs-retry decision atomic with setup.
        if state.relay_tx.is_some() && state.relay_generation != generation {
            drop(state);
            continue;
        }
        if state.control_outbox.len() >= 256 {
            state.control_outbox.pop_front();
            eprintln!("intercom relay: outbox full; dropped oldest idle control");
        }
        state.control_outbox.push_back(frame);
        return Ok(());
    }
}
pub struct LegacyActive {
    pub private_id: String,
    pub current_id: String,
    pub origin: String,
    pub target: String,
}
pub async fn send(
    rt: &Runtime,
    origin: &str,
    target: &str,
    body: &str,
    mode: &str,
    mid: &str,
    fence: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> Result<Value, String> {
    let (capability, generation, connected) = {
        let state = rt.state.lock().unwrap();
        (
            state.relay_message_ids,
            state.relay_generation,
            state.relay_tx.is_some(),
        )
    };
    if !connected {
        rt.record_status(mid, "unavailable", false);
        return Ok(
            json!({"message_id":mid,"status":"unavailable","error":"not connected to relay"}),
        );
    }
    if origin.starts_with("claude/") && !capability {
        rt.state.lock().unwrap().forget_delivery(mid);
        return Err(
            "upgrade relay: native Claude sends require send_message_id_v1 correlation support"
                .into(),
        );
    }
    let mut request =
        json!({"type":"send","to":target,"body":body,"reply_to":origin,"from_mode":mode});
    if capability {
        request["message_id"] = json!(mid);
    }
    // Leave headroom for the request id call_on adds.
    if request.to_string().len() + 64 > MAX_FRAME {
        rt.state.lock().unwrap().forget_delivery(mid);
        return Err(format!(
            "message too large for relay: encoded send frame exceeds {MAX_FRAME} bytes"
        ));
    }
    let _gate = if capability {
        None
    } else {
        Some(
            tokio::time::timeout(Duration::from_secs(15), rt.legacy_gate.lock())
                .await
                .map_err(|_| "legacy relay admission timed out".to_string())?,
        )
    };
    let rows = rt.directory().await;
    let snapshot =
        json!({"type":"snapshot","machine_id":rt.machine,"sessions":snapshot_rows(rows)});
    let writer = {
        let mut state = rt.state.lock().unwrap();
        if state.relay_generation != generation {
            return Err("relay connection changed before send".into());
        }
        if !capability {
            state.legacy_active = Some(LegacyActive {
                private_id: mid.into(),
                current_id: mid.into(),
                origin: origin.into(),
                target: target.into(),
            });
        }
        state.relay_tx.clone().ok_or("not connected to relay")?
    };
    if writer.send(snapshot.into()).await.is_err() {
        rt.state.lock().unwrap().legacy_active = None;
        return Err("relay connection changed before send".into());
    }
    let response = call_on(rt, request, generation, fence).await;
    let mut state = rt.state.lock().unwrap();
    let active = if !capability {
        state.legacy_active.take()
    } else {
        None
    };
    match response {
        Ok(value) => {
            let assigned = str_field(&value, "message_id");
            let status = if str_field(&value, "status").is_empty() {
                "accepted"
            } else {
                str_field(&value, "status")
            };
            if assigned.is_empty()
                || capability && assigned != mid
                || super::deliveries::rank(status) == 0
                || str_field(&value, "type") != "send"
            {
                state.forget_delivery(mid);
                return Ok(
                    json!({"message_id":mid,"error":"protocol error: invalid relay send acknowledgement"}),
                );
            }
            if !capability {
                let current = active
                    .as_ref()
                    .map(|a| a.current_id.as_str())
                    .unwrap_or(mid);
                if current != mid && current != assigned {
                    return Ok(
                        json!({"message_id":current,"status":"unknown","error":"legacy relay ACK conflicts with early receipt; outcome unknown"}),
                    );
                }
                if current != assigned {
                    state.rekey_delivery(current, assigned)?;
                }
            }
            if let Some(delivery) = state.messages.get_mut(assigned) {
                delivery.consider(status, true, rt.clock.now_ms());
            }
            Ok(json!({"message_id":assigned,"status":status}))
        }
        Err(error) => {
            let id = active
                .as_ref()
                .map(|a| a.current_id.as_str())
                .unwrap_or(mid);
            let status = if error == "not connected to relay" {
                "unavailable"
            } else {
                "unknown"
            };
            if let Some(delivery) = state.messages.get_mut(id) {
                delivery.consider(status, true, rt.clock.now_ms());
            }
            Ok(json!({"message_id":id,"status":status,"error":error}))
        }
    }
}
pub async fn call(rt: &Runtime, req: Value) -> Result<Value, String> {
    let generation = rt.state.lock().unwrap().relay_generation;
    call_on(rt, req, generation, None).await
}
async fn call_on(
    rt: &Runtime,
    mut req: Value,
    generation: u64,
    fence: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    req["id"] = json!(id);
    let (tx, rx) = oneshot::channel();
    let writer = {
        let mut s = rt.state.lock().unwrap();
        if s.relay_generation != generation {
            return Err("relay connection changed before send".into());
        }
        let writer = s.relay_tx.clone().ok_or("not connected to relay")?;
        s.pending.insert(id.clone(), tx);
        writer
    };
    let _pending = PendingCall {
        runtime: rt,
        id: id.clone(),
    };
    let sent = writer
        .send(super::Push {
            value: req,
            written: None,
            fence,
        })
        .await;
    let result = if sent.is_err() {
        Err("relay send outcome unknown".into())
    } else {
        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(v)) => {
                if str_field(&v, "type") == "error" {
                    Err(format!(
                        "{}: {}",
                        str_field(&v, "code"),
                        str_field(&v, "message")
                    ))
                } else {
                    Ok(v)
                }
            }
            _ => Err("relay send outcome unknown".into()),
        }
    };
    result
}
pub(super) const MAX_FRAME: usize = 1 << 20;
pub(super) const AUTH_RETRY_DELAY: Duration = Duration::from_secs(300);
pub(super) const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub(super) const BACKOFF_MAX: Duration = Duration::from_secs(30);
const HEALTHY_SESSION: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(super) enum TransportError {
    Http { status: u16, reason: String },
    Remote { code: String, reason: String },
    Io(String),
    TlsRecordHeader { prefix: [u8; 5], reason: String },
}
impl TransportError {
    pub(super) fn is_auth(&self) -> bool {
        matches!(self, Self::Http { status: 401, .. })
            || matches!(self,Self::Remote{code,..} if code=="unauthenticated")
    }
    pub(super) fn remote(frame: &Value) -> Self {
        Self::Remote {
            code: str_field(frame, "code").into(),
            reason: str_field(frame, "message").into(),
        }
    }
}
impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http { status, reason } => write!(f, "relay handshake HTTP {status}: {reason}"),
            Self::Remote { code, reason } => write!(f, "{code}: {reason}"),
            Self::Io(reason) | Self::TlsRecordHeader { reason, .. } => f.write_str(reason),
        }?;
        if self.is_auth() {
            write!(f, "; run pi-famulus intercom enroll --server <server> --token <enrollment-token> to enroll or renew this device; the relay requires an enrolled device credential")?;
        }
        Ok(())
    }
}
impl From<String> for TransportError {
    fn from(value: String) -> Self {
        Self::Io(value)
    }
}
impl From<&str> for TransportError {
    fn from(value: &str) -> Self {
        Self::Io(value.into())
    }
}
impl std::error::Error for TransportError {}

pub(super) fn next_backoff(cur: Duration) -> (Duration, Duration) {
    let cur = cur.clamp(BACKOFF_MIN, BACKOFF_MAX);
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let random = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as f64 / u64::MAX as f64;
    (
        Duration::from_secs_f64(cur.as_secs_f64() * (0.75 + random * 0.5)),
        (cur * 2).min(BACKOFF_MAX),
    )
}

struct ObservedTcp {
    stream: tokio::net::TcpStream,
    header: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}
impl std::ops::Deref for ObservedTcp {
    type Target = tokio::net::TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}
impl tokio::io::AsyncRead for ObservedTcp {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = std::pin::Pin::new(&mut self.stream).poll_read(cx, buffer);
        if matches!(result, std::task::Poll::Ready(Ok(()))) {
            let mut header = self.header.lock().unwrap();
            let take = (5 - header.len()).min(buffer.filled().len() - before);
            header.extend_from_slice(&buffer.filled()[before..before + take]);
        }
        result
    }
}
impl tokio::io::AsyncWrite for ObservedTcp {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buffer)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
type Socket = WebSocketStream<MaybeTlsStream<ObservedTcp>>;
type Writer = futures_util::stream::SplitSink<Socket, Message>;

pub(super) struct Conn {
    reader: futures_util::stream::SplitStream<Socket>,
    writer: std::sync::Arc<tokio::sync::Mutex<Writer>>,
    clock: crate::clock::Clock,
    last_read: u64,
    failed: tokio::sync::watch::Receiver<Option<TransportError>>,
    failure: tokio::sync::watch::Sender<Option<TransportError>>,
    ping: Option<tokio::task::JoinHandle<()>>,
}
impl Conn {
    pub(super) async fn connect(
        c: &Config,
        clock: crate::clock::Clock,
    ) -> Result<Self, TransportError> {
        let socket = dial_typed(c).await?;
        let (writer, reader) = socket.split();
        let (failure, failed) = tokio::sync::watch::channel(None);
        Ok(Self {
            reader,
            writer: std::sync::Arc::new(tokio::sync::Mutex::new(writer)),
            last_read: clock.now_ms(),
            clock,
            failed,
            failure,
            ping: None,
        })
    }
    pub(super) async fn register(&mut self, registration: Value) -> Result<Value, TransportError> {
        tokio::time::timeout(Duration::from_secs(15), async {
            self.send(Message::Text(registration.to_string().into()))
                .await?;
            loop {
                match self.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let frame: Value = serde_json::from_str(&text)
                            .map_err(|error| TransportError::Io(error.to_string()))?;
                        match str_field(&frame, "type") {
                            "welcome" => return Ok(frame),
                            "error" => return Err(TransportError::remote(&frame)),
                            kind => {
                                return Err(TransportError::Io(format!(
                                    "unexpected relay handshake frame: {kind}"
                                )))
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => self.send(Message::Pong(payload)).await?,
                    Some(Err(error)) => return Err(error),
                    Some(Ok(Message::Close(_))) | None => {
                        return Err("relay closed before welcome".into())
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| TransportError::Io("relay welcome timed out".into()))?
    }
    pub(super) fn start_ping(&mut self) {
        if self.ping.is_some() {
            return;
        }
        let clock = self.clock.clone();
        let writer = self.writer.clone();
        let failure = self.failure.clone();
        let mut failed = self.failed.clone();
        self.ping = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=failed.changed()=>break,
                    _=clock.sleep("intercom-relay-ping",Duration::from_secs(20))=>{
                        if let Err(error)=write_frame(&writer,&failed,Message::Ping(Vec::new().into())).await {
                            failure.send_replace(Some(error));
                            break;
                        }
                    }
                }
            }
        }));
    }
    pub(super) async fn send(&mut self, message: Message) -> Result<(), TransportError> {
        let result = write_frame(&self.writer, &self.failed, message).await;
        if let Err(error) = &result {
            self.failure.send_replace(Some(error.clone()));
        }
        result
    }
    pub(super) async fn next(&mut self) -> Option<Result<Message, TransportError>> {
        if let Some(error) = self.failed.borrow().clone() {
            return Some(Err(error));
        }
        let remaining =
            60_000u64.saturating_sub(self.clock.now_ms().saturating_sub(self.last_read));
        let frame = tokio::select! {
            biased;
            _=self.failed.changed()=>return self.failed.borrow().clone().map(Err),
            _=self.clock.sleep("intercom-relay-read",Duration::from_millis(remaining))=>{
                let error=TransportError::Io("relay websocket read timed out".into());
                self.failure.send_replace(Some(error.clone()));
                return Some(Err(error));
            },
            frame=self.reader.next()=>frame,
        };
        match frame {
            Some(Ok(frame)) => {
                if matches!(
                    frame,
                    Message::Text(_) | Message::Binary(_) | Message::Ping(_)
                ) {
                    self.last_read = self.clock.now_ms();
                }
                Some(Ok(frame))
            }
            Some(Err(error)) => {
                let error = TransportError::Io(error.to_string());
                self.failure.send_replace(Some(error.clone()));
                Some(Err(error))
            }
            None => None,
        }
    }
}
impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(task) = self.ping.take() {
            task.abort();
        }
    }
}
async fn write_frame(
    writer: &std::sync::Arc<tokio::sync::Mutex<Writer>>,
    failed: &tokio::sync::watch::Receiver<Option<TransportError>>,
    message: Message,
) -> Result<(), TransportError> {
    let mut writer = writer.lock().await;
    if let Some(error) = failed.borrow().clone() {
        return Err(error);
    }
    tokio::time::timeout(Duration::from_secs(10), writer.send(message))
        .await
        .map_err(|_| TransportError::Io("relay websocket write timed out".into()))?
        .map_err(|error| TransportError::Io(error.to_string()))
}
#[cfg(test)]
async fn dial(c: &Config) -> Result<Socket, String> {
    dial_typed(c).await.map_err(|error| error.to_string())
}
async fn dial_typed(c: &Config) -> Result<Socket, TransportError> {
    match dial_attempt(c).await {
        Err(TransportError::TlsRecordHeader { prefix, .. }) if prefix == *b"HTTP/" => {
            Err(TransportError::Io(
                "relay server answered TLS with cleartext HTTP; refusing to send the relay token \
                 unencrypted (configure an http:// or ws:// server URL to opt in)"
                    .into(),
            ))
        }
        result => result,
    }
}
async fn dial_attempt(c: &Config) -> Result<Socket, TransportError> {
    let mut url =
        reqwest::Url::parse(&c.server).map_err(|error| TransportError::Io(error.to_string()))?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return Err("relay URL must use http(s) or ws(s)".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| TransportError::Io("invalid relay URL".into()))?;
    url.set_path("/v1/relay");
    url.set_query(None);
    url.set_fragment(None);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|error| TransportError::Io(error.to_string()))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", c.token)
            .parse()
            .map_err(|_| TransportError::Io("invalid bearer credential".into()))?,
    );
    let header = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = header.clone();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let host = url
            .host_str()
            .ok_or_else(|| TransportError::Io("relay host missing".into()))?
            .trim_matches(['[', ']']);
        // Prepare TLS before connecting so the ClientHello follows the TCP
        // handshake immediately.
        let connector = tls::connector(c.tls_insecure).map_err(TransportError::Io)?;
        let stream = tokio::net::TcpStream::connect((
            host,
            url.port_or_known_default()
                .unwrap_or(if scheme == "wss" { 443 } else { 80 }),
        ))
        .await
        .map_err(|error| TransportError::Io(error.to_string()))?;
        stream
            .set_nodelay(true)
            .map_err(|error| TransportError::Io(error.to_string()))?;
        tokio_tungstenite::client_async_tls_with_config(
            request,
            ObservedTcp {
                stream,
                header: observed,
            },
            Some(
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                    .max_message_size(Some(MAX_FRAME))
                    .max_frame_size(Some(MAX_FRAME)),
            ),
            Some(connector),
        )
        .await
        .map(|(socket, _)| socket)
        .map_err(|error| {
            let record_header = match &error {
                tokio_tungstenite::tungstenite::Error::Io(io) => io
                    .get_ref()
                    .and_then(|cause| cause.downcast_ref::<rustls::Error>())
                    .is_some_and(|error| matches!(error, rustls::Error::InvalidMessage(_))),
                tokio_tungstenite::tungstenite::Error::Tls(
                    tokio_tungstenite::tungstenite::error::TlsError::Rustls(error),
                ) => matches!(error, rustls::Error::InvalidMessage(_)),
                _ => false,
            };
            if record_header {
                let mut prefix = [0; 5];
                let header = header.lock().unwrap();
                prefix[..header.len()].copy_from_slice(&header);
                return TransportError::TlsRecordHeader {
                    prefix,
                    reason: error.to_string(),
                };
            }
            match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => TransportError::Http {
                    status: response.status().as_u16(),
                    reason: response
                        .body()
                        .as_ref()
                        .map(|body| {
                            String::from_utf8_lossy(&body[..body.len().min(512)])
                                .trim()
                                .to_string()
                        })
                        .unwrap_or_default(),
                },
                error => TransportError::Io(error.to_string()),
            }
        })
    })
    .await
    .map_err(|_| TransportError::Io("relay dial timed out".into()))?;
    result
}
/// The relay supervisor's reconnect schedule, kept in [`super::State`].
#[derive(Default)]
pub struct Link {
    /// Epoch ms of the next scheduled attempt while waiting out a backoff.
    pub retry_at_ms: Option<u64>,
    /// Wakes the backoff wait so the next attempt happens at once.
    pub wake: std::sync::Arc<tokio::sync::Notify>,
}
/// Aborts the wrapped task when dropped, so aborting the supervisor never
/// leaves its relay task running on its own.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// Best-effort diagnostics: unlike `eprintln!`, a closed or broken stderr
/// must not be able to panic the relay.
fn log_line(message: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "intercom relay: {message}");
}
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map_or_else(|| "unknown panic".into(), |text| (*text).into()),
    }
}
/// Runs [`run`] under supervision: if the relay task ever ends while the
/// bridge is not stopping (a panic, or an early return), the reason becomes
/// the relay error and the task is restarted after a backoff that
/// `Runtime::relay_reconnect_now` can cut short.
pub async fn supervise(rt: Shared) {
    supervise_with(rt, |rt| tokio::spawn(run(rt))).await
}
async fn supervise_with(rt: Shared, start: impl Fn(Shared) -> tokio::task::JoinHandle<()>) {
    let mut stop = rt.stop.subscribe();
    let mut delay = BACKOFF_MIN;
    loop {
        if *stop.borrow() {
            break;
        }
        let mut task = AbortOnDrop(start(rt.clone()));
        let joined = (&mut task.0).await;
        if *stop.borrow() {
            break;
        }
        let reason = match joined {
            Ok(()) => "relay task exited unexpectedly".to_string(),
            Err(error) if error.is_panic() => {
                format!("relay task panicked: {}", panic_message(error.into_panic()))
            }
            Err(error) => format!("relay task failed: {error}"),
        };
        let (wait, next) = next_backoff(delay);
        let wake = {
            // A panic may have poisoned the lock; the state is still usable.
            let mut state = rt.state.lock().unwrap_or_else(|e| e.into_inner());
            // The dead session's sender no longer reaches a writer.
            state.relay_tx = None;
            state.relay_error = reason.clone();
            state.relay_link.retry_at_ms = Some(rt.clock.now_ms().saturating_add(wait.as_millis() as u64));
            state.relay_link.wake.clone()
        };
        log_line(&reason);
        tokio::select! {
            _ = stop.changed() => break,
            _ = wake.notified() => {}
            _ = rt.clock.sleep("intercom-relay-restart", wait) => {}
        }
        rt.state.lock().unwrap_or_else(|e| e.into_inner()).relay_link.retry_at_ms = None;
        delay = next;
    }
}
pub async fn run(rt: Shared) {
    let mut stop = rt.stop.subscribe();
    let wake = rt.state.lock().unwrap().relay_link.wake.clone();
    let mut delay = BACKOFF_MIN;
    let mut idle_tasks = tokio::task::JoinSet::new();
    // Keep one bounded operation pool across reconnects. Side-effecting
    // Herdr calls must finish their ownership bookkeeping even if the reply
    // path disappears; responses are separately gated by session generation.
    let mut op_tasks = tokio::task::JoinSet::new();
    let deliveries = delivery_lanes(&rt);
    loop {
        while idle_tasks.try_join_next().is_some() {}
        while op_tasks.try_join_next().is_some() {}
        if *stop.borrow() {
            break;
        }
        // An attempt starts now: no retry is pending, and a reconnect request
        // that arrived before this point is already satisfied.
        rt.state.lock().unwrap().relay_link.retry_at_ms = None;
        let _ = futures_util::FutureExt::now_or_never(wake.notified());
        let mut welcomed = None;
        let result = session(
            rt.clone(),
            &mut idle_tasks,
            &mut op_tasks,
            &deliveries,
            &mut welcomed,
        )
        .await;
        if *stop.borrow() {
            break;
        }
        // A relay that welcomes and then drops at once must not defeat backoff.
        if welcomed.is_some_and(|at| {
            rt.clock.now_ms().saturating_sub(at) >= HEALTHY_SESSION.as_millis() as u64
        }) {
            delay = BACKOFF_MIN;
        }
        let (mut wait, next) = next_backoff(delay);
        let mut failure = None;
        if let Err(error) = result {
            if error.is_auth() {
                wait = AUTH_RETRY_DELAY;
            }
            failure = Some(error.to_string());
        }
        {
            // Error and schedule are published together.
            let mut state = rt.state.lock().unwrap();
            if let Some(error) = &failure {
                state.relay_error = error.clone();
            }
            state.relay_link.retry_at_ms = Some(rt.clock.now_ms().saturating_add(wait.as_millis() as u64));
        }
        if let Some(error) = failure {
            log_line(&error);
        }
        tokio::select! {_=stop.changed()=>break,_=wake.notified()=>{},_=rt.clock.sleep("intercom-relay-reconnect",wait)=>{}}
        delay = next;
    }
}
type Abandon<T> = std::sync::Arc<dyn Fn(Vec<T>) + Send + Sync>;
type Runner<T> =
    std::sync::Arc<dyn Fn(T) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;
/// Per-key FIFO lanes: one worker per busy key keeps that key's items in
/// arrival order, a shared semaphore caps how many keys run at once, and a
/// byte budget caps what all lanes hold. Dropping the lanes aborts workers.
struct Lanes<T> {
    inner: std::sync::Arc<LanesInner<T>>,
    workers: std::sync::Mutex<tokio::task::JoinSet<()>>,
}
struct LanesInner<T> {
    queues: std::sync::Mutex<LaneQueues<T>>,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    per_key: usize,
    keys: usize,
    budget: usize,
    max_items: usize,
    size: fn(&T) -> usize,
    run: Runner<T>,
    abandon: Abandon<T>,
}
struct LaneQueues<T> {
    lanes: std::collections::HashMap<String, std::collections::VecDeque<T>>,
    bytes: usize,
    items: usize,
}
/// Removes a worker's lane if it dies mid-item (panic or abort) so later
/// pushes start a fresh worker instead of queueing behind a dead one, and
/// hands the stranded items to `abandon`.
struct LaneWorker<T> {
    inner: std::sync::Arc<LanesInner<T>>,
    key: String,
    retired: bool,
}
impl<T> Drop for LaneWorker<T> {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        let stranded = {
            let mut queues = self.inner.queues.lock().unwrap_or_else(|e| e.into_inner());
            let queue = queues.lanes.remove(&self.key).unwrap_or_default();
            let freed: usize = queue.iter().map(self.inner.size).sum();
            queues.bytes = queues.bytes.saturating_sub(freed);
            queues.items = queues.items.saturating_sub(queue.len());
            queue
        };
        if !stranded.is_empty() {
            (self.inner.abandon)(stranded.into());
        }
    }
}

/// Capacity knobs for a [`Lanes`] table: the worker pool size plus the
/// per-lane, lane-count, byte and item bounds enforced on `push`.
#[derive(Clone, Copy)]
struct LaneCaps {
    workers: usize,
    per_key: usize,
    keys: usize,
    budget: usize,
    max_items: usize,
}
impl<T: Send + 'static> Lanes<T> {
    fn new(caps: LaneCaps, size: fn(&T) -> usize, run: Runner<T>, abandon: Abandon<T>) -> Self {
        Self {
            inner: std::sync::Arc::new(LanesInner {
                queues: std::sync::Mutex::new(LaneQueues {
                    lanes: Default::default(),
                    bytes: 0,
                    items: 0,
                }),
                permits: std::sync::Arc::new(tokio::sync::Semaphore::new(caps.workers)),
                per_key: caps.per_key,
                keys: caps.keys,
                budget: caps.budget,
                max_items: caps.max_items,
                size,
                run,
                abandon,
            }),
            workers: Default::default(),
        }
    }
    /// Returns the item back when its lane, the lane table, the byte budget
    /// or the global item cap is full.
    fn push(&self, key: &str, item: T) -> Result<(), T> {
        let size = (self.inner.size)(&item);
        let mut queues = self.inner.queues.lock().unwrap();
        if queues.bytes.saturating_add(size) > self.inner.budget {
            return Err(item);
        }
        if queues.items >= self.inner.max_items {
            return Err(item);
        }
        if let Some(queue) = queues.lanes.get_mut(key) {
            if queue.len() >= self.inner.per_key {
                return Err(item);
            }
            queue.push_back(item);
            queues.bytes += size;
            queues.items += 1;
            return Ok(());
        }
        if queues.lanes.len() >= self.inner.keys {
            return Err(item);
        }
        queues
            .lanes
            .insert(key.to_string(), std::collections::VecDeque::from([item]));
        queues.bytes += size;
        queues.items += 1;
        drop(queues);
        let mut worker = LaneWorker {
            inner: self.inner.clone(),
            key: key.to_string(),
            retired: false,
        };
        let mut workers = self.workers.lock().unwrap();
        while workers.try_join_next().is_some() {}
        let inner = self.inner.clone();
        workers.spawn(async move {
            loop {
                let Ok(_permit) = inner.permits.clone().acquire_owned().await else {
                    break;
                };
                let next = {
                    let mut queues = inner.queues.lock().unwrap();
                    let next = queues
                        .lanes
                        .get_mut(&worker.key)
                        .and_then(|queue| queue.pop_front());
                    match &next {
                        Some(item) => {
                            let freed = (inner.size)(item);
                            queues.bytes = queues.bytes.saturating_sub(freed);
                            queues.items = queues.items.saturating_sub(1);
                        }
                        None => {
                            queues.lanes.remove(&worker.key);
                            worker.retired = true;
                        }
                    }
                    next
                };
                let Some(item) = next else { break };
                (inner.run)(item).await;
            }
        });
        Ok(())
    }
}
const DELIVERY_WORKERS: usize = 32;
/// Hard cap on in-flight relay-initiated operations (and their overflow
/// receipts): spawning one detached task per operation lets a chatty relay
/// exhaust the runtime, so beyond this the caller rejects or drops instead.
const REMOTE_OP_TASKS: usize = 64;
/// Spawns session-owned operation work, returning false when the pool is at
/// capacity so the caller can reject before any task exists.
fn spawn_remote_op(
    tasks: &mut tokio::task::JoinSet<()>,
    work: impl std::future::Future<Output = ()> + Send + 'static,
) -> bool {
    while tasks.try_join_next().is_some() {}
    if tasks.len() >= REMOTE_OP_TASKS {
        return false;
    }
    tasks.spawn(work);
    true
}
fn snapshot_rows(rows: Vec<Value>) -> Vec<Value> {
    rows.into_iter()
        .filter(|v| {
            str_field(v, "machine_id").is_empty() && !str_field(v, "id").starts_with("webhook/")
        })
        .map(super::public_row)
        .collect()
}
const DELIVERY_PER_TARGET: usize = 256;
const DELIVERY_TARGETS: usize = 1024;
const DELIVERY_ITEMS: usize = 4096;
const DELIVERY_BYTES: usize = 64 * 1024 * 1024;
fn delivery_lanes(rt: &Shared) -> Lanes<Value> {
    let rt = rt.clone();
    Lanes::new(
        LaneCaps {
            workers: DELIVERY_WORKERS,
            per_key: DELIVERY_PER_TARGET,
            keys: DELIVERY_TARGETS,
            budget: DELIVERY_BYTES,
            max_items: DELIVERY_ITEMS,
        },
        // Charge the complete encoded frame, not just the body: the queue
        // retains the whole Value and headers/metadata can dominate a small
        // body, so sizing by body would let frames under-report their cost.
        |frame| frame.to_string().len() + 256,
        {
            let rt = rt.clone();
            std::sync::Arc::new(move |frame| Box::pin(deliver(rt.clone(), frame)))
        },
        std::sync::Arc::new(move |frames| abandon_deliveries(&rt, frames)),
    )
}
/// Nacks deliveries stranded behind a lane worker that died, so senders get
/// a terminal receipt instead of waiting out their timeout.
fn abandon_deliveries(rt: &Shared, frames: Vec<Value>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let rt = rt.clone();
    handle.spawn(async move {
        for frame in frames {
            let mid = str_field(&frame, "message_id");
            if !mid.is_empty() {
                send_receipt(&rt, mid, deliver_target(&frame), "dropped").await;
            }
        }
    });
}
fn deliver_target(v: &Value) -> &str {
    if str_field(v, "session_id").is_empty() {
        str_field(v, "to")
    } else {
        str_field(v, "session_id")
    }
}
async fn send_receipt(rt: &Runtime, mid: &str, target: &str, status: &str) {
    let writer = rt.state.lock().unwrap().relay_tx.clone();
    if let Some(writer) = writer {
        let _ = writer
            .send(json!({"type":"receipt","message_id":mid,"from":target,"status":status}).into())
            .await;
    }
}
// Every relay-initiated operation is refused, reads included: status and
// agent_read/agent_wait expose local session state and terminal contents.
const REMOTE_REFUSED: &str = "cross-machine operation refused: remote_messages=refuse";
async fn deliver(rt: Shared, v: Value) {
    let mid = str_field(&v, "message_id");
    let target = deliver_target(&v);
    let reply_to = str_field(&v, "reply_to");
    let origin = match reply_to {
        "" if super::valid_intercom_id(str_field(&v, "from")) => str_field(&v, "from"),
        "" => "",
        reply_to => reply_to,
    };
    let refused = rt.cfg.remote_messages == "refuse";
    let result = if refused {
        Err(REMOTE_REFUSED.to_string())
    } else if target.is_empty() {
        Err("deliver without session_id".into())
    } else if !origin.is_empty() && !super::valid_intercom_id(origin) {
        Err(format!("deliver with invalid reply_to {origin:?}"))
    } else if let Err(error) = super::check_from_mode(str_field(&v, "from_mode")) {
        Err(error)
    } else {
        match rt.resolve(target).await {
            Ok(row) => {
                let from = if target.starts_with("pi/") && str_field(&v, "from").is_empty() {
                    "relay"
                } else {
                    str_field(&v, "from")
                };
                let frame = json!({"type":"deliver","session_id":target,"from":from,"body":v["body"],"reply_to":origin,"message_id":mid});
                rt.inject_local(&row, frame, str_field(&v, "from_mode"), false, None)
                    .await
            }
            Err(error) => Err(error),
        }
    };
    if mid.is_empty() {
        return;
    }
    let status = match result {
        Ok(_) => {
            if !target.starts_with("pi/") && !target.starts_with("codex/") {
                return;
            }
            rt.state
                .lock()
                .unwrap()
                .messages
                .get(mid)
                .filter(|delivery| super::deliveries::rank(&delivery.status) >= 3)
                .map(|delivery| delivery.status.clone())
                .unwrap_or_else(|| "delivered".into())
        }
        Err(_) if refused => "refused".into(),
        Err(error) => {
            // Go parity: a pi injection failure nacks `dropped`; only codex
            // reports an uncertain write as `unknown`.
            if target.starts_with("codex/") && error.contains("outcome unknown") {
                rt.record_status(mid, "unknown", false);
                "unknown".into()
            } else {
                "dropped".into()
            }
        }
    };
    send_receipt(&rt, mid, target, &status).await;
}
async fn session(
    rt: Shared,
    idle_tasks: &mut tokio::task::JoinSet<()>,
    op_tasks: &mut tokio::task::JoinSet<()>,
    deliveries: &Lanes<Value>,
    welcomed: &mut Option<u64>,
) -> Result<(), TransportError> {
    let mut ws = Conn::connect(&rt.cfg, rt.clock.clone()).await?;
    let welcome=ws.register(json!({"type":"register","machine_id":rt.machine,"machine_name":rt.cfg.name,"device_id":rt.identity.device_id})).await?;
    *welcomed = Some(rt.clock.now_ms());
    ws.start_ping();
    let (tx, mut rx) = mpsc::channel::<super::Push>(128);
    loop {
        let batch = {
            let mut state = rt.state.lock().unwrap();
            if state.control_outbox.is_empty() {
                state.relay_generation = state.relay_generation.wrapping_add(1);
                state.relay_message_ids = welcome["send_message_id_v1"].as_bool() == Some(true);
                state.relay_tx = Some(tx.clone());
                state.relay_error.clear();
                None
            } else {
                Some(std::mem::take(&mut state.control_outbox))
            }
        };
        let Some(mut batch) = batch else { break };
        while let Some(frame) = batch.pop_front() {
            if let Err(error) = ws.send(Message::Text(frame.to_string().into())).await {
                batch.push_front(frame);
                park_controls(&mut rt.state.lock().unwrap(), batch);
                return Err(error);
            }
        }
    }
    let (idle_tx, mut idle_rx) = ordered_queue();
    let idle_runtime = rt.clone();
    let idle_writer = tx.clone();
    idle_tasks.spawn(async move {
        while let Some(frame) = idle_rx.recv().await {
            match frame {
                OrderedIncoming::Reply(value, reply) => {
                    let _ = reply.send(value);
                }
                OrderedIncoming::Control(frame) => {
                    if let Some(response) = idle_runtime.idle_wire(frame).await {
                        let _ = idle_writer.send(response.into()).await;
                    }
                }
            }
        }
    });
    let mut failed_control = None;
    let mut stop = rt.stop.subscribe();
    // A stop signaled before or during the handshake must not be missed just
    // because the subscription was only taken after registration.
    if *stop.borrow() {
        return Ok(());
    }
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let result = loop {
        tokio::select! {
            _=stop.changed()=>break Ok(()),
            _=tick.tick()=>{let local=snapshot_rows(rt.directory().await);let frame=json!({"type":"snapshot","machine_id":rt.machine,"sessions":local});if let Err(e)=ws.send(Message::Text(frame.to_string().into())).await{break Err(e);}
            if let Err(e)=ws.send(Message::Text(json!({"type":"list","id":"directory"}).to_string().into())).await{break Err(e);}}
            req=rx.recv()=>{if let Some(packet)=req{let v=packet.value;let result=ws.send(Message::Text(v.to_string().into())).await;drop(packet.fence);if let Err(e)=result{if is_control(&v){failed_control=Some(v);}break Err(e);}}}
            msg=ws.next()=>match msg{
                Some(Ok(Message::Text(t)))=>{
                    let v:Value=match serde_json::from_str(&t){Ok(v)=>v,Err(e)=>break Err(TransportError::Io(e.to_string()))};
                    let pending={rt.state.lock().unwrap().pending.remove(str_field(&v,"id"))};if let Some(p)=pending{idle_tx.reply(v,p);continue;}
                    if v["rpc_reply"]==true{continue;}
                    if str_field(&v,"id")=="directory"{rt.state.lock().unwrap().remote=v["sessions"].as_array().into_iter().flatten().filter(|s|str_field(s,"machine_id")!=rt.machine).cloned().collect();continue;}
                    match str_field(&v,"type"){
                        "idle_subscribe"|"idle_cancel"|"idle_notice"=>{if v["rpc_reply"]!=true&&idle_tx.control(v).is_err(){break Err("idle relay control queue capacity reached".into());}}
                        "receipt"=>{
                            let mid=str_field(&v,"message_id");let reporter=str_field(&v,"from");let status=str_field(&v,"status");
                            let origin={let mut state=rt.state.lock().unwrap();
                                let rekey=state.legacy_active.as_ref().filter(|a|a.origin==str_field(&v,"session_id")&&a.target==reporter&&a.current_id==a.private_id&&!state.messages.contains_key(mid)&&super::deliveries::rank(status)>0).map(|a|a.current_id.clone());
                                if let Some(private)=rekey{if state.rekey_delivery(&private,mid).is_ok(){if let Some(active)=state.legacy_active.as_mut(){active.current_id=mid.into();}}}
                                state.messages.get(mid).filter(|d|d.target==reporter&&d.origin==str_field(&v,"session_id")).map(|d|d.origin.clone())};
                            if let Some(origin)=origin {
                                if origin.starts_with("claude/"){rt.receipt_with_reason(mid,reporter,status,str_field(&v,"drop_reason")).await;}else{
                                    rt.record_status(mid,status,true);
                                    let _=rt.push(&origin,json!({"type":"receipt","from":reporter,"status":status,"message_id":mid})).await;
                                }
                            }
                        }
                        "deliver"=>{
                            let target=deliver_target(&v).to_string();
                            if let Err(v)=deliveries.push(&target,v){
                                eprintln!("intercom relay: delivery queue for {target} full; dropped {}",str_field(&v,"message_id"));
                                if !str_field(&v,"message_id").is_empty(){
                                    let rt=rt.clone();
                                    let mid=str_field(&v,"message_id").to_string();
                                    let nack_mid=mid.clone();
                                    if !spawn_remote_op(op_tasks,async move{send_receipt(&rt,&nack_mid,&target,"dropped").await;}){
                                        eprintln!("intercom relay: operation pool full; delivery nack for {mid} abandoned");
                                    }
                                }
                            }
                        }
                        "status"|"workspace_list"|"workspace_create"|"workspace_close"|"session_create"|"session_close"|"agent_prompt"|"agent_keys"|"agent_read"|"agent_wait"=>{
                            let request_id=v["id"].clone();
                            let generation=rt.state.lock().unwrap().relay_generation;
                            let spawned = spawn_remote_op(op_tasks,{
                                let rt=rt.clone();
                                async move{
                                let mut req=v.clone();req["op"]=v["type"].clone();req["machine"]=json!(rt.machine);req["to"]=if str_field(&v,"type")=="workspace_close"{v["workspace_id"].clone()}else{v["session_id"].clone()};req["workspace"]=v["workspace_id"].clone();
                                let mut response=if rt.cfg.remote_messages=="refuse"{json!({"type":"error","code":"refused","message":REMOTE_REFUSED})}else{
                                    let result=if str_field(&v,"type")=="status"{{let mut report=rt.status_report().await;if let Some(fields)=report.as_object_mut(){fields.remove("mcp_token_file");}Ok(json!({"report":report}))}}else{super::herdr::dispatch_relay(&rt,&req).await};
                                    match result{Ok(v)=>v,Err(e)=>json!({"type":"error","code":"unavailable","message":e})}
                                };
                                response["id"]=v["id"].clone();if response.get("type").is_none(){response["type"]=v["type"].clone();}
                                let tx={let state=rt.state.lock().unwrap();(state.relay_generation==generation).then(||state.relay_tx.clone()).flatten()};if let Some(tx)=tx{let _=tx.send(response.into()).await;}
                                }
                            });
                            if !spawned {
                                let mut response=json!({"type":"error","code":"unavailable","message":"relay operation capacity reached; retry later"});
                                response["id"]=request_id;
                                if let Some(tx)=rt.state.lock().unwrap().relay_tx.clone(){
                                    let _=tx.try_send(response.into());
                                }
                            }
                        }
                        _=>{}
                    }
                },
                Some(Ok(Message::Ping(p)))=>if let Err(e)=ws.send(Message::Pong(p)).await{break Err(e);},
                Some(Ok(Message::Close(_)))|None=>break Err("relay disconnected".into()),
                Some(Err(e))=>break Err(e),_=>{}
            }
        }
    };
    {
        let mut s = rt.state.lock().unwrap();
        s.relay_tx = None;
        rx.close();
        let mut unsent = std::collections::VecDeque::new();
        if let Some(frame) = failed_control {
            unsent.push_back(frame);
        }
        while let Ok(frame) = rx.try_recv() {
            if is_control(&frame.value) {
                unsent.push_back(frame.value);
            }
        }
        park_controls(&mut s, unsent);
        s.relay_message_ids = false;
        s.pending.clear();
        s.remote.clear();
    }
    result
}
pub async fn enroll(c: &Config, new_identity: bool) -> Result<(), String> {
    if c.token.is_empty() {
        return Err("enroll token required".into());
    }
    // Build the candidate identity fully in memory: a failed or incomplete
    // enrollment must never destroy the only persisted copy of the current
    // device identity, so device.json is replaced (atomically, via save's
    // temp-write + rename) only after a valid successful response.
    let mut id = if new_identity {
        Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            ..Identity::default()
        }
    } else {
        Identity::load(&c.state_dir)?
    };
    let mut req = c
        .http()?
        .post(format!("{}/v1/enroll", c.server.trim_end_matches('/')))
        .json(&json!({"token":c.token,"device_id":id.device_id,"name":c.display_name(),"client_version":crate::VERSION}));
    if !id.credential.is_empty()
        && id.server.trim_end_matches('/') == c.server.trim_end_matches('/')
    {
        req = req.bearer_auth(&id.credential);
    }
    let r = req.send().await.map_err(|e| e.to_string())?;
    if r.status() != reqwest::StatusCode::CREATED {
        return Err(format!(
            "enroll: {}",
            r.text().await.map_err(|e| e.to_string())?
        ));
    }
    let v: Value = r.json().await.map_err(|e| e.to_string())?;
    if v["device_id"] != id.device_id
        || str_field(&v, "credential").is_empty()
        || str_field(&v, "tenant_id").is_empty()
    {
        return Err("enroll: incomplete server response".into());
    }
    id.credential = str_field(&v, "credential").into();
    id.tenant_id = str_field(&v, "tenant_id").into();
    id.server = c.server.trim_end_matches('/').into();
    id.save(&c.state_dir)?;
    super::cli::print_json(
        &json!({"device_id":id.device_id,"tenant_id":id.tenant_id,"server":id.server}),
    );
    Ok(())
}
pub async fn webhook(
    c: &Config,
    target: &str,
    origin: &str,
    body: &str,
    mid: &str,
) -> Result<String, String> {
    let ep = c
        .endpoints
        .iter()
        .find(|v| format!("webhook/{}", v.name) == target)
        .ok_or("unknown endpoint")?;
    post_webhook(
        c, &ep.url, &ep.secret, &ep.auth, &ep.name, origin, body, mid,
    )
    .await
}
#[allow(clippy::too_many_arguments)] // Keep the explicit webhook protocol fields.
async fn post_webhook(
    c: &Config,
    url: &str,
    secret: &str,
    auth: &str,
    name: &str,
    origin: &str,
    body: &str,
    mid: &str,
) -> Result<String, String> {
    let endpoint = super::config::Endpoint {
        name: name.into(),
        url: url.into(),
        secret: secret.into(),
        auth: auth.into(),
    };
    let message = WebhookMessage {
        id: mid,
        from: origin,
        reply_to: origin,
        body,
    };
    post_webhook_message(c, &endpoint, &message, &crate::clock::Clock::from_env()).await
}
struct WebhookMessage<'a> {
    id: &'a str,
    from: &'a str,
    reply_to: &'a str,
    body: &'a str,
}
fn webhook_timestamp(ms: u64) -> String {
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
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn webhook_client() -> Result<reqwest::Client, String> {
    let roots = tls::native_roots();
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| error.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())
}
async fn post_webhook_message(
    _c: &Config,
    endpoint: &super::config::Endpoint,
    message: &WebhookMessage<'_>,
    clock: &crate::clock::Clock,
) -> Result<String, String> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    super::config::webhook_url_allowed(&endpoint.url, &endpoint.secret)
        .map_err(|error| format!("endpoint {}: {error}", endpoint.name))?;
    let mid = if message.id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        message.id.into()
    };
    let from = if message.from.is_empty() {
        "pi-famulus"
    } else {
        message.from
    };
    let mut payload = json!({"event":"message.created","event_type":"message.created","message":{"id":mid,"from":from,"to":endpoint.name,"body":message.body,"created_at":webhook_timestamp(crate::proto::now_ms())}});
    if !message.reply_to.is_empty() {
        payload["message"]["from_session"] = json!(message.reply_to);
    }
    let raw = payload.to_string();
    let client = webhook_client()?;
    let mut request = client
        .post(&endpoint.url)
        .timeout(Duration::from_secs(10))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", "message.created")
        .header("X-Request-ID", &mid);
    if !endpoint.secret.is_empty() {
        if endpoint.auth == "bearer" {
            request = request.bearer_auth(&endpoint.secret);
        } else {
            let mut mac = Hmac::<Sha256>::new_from_slice(endpoint.secret.as_bytes())
                .map_err(|e| e.to_string())?;
            mac.update(raw.as_bytes());
            let signature: String = mac
                .finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            request = request
                .header("X-Intercom-Signature", format!("sha256={signature}"))
                .header("X-Hub-Signature-256", format!("sha256={signature}"));
        }
    }
    let request = request
        .body(raw)
        .build()
        .map_err(|e| format!("endpoint {}: {e}", endpoint.name))?;
    let mut error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            clock
                .sleep(
                    "intercom_webhook_retry",
                    Duration::from_secs(if attempt == 1 { 1 } else { 3 }),
                )
                .await;
        }
        let request = request
            .try_clone()
            .ok_or("webhook request body cannot be replayed")?;
        match client.execute(request).await {
            Err(failure) => {
                // reqwest's Display omits the cause (TLS, refused, timeout).
                let mut detail = failure.to_string();
                let mut source = std::error::Error::source(&failure);
                while let Some(cause) = source {
                    detail.push_str(&format!(": {cause}"));
                    source = cause.source();
                }
                error = format!("endpoint {}: {detail}", endpoint.name)
            }
            Ok(mut response) => {
                let status = response.status();
                if !status.is_success() {
                    error = format!("endpoint {}: HTTP {}", endpoint.name, status.as_u16());
                    if !status.is_server_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        return Err(error);
                    }
                    continue;
                }
                let mut bytes = Vec::new();
                while bytes.len() < 8 << 10 {
                    match response.chunk().await {
                        Ok(Some(chunk)) => bytes
                            .extend_from_slice(&chunk[..chunk.len().min((8 << 10) - bytes.len())]),
                        _ => break,
                    }
                }
                if serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .is_some_and(|value| str_field(&value, "status") == "ignored")
                {
                    return Err(format!("endpoint {}: ignored", endpoint.name));
                }
                return Ok("delivered".into());
            }
        }
    }
    Err(error)
}

fn http_credential(c: &Config) -> Result<String, String> {
    if !c.token.is_empty() {
        return Ok(c.token.clone());
    }
    let identity = Identity::load(&c.state_dir)?;
    if identity.credential.is_empty()
        || identity.server.trim_end_matches('/') != c.server.trim_end_matches('/')
    {
        return Err("enroll this device for the selected server or pass an endpoint key".into());
    }
    Ok(identity.credential)
}
pub async fn endpoint_send(c: &Config, to: &str, body: &str, origin: &str) -> Result<(), String> {
    if c.remote_messages == "refuse" {
        return Err("cross-machine operation refused: remote_messages=refuse".into());
    }
    let token = http_credential(c)?;
    let response = c
        .http()?
        .post(format!("{}/v1/messages", c.server.trim_end_matches('/')))
        .query(&[("as", origin)])
        .bearer_auth(&token)
        .json(&json!({"to":to,"body":body}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if status == reqwest::StatusCode::CREATED {
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        super::cli::print_json(&v);
        return Ok(());
    }
    if status == reqwest::StatusCode::NOT_FOUND
        && String::from_utf8_lossy(&bytes).trim() == "endpoint not found"
    {
        let identity = Identity::load(&c.state_dir)?;
        if !identity.credential.is_empty()
            && identity.server.trim_end_matches('/') == c.server.trim_end_matches('/')
            && token == identity.credential
        {
            let v=super::localrpc::call(c,json!({"op":"send","to":to,"body":body,"reply_to":format!("listen/{}",origin.trim_start_matches("listen/"))})).await.map_err(|e|format!("listen reply requires a running client daemon; no one-shot connection was opened: {e}"))?;
            super::cli::print_json(&v);
            return Ok(());
        }
    }
    Err(format!(
        "endpoint HTTP {status}: {}",
        String::from_utf8_lossy(&bytes)
    ))
}
pub async fn inbox(
    c: &Config,
    id: Option<String>,
    origin: String,
    limit: u32,
    cursor: Option<String>,
) -> Result<(), String> {
    let token = http_credential(c)?;
    let path = id
        .map(|v| format!("/v1/inbox/{v}"))
        .unwrap_or_else(|| "/v1/inbox".into());
    let mut req = c
        .http()?
        .get(format!("{}{}", c.server.trim_end_matches('/'), path))
        .bearer_auth(&token)
        .query(&[("as", origin), ("limit", limit.to_string())]);
    if let Some(cursor) = cursor {
        req = req.query(&[("cursor", cursor)]);
    }
    let r = req
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let v: Value = r.json().await.map_err(|e| e.to_string())?;
    super::cli::print_json(&v);
    Ok(())
}
pub async fn call_once(c: &Config, name: &str, mut frame: Value) -> Result<Value, String> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let identity = Identity::load(&c.state_dir)?;
        let mut config = c.clone();
        super::adopt_identity_credential(&mut config, &identity)?;
        let mut ws = Conn::connect(&config,crate::clock::Clock::default()).await.map_err(|error|error.to_string())?;
        let host = super::config::hostname();
        ws.register(json!({"type":"register","device_id":identity.device_id,"machine_id":format!("listen-{name}-{host}-{}-{}",std::process::id(),uuid::Uuid::new_v4()),"machine_name":name})).await.map_err(|error|error.to_string())?;
        let send=str_field(&frame,"type")=="send";
        if str_field(&frame,"id").is_empty(){frame["id"]=json!(if send{"send-1"}else{"call-1"});}
        if send {
            if str_field(&frame,"message_id").is_empty(){frame["message_id"]=json!(uuid::Uuid::new_v4().to_string());}
            if !name.is_empty()&&str_field(&frame,"reply_to").is_empty(){frame["reply_to"]=json!(format!("listen/{name}"));}
        }
        ws.send(Message::Text(frame.to_string().into())).await.map_err(|error|error.to_string())?;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text)))=>{
                    let response:Value=serde_json::from_str(&text).map_err(|error|error.to_string())?;
                    if response["id"]!=frame["id"]{continue;}
                    if str_field(&response,"type")=="error"{return Err(format!("{}: {}",str_field(&response,"code"),str_field(&response,"message")));}
                    if send && (response["type"]!=frame["type"]||response["message_id"]!=frame["message_id"]||!matches!(str_field(&response,"status"),"accepted"|"queued")){
                        return Err("invalid send acknowledgement: expected matching type, id, message_id and accepted or queued status".into());
                    }
                    return Ok(response);
                },
                Some(Ok(Message::Ping(payload)))=>ws.send(Message::Pong(payload)).await.map_err(|error|error.to_string())?,
                Some(Err(error))=>return Err(error.to_string()),
                Some(Ok(Message::Close(_)))|None=>return Err("relay closed before reply".into()),
                _=>{}
            }
        }
    }).await.map_err(|_|"one-shot relay call timed out".to_string())?
}
pub async fn listen(c: Config, name: String, url: String, secret: String) -> Result<(), String> {
    // The name becomes the `listen/<name>` reply origin peers validate.
    if !name.is_empty() && !crate::daemon::valid_session_id(&name) {
        return Err("endpoint name must be 1-128 letters, digits, '-', '_' or '.'".into());
    }
    super::config::webhook_url_allowed(&url, &secret)?;
    let identity = Identity::load(&c.state_dir)?;
    let mut c = c;
    super::adopt_identity_credential(&mut c, &identity)?;
    let endpoint = super::config::Endpoint {
        name: name.clone(),
        url,
        secret,
        auth: "hmac".into(),
    };
    let clock = crate::clock::Clock::from_env();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = listen_loop(&c, &identity, &name, &endpoint, &clock) => Ok(()),
    }
}
async fn listen_loop(
    c: &Config,
    identity: &Identity,
    name: &str,
    endpoint: &super::config::Endpoint,
    clock: &crate::clock::Clock,
) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let (healthy, error) = listen_once(c, identity, name, endpoint, clock).await;
        let (sleep, next) = listen_retry_delay(backoff, healthy, &error);
        if error.is_auth() {
            eprintln!("relay authentication rejected: {error}; re-enroll this device");
        }
        eprintln!("relay connection: {error} (reconnect in {sleep:?})");
        clock.sleep("intercom_listen_reconnect", sleep).await;
        backoff = next;
    }
}
fn listen_retry_delay(
    base: Duration,
    healthy: bool,
    error: &TransportError,
) -> (Duration, Duration) {
    let (sleep, next) = next_backoff(if healthy { BACKOFF_MIN } else { base });
    (
        if error.is_auth() {
            AUTH_RETRY_DELAY
        } else {
            sleep
        },
        next,
    )
}
fn listen_json(frame: Message) -> Result<Option<Value>, TransportError> {
    match frame {
        Message::Text(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| TransportError::Io(error.to_string())),
        Message::Binary(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| TransportError::Io(error.to_string())),
        Message::Close(_) => Err(TransportError::Io("relay closed connection".into())),
        _ => Ok(None),
    }
}
async fn listen_once(
    c: &Config,
    identity: &Identity,
    name: &str,
    endpoint: &super::config::Endpoint,
    clock: &crate::clock::Clock,
) -> (bool, TransportError) {
    let mut connection = match Conn::connect(c, clock.clone()).await {
        Ok(connection) => connection,
        Err(error) => return (false, error),
    };
    let registered = tokio::time::timeout(Duration::from_secs(15), async {
        let host = super::config::hostname();
        connection.send(Message::text(json!({"type":"register","device_id":identity.device_id,"machine_id":format!("listen-{name}-{host}-{}", std::process::id()),"machine_name":name}).to_string())).await?;
        loop {
            let frame = connection.next().await.ok_or_else(|| TransportError::Io("relay closed before welcome".into()))??;
            let Some(welcome) = listen_json(frame)? else { continue; };
            return match str_field(&welcome, "type") {
                "welcome" => Ok(()),
                "error" => Err(TransportError::remote(&welcome)),
                kind => Err(TransportError::Io(format!("relay registration: expected welcome, received {kind:?}"))),
            };
        }
    }).await.unwrap_or_else(|_| Err(TransportError::Io("relay handshake timeout".into())));
    if let Err(error) = registered {
        return (false, error);
    }
    let welcomed = clock.now_ms();
    let result: Result<(), TransportError> = async {
        let session = format!("listen/{name}");
        connection.send(Message::text(json!({"type":"snapshot","sessions":[{"id":session,"name":name,"harness":"webhook","status":"online"}]}).to_string())).await?;
        connection.start_ping();
        loop {
            let frame = connection.next().await.ok_or_else(|| TransportError::Io("relay closed connection".into()))??;
            let Some(event) = listen_json(frame)? else { continue; };
            match str_field(&event, "type") {
                "deliver" if str_field(&event, "session_id") == session => {
                    let mid = str_field(&event, "message_id");
                    // Mirror the daemon path: under remote_messages=refuse no
                    // remote delivery may leave the machine, so refuse on the
                    // wire without touching the webhook endpoint at all.
                    let status = if c.remote_messages == "refuse" {
                        "refused"
                    } else {
                        let message = WebhookMessage { id: mid, from: str_field(&event, "from"), reply_to: str_field(&event, "reply_to"), body: str_field(&event, "body") };
                        match post_webhook_message(c, endpoint, &message, clock).await {
                            Ok(_) => "delivered",
                            Err(error) => { eprintln!("forward {mid}: {error}"); "dropped" },
                        }
                    };
                    if !mid.is_empty() {
                        connection.send(Message::text(json!({"type":"receipt","message_id":mid,"from":session,"status":status}).to_string())).await?;
                    }
                }
                "error" => eprintln!("relay error {}: {}", str_field(&event, "code"), str_field(&event, "message")),
                _ => {},
            }
        }
    }.await;
    let healthy = clock.now_ms().saturating_sub(welcomed) >= HEALTHY_SESSION.as_millis() as u64;
    (healthy, result.unwrap_err())
}

#[cfg(test)]
mod listen_webhook_tests {
    use super::*;

    #[tokio::test]
    async fn webhook_tls_uses_native_roots_and_standard_verification_not_relay_insecure() {
        if let Ok(url) = std::env::var("PI_FAMULUS_WEBHOOK_TLS_URL") {
            let config = Config {
                tls_insecure: true,
                ..Config::default()
            };
            let result = post_webhook(
                &config, &url, "secret", "hmac", "hermes", "sender", "body", "tls-id",
            )
            .await;
            if std::env::var("PI_FAMULUS_WEBHOOK_TLS_CASE").unwrap() == "trusted" {
                assert_eq!(
                    result.unwrap_or_else(|error| panic!(
                        "{error}; trust store held {} roots from SSL_CERT_FILE={:?}",
                        tls::native_roots().len(),
                        std::env::var_os("SSL_CERT_FILE")
                    )),
                    "delivered"
                );
            } else {
                assert!(
                    result.is_err(),
                    "relay --insecure disabled webhook certificate validation"
                );
            }
            return;
        }
        let directory = StateDir::new();
        std::fs::create_dir(&directory.0).unwrap();
        // An explicit config: OpenSSL 1.1's default v3_ca section would
        // duplicate basicConstraints alongside -addext, which webpki rejects.
        std::fs::write(directory.0.join("root.cnf"), "[req]\ndistinguished_name=dn\nx509_extensions=root\nprompt=no\n[dn]\nCN=Webhook fixture root\n[root]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n").unwrap();
        std::fs::write(directory.0.join("leaf.cnf"), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n").unwrap();
        for args in [
            vec![
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "root.key", "-out",
                "root.pem", "-days", "1", "-config", "root.cnf",
            ],
            vec![
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "leaf.key",
                "-out",
                "leaf.csr",
                "-subj",
                "/CN=localhost",
            ],
            vec![
                "x509",
                "-req",
                "-in",
                "leaf.csr",
                "-CA",
                "root.pem",
                "-CAkey",
                "root.key",
                "-CAcreateserial",
                "-out",
                "leaf.pem",
                "-days",
                "1",
                "-extfile",
                "leaf.cnf",
            ],
            vec![
                "x509", "-in", "leaf.pem", "-outform", "DER", "-out", "leaf.der",
            ],
            vec![
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                "leaf.key",
                "-outform",
                "DER",
                "-out",
                "leaf-key.der",
            ],
        ] {
            let output = crate::sys::command_std("openssl")
                .args(args)
                .current_dir(&directory.0)
                .output()
                .expect("OpenSSL certificate fixture is required");
            assert!(
                output.status.success(),
                "OpenSSL fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        for trusted in [true, false] {
            let versions: &[&rustls::SupportedProtocolVersion] = if trusted {
                &[&rustls::version::TLS12]
            } else {
                &[&rustls::version::TLS13]
            };
            let tls = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(versions)
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    std::fs::read(directory.0.join("leaf.der")).unwrap(),
                )],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    std::fs::read(directory.0.join("leaf-key.der"))
                        .unwrap()
                        .into(),
                ),
            )
            .unwrap();
            let tls = std::sync::Arc::new(tls);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopping = stop.clone();
            let worker = std::thread::spawn(move || {
                use std::io::{BufRead as _, Read as _, Write as _};
                let mut accepted = 0;
                let mut requests = 0;
                while !stopping.load(std::sync::atomic::Ordering::Acquire) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("TLS fixture accept: {error}"),
                    };
                    accepted += 1;
                    // BSD-derived systems inherit O_NONBLOCK from the listener.
                    stream.set_nonblocking(false).unwrap();
                    // Only a wedged-fixture guard, as long as the client's own
                    // 10s timeout: a shorter one turns a slow client into a
                    // retry and a second accept.
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let stream = rustls::StreamOwned::new(
                        rustls::ServerConnection::new(tls.clone()).unwrap(),
                        stream,
                    );
                    let mut reader = std::io::BufReader::new(stream);
                    let mut length = 0;
                    let mut complete = false;
                    loop {
                        let mut line = String::new();
                        if !matches!(reader.read_line(&mut line), Ok(count) if count > 0) {
                            break;
                        }
                        if line == "\r\n" {
                            complete = true;
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    if complete {
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).unwrap();
                        requests += 1;
                        reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
                        reader.get_mut().flush().unwrap();
                    }
                }
                (accepted, requests)
            });
            let output = crate::sys::command_std(std::env::current_exe().unwrap())
                .args(["--exact", "intercom::relay::listen_webhook_tests::webhook_tls_uses_native_roots_and_standard_verification_not_relay_insecure", "--nocapture"])
                .env("PI_FAMULUS_WEBHOOK_TLS_URL", format!("https://{address}/hook"))
                .env("PI_FAMULUS_WEBHOOK_TLS_CASE", if trusted { "trusted" } else { "untrusted" })
                .env("SSL_CERT_FILE", directory.0.join(if trusted { "root.pem" } else { "absent.pem" }))
                .env("SSL_CERT_DIR", directory.0.join("absent-dir"))
                .env_remove("PI_FAMULUS_TEST_CLOCK")
                .output().unwrap();
            stop.store(true, std::sync::atomic::Ordering::Release);
            let (accepted, requests) = worker.join().unwrap();
            if !output.status.success() {
                let describe = |args: &[&str]| {
                    crate::sys::command_std("openssl")
                        .args(args)
                        .current_dir(&directory.0)
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        .unwrap_or_default()
                };
                panic!(
                    "TLS case trusted={trusted} failed:\n{}\n{}\n{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                    describe(&["version"]),
                    describe(&["x509", "-in", "root.pem", "-noout", "-text"])
                );
            }
            assert_eq!(accepted, if trusted { 1 } else { 3 });
            assert_eq!(requests, usize::from(trusted));
        }
    }

    async fn redirect_peer(
        status: u16,
        location: String,
    ) -> (
        String,
        oneshot::Receiver<(String, Vec<u8>)>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                headers.push_str(&line);
            }
            let mut raw = vec![0; length];
            reader.read_exact(&mut raw).await.unwrap();
            tx.send((headers, raw)).unwrap();
            reader.get_mut().write_all(format!("HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        (format!("http://{address}/redirect"), rx, task)
    }
    async fn redirected_webhook(status: u16) {
        let (target, mut delivered, target_task) = http_peer(vec![(200, "{}")]).await;
        let (url, first, redirect_task) = redirect_peer(status, target).await;
        let result = post_webhook(
            &Config::default(),
            &url,
            "secret",
            "hmac",
            "hermes",
            "sender",
            "body",
            "redirect-id",
        )
        .await;
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .contains(&format!("HTTP {status}")),
            "{result:?}"
        );
        assert!(first.await.unwrap().0.starts_with("POST /redirect "));
        redirect_task.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), delivered.recv())
                .await
                .is_err(),
            "signed webhook body was replayed to the redirect target"
        );
        target_task.abort();
    }
    #[tokio::test]
    async fn webhook_307_redirect_is_final_and_not_replayed() {
        redirected_webhook(307).await;
    }
    #[tokio::test]
    async fn webhook_308_redirect_is_final_and_not_replayed() {
        redirected_webhook(308).await;
    }
    #[tokio::test]
    async fn webhook_302_redirect_is_final_and_not_followed() {
        redirected_webhook(302).await;
    }
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    struct StateDir(std::path::PathBuf);
    impl StateDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("famulus-listen-{}", uuid::Uuid::new_v4())))
        }
    }
    impl Drop for StateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn http_peer(
        responses: Vec<(u16, &'static str)>,
    ) -> (
        String,
        mpsc::UnboundedReceiver<(String, Vec<u8>)>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for (status, body) in responses {
                let (stream, _) = tokio::time::timeout(Duration::from_secs(6), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    headers.push_str(&line);
                }
                let mut raw = vec![0; length];
                reader.read_exact(&mut raw).await.unwrap();
                tx.send((headers, raw)).unwrap();
                if status > 0 {
                    reader.get_mut().write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            }
        });
        (format!("http://{address}/hook"), rx, task)
    }
    // tungstenite fixes the handshake callback's error type to a full HTTP response.
    #[allow(clippy::result_large_err)]
    async fn relay_peer(
        listener: &tokio::net::TcpListener,
    ) -> (WebSocketStream<tokio::net::TcpStream>, String) {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut token = String::new();
        let socket = tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                token = request
                    .headers()
                    .get("Authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .into();
                Ok(response)
            },
        )
        .await
        .unwrap();
        (socket, token)
    }
    async fn json_frame(socket: &mut WebSocketStream<tokio::net::TcpStream>) -> Value {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn listen_preserves_explicit_token_over_conflicting_saved_credentials() {
        let state = StateDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            credential: "stored-wrong-token".into(),
            server: "http://another-server.invalid".into(),
            ..Identity::default()
        }
        .save(&state.0)
        .unwrap();
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "explicit-token".into(),
            state_dir: state.0.clone(),
            ..Config::default()
        };
        let task = tokio::spawn(listen(
            config,
            "hermes".into(),
            "http://unused.invalid".into(),
            String::new(),
        ));
        let (_socket, token) = relay_peer(&listener).await;
        task.abort();
        let _ = task.await;
        assert_eq!(token, "Bearer explicit-token");
    }

    #[tokio::test]
    async fn listen_reconnects_after_an_initial_dial_failure_instead_of_returning() {
        let state = StateDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let config = Config {
            server: format!("http://{address}"),
            token: "explicit-token".into(),
            state_dir: state.0.clone(),
            ..Config::default()
        };
        let task = tokio::spawn(listen(
            config,
            "hermes".into(),
            "http://unused.invalid".into(),
            String::new(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "listen terminated instead of scheduling a reconnect"
        );
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let (_socket, _) = relay_peer(&listener).await;
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn listen_ignores_foreign_targets_and_preserves_reply_to_in_the_webhook() {
        let state = StateDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (url, mut posts, http) = http_peer(vec![(200, "{}"), (200, "{}")]).await;
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "explicit-token".into(),
            state_dir: state.0.clone(),
            ..Config::default()
        };
        let task = tokio::spawn(listen(config, "hermes".into(), url, "secret".into()));
        let (mut socket, _) = relay_peer(&listener).await;
        let registration = json_frame(&mut socket).await;
        assert_eq!(registration["type"], "register");
        socket
            .send(Message::text(json!({"type":"welcome"}).to_string()))
            .await
            .unwrap();
        assert_eq!(json_frame(&mut socket).await["type"], "snapshot");
        for (target, body, id) in [
            ("listen/foreign", "wrong-target", "foreign-id"),
            ("listen/hermes", "right-target", "own-id"),
        ] {
            socket.send(Message::text(json!({"type":"deliver","session_id":target,"from":"Display label","reply_to":"pi/reply-target","body":body,"message_id":id}).to_string())).await.unwrap();
        }
        let (_, raw) = tokio::time::timeout(Duration::from_secs(3), posts.recv())
            .await
            .unwrap()
            .unwrap();
        let payload: Value = serde_json::from_slice(&raw).unwrap();
        let receipt = json_frame(&mut socket).await;
        assert_eq!(
            receipt,
            json!({"type":"receipt","message_id":"own-id","from":"listen/hermes","status":"delivered"})
        );
        assert!(posts.try_recv().is_err());
        task.abort();
        let _ = task.await;
        http.abort();
        assert_eq!(payload["message"]["body"], "right-target");
        assert_eq!(payload["message"]["from"], "Display label");
        assert_eq!(payload["message"]["from_session"], "pi/reply-target");
    }

    #[tokio::test]
    async fn listen_refuses_remote_deliveries_without_any_http_request_under_refuse() {
        let state = StateDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (url, mut posts, http) = http_peer(vec![]).await;
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "explicit-token".into(),
            state_dir: state.0.clone(),
            remote_messages: "refuse".into(),
            ..Config::default()
        };
        let task = tokio::spawn(listen(config, "hermes".into(), url, "secret".into()));
        let (mut socket, _) = relay_peer(&listener).await;
        assert_eq!(json_frame(&mut socket).await["type"], "register");
        socket
            .send(Message::text(json!({"type":"welcome"}).to_string()))
            .await
            .unwrap();
        assert_eq!(json_frame(&mut socket).await["type"], "snapshot");
        socket
            .send(Message::text(
                json!({"type":"deliver","session_id":"listen/hermes","from":"pi/remote","reply_to":"pi/remote","message_id":"refused-id","body":"must not leave the machine"})
                    .to_string(),
            ))
            .await
            .unwrap();
        let receipt = json_frame(&mut socket).await;
        assert_eq!(
            receipt,
            json!({"type":"receipt","message_id":"refused-id","from":"listen/hermes","status":"refused"})
        );
        assert!(
            posts.try_recv().is_err(),
            "webhook endpoint received an HTTP request under remote_messages=refuse"
        );
        task.abort();
        let _ = task.await;
        http.abort();
    }

    #[tokio::test]
    async fn enroll_uses_hostname_and_never_sends_another_servers_saved_credential() {
        for same_server in [true, false] {
            let state = StateDir::new();
            let (url, mut posts, http) = http_peer(vec![(401, "invalid enroll token")]).await;
            Identity {
                device_id: uuid::Uuid::new_v4().to_string(),
                credential: "saved-device-secret".into(),
                tenant_id: "tenant".into(),
                server: if same_server {
                    url.clone()
                } else {
                    "http://other-server.invalid".into()
                },
            }
            .save(&state.0)
            .unwrap();
            let config = Config {
                server: url,
                token: "bootstrap".into(),
                name: String::new(),
                state_dir: state.0.clone(),
                ..Config::default()
            };
            assert!(enroll(&config, false)
                .await
                .unwrap_err()
                .contains("invalid enroll token"));
            let (headers, raw) = posts.recv().await.unwrap();
            assert_eq!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer saved-device-secret"),
                same_server
            );
            let request: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(request["name"], super::super::config::hostname());
            assert_eq!(request["token"], "bootstrap");
            http.await.unwrap();
        }
    }

    #[tokio::test]
    async fn enroll_new_identity_preserves_the_old_device_identity_on_any_failure() {
        let state = StateDir::new();
        Identity {
            device_id: "old-device-id".into(),
            credential: "old-credential".into(),
            tenant_id: "old-tenant".into(),
            server: "http://old-server.invalid".into(),
        }
        .save(&state.0)
        .unwrap();
        let saved = || -> Value {
            serde_json::from_slice(&std::fs::read(state.0.join("device.json")).unwrap()).unwrap()
        };
        let config = |server: &str| Config {
            server: server.into(),
            token: "enroll-token".into(),
            state_dir: state.0.clone(),
            ..Config::default()
        };
        // HTTP failure from the enrollment server.
        let (url, mut posts, http) = http_peer(vec![(401, "expired")]).await;
        let error = enroll(&config(&url), true).await.unwrap_err();
        assert!(error.contains("enroll"), "{error}");
        assert_eq!(saved()["device_id"], "old-device-id");
        assert_eq!(saved()["credential"], "old-credential");
        assert!(posts.recv().await.is_some());
        http.await.unwrap();
        // A 201 whose body is not a valid complete enrollment must not
        // destroy the old identity either.
        let (url, mut posts, http) = http_peer(vec![(
            201,
            "{\"device_id\":\"other-device\",\"credential\":\"\"}",
        )])
        .await;
        let error = enroll(&config(&url), true).await.unwrap_err();
        assert!(error.contains("incomplete"), "{error}");
        assert_eq!(saved()["device_id"], "old-device-id");
        assert_eq!(saved()["credential"], "old-credential");
        assert!(posts.recv().await.is_some());
        http.await.unwrap();
    }

    #[tokio::test]
    async fn enroll_new_identity_replaces_device_json_only_after_a_valid_enrollment() {
        let state = StateDir::new();
        Identity {
            device_id: "old-device-id".into(),
            credential: "old-credential".into(),
            tenant_id: "old-tenant".into(),
            server: "http://old-server.invalid".into(),
        }
        .save(&state.0)
        .unwrap();
        // Echo peer: answers 201 with the candidate device_id plus fresh
        // credentials, like a real enrollment server.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut raw = vec![0; length];
            reader.read_exact(&mut raw).await.unwrap();
            let request: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(request["name"], super::super::config::hostname());
            let body = json!({"device_id":request["device_id"],"credential":"new-credential","tenant_id":"new-tenant"})
                .to_string();
            reader
                .get_mut()
                .write_all(format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await
                .unwrap();
        });
        let config = Config {
            server: format!("http://{address}"),
            token: "enroll-token".into(),
            state_dir: state.0.clone(),
            ..Config::default()
        };
        enroll(&config, true).await.unwrap();
        server.await.unwrap();
        let saved: Value =
            serde_json::from_slice(&std::fs::read(state.0.join("device.json")).unwrap()).unwrap();
        assert_ne!(saved["device_id"], "old-device-id");
        assert!(uuid::Uuid::parse_str(saved["device_id"].as_str().unwrap()).is_ok());
        assert_eq!(saved["credential"], "new-credential");
        assert_eq!(saved["tenant_id"], "new-tenant");
        assert_eq!(saved["server"], format!("http://{address}"));
    }

    #[tokio::test]
    async fn webhook_created_at_is_rfc3339_and_empty_secret_adds_no_signature() {
        let (url, mut posts, http) = http_peer(vec![(200, "{}")]).await;
        post_webhook(
            &Config::default(),
            &url,
            "",
            "hmac",
            "hermes",
            "sender",
            "body",
            "fixed-id",
        )
        .await
        .unwrap();
        let (headers, raw) = posts.recv().await.unwrap();
        http.await.unwrap();
        let payload: Value = serde_json::from_slice(&raw).unwrap();
        assert!(
            payload["message"]["created_at"].is_string(),
            "Go webhook created_at is RFC3339, not integer milliseconds"
        );
        assert!(!headers
            .to_ascii_lowercase()
            .contains("x-intercom-signature"));
        assert!(!headers.to_ascii_lowercase().contains("x-hub-signature-256"));
    }

    #[tokio::test]
    async fn webhook_retries_transient_http_errors_with_identical_body_id_and_signature() {
        let (url, mut posts, http) = http_peer(vec![
            (500, "{}"),
            (429, "{}"),
            (200, "{\"status\":\"duplicate\"}"),
        ])
        .await;
        let result = post_webhook(
            &Config::default(),
            &url,
            "secret",
            "hmac",
            "hermes",
            "sender",
            "body",
            "fixed-id",
        )
        .await;
        assert_eq!(result.unwrap(), "delivered");
        let first = posts.recv().await.unwrap();
        for _ in 0..2 {
            assert_eq!(posts.recv().await.unwrap(), first);
        }
        http.await.unwrap();
    }

    #[tokio::test]
    async fn listen_rejects_cross_server_saved_credentials_without_a_connection() {
        let state = StateDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            credential: "must-not-leak".into(),
            server: "http://another-server.invalid".into(),
            ..Identity::default()
        }
        .save(&state.0)
        .unwrap();
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            state_dir: state.0.clone(),
            ..Config::default()
        };
        // Liveness only: a dial would park in the listener's backlog, which
        // the accept probe below catches.
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            listen(
                config,
                "hermes".into(),
                "http://unused.invalid".into(),
                String::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.contains("selected server"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn listen_uses_saved_credentials_only_for_the_same_normalized_server() {
        let state = StateDir::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            credential: "bound-token".into(),
            server: format!("{server}///"),
            ..Identity::default()
        }
        .save(&state.0)
        .unwrap();
        let config = Config {
            server,
            state_dir: state.0.clone(),
            ..Config::default()
        };
        let task = tokio::spawn(listen(
            config,
            "hermes".into(),
            "http://unused.invalid".into(),
            String::new(),
        ));
        let (mut socket, token) = relay_peer(&listener).await;
        let registration = json_frame(&mut socket).await;
        task.abort();
        let _ = task.await;
        assert_eq!(token, "Bearer bound-token");
        assert!(
            registration.get("sessions").is_none(),
            "registration must be the sole pre-welcome application frame"
        );
        assert_eq!(
            registration["machine_id"],
            format!(
                "listen-hermes-{}-{}",
                super::super::config::hostname(),
                std::process::id()
            )
        );
    }

    #[test]
    fn listen_backoff_resets_only_after_healthy_session_and_types_auth_errors() {
        let denied = TransportError::Http {
            status: 401,
            reason: "revoked".into(),
        };
        let remote = TransportError::Remote {
            code: "unauthenticated".into(),
            reason: "expired".into(),
        };
        for error in [denied, remote] {
            assert_eq!(
                listen_retry_delay(Duration::from_secs(30), false, &error),
                (Duration::from_secs(300), Duration::from_secs(30))
            );
        }
        for error in [
            TransportError::Http {
                status: 403,
                reason: "forbidden".into(),
            },
            TransportError::Io("HTTP 401 is only text".into()),
        ] {
            for _ in 0..64 {
                let (sleep, next) = listen_retry_delay(Duration::from_secs(30), true, &error);
                assert!((Duration::from_millis(750)..=Duration::from_millis(1250)).contains(&sleep));
                assert_eq!(next, Duration::from_secs(2));
                let (sleep, next) = listen_retry_delay(Duration::from_secs(30), false, &error);
                assert!(
                    (Duration::from_millis(22500)..=Duration::from_millis(37500)).contains(&sleep)
                );
                assert_eq!(next, Duration::from_secs(30));
            }
        }
    }

    #[tokio::test]
    async fn webhook_client_errors_and_ignored_are_final_without_retry() {
        // No wall-clock bound: the peer answers once and closes, so a retry
        // (after a >=1s backoff) would end in a connect error, not `expected`.
        for (status, body, expected) in [
            (400, "{}", "HTTP 400"),
            (200, "{\"status\":\"ignored\"}", "ignored"),
        ] {
            let (url, mut posts, http) = http_peer(vec![(status, body)]).await;
            let result = post_webhook(
                &Config::default(),
                &url,
                "secret",
                "hmac",
                "hermes",
                "sender",
                "body",
                "fixed-id",
            )
            .await;
            let error = result.unwrap_err();
            assert!(
                error.contains(expected),
                "final response scheduled a retry: {error}"
            );
            assert!(posts.recv().await.is_some());
            http.await.unwrap();
            assert!(posts.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn webhook_network_failure_retries_and_generated_wire_identity_is_stable() {
        let (url, mut posts, http) = http_peer(vec![(0, ""), (200, "{}")]).await;
        post_webhook(
            &Config::default(),
            &url,
            "secret",
            "bearer",
            "hermes",
            "",
            "body",
            "",
        )
        .await
        .unwrap();
        let first = posts.recv().await.unwrap();
        assert_eq!(posts.recv().await.unwrap(), first);
        http.await.unwrap();
        let payload: Value = serde_json::from_slice(&first.1).unwrap();
        let mid = payload["message"]["id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(mid).is_ok());
        assert!(first
            .0
            .to_ascii_lowercase()
            .contains(&format!("x-request-id: {mid}")));
        assert_eq!(payload["message"]["from"], "pi-famulus");
        assert!(payload["message"].get("from_session").is_none());
        assert!(first
            .0
            .to_ascii_lowercase()
            .contains("authorization: bearer secret"));
        assert!(!first.0.to_ascii_lowercase().contains("signature"));
        assert_eq!(webhook_timestamp(951827696123), "2000-02-29T12:34:56Z");
    }

    #[cfg(feature = "test-clock")]
    async fn wait_timer(clock: &crate::clock::Clock, label: &str) -> u64 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(timer) = clock
                .manual()
                .unwrap()
                .status()
                .pending
                .into_iter()
                .find(|timer| timer.label == label && timer.due_in_ms > 0)
            {
                return timer.due_in_ms;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timer never armed: {label}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[cfg(feature = "test-clock")]
    #[tokio::test]
    async fn manual_webhook_retry_waits_one_then_three_seconds_and_cancellation_releases_timer() {
        let clock = crate::clock::Clock::manual_for_test();
        let (url, mut posts, http) = http_peer(vec![(500, "{}"), (429, "{}"), (200, "{}")]).await;
        let timer = clock.clone();
        let task = tokio::spawn(async move {
            let endpoint = super::super::config::Endpoint {
                name: "hermes".into(),
                url,
                secret: "secret".into(),
                auth: "hmac".into(),
            };
            let message = WebhookMessage {
                id: "fixed-id",
                from: "Display label",
                reply_to: "pi/reply-target",
                body: "body",
            };
            post_webhook_message(&Config::default(), &endpoint, &message, &timer).await
        });
        let first = posts.recv().await.unwrap();
        assert_eq!(wait_timer(&clock, "intercom_webhook_retry").await, 1000);
        clock.manual().unwrap().advance(999);
        tokio::task::yield_now().await;
        assert!(posts.try_recv().is_err());
        clock.manual().unwrap().advance(1);
        assert_eq!(posts.recv().await.unwrap(), first);
        assert_eq!(wait_timer(&clock, "intercom_webhook_retry").await, 3000);
        clock.manual().unwrap().advance(2999);
        tokio::task::yield_now().await;
        assert!(posts.try_recv().is_err());
        clock.manual().unwrap().advance(1);
        assert_eq!(posts.recv().await.unwrap(), first);
        assert_eq!(task.await.unwrap().unwrap(), "delivered");
        http.await.unwrap();
        assert!(clock.manual().unwrap().status().pending.is_empty());
        let (url, mut posts, http) = http_peer(vec![(500, "{}"), (200, "{}")]).await;
        let timer = clock.clone();
        let task = tokio::spawn(async move {
            let endpoint = super::super::config::Endpoint {
                name: "hermes".into(),
                url,
                secret: String::new(),
                auth: "hmac".into(),
            };
            post_webhook_message(
                &Config::default(),
                &endpoint,
                &WebhookMessage {
                    id: "cancelled",
                    from: "sender",
                    reply_to: "",
                    body: "body",
                },
                &timer,
            )
            .await
        });
        posts.recv().await.unwrap();
        assert_eq!(wait_timer(&clock, "intercom_webhook_retry").await, 1000);
        task.abort();
        let _ = task.await;
        http.abort();
        assert!(clock.manual().unwrap().status().pending.is_empty());
    }

    #[cfg(feature = "test-clock")]
    #[tokio::test]
    async fn manual_listen_http_and_registration_auth_failures_wait_five_minutes_then_reconnect() {
        for remote in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let clock = crate::clock::Clock::manual_for_test();
            let timer = clock.clone();
            let config = Config {
                server: format!("http://{}", listener.local_addr().unwrap()),
                token: "explicit-token".into(),
                ..Config::default()
            };
            let task = tokio::spawn(async move {
                let identity = Identity {
                    device_id: uuid::Uuid::new_v4().to_string(),
                    ..Identity::default()
                };
                let endpoint = super::super::config::Endpoint {
                    name: "hermes".into(),
                    url: "http://unused.invalid".into(),
                    secret: String::new(),
                    auth: "hmac".into(),
                };
                listen_loop(&config, &identity, "hermes", &endpoint, &timer).await;
            });
            if remote {
                let (mut socket, _) = relay_peer(&listener).await;
                assert_eq!(json_frame(&mut socket).await["type"], "register");
                socket
                    .send(Message::text(
                        json!({"type":"error","code":"unauthenticated","message":"expired device"})
                            .to_string(),
                    ))
                    .await
                    .unwrap();
            } else {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                reader.get_mut().write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 14\r\nConnection: close\r\n\r\nexpired device").await.unwrap();
            }
            assert_eq!(
                wait_timer(&clock, "intercom_listen_reconnect").await,
                300000
            );
            clock.manual().unwrap().advance(299999);
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err()
            );
            clock.manual().unwrap().advance(1);
            let (mut socket, _) = relay_peer(&listener).await;
            let registration = json_frame(&mut socket).await;
            assert!(registration.get("sessions").is_none());
            socket
                .send(Message::text(json!({"type":"welcome"}).to_string()))
                .await
                .unwrap();
            let snapshot = json_frame(&mut socket).await;
            assert_eq!(snapshot["sessions"][0]["status"], "online");
            task.abort();
            let _ = task.await;
            assert!(!clock
                .manual()
                .unwrap()
                .status()
                .pending
                .iter()
                .any(|timer| timer.label == "intercom_listen_reconnect"));
        }
    }

    #[cfg(feature = "test-clock")]
    #[tokio::test]
    async fn manual_listen_relay_that_welcomes_then_drops_keeps_backing_off() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let clock = crate::clock::Clock::manual_for_test();
        let timer = clock.clone();
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "explicit-token".into(),
            ..Config::default()
        };
        let task = tokio::spawn(async move {
            let identity = Identity {
                device_id: uuid::Uuid::new_v4().to_string(),
                ..Identity::default()
            };
            let endpoint = super::super::config::Endpoint {
                name: "hermes".into(),
                url: "http://unused.invalid".into(),
                secret: String::new(),
                auth: "hmac".into(),
            };
            listen_loop(&config, &identity, "hermes", &endpoint, &timer).await;
        });
        let mut waits = Vec::new();
        for _ in 0..3 {
            let (mut socket, _) = relay_peer(&listener).await;
            assert_eq!(json_frame(&mut socket).await["type"], "register");
            socket
                .send(Message::text(json!({"type":"welcome"}).to_string()))
                .await
                .unwrap();
            assert_eq!(json_frame(&mut socket).await["type"], "snapshot");
            socket.close(None).await.unwrap();
            let wait = wait_timer(&clock, "intercom_listen_reconnect").await;
            waits.push(wait);
            clock.manual().unwrap().advance(wait);
        }
        task.abort();
        let _ = task.await;
        assert!(
            waits[2] >= 3000,
            "welcome-then-drop reset the listen backoff: {waits:?}"
        );
    }

    #[cfg(feature = "test-clock")]
    async fn held_webhook() -> (
        String,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (seen, received) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            seen.send(()).unwrap();
            if wait.await.is_ok() {
                let _ = reader
                    .get_mut()
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
            }
        });
        (format!("http://{address}/hook"), received, release, task)
    }

    #[cfg(feature = "test-clock")]
    #[tokio::test]
    async fn manual_listen_keeps_independent_pings_during_slow_webhook_and_cancels_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let clock = crate::clock::Clock::manual_for_test();
        let timer = clock.clone();
        let (url, received, _release, http) = held_webhook().await;
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "explicit-token".into(),
            ..Config::default()
        };
        let task = tokio::spawn(async move {
            let identity = Identity {
                device_id: uuid::Uuid::new_v4().to_string(),
                ..Identity::default()
            };
            let endpoint = super::super::config::Endpoint {
                name: "hermes".into(),
                url,
                secret: String::new(),
                auth: "hmac".into(),
            };
            listen_loop(&config, &identity, "hermes", &endpoint, &timer).await;
        });
        let (mut socket, _) = relay_peer(&listener).await;
        assert_eq!(json_frame(&mut socket).await["type"], "register");
        socket
            .send(Message::text(json!({"type":"welcome"}).to_string()))
            .await
            .unwrap();
        assert_eq!(json_frame(&mut socket).await["type"], "snapshot");
        socket.send(Message::text(json!({"type":"deliver","session_id":"listen/hermes","message_id":"slow-hook","body":"body"}).to_string())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wait_timer(&clock, "intercom-relay-ping").await, 20000);
        for advance in [20000, 300000] {
            clock.manual().unwrap().advance(advance);
            let ping = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                matches!(ping, Message::Ping(_)),
                "webhook handler prevented independent protocol pings: {ping:?}"
            );
            assert_eq!(wait_timer(&clock, "intercom-relay-ping").await, 20000);
        }
        task.abort();
        let _ = task.await;
        let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("cancelled listen retained its socket/ping task");
        assert!(matches!(
            closed,
            None | Some(Err(_)) | Some(Ok(Message::Close(_)))
        ));
        http.abort();
        let _ = http.await;
    }

    #[cfg(feature = "test-clock")]
    #[tokio::test]
    async fn manual_listen_reconnects_when_relay_closes_during_webhook_receipt() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let clock = crate::clock::Clock::manual_for_test();
        let timer = clock.clone();
        let (url, received, release, http) = held_webhook().await;
        let config = Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "explicit-token".into(),
            ..Config::default()
        };
        let task = tokio::spawn(async move {
            let identity = Identity {
                device_id: uuid::Uuid::new_v4().to_string(),
                ..Identity::default()
            };
            let endpoint = super::super::config::Endpoint {
                name: "hermes".into(),
                url,
                secret: String::new(),
                auth: "hmac".into(),
            };
            listen_loop(&config, &identity, "hermes", &endpoint, &timer).await;
        });
        let (mut socket, _) = relay_peer(&listener).await;
        assert_eq!(json_frame(&mut socket).await["type"], "register");
        socket
            .send(Message::text(json!({"type":"welcome"}).to_string()))
            .await
            .unwrap();
        assert_eq!(json_frame(&mut socket).await["type"], "snapshot");
        socket.send(Message::text(json!({"type":"deliver","session_id":"listen/hermes","message_id":"closed-relay","body":"body"}).to_string())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        socket.close(None).await.unwrap();
        drop(socket);
        release.send(()).unwrap();
        http.await.unwrap();
        let delay = wait_timer(&clock, "intercom_listen_reconnect").await;
        assert!((750..=1250).contains(&delay));
        clock.manual().unwrap().advance(delay);
        let (mut socket, _) = relay_peer(&listener).await;
        assert_eq!(json_frame(&mut socket).await["type"], "register");
        socket
            .send(Message::text(json!({"type":"welcome"}).to_string()))
            .await
            .unwrap();
        assert_eq!(
            json_frame(&mut socket).await["sessions"][0]["status"],
            "online"
        );
        task.abort();
        let _ = task.await;
    }
}
