//! The `top` session behind the window: the shared [`Controller`] on its
//! own thread, and the [`Frame`] each change produces for the UI thread.
//!
//! Everything the window shows is computed here with the TUI's own view
//! functions (`pi_famulus::top::ui`), at the size the window reports in
//! character cells, so both frontends show the same rows, cells and text,
//! and scroll / follow / search the same way.

use std::sync::Arc;
use std::thread;

use pi_famulus::top::controller::Controller;
use pi_famulus::top::model::{
    agent_cols, task_cols, tree_cols, width_class, Col, Tab, TabHit, TopState,
};
use pi_famulus::top::ui;
use pi_famulus::top::Input;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use tokio::sync::{mpsc, watch};

/// One styled run of text.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Run {
    pub text: String,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

pub type Runs = Vec<Run>;

/// The window's size in character cells, per region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// Whole window width in columns (picks the TUI's width class).
    pub cols: u16,
    /// Table rows that fit below the header row.
    pub table_rows: usize,
    pub overview_cols: usize,
    pub kinds_cols: usize,
    pub kinds_rows: usize,
    pub preview_cols: usize,
    pub preview_rows: usize,
    /// Text area of an open detail (card, tab row and body).
    pub detail_cols: usize,
    pub detail_rows: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            cols: 120,
            table_rows: 20,
            overview_cols: 70,
            kinds_cols: 50,
            kinds_rows: 5,
            preview_cols: 110,
            preview_rows: 8,
            detail_cols: 110,
            detail_rows: 30,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabItem {
    pub label: String,
    pub count: Option<usize>,
    pub active: bool,
    pub hit: TabHit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub title: String,
    /// Fixed width in characters; `None` takes the rest.
    pub width: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub selected: bool,
    pub cells: Vec<Runs>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scrolled {
    pub len: usize,
    pub height: usize,
    pub scroll: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Detail {
    Task {
        title: String,
        card: Vec<Runs>,
        tabs: Vec<(String, bool)>,
        count: Runs,
        rows: Vec<Runs>,
        bar: Scrolled,
        /// The search prompt is open (native input).
        editing: bool,
        query: String,
        /// Notice or applied query shown beside the keys.
        prompt: Option<Runs>,
        position: Option<String>,
        keys: Vec<Key>,
    },
    Chat {
        title: String,
        rows: Vec<Runs>,
        /// Fold id under each visible row, if it has a chevron.
        folds: Vec<Option<usize>>,
        bar: Scrolled,
        status: Runs,
        /// The agent's live input: (text, facts).
        live: Option<(String, String)>,
        position: Option<String>,
        keys: Vec<Key>,
    },
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Frame {
    pub version: String,
    pub pill: Runs,
    pub pid_up: Option<String>,
    pub messaging: Runs,
    pub overview: Vec<Runs>,
    pub kinds: Vec<Runs>,
    pub tabs: Vec<TabItem>,
    pub scope: String,
    pub sort: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    pub events: bool,
    pub position: (usize, usize),
    pub filter: String,
    pub filter_editing: bool,
    pub message: Option<String>,
    pub confirm_stop: Option<String>,
    pub keys: Vec<Key>,
    pub preview: Option<(Runs, Vec<Runs>)>,
    pub help: bool,
    pub detail: Option<Detail>,
    pub quit: bool,
}

pub enum Msg {
    Input(Input),
    Viewport(Viewport),
}

/// Handle the UI thread keeps: send inputs, watch frames.
#[derive(Clone)]
pub struct Engine {
    tx: mpsc::UnboundedSender<Msg>,
    pub frames: watch::Receiver<Arc<Frame>>,
}

impl Engine {
    pub fn send(&self, input: Input) {
        let _ = self.tx.send(Msg::Input(input));
    }

    pub fn viewport(&self, v: Viewport) {
        let _ = self.tx.send(Msg::Viewport(v));
    }
}

/// Starts the controller on its own thread and runtime.
pub fn start() -> Engine {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (frame_tx, frames) = watch::channel(Arc::new(Frame::default()));
    thread::Builder::new()
        .name("top-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("engine runtime");
            rt.block_on(async move {
                let home = pi_famulus::lifecycle::resolve_home(None);
                let mut ctl = Controller::start(&home, 1_000, None, None, None);
                let mut view = Viewport::default();
                let mut quit = false;
                loop {
                    ctl.settle();
                    let mut frame = build(ctl.state_mut(), &view);
                    frame.quit = quit;
                    if **frame_tx.borrow() != frame {
                        let _ = frame_tx.send(Arc::new(frame));
                    }
                    if quit {
                        return;
                    }
                    let handle = |msg: Msg, view: &mut Viewport| match msg {
                        Msg::Input(i) => Some(i),
                        Msg::Viewport(v) => {
                            *view = v;
                            None
                        }
                    };
                    tokio::select! {
                        () = ctl.wait() => {}
                        msg = rx.recv() => {
                            let Some(msg) = msg else { return };
                            if let Some(input) = handle(msg, &mut view) {
                                quit = ctl.input(input).await.unwrap_or(false);
                            }
                        }
                    }
                    while !quit {
                        let Ok(msg) = rx.try_recv() else { break };
                        if let Some(input) = handle(msg, &mut view) {
                            quit = ctl.input(input).await.unwrap_or(false);
                        }
                    }
                }
            });
        })
        .expect("engine thread");
    Engine { tx, frames }
}

// ── frame ───────────────────────────────────────────────────────────────────

fn rgb(c: Color) -> Option<u32> {
    Some(match c {
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        Color::Reset => return None,
        Color::Black => 0x000000,
        Color::Red => 0xcd3131,
        Color::Green => 0x0dbc79,
        Color::Yellow => 0xe5e510,
        Color::Blue => 0x2472c8,
        Color::Magenta => 0xbc3fbc,
        Color::Cyan => 0x11a8cd,
        Color::Gray => 0xc0c0c0,
        Color::DarkGray => 0x666666,
        Color::LightRed => 0xf14c4c,
        Color::LightGreen => 0x23d18b,
        Color::LightYellow => 0xf5f543,
        Color::LightBlue => 0x3b8eea,
        Color::LightMagenta => 0xd670d6,
        Color::LightCyan => 0x29b8db,
        Color::White => 0xe5e5e5,
        Color::Indexed(_) => return None,
    })
}

fn runs(spans: &[Span<'_>], line_style: ratatui::style::Style) -> Runs {
    spans
        .iter()
        .filter(|s| !s.content.is_empty())
        .map(|s| {
            let st = line_style.patch(s.style);
            Run {
                text: s.content.to_string(),
                fg: st.fg.and_then(rgb),
                bg: st.bg.and_then(rgb),
                bold: st.add_modifier.contains(Modifier::BOLD),
                italic: st.add_modifier.contains(Modifier::ITALIC),
                underline: st.add_modifier.contains(Modifier::UNDERLINED),
            }
        })
        .collect()
}

fn line_runs(line: &Line<'_>) -> Runs {
    runs(&line.spans, line.style)
}

fn keys(hints: &[ui::KeyHint]) -> Vec<Key> {
    hints.iter().map(|(k, l, _)| Key { key: (*k).into(), label: (*l).into() }).collect()
}

fn tabs(state: &TopState) -> Vec<TabItem> {
    [Tab::Tree, Tab::Agents, Tab::Tasks, Tab::Events]
        .into_iter()
        .map(|tab| {
            let (name, count) = match tab {
                Tab::Tree => ("tree", Some(state.count_tab(Tab::Tree))),
                Tab::Agents => ("agents", Some(state.count_tab(Tab::Agents))),
                Tab::Tasks => ("tasks", Some(state.count_tab(Tab::Tasks))),
                Tab::Events => ("events", None),
            };
            // `e` narrows events to one id; say so, Esc clears it.
            let label = match (&state.events_id, tab) {
                (Some(id), Tab::Events) => format!("events for {}", pi_famulus::fmt::truncate_width(id, 12)),
                _ => name.to_string(),
            };
            TabItem { label, count, active: state.tab == tab, hit: TabHit::Tab(tab) }
        })
        .collect()
}

fn table(state: &mut TopState, view: &Viewport) -> (Vec<Column>, Vec<Row>) {
    let page = view.table_rows.max(1);
    state.ensure_scroll(page);
    let start = state.scroll.min(state.visible.len());
    let end = (start + page).min(state.visible.len());
    let window = state.visible[start..end].to_vec();
    let sel = state.selected.clone();
    if state.tab == Tab::Events {
        let columns = ui::EVENTS_HEADER
            .iter()
            .zip([Some(20u16), Some(10), Some(10), Some(19), Some(13), None])
            .map(|(t, w)| Column { title: (*t).into(), width: w })
            .collect();
        let rows = window
            .iter()
            .filter_map(|vr| {
                let ev = &state.events[*state.event_index.get(&vr.id)?];
                let cells = ui::event_spans(ev).iter().map(|s| runs(std::slice::from_ref(s), Default::default())).collect();
                Some(Row { id: vr.id.clone(), selected: sel.as_deref() == Some(vr.id.as_str()), cells })
            })
            .collect();
        return (columns, rows);
    }
    let wclass = width_class(view.cols);
    let all: &[Col] = match state.tab {
        Tab::Agents => agent_cols(wclass),
        Tab::Tasks => task_cols(wclass),
        _ => tree_cols(wclass),
    };
    let cols = ui::live_cols(state, &window, all);
    let columns = cols.iter().map(|c| Column { title: c.header().to_lowercase(), width: c.width() }).collect();
    let rows = window
        .iter()
        .map(|vr| Row {
            id: vr.id.clone(),
            selected: sel.as_deref() == Some(vr.id.as_str()),
            cells: cols.iter().map(|c| runs(&ui::cell_spans(state, vr, *c), Default::default())).collect(),
        })
        .collect();
    (columns, rows)
}

fn detail(state: &mut TopState, view: &Viewport) -> Option<Detail> {
    let composer = state.composer.clone();
    let d = state.detail.as_mut()?;
    let cols = view.detail_cols.max(10);
    let rows = view.detail_rows.max(6);
    if ui::is_task_detail(d) {
        let (info, lines) = ui::task_parts(d);
        let card = ui::task_card(&info, d, cols, rows);
        let height = rows.saturating_sub(card.len() + 1).max(1);
        let body = ui::task_body(d, &lines, cols.saturating_sub(2).max(1), height);
        let count = ui::task_count(d, &body);
        let (prompt, hints) = ui::task_footer(d, info.running);
        let prompt = if d.log.editing { None } else { prompt.map(|p| runs(&p, Default::default())) };
        return Some(Detail::Task {
            title: d.title.clone(),
            card: card.iter().map(line_runs).collect(),
            tabs: d.tabs.iter().enumerate().map(|(i, t)| (t.clone(), i == d.tab)).collect(),
            count: runs(&count, Default::default()),
            rows: body.rows.iter().map(line_runs).collect(),
            bar: Scrolled { len: body.len, height, scroll: d.scroll },
            editing: d.log.editing,
            query: d.log.query.clone(),
            prompt,
            position: ui::detail_position(d.follow, info.running, body.len, height, d.scroll),
            keys: keys(&hints),
        });
    }
    let live = composer.as_ref().filter(|c| c.child_id == d.id);
    let height = rows.saturating_sub(1).max(1);
    let body = ui::chat_body(d, cols.saturating_sub(2).max(1), height);
    let running = ui::chat_running(d);
    Some(Detail::Chat {
        title: d.title.clone(),
        rows: body.rows.iter().map(line_runs).collect(),
        folds: body.folds.iter().map(|f| f.map(|f| f.id)).collect(),
        bar: Scrolled { len: body.len, height, scroll: d.scroll },
        status: runs(&ui::chat_status(d), Default::default()),
        live: live.map(|c| (c.input.clone(), ui::composer_facts(c))),
        position: ui::detail_position(d.follow, running, body.len, height, d.scroll),
        keys: keys(&ui::chat_keys(d, live.is_some())),
    })
}

/// The frame for `state` at `view`'s size. Like a TUI draw, it updates the
/// table's scroll and the detail's bookkeeping.
pub fn build(state: &mut TopState, view: &Viewport) -> Frame {
    let h = state.header.clone();
    let (columns, rows) = table(state, view);
    let total = state.visible.len();
    let at = state
        .selected
        .as_deref()
        .and_then(|id| state.visible.iter().position(|v| v.id == id))
        .map_or(0, |i| i + 1);
    let preview = (state.preview && state.detail.is_none()).then(|| {
        let mut title = vec![Run { text: "preview".into(), bold: true, ..Default::default() }];
        if let Some(n) = state.selected_node() {
            title.push(Run { text: format!(" · {} · {}", n.id, n.label), fg: Some(0x969cac), ..Default::default() });
        }
        let lines = ui::preview_lines(state, view.preview_cols.max(10), view.preview_rows.max(1));
        (title, lines.iter().map(line_runs).collect())
    });
    let detail = detail(state, view);
    Frame {
        version: h.version.clone(),
        pill: runs(&ui::live_pill(state), Default::default()),
        pid_up: h.pid.map(|pid| {
            let up = h.uptime_ms.map(pi_famulus::fmt::human_duration).unwrap_or_else(|| "-".into());
            format!("pid {pid} · up {up}")
        }),
        messaging: h.messaging.as_ref().map(|m| runs(&ui::messaging_spans(m), Default::default())).unwrap_or_default(),
        overview: ui::overview_rows(state, view.overview_cols.max(20)).iter().map(|r| runs(r, Default::default())).collect(),
        kinds: ui::kinds_lines(state, view.kinds_cols.max(20), view.kinds_rows.max(2)).iter().map(line_runs).collect(),
        tabs: tabs(state),
        scope: state.scope.label().to_lowercase(),
        sort: format!("{} {}", state.sort.label().to_lowercase(), if state.sort_desc { "↓" } else { "↑" }),
        columns,
        rows,
        events: state.tab == Tab::Events,
        position: (at, total),
        filter: state.filter.clone(),
        filter_editing: state.filter_editing,
        message: state.message.clone(),
        confirm_stop: state.confirm_stop.clone(),
        keys: keys(&ui::main_keys(state)),
        preview,
        help: state.help,
        detail,
        quit: false,
    }
}
