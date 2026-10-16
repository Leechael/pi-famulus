//! Pure TUI state: tree construction, scope/sort/filter, cursor, layout.
//!
//! No I/O. The collector feeds a [`crate::inspect::Snapshot`] plus events;
//! [`TopState::apply`] rebuilds the forest and restabilizes the cursor.

use crate::events::EventLine;
use crate::fmt;
use crate::inspect::{self, AgentRecord, Row, SessionView, Snapshot, TranscriptTurn};
use crate::proto::{TaskRecord, TaskStatus};
use crate::stats::{self, By};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tree,
    Agents,
    Tasks,
    Events,
}

impl Tab {
    pub fn next(self) -> Tab {
        match self {
            Tab::Tree => Tab::Agents,
            Tab::Agents => Tab::Tasks,
            Tab::Tasks => Tab::Events,
            Tab::Events => Tab::Tree,
        }
    }

    pub fn prev(self) -> Tab {
        match self {
            Tab::Tree => Tab::Events,
            Tab::Agents => Tab::Tree,
            Tab::Tasks => Tab::Agents,
            Tab::Events => Tab::Tasks,
        }
    }

    pub fn from_digit(c: char) -> Option<Tab> {
        match c {
            '1' => Some(Tab::Tree),
            '2' => Some(Tab::Agents),
            '3' => Some(Tab::Tasks),
            '4' => Some(Tab::Events),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only sessions/agents/tasks that are currently running. Default for `top`.
    ActiveOnly,
    /// Connected sessions, plus a gone session that still has running work.
    Connected,
    /// Every retained record.
    AllRetained,
}

impl Scope {
    pub fn next(self) -> Scope {
        match self {
            Scope::ActiveOnly => Scope::Connected,
            Scope::Connected => Scope::AllRetained,
            Scope::AllRetained => Scope::ActiveOnly,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::Connected => "Connected",
            Scope::ActiveOnly => "Active",
            Scope::AllRetained => "All",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Cpu,
    Now,
    Wall,
    Tasks,
    Tokens,
    Time,
}

impl SortKey {
    pub const ALL: [SortKey; 6] = [
        SortKey::Cpu,
        SortKey::Now,
        SortKey::Wall,
        SortKey::Tasks,
        SortKey::Tokens,
        SortKey::Time,
    ];

    pub fn next(self) -> SortKey {
        let i = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> SortKey {
        let i = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Now => "NOW%",
            SortKey::Wall => "WALL",
            SortKey::Tasks => "TASKS",
            SortKey::Tokens => "TOKENS",
            SortKey::Time => "TIME",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Session,
    Main,
    Run,
    Agent,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthClass {
    Compact,
    Standard,
    Wide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightClass {
    Short,
    Standard,
    Tall,
}

/// In-app refresh presets (`+` / `-`). A custom `--interval` is kept until
/// the user steps it onto this ladder.
pub const INTERVALS_MS: [u64; 4] = [500, 1_000, 2_000, 5_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Node,
    CwdModel,
    Status,
    Tasks,
    Wall,
    Cpu,
    Now,
    Cores,
    TokIo,
    TokS,
    Cache,
    WallSplit,
    Reason,
    Id,
    Kind,
    Dur,
    Title,
    Sess,
    Cwd,
    Exit,
    WorkKind,
    Sample,
}

impl Col {
    pub fn header(self) -> &'static str {
        match self {
            Col::Node => "NODE / ID",
            Col::CwdModel => "CWD / MODEL",
            Col::Status => "STATUS",
            Col::Tasks => "TASKS",
            Col::Wall => "WALL",
            Col::Cpu => "CPU",
            Col::Now => "NOW%",
            Col::Cores => "CORES",
            Col::TokIo => "TOK I/O",
            Col::TokS => "TOK/S",
            Col::Cache => "CACHE R/W",
            Col::WallSplit => "WALL L/T/Q",
            Col::Reason => "REASON",
            Col::Id => "ID",
            Col::Kind => "KIND",
            Col::Dur => "DUR",
            Col::Title => "TITLE",
            Col::Sess => "SESS",
            Col::Cwd => "CWD",
            Col::Exit => "EXIT",
            Col::WorkKind => "WORK_KIND",
            Col::Sample => "SAMPLE",
        }
    }

    /// Fixed column width, or `None` for the flex column.
    pub fn width(self) -> Option<u16> {
        Some(match self {
            Col::Node | Col::Title => return None,
            Col::CwdModel => 20,
            Col::Status => 9,
            Col::Tasks => 5,
            Col::Wall | Col::Cpu | Col::Dur => 6,
            Col::Now | Col::Cores => 5,
            Col::TokIo | Col::Cache => 11,
            Col::TokS => 6,
            Col::WallSplit => 16,
            Col::Reason => 12,
            Col::Id => 12,
            Col::Kind => 7,
            Col::Sess => 9,
            Col::Cwd => 18,
            Col::Exit => 7,
            Col::WorkKind => 10,
            Col::Sample => 8,
        })
    }
}

pub fn width_class(cols: u16) -> WidthClass {
    if cols < 100 {
        WidthClass::Compact
    } else if cols < 140 {
        WidthClass::Standard
    } else {
        WidthClass::Wide
    }
}

pub fn height_class(rows: u16) -> HeightClass {
    if rows < 20 {
        HeightClass::Short
    } else if rows < 28 {
        HeightClass::Standard
    } else {
        HeightClass::Tall
    }
}

pub fn header_height(h: HeightClass) -> u16 {
    match h {
        HeightClass::Short => 2,
        // Boxed bands: border + 4 / 5 stat rows.
        HeightClass::Standard => 6,
        HeightClass::Tall => 7,
    }
}

pub fn tree_cols(w: WidthClass) -> &'static [Col] {
    match w {
        WidthClass::Compact => &[Col::Node, Col::Status, Col::Wall, Col::Cpu, Col::Now, Col::TokIo],
        WidthClass::Standard => &[
            Col::Node,
            Col::CwdModel,
            Col::Status,
            Col::Tasks,
            Col::Wall,
            Col::Cpu,
            Col::Now,
            Col::Cores,
            Col::TokIo,
            Col::TokS,
        ],
        WidthClass::Wide => &[
            Col::Node,
            Col::CwdModel,
            Col::Status,
            Col::Tasks,
            Col::Wall,
            Col::Cpu,
            Col::Now,
            Col::Cores,
            Col::TokIo,
            Col::TokS,
            Col::Cache,
            Col::WallSplit,
            Col::Reason,
        ],
    }
}

pub fn agent_cols(w: WidthClass) -> &'static [Col] {
    tree_cols(w)
}

pub fn task_cols(w: WidthClass) -> &'static [Col] {
    match w {
        WidthClass::Compact => &[Col::Id, Col::Kind, Col::Status, Col::Dur, Col::Now, Col::TokIo, Col::Title],
        WidthClass::Standard => &[
            Col::Id,
            Col::Kind,
            Col::Status,
            Col::Dur,
            Col::Cpu,
            Col::Now,
            Col::TokIo,
            Col::Sess,
            Col::Cwd,
            Col::Cores,
            Col::Exit,
            Col::Title,
        ],
        WidthClass::Wide => &[
            Col::Id,
            Col::Kind,
            Col::Status,
            Col::Dur,
            Col::Cpu,
            Col::Now,
            Col::Sess,
            Col::Cwd,
            Col::Cores,
            Col::Exit,
            Col::WorkKind,
            Col::Sample,
            Col::Reason,
            Col::Title,
        ],
    }
}

/// Sum `Some` values and ignore `None`, so one missing field does not blank
/// the total (unlike `inspect::sum_agent_field`'s strict `try_fold`).
pub fn sum_partial(values: impl IntoIterator<Item = Option<u64>>) -> Option<u64> {
    let mut out = None;
    for n in values.into_iter().flatten() {
        out = Some(out.unwrap_or(0u64).saturating_add(n));
    }
    out
}

fn add_partial(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    sum_partial([a, b])
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub tasks: u64,
    pub wall_ms: u64,
    pub cpu_ms: Option<u64>,
    pub cpu_now: Option<f64>,
    pub cpu_stale: bool,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub tok_s: Option<f64>,
    /// Output tokens and LLM ms summed only over agents that have both; the
    /// source of `tok_s` on run / session roll-ups.
    pub rate: Option<(u64, u64)>,
    pub cache_r: Option<u64>,
    pub cache_w: Option<u64>,
    pub llm_ms: Option<u64>,
    pub tool_ms: Option<u64>,
    pub queue_ms: Option<u64>,
    #[allow(dead_code)]
    pub wall_other_ms: Option<u64>,
    pub running: bool,
    pub started_at: u64,
    pub active_at: u64,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub session_id: String,
    pub parent: Option<String>,
    pub label: String,
    pub extra: String,
    pub title: String,
    pub status: String,
    pub reason: Option<String>,
    pub work_kind: Option<String>,
    pub cwd: Option<String>,
    pub exit: Option<String>,
    pub metrics: Metrics,
    pub children: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct VisibleRow {
    pub id: String,
    pub depth: usize,
    pub expanded: bool,
    pub has_hidden_children: bool,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub ts: u64,
    pub session: String,
    pub src: String,
    pub ty: String,
    pub id: String,
    pub summary: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkKindRow {
    pub kind: String,
    pub used: usize,
    /// Slot budget; 0 for kinds without one (monitors).
    pub total: usize,
    pub tasks: u64,
    pub wall_ms: u64,
    pub cpu_ms: Option<u64>,
    pub cpu_now: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Header {
    pub version: String,
    pub pid: Option<u32>,
    pub uptime_ms: Option<u64>,
    pub live: bool,
    pub sessions_conn: usize,
    pub sessions_tot: usize,
    pub agents_used: usize,
    pub agents_total: usize,
    pub tasks_run: usize,
    pub tasks_sh: usize,
    pub tasks_ag: usize,
    pub cpu_now: Option<f64>,
    pub cpu_tot: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub tok_s: Option<f64>,
    pub llm_share: Option<u64>,
    pub tool_share: Option<u64>,
    pub queue_share: Option<u64>,
    pub work_kinds: Vec<WorkKindRow>,
}

impl Default for Header {
    fn default() -> Self {
        Header {
            version: crate::VERSION.to_string(),
            pid: None,
            uptime_ms: None,
            live: false,
            sessions_conn: 0,
            sessions_tot: 0,
            agents_used: 0,
            agents_total: 0,
            tasks_run: 0,
            tasks_sh: 0,
            tasks_ag: 0,
            cpu_now: None,
            cpu_tot: None,
            tokens_in: None,
            tokens_out: None,
            tok_s: None,
            llm_share: None,
            tool_share: None,
            queue_share: None,
            work_kinds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HitRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl HitRect {
    pub fn from_xywh(x: u16, y: u16, w: u16, h: u16) -> HitRect {
        HitRect { x, y, w, h }
    }

    pub fn contains(self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x.saturating_add(self.w) && row >= self.y && row < self.y.saturating_add(self.h)
    }
}

/// What a click on the tab bar lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabHit {
    Tab(Tab),
    Scope,
    Sort,
}

#[derive(Debug, Default)]
pub struct MouseHit {
    pub tabs: HitRect,
    pub table: HitRect,
    pub detail: HitRect,
    pub chat: HitRect,
    pub scrollbar: HitRect,
    pub dragging_bar: bool,
    /// Column ranges `[x0, x1)` on the tab bar, relative to `tabs.x`.
    pub tab_hits: Vec<(u16, u16, TabHit)>,
    pub last: Option<(String, std::time::Instant)>,
}

/// Agent conversation plus the footer stats the chat view needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub turns: Vec<TranscriptTurn>,
    /// Child display name for speaker headers.
    pub agent: String,
    /// `(value, label)` pairs for the footer, e.g. `("12k", "in")`.
    pub stats: Vec<(String, String)>,
    /// Shown in place of `stats` when the record carries no usage at all.
    pub note: Option<String>,
    pub running: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DetailBody {
    #[default]
    Empty,
    Conversation(Chat),
    /// Task output or stderr tail, with the task's identity for the pinned
    /// info panel.
    Log { info: Box<TaskInfo>, lines: Vec<String> },
}

/// What the pinned panel above a task's output shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskInfo {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub running: bool,
    pub command: String,
    pub cwd: String,
    pub pid: u32,
    pub session: String,
    pub started_at: u64,
    pub wall_ms: u64,
    pub cpu_ms: Option<u64>,
    pub cpu_now: Option<f64>,
    pub cpu_stale: bool,
    pub work_kind: String,
    pub exit: Option<String>,
    pub reason: Option<String>,
    pub output: String,
}

impl Eq for TaskInfo {}

/// Search and display state for a task's output view.
#[derive(Debug, Clone, Default)]
pub struct LogView {
    /// Search text; `a|b` matches either, lowercase is case-insensitive.
    pub query: String,
    /// Typing into the search prompt.
    pub editing: bool,
    /// Show only matching lines instead of highlighting them.
    pub filter: bool,
    /// Keep blank lines (hidden by default).
    pub show_blank: bool,
    /// Index into `matches` of the match `n` / `N` last jumped to.
    pub current: usize,
    /// Painted line indexes holding a match, from the last draw.
    pub matches: Vec<usize>,
    /// Scroll to the first match at or below the view on the next draw.
    pub seek: bool,
    /// Tab click ranges `(x0, x1, tab)` on row `tab_y`, absolute.
    pub tab_hits: Vec<(u16, u16, usize)>,
    pub tab_y: u16,
}

#[derive(Debug, Clone)]
pub struct Detail {
    pub id: String,
    pub tab: usize,
    pub follow: bool,
    pub scroll: usize,
    pub title: String,
    pub tabs: Vec<String>,
    pub body: DetailBody,
    /// Wrapped line count from the last draw; used to clamp scroll.
    pub wrap_len: usize,
    /// Viewport height from the last draw; used to map scrollbar drags.
    pub view_h: usize,
    /// `o` expands every foldable block.
    pub tools_expanded: bool,
    /// Per-block open set (thinking / tool cards). Click toggles one id.
    pub open: HashSet<usize>,
    /// Toggle hit for each visible line. Only the ▸/▾ cell is armed.
    pub hits: Vec<Option<FoldHit>>,
    /// Task output search / filter state.
    pub log: LogView,
}

/// Click target for one foldable header. Coordinates are relative to the chat pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldHit {
    pub id: usize,
    pub x0: u16,
    pub x1: u16,
}

#[derive(Debug)]
pub struct TopState {
    pub tab: Tab,
    pub scope: Scope,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub filter: String,
    pub filter_editing: bool,
    pub selected: Option<String>,
    pub scroll: usize,
    pub collapsed: HashSet<String>,
    pub revealed: HashSet<String>,
    pub expand_all: bool,
    pub preview: bool,
    pub paused: bool,
    pub interval_ms: u64,
    pub help: bool,
    pub detail: Option<Detail>,
    pub confirm_stop: Option<String>,
    pub message: Option<String>,
    pub session_prefix: Option<String>,
    pub cwd_prefix: Option<String>,
    pub events_id: Option<String>,
    pub nodes: HashMap<String, Node>,
    pub roots: Vec<String>,
    pub task_rows: Vec<Row>,
    pub events: Vec<EventRow>,
    pub event_index: HashMap<String, usize>,
    pub task_index: HashMap<String, usize>,
    pub preview_body: DetailBody,
    pub header: Header,
    pub visible: Vec<VisibleRow>,
    pub now: u64,
    pub want_refresh: bool,
    pub mouse: MouseHit,
    /// Recent machine CPU% samples, oldest first, one per snapshot.
    pub cpu_hist: Vec<f64>,
    /// Recent running-task counts, oldest first, one per snapshot.
    pub run_hist: Vec<f64>,
}

/// Samples kept for the header graphs (two minutes at the default 1s).
pub const HIST_CAP: usize = 120;

fn push_hist(hist: &mut Vec<f64>, v: f64) {
    hist.push(v);
    if hist.len() > HIST_CAP {
        hist.drain(..hist.len() - HIST_CAP);
    }
}

impl TopState {
    pub fn new(interval_ms: u64, session: Option<String>, cwd: Option<String>) -> TopState {
        TopState {
            tab: Tab::Tree,
            scope: Scope::ActiveOnly,
            sort: SortKey::Cpu,
            sort_desc: true,
            filter: String::new(),
            filter_editing: false,
            selected: None,
            scroll: 0,
            collapsed: HashSet::new(),
            revealed: HashSet::new(),
            expand_all: false,
            preview: false,
            paused: false,
            interval_ms: snap_interval(interval_ms),
            help: false,
            detail: None,
            confirm_stop: None,
            message: None,
            session_prefix: session,
            cwd_prefix: cwd.as_deref().map(inspect::normalize_dir),
            events_id: None,
            nodes: HashMap::new(),
            roots: Vec::new(),
            task_rows: Vec::new(),
            events: Vec::new(),
            event_index: HashMap::new(),
            task_index: HashMap::new(),
            preview_body: DetailBody::Empty,
            header: Header::default(),
            visible: Vec::new(),
            now: 0,
            want_refresh: false,
            mouse: MouseHit::default(),
            cpu_hist: Vec::new(),
            run_hist: Vec::new(),
        }
    }

    pub fn apply(&mut self, snap: &Snapshot, events: &[EventLine], kind_slots: &HashMap<String, (usize, usize)>) {
        self.now = snap.now;
        let prefixes = inspect::session_prefixes(
            snap.sessions
                .keys()
                .map(|s| s.as_str())
                .chain(snap.tasks.iter().map(|t| t.session_id.as_str()))
                .chain(snap.agents.iter().map(|a| a.session_id.as_str())),
        );
        let prev = self.selected.clone();
        let ancestors = {
            let mut v = Vec::new();
            let mut cur = prev.clone();
            while let Some(id) = cur {
                match self.nodes.get(&id).and_then(|n| n.parent.clone()) {
                    Some(p) => {
                        v.push(p.clone());
                        cur = Some(p);
                    }
                    None => break,
                }
            }
            v
        };
        let (nodes, roots) = build_forest(snap, &prefixes);
        self.nodes = nodes;
        self.roots = roots;
        self.rebuild_header(snap, kind_slots);
        self.task_rows = inspect::all_rows(snap);
        self.task_index = self.task_rows.iter().enumerate().map(|(i, r)| (r.id.clone(), i)).collect();
        self.events = events.iter().map(|e| event_row(e, &prefixes)).collect();
        self.event_index = self
            .events
            .iter()
            .enumerate()
            .map(|(i, e)| (event_visible_id(e), i))
            .collect();
        self.recompute_visible();
        if self.selected.as_ref() != prev.as_ref() {
            for a in ancestors {
                if self.visible.iter().any(|r| r.id == a) {
                    self.selected = Some(a);
                    break;
                }
            }
        }
    }

    fn rebuild_header(&mut self, snap: &Snapshot, kind_slots: &HashMap<String, (usize, usize)>) {
        let names: HashMap<String, String> = snap.agents.iter().map(|a| (a.child_id.clone(), a.name.clone())).collect();
        let prefixes = inspect::session_prefixes(
            snap.sessions.keys().map(|s| s.as_str()).chain(snap.tasks.iter().map(|t| t.session_id.as_str())),
        );
        let tasks: Vec<&TaskRecord> = snap
            .tasks
            .iter()
            .filter(|t| match self.scope {
                Scope::ActiveOnly => t.status == TaskStatus::Running,
                Scope::Connected => t.status == TaskStatus::Running || snap.connected.contains(&t.session_id),
                Scope::AllRetained => true,
            })
            .collect();
        let kind_groups = stats::group(&tasks, By::Kind, &names, &prefixes, snap.now);
        let agent_groups = stats::group(&tasks, By::Agent, &names, &prefixes, snap.now);
        let tot = stats::total(&agent_groups);
        let agents_for_tokens = snap.agents.iter().filter(|a| match self.scope {
            Scope::ActiveOnly => !inspect::agent_status_terminal(&a.status),
            Scope::Connected => !inspect::agent_status_terminal(&a.status) || snap.connected.contains(&a.session_id),
            Scope::AllRetained => true,
        });
        let tokens_in = sum_partial(agents_for_tokens.clone().map(|a| a.tokens_input));
        let tokens_out = sum_partial(agents_for_tokens.clone().map(|a| a.tokens_output));
        let llm = sum_partial(agents_for_tokens.clone().map(|a| a.llm_ms));
        let tool = sum_partial(agents_for_tokens.clone().map(|a| a.tool_ms));
        let queue = sum_partial(agents_for_tokens.clone().map(|a| a.queue_ms));
        let wall_split = llm.unwrap_or(0) + tool.unwrap_or(0) + queue.unwrap_or(0);
        let share = |part: Option<u64>| part.filter(|_| wall_split > 0).map(|p| p * 100 / wall_split);
        let running_agents = snap.agents.iter().filter(|a| !inspect::agent_status_terminal(&a.status)).count();
        let (agents_used, agents_total) = snap
            .daemon
            .as_ref()
            .and_then(|d| d.agent_capacity.as_ref())
            .map(|c| (c.used, c.total))
            .unwrap_or((running_agents, crate::capacity::DEFAULT_MAX_AGENTS));
        let work_kinds = kind_groups
            .iter()
            .map(|g| {
                let kind = g.kind.clone().unwrap_or_else(|| "unknown".into());
                let (used, total) = kind_slots.get(&kind).copied().unwrap_or((0, kind_slot_default(&kind)));
                WorkKindRow {
                    kind,
                    used,
                    total,
                    tasks: g.tasks,
                    wall_ms: g.wall_ms,
                    cpu_ms: (g.measured > 0).then(|| g.cpu_ms()),
                    cpu_now: g.cpu_now_percent,
                }
            })
            .collect();
        self.header = Header {
            version: snap.daemon.as_ref().map(|d| d.version.clone()).unwrap_or_else(|| crate::VERSION.to_string()),
            pid: snap.daemon.as_ref().map(|d| d.pid),
            uptime_ms: snap.daemon.as_ref().map(|d| d.uptime_ms),
            live: snap.daemon.is_some(),
            sessions_conn: snap.connected.len(),
            sessions_tot: snap.sessions.len(),
            agents_used,
            agents_total,
            tasks_run: snap.tasks.iter().filter(|t| t.status == TaskStatus::Running).count(),
            tasks_sh: tasks.len(),
            tasks_ag: snap
                .agents
                .iter()
                .filter(|a| match self.scope {
                    Scope::ActiveOnly => !inspect::agent_status_terminal(&a.status),
                    Scope::Connected => !inspect::agent_status_terminal(&a.status) || snap.connected.contains(&a.session_id),
                    Scope::AllRetained => true,
                })
                .count(),
            cpu_now: tot.cpu_now_percent,
            cpu_tot: (tot.measured > 0).then(|| tot.cpu_ms()),
            tokens_in,
            tokens_out,
            tok_s: inspect::pooled_output_tokens_per_second(
                agents_for_tokens.clone().map(|a| inspect::rate_pair(a.tokens_output, a.llm_ms)),
            ),
            llm_share: share(llm),
            tool_share: share(tool),
            queue_share: share(queue),
            work_kinds,
        };
        push_hist(&mut self.cpu_hist, self.header.cpu_now.unwrap_or(0.0));
        push_hist(&mut self.run_hist, self.header.tasks_run as f64);
    }

    pub fn recompute_visible(&mut self) {
        let prev = self.selected.clone();
        match self.tab {
            Tab::Tree => self.visible = self.flatten_tree(),
            Tab::Agents => self.visible = self.flatten_agents(),
            Tab::Tasks => self.visible = self.flatten_tasks(),
            Tab::Events => self.visible = self.flatten_events(),
        }
        self.stabilize_cursor(prev.as_deref());
    }

    fn session_allowed(&self, sid: &str) -> bool {
        if let Some(p) = &self.session_prefix {
            if !sid.starts_with(p.as_str()) {
                return false;
            }
        }
        true
    }

    fn cwd_allowed(&self, cwd: Option<&str>) -> bool {
        match (&self.cwd_prefix, cwd) {
            (None, _) => true,
            (Some(dir), Some(c)) => inspect::under_dir(c, dir),
            (Some(_), None) => false,
        }
    }

    fn in_scope(&self, node: &Node) -> bool {
        if !self.session_allowed(&node.session_id) {
            return false;
        }
        if !self.cwd_allowed(node.cwd.as_deref()) && node.kind == NodeKind::Session {
            // A session with no cwd still passes when it has in-scope children;
            // cwd is enforced on leaves/agents below.
        } else if !self.cwd_allowed(node.cwd.as_deref()) && matches!(node.kind, NodeKind::Task | NodeKind::Agent | NodeKind::Main) {
            return false;
        }
        match self.scope {
            Scope::AllRetained => true,
            Scope::ActiveOnly => node.metrics.running,
            Scope::Connected => {
                node.status == "connected"
                    || node.metrics.running
                    || self.nodes.get(&session_key(&node.session_id)).is_some_and(|s| s.status == "connected")
            }
        }
    }

    fn matches_filter(&self, node: &Node) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_ascii_lowercase();
        let hay = [
            node.id.as_str(),
            node.label.as_str(),
            node.extra.as_str(),
            node.title.as_str(),
            node.status.as_str(),
            node.work_kind.as_deref().unwrap_or(""),
            node.cwd.as_deref().unwrap_or(""),
            node.reason.as_deref().unwrap_or(""),
        ];
        hay.iter().any(|s| s.to_ascii_lowercase().contains(&q))
    }

    fn subtree_matches(&self, id: &str) -> bool {
        let Some(n) = self.nodes.get(id) else {
            return false;
        };
        self.matches_filter(n) || n.children.iter().any(|c| self.subtree_matches(c))
    }

    fn default_expanded(&self, id: &str) -> bool {
        let Some(n) = self.nodes.get(id) else {
            return false;
        };
        if self.expand_all || !self.filter.is_empty() {
            return true;
        }
        n.metrics.running && n.kind != NodeKind::Task
    }

    fn is_expanded(&self, id: &str) -> bool {
        if self.collapsed.contains(id) {
            return false;
        }
        if self.revealed.contains(id) {
            return true;
        }
        self.default_expanded(id)
    }

    /// Finished task leaves stay hidden unless the parent was opened or
    /// expand-all / a filter is active.
    fn show_child(&self, parent: &str, child: &str) -> bool {
        let Some(n) = self.nodes.get(child) else {
            return false;
        };
        if !self.in_scope(n)
            && self.scope != Scope::AllRetained
            && !n.metrics.running
            && self.scope == Scope::ActiveOnly
        {
            return false;
        }
        if !self.filter.is_empty() {
            return self.subtree_matches(child);
        }
        if n.kind == NodeKind::Task && !n.metrics.running {
            return self.expand_all || self.revealed.contains(parent);
        }
        self.in_scope(n) || n.children.iter().any(|c| self.show_child(child, c))
    }

    fn visible_ancestor(&self, id: &str) -> Option<String> {
        let mut cur = self.nodes.get(id).and_then(|n| n.parent.clone());
        while let Some(p) = cur {
            if self.visible.iter().any(|r| r.id == p) {
                return Some(p);
            }
            cur = self.nodes.get(&p).and_then(|n| n.parent.clone());
        }
        None
    }

    fn flatten_tree(&self) -> Vec<VisibleRow> {
        let mut out = Vec::new();
        let mut roots: Vec<&String> = self.roots.iter().filter(|id| {
            self.nodes.get(id.as_str()).is_some_and(|n| self.in_scope(n) || n.children.iter().any(|c| self.show_child(id, c)))
                && (self.filter.is_empty() || self.subtree_matches(id))
        }).collect();
        roots.sort_by(|a, b| self.cmp_nodes(a, b));
        for id in roots {
            self.walk(id, 0, &mut out);
        }
        out
    }

    fn walk(&self, id: &str, depth: usize, out: &mut Vec<VisibleRow>) {
        let Some(n) = self.nodes.get(id) else {
            return;
        };
        if self.scope == Scope::ActiveOnly && !n.metrics.running && !n.children.iter().any(|c| self.nodes.get(c).is_some_and(|x| x.metrics.running)) {
            return;
        }
        // Active view: skip the synthetic "main" bucket so session-owned
        // monitors sit directly under the session.
        if n.kind == NodeKind::Main && self.scope == Scope::ActiveOnly {
            let mut kids: Vec<&String> = n.children.iter().filter(|c| self.show_child(id, c)).collect();
            kids.sort_by(|a, b| self.cmp_nodes(a, b));
            for c in kids {
                self.walk(c, depth, out);
            }
            return;
        }
        let kids: Vec<&String> = n.children.iter().filter(|c| self.show_child(id, c)).collect();
        let expanded = self.is_expanded(id) && !kids.is_empty();
        out.push(VisibleRow {
            id: id.to_string(),
            depth,
            expanded,
            has_hidden_children: !n.children.is_empty(),
        });
        if expanded {
            let mut kids = kids;
            kids.sort_by(|a, b| self.cmp_nodes(a, b));
            for c in kids {
                self.walk(c, depth + 1, out);
            }
        }
    }

    fn flatten_agents(&self) -> Vec<VisibleRow> {
        let mut ids: Vec<String> = self
            .nodes
            .values()
            .filter(|n| n.kind == NodeKind::Agent)
            .filter(|n| self.in_scope(n) && self.matches_filter(n))
            .map(|n| n.id.clone())
            .collect();
        ids.sort_by(|a, b| self.cmp_nodes(a, b));
        ids.into_iter()
            .map(|id| VisibleRow {
                id,
                depth: 0,
                expanded: false,
                has_hidden_children: false,
            })
            .collect()
    }

    fn flatten_tasks(&self) -> Vec<VisibleRow> {
        let mut rows: Vec<&Row> = self
            .task_rows
            .iter()
            .filter(|r| self.session_allowed(&r.session_id))
            .filter(|r| self.cwd_allowed(r.cwd.as_deref()))
            .filter(|r| match self.scope {
                Scope::AllRetained => true,
                Scope::ActiveOnly => r.running,
                Scope::Connected => r.running || self.nodes.get(&session_key(&r.session_id)).is_some_and(|s| s.status == "connected"),
            })
            .filter(|r| {
                if self.filter.is_empty() {
                    return true;
                }
                let q = self.filter.to_ascii_lowercase();
                [
                    r.id.as_str(),
                    r.title.as_str(),
                    r.status.as_str(),
                    r.kind.as_str(),
                    r.work_kind.as_deref().unwrap_or(""),
                    r.cwd.as_deref().unwrap_or(""),
                    r.session_id.as_str(),
                ]
                .iter()
                .any(|s| s.to_ascii_lowercase().contains(&q))
            })
            .collect();
        rows.sort_by(|a, b| cmp_task_rows(a, b, self.sort, self.sort_desc));
        rows.into_iter()
            .map(|r| VisibleRow {
                id: r.id.clone(),
                depth: 0,
                expanded: false,
                has_hidden_children: false,
            })
            .collect()
    }

    fn flatten_events(&self) -> Vec<VisibleRow> {
        let q = self.filter.to_ascii_lowercase();
        self.events
            .iter()
            .rev()
            .filter(|e| self.session_allowed(if e.session == "-" { "" } else { &e.session }) || e.session == "-")
            .filter(|e| self.events_id.as_ref().is_none_or(|id| e.keys.iter().any(|k| k == id) || e.id == *id))
            .filter(|e| {
                q.is_empty()
                    || e.id.to_ascii_lowercase().contains(&q)
                    || e.ty.to_ascii_lowercase().contains(&q)
                    || e.summary.to_ascii_lowercase().contains(&q)
                    || e.session.to_ascii_lowercase().contains(&q)
            })
            .map(|e| VisibleRow {
                id: event_visible_id(e),
                depth: 0,
                expanded: false,
                has_hidden_children: false,
            })
            .collect()
    }

    fn cmp_nodes(&self, a: &str, b: &str) -> Ordering {
        let (Some(na), Some(nb)) = (self.nodes.get(a), self.nodes.get(b)) else {
            return a.cmp(b);
        };
        cmp_metrics(&na.metrics, &nb.metrics, self.sort, self.sort_desc).then_with(|| na.id.cmp(&nb.id))
    }

    fn stabilize_cursor(&mut self, prev: Option<&str>) {
        if self.visible.is_empty() {
            self.selected = None;
            self.scroll = 0;
            return;
        }
        if let Some(id) = prev {
            if let Some(i) = self.visible.iter().position(|r| r.id == id) {
                self.selected = Some(id.to_string());
                self.scroll = self.scroll.min(i);
                return;
            }
            if let Some(parent) = self.visible_ancestor(id) {
                if let Some(i) = self.visible.iter().position(|r| r.id == parent) {
                    self.selected = Some(parent);
                    self.scroll = self.scroll.min(i);
                    return;
                }
            }
        }
        if self.selected.as_ref().is_none_or(|id| !self.visible.iter().any(|r| r.id == *id)) {
            self.selected = Some(self.visible[0].id.clone());
            self.scroll = 0;
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.visible.iter().position(|r| r.id == *id)
    }

    pub fn page(&mut self, pages: isize, page_rows: usize) {
        let delta = pages.saturating_mul(page_rows.max(1) as isize);
        self.move_sel(delta);
    }

    pub fn ensure_scroll(&mut self, page_rows: usize) {
        let page = page_rows.max(1);
        let i = self.selected_index().unwrap_or(0);
        if i < self.scroll {
            self.scroll = i;
        } else if i >= self.scroll.saturating_add(page) {
            self.scroll = i + 1 - page;
        }
        let max = self.visible.len().saturating_sub(page);
        self.scroll = self.scroll.min(max);
    }

    pub fn move_sel(&mut self, delta: isize) {
        let Some(i) = self.selected_index() else {
            if !self.visible.is_empty() {
                self.selected = Some(self.visible[0].id.clone());
            }
            return;
        };
        let n = self.visible.len() as isize;
        let j = (i as isize + delta).clamp(0, n - 1) as usize;
        self.selected = Some(self.visible[j].id.clone());
    }

    pub fn jump_home(&mut self) {
        if let Some(r) = self.visible.first() {
            self.selected = Some(r.id.clone());
            self.scroll = 0;
        }
    }

    pub fn jump_end(&mut self) {
        if let Some(r) = self.visible.last() {
            self.selected = Some(r.id.clone());
        }
    }

    pub fn fold(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.tab != Tab::Tree {
            return;
        }
        if self.is_expanded(&id) {
            self.revealed.remove(&id);
            self.collapsed.insert(id);
            self.recompute_visible();
        } else if let Some(parent) = self.visible_ancestor(&id) {
            self.selected = Some(parent);
        }
    }

    pub fn unfold(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.tab != Tab::Tree {
            return;
        }
        if self.nodes.get(&id).is_some_and(|n| !n.children.is_empty()) {
            self.collapsed.remove(&id);
            self.revealed.insert(id);
            self.recompute_visible();
        }
    }

    pub fn toggle_expand_all(&mut self) {
        self.expand_all = !self.expand_all;
        self.collapsed.clear();
        self.revealed.clear();
        self.recompute_visible();
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            if tab != Tab::Events {
                self.events_id = None;
            }
            self.recompute_visible();
        }
    }

    pub fn cycle_scope(&mut self) {
        self.scope = self.scope.next();
        self.recompute_visible();
    }

    pub fn cycle_sort(&mut self, prev: bool) {
        self.sort = if prev { self.sort.prev() } else { self.sort.next() };
        self.recompute_visible();
    }

    pub fn invert_sort(&mut self) {
        self.sort_desc = !self.sort_desc;
        self.recompute_visible();
    }

    pub fn set_filter(&mut self, f: String) {
        self.filter = f;
        self.recompute_visible();
    }

    pub fn jump_events_for_selected(&mut self) {
        if let Some(id) = self.selected.clone() {
            let key = self.nodes.get(&id).map(|n| match n.kind {
                NodeKind::Session => n.session_id.clone(),
                _ => n.id.clone(),
            }).unwrap_or(id);
            self.events_id = Some(key);
            self.tab = Tab::Events;
            self.recompute_visible();
        }
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.selected.as_ref().and_then(|id| self.nodes.get(id))
    }

    /// Redraw on a short cadence so running indicators spin: the chat's
    /// footer in detail, or running rows on the table. Paused stays still.
    pub fn wants_anim(&self) -> bool {
        if let Some(d) = &self.detail {
            return matches!(&d.body, DetailBody::Conversation(c) if c.running);
        }
        !self.paused
            && matches!(self.tab, Tab::Tree | Tab::Agents)
            && self.visible.iter().any(|v| {
                self.nodes
                    .get(&v.id)
                    .is_some_and(|n| n.metrics.running && matches!(n.kind, NodeKind::Task | NodeKind::Agent))
            })
    }

    pub fn bump_interval(&mut self, up: bool) {
        self.interval_ms = step_interval(self.interval_ms, up);
    }

    pub fn selected_is_stoppable_task(&self) -> Option<&Node> {
        self.selected_node().filter(|n| n.kind == NodeKind::Task && n.metrics.running)
    }

    pub fn count_tab(&self, tab: Tab) -> usize {
        match tab {
            Tab::Tree => self
                .roots
                .iter()
                .filter(|id| {
                    self.nodes.get(id.as_str()).is_some_and(|n| self.in_scope(n) || n.children.iter().any(|c| self.show_child(id, c)))
                        && (self.filter.is_empty() || self.subtree_matches(id))
                })
                .count(),
            Tab::Agents => self
                .nodes
                .values()
                .filter(|n| n.kind == NodeKind::Agent)
                .filter(|n| self.in_scope(n) && self.matches_filter(n))
                .count(),
            Tab::Tasks => self
                .task_rows
                .iter()
                .filter(|r| self.session_allowed(&r.session_id))
                .filter(|r| self.cwd_allowed(r.cwd.as_deref()))
                .filter(|r| match self.scope {
                    Scope::AllRetained => true,
                    Scope::ActiveOnly => r.running,
                    Scope::Connected => {
                        r.running || self.nodes.get(&session_key(&r.session_id)).is_some_and(|s| s.status == "connected")
                    }
                })
                .filter(|r| {
                    if self.filter.is_empty() {
                        return true;
                    }
                    let q = self.filter.to_ascii_lowercase();
                    [
                        r.id.as_str(),
                        r.title.as_str(),
                        r.status.as_str(),
                        r.kind.as_str(),
                        r.work_kind.as_deref().unwrap_or(""),
                        r.cwd.as_deref().unwrap_or(""),
                        r.session_id.as_str(),
                    ]
                    .iter()
                    .any(|s| s.to_ascii_lowercase().contains(&q))
                })
                .count(),
            Tab::Events => self.events.len(),
        }
    }
}

pub fn snap_interval(ms: u64) -> u64 {
    if INTERVALS_MS.contains(&ms) || ms > 0 {
        ms.max(100)
    } else {
        1_000
    }
}

pub fn step_interval(current: u64, up: bool) -> u64 {
    if up {
        INTERVALS_MS.iter().copied().find(|v| *v > current).unwrap_or(*INTERVALS_MS.last().unwrap())
    } else {
        INTERVALS_MS.iter().rev().copied().find(|v| *v < current).unwrap_or(INTERVALS_MS[0])
    }
}

pub fn session_key(sid: &str) -> String {
    format!("s:{sid}")
}

fn main_key(sid: &str) -> String {
    format!("m:{sid}")
}

fn session_display_name(prefix: &str, cwd: Option<&str>, pid: Option<u32>) -> String {
    let place = cwd
        .and_then(|c| std::path::Path::new(c).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && n != "." && n != "/");
    let head = match place {
        Some(p) if p != prefix => format!("{p} · {prefix}"),
        Some(p) => p,
        None => prefix.to_string(),
    };
    match pid {
        Some(p) => format!("{head} (pi:{p})"),
        None => head,
    }
}

fn build_forest(snap: &Snapshot, prefixes: &HashMap<String, String>) -> (HashMap<String, Node>, Vec<String>) {
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut session_ids: HashSet<String> = snap.sessions.keys().cloned().collect();
    for t in &snap.tasks {
        session_ids.insert(t.session_id.clone());
    }
    for a in &snap.agents {
        session_ids.insert(a.session_id.clone());
    }
    let mut roots = Vec::new();
    for sid in &session_ids {
        let view = snap.sessions.get(sid);
        let prefix = prefixes.get(sid).cloned().unwrap_or_else(|| sid.clone());
        let cwd = view.and_then(|v| v.cwd.clone());
        let state = view.map(|v| v.state.as_str()).unwrap_or("gone");
        let pid = view.and_then(|v| v.pi_pid);
        let label = session_display_name(&prefix, cwd.as_deref(), pid);
        let id = session_key(sid);
        nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                kind: NodeKind::Session,
                session_id: sid.clone(),
                parent: None,
                extra: cwd.as_deref().map(fmt::tilde).unwrap_or_default(),
                title: cwd.as_deref().map(fmt::tilde).unwrap_or_default(),
                label,
                status: state.to_string(),
                reason: None,
                work_kind: None,
                cwd,
                exit: None,
                metrics: Metrics {
                    running: false,
                    started_at: view.and_then(|v| v.since).unwrap_or(0),
                    active_at: view.and_then(|v| v.last_seen).unwrap_or(0),
                    ..Default::default()
                },
                children: Vec::new(),
            },
        );
        roots.push(id);
    }

    for a in &snap.agents {
        let parent_run = a.run_id.as_deref();
        let sess = session_key(&a.session_id);
        let parent = if let Some(rid) = parent_run {
            ensure_run(&mut nodes, rid, &a.session_id, &sess);
            rid.to_string()
        } else {
            sess.clone()
        };
        let node = agent_node(a, snap.sessions.get(&a.session_id), snap.now);
        link(&mut nodes, &parent, node);
    }

    for t in &snap.tasks {
        let sess = session_key(&t.session_id);
        let child_id = t.origin.as_ref().and_then(|o| o.child_id.clone());
        let parent = match &child_id {
            Some(ch) => {
                if !nodes.contains_key(ch) {
                    let run = t.origin.as_ref().and_then(|o| o.run_id.clone());
                    let p = if let Some(rid) = &run {
                        ensure_run(&mut nodes, rid, &t.session_id, &sess);
                        rid.clone()
                    } else {
                        sess.clone()
                    };
                    link(&mut nodes, &p, synthetic_agent(ch, &t.session_id));
                }
                ch.clone()
            }
            None => {
                let main = main_key(&t.session_id);
                if !nodes.contains_key(&main) {
                    let prefix = prefixes.get(&t.session_id).cloned().unwrap_or_else(|| t.session_id.clone());
                    link(
                        &mut nodes,
                        &sess,
                        Node {
                            id: main.clone(),
                            kind: NodeKind::Main,
                            session_id: t.session_id.clone(),
                            parent: Some(sess.clone()),
                            label: format!("main {prefix}"),
                            extra: "shells".into(),
                            title: format!("main {prefix}"),
                            status: "completed".into(),
                            reason: None,
                            work_kind: None,
                            cwd: Some(t.cwd.clone()),
                            exit: None,
                            metrics: Metrics::default(),
                            children: Vec::new(),
                        },
                    );
                }
                main
            }
        };
        link(&mut nodes, &parent, task_node(t, snap.now));
    }

    let root_ids = roots.clone();
    for id in &root_ids {
        rollup(&mut nodes, id);
    }
    (nodes, roots)
}

fn ensure_run(nodes: &mut HashMap<String, Node>, rid: &str, session_id: &str, sess: &str) {
    if nodes.contains_key(rid) {
        return;
    }
    link(
        nodes,
        sess,
        Node {
            id: rid.to_string(),
            kind: NodeKind::Run,
            session_id: session_id.to_string(),
            parent: Some(sess.to_string()),
            label: rid.to_string(),
            extra: "children".into(),
            title: rid.to_string(),
            status: "completed".into(),
            reason: None,
            work_kind: None,
            cwd: None,
            exit: None,
            metrics: Metrics::default(),
            children: Vec::new(),
        },
    );
}

fn link(nodes: &mut HashMap<String, Node>, parent: &str, mut child: Node) {
    child.parent = Some(parent.to_string());
    let id = child.id.clone();
    if let Some(p) = nodes.get_mut(parent) {
        if !p.children.contains(&id) {
            p.children.push(id.clone());
        }
    }
    nodes.insert(id, child);
}

fn agent_node(a: &AgentRecord, session: Option<&SessionView>, now: u64) -> Node {
    let running = !inspect::agent_status_terminal(&a.status);
    let end = a.ended_at.unwrap_or(now);
    let wall = a.started_at.map(|s| end.saturating_sub(s)).unwrap_or(0);
    Node {
        id: a.child_id.clone(),
        kind: NodeKind::Agent,
        session_id: a.session_id.clone(),
        parent: None,
        label: {
            let tok = tok_io_cell(a.tokens_input, a.tokens_output);
            if tok == "-" {
                format!("{} {}", a.name, short_id(&a.child_id))
            } else {
                format!("{} {}  {tok}", a.name, short_id(&a.child_id))
            }
        },
        extra: a.model.clone().unwrap_or_else(|| a.agent.clone()),
        title: a.title(),
        status: a.status.clone(),
        reason: a.end_reason.clone(),
        work_kind: None,
        cwd: session.and_then(|s| s.cwd.clone()),
        exit: None,
        metrics: Metrics {
            tasks: 0,
            wall_ms: wall,
            tokens_in: a.tokens_input,
            tokens_out: a.tokens_output,
            tok_s: inspect::agent_output_tokens_per_second(a.tokens_output, a.llm_ms),
            rate: inspect::rate_pair(a.tokens_output, a.llm_ms),
            cache_r: a.tokens_cache_read,
            cache_w: a.tokens_cache_write,
            llm_ms: a.llm_ms,
            tool_ms: a.tool_ms,
            queue_ms: a.queue_ms,
            wall_other_ms: a.wall_other_ms,
            running,
            started_at: a.started_at.unwrap_or(0),
            active_at: a.ended_at.or(a.started_at).unwrap_or(0),
            ..Default::default()
        },
        children: Vec::new(),
    }
}

fn synthetic_agent(id: &str, session_id: &str) -> Node {
    Node {
        id: id.to_string(),
        kind: NodeKind::Agent,
        session_id: session_id.to_string(),
        parent: None,
        label: id.to_string(),
        extra: String::new(),
        title: id.to_string(),
        status: "unknown".into(),
        reason: None,
        work_kind: None,
        cwd: None,
        exit: None,
        metrics: Metrics::default(),
        children: Vec::new(),
    }
}

fn task_node(t: &TaskRecord, now: u64) -> Node {
    let running = t.status == TaskStatus::Running;
    let wall = t.ended_at.unwrap_or(now).saturating_sub(t.started_at);
    let cpu = if running {
        inspect::cpu_ms(t.live_cpu_user_ms, t.live_cpu_sys_ms)
    } else {
        inspect::cpu_ms(t.cpu_user_ms, t.cpu_sys_ms)
    };
    let (cpu_now, cpu_stale) = if running {
        if t.live_cpu_stale {
            (None, true)
        } else {
            (t.live_cpu_percent, false)
        }
    } else {
        (None, false)
    };
    let status = serde_json::to_value(t.status)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| format!("{:?}", t.status));
    Node {
        id: t.task_id.clone(),
        kind: NodeKind::Task,
        session_id: t.session_id.clone(),
        parent: None,
        label: inspect::task_title(t),
        extra: inspect::work_kind(t).to_string(),
        title: inspect::task_title(t),
        status,
        reason: t.end_reason.clone(),
        work_kind: Some(inspect::work_kind(t).to_string()),
        cwd: Some(t.cwd.clone()),
        exit: t.signal.clone().or_else(|| t.exit_code.map(|c| c.to_string())),
        metrics: Metrics {
            tasks: 1,
            wall_ms: wall,
            cpu_ms: cpu,
            cpu_now,
            cpu_stale,
            running,
            started_at: t.started_at,
            active_at: t.ended_at.unwrap_or(t.started_at),
            ..Default::default()
        },
        children: Vec::new(),
    }
}

fn rollup(nodes: &mut HashMap<String, Node>, id: &str) {
    let children = nodes.get(id).map(|n| n.children.clone()).unwrap_or_default();
    for c in &children {
        rollup(nodes, c);
    }
    let Some(kind) = nodes.get(id).map(|n| n.kind) else {
        return;
    };
    if kind == NodeKind::Task {
        return;
    }
    let own_tokens = nodes.get(id).map(|n| {
        (
            n.metrics.tokens_in,
            n.metrics.tokens_out,
            n.metrics.tok_s,
            n.metrics.rate,
            n.metrics.cache_r,
            n.metrics.cache_w,
            n.metrics.llm_ms,
            n.metrics.tool_ms,
            n.metrics.queue_ms,
            n.metrics.running,
            n.metrics.started_at,
            n.metrics.wall_ms,
        )
    });
    let mut m = Metrics::default();
    if let Some((tin, tout, toks, rate, cr, cw, llm, tool, queue, running, started, wall)) = own_tokens {
        if kind == NodeKind::Agent {
            m.tokens_in = tin;
            m.tokens_out = tout;
            m.tok_s = toks;
            m.rate = rate;
            m.cache_r = cr;
            m.cache_w = cw;
            m.llm_ms = llm;
            m.tool_ms = tool;
            m.queue_ms = queue;
            m.wall_ms = wall;
        }
        m.running = running;
        m.started_at = started;
        m.active_at = started;
    }
    for c in &children {
        let Some(cn) = nodes.get(c) else { continue };
        m.tasks = m.tasks.saturating_add(if cn.kind == NodeKind::Task { 1 } else { cn.metrics.tasks });
        m.wall_ms = m.wall_ms.saturating_add(if kind == NodeKind::Agent && cn.kind == NodeKind::Task {
            cn.metrics.wall_ms
        } else if kind == NodeKind::Agent {
            0
        } else {
            cn.metrics.wall_ms
        });
        if kind != NodeKind::Agent {
            m.tokens_in = add_partial(m.tokens_in, cn.metrics.tokens_in);
            m.tokens_out = add_partial(m.tokens_out, cn.metrics.tokens_out);
            m.cache_r = add_partial(m.cache_r, cn.metrics.cache_r);
            m.cache_w = add_partial(m.cache_w, cn.metrics.cache_w);
            m.llm_ms = add_partial(m.llm_ms, cn.metrics.llm_ms);
            m.rate = match (m.rate, cn.metrics.rate) {
                (Some((t, ms)), Some((ct, cms))) => Some((t.saturating_add(ct), ms.saturating_add(cms))),
                (a, b) => a.or(b),
            };
            m.tool_ms = add_partial(m.tool_ms, cn.metrics.tool_ms);
            m.queue_ms = add_partial(m.queue_ms, cn.metrics.queue_ms);
        }
        m.cpu_ms = add_partial(m.cpu_ms, cn.metrics.cpu_ms);
        merge_now(&mut m, &cn.metrics);
        m.running |= cn.metrics.running;
        if cn.metrics.started_at > 0 {
            m.started_at = if m.started_at == 0 { cn.metrics.started_at } else { m.started_at.min(cn.metrics.started_at) };
        }
        m.active_at = m.active_at.max(cn.metrics.active_at);
    }
    if kind != NodeKind::Agent {
        m.tok_s = inspect::pooled_output_tokens_per_second([m.rate]);
    }
    if let Some(n) = nodes.get_mut(id) {
        if m.running && n.kind != NodeKind::Task && n.status == "completed" {
            n.status = "running".into();
        }
        n.metrics.tasks = m.tasks;
        if kind != NodeKind::Agent || n.metrics.wall_ms == 0 {
            n.metrics.wall_ms = m.wall_ms;
        }
        n.metrics.cpu_ms = m.cpu_ms;
        n.metrics.cpu_now = m.cpu_now;
        n.metrics.cpu_stale = m.cpu_stale;
        n.metrics.running = m.running;
        n.metrics.started_at = m.started_at;
        n.metrics.active_at = m.active_at;
        if kind != NodeKind::Agent {
            n.metrics.tokens_in = m.tokens_in;
            n.metrics.tokens_out = m.tokens_out;
            n.metrics.tok_s = m.tok_s;
            n.metrics.rate = m.rate;
            n.metrics.cache_r = m.cache_r;
            n.metrics.cache_w = m.cache_w;
            n.metrics.llm_ms = m.llm_ms;
            n.metrics.tool_ms = m.tool_ms;
            n.metrics.queue_ms = m.queue_ms;
        }
        if n.kind == NodeKind::Main {
            n.extra = format!("{} shells", n.metrics.tasks);
        } else if n.kind == NodeKind::Run {
            n.extra = format!("{} children", n.children.len());
        }
    }
}

fn merge_now(into: &mut Metrics, from: &Metrics) {
    if from.cpu_stale {
        into.cpu_stale = true;
        into.cpu_now = None;
    } else if !into.cpu_stale {
        if let Some(p) = from.cpu_now {
            into.cpu_now = Some(into.cpu_now.unwrap_or(0.0) + p);
        }
    }
}

fn short_id(id: &str) -> String {
    let rest = id.split_once('_').map(|(_, r)| r).unwrap_or(id);
    if rest.len() <= 8 {
        id.to_string()
    } else {
        let prefix = id.split_once('_').map(|(p, _)| p).unwrap_or("");
        if prefix.is_empty() {
            rest[..8].to_string()
        } else {
            format!("{prefix}_{}", &rest[..8.min(rest.len())])
        }
    }
}

pub fn event_visible_id(e: &EventRow) -> String {
    format!("{}:{}:{}", e.ts, e.ty, e.id)
}

fn event_row(e: &EventLine, prefixes: &HashMap<String, String>) -> EventRow {
    let mut keys = Vec::new();
    if let Some(id) = &e.id {
        keys.push(id.clone());
    }
    if let Some(s) = &e.session {
        keys.push(s.clone());
    }
    if let Some(c) = e.raw.get("child_id").and_then(|v| v.as_str()) {
        keys.push(c.to_string());
    }
    if let Some(r) = e.raw.get("run_id").and_then(|v| v.as_str()) {
        keys.push(r.to_string());
    }
    if let Some(arr) = e.raw.get("ids").and_then(|v| v.as_array()) {
        for x in arr {
            if let Some(s) = x.as_str() {
                keys.push(s.to_string());
            }
        }
    }
    let mut fields = Vec::new();
    if let Some(o) = e.raw.as_object() {
        for (k, v) in o {
            if matches!(k.as_str(), "ts" | "src" | "type" | "id") || v.is_null() {
                continue;
            }
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            fields.push(format!("{k}={val}"));
        }
    }
    EventRow {
        ts: e.ts,
        session: e
            .session
            .as_ref()
            .map(|s| prefixes.get(s).cloned().unwrap_or_else(|| s.clone()))
            .unwrap_or_else(|| "-".into()),
        src: e.src.clone(),
        ty: e.ty.clone(),
        id: e.id.clone().unwrap_or_else(|| "-".into()),
        summary: fields.join(" "),
        keys,
    }
}

fn cmp_metrics(a: &Metrics, b: &Metrics, key: SortKey, desc: bool) -> Ordering {
    let primary = cmp_key(a, b, key);
    let ordered = if desc { primary.reverse() } else { primary };
    let rest = [
        SortKey::Now,
        SortKey::Cpu,
        SortKey::Wall,
        SortKey::Tasks,
        SortKey::Tokens,
        SortKey::Time,
    ];
    let mut ord = b.running.cmp(&a.running).then(ordered);
    for k in rest {
        if k == key {
            continue;
        }
        ord = ord.then_with(|| cmp_key(a, b, k).reverse());
    }
    ord
}

fn cmp_key(a: &Metrics, b: &Metrics, key: SortKey) -> Ordering {
    match key {
        SortKey::Cpu => a.cpu_ms.unwrap_or(0).cmp(&b.cpu_ms.unwrap_or(0)),
        SortKey::Now => cmp_f64(a.cpu_now.unwrap_or(0.0), b.cpu_now.unwrap_or(0.0)),
        SortKey::Wall => a.wall_ms.cmp(&b.wall_ms),
        SortKey::Tasks => a.tasks.cmp(&b.tasks),
        SortKey::Tokens => a.tokens_out.unwrap_or(0).cmp(&b.tokens_out.unwrap_or(0)),
        SortKey::Time => a.active_at.cmp(&b.active_at),
    }
}

fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

fn cmp_task_rows(a: &Row, b: &Row, key: SortKey, desc: bool) -> Ordering {
    let am = row_metrics(a);
    let bm = row_metrics(b);
    cmp_metrics(&am, &bm, key, desc).then_with(|| a.id.cmp(&b.id))
}

fn row_metrics(r: &Row) -> Metrics {
    let cpu = if r.running {
        inspect::cpu_ms(r.live_cpu_user_ms, r.live_cpu_sys_ms)
    } else {
        inspect::cpu_ms(r.cpu_user_ms, r.cpu_sys_ms)
    };
    Metrics {
        tasks: 1,
        wall_ms: r.duration_ms.unwrap_or(0),
        cpu_ms: cpu,
        cpu_now: if r.live_cpu_stale { None } else { r.live_cpu_percent },
        cpu_stale: r.live_cpu_stale,
        running: r.running,
        started_at: r.started_at.unwrap_or(0),
        active_at: r.active_at.unwrap_or(0),
        ..Default::default()
    }
}

/// Format helpers shared with the renderer so unit tests can pin labels.
pub fn cpu_cell(ms: Option<u64>) -> String {
    inspect::cpu_text(ms)
}

pub fn now_cell(percent: Option<f64>, stale: bool) -> String {
    if stale {
        "stale".into()
    } else {
        percent.map(|p| format!("{p:.1}")).unwrap_or_else(|| "-".into())
    }
}

pub fn wall_cell(ms: u64) -> String {
    fmt::human_duration(ms)
}

pub fn tok_io_cell(i: Option<u64>, o: Option<u64>) -> String {
    match (i, o) {
        (None, None) => "-".into(),
        (i, o) => format!("{}/{}", compact_num(i.unwrap_or(0)), compact_num(o.unwrap_or(0))),
    }
}

pub fn compact_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{}k", n / 1_000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Budget for a kind the daemon did not report: the configured default for
/// budgeted kinds, 0 for kinds that take no slot (monitors).
fn kind_slot_default(kind: &str) -> usize {
    match kind {
        "test-suite" | "test" => crate::capacity::DEFAULT_MAX_TEST,
        k if crate::capacity::WORK_KINDS.contains(&k) => crate::capacity::DEFAULT_MAX_AGENTS,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Origin, TaskKind};
    use std::collections::BTreeMap;

    #[allow(clippy::too_many_arguments)]
    fn rec(
        id: &str,
        sid: &str,
        child: Option<&str>,
        run: Option<&str>,
        cmd: &str,
        wall: u64,
        cpu: Option<(u64, u64)>,
        status: TaskStatus,
        now_pct: Option<f64>,
    ) -> TaskRecord {
        TaskRecord {
            task_id: id.into(),
            session_id: sid.into(),
            kind: TaskKind::Shell,
            command: cmd.into(),
            cwd: "/tmp/app".into(),
            pid: 1,
            status,
            exit_code: None,
            signal: None,
            started_at: 1_000,
            ended_at: (status != TaskStatus::Running).then_some(1_000 + wall),
            output_path: String::new(),
            output_size: 0,
            origin: child.map(|c| Origin {
                via: "child-bash".into(),
                child_id: Some(c.into()),
                run_id: run.map(|r| r.into()),
            }).or_else(|| {
                Some(Origin { via: "bash-fg".into(), child_id: None, run_id: None })
            }),
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: cpu.map(|c| c.0),
            cpu_sys_ms: cpu.map(|c| c.1),
            max_rss_kb: cpu.map(|_| 1),
            live_cpu_user_ms: now_pct.map(|_| cpu.map(|c| c.0).unwrap_or(0)),
            live_cpu_sys_ms: now_pct.map(|_| cpu.map(|c| c.1).unwrap_or(0)),
            live_cpu_percent: now_pct,
            live_cpu_sampled_at: now_pct.map(|_| 2_000),
            live_cpu_stale: false,
            label: None,
        }
    }

    fn agent(id: &str, sid: &str, run: &str, name: &str, status: &str, tokens_in: Option<u64>) -> AgentRecord {
        AgentRecord {
            child_id: id.into(),
            run_id: Some(run.into()),
            session_id: sid.into(),
            name: name.into(),
            agent: "worker".into(),
            model: Some("haiku-4-5".into()),
            status: status.into(),
            started_at: Some(1_000),
            ended_at: (status != "running").then_some(2_000),
            error: None,
            end_reason: None,
            prompt_head: None,
            result_tail: None,
            tool_calls: None,
            tokens_input: tokens_in,
            tokens_output: tokens_in.map(|n| n / 5),
            tokens_cache_read: None,
            tokens_cache_write: None,
            llm_ms: tokens_in.map(|_| 1_000),
            tool_ms: None,
            queue_ms: None,
            wall_other_ms: None,
            wall_approximate: false,
            transcript: None,
            stale: false,
        }
    }

    fn session(id: &str, state: &str) -> (String, SessionView) {
        (
            id.into(),
            SessionView {
                session_id: id.into(),
                pi_pid: Some(9),
                cwd: Some("/Users/me/workshop/app".into()),
                state: state.into(),
                since: Some(1),
                last_seen: Some(2),
                protocol: Some(4),
                extension_version: None,
            },
        )
    }

    fn snapshot(tasks: Vec<TaskRecord>, agents: Vec<AgentRecord>, sessions: Vec<(&str, &str)>) -> Snapshot {
        let sessions: BTreeMap<_, _> = sessions.into_iter().map(|(id, st)| session(id, st)).collect();
        let connected: HashSet<String> = sessions
            .values()
            .filter(|s| s.state == "connected")
            .map(|s| s.session_id.clone())
            .collect();
        Snapshot {
            now: 10_000,
            daemon: None,
            tasks,
            agents,
            sessions,
            connected,
        }
    }

    fn state_from(snap: &Snapshot) -> TopState {
        let mut s = TopState::new(1_000, None, None);
        s.scope = Scope::AllRetained;
        s.apply(snap, &[], &HashMap::new());
        s
    }

    #[test]
    fn tree_rolls_up_session_run_agent_task_and_folds_finished_leaves() {
        let snap = snapshot(
            vec![
                rec("sh_live", "sessA", Some("ch_work"), Some("run_1"), "pytest -n 4", 4_000, Some((3_000, 200)), TaskStatus::Running, Some(85.0)),
                rec("sh_old", "sessA", Some("ch_work"), Some("run_1"), "rg foo", 100, Some((50, 10)), TaskStatus::Completed, None),
                rec("sh_main", "sessA", None, None, "git status", 200, Some((20, 20)), TaskStatus::Completed, None),
                rec("sh_done", "sessB", Some("ch_idle"), Some("run_2"), "cargo build", 2_000, Some((1_000, 0)), TaskStatus::Completed, None),
            ],
            vec![
                agent("ch_work", "sessA", "run_1", "P5-contracts", "running", Some(100)),
                agent("ch_idle", "sessB", "run_2", "idle", "completed", None),
            ],
            vec![("sessA", "connected"), ("sessB", "gone")],
        );
        let mut s = state_from(&snap);
        let sess_a = session_key("sessA");
        let sess = s.nodes.get(&sess_a).unwrap();
        assert_eq!(sess.metrics.tasks, 3, "main + two agent tasks");
        assert_eq!(sess.metrics.cpu_ms, Some(3_300));
        assert_eq!(sess.metrics.cpu_now, Some(85.0));
        assert_eq!(sess.metrics.tokens_in, Some(100));
        let work = s.nodes.get("ch_work").unwrap();
        assert_eq!(work.metrics.tasks, 2);
        assert_eq!(work.metrics.cpu_now, Some(85.0));
        assert!(work.metrics.running);
        assert!(work.label.contains("100/20"), "subagent tokens on the node: {}", work.label);

        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"s:sessA"), "{ids:?}");
        assert!(ids.contains(&"ch_work"), "live agent visible: {ids:?}");
        assert!(ids.contains(&"sh_live"), "running leaf auto-expanded: {ids:?}");
        assert!(!ids.contains(&"sh_old"), "finished leaf folded: {ids:?}");
        assert!(!ids.contains(&"sh_main"), "finished main shells folded: {ids:?}");

        s.selected = Some("ch_work".into());
        s.unfold();
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"sh_old"), "unfold reveals finished child: {ids:?}");
    }

    #[test]
    fn scope_hides_gone_idle_sessions_by_default() {
        let snap = snapshot(
            vec![
                rec("sh_live", "sessA", Some("ch_work"), Some("run_1"), "sleep 1", 100, None, TaskStatus::Running, Some(10.0)),
                rec("sh_old", "sessB", None, None, "echo", 50, Some((1, 0)), TaskStatus::Completed, None),
                rec("sh_done", "sessC", Some("ch_done"), Some("run_3"), "rg foo", 50, Some((1, 0)), TaskStatus::Completed, None),
            ],
            vec![
                agent("ch_work", "sessA", "run_1", "w", "running", Some(1)),
                agent("ch_done", "sessC", "run_3", "done", "completed", None),
            ],
            vec![("sessA", "connected"), ("sessB", "gone"), ("sessC", "connected")],
        );
        let mut s = TopState::new(1_000, None, None);
        s.apply(&snap, &[], &HashMap::new());
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.iter().any(|id| id.contains("sessA")), "{ids:?}");
        assert!(!ids.iter().any(|id| id.contains("sessB")), "gone idle hidden: {ids:?}");
        assert!(!ids.iter().any(|id| id.contains("sessC")), "idle connected hidden: {ids:?}");
        assert!(!ids.iter().any(|id| *id == "ch_done" || *id == "sh_done" || *id == "sh_old"), "completed hidden: {ids:?}");
        assert_eq!(s.scope, Scope::ActiveOnly);

        s.scope = Scope::Connected;
        s.recompute_visible();
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.iter().any(|id| id.contains("sessC")), "connected idle session: {ids:?}");

        s.scope = Scope::AllRetained;
        s.recompute_visible();
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.iter().any(|id| id.contains("sessB")), "{ids:?}");
    }

    #[test]
    fn default_hides_completed_even_when_unfolded() {
        let snap = snapshot(
            vec![
                rec("sh_live", "sessA", Some("ch_work"), Some("run_1"), "pytest", 100, None, TaskStatus::Running, Some(10.0)),
                rec("sh_old", "sessA", Some("ch_work"), Some("run_1"), "rg foo", 50, Some((1, 0)), TaskStatus::Completed, None),
            ],
            vec![
                agent("ch_work", "sessA", "run_1", "w", "running", Some(1)),
                agent("ch_done", "sessA", "run_2", "done", "completed", None),
            ],
            vec![("sessA", "connected")],
        );
        let mut s = TopState::new(1_000, None, None);
        s.apply(&snap, &[], &HashMap::new());
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"sh_live"), "{ids:?}");
        assert!(!ids.contains(&"sh_old"), "{ids:?}");
        assert!(!ids.contains(&"ch_done"), "{ids:?}");

        s.selected = Some("ch_work".into());
        s.unfold();
        s.toggle_expand_all();
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(!ids.contains(&"sh_old"), "active scope never reveals completed: {ids:?}");
        assert!(!ids.contains(&"ch_done"), "{ids:?}");
        assert_eq!(s.count_tab(Tab::Tasks), 2, "running shell + running agent");
        assert_eq!(s.count_tab(Tab::Agents), 1);
    }

    #[test]
    fn filter_reveals_matching_finished_leaf() {
        let snap = snapshot(
            vec![rec("sh_old", "sessA", Some("ch_work"), Some("run_1"), "rg secret-token", 100, Some((1, 0)), TaskStatus::Completed, None)],
            vec![agent("ch_work", "sessA", "run_1", "w", "completed", None)],
            vec![("sessA", "connected")],
        );
        let mut s = state_from(&snap);
        assert!(!s.visible.iter().any(|r| r.id == "sh_old"));
        s.set_filter("secret-token".into());
        assert!(s.visible.iter().any(|r| r.id == "sh_old"), "{:?}", s.visible.iter().map(|r| &r.id).collect::<Vec<_>>());
    }

    #[test]
    fn default_sort_ranks_running_and_hot_cpu_first() {
        let snap = snapshot(
            vec![
                rec("sh_hot", "s1", Some("ch_hot"), Some("run_h"), "pytest", 1_000, Some((100, 0)), TaskStatus::Running, Some(90.0)),
                rec("sh_warm", "s1", Some("ch_warm"), Some("run_w"), "pytest", 8_000, Some((5_000, 0)), TaskStatus::Completed, None),
                rec("sh_idle", "s1", Some("ch_idle"), Some("run_i"), "echo", 50, None, TaskStatus::Completed, None),
            ],
            vec![
                agent("ch_hot", "s1", "run_h", "hot", "running", Some(1)),
                agent("ch_warm", "s1", "run_w", "warm", "completed", Some(2)),
                agent("ch_idle", "s1", "run_i", "idle", "completed", None),
            ],
            vec![("s1", "connected")],
        );
        let mut s = state_from(&snap);
        s.set_tab(Tab::Agents);
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids[0], "ch_hot", "running + NOW% first: {ids:?}");
        assert_eq!(ids[1], "ch_warm", "measured CPU next: {ids:?}");
    }

    #[test]
    fn cursor_follows_id_across_refresh_and_falls_back_to_parent() {
        let mut snap = snapshot(
            vec![
                rec("sh_a", "s1", Some("ch_a"), Some("run_1"), "true", 10, Some((1, 0)), TaskStatus::Running, Some(5.0)),
                rec("sh_b", "s1", Some("ch_b"), Some("run_1"), "true", 10, Some((9, 0)), TaskStatus::Running, Some(50.0)),
            ],
            vec![
                agent("ch_a", "s1", "run_1", "a", "running", None),
                agent("ch_b", "s1", "run_1", "b", "running", None),
            ],
            vec![("s1", "connected")],
        );
        let mut s = state_from(&snap);
        s.selected = Some("sh_a".into());
        s.recompute_visible();
        assert_eq!(s.selected.as_deref(), Some("sh_a"));
        snap.tasks.retain(|t| t.task_id != "sh_a");
        s.apply(&snap, &[], &HashMap::new());
        assert_eq!(s.selected.as_deref(), Some("ch_a"), "gone leaf → parent agent");
    }

    #[test]
    fn columns_grow_with_width_and_never_drop_flex() {
        assert_eq!(width_class(80), WidthClass::Compact);
        assert_eq!(width_class(120), WidthClass::Standard);
        assert_eq!(width_class(160), WidthClass::Wide);
        for w in [WidthClass::Compact, WidthClass::Standard, WidthClass::Wide] {
            let cols = tree_cols(w);
            assert!(cols.contains(&Col::Node) && cols.contains(&Col::Now));
            assert!(cols.iter().any(|c| c.width().is_none()), "{w:?} needs a flex column");
            let t = task_cols(w);
            assert!(t.contains(&Col::Title) && t.iter().any(|c| c.width().is_none()));
        }
        assert!(tree_cols(WidthClass::Wide).contains(&Col::WallSplit));
        assert!(tree_cols(WidthClass::Compact).contains(&Col::TokIo));
        assert!(task_cols(WidthClass::Wide).contains(&Col::WorkKind));
        assert!(task_cols(WidthClass::Compact).contains(&Col::TokIo));
    }

    #[test]
    fn monitors_have_no_slot_budget() {
        assert_eq!(kind_slot_default("monitor"), 0);
        assert_eq!(kind_slot_default("test"), crate::capacity::DEFAULT_MAX_TEST);
        assert_eq!(kind_slot_default("build"), crate::capacity::DEFAULT_MAX_AGENTS);
    }

    #[test]
    fn partial_token_sum_skips_missing_records() {
        assert_eq!(sum_partial([Some(10), None, Some(5)]), Some(15));
        assert_eq!(sum_partial([None, None]), None);
    }

    #[test]
    fn tok_s_rollups_skip_agents_without_llm_time() {
        // 1000 out over 10 s = 100 tok/s. The second agent has tokens but an
        // unmeasured (0 / missing) LLM time; it must not add tokens alone.
        let mut fast = agent("ch_a", "sessA", "run_1", "a", "running", Some(5_000));
        fast.tokens_output = Some(1_000);
        fast.llm_ms = Some(10_000);
        let mut zero = agent("ch_b", "sessA", "run_1", "b", "running", Some(5_000));
        zero.tokens_output = Some(9_000);
        zero.llm_ms = Some(0);
        let mut missing = agent("ch_c", "sessA", "run_1", "c", "running", Some(5_000));
        missing.tokens_output = Some(9_000);
        missing.llm_ms = None;
        let snap = snapshot(vec![], vec![fast, zero, missing], vec![("sessA", "connected")]);
        let mut s = TopState::new(1_000, None, None);
        s.apply(&snap, &[], &HashMap::new());
        let run = s.nodes.values().find(|n| n.kind == NodeKind::Run).expect("run node");
        assert_eq!(run.metrics.tok_s, Some(100.0));
        assert_eq!(s.nodes.get(&session_key("sessA")).unwrap().metrics.tok_s, Some(100.0));
        assert_eq!(s.header.tok_s, Some(100.0));
        assert_eq!(s.nodes.get("ch_b").unwrap().metrics.tok_s, None);
        assert_eq!(run.metrics.tokens_out, Some(19_000), "token totals still count every agent");
    }

    #[test]
    fn monitor_row_uses_readable_description() {
        let mut t = rec("mon_watch", "sessA", None, None, "tail -n +1 -F app.log | grep --line-buffered ERROR", 100, None, TaskStatus::Running, None);
        t.kind = TaskKind::Monitor;
        t.label = Some("dev server errors".into());
        assert_eq!(inspect::task_title(&t), "dev server errors");
        let snap = snapshot(vec![t], vec![], vec![("sessA", "connected")]);
        let mut s = TopState::new(1_000, None, None);
        s.apply(&snap, &[], &HashMap::new());
        let n = s.nodes.get("mon_watch").unwrap();
        assert_eq!(n.label, "dev server errors");
        assert_eq!(n.title, "dev server errors");
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"mon_watch"), "{ids:?}");
        assert!(!ids.iter().any(|id| id.starts_with("m:")), "Active skips main: {ids:?}");
        let sess = s.nodes.get(&session_key("sessA")).unwrap();
        assert!(sess.label.starts_with("app · sessA"), "{}", sess.label);
    }

    #[test]
    fn unlabeled_monitor_title_comes_from_the_command() {
        let mut incident = rec(
            "mon_inc",
            "sessA",
            None,
            None,
            "bash /Users/me/storage/2026-09-27-stripe-risk-block-cooldown/watch.sh",
            100,
            None,
            TaskStatus::Running,
            None,
        );
        incident.kind = TaskKind::Monitor;
        let mut ssh = rec(
            "mon_ssh",
            "sessA",
            None,
            None,
            "ssh -o BatchMode=yes phala@phala-cloud-01 'sudo journalctl -fu teehouse'",
            100,
            None,
            TaskStatus::Running,
            None,
        );
        ssh.kind = TaskKind::Monitor;
        let mut tick = rec(
            "mon_tick",
            "sessA",
            None,
            None,
            "while true; do sleep 120; echo TICK; done",
            100,
            None,
            TaskStatus::Running,
            None,
        );
        tick.kind = TaskKind::Monitor;
        let snap = snapshot(vec![incident, ssh, tick], vec![], vec![("sessA", "connected")]);
        let mut s = TopState::new(1_000, None, None);
        s.apply(&snap, &[], &HashMap::new());
        assert_eq!(s.nodes.get("mon_inc").unwrap().label, "stripe-risk-block-cooldown");
        assert_eq!(s.nodes.get("mon_ssh").unwrap().label, "journalctl @phala-cloud-01");
        assert_eq!(s.nodes.get("mon_tick").unwrap().label, "tick every 2m");
        let ids: Vec<&str> = s.visible.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.iter().filter(|id| id.starts_with("s:")).count(), 1);
        assert!(ids.contains(&"mon_inc") && ids.contains(&"mon_ssh") && ids.contains(&"mon_tick"), "{ids:?}");
        assert!(!ids.iter().any(|id| id.starts_with("m:")), "{ids:?}");
        assert_eq!(s.count_tab(Tab::Agents), 0);
    }

    #[test]
    fn interval_steps_along_the_ladder() {
        assert_eq!(step_interval(1_000, true), 2_000);
        assert_eq!(step_interval(2_000, true), 5_000);
        assert_eq!(step_interval(5_000, true), 5_000);
        assert_eq!(step_interval(1_000, false), 500);
        assert_eq!(step_interval(500, false), 500);
        assert_eq!(step_interval(1_500, true), 2_000);
    }
}
