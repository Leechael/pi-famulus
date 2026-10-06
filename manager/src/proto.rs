//! Wire protocol: frame codec and message types (design doc §3.3).
//!
//! Frame format: `u32 BE length + UTF-8 JSON payload`, max frame 4 MiB.
//! Requests:  `{"v":1, "id":"<uuid>", "type":"...", ...}`
//! Responses: `{"v":1, "id":"<uuid>", "ok":true, ...}` or
//!            `{"v":1, "id":"...", "ok":false, "error":{"code":"E_*","message":"..."}}`
//! Events (server push, no id): `{"v":1, "type":"event", "event":"...", ...}`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTO_VERSION: u32 = 1;
/// Feature level of the protocol, exchanged in hello (`protocol`) and
/// returned by `status`. 1 = original §3.3; 2 = observability contract
/// (origin, mark_background, stop.reason, end_reason, events.jsonl); 3 =
/// in-place upgrade (`upgrade`, status `generation`/`last_upgrade`, start
/// keys, resend on reconnect); 4 = machine-wide agent admission; 5 = per-kind
/// agent budgets and queued acquire.
pub const PROTOCOL: u32 = 5;
/// The first protocol level whose manager understands `upgrade`.
pub const PROTOCOL_UPGRADE: u32 = 3;
/// §3.3: max frame 4 MiB.
pub const MAX_FRAME_SIZE: u32 = 4 * 1024 * 1024;

// Error codes (§3.3).
pub const E_NOT_FOUND: &str = "E_NOT_FOUND";
pub const E_BAD_REQUEST: &str = "E_BAD_REQUEST";
pub const E_VERSION: &str = "E_VERSION";
pub const E_SESSION_REQUIRED: &str = "E_SESSION_REQUIRED";
pub const E_FORBIDDEN: &str = "E_FORBIDDEN";
pub const E_INTERNAL: &str = "E_INTERNAL";
/// `E_INTERNAL` message for requests (hello included) during graceful
/// shutdown. Clients wait for that manager to exit, then spawn a successor.
pub const SHUTTING_DOWN: &str = "manager is shutting down";

/// §3.3: signals travel as names ("SIGTERM", "SIGKILL"). Unknown numbers
/// render as "SIG<n>".
pub fn signal_name(sig: i32) -> String {
    let name = match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => return format!("SIG{sig}"),
    };
    name.to_string()
}

/// Accept a signal as a name (current format) or a number (records written
/// before signal names, which are converted).
fn de_signal<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Sig {
        Name(String),
        Num(i32),
    }
    Ok(match Option::<Sig>::deserialize(d)? {
        None => None,
        Some(Sig::Name(s)) => Some(s),
        Some(Sig::Num(n)) => Some(signal_name(n)),
    })
}

/// Epoch milliseconds; used for started_at/ended_at/ts fields everywhere.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Shared enums & records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Extension,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Shell,
    Monitor,
}

impl TaskKind {
    /// §3.3: task_id prefix — `sh` (shell) / `mon` (monitor).
    pub fn prefix(self) -> &'static str {
        match self {
            TaskKind::Shell => "sh",
            TaskKind::Monitor => "mon",
        }
    }
}

/// §3.4 task state machine: running -> completed | failed | killed | orphaned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
    Orphaned,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

/// Who asked for a task (observability contract): how the extension ran it,
/// and for shells run by a subagent, which child and run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// "bash-fg" | "bash-bg" | "child-bash" | "monitor". Stored as sent, so a
    /// newer extension adding a value never breaks `start`.
    pub via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Why a task ended (TaskRecord.end_reason, task_exited.end_reason).
pub mod end_reason {
    pub const EXITED: &str = "exited";
    pub const TIMEOUT: &str = "timeout";
    pub const SESSION_END: &str = "session-end";
    pub const MANAGER_SHUTDOWN: &str = "manager-shutdown";
    /// A record left "running" by a daemon that died without shutting down
    /// (kill -9, panic). Its runners took the tasks down (lifeline, §3.2);
    /// the next daemon only marks the record.
    pub const MANAGER_CRASH: &str = "manager-crash";
}

/// `stop.reason` values accepted on the wire.
pub const STOP_REASONS: &[&str] = &["tui", "cli", "tool", "timeout", "rate-limit", "session-end"];

/// Map a `stop.reason` to the task's end_reason: `stopped:<reason>`, except
/// timeout / rate-limit / session-end, which map to themselves. A stop with
/// no reason (older clients) is `stopped:tool`.
pub fn end_reason_for_stop(reason: Option<&str>) -> String {
    match reason {
        Some(r @ ("timeout" | "rate-limit" | "session-end")) => r.to_string(),
        Some(r) => format!("stopped:{r}"),
        None => "stopped:tool".to_string(),
    }
}

/// §3.4: TaskRecord persisted at sessions/<sid>/tasks/<task_id>.json.
/// Fields after `output_size` were added by the observability contract; they
/// are optional so records written by older managers still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub session_id: String,
    pub kind: TaskKind,
    pub command: String,
    pub cwd: String,
    pub pid: u32,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    /// Terminating signal name, e.g. "SIGTERM" / "SIGKILL" (§3.3). Records
    /// written by older managers stored the number; those still load.
    #[serde(default, deserialize_with = "de_signal")]
    pub signal: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub output_path: String,
    pub output_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// When the extension moved the task to the background (ms epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backgrounded_at: Option<u64>,
    /// See [`end_reason`]; set when the task reaches a terminal status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// CPU time of the command and every descendant it waited for, set at
    /// exit from the runner's report (`crate::runner`). Absent while the
    /// task runs, for records from older managers, and when the runner
    /// could not report (it was SIGKILLed with its group: `timeout_ms`, a
    /// stop that outlived its grace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_user_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_sys_ms: Option<u64>,
    /// Peak RSS of the single largest process in that set, in KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rss_kb: Option<u64>,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// §3.3 framing says requests carry `"v":1`, but the doc's hello example
    /// omits it. Lenient read: absent == current version; present must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<u32>,
    /// §3.3 hello example also omits id; it is then echoed as "".
    #[serde(default)]
    pub id: String,
    #[serde(flatten)]
    pub kind: RequestKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestKind {
    /// §3.3 hello — must be the first message on a connection.
    Hello {
        client_kind: ClientKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pi_pid: Option<u32>,
        /// Session working directory. Optional so older clients still hello.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Extension package version, stored per session (doctor, sessions).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension_version: Option<String>,
        /// Protocol level the client speaks; see [`PROTOCOL`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol: Option<u32>,
    },
    /// §3.3 start — env is the child's *complete* environment.
    Start {
        kind: TaskKind,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        run_in_background: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
        /// Client-chosen idempotency key. A start resent with the same key
        /// (after a lost connection, e.g. an in-place upgrade) returns the
        /// task the first one started instead of starting it again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Record that the extension moved a task to the background.
    MarkBackground {
        task_id: String,
    },
    Wait {
        task_id: String,
        budget_ms: u64,
    },
    Output {
        task_id: String,
        cursor: u64,
        max_bytes: u64,
    },
    Stop {
        task_id: String,
        /// One of [`STOP_REASONS`]; absent = "tool" (older clients).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    List {
        #[serde(default)]
        all: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Page the answer to fit a frame; `next` in the answer is the
        /// `after` of the following page. Without it the whole list comes
        /// in one frame, or not at all past 4 MiB.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        paged: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    Watch {
        task_id: String,
    },
    Unwatch {
        task_id: String,
    },
    ShutdownSession,
    Status,
    /// Request an idempotent machine-wide agent permit.
    AcquireAgent {
        child_id: String,
        #[serde(default)]
        work_kind: String,
    },
    /// Cancel a queued acquire by its original request id.
    CancelAcquireAgent {
        request_id: String,
    },
    /// Return the child-owned agent permit.
    ReleaseAgent {
        child_id: String,
    },
    Shutdown,
    /// CLI only: replace this daemon in place with the binary now at its
    /// executable path (exec, same pid; see `handover.rs`). Answered before
    /// the handover starts; the result shows in `status.last_upgrade`.
    Upgrade,
    /// Test-only (`test-clock` feature): pending manual-clock timers. Sent
    /// as the first frame of a connection, without hello, so it never counts
    /// as an active connection.
    #[cfg(feature = "test-clock")]
    ClockStatus,
    /// Test-only (`test-clock` feature): advance the manual clock.
    #[cfg(feature = "test-clock")]
    ClockAdvance {
        ms: u64,
    },
    /// Test-only (`test-clock` feature): end the daemon on the spot the way
    /// a panic in its main future does (exit status 101, no shutdown path,
    /// no destructors). Sent without hello.
    #[cfg(feature = "test-clock")]
    DebugCrash,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Generic response envelope; `T` is the success payload (flattened).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response<T> {
    pub v: u32,
    pub id: String,
    pub ok: bool,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoError {
    pub code: String,
    pub message: String,
}

impl ProtoError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        ProtoError {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ProtoError,
}

/// Empty success payload: renders as just `{"v":1,"id":"...","ok":true}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitOk {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloOk {
    pub version: String,
    pub pid: u32,
    pub started_at: u64,
    /// Maximum protocol level supported by this daemon.
    #[serde(default)]
    pub protocol: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartOk {
    pub task_id: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitOk {
    pub done: bool,
    /// §3.3: present on done:true; omitted when the budget expired (done:false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputOk {
    /// UTF-8 lossy chunk (§3.3: v1 has no binary fidelity).
    pub chunk: String,
    pub next_cursor: u64,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    pub total_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOk {
    pub tasks: Vec<TaskRecord>,
    /// More records follow a paged answer: pass this as the next `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownSessionOk {
    pub stopped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub pi_pid: u32,
    pub connected: bool,
    /// Present when the extension sent cwd on hello. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
    /// Protocol level the session's client announced (absent: older client).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    /// First hello of this session seen by this manager (ms epoch).
    #[serde(default)]
    pub connected_at: u64,
    /// Last request or disconnect (ms epoch); "now" while connected.
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCapacity {
    pub used: usize,
    pub total: usize,
    #[serde(default)]
    pub by_kind: HashMap<String, AgentKindCapacity>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentKindCapacity { pub used: usize, pub total: usize }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAdmissionOk {
    pub granted: bool,
    /// Extensible reason code, e.g. `global_capacity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCounts {
    pub running: usize,
    pub terminal: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusOk {
    pub version: String,
    pub pid: u32,
    pub uptime_ms: u64,
    pub sessions: Vec<SessionInfo>,
    pub task_counts: TaskCounts,
    /// Machine-wide active subagent permits / configured maximum.
    #[serde(default)]
    pub agent_capacity: AgentCapacity,
    /// The manager's protocol level ([`PROTOCOL`]).
    #[serde(default)]
    pub protocol: u32,
    /// In-place upgrades this daemon (this pid) has gone through.
    #[serde(default)]
    pub generation: u32,
    /// The latest in-place upgrade attempt, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_upgrade: Option<UpgradeInfo>,
    /// The daemon's binary: the file an in-place upgrade execs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// Outcome of an in-place upgrade attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpgradeInfo {
    /// When it finished (ms epoch).
    pub at: u64,
    pub ok: bool,
    pub from_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    /// Why it did not happen; the daemon kept running the old binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What started it: "cli" (`pi-famulus upgrade`) or "binary-changed".
    #[serde(default)]
    pub trigger: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeOk {
    pub from_version: String,
    pub generation: u32,
}

// ---------------------------------------------------------------------------
// Events (server push, no id)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    #[serde(rename = "type")]
    pub msg_type: String, // always "event"
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(kind: EventKind) -> Self {
        Event {
            v: PROTO_VERSION,
            msg_type: "event".to_string(),
            kind,
        }
    }
}

/// §3.3 event table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    /// Always pushed to the owning session.
    TaskStarted {
        task_id: String,
        kind: TaskKind,
        command: String,
        pid: u32,
        ts: u64,
    },
    /// Only pushed after `watch`.
    Output {
        task_id: String,
        chunk: String,
        next_cursor: u64,
    },
    /// Always pushed to the owning session.
    TaskExited {
        task_id: String,
        exit_code: Option<i32>,
        /// Signal name ("SIGTERM"/"SIGKILL"/...), null when the task exited.
        signal: Option<String>,
        duration_ms: u64,
        output_path: String,
        output_size: u64,
        ts: u64,
        /// Why the task ended; see [`end_reason`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_reason: Option<String>,
    },
    /// Pushed to the old connection when a session is rebound (§3.3 hello).
    SessionRebound {},
}

// ---------------------------------------------------------------------------
// Frame codec (§3.3)
// ---------------------------------------------------------------------------

/// Read one frame. Ok(None) = clean EOF (peer closed). Errors on oversized
/// frames and on EOF mid-payload.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds 4 MiB limit"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Write one frame (length-prefixed, flushed).
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload {} bytes exceeds 4 MiB limit", payload.len()),
        ));
    }
    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// Serialize a message to a frame payload. Infallible for the types in this
/// module (no custom serializers, string-keyed maps only).
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).expect("protocol message serialization is infallible")
}

// ---------------------------------------------------------------------------
// id / randomness helpers
// ---------------------------------------------------------------------------

/// Fill buf from /dev/urandom, falling back to a time/pid mix (unix targets).
pub fn random_bytes(buf: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback: hash of time + pid + address — good enough for id uniqueness.
    let mut x = now_ms() ^ (std::process::id() as u64) << 32 ^ (buf.as_ptr() as usize as u64);
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
}

/// RFC-4122 v4-shaped request id (§3.3: `"id":"<uuid>"`).
pub fn new_request_id() -> String {
    let mut b = [0u8; 16];
    random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = duplex(64 * 1024);
        let payload = br#"{"v":1,"id":"x","type":"status"}"#;
        write_frame(&mut a, payload).await.unwrap();
        let got = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, payload);
        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none()); // clean EOF
    }

    #[tokio::test]
    async fn frame_over_limit_rejected() {
        let (mut a, mut b) = duplex(1024);
        // Reader rejects an announced length above 4 MiB.
        a.write_all(&(MAX_FRAME_SIZE + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Writer refuses to send an oversized payload.
        let big = vec![0u8; MAX_FRAME_SIZE as usize + 1];
        let err = write_frame(&mut a, &big).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn hello_request_json_shape() {
        let req = Request {
            v: Some(1),
            id: "abc".into(),
            kind: RequestKind::Hello {
                client_kind: ClientKind::Extension,
                session_id: Some("sess1".into()),
                pi_pid: Some(1234),
                cwd: None,
                extension_version: None,
                protocol: None,
            },
        };
        let v: serde_json::Value = serde_json::from_slice(&encode(&req)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "v": 1, "id": "abc", "type": "hello",
                "client_kind": "extension", "session_id": "sess1", "pi_pid": 1234
            })
        );
        // cli hello omits session fields entirely (§3.3).
        let req = Request {
            v: Some(1),
            id: "abc".into(),
            kind: RequestKind::Hello {
                client_kind: ClientKind::Cli,
                session_id: None,
                pi_pid: None,
                cwd: None,
                extension_version: None,
                protocol: None,
            },
        };
        let v: serde_json::Value = serde_json::from_slice(&encode(&req)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"v": 1, "id": "abc", "type": "hello", "client_kind": "cli"})
        );
    }

    #[test]
    fn hello_without_v_and_id_is_accepted() {
        // The doc's hello examples (and the black-box tests built on them)
        // send hello with no v/id envelope at all.
        let raw = br#"{"type":"hello","client_kind":"extension","session_id":"s","pi_pid":1}"#;
        let req: Request = serde_json::from_slice(raw).unwrap();
        assert_eq!(req.v, None);
        assert_eq!(req.id, "");
        assert!(matches!(req.kind, RequestKind::Hello { .. }));
        let raw = br#"{"type":"hello","client_kind":"cli"}"#;
        let req: Request = serde_json::from_slice(raw).unwrap();
        assert!(matches!(
            req.kind,
            RequestKind::Hello {
                client_kind: ClientKind::Cli,
                ..
            }
        ));
    }

    #[test]
    fn start_request_parses_doc_example() {
        // §3.3 start example, plus the v/id envelope.
        let raw = br#"{"v":1,"id":"r1","type":"start","kind":"shell","command":"ls -la",
            "cwd":"/tmp","env":{"PATH":"/bin"},"run_in_background":false,"timeout_ms":null}"#;
        let req: Request = serde_json::from_slice(raw).unwrap();
        match req.kind {
            RequestKind::Start {
                kind,
                command,
                cwd,
                env,
                run_in_background,
                timeout_ms,
                origin,
                key,
            } => {
                assert_eq!(key, None, "key is optional (older clients)");
                assert_eq!(kind, TaskKind::Shell);
                assert_eq!(command, "ls -la");
                assert_eq!(cwd.as_deref(), Some("/tmp"));
                assert_eq!(env.get("PATH").unwrap(), "/bin");
                assert!(!run_in_background);
                assert_eq!(timeout_ms, None);
                assert_eq!(origin, None, "origin is optional (older clients)");
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn error_response_shape() {
        let resp = Response {
            v: 1,
            id: "x".into(),
            ok: false,
            body: ErrorBody {
                error: ProtoError::new(E_NOT_FOUND, "no such task"),
            },
        };
        let v: serde_json::Value = serde_json::from_slice(&encode(&resp)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "v": 1, "id": "x", "ok": false,
                "error": {"code": "E_NOT_FOUND", "message": "no such task"}
            })
        );
    }

    #[test]
    fn wait_ok_omits_exit_code_when_budget_expired() {
        // §3.3: budget expiry responds {"ok":true,"done":false} — no exit_code key.
        let resp = Response {
            v: 1,
            id: "x".into(),
            ok: true,
            body: WaitOk {
                done: false,
                exit_code: None,
            },
        };
        let v: serde_json::Value = serde_json::from_slice(&encode(&resp)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"v": 1, "id": "x", "ok": true, "done": false})
        );
    }

    #[test]
    fn output_ok_keeps_null_exit_code() {
        // §3.3 output example shows "exit_code":null explicitly.
        let resp = Response {
            v: 1,
            id: "x".into(),
            ok: true,
            body: OutputOk {
                chunk: "hi".into(),
                next_cursor: 2,
                status: TaskStatus::Running,
                exit_code: None,
                total_size: 2,
            },
        };
        let v: serde_json::Value = serde_json::from_slice(&encode(&resp)).unwrap();
        assert_eq!(v["exit_code"], serde_json::Value::Null);
        assert_eq!(v["status"], serde_json::json!("running"));
    }

    #[test]
    fn event_shapes() {
        let ev = Event::new(EventKind::TaskExited {
            task_id: "sh_a1b2c3d4".into(),
            exit_code: Some(0),
            signal: None,
            duration_ms: 42,
            output_path: "/tmp/x.output".into(),
            output_size: 7,
            ts: 1726000000000,
            end_reason: None,
        });
        let v: serde_json::Value = serde_json::from_slice(&encode(&ev)).unwrap();
        assert_eq!(v["v"], serde_json::json!(1));
        assert_eq!(v["type"], serde_json::json!("event"));
        assert_eq!(v["event"], serde_json::json!("task_exited"));
        assert_eq!(v["task_id"], serde_json::json!("sh_a1b2c3d4"));
        assert!(v.get("id").is_none()); // events carry no id (§3.3)

        let ev = Event::new(EventKind::SessionRebound {});
        let v: serde_json::Value = serde_json::from_slice(&encode(&ev)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"v": 1, "type": "event", "event": "session_rebound"})
        );
    }

    #[test]
    fn signal_names_on_wire_and_legacy_numbers_load() {
        for (sig, name) in [
            (libc::SIGHUP, "SIGHUP"),
            (libc::SIGINT, "SIGINT"),
            (libc::SIGQUIT, "SIGQUIT"),
            (libc::SIGILL, "SIGILL"),
            (libc::SIGTRAP, "SIGTRAP"),
            (libc::SIGABRT, "SIGABRT"),
            (libc::SIGBUS, "SIGBUS"),
            (libc::SIGFPE, "SIGFPE"),
            (libc::SIGKILL, "SIGKILL"),
            (libc::SIGUSR1, "SIGUSR1"),
            (libc::SIGSEGV, "SIGSEGV"),
            (libc::SIGUSR2, "SIGUSR2"),
            (libc::SIGPIPE, "SIGPIPE"),
            (libc::SIGALRM, "SIGALRM"),
            (libc::SIGTERM, "SIGTERM"),
            (libc::SIGXCPU, "SIGXCPU"),
            (libc::SIGXFSZ, "SIGXFSZ"),
        ] {
            assert_eq!(signal_name(sig), name);
        }
        assert_eq!(signal_name(250), "SIG250");
        let base = serde_json::json!({
            "task_id":"sh_00000001","session_id":"s","kind":"shell","command":"x",
            "cwd":"/","pid":1,"status":"killed","exit_code":null,
            "started_at":1,"ended_at":2,"output_path":"/x","output_size":0
        });
        let with = |sig: serde_json::Value| {
            let mut v = base.clone();
            v["signal"] = sig;
            serde_json::from_value::<TaskRecord>(v).unwrap().signal
        };
        assert_eq!(with(serde_json::json!(9)).as_deref(), Some("SIGKILL"));
        assert_eq!(
            with(serde_json::json!("SIGTERM")).as_deref(),
            Some("SIGTERM")
        );
        assert_eq!(with(serde_json::Value::Null), None);
        // Missing field is fine too.
        assert_eq!(
            serde_json::from_value::<TaskRecord>(base).unwrap().signal,
            None
        );
        // And it serializes as the name.
        let ev = Event::new(EventKind::TaskExited {
            task_id: "sh_a".into(),
            exit_code: None,
            signal: Some(signal_name(libc::SIGKILL)),
            duration_ms: 1,
            output_path: "/x".into(),
            output_size: 0,
            ts: 1,
            end_reason: None,
        });
        let v: serde_json::Value = serde_json::from_slice(&encode(&ev)).unwrap();
        assert_eq!(v["signal"], serde_json::json!("SIGKILL"));
    }

    #[test]
    fn observability_fields_round_trip() {
        let raw = r#"{"v":1,"id":"a","type":"start","kind":"shell","command":"x",
            "origin":{"via":"child-bash","child_id":"ch_1","run_id":"run_1"}}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        let RequestKind::Start { origin, .. } = req.kind else {
            panic!()
        };
        let o = origin.unwrap();
        assert_eq!(
            (o.via.as_str(), o.child_id.as_deref(), o.run_id.as_deref()),
            ("child-bash", Some("ch_1"), Some("run_1"))
        );
        let req: Request =
            serde_json::from_str(r#"{"id":"b","type":"mark_background","task_id":"sh_1"}"#)
                .unwrap();
        assert!(
            matches!(req.kind, RequestKind::MarkBackground { ref task_id } if task_id == "sh_1")
        );
        let req: Request =
            serde_json::from_str(r#"{"id":"c","type":"stop","task_id":"sh_1","reason":"cli"}"#)
                .unwrap();
        assert!(matches!(req.kind, RequestKind::Stop { reason: Some(ref r), .. } if r == "cli"));
        let req: Request = serde_json::from_str(
            r#"{"type":"hello","client_kind":"extension","session_id":"s","pi_pid":1,"extension_version":"0.3.0","protocol":2}"#,
        )
        .unwrap();
        assert!(
            matches!(req.kind, RequestKind::Hello { protocol: Some(2), extension_version: Some(ref v), .. } if v == "0.3.0")
        );
        // end_reason mapping
        assert_eq!(end_reason_for_stop(Some("cli")), "stopped:cli");
        assert_eq!(end_reason_for_stop(Some("tui")), "stopped:tui");
        assert_eq!(end_reason_for_stop(Some("tool")), "stopped:tool");
        assert_eq!(end_reason_for_stop(Some("timeout")), "timeout");
        assert_eq!(end_reason_for_stop(Some("rate-limit")), "rate-limit");
        assert_eq!(end_reason_for_stop(Some("session-end")), "session-end");
        assert_eq!(end_reason_for_stop(None), "stopped:tool");
    }

    #[test]
    fn request_id_is_uuid_shaped() {
        let id = new_request_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(new_request_id(), id);
        // RFC 4122 v4: version nibble 4, variant 10xx, on every id.
        for _ in 0..200 {
            let id = new_request_id();
            let c: Vec<char> = id.chars().collect();
            assert_eq!(c[14], '4', "{id}");
            assert!(matches!(c[19], '8' | '9' | 'a' | 'b'), "{id}");
        }
    }

    #[test]
    fn task_id_prefixes() {
        assert_eq!(TaskKind::Shell.prefix(), "sh");
        assert_eq!(TaskKind::Monitor.prefix(), "mon");
    }
}
