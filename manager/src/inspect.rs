//! Inspection subcommands (observability contract): `status`, `sessions`,
//! `ls`, `show`, `agent`, `events`, plus the shared data layer they use.
//!
//! Sources: the live daemon (tasks, sessions) when it runs, TaskRecords on
//! disk otherwise; extension-owned agent records and transcripts under
//! `sessions/<sid>/agents/`; `events.jsonl` files for session history.

use crate::client::{self, HelloMode};
use crate::events::{self, EventLine};
use crate::fmt;
use crate::outln;
use crate::proto::*;
use crate::registry;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Agent records (extension-owned, read-only here)
// ---------------------------------------------------------------------------

/// `<home>/sessions/<sid>/agents/<child_id>.json`, written by the extension.
/// Every field beyond the identity is optional so older records still load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub child_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_cache_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_other_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wall_approximate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Set by the CLI (not on disk) when the record says running but its
    /// session is not connected: the child cannot be alive.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

pub fn agent_status_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted")
}

pub fn agent_output_tokens_per_second(tokens_output: Option<u64>, llm_ms: Option<u64>) -> Option<f64> {
    match (tokens_output, llm_ms) {
        (Some(tokens), Some(ms)) if ms > 0 => Some(tokens as f64 * 1000.0 / ms as f64),
        _ => None,
    }
}

impl AgentRecord {
    pub fn title(&self) -> String {
        let model = self.model.as_deref().map(|m| format!(" {m}")).unwrap_or_default();
        format!("{} ({}){model}", self.name, self.agent)
    }

    /// Transcript path: the record's own `transcript`, or the contract's
    /// default `<sid>/agents/<child_id>.jsonl`.
    pub fn transcript_path(&self, home: &Path) -> PathBuf {
        match &self.transcript {
            Some(p) => PathBuf::from(p),
            None => agents_dir(home, &self.session_id).join(format!("{}.jsonl", self.child_id)),
        }
    }
}

pub fn agents_dir(home: &Path, sid: &str) -> PathBuf {
    home.join("sessions").join(sid).join("agents")
}

/// `ts` of a transcript's last line, read from the file's tail. A tail with
/// no complete line (one message over 64 KiB) falls back to the mtime.
/// Costs an open + seek + read, so callers pay for it only when they need
/// TIME (`ls`, `show`) — not `load_agent_records` itself, which every
/// caller (including `status` and `doctor`) runs over every record.
fn last_message_ts(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let from_lines = buf
        .split(|b| *b == b'\n')
        .rev()
        .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
        .find_map(|v| v.get("ts").and_then(|t| t.as_u64()));
    from_lines.or_else(|| {
        let m = f.metadata().ok()?.modified().ok()?;
        Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
    })
}

/// An agent's TIME: its last transcript message, or its start if the
/// transcript has none yet. Pays for `last_message_ts`'s read, so callers
/// use it only for rows they are about to show (`ls`'s survivors, `show`,
/// a run's children) rather than every record `load_agent_records` loads.
pub fn agent_active_at(a: &AgentRecord, home: &Path) -> Option<u64> {
    last_message_ts(&a.transcript_path(home)).or(a.started_at)
}

/// Every agent record under `home`. Records whose session is not in
/// `connected` and that still claim to be running are shown as
/// `interrupted` (stale = true): a gone session cannot host a live child.
pub fn load_agent_records(home: &Path, connected: &HashSet<String>) -> Vec<AgentRecord> {
    let mut out = Vec::new();
    let Ok(sessions) = std::fs::read_dir(home.join("sessions")) else {
        return out;
    };
    for s in sessions.flatten() {
        let Ok(files) = std::fs::read_dir(s.path().join("agents")) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let Ok(mut rec) = serde_json::from_slice::<AgentRecord>(&bytes) else {
                continue;
            };
            if !agent_status_terminal(&rec.status) && !connected.contains(&rec.session_id) {
                rec.status = "interrupted".into();
                rec.stale = true;
            }
            out.push(rec);
        }
    }
    out.sort_by(|a, b| (a.started_at, &a.child_id).cmp(&(b.started_at, &b.child_id)));
    out
}

// ---------------------------------------------------------------------------
// Snapshot: everything the inspection commands need
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub pi_pid: Option<u32>,
    pub cwd: Option<String>,
    /// "connected" | "gone"
    pub state: String,
    pub since: Option<u64>,
    pub last_seen: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
}

pub struct Snapshot {
    pub now: u64,
    /// `status` of the live daemon, when one was reached.
    pub daemon: Option<StatusOk>,
    pub tasks: Vec<TaskRecord>,
    pub agents: Vec<AgentRecord>,
    pub sessions: BTreeMap<String, SessionView>,
    /// Sessions the daemon reports as connected (empty without a daemon).
    pub connected: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Live {
    /// Start the daemon if needed (commands that already did so).
    Spawn,
    /// Use the daemon if it runs; otherwise read the disk.
    IfRunning,
}

pub async fn snapshot(home: &Path, live: Live) -> Result<Snapshot, String> {
    let now = now_ms();
    let conn = match live {
        Live::Spawn => Some(client::connect(home, &HelloMode::Cli).await?),
        Live::IfRunning => client::connect_existing(home, &HelloMode::Cli).await.ok(),
    };
    let (daemon, tasks) = match conn {
        Some(mut c) => {
            let st: StatusOk = c.roundtrip(RequestKind::Status).await?;
            let tasks = client::list_tasks(&mut c, None).await?;
            (Some(st), tasks)
        }
        // No daemon: every task died with it (lifeline, §3.2), so a record
        // still saying "running" is what the next daemon start marks orphaned.
        None => {
            let mut tasks = registry::load_all_records(home);
            for t in tasks.iter_mut() {
                // The persisted output_size lags the output file (a running
                // task's is only written at exit); with no daemon the file
                // can no longer grow, so it is the truth (as in scan_tasks).
                if let Ok(m) = std::fs::metadata(&t.output_path) {
                    t.output_size = t.output_size.max(m.len());
                }
                if t.status == TaskStatus::Running {
                    t.status = TaskStatus::Orphaned;
                    t.end_reason = Some(crate::proto::end_reason::MANAGER_CRASH.to_string());
                }
            }
            (None, tasks)
        }
    };
    // A gone session's tasks may have left the daemon's memory while their
    // records are still retained on disk (§3.2): `show` must still find them.
    let mut tasks = tasks;
    if daemon.is_some() {
        let known: HashSet<String> = tasks.iter().map(|t| t.task_id.clone()).collect();
        tasks.extend(registry::load_all_records(home).into_iter().filter(|t| !known.contains(&t.task_id)));
    }
    let connected: HashSet<String> = daemon
        .as_ref()
        .map(|d| {
            d.sessions
                .iter()
                .filter(|s| s.connected)
                .map(|s| s.session_id.clone())
                .collect()
        })
        .unwrap_or_default();
    let agents = load_agent_records(home, &connected);
    let sessions = session_views(home, daemon.as_ref());
    tasks.sort_by_key(|t| t.started_at);
    Ok(Snapshot {
        now,
        daemon,
        tasks,
        agents,
        sessions,
        connected,
    })
}

/// A CLI newer than the running daemon reads what the old daemon reports:
/// columns it never recorded (a session's CWD, SINCE) stay empty.
pub(crate) fn warn_if_older_daemon(snap: &Snapshot) {
    if let Some(d) = &snap.daemon {
        if d.protocol < crate::proto::PROTOCOL {
            eprintln!(
                "note: the running pi-famulus is older (protocol {}, this CLI {}); some columns stay empty until it restarts. \
                 Run `pi-famulus shutdown` once no pi session needs it.",
                d.protocol,
                crate::proto::PROTOCOL
            );
        }
    }
}

/// Live sessions from the daemon plus every session directory on disk; gone
/// sessions get pid/cwd/times from their events.jsonl.
fn session_views(home: &Path, daemon: Option<&StatusOk>) -> BTreeMap<String, SessionView> {
    let mut out = BTreeMap::new();
    if let Ok(dirs) = std::fs::read_dir(home.join("sessions")) {
        for d in dirs.flatten() {
            if !d.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let sid = d.file_name().to_string_lossy().into_owned();
            let mut v = SessionView {
                session_id: sid.clone(),
                state: "gone".into(),
                ..Default::default()
            };
            let evs = std::fs::read(d.path().join("events.jsonl"))
                .map(|b| events::parse_bytes(&b, Some(&sid)))
                .unwrap_or_default();
            for e in &evs {
                if v.since.is_none() {
                    v.since = Some(e.ts);
                }
                v.last_seen = Some(v.last_seen.map_or(e.ts, |l| l.max(e.ts)));
                if e.ty == "session.connect" {
                    v.pi_pid = e.raw.get("pi_pid").and_then(|x| x.as_u64()).map(|x| x as u32);
                    if let Some(c) = e.raw.get("cwd").and_then(|x| x.as_str()) {
                        v.cwd = Some(c.to_string());
                    }
                    v.protocol = e.raw.get("protocol").and_then(|x| x.as_u64()).map(|x| x as u32);
                    v.extension_version = e
                        .raw
                        .get("extension_version")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                }
            }
            out.insert(sid, v);
        }
    }
    if let Some(d) = daemon {
        for s in &d.sessions {
            let v = out.entry(s.session_id.clone()).or_insert_with(|| SessionView {
                session_id: s.session_id.clone(),
                ..Default::default()
            });
            v.pi_pid = Some(s.pi_pid);
            if s.cwd.is_some() {
                v.cwd = s.cwd.clone();
            }
            v.state = if s.connected { "connected" } else { "gone" }.into();
            if s.connected_at > 0 {
                v.since = Some(v.since.map_or(s.connected_at, |x| x.min(s.connected_at)));
            }
            if s.last_seen > 0 {
                v.last_seen = Some(s.last_seen);
            }
            v.protocol = s.protocol.or(v.protocol);
            if s.extension_version.is_some() {
                v.extension_version = s.extension_version.clone();
            }
        }
    }
    out
}

/// Shortest unique prefix (at least 8 chars, or the whole id) per session.
pub fn session_prefixes<'a>(ids: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let ids: Vec<&str> = {
        let mut v: Vec<&str> = ids.into_iter().collect();
        v.sort();
        v.dedup();
        v
    };
    let mut out = HashMap::new();
    for (i, id) in ids.iter().enumerate() {
        let chars: Vec<char> = id.chars().collect();
        let mut len = 8.min(chars.len());
        loop {
            let p: String = chars[..len].iter().collect();
            let clash = ids
                .iter()
                .enumerate()
                .any(|(j, other)| j != i && other.starts_with(p.as_str()));
            if !clash || len >= chars.len() {
                out.insert(id.to_string(), p);
                break;
            }
            len += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Rows (ls / show / --json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: String,
    pub kind: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: String,
    pub started_at: Option<u64>,
    /// What `ls` sorts and shows: a task's start, an agent's last message.
    pub active_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    pub title: String,
    /// A task's work kind (`crate::workkind`), or `monitor`; agents: none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_kind: Option<String>,
    /// A finished task's CPU, as its record has it (absent: not measured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_user_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_sys_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rss_kb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_cpu_user_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_cpu_sys_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_cpu_sampled_at: Option<u64>,
    pub live_cpu_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backgrounded_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub running: bool,
}

/// What a task's command does (`crate::workkind`). A monitor's wall time
/// is watching, not work, so monitors are their own kind.
pub fn work_kind(t: &TaskRecord) -> &'static str {
    match t.kind {
        TaskKind::Monitor => "monitor",
        TaskKind::Shell => crate::workkind::classify(&t.command).label(),
    }
}

fn status_str<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

pub fn task_row(t: &TaskRecord, now: u64) -> Row {
    let end = t.ended_at.unwrap_or(now);
    Row {
        id: t.task_id.clone(),
        kind: status_str(&t.kind),
        session_id: t.session_id.clone(),
        cwd: Some(t.cwd.clone()),
        status: status_str(&t.status),
        started_at: Some(t.started_at),
        active_at: Some(t.started_at),
        ended_at: t.ended_at,
        duration_ms: Some(end.saturating_sub(t.started_at)),
        exit_code: t.exit_code,
        signal: t.signal.clone(),
        end_reason: t.end_reason.clone(),
        title: fmt::first_line(&t.command).to_string(),
        work_kind: Some(work_kind(t).to_string()),
        cpu_user_ms: t.cpu_user_ms,
        cpu_sys_ms: t.cpu_sys_ms,
        max_rss_kb: t.max_rss_kb,
        live_cpu_user_ms: t.live_cpu_user_ms,
        live_cpu_sys_ms: t.live_cpu_sys_ms,
        live_cpu_percent: t.live_cpu_percent,
        live_cpu_sampled_at: t.live_cpu_sampled_at,
        live_cpu_stale: t.live_cpu_stale,
        pid: Some(t.pid),
        origin: t.origin.clone(),
        backgrounded_at: t.backgrounded_at,
        run_id: t.origin.as_ref().and_then(|o| o.run_id.clone()),
        error: None,
        running: t.status == TaskStatus::Running,
    }
}

/// `active_at` is the caller's choice: `a.started_at` is cheap and right for
/// rows that will not survive filtering, `agent_active_at` (a transcript
/// read) is right for rows about to be shown.
pub fn agent_row(a: &AgentRecord, sessions: &BTreeMap<String, SessionView>, now: u64, active_at: Option<u64>) -> Row {
    let end = a.ended_at.unwrap_or(now);
    Row {
        id: a.child_id.clone(),
        kind: "agent".into(),
        session_id: a.session_id.clone(),
        cwd: sessions.get(&a.session_id).and_then(|s| s.cwd.clone()),
        status: a.status.clone(),
        started_at: a.started_at,
        active_at,
        ended_at: a.ended_at,
        duration_ms: a.started_at.map(|s| end.saturating_sub(s)),
        exit_code: None,
        signal: None,
        end_reason: a.end_reason.clone().or_else(|| a.stale.then(|| "session-gone".to_string())),
        title: a.title(),
        work_kind: None,
        cpu_user_ms: None,
        cpu_sys_ms: None,
        max_rss_kb: None,
        live_cpu_user_ms: None,
        live_cpu_sys_ms: None,
        live_cpu_percent: None,
        live_cpu_sampled_at: None,
        live_cpu_stale: false,
        pid: None,
        origin: None,
        backgrounded_at: None,
        run_id: a.run_id.clone(),
        error: a.error.clone(),
        running: !agent_status_terminal(&a.status),
    }
}

/// Every task and agent as a `Row`, unsorted and with a placeholder
/// `active_at` (`started_at`) for agents: sorting and TIME both wait for
/// `filter_rows`, which enriches only the rows that survive filtering
/// (`sort_rows`) so a transcript is read once per row actually shown.
pub fn all_rows(snap: &Snapshot) -> Vec<Row> {
    let mut rows: Vec<Row> = snap.tasks.iter().map(|t| task_row(t, snap.now)).collect();
    rows.extend(snap.agents.iter().map(|a| agent_row(a, &snap.sessions, snap.now, a.started_at)));
    rows
}

/// Running first, then finished; each newest first.
pub fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        (!a.running, std::cmp::Reverse(a.active_at), &a.id).cmp(&(!b.running, std::cmp::Reverse(b.active_at), &b.id))
    });
}

/// User + system CPU, when measured.
pub fn cpu_ms(user: Option<u64>, sys: Option<u64>) -> Option<u64> {
    Some(user? + sys?)
}

/// CPU seconds: "0.4s", "12.3s", then "3m04s" style past a minute.
pub fn cpu_text(ms: Option<u64>) -> String {
    match ms {
        None => "-".into(),
        Some(ms) if ms < 60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        Some(ms) => fmt::human_duration(ms),
    }
}

/// Average cores busy over the wall time: CPU / wall.
pub fn cores_text(cpu_ms: Option<u64>, wall_ms: u64) -> String {
    match cpu_ms {
        Some(c) if wall_ms > 0 => format!("{:.1}", c as f64 / wall_ms as f64),
        _ => "-".into(),
    }
}

fn exit_col(r: &Row) -> String {
    r.exit_code
        .map(|c| c.to_string())
        .or_else(|| r.signal.clone())
        .unwrap_or_else(|| "-".into())
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

pub struct LsOpts {
    pub all: bool,
    pub session: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<String>,
    pub json: bool,
}

/// Terminal columns when stdout is a tty (COLUMNS env wins over TIOCGWINSZ).
fn term_width() -> Option<usize> {
    let tty = crate::sys::stdout_tty_columns()?;
    Some(std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(tty))
}

pub fn normalize_dir(d: &str) -> String {
    let expanded = if let Some(rest) = d.strip_prefix('~') {
        format!("{}{rest}", std::env::var("HOME").unwrap_or_default())
    } else {
        d.to_string()
    };
    let p = std::fs::canonicalize(&expanded).unwrap_or_else(|_| PathBuf::from(&expanded));
    p.to_string_lossy().trim_end_matches('/').to_string()
}

pub fn under_dir(cwd: &str, dir: &str) -> bool {
    let c = normalize_dir(cwd);
    c == dir || c.starts_with(&format!("{dir}/"))
}

/// `ls` rows: anything running anywhere, so a live process is never hidden;
/// with `all`, also the finished work of connected sessions. A gone
/// session's finished work stays inspectable by id (`show`) until its
/// retention expires, but is no longer listed.
///
/// `--since` is parsed first — a bad duration must error before anything
/// pays for a transcript read. The cheap filters (running/all, session,
/// cwd) run next; only rows that survive them have an agent's real TIME
/// filled in (a transcript read), so `--since` itself and the final order
/// never cost a read for a row that was going to be dropped anyway.
pub fn filter_rows(
    rows: Vec<Row>,
    o: &LsOpts,
    now: u64,
    connected: &HashSet<String>,
    agents: &[AgentRecord],
    home: &Path,
) -> Result<Vec<Row>, String> {
    let since = match &o.since {
        Some(s) => Some(now.saturating_sub(fmt::parse_duration(s)?)),
        None => None,
    };
    let dir = o.cwd.as_deref().map(normalize_dir);
    let mut rows: Vec<Row> = rows
        .into_iter()
        .filter(|r| r.running || (o.all && connected.contains(&r.session_id)))
        .filter(|r| o.session.as_ref().map_or(true, |p| r.session_id.starts_with(p.as_str())))
        .filter(|r| match (&dir, &r.cwd) {
            (None, _) => true,
            (Some(d), Some(c)) => under_dir(c, d),
            (Some(_), None) => false,
        })
        .collect();
    if rows.iter().any(|r| r.kind == "agent") {
        let by_id: HashMap<&str, &AgentRecord> = agents.iter().map(|a| (a.child_id.as_str(), a)).collect();
        for r in rows.iter_mut().filter(|r| r.kind == "agent") {
            if let Some(a) = by_id.get(r.id.as_str()) {
                r.active_at = agent_active_at(a, home);
            }
        }
    }
    rows.retain(|r| since.map_or(true, |s| r.active_at.unwrap_or(0) >= s));
    Ok(rows)
}

pub const LS_COLUMNS: [&str; 14] = [
    "ID", "KIND", "SESSION", "CWD", "STATUS", "TIME", "DUR", "CPU", "CORES", "NOW", "SAMPLE", "EXIT", "REASON", "TITLE",
];
/// Columns before TITLE.
const LS_FIXED: usize = LS_COLUMNS.len() - 1;

/// Render the ls table. TITLE is truncated to fit `width` (display columns,
/// CJK-aware); without a terminal it is capped at 60 columns.
pub fn render_ls(rows: &[Row], prefixes: &HashMap<String, String>, now: u64, width: Option<usize>) -> Vec<String> {
    let cells: Vec<[String; LS_FIXED]> = rows
        .iter()
        .map(|r| {
            let cpu = if r.running {
                cpu_ms(r.live_cpu_user_ms, r.live_cpu_sys_ms)
            } else {
                cpu_ms(r.cpu_user_ms, r.cpu_sys_ms)
            };
            [
                r.id.clone(),
                r.kind.clone(),
                prefixes.get(&r.session_id).cloned().unwrap_or_else(|| r.session_id.clone()),
                fmt::truncate_width_left(&fmt::tilde(r.cwd.as_deref().unwrap_or("-")), 24),
                r.status.clone(),
                r.active_at.map(|s| fmt::short_time(s, now)).unwrap_or_else(|| "-".into()),
                r.duration_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into()),
                cpu_text(cpu),
                cores_text(cpu, r.duration_ms.unwrap_or(0)),
                if r.live_cpu_stale {
                    "stale".into()
                } else {
                    r.live_cpu_percent.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "-".into())
                },
                if r.running {
                    r.live_cpu_sampled_at
                        .map(|t| fmt::short_time(t, now))
                        .unwrap_or_else(|| if r.live_cpu_stale { "never".into() } else { "-".into() })
                } else {
                    "-".into()
                },
                exit_col(r),
                r.end_reason.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    let mut widths: Vec<usize> = LS_COLUMNS[..LS_FIXED].iter().map(|h| fmt::display_width(h)).collect();
    for c in &cells {
        for (i, v) in c.iter().enumerate() {
            widths[i] = widths[i].max(fmt::display_width(v));
        }
    }
    let used: usize = widths.iter().map(|w| w + 1).sum();
    let title_w = match width {
        Some(w) => w.saturating_sub(used).max(20),
        None => 60,
    };
    let line = |vals: [&str; LS_FIXED], title: &str| {
        let mut s = String::new();
        for (i, v) in vals.iter().enumerate() {
            s.push_str(&fmt::pad(v, widths[i]));
            s.push(' ');
        }
        s.push_str(&fmt::truncate_width(title, title_w));
        s.trim_end().to_string()
    };
    let mut out = Vec::new();
    let h: [&str; LS_FIXED] = LS_COLUMNS[..LS_FIXED].try_into().unwrap();
    out.push(line(h, "TITLE"));
    for (r, c) in rows.iter().zip(cells.iter()) {
        let vals: [&str; LS_FIXED] = std::array::from_fn(|i| c[i].as_str());
        out.push(line(vals, &r.title));
    }
    out
}

pub async fn cmd_ls(home: &Path, o: LsOpts) -> Result<(), String> {
    let snap = snapshot(home, Live::Spawn).await?;
    warn_if_older_daemon(&snap);
    let mut rows = filter_rows(all_rows(&snap), &o, snap.now, &snap.connected, &snap.agents, home)?;
    sort_rows(&mut rows);
    if o.json {
        outln!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return Ok(());
    }
    if rows.is_empty() {
        match (o.all, o.session.is_some() || o.cwd.is_some() || o.since.is_some()) {
            (false, _) => outln!("nothing running (--all adds finished work)"),
            (true, true) => outln!("no tasks"),
            (true, false) => outln!("no tasks in connected sessions"),
        }
        return Ok(());
    }
    let prefixes = session_prefixes(snap.sessions.keys().map(|s| s.as_str()).chain(rows.iter().map(|r| r.session_id.as_str())));
    for l in render_ls(&rows, &prefixes, snap.now, term_width()) {
        outln!("{l}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Id resolution across tasks, agents and runs
// ---------------------------------------------------------------------------

pub enum Target {
    Task(TaskRecord),
    Agent(AgentRecord),
    Run(String, Vec<AgentRecord>),
}

pub fn resolve(snap: &Snapshot, typed: &str) -> Result<Target, String> {
    let mut known: Vec<String> = snap.tasks.iter().map(|t| t.task_id.clone()).collect();
    known.extend(snap.agents.iter().map(|a| a.child_id.clone()));
    let mut runs: Vec<String> = snap.agents.iter().filter_map(|a| a.run_id.clone()).collect();
    runs.sort();
    runs.dedup();
    known.extend(runs.iter().cloned());
    let id = client::resolve_task_id(typed, &known)?;
    if let Some(t) = snap.tasks.iter().find(|t| t.task_id == id) {
        return Ok(Target::Task(t.clone()));
    }
    if let Some(a) = snap.agents.iter().find(|a| a.child_id == id) {
        return Ok(Target::Agent(a.clone()));
    }
    let kids: Vec<AgentRecord> = snap
        .agents
        .iter()
        .filter(|a| a.run_id.as_deref() == Some(id.as_str()))
        .cloned()
        .collect();
    Ok(Target::Run(id, kids))
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

/// Events of every session mentioning `id` (as `id`, in `ids[]`, or as
/// `child_id`), time-ordered.
fn events_for(home: &Path, id: &str) -> Vec<EventLine> {
    let mut out: Vec<EventLine> = read_all_events(home, None).0.into_iter().filter(|e| event_mentions(e, id)).collect();
    out.sort_by_key(|e| e.ts);
    out
}

fn event_mentions(e: &EventLine, id: &str) -> bool {
    e.id.as_deref() == Some(id)
        || e.raw.get("child_id").and_then(|v| v.as_str()) == Some(id)
        || e.raw
            .get("ids")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(id)))
}

fn last_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}

fn kv(k: &str, v: impl AsRef<str>) {
    outln!("{:<13} {}", format!("{k}:"), v.as_ref());
}

fn wake_summary(evs: &[EventLine]) -> Option<String> {
    let emit = evs.iter().find(|e| e.ty == "wake.emit");
    let deliver = evs.iter().find(|e| e.ty == "wake.deliver");
    match (emit, deliver) {
        (None, None) => None,
        (Some(e), None) => Some(format!("emitted {} → not delivered", fmt::datetime(e.ts))),
        (e, Some(d)) => Some(format!(
            "{}delivered {} ({})",
            e.map(|e| format!("emitted {} → ", fmt::datetime(e.ts))).unwrap_or_default(),
            fmt::datetime(d.ts),
            d.raw.get("mode").and_then(|m| m.as_str()).unwrap_or("?")
        )),
    }
}

pub async fn cmd_show(home: &Path, typed: &str, json_out: bool) -> Result<(), String> {
    let snap = snapshot(home, Live::IfRunning).await?;
    let target = resolve(&snap, typed)?;
    let now = snap.now;
    match target {
        Target::Task(t) => {
            let evs = events_for(home, &t.task_id);
            let out_path = PathBuf::from(&t.output_path);
            let err_path = crate::task::stderr_path_for(&out_path);
            let tail = last_lines(&out_path, 10);
            if json_out {
                let v = json!({
                    "task": t,
                    "work_kind": work_kind(&t),
                    "stderr_path": err_path,
                    "session": snap.sessions.get(&t.session_id),
                    "output_tail": tail,
                    "events": evs.iter().map(|e| e.raw.clone()).collect::<Vec<_>>(),
                });
                outln!("{}", serde_json::to_string_pretty(&v).unwrap());
                return Ok(());
            }
            let r = task_row(&t, now);
            kv("id", &t.task_id);
            kv("kind", &r.kind);
            kv("work", work_kind(&t));
            let exit = if r.running { String::new() } else { format!(" ({})", exit_col(&r)) };
            kv("status", format!("{}{exit}", r.status));
            if let Some(reason) = &t.end_reason {
                kv("reason", reason);
            }
            let s = snap.sessions.get(&t.session_id);
            kv(
                "session",
                format!(
                    "{} ({}, pi pid {})",
                    t.session_id,
                    s.map(|s| s.state.as_str()).unwrap_or("unknown"),
                    s.and_then(|s| s.pi_pid).map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                ),
            );
            let mut lines = t.command.lines();
            kv("command", lines.next().unwrap_or(""));
            for l in lines {
                outln!("{:<13} {l}", "");
            }
            kv("cwd", fmt::tilde(&t.cwd));
            kv("pid", t.pid.to_string());
            kv("started", format!("{} ({})", fmt::datetime(t.started_at), fmt::ago(t.started_at, now)));
            if let Some(e) = t.ended_at {
                kv("ended", fmt::datetime(e));
            }
            kv("duration", fmt::human_duration(r.duration_ms.unwrap_or(0)));
            if r.running {
                let last_sample = t.live_cpu_sampled_at.map(fmt::datetime).unwrap_or_else(|| "never".into());
                let sample_state = if t.live_cpu_stale {
                    "unavailable"
                } else if t.live_cpu_sampled_at.is_some() {
                    "available"
                } else {
                    "not sampled yet"
                };
                if let Some(c) = cpu_ms(t.live_cpu_user_ms, t.live_cpu_sys_ms) {
                    let current = t.live_cpu_percent.map(|p| format!(", now {p:.0}%")).unwrap_or_default();
                    kv(
                        "cpu",
                        format!(
                            "{} (live user {}, sys {}; best-effort sample{current})",
                            cpu_text(Some(c)),
                            cpu_text(t.live_cpu_user_ms),
                            cpu_text(t.live_cpu_sys_ms),
                        ),
                    );
                }
                kv("cpu sample", format!("{sample_state} (last successful sample {last_sample})"));
            } else {
                match cpu_ms(t.cpu_user_ms, t.cpu_sys_ms) {
                    Some(c) => kv(
                        "cpu",
                        format!(
                            "{} (user {}, sys {}), {} cores avg, peak rss {}",
                            cpu_text(Some(c)),
                            cpu_text(t.cpu_user_ms),
                            cpu_text(t.cpu_sys_ms),
                            cores_text(Some(c), r.duration_ms.unwrap_or(0)),
                            t.max_rss_kb.map(|k| format!("{} MiB", k / 1024)).unwrap_or_else(|| "?".into())
                        ),
                    ),
                    None => kv("cpu", "not measured (no runner report: SIGKILLed with its group, or an older record)"),
                }
            }
            if let Some(b) = t.backgrounded_at {
                kv("backgrounded", format!("after {}", fmt::human_duration(b.saturating_sub(t.started_at))));
            }
            if let Some(o) = &t.origin {
                let by = match (&o.child_id, &o.run_id) {
                    (Some(c), Some(r)) => format!("{} ({c}, run {r})", o.via),
                    (Some(c), None) => format!("{} ({c})", o.via),
                    _ => o.via.clone(),
                };
                kv("spawned by", by);
            }
            kv("output", format!("{} ({} bytes)", fmt::tilde(&t.output_path), t.output_size));
            kv("stderr", fmt::tilde(&err_path.to_string_lossy()));
            if let Some(w) = wake_summary(&evs) {
                kv("wake", w);
            }
            outln!("--- last {} line(s) of output ---", tail.len());
            for l in tail {
                outln!("{l}");
            }
        }
        Target::Agent(a) => {
            let shells: Vec<&TaskRecord> = snap
                .tasks
                .iter()
                .filter(|t| t.origin.as_ref().and_then(|o| o.child_id.as_deref()) == Some(a.child_id.as_str()))
                .collect();
            let transcript = a.transcript_path(home);
            let evs = events_for(home, &a.child_id);
            if json_out {
                let v = json!({
                    "agent": a,
                    "transcript_path": transcript,
                    "shells": shells.iter().map(|t| &t.task_id).collect::<Vec<_>>(),
                    "session": snap.sessions.get(&a.session_id),
                    "events": evs.iter().map(|e| e.raw.clone()).collect::<Vec<_>>(),
                });
                outln!("{}", serde_json::to_string_pretty(&v).unwrap());
                return Ok(());
            }
            let r = agent_row(&a, &snap.sessions, now, agent_active_at(&a, home));
            kv("id", &a.child_id);
            kv("kind", "agent");
            kv("name", a.title());
            if let Some(run) = &a.run_id {
                kv("run", run);
            }
            let reason = r.end_reason.as_deref().map(|x| format!(" ({x})")).unwrap_or_default();
            kv("status", format!("{}{reason}", a.status));
            if a.stale {
                kv("note", "record says running but its session is gone; the child cannot be alive");
            }
            if let Some(e) = &a.error {
                kv("error", e);
            }
            kv("session", &a.session_id);
            if let Some(s) = a.started_at {
                kv("started", format!("{} ({})", fmt::datetime(s), fmt::ago(s, now)));
            }
            if let Some(e) = a.ended_at {
                kv("ended", fmt::datetime(e));
            }
            if let Some(d) = r.duration_ms {
                kv("duration", fmt::human_duration(d));
            }
            if let Some(n) = a.tool_calls {
                kv("tool calls", n.to_string());
            }
            if a.tokens_input.is_some() || a.tokens_output.is_some() || a.tokens_cache_read.is_some() || a.tokens_cache_write.is_some() {
                let cache = if a.tokens_cache_read.is_some() || a.tokens_cache_write.is_some() {
                    format!(" (cache read {} / write {})", a.tokens_cache_read.unwrap_or(0), a.tokens_cache_write.unwrap_or(0))
                } else {
                    String::new()
                };
                let rate = agent_output_tokens_per_second(a.tokens_output, a.llm_ms)
                    .map(|v| format!(" ({v:.1} output tok/s of LLM time)"))
                    .unwrap_or_default();
                kv("tokens", format!("{} input / {} output{cache}{rate}", a.tokens_input.unwrap_or(0), a.tokens_output.unwrap_or(0)));
            }
            if a.llm_ms.is_some() || a.tool_ms.is_some() || a.queue_ms.is_some() || a.wall_other_ms.is_some() {
                let approx = if a.wall_approximate { " (approximate; unclassified segments are not attributed)" } else { "" };
                kv(
                    "wall split",
                    format!(
                        "LLM {} / tool {} / queued {} / unclassified {}{approx}",
                        fmt::human_duration(a.llm_ms.unwrap_or(0)),
                        fmt::human_duration(a.tool_ms.unwrap_or(0)),
                        fmt::human_duration(a.queue_ms.unwrap_or(0)),
                        fmt::human_duration(a.wall_other_ms.unwrap_or(0)),
                    ),
                );
            }
            if shells.is_empty() {
                kv("shells", "none");
            } else {
                kv(
                    "shells",
                    shells
                        .iter()
                        .map(|t| format!("{} ({})", t.task_id, status_str(&t.status)))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            kv("transcript", fmt::tilde(&transcript.to_string_lossy()));
            if let Some(p) = &a.prompt_head {
                outln!("--- prompt ---");
                outln!("{p}");
            }
            let result = a.result_tail.as_deref().unwrap_or("");
            let lines: Vec<&str> = result.lines().collect();
            let tail = &lines[lines.len().saturating_sub(20)..];
            outln!("--- result (last {} line(s)) ---", tail.len());
            for l in tail {
                outln!("{l}");
            }
        }
        Target::Run(run, kids) => {
            if json_out {
                outln!("{}", serde_json::to_string_pretty(&json!({"run_id": run, "children": kids})).unwrap());
                return Ok(());
            }
            kv("run", &run);
            let rows: Vec<Row> = kids.iter().map(|a| agent_row(a, &snap.sessions, now, agent_active_at(a, home))).collect();
            let done = rows.iter().filter(|r| !r.running).count();
            kv("children", format!("{} ({} finished)", rows.len(), done));
            let prefixes = session_prefixes(rows.iter().map(|r| r.session_id.as_str()));
            for l in render_ls(&rows, &prefixes, now, term_width()) {
                outln!("{l}");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// agent transcript
// ---------------------------------------------------------------------------

/// Render one transcript line. `None` hides it (system/preamble lines unless
/// `full`). Transcript lines: {role, text, tool?, args?, isError?, ts?}.
pub fn render_transcript_line(line: &str, full: bool, prompt_head: Option<&str>, first_user: &mut bool) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("?");
    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let ts = v
        .get("ts")
        .and_then(|t| t.as_u64())
        .map(|t| format!("[{}] ", fmt::datetime(t).split(' ').nth(1).unwrap_or("").to_string()))
        .unwrap_or_default();
    let is_err = v
        .get("isError")
        .or_else(|| v.get("is_error"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    if !full && (role == "system" || v.get("preamble").and_then(|p| p.as_bool()) == Some(true)) {
        return None;
    }
    if let Some(tool) = v.get("tool").and_then(|t| t.as_str()) {
        let args = match v.get("args") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let mark = if is_err { " ✗" } else { "" };
        let body = if text.is_empty() { String::new() } else { format!(" → {text}") };
        return Some(format!("{ts}tool {tool}({args}){mark}{body}"));
    }
    let mut text = text.to_string();
    if role == "user" && *first_user {
        *first_user = false;
        if !full {
            if let Some(p) = prompt_head {
                if text != p && text.contains(p) {
                    text = format!("{p}\n  (agent preamble hidden; --full to show)");
                }
            }
        }
    }
    let err = if is_err { " (error)" } else { "" };
    Some(format!("{ts}{role}{err}: {text}"))
}

pub async fn cmd_agent(home: &Path, typed: &str, full: bool, follow: bool) -> Result<(), String> {
    let snap = snapshot(home, Live::IfRunning).await?;
    let a = match resolve(&snap, typed)? {
        Target::Agent(a) => a,
        Target::Task(t) => {
            return Err(format!("{} is a {} task, not an agent (use `show` or `log`)", t.task_id, status_str(&t.kind)))
        }
        Target::Run(r, _) => return Err(format!("{r} is a run; pick one of its children (`show {r}`)")),
    };
    print_transcript(home, &a, full, follow, usize::MAX).await
}

/// Render an agent transcript (last `lines` rendered lines), optionally
/// following it as it grows.
pub async fn print_transcript(home: &Path, a: &AgentRecord, full: bool, follow: bool, lines: usize) -> Result<(), String> {
    let path = a.transcript_path(home);
    let mut first_user = true;
    let mut offset = 0u64;
    let mut pending = String::new();
    let mut rendered: Vec<String> = Vec::new();
    if let Ok(bytes) = std::fs::read(&path) {
        offset = bytes.len() as u64;
        let text = String::from_utf8_lossy(&bytes);
        let (complete, rest) = match text.rfind('\n') {
            Some(i) => (&text[..=i], &text[i + 1..]),
            None => ("", text.as_ref()),
        };
        pending.push_str(rest);
        for l in complete.lines() {
            if let Some(r) = render_transcript_line(l, full, a.prompt_head.as_deref(), &mut first_user) {
                rendered.push(r);
            }
        }
    } else if !follow {
        return Err(format!("no transcript for {} at {}", a.child_id, path.display()));
    }
    for r in &rendered[rendered.len().saturating_sub(lines)..] {
        outln!("{r}");
    }
    if !follow {
        return Ok(());
    }
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.len() <= offset {
            continue;
        }
        let (bytes, next) = crate::task::read_file_range(&path, offset, (meta.len() - offset) as usize)
            .map_err(|e| e.to_string())?;
        offset = next;
        pending.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(i) = pending.find('\n') {
            let l: String = pending.drain(..=i).collect();
            if let Some(r) = render_transcript_line(&l, full, a.prompt_head.as_deref(), &mut first_user) {
                outln!("{r}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

pub struct EventsOpts {
    pub follow: bool,
    pub session: Option<String>,
    pub id: Option<String>,
    pub since: Option<String>,
    pub json: bool,
}

/// Read every events file; returns (events, malformed line count).
/// Every event line so far, time-ordered, plus the malformed-line count and,
/// per file, the offset just past the last complete line read. `-f`
/// continues from exactly those offsets, so a line written while history is
/// printed is neither lost nor shown twice.
fn read_all_events(
    home: &Path,
    session_prefix: Option<&str>,
) -> (Vec<EventLine>, usize, HashMap<PathBuf, u64>) {
    let mut out = Vec::new();
    let mut bad = 0;
    let mut offsets = HashMap::new();
    for (path, sid) in events::all_event_files(home) {
        if let Some(p) = session_prefix {
            if !sid.as_deref().is_some_and(|s| s.starts_with(p)) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let evs = events::parse_bytes(&bytes[..complete], sid.as_deref());
        let lines = bytes[..complete].iter().filter(|b| **b == b'\n').count();
        bad += lines.saturating_sub(evs.len());
        out.extend(evs);
        offsets.insert(path, complete as u64);
    }
    out.sort_by_key(|e| e.ts);
    (out, bad, offsets)
}

fn render_event(e: &EventLine, prefixes: &HashMap<String, String>) -> String {
    let mut fields = Vec::new();
    if let Some(o) = e.raw.as_object() {
        for (k, v) in o {
            if matches!(k.as_str(), "ts" | "src" | "type" | "id") || v.is_null() {
                continue;
            }
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            fields.push(format!("{k}={val}"));
        }
    }
    let sess = e
        .session
        .as_ref()
        .map(|s| prefixes.get(s).cloned().unwrap_or_else(|| s.clone()))
        .unwrap_or_else(|| "-".into());
    format!(
        "{} {} {:<9} {:<18} {} {}",
        fmt::datetime_ms(e.ts),
        sess,
        e.src,
        e.ty,
        e.id.as_deref().unwrap_or("-"),
        fields.join(" ")
    )
    .trim_end()
    .to_string()
}

fn event_json(e: &EventLine) -> String {
    let mut v = e.raw.clone();
    if let (Some(o), Some(s)) = (v.as_object_mut(), &e.session) {
        o.entry("session").or_insert(Value::from(s.clone()));
    }
    v.to_string()
}

pub async fn cmd_events(home: &Path, o: EventsOpts) -> Result<(), String> {
    let now = now_ms();
    let since = match &o.since {
        Some(s) => Some(now.saturating_sub(fmt::parse_duration(s)?)),
        None => None,
    };
    let keep = |e: &EventLine| -> bool {
        since.map_or(true, |s| e.ts >= s) && o.id.as_deref().map_or(true, |id| event_mentions(e, id))
    };
    let (evs, bad, mut offsets) = read_all_events(home, o.session.as_deref());
    if bad > 0 {
        eprintln!("note: skipped {bad} malformed event line(s)");
    }
    let mut sids: Vec<String> = events::all_event_files(home).into_iter().filter_map(|(_, s)| s).collect();
    let prefixes = session_prefixes(sids.iter().map(|s| s.as_str()));
    for e in evs.iter().filter(|e| keep(e)) {
        if o.json {
            outln!("{}", event_json(e));
        } else {
            outln!("{}", render_event(e, &prefixes));
        }
    }
    if !o.follow {
        return Ok(());
    }
    // Follow: poll every file for growth from where history stopped; a file
    // history did not see (a new session) is read from its start.
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut batch = Vec::new();
        for (p, sid) in events::all_event_files(home) {
            if let Some(prefix) = &o.session {
                if !sid.as_deref().is_some_and(|s| s.starts_with(prefix.as_str())) {
                    continue;
                }
            }
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            let off = offsets.entry(p.clone()).or_insert(0);
            if meta.len() < *off {
                *off = 0;
            }
            if meta.len() == *off {
                continue;
            }
            let Ok((bytes, _)) = crate::task::read_file_range(&p, *off, (meta.len() - *off) as usize) else {
                continue;
            };
            // Consume only complete lines; a partial last line waits.
            let complete = bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
            *off += complete as u64;
            batch.extend(events::parse_bytes(&bytes[..complete], sid.as_deref()));
        }
        batch.sort_by_key(|e| e.ts);
        let new_sids: Vec<String> = batch.iter().filter_map(|e| e.session.clone()).collect();
        if new_sids.iter().any(|s| !sids.contains(s)) {
            sids.extend(new_sids);
        }
        let prefixes = session_prefixes(sids.iter().map(|s| s.as_str()));
        for e in batch.iter().filter(|e| keep(e)) {
            if o.json {
                outln!("{}", event_json(e));
            } else {
                outln!("{}", render_event(e, &prefixes));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// sessions & status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SessionRow<'a> {
    #[serde(flatten)]
    view: &'a SessionView,
    running: usize,
    tasks: usize,
    agents: usize,
}

/// Connected sessions, plus a gone one only while it still runs something.
pub async fn cmd_sessions(home: &Path, json_out: bool) -> Result<(), String> {
    let snap = snapshot(home, Live::IfRunning).await?;
    warn_if_older_daemon(&snap);
    let rows: Vec<SessionRow> = snap
        .sessions
        .values()
        .map(|v| {
            let tasks: Vec<&TaskRecord> = snap.tasks.iter().filter(|t| t.session_id == v.session_id).collect();
            let agents: Vec<&AgentRecord> = snap.agents.iter().filter(|a| a.session_id == v.session_id).collect();
            SessionRow {
                view: v,
                running: tasks.iter().filter(|t| t.status == TaskStatus::Running).count()
                    + agents.iter().filter(|a| !agent_status_terminal(&a.status)).count(),
                tasks: tasks.len(),
                agents: agents.len(),
            }
        })
        .filter(|r| r.view.state == "connected" || r.running > 0)
        .collect();
    if json_out {
        outln!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return Ok(());
    }
    if rows.is_empty() {
        match &snap.daemon {
            None => outln!("no connected sessions (pi-famulus is not running)"),
            Some(_) => outln!("no connected sessions"),
        }
        return Ok(());
    }
    let prefixes = session_prefixes(snap.sessions.keys().map(|s| s.as_str()));
    let header = ["SESSION", "PI_PID", "STATE", "CWD", "SINCE", "LAST_SEEN", "RUNNING", "TASKS", "AGENTS"];
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                prefixes.get(&r.view.session_id).cloned().unwrap_or_default(),
                r.view.pi_pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                r.view.state.clone(),
                fmt::truncate_width_left(&fmt::tilde(r.view.cwd.as_deref().unwrap_or("-")), 32),
                r.view.since.map(|s| fmt::short_time(s, snap.now)).unwrap_or_else(|| "-".into()),
                r.view
                    .last_seen
                    .map(|s| if r.view.state == "connected" { "now".into() } else { fmt::ago(s, snap.now) })
                    .unwrap_or_else(|| "-".into()),
                r.running.to_string(),
                r.tasks.to_string(),
                r.agents.to_string(),
            ]
        })
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for c in &cells {
        for (i, v) in c.iter().enumerate() {
            widths[i] = widths[i].max(fmt::display_width(v));
        }
    }
    let render = |vals: Vec<&str>| {
        vals.iter()
            .enumerate()
            .map(|(i, v)| fmt::pad(v, widths[i]))
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end()
            .to_string()
    };
    outln!("{}", render(header.to_vec()));
    for c in &cells {
        outln!("{}", render(c.iter().map(|s| s.as_str()).collect()));
    }
    Ok(())
}

pub async fn cmd_status(home: &Path, json_out: bool) -> Result<(), String> {
    let mut conn = client::connect_existing(home, &HelloMode::Cli)
        .await
        .map_err(|_| "pi-famulus is not running".to_string())?;
    let st: StatusOk = conn.roundtrip(RequestKind::Status).await?;
    let connected: HashSet<String> = st.sessions.iter().filter(|s| s.connected).map(|s| s.session_id.clone()).collect();
    let agents = load_agent_records(home, &connected);
    let a_running = agents.iter().filter(|a| !agent_status_terminal(&a.status)).count();
    let a_done = agents.len() - a_running;
    let tokens_input: u64 = agents.iter().map(|a| a.tokens_input.unwrap_or(0)).sum();
    let tokens_output: u64 = agents.iter().map(|a| a.tokens_output.unwrap_or(0)).sum();
    let tokens_cache_read: u64 = agents.iter().map(|a| a.tokens_cache_read.unwrap_or(0)).sum();
    let tokens_cache_write: u64 = agents.iter().map(|a| a.tokens_cache_write.unwrap_or(0)).sum();
    let llm_ms: u64 = agents.iter().map(|a| a.llm_ms.unwrap_or(0)).sum();
    let tokens_per_second = agent_output_tokens_per_second(Some(tokens_output), Some(llm_ms));
    if json_out {
        let mut v = serde_json::to_value(&st).unwrap();
        v["agent_counts"] = json!({"running": a_running, "terminal": a_done});
        v["agent_tokens"] = json!({
            "input": tokens_input,
            "output": tokens_output,
            "tokens_cache_read": tokens_cache_read,
            "tokens_cache_write": tokens_cache_write,
            "llm_ms": llm_ms,
            "output_tokens_per_second": tokens_per_second,
        });
        outln!("{}", serde_json::to_string_pretty(&v).unwrap());
        return Ok(());
    }
    outln!("version:  {} (protocol {})", st.version, st.protocol);
    outln!("pid:      {}", st.pid);
    if let Some(exe) = &st.exe {
        outln!("binary:   {exe}");
    }
    outln!("uptime:   {}", fmt::human_duration(st.uptime_ms));
    if st.protocol >= 4 {
        if let Some(agent_capacity) = &st.agent_capacity {
            outln!("agent slots: {}/{} used", agent_capacity.used, agent_capacity.total);
            for kind in crate::capacity::WORK_KINDS {
                if let Some(capacity) = agent_capacity.by_kind.get(*kind) {
                    outln!("  {kind}: {}/{} used", capacity.used, capacity.total);
                }
            }
            let mut unknown_kinds: Vec<_> = agent_capacity
                .by_kind
                .keys()
                .filter(|kind| !crate::capacity::WORK_KINDS.contains(&kind.as_str()))
                .collect();
            unknown_kinds.sort();
            for kind in unknown_kinds {
                if let Some(capacity) = agent_capacity.by_kind.get(kind) {
                    let escaped: String = kind.chars().flat_map(char::escape_default).collect();
                    outln!("  {escaped}: {}/{} used", capacity.used, capacity.total);
                }
            }
        }
    }
    outln!(
        "sessions: {} ({} connected)",
        st.sessions.len(),
        st.sessions.iter().filter(|s| s.connected).count()
    );
    outln!(
        "tasks:    {} running, {} finished (shells {}/{}, agents {}/{})",
        st.task_counts.running + a_running,
        st.task_counts.terminal + a_done,
        st.task_counts.running,
        st.task_counts.terminal,
        a_running,
        a_done
    );
    let rate = tokens_per_second.map(|v| format!(" ({v:.1} output tok/s over {llm_ms} ms of LLM time)")).unwrap_or_default();
    outln!("agent tokens: {tokens_input} input / {tokens_output} output (cache read {tokens_cache_read} / write {tokens_cache_write}){rate}");
    if let Some(line) = upgrade_line(&st, now_ms()) {
        outln!("{line}");
    }
    Ok(())
}

/// The `status` line about in-place upgrades: how many this pid went
/// through and the latest, or why the latest attempt did not happen.
pub fn upgrade_line(st: &StatusOk, now: u64) -> Option<String> {
    match &st.last_upgrade {
        Some(u) if !u.ok => Some(format!(
            "upgrades: {} (last attempt failed {}, {}: {})",
            st.generation,
            fmt::ago(u.at, now),
            u.trigger,
            u.error.as_deref().unwrap_or("unknown error")
        )),
        Some(u) => Some(format!(
            "upgrades: {} (last: {} -> {}, {}, {})",
            st.generation,
            u.from_version,
            u.to_version.as_deref().unwrap_or("?"),
            u.trigger,
            fmt::ago(u.at, now)
        )),
        None if st.generation > 0 => Some(format!("upgrades: {}", st.generation)),
        None => None,
    }
}

/// For `wait` on an agent: poll its record until terminal or the budget ends.
pub async fn wait_agent(home: &Path, child_id: &str, budget_ms: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(budget_ms);
    loop {
        let snap = snapshot(home, Live::IfRunning).await?;
        let Some(a) = snap.agents.iter().find(|a| a.child_id == child_id) else {
            return Err(format!("agent {child_id} disappeared"));
        };
        if agent_status_terminal(&a.status) {
            outln!("done status={}", a.status);
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            outln!("not done (budget expired; agent still {})", a.status);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_unique_and_at_least_8() {
        let ids = ["0199aaaa-1111-7000", "0199aaaa-2222-7000", "abc", "zzzzzzzzzzzz"];
        let p = session_prefixes(ids.iter().copied());
        assert_eq!(p["abc"], "abc");
        assert_eq!(p["zzzzzzzzzzzz"], "zzzzzzzz");
        assert_eq!(p["0199aaaa-1111-7000"], "0199aaaa-1");
        assert_eq!(p["0199aaaa-2222-7000"], "0199aaaa-2");
    }

    #[test]
    fn transcript_rendering_hides_preamble() {
        let mut first = true;
        let head = Some("do the thing");
        let l = r#"{"role":"system","text":"sys"}"#;
        assert_eq!(render_transcript_line(l, false, head, &mut first), None);
        assert!(render_transcript_line(l, true, head, &mut first).is_some());
        let u = r#"{"role":"user","text":"You are a worker...\n\ndo the thing"}"#;
        let r = render_transcript_line(u, false, head, &mut first).unwrap();
        assert!(r.starts_with("user: do the thing"), "{r}");
        assert!(r.contains("preamble hidden"));
        let mut first = true;
        let r = render_transcript_line(u, true, head, &mut first).unwrap();
        assert!(r.contains("You are a worker"));
        let t = r#"{"role":"tool","tool":"bash","args":"ls","isError":true,"text":"boom"}"#;
        assert_eq!(render_transcript_line(t, false, head, &mut first).unwrap(), "tool bash(ls) ✗ → boom");
        assert_eq!(render_transcript_line("not json", false, head, &mut first), None);
    }
}
