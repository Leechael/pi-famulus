//! Interactive `pi-famulus top` TUI.
//!
//! Entered only when stdin and stdout are terminals and neither `--json` nor
//! `--once` was passed. Restoration of the terminal (raw mode, alternate
//! screen) is guaranteed on return, error, and panic.

pub mod collector;
pub mod live;
pub mod model;
pub mod ui;

use crate::client::{self, HelloMode};
use crate::proto::{RequestKind, UnitOk};
use crate::top::collector::{open_detail, CollectView, Collector, Tick};
use crate::top::live::{LiveCmd, LiveMsg};
use crate::top::model::{AgentComposer, Chat, Detail, DetailBody, NodeKind, Tab, TabHit, TopState};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, stdout};
use std::panic;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

enum CollectorCmd {
    View(CollectView),
    SetInterval(u64),
    Pause(bool),
    Refresh,
    Tails,
}

pub fn wants_interactive(json: bool, once: bool) -> bool {
    !json && !once && crate::sys::stdio_interactive()
}

pub async fn run(
    home: &Path,
    interval_ms: u64,
    session: Option<String>,
    cwd: Option<String>,
    focus: Option<String>,
) -> Result<(), String> {
    let mut state = TopState::new(interval_ms, session, cwd);
    state.focus = focus;
    state.message = Some("loading…".into());

    let _guard = TerminalGuard::enter().map_err(|e| e.to_string())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout())).map_err(|e| e.to_string())?;

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if ev_tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    let (live_tx, mut live_rx) = mpsc::unbounded_channel::<LiveMsg>();
    let mut live_slot: Option<(String, mpsc::UnboundedSender<LiveCmd>)> = None;
    let (tick_tx, mut tick_rx) = mpsc::channel::<Result<Tick, String>>(1);
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<CollectorCmd>();
    let home_buf = home.to_path_buf();
    let start_interval = state.interval_ms.max(100);
    let start_view = CollectView::from_state(&state);
    tokio::spawn(async move {
        let mut collector = Collector::new(home_buf);
        let mut interval = tokio::time::interval(Duration::from_millis(start_interval));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut paused = false;
        let mut view = start_view;
        loop {
            tokio::select! {
                _ = interval.tick(), if !paused => {
                    let _ = tick_tx.send(collector.tick(&view).await).await;
                    if collector.disk_stale() {
                        collector.refresh_disk().await;
                        let _ = tick_tx.send(collector.tick(&view).await).await;
                    }
                }
                Some(cmd) = cmd_rx.recv() => match cmd {
                    CollectorCmd::View(v) => view = v,
                    CollectorCmd::SetInterval(ms) => {
                        interval = tokio::time::interval(Duration::from_millis(ms.max(100)));
                        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    }
                    CollectorCmd::Pause(p) => paused = p,
                    CollectorCmd::Refresh => {
                        let _ = tick_tx.send(collector.tick(&view).await).await;
                    }
                    CollectorCmd::Tails => {
                        let _ = tick_tx.send(Ok(collector.tails(&view))).await;
                    }
                }
            }
        }
    });

    loop {
        terminal.draw(|f| ui::draw(f, &mut state)).map_err(|e| e.to_string())?;
        let anim = state.wants_anim();
        tokio::select! {
            Some(res) = tick_rx.recv() => apply_latest(&mut state, &mut tick_rx, res),
            Some(ev) = ev_rx.recv() => {
                if dispatch_event(&mut state, ev, home, &cmd_tx).await? {
                    break;
                }
            }
            Some(msg) = live_rx.recv() => apply_live(&mut state, msg),
            _ = tokio::time::sleep(Duration::from_millis(80)), if anim => {}
        }
        while let Ok(ev) = ev_rx.try_recv() {
            if dispatch_event(&mut state, ev, home, &cmd_tx).await? {
                return Ok(());
            }
        }
        while let Ok(res) = tick_rx.try_recv() {
            apply_latest(&mut state, &mut tick_rx, res);
        }
        while let Ok(msg) = live_rx.try_recv() {
            apply_live(&mut state, msg);
        }
        sync_live(&mut state, home, &mut live_slot, &live_tx);
        for (op, text) in std::mem::take(&mut state.pending_live) {
            if let Some((_, tx)) = &live_slot {
                let _ = tx.send(LiveCmd::Send { op, text });
            }
        }
    }
    if let Some((_, tx)) = live_slot {
        let _ = tx.send(LiveCmd::Stop);
    }
    Ok(())
}

fn apply_latest(state: &mut TopState, tick_rx: &mut mpsc::Receiver<Result<Tick, String>>, first: Result<Tick, String>) {
    let mut last_full = None;
    let mut last_tails = None;
    let mut tails_after_full = false;
    let mut err = None;
    let mut take = |res: Result<Tick, String>| match res {
        Err(e) => err = Some(e),
        Ok(t) if t.snap.is_some() => {
            last_full = Some(t);
            tails_after_full = false;
        }
        Ok(t) => {
            last_tails = Some(t);
            tails_after_full = true;
        }
    };
    take(first);
    while let Ok(next) = tick_rx.try_recv() {
        take(next);
    }
    if let Some(e) = err {
        state.message = Some(e);
    }
    if let Some(t) = last_full {
        apply_tick(state, t);
    }
    if tails_after_full {
        if let Some(t) = last_tails {
            apply_tick(state, t);
        }
    }
}

async fn dispatch_event(
    state: &mut TopState,
    ev: Event,
    home: &Path,
    cmd_tx: &mpsc::UnboundedSender<CollectorCmd>,
) -> Result<bool, String> {
    let prev_interval = state.interval_ms;
    let prev_paused = state.paused;
    let prev_view = CollectView::from_state(state);
    if handle_event(state, ev, home).await? {
        return Ok(true);
    }
    if state.interval_ms != prev_interval {
        let _ = cmd_tx.send(CollectorCmd::SetInterval(state.interval_ms));
    }
    if state.paused != prev_paused {
        let _ = cmd_tx.send(CollectorCmd::Pause(state.paused));
    }
    let next_view = CollectView::from_state(state);
    if next_view != prev_view {
        let _ = cmd_tx.send(CollectorCmd::View(next_view));
        if state.preview || state.detail.is_some() {
            let _ = cmd_tx.send(CollectorCmd::Tails);
        }
    }
    if state.want_refresh {
        state.want_refresh = false;
        let _ = cmd_tx.send(CollectorCmd::Refresh);
    }
    Ok(false)
}

fn apply_tick(state: &mut TopState, tick: Tick) {
    if let Some(snap) = &tick.snap {
        state.apply(snap, &tick.events, &tick.kind_slots);
        if state.message.as_deref() == Some("loading…") {
            state.message = None;
        }
    }
    state.preview_body = tick.preview_body;
    let body = tick.detail_body;
    let live = state.composer.as_ref().map(|c| c.connected).unwrap_or(false);
    if !live {
        if let Some(body) = body {
            if let Some(d) = state.detail.as_mut() {
                d.body = body;
            }
        }
    }
    let focus = state.focus.is_some() && state.detail.is_none();
    if focus {
        open_focused(state);
    }
}

fn open_focused(state: &mut TopState) {
    let Some(id) = state.focus.clone() else { return };
    state.focus = None;
    if state.nodes.contains_key(&id) {
        state.selected = Some(id.clone());
    }
    if let Some(d) = open_detail(state) {
        if d.id == id {
            state.detail = Some(d);
            return;
        }
    }
    state.detail = Some(Detail {
        id: id.clone(),
        tab: 0,
        follow: true,
        hold: false,
        scroll: 0,
        title: id,
        tabs: Vec::new(),
        body: DetailBody::Conversation(Chat {
            turns: Vec::new(),
            agent: String::new(),
            stats: Vec::new(),
            note: None,
            running: false,
        }),
        wrap_len: 0,
        view_h: 0,
        tools_expanded: false,
        open: std::collections::HashSet::new(),
        hits: Vec::new(),
        log: crate::top::model::LogView::default(),
        fold_count: 0,
    });
}

fn sync_live(
    state: &mut TopState,
    home: &Path,
    slot: &mut Option<(String, mpsc::UnboundedSender<LiveCmd>)>,
    live_tx: &mpsc::UnboundedSender<LiveMsg>,
) {
    let want = state.detail.as_ref().filter(|d| d.tabs.is_empty()).map(|d| d.id.clone());
    match (want, slot.as_ref().map(|(id, _)| id.clone())) {
        (Some(id), Some(cur)) if cur == id => {}
        (Some(id), _) => {
            if let Some((_, tx)) = slot.take() {
                let _ = tx.send(LiveCmd::Stop);
            }
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
            let tx = live_tx.clone();
            let home = home.to_path_buf();
            let watch_id = id.clone();
            tokio::spawn(async move { live::run(home, watch_id, tx, cmd_rx).await });
            *slot = Some((id.clone(), cmd_tx));
            if state.composer.as_ref().map(|c| c.child_id.as_str()) != Some(id.as_str()) {
                state.composer = Some(AgentComposer::new(id));
            }
        }
        (None, Some(_)) => {
            if let Some((_, tx)) = slot.take() {
                let _ = tx.send(LiveCmd::Stop);
            }
            state.composer = None;
        }
        (None, None) => {}
    }
}

fn apply_live(state: &mut TopState, msg: LiveMsg) {
    let Some(composer) = state.composer.as_mut() else { return };
    let Some(detail) = state.detail.as_mut() else { return };
    if detail.id != composer.child_id {
        return;
    }
    match msg {
        LiveMsg::Error(text) => state.message = Some(text),
        LiveMsg::Restored(text) => {
            if !text.is_empty() {
                composer.input = text;
            }
            composer.esc_armed = false;
            state.message = Some("aborted; queued text is back in the input".into());
        }
        LiveMsg::Turns(turns) => {
            composer.connected = true;
            composer.draft = false;
            conversation(detail).turns = turns;
            detail.follow = true;
        }
        LiveMsg::Delta(delta) => {
            composer.connected = true;
            let chat = conversation(detail);
            if composer.draft {
                if let Some(last) = chat.turns.last_mut() {
                    last.text.push_str(&delta);
                }
            } else {
                composer.draft = true;
                chat.turns.push(crate::inspect::TranscriptTurn {
                    role: "assistant".into(),
                    tool: None,
                    args: None,
                    text: delta,
                    ts: None,
                    error: false,
                });
            }
            chat.running = true;
        }
        LiveMsg::Assistant(text) => {
            composer.connected = true;
            let chat = conversation(detail);
            if composer.draft {
                if let Some(last) = chat.turns.last_mut() {
                    last.text = text;
                }
                composer.draft = false;
            } else {
                chat.turns.push(crate::inspect::TranscriptTurn {
                    role: "assistant".into(),
                    tool: None,
                    args: None,
                    text,
                    ts: None,
                    error: false,
                });
            }
        }
        LiveMsg::User(text) => {
            composer.connected = true;
            composer.draft = false;
            conversation(detail).turns.push(crate::inspect::TranscriptTurn {
                role: "user".into(),
                tool: None,
                args: None,
                text,
                ts: None,
                error: false,
            });
        }
        LiveMsg::State { activity, residency, cache } => {
            composer.connected = true;
            if !activity.is_empty() {
                composer.activity = activity;
            }
            if !residency.is_empty() {
                composer.residency = residency;
            }
            if !cache.is_empty() {
                composer.cache = cache;
            }
            if let DetailBody::Conversation(chat) = &mut detail.body {
                chat.running = composer.activity == "running";
            }
        }
        LiveMsg::Meta { model, context } => {
            if !model.is_empty() {
                composer.model = model;
            }
            if !context.is_empty() {
                composer.context = context;
            }
        }
    }
}

fn conversation(detail: &mut Detail) -> &mut Chat {
    if !matches!(detail.body, DetailBody::Conversation(_)) {
        detail.body = DetailBody::Conversation(Chat {
            turns: Vec::new(),
            agent: String::new(),
            stats: Vec::new(),
            note: None,
            running: false,
        });
    }
    match &mut detail.body {
        DetailBody::Conversation(chat) => chat,
        _ => unreachable!(),
    }
}

async fn handle_event(state: &mut TopState, ev: Event, home: &Path) -> Result<bool, String> {
    match ev {
        Event::Key(key) => handle_key(state, key, home).await,
        Event::Mouse(mouse) => {
            handle_mouse(state, mouse);
            Ok(false)
        }
        _ => Ok(false),
    }
}

async fn handle_key(state: &mut TopState, key: KeyEvent, home: &Path) -> Result<bool, String> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }
    // Ctrl+C quits from anywhere, overlays and prompts included.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        return Ok(true);
    }
    if state.confirm_stop.is_some() {
        return handle_confirm(state, key, home).await;
    }
    if state.filter_editing {
        return Ok(handle_filter(state, key));
    }
    if state.help {
        if matches!(key.code, KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc) {
            state.help = false;
        }
        return Ok(false);
    }
    if state.detail.is_some() {
        return Ok(handle_detail(state, key));
    }
    handle_main(state, key).await
}

async fn handle_confirm(state: &mut TopState, key: KeyEvent, home: &Path) -> Result<bool, String> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let id = state.confirm_stop.take().unwrap();
            match stop_task(home, &id).await {
                Ok(()) => state.message = Some(format!("stopped {id}")),
                Err(e) => state.message = Some(e),
            }
            state.want_refresh = true;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.confirm_stop = None;
        }
        _ => {}
    }
    Ok(false)
}

fn handle_mouse(state: &mut TopState, ev: MouseEvent) {
    // The wheel never moves the table under the help or stop prompt.
    let blocked = state.help || state.confirm_stop.is_some();
    match ev.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if blocked => {}
        MouseEventKind::ScrollDown => {
            if let Some(d) = state.detail.as_mut() {
                detail_scroll_down(d, 3);
            } else {
                state.move_sel(1);
            }
        }
        MouseEventKind::ScrollUp => {
            if let Some(d) = state.detail.as_mut() {
                detail_scroll_up(d, 3);
            } else {
                state.move_sel(-1);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => handle_mouse_click(state, ev.column, ev.row),
        MouseEventKind::Drag(MouseButton::Left) => {
            if state.mouse.dragging_bar {
                drag_scrollbar(state, ev.row);
            }
        }
        MouseEventKind::Up(_) => state.mouse.dragging_bar = false,
        _ => {}
    }
}

fn handle_mouse_click(state: &mut TopState, col: u16, row: u16) {
    if state.confirm_stop.is_some() {
        return;
    }
    if state.help {
        state.help = false;
        return;
    }
    if let Some(d) = state.detail.as_mut() {
        if row == d.log.tab_y && !d.log.tab_hits.is_empty() {
            if let Some(&(_, _, tab)) = d.log.tab_hits.iter().find(|(x0, x1, _)| col >= *x0 && col < *x1) {
                set_detail_tab(d, tab);
                return;
            }
        }
        if state.mouse.scrollbar.contains(col, row) {
            state.mouse.dragging_bar = true;
            drag_scrollbar(state, row);
            return;
        }
        if state.mouse.chat.contains(col, row) {
            toggle_fold_at(state, col, row);
            return;
        }
        return;
    }
    if state.mouse.tabs.contains(col, row) {
        click_tab_bar(state, col);
        return;
    }
    if !state.mouse.table.contains(col, row) {
        return;
    }
    let local = row.saturating_sub(state.mouse.table.y);
    if local == 0 {
        state.cycle_sort(false);
        return;
    }
    let i = state.scroll.saturating_add(local as usize - 1);
    if i >= state.visible.len() {
        return;
    }
    let id = state.visible[i].id.clone();
    let now = std::time::Instant::now();
    let dbl = state.mouse.last.as_ref().is_some_and(|(prev, at)| prev == &id && now.duration_since(*at) < Duration::from_millis(400));
    state.mouse.last = Some((id.clone(), now));
    if dbl {
        state.selected = Some(id);
        if let Some(d) = open_detail(state) {
            state.detail = Some(d);
        }
        return;
    }
    if state.selected.as_deref() == Some(id.as_str()) {
        if state.tab == Tab::Tree {
            if state.visible.get(i).is_some_and(|r| r.expanded) {
                state.fold();
            } else {
                state.unfold();
            }
        } else if let Some(d) = open_detail(state) {
            state.detail = Some(d);
        }
        return;
    }
    state.selected = Some(id);
}

fn click_tab_bar(state: &mut TopState, col: u16) {
    let x = col.saturating_sub(state.mouse.tabs.x);
    let hit = state.mouse.tab_hits.iter().find(|(x0, x1, _)| x >= *x0 && x < *x1).map(|h| h.2);
    match hit {
        Some(TabHit::Tab(tab)) => state.set_tab(tab),
        Some(TabHit::Scope) => state.cycle_scope(),
        Some(TabHit::Sort) => state.cycle_sort(false),
        None => {}
    }
}

fn handle_filter(state: &mut TopState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            state.filter_editing = false;
            state.set_filter(String::new());
        }
        KeyCode::Enter => state.filter_editing = false,
        KeyCode::Backspace => {
            state.filter.pop();
            let f = state.filter.clone();
            state.set_filter(f);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.filter.push(c);
            let f = state.filter.clone();
            state.set_filter(f);
        }
        _ => {}
    }
    false
}

fn toggle_fold_at(state: &mut TopState, col: u16, row: u16) {
    let Some(d) = state.detail.as_mut() else {
        return;
    };
    let i = row.saturating_sub(state.mouse.chat.y) as usize;
    let Some(hit) = d.hits.get(i).copied().flatten() else {
        return;
    };
    let x = col.saturating_sub(state.mouse.chat.x);
    if x < hit.x0 || x >= hit.x1 {
        return;
    }
    // Leaving "unfold all" by clicking one block folds only that block.
    if d.tools_expanded {
        d.tools_expanded = false;
        d.open = (0..d.fold_count).collect();
    }
    if !d.open.remove(&hit.id) {
        d.open.insert(hit.id);
    }
}

fn drag_scrollbar(state: &mut TopState, row: u16) {
    let Some(d) = state.detail.as_mut() else {
        return;
    };
    let bar = state.mouse.scrollbar;
    let max = d.wrap_len.saturating_sub(d.view_h);
    let h = bar.h.max(1) as usize;
    let y = (row.saturating_sub(bar.y) as usize).min(h.saturating_sub(1));
    d.follow = false;
    d.hold = false;
    d.scroll = if h <= 1 { 0 } else { y * max / (h - 1) };
}

fn detail_scroll_up(d: &mut Detail, n: usize) {
    d.follow = false;
    d.hold = false;
    d.scroll = d.scroll.saturating_sub(n);
}

fn detail_scroll_down(d: &mut Detail, n: usize) {
    if d.follow {
        return;
    }
    d.hold = false;
    d.scroll = d.scroll.saturating_add(n);
}

/// Switch the detail tab and start the new stream at its live end.
fn set_detail_tab(d: &mut Detail, tab: usize) {
    if tab >= d.tabs.len().max(1) || tab == d.tab {
        return;
    }
    d.tab = tab;
    d.scroll = 0;
    d.follow = true;
    d.log.matches.clear();
    d.log.current = 0;
}

/// `n` / `N`: move to the next / previous painted row holding a match.
fn jump_match(d: &mut Detail, forward: bool) {
    let n = d.log.matches.len();
    if n == 0 {
        return;
    }
    d.log.current = if forward { (d.log.current + 1) % n } else { (d.log.current + n - 1) % n };
    d.follow = false;
    d.scroll = d.log.matches[d.log.current].saturating_sub(d.view_h / 3);
}

/// Typing into a task output's search prompt.
fn handle_search(d: &mut Detail, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            d.log.query.clear();
            d.log.editing = false;
            d.log.matches.clear();
            d.log.current = 0;
        }
        KeyCode::Enter => d.log.editing = false,
        KeyCode::Tab => d.log.filter = !d.log.filter,
        KeyCode::Backspace => {
            d.log.query.pop();
            d.log.seek = true;
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            d.log.query.push(c);
            d.log.seek = true;
        }
        _ => {}
    }
}

fn handle_detail(state: &mut TopState, key: KeyEvent) -> bool {
    let live = {
        let Some(d) = state.detail.as_ref() else { return false };
        state.composer.as_ref().is_some_and(|c| c.child_id == d.id) && d.tabs.is_empty()
    };
    if live {
        return handle_live_keys(state, key);
    }
    let Some(d) = state.detail.as_mut() else {
        return false;
    };
    let log = matches!(d.body, DetailBody::Log { .. });
    if log && d.log.editing {
        handle_search(d, key);
        return false;
    }
    match key.code {
        KeyCode::Esc if log && !d.log.query.is_empty() => {
            d.log.query.clear();
            d.log.matches.clear();
        }
        KeyCode::Esc | KeyCode::Char('q') => state.detail = None,
        KeyCode::Char('1') => set_detail_tab(d, 0),
        KeyCode::Char('2') => set_detail_tab(d, 1),
        KeyCode::Char('/') if log => {
            d.log.editing = true;
            d.log.seek = true;
        }
        KeyCode::Char('&') if log => d.log.filter = !d.log.filter,
        KeyCode::Char('n') if log => jump_match(d, true),
        KeyCode::Char('N') if log => jump_match(d, false),
        KeyCode::Char('b') if log => d.log.show_blank = !d.log.show_blank,
        KeyCode::Char('f') | KeyCode::Char('F') => {
            d.follow = !d.follow;
            d.hold = !d.follow;
        }
        KeyCode::Char('o') | KeyCode::Char('O') => d.tools_expanded = !d.tools_expanded,
        KeyCode::Up | KeyCode::Char('k') => detail_scroll_up(d, 1),
        KeyCode::Down | KeyCode::Char('j') => detail_scroll_down(d, 1),
        KeyCode::PageUp => detail_scroll_up(d, 16),
        KeyCode::PageDown => detail_scroll_down(d, 16),
        KeyCode::Home | KeyCode::Char('g') => {
            d.follow = false;
            d.scroll = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            d.follow = true;
            d.hold = false;
        }
        _ => {}
    }
    false
}

/// Agent detail: letters go to the input line. Arrows still scroll.
/// Enter sends (steered while the child is running). Alt-Enter queues a
/// follow-up. Esc twice clears the queue, aborts, and puts that text back.
fn handle_live_keys(state: &mut TopState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up => {
            if let Some(d) = state.detail.as_mut() { detail_scroll_up(d, 1); }
            return false;
        }
        KeyCode::Down => {
            if let Some(d) = state.detail.as_mut() { detail_scroll_down(d, 1); }
            return false;
        }
        KeyCode::PageUp => {
            if let Some(d) = state.detail.as_mut() { detail_scroll_up(d, 16); }
            return false;
        }
        KeyCode::PageDown => {
            if let Some(d) = state.detail.as_mut() { detail_scroll_down(d, 16); }
            return false;
        }
        KeyCode::Char('q')
            if state.composer.as_ref().is_some_and(|c| c.input.is_empty())
                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            state.detail = None;
            return false;
        }
        _ => {}
    }
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let Some(composer) = state.composer.as_mut() else { return false };
    match key.code {
        KeyCode::Esc => {
            if composer.esc_armed {
                composer.esc_armed = false;
                state.pending_live.push(("abort".into(), String::new()));
                state.message = Some("aborting…".into());
            } else {
                composer.esc_armed = true;
                state.message = Some("esc again aborts this turn".into());
            }
        }
        KeyCode::Enter if alt => {
            composer.esc_armed = false;
            let text = std::mem::take(&mut composer.input);
            if !text.is_empty() {
                state.pending_live.push(("prompt_follow".into(), text));
            }
        }
        KeyCode::Enter => {
            composer.esc_armed = false;
            let text = std::mem::take(&mut composer.input);
            if !text.is_empty() {
                let op = if composer.activity == "running" { "prompt_steer" } else { "prompt" };
                state.pending_live.push((op.into(), text));
            }
        }
        KeyCode::Backspace => {
            composer.esc_armed = false;
            composer.input.pop();
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            composer.esc_armed = false;
            composer.input.push(c);
        }
        _ => {}
    }
    false
}

async fn handle_main(state: &mut TopState, key: KeyEvent) -> Result<bool, String> {
    // A status message lasts until the next key; the key may set a new one.
    state.message = None;
    if (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X')))
        || key.code == KeyCode::F(9)
    {
        request_stop(state);
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') | KeyCode::Char('4') => {
            if let KeyCode::Char(c) = key.code {
                if let Some(tab) = Tab::from_digit(c) {
                    state.set_tab(tab);
                }
            }
        }
        KeyCode::Tab => state.set_tab(state.tab.next()),
        KeyCode::BackTab => state.set_tab(state.tab.prev()),
        KeyCode::Up | KeyCode::Char('k') => state.move_sel(-1),
        KeyCode::Down | KeyCode::Char('j') => state.move_sel(1),
        KeyCode::Left | KeyCode::Char('h') => state.fold(),
        KeyCode::Right | KeyCode::Char('l') => state.unfold(),
        KeyCode::PageUp => state.page(-1, 16),
        KeyCode::PageDown => state.page(1, 16),
        KeyCode::Home | KeyCode::Char('g') => state.jump_home(),
        KeyCode::End | KeyCode::Char('G') => state.jump_end(),
        KeyCode::Char('z') => state.toggle_expand_all(),
        KeyCode::Char('a') => state.cycle_scope(),
        KeyCode::Char('s') | KeyCode::Char('>') => state.cycle_sort(false),
        KeyCode::Char('S') | KeyCode::Char('<') => state.cycle_sort(true),
        KeyCode::Char('I') => state.invert_sort(),
        KeyCode::Char('/') => {
            state.filter_editing = true;
            state.message = None;
        }
        KeyCode::Esc if !state.filter.is_empty() => state.set_filter(String::new()),
        KeyCode::Esc if state.events_id.is_some() => {
            state.events_id = None;
            state.recompute_visible();
        }
        KeyCode::Char('v') => {
            if crate::top::model::height_class(crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24))
                != crate::top::model::HeightClass::Short
            {
                state.preview = !state.preview;
            } else {
                state.message = Some("preview needs a taller terminal (20+ rows); Enter opens the detail".into());
            }
        }
        KeyCode::Enter => {
            if let Some(d) = open_detail(state) {
                state.detail = Some(d);
            }
        }
        KeyCode::Char('e') => state.jump_events_for_selected(),
        KeyCode::Char('x') => request_stop(state),
        KeyCode::Char('p') | KeyCode::Char(' ') => state.paused = !state.paused,
        KeyCode::Char('+') | KeyCode::Char('=') => state.bump_interval(true),
        KeyCode::Char('-') => state.bump_interval(false),
        KeyCode::Char('?') => state.help = true,
        _ => {}
    }
    Ok(false)
}

fn request_stop(state: &mut TopState) {
    if let Some(n) = state.selected_is_stoppable_task() {
        state.confirm_stop = Some(n.id.clone());
        state.message = None;
        return;
    }
    match state.selected_node() {
        Some(n) if n.kind == NodeKind::Agent => {
            state.message = Some("Enter opens this subagent; esc twice there aborts its turn".into());
        }
        Some(n) if n.kind == NodeKind::Task => {
            state.message = Some(format!("{} is not running", n.id));
        }
        _ => state.message = Some("select a running sh_/mon_ task to stop".into()),
    }
}

/// The stop runs on the UI loop, so a daemon that never answers must not
/// freeze keys and drawing: give up after a few seconds.
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn stop_task(home: &Path, task_id: &str) -> Result<(), String> {
    let stop = async {
        let mut conn = client::connect_existing(home, &HelloMode::Cli)
            .await
            .map_err(|_| "pi-famulus is not running".to_string())?;
        let _: UnitOk = conn
            .roundtrip(RequestKind::Stop {
                task_id: task_id.to_string(),
                reason: Some("tui".into()),
            })
            .await?;
        Ok(())
    };
    tokio::time::timeout(STOP_TIMEOUT, stop)
        .await
        .unwrap_or_else(|_| Err(format!("stop {task_id}: pi-famulus did not answer")))
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // No guard exists yet to undo raw mode, so undo it here on failure.
        if let Err(e) = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture) {
            let _ = restore();
            return Err(e);
        }
        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = restore();
            prev(info);
        }));
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore();
    }
}

fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::top::model::{LogView, TaskInfo};
    use std::collections::HashSet;

    fn log_detail() -> Detail {
        Detail {
            id: "sh_1".into(),
            tab: 0,
            follow: true,
            hold: false,
            scroll: 0,
            title: "sh_1".into(),
            tabs: vec!["output".into(), "stderr".into()],
            body: DetailBody::Log { info: Box::new(TaskInfo::default()), lines: vec![] },
            wrap_len: 100,
            view_h: 9,
            tools_expanded: false,
            open: HashSet::new(),
            hits: Vec::new(),
            log: LogView::default(),
            fold_count: 0,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn agent_detail() -> Detail {
        Detail {
            id: "ch_1".into(),
            tab: 0,
            follow: true,
            hold: false,
            scroll: 0,
            title: "ch_1".into(),
            tabs: Vec::new(),
            body: DetailBody::Conversation(crate::top::model::Chat {
                turns: Vec::new(),
                agent: "worker".into(),
                stats: Vec::new(),
                note: None,
                running: false,
            }),
            wrap_len: 0,
            view_h: 0,
            tools_expanded: false,
            open: HashSet::new(),
            hits: Vec::new(),
            log: LogView::default(),
            fold_count: 0,
        }
    }

    #[test]
    fn live_agent_detail_types_sends_and_aborts_on_a_second_esc() {
        let mut state = TopState::new(1_000, None, None);
        state.detail = Some(agent_detail());
        state.composer = Some(crate::top::model::AgentComposer::new("ch_1".into()));
        handle_detail(&mut state, key(KeyCode::Char('h')));
        handle_detail(&mut state, key(KeyCode::Char('i')));
        handle_detail(&mut state, key(KeyCode::Enter));
        assert_eq!(state.pending_live, vec![("prompt".into(), "hi".into())]);
        state.composer.as_mut().unwrap().activity = "running".into();
        state.composer.as_mut().unwrap().input = "more".into();
        handle_detail(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(state.pending_live[1], ("prompt_follow".into(), "more".into()));
        handle_detail(&mut state, key(KeyCode::Esc));
        assert!(state.composer.as_ref().unwrap().esc_armed);
        handle_detail(&mut state, key(KeyCode::Esc));
        assert_eq!(state.pending_live[2].0, "abort");
        assert!(state.detail.is_some(), "abort stays on the conversation");
        state.composer.as_mut().unwrap().input.clear();
        handle_detail(&mut state, key(KeyCode::Char('q')));
        assert!(state.detail.is_none());
    }

    #[test]
    fn slash_types_a_query_tab_flips_mode_and_n_walks_matches() {
        let mut state = TopState::new(1_000, None, None);
        state.detail = Some(log_detail());
        for c in ['/', 'e', 'r', 'r'] {
            handle_detail(&mut state, key(KeyCode::Char(c)));
        }
        let d = state.detail.as_mut().unwrap();
        assert!(d.log.editing && d.log.seek);
        assert_eq!(d.log.query, "err");
        handle_search(d, key(KeyCode::Tab));
        assert!(d.log.filter, "tab flips to filter while typing");
        handle_search(d, key(KeyCode::Enter));
        assert!(!d.log.editing);

        d.log.matches = vec![10, 30, 50];
        handle_detail(&mut state, key(KeyCode::Char('n')));
        let d = state.detail.as_ref().unwrap();
        assert_eq!((d.log.current, d.scroll, d.follow), (1, 27, false));
        handle_detail(&mut state, key(KeyCode::Char('N')));
        handle_detail(&mut state, key(KeyCode::Char('N')));
        assert_eq!(state.detail.as_ref().unwrap().log.current, 2, "N wraps backwards");

        handle_detail(&mut state, key(KeyCode::Esc));
        assert!(state.detail.as_ref().is_some_and(|d| d.log.query.is_empty()), "first esc clears the search");
        handle_detail(&mut state, key(KeyCode::Esc));
        assert!(state.detail.is_none(), "second esc closes");
    }

    #[tokio::test]
    async fn ctrl_c_quits_from_a_detail_and_messages_clear_on_the_next_key() {
        let home = std::env::temp_dir();
        let mut state = TopState::new(1_000, None, None);
        state.detail = Some(log_detail());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(handle_key(&mut state, ctrl_c, &home).await.unwrap());
        state.detail = None;
        state.message = Some("stopped sh_x".into());
        assert!(!handle_key(&mut state, key(KeyCode::Down), &home).await.unwrap());
        assert_eq!(state.message, None);
    }

    #[test]
    fn number_keys_and_tab_clicks_switch_streams() {
        let mut state = TopState::new(1_000, None, None);
        let mut d = log_detail();
        d.follow = false;
        d.scroll = 40;
        d.log.tab_y = 5;
        d.log.tab_hits = vec![(2, 12, 0), (12, 22, 1)];
        state.detail = Some(d);
        handle_mouse_click(&mut state, 15, 5);
        let d = state.detail.as_ref().unwrap();
        assert_eq!((d.tab, d.scroll, d.follow), (1, 0, true), "a new stream starts at its live end");
        handle_detail(&mut state, key(KeyCode::Char('1')));
        assert_eq!(state.detail.as_ref().unwrap().tab, 0);
        handle_detail(&mut state, key(KeyCode::Char('3')));
        assert_eq!(state.detail.as_ref().unwrap().tab, 0, "no third tab");
    }
}
