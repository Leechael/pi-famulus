//! Background snapshot + incremental event tail for interactive `top`.
//!
//! The collector never runs on the UI thread. A full `inspect::snapshot` on a
//! real home re-reads every task JSON and every `events.jsonl` (thousands of
//! files, tens of thousands of lines) — that froze key handling when it sat
//! in the render loop. Live ticks talk to the daemon and reuse a disk cache;
//! historical records refresh on a slower cadence.

use crate::client::{self, HelloMode};
use crate::events::{self, EventLine};
use crate::inspect::{self, AgentRecord, SessionView, Snapshot};
use crate::proto::{now_ms, RequestKind, StatusOk, TaskRecord, TaskStatus};
use crate::registry;
use crate::task;
use crate::top::model::{compact_num, Chat, Detail, DetailBody, LogView, NodeKind, TaskInfo, TopState};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TAIL: u64 = 64 * 1024;
/// Newest events kept for the Events tab. A real home can have 50k+ lines;
/// rebuilding rows from all of them every tick is what made the TUI hitch.
pub const EVENT_CAP: usize = 800;
/// First read of a large events file: only the tail. Subsequent reads are
/// incremental from the stored offset.
const EVENT_FILE_TAIL: u64 = 32 * 1024;
const DISK_RESYNC: Duration = Duration::from_secs(5);

pub struct Tick {
    /// `None` on a tails-only refresh (preview/detail follow).
    pub snap: Option<Snapshot>,
    pub events: Vec<EventLine>,
    pub kind_slots: HashMap<String, (usize, usize)>,
    pub preview_body: DetailBody,
    pub detail_body: Option<DetailBody>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectView {
    pub session_prefix: Option<String>,
    pub preview: bool,
    pub selected: Option<String>,
    pub detail: Option<DetailView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailView {
    pub id: String,
    pub tab: usize,
    pub follow: bool,
}

impl CollectView {
    pub fn from_state(state: &TopState) -> CollectView {
        CollectView {
            session_prefix: state.session_prefix.clone(),
            preview: state.preview,
            selected: state.selected.clone(),
            detail: state.detail.as_ref().map(|d| DetailView {
                id: d.id.clone(),
                tab: d.tab,
                follow: d.follow,
            }),
        }
    }
}

pub struct Collector {
    home: PathBuf,
    disk_tasks: Vec<TaskRecord>,
    last_disk: Option<Instant>,
    agents: Vec<AgentRecord>,
    events: Vec<EventLine>,
    event_offsets: HashMap<PathBuf, u64>,
    event_prefix: Option<String>,
    last_tasks: Vec<TaskRecord>,
    last_now: u64,
    /// Extension version each session reported on hello, by session id.
    ext_versions: HashMap<String, String>,
}

impl Collector {
    pub fn new(home: PathBuf) -> Collector {
        Collector {
            home,
            disk_tasks: Vec::new(),
            last_disk: None,
            agents: Vec::new(),
            events: Vec::new(),
            event_offsets: HashMap::new(),
            event_prefix: None,
            last_tasks: Vec::new(),
            last_now: 0,
            ext_versions: HashMap::new(),
        }
    }

    pub fn disk_stale(&self) -> bool {
        self.last_disk.is_none_or(|t| t.elapsed() >= DISK_RESYNC)
    }

    pub async fn refresh_disk(&mut self) {
        let home = self.home.clone();
        if let Ok(tasks) = tokio::task::spawn_blocking(move || registry::load_all_records(&home)).await {
            self.disk_tasks = tasks;
        }
        self.last_disk = Some(Instant::now());
    }

    pub async fn tick(&mut self, view: &CollectView) -> Result<Tick, String> {
        let now = now_ms();
        let (daemon, live_tasks) = live_daemon(&self.home).await;
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

        self.refresh_agents(&connected).await;
        self.refresh_events(view.session_prefix.as_deref());

        let mut tasks = if daemon.is_some() {
            merge_tasks(live_tasks, &self.disk_tasks)
        } else if !self.disk_tasks.is_empty() {
            orphan_disk(&self.disk_tasks)
        } else {
            live_tasks
        };
        overlay_live_signals(&mut self.agents, &mut tasks, &self.events);
        let sessions = sessions_light(&self.home, daemon.as_ref(), &tasks, &self.agents);
        let kind_slots = kind_slots(&self.home, daemon.as_ref());
        self.last_tasks = tasks.clone();
        self.last_now = now;
        self.ext_versions = sessions
            .values()
            .filter_map(|s| s.extension_version.clone().map(|v| (s.session_id.clone(), v)))
            .collect();
        let snap = Snapshot {
            now,
            daemon,
            tasks,
            agents: self.agents.clone(),
            sessions,
            connected,
        };
        Ok(self.finish(Some(snap), kind_slots, view))
    }

    pub fn tails(&self, view: &CollectView) -> Tick {
        Tick {
            snap: None,
            events: Vec::new(),
            kind_slots: HashMap::new(),
            preview_body: self.preview_body(view),
            detail_body: self.detail_body(view),
        }
    }

    fn finish(&self, snap: Option<Snapshot>, kind_slots: HashMap<String, (usize, usize)>, view: &CollectView) -> Tick {
        Tick {
            snap,
            events: self.events.clone(),
            kind_slots,
            preview_body: self.preview_body(view),
            detail_body: self.detail_body(view),
        }
    }

    fn preview_body(&self, view: &CollectView) -> DetailBody {
        if view.preview {
            tail_for(
                &self.home,
                &self.ext_versions,
                &self.last_tasks,
                &self.agents,
                self.last_now,
                view.selected.as_deref(),
                0,
            )
        } else {
            DetailBody::Empty
        }
    }

    fn detail_body(&self, view: &CollectView) -> Option<DetailBody> {
        view.detail
            .as_ref()
            .map(|d| {
                tail_for(&self.home, &self.ext_versions, &self.last_tasks, &self.agents, self.last_now, Some(&d.id), d.tab)
            })
    }

    async fn refresh_agents(&mut self, connected: &HashSet<String>) {
        let home = self.home.clone();
        let connected = connected.clone();
        if let Ok(agents) = tokio::task::spawn_blocking(move || inspect::load_agent_records(&home, &connected)).await {
            self.agents = agents;
        }
    }

    fn refresh_events(&mut self, prefix: Option<&str>) {
        if self.event_prefix.as_deref() != prefix {
            self.events.clear();
            self.event_offsets.clear();
            self.event_prefix = prefix.map(str::to_string);
        }
        for (path, sid) in events::all_event_files(&self.home) {
            if let Some(p) = prefix {
                if !sid.as_deref().is_some_and(|s| s.starts_with(p)) {
                    continue;
                }
            }
            let off = self.event_offsets.get(&path).copied().unwrap_or(0);
            let (new, next) = read_new_events(&path, sid.as_deref(), off);
            self.event_offsets.insert(path, next);
            self.events.extend(new);
        }
        self.events.sort_by_key(|e| e.ts);
        cap_events(&mut self.events, EVENT_CAP);
    }
}

async fn live_daemon(home: &Path) -> (Option<StatusOk>, Vec<TaskRecord>) {
    let Ok(mut c) = client::connect_existing(home, &HelloMode::Cli).await else {
        return (None, Vec::new());
    };
    let st = c.roundtrip(RequestKind::Status).await.ok();
    let tasks = match &st {
        Some(_) => client::list_tasks(&mut c, None).await.unwrap_or_default(),
        None => Vec::new(),
    };
    (st, tasks)
}

/// Live daemon records win; disk fills in tasks the daemon has already forgotten.
pub fn merge_tasks(live: Vec<TaskRecord>, disk: &[TaskRecord]) -> Vec<TaskRecord> {
    let known: HashSet<String> = live.iter().map(|t| t.task_id.clone()).collect();
    let mut out = live;
    out.extend(disk.iter().filter(|t| !known.contains(&t.task_id)).cloned());
    out.sort_by_key(|t| t.started_at);
    out
}

fn orphan_disk(disk: &[TaskRecord]) -> Vec<TaskRecord> {
    disk.iter()
        .cloned()
        .map(|mut t| {
            if let Ok(m) = std::fs::metadata(&t.output_path) {
                t.output_size = t.output_size.max(m.len());
            }
            if t.status == TaskStatus::Running {
                t.status = TaskStatus::Orphaned;
                t.end_reason = Some(crate::proto::end_reason::MANAGER_CRASH.to_string());
            }
            t
        })
        .collect()
}

/// Session list without parsing every `events.jsonl`. Connected rows come from
/// the daemon; cwd / times are filled from tasks and agents already in memory.
fn sessions_light(
    home: &Path,
    daemon: Option<&StatusOk>,
    tasks: &[TaskRecord],
    agents: &[AgentRecord],
) -> BTreeMap<String, SessionView> {
    let mut out = BTreeMap::new();
    if let Ok(dirs) = std::fs::read_dir(home.join("sessions")) {
        for d in dirs.flatten() {
            if !d.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let sid = d.file_name().to_string_lossy().into_owned();
            out.insert(
                sid.clone(),
                SessionView {
                    session_id: sid,
                    state: "gone".into(),
                    ..Default::default()
                },
            );
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
    for t in tasks {
        let v = out.entry(t.session_id.clone()).or_insert_with(|| SessionView {
            session_id: t.session_id.clone(),
            state: "gone".into(),
            ..Default::default()
        });
        if v.cwd.is_none() {
            v.cwd = Some(t.cwd.clone());
        }
        v.since = Some(v.since.map_or(t.started_at, |s| s.min(t.started_at)));
        let last = t.ended_at.unwrap_or(t.started_at);
        v.last_seen = Some(v.last_seen.map_or(last, |l| l.max(last)));
    }
    for a in agents {
        let v = out.entry(a.session_id.clone()).or_insert_with(|| SessionView {
            session_id: a.session_id.clone(),
            state: "gone".into(),
            ..Default::default()
        });
        if let Some(s) = a.started_at {
            v.since = Some(v.since.map_or(s, |x| x.min(s)));
        }
        if let Some(e) = a.ended_at.or(a.started_at) {
            v.last_seen = Some(v.last_seen.map_or(e, |l| l.max(e)));
        }
    }
    out
}

/// Live agent.usage and monitor labels from events overlay disk/daemon rows.
fn overlay_live_signals(agents: &mut [AgentRecord], tasks: &mut [TaskRecord], events: &[EventLine]) {
    for e in events {
        if e.ty == "agent.usage" {
            let id = e
                .id
                .as_deref()
                .or_else(|| e.raw.get("child_id").and_then(|v| v.as_str()));
            let Some(id) = id else { continue };
            let Some(a) = agents.iter_mut().find(|a| a.child_id == id) else { continue };
            if let Some(n) = e.raw.get("tokens_input").and_then(|v| v.as_u64()) {
                a.tokens_input = Some(n);
            }
            if let Some(n) = e.raw.get("tokens_output").and_then(|v| v.as_u64()) {
                a.tokens_output = Some(n);
            }
            if let Some(n) = e.raw.get("tokens_cache_read").and_then(|v| v.as_u64()) {
                a.tokens_cache_read = Some(n);
            }
            if let Some(n) = e.raw.get("tokens_cache_write").and_then(|v| v.as_u64()) {
                a.tokens_cache_write = Some(n);
            }
            if let Some(n) = e.raw.get("llm_ms").and_then(|v| v.as_u64()) {
                a.llm_ms = Some(n);
            }
            continue;
        }
        let desc = e
            .raw
            .get("description")
            .or_else(|| e.raw.get("label"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(desc) = desc else { continue };
        if e.ty != "monitor.start" && e.ty != "task.start" {
            continue;
        }
        let Some(id) = e.id.as_deref() else { continue };
        if let Some(t) = tasks.iter_mut().find(|t| t.task_id == id) {
            if t.label.as_deref().unwrap_or("").is_empty() {
                t.label = Some(desc.to_string());
            }
        }
    }
}

pub fn cap_events(events: &mut Vec<EventLine>, cap: usize) {
    if events.len() > cap {
        let drain = events.len() - cap;
        events.drain(0..drain);
    }
}

fn read_new_events(path: &Path, sid: Option<&str>, offset: u64) -> (Vec<EventLine>, u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (Vec::new(), offset);
    };
    let len = meta.len();
    if len <= offset {
        return (Vec::new(), offset);
    }
    let start = if offset == 0 && len > EVENT_FILE_TAIL {
        len - EVENT_FILE_TAIL
    } else {
        offset
    };
    let to_read = (len - start) as usize;
    let Ok((bytes, _)) = task::read_file_range(path, start, to_read) else {
        return (Vec::new(), offset);
    };
    let (data, skip) = if offset == 0 && start > 0 {
        match bytes.iter().position(|b| *b == b'\n') {
            Some(i) => (&bytes[i + 1..], i + 1),
            None => (bytes.as_slice(), 0usize),
        }
    } else {
        (bytes.as_slice(), 0usize)
    };
    let complete = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let evs = events::parse_bytes(&data[..complete], sid);
    (evs, start + skip as u64 + complete as u64)
}

fn kind_slots(home: &Path, daemon: Option<&StatusOk>) -> HashMap<String, (usize, usize)> {
    if let Some(cap) = daemon.and_then(|d| d.agent_capacity.as_ref()) {
        return cap
            .by_kind
            .iter()
            .map(|(k, v)| (k.clone(), (v.used, v.total)))
            .collect();
    }
    crate::capacity::WORK_KINDS
        .iter()
        .filter_map(|k| crate::capacity::max_kind(home, k).ok().map(|total| ((*k).to_string(), (0, total))))
        .collect()
}

fn tail_for(
    home: &Path,
    ext_versions: &HashMap<String, String>,
    tasks: &[TaskRecord],
    agents: &[AgentRecord],
    now: u64,
    id: Option<&str>,
    tab: usize,
) -> DetailBody {
    let Some(id) = id else {
        return DetailBody::Empty;
    };
    if let Some(t) = tasks.iter().find(|t| t.task_id == id) {
        return match tab {
            1 => task_log(t, now, &task::stderr_path_for(Path::new(&t.output_path)), true),
            _ => task_log(t, now, Path::new(&t.output_path), false),
        };
    }
    if let Some(a) = agents.iter().find(|a| a.child_id == id) {
        let _ = tab;
        return agent_chat(home, a, tasks, ext_versions.get(&a.session_id).map(String::as_str));
    }
    DetailBody::Empty
}

fn task_log(t: &TaskRecord, now: u64, path: &Path, stderr: bool) -> DetailBody {
    let lines = tail_file(path);
    let empty = lines.len() == 1 && (lines[0] == "(empty)" || lines[0].starts_with("(no file"));
    let running = t.status == TaskStatus::Running;
    let lines = if empty {
        vec![match (stderr, running) {
            (true, true) => "(nothing on stderr yet)".into(),
            (true, false) => "(nothing on stderr)".into(),
            (false, true) => "(no output yet)".into(),
            (false, false) => "(no output; the command printed nothing)".into(),
        }]
    } else {
        lines
    };
    DetailBody::Log { info: Box::new(task_info(t, now)), lines }
}

#[cfg(test)]
pub fn task_info_for_test(t: &TaskRecord, now: u64) -> TaskInfo {
    task_info(t, now)
}

fn task_info(t: &TaskRecord, now: u64) -> TaskInfo {
    let running = t.status == TaskStatus::Running;
    let cpu = if running {
        inspect::cpu_ms(t.live_cpu_user_ms, t.live_cpu_sys_ms)
    } else {
        inspect::cpu_ms(t.cpu_user_ms, t.cpu_sys_ms)
    };
    TaskInfo {
        id: t.task_id.clone(),
        kind: match t.kind {
            crate::proto::TaskKind::Monitor => "monitor".into(),
            crate::proto::TaskKind::Shell => "shell".into(),
        },
        status: serde_json::to_value(t.status)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{:?}", t.status)),
        running,
        command: t.command.clone(),
        cwd: t.cwd.clone(),
        pid: t.pid,
        session: t.session_id.clone(),
        started_at: t.started_at,
        wall_ms: t.ended_at.unwrap_or(now).saturating_sub(t.started_at),
        cpu_ms: cpu,
        cpu_now: if running { t.live_cpu_percent } else { None },
        cpu_stale: running && t.live_cpu_stale,
        work_kind: inspect::work_kind(t).to_string(),
        exit: t.signal.clone().or_else(|| t.exit_code.map(|c| c.to_string())),
        reason: t.end_reason.clone(),
        output: t.output_path.clone(),
    }
}

fn tail_file(path: &Path) -> Vec<String> {
    let Ok(meta) = std::fs::metadata(path) else {
        return vec![format!("(no file {})", path.display())];
    };
    let off = meta.len().saturating_sub(TAIL);
    match task::read_file_range(path, off, TAIL as usize) {
        Ok((bytes, _)) => {
            let text = String::from_utf8_lossy(&bytes);
            // `lines` strips one `\r` of a CRLF; pty output can carry a second.
            let lines: Vec<String> = text.lines().map(|s| s.trim_end_matches('\r').to_string()).collect();
            if lines.is_empty() {
                vec!["(empty)".into()]
            } else {
                lines
            }
        }
        Err(e) => vec![format!("(read error: {e})")],
    }
}

/// Read the whole transcript (not a 64 KiB tail). Files over 2 MiB keep the
/// newest complete lines so a long run still opens as a conversation.
const TRANSCRIPT_CAP: u64 = 2 * 1024 * 1024;

fn load_transcript(home: &Path, a: &AgentRecord) -> Vec<inspect::TranscriptTurn> {
    let path = a.transcript_path(home);
    let Ok(meta) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    let len = meta.len();
    let off = len.saturating_sub(TRANSCRIPT_CAP);
    let Ok((bytes, _)) = task::read_file_range(&path, off, (len - off) as usize) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let body = if off > 0 {
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or(&text)
    } else {
        &text
    };
    // Mid-file windows must not treat the first user line as the task prompt.
    let mut first_user = off == 0;
    let mut out = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(turn) = inspect::parse_transcript_line(line, false, a.prompt_head.as_deref(), &mut first_user) {
            out.push(turn);
        }
    }
    out
}

/// Why an agent has no usage at all: extensions before 0.1.4 never wrote
/// tokens or wall time into the record, so `-` everywhere is not a bug in
/// the numbers.
fn usage_note(a: &AgentRecord, ext_version: Option<&str>) -> Option<String> {
    let recorded = a.tokens_input.is_some() || a.tokens_output.is_some() || a.llm_ms.is_some();
    if recorded {
        return None;
    }
    Some(match ext_version {
        Some(v) => format!("usage not recorded: this session runs extension {v}; 0.1.4+ records tokens and wall time"),
        None => "usage not recorded by this session's extension (0.1.4+ records tokens and wall time)".into(),
    })
}

fn agent_chat(home: &Path, a: &AgentRecord, tasks: &[TaskRecord], ext_version: Option<&str>) -> DetailBody {
    let shells = tasks
        .iter()
        .filter(|t| t.origin.as_ref().and_then(|o| o.child_id.as_deref()) == Some(a.child_id.as_str()))
        .count();
    let tok_in = a.tokens_input.map(compact_num).unwrap_or_else(|| "-".into());
    let tok_out = a.tokens_output.map(compact_num).unwrap_or_else(|| "-".into());
    let tps = inspect::agent_output_tokens_per_second(a.tokens_output, a.llm_ms)
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "-".into());
    let shells_label = if shells == 1 { "shell" } else { "shells" };
    let agent = [&a.name, &a.agent].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
    DetailBody::Conversation(Chat {
        turns: load_transcript(home, a),
        agent,
        note: usage_note(a, ext_version),
        stats: vec![
            (tok_in, "in".into()),
            (tok_out, "out".into()),
            (tps, "tok/s".into()),
            (dur(a.llm_ms), "llm".into()),
            (dur(a.tool_ms), "tool".into()),
            (dur(a.queue_ms), "queue".into()),
            (shells.to_string(), shells_label.into()),
        ],
        running: !inspect::agent_status_terminal(&a.status) && !a.stale,
    })
}

fn dur(ms: Option<u64>) -> String {
    ms.map(crate::fmt::human_duration).unwrap_or_else(|| "-".into())
}

pub fn detail_tabs(kind: NodeKind) -> Vec<String> {
    match kind {
        NodeKind::Task => vec!["output".into(), "stderr".into()],
        NodeKind::Agent => Vec::new(),
        _ => vec!["Info".into()],
    }
}

pub fn open_detail(state: &TopState) -> Option<Detail> {
    let n = state.selected_node()?;
    if matches!(n.kind, NodeKind::Session | NodeKind::Main | NodeKind::Run) {
        return None;
    }
    let title = if n.kind == NodeKind::Agent {
        let name = n.title.split(" (").next().unwrap_or(n.label.as_str());
        format!("subagent · {name}")
    } else {
        format!("{} · {}", n.id, n.label)
    };
    Some(Detail {
        id: n.id.clone(),
        tab: 0,
        follow: true,
        scroll: 0,
        title,
        tabs: detail_tabs(n.kind),
        body: DetailBody::Empty,
        wrap_len: 0,
        view_h: 0,
        tools_expanded: false,
        open: HashSet::new(),
        hits: Vec::new(),
        log: LogView::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::parse_line;
    use crate::proto::{Origin, TaskKind};

    fn rec(id: &str, live: bool) -> TaskRecord {
        TaskRecord {
            task_id: id.into(),
            session_id: "s".into(),
            kind: TaskKind::Shell,
            command: "echo".into(),
            cwd: "/tmp".into(),
            pid: 1,
            status: if live { TaskStatus::Running } else { TaskStatus::Completed },
            exit_code: None,
            signal: None,
            started_at: if live { 2 } else { 1 },
            ended_at: (!live).then_some(2),
            output_path: String::new(),
            output_size: 0,
            origin: Some(Origin {
                via: "child-bash".into(),
                child_id: None,
                run_id: None,
            }),
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            max_rss_kb: None,
            live_cpu_user_ms: None,
            live_cpu_sys_ms: None,
            live_cpu_percent: live.then_some(10.0),
            live_cpu_sampled_at: None,
            live_cpu_stale: false,
            label: None,
        }
    }

    #[test]
    fn agent_without_usage_says_which_extension_left_it_out() {
        // Shape written by extension 0.1.3: identity and status, no usage.
        let old: AgentRecord = serde_json::from_str(
            r#"{"child_id":"ch_old","session_id":"s","name":"worker","agent":"worker","status":"running"}"#,
        )
        .unwrap();
        let home = std::env::temp_dir();
        let DetailBody::Conversation(chat) = agent_chat(&home, &old, &[], Some("0.1.3")) else { panic!("chat") };
        let note = chat.note.expect("note");
        assert!(note.contains("0.1.3") && note.contains("0.1.4+"), "{note}");
        let DetailBody::Conversation(chat) = agent_chat(&home, &old, &[], None) else { panic!("chat") };
        assert!(chat.note.is_some());

        let mut new = old.clone();
        new.tokens_input = Some(0);
        new.tokens_output = Some(0);
        new.llm_ms = Some(0);
        let DetailBody::Conversation(chat) = agent_chat(&home, &new, &[], Some("0.1.7")) else { panic!("chat") };
        assert_eq!(chat.note, None, "zeros are recorded usage, not missing usage");
    }

    #[test]
    fn agent_detail_has_no_result_or_info_tabs() {
        assert!(detail_tabs(NodeKind::Agent).is_empty());
        assert_eq!(detail_tabs(NodeKind::Task), vec!["output", "stderr"]);
    }

    #[test]
    fn merge_prefers_live_and_keeps_disk_only() {
        let live = vec![rec("sh_live", true)];
        let disk = vec![rec("sh_old", false), rec("sh_live", false)];
        let merged = merge_tasks(live, &disk);
        assert_eq!(merged.len(), 2);
        let live = merged.iter().find(|t| t.task_id == "sh_live").unwrap();
        assert_eq!(live.status, TaskStatus::Running);
        assert!(merged.iter().any(|t| t.task_id == "sh_old"));
    }

    #[test]
    fn cap_keeps_newest() {
        let ev = |ts: u64| parse_line(&format!(r#"{{"ts":{ts},"type":"x","id":"{ts}"}}"#), Some("s")).unwrap();
        let mut events: Vec<EventLine> = (1..=10).map(ev).collect();
        cap_events(&mut events, 3);
        assert_eq!(events.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![8, 9, 10]);
    }

    #[test]
    fn incremental_events_read_only_new_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-tui-ev-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(&path, "{\"ts\":1,\"type\":\"a\"}\n{\"ts\":2,\"type\":\"b\"}\n").unwrap();
        let (first, off) = read_new_events(&path, Some("s"), 0);
        assert_eq!(first.len(), 2);
        assert!(off > 0);
        let (again, off2) = read_new_events(&path, Some("s"), off);
        assert!(again.is_empty());
        assert_eq!(off, off2);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, r#"{{"ts":3,"type":"c"}}"#).unwrap();
        drop(f);
        let (more, _) = read_new_events(&path, Some("s"), off);
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].ts, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
