//! Interactive `pi-famulus top` TUI.
//!
//! Entered only when stdin and stdout are terminals and neither `--json` nor
//! `--once` was passed. Restoration of the terminal (raw mode, alternate
//! screen) is guaranteed on return, error, and panic.

pub mod collector;
pub mod model;
pub mod ui;

use crate::client::{self, HelloMode};
use crate::proto::{RequestKind, UnitOk};
use crate::top::collector::{open_detail, CollectView, Collector, Tick};
use crate::top::model::{NodeKind, Tab, TopState};
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

pub async fn run(home: &Path, interval_ms: u64, session: Option<String>, cwd: Option<String>) -> Result<(), String> {
    let mut state = TopState::new(interval_ms, session, cwd);
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
        tokio::select! {
            Some(res) = tick_rx.recv() => apply_latest(&mut state, &mut tick_rx, res),
            Some(ev) = ev_rx.recv() => {
                if dispatch_event(&mut state, ev, home, &cmd_tx).await? {
                    break;
                }
            }
        }
        while let Ok(ev) = ev_rx.try_recv() {
            if dispatch_event(&mut state, ev, home, &cmd_tx).await? {
                return Ok(());
            }
        }
        while let Ok(res) = tick_rx.try_recv() {
            apply_latest(&mut state, &mut tick_rx, res);
        }
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
    state.preview_lines = tick.preview_lines;
    if let (Some(d), Some(body)) = (state.detail.as_mut(), tick.detail_body) {
        d.body = body;
    }
}

async fn handle_event(state: &mut TopState, ev: Event, home: &Path) -> Result<bool, String> {
    match ev {
        Event::Key(key) => handle_key(state, key, home).await,
        Event::Mouse(mouse) => {
            handle_mouse(state, mouse);
            Ok(false)
        }
        Event::Resize(_, _) => Ok(false),
        _ => Ok(false),
    }
}

async fn handle_key(state: &mut TopState, key: KeyEvent, home: &Path) -> Result<bool, String> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
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
    handle_main(state, key, home).await
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
    match ev.kind {
        MouseEventKind::ScrollDown => {
            if let Some(d) = state.detail.as_mut() {
                d.follow = false;
                d.scroll = d.scroll.saturating_add(3).min(d.wrap_len.saturating_sub(1));
            } else {
                state.move_sel(1);
            }
        }
        MouseEventKind::ScrollUp => {
            if let Some(d) = state.detail.as_mut() {
                d.follow = false;
                d.scroll = d.scroll.saturating_sub(3);
            } else {
                state.move_sel(-1);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => handle_mouse_click(state, ev.column, ev.row),
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
    if state.detail.is_some() {
        if !state.mouse.detail.contains(col, row) {
            state.detail = None;
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
    let line = &state.mouse.tab_line;
    let x = col.saturating_sub(state.mouse.tabs.x) as usize;
    if let Some(i) = line.find("Scope:") {
        if x >= i {
            state.cycle_scope();
            return;
        }
    }
    if let Some(i) = line.find("Sort:") {
        if x >= i {
            state.cycle_sort(false);
            return;
        }
    }
    let mut hit = Tab::Tree;
    for (digit, tab) in [('1', Tab::Tree), ('2', Tab::Agents), ('3', Tab::Tasks), ('4', Tab::Events)] {
        if let Some(i) = line.find(&format!("{digit}:")) {
            if x >= i {
                hit = tab;
            }
        }
    }
    state.set_tab(hit);
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

fn handle_detail(state: &mut TopState, key: KeyEvent) -> bool {
    let Some(d) = state.detail.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => state.detail = None,
        KeyCode::Char('1') => d.tab = 0,
        KeyCode::Char('2') if d.tabs.len() > 1 => d.tab = 1,
        KeyCode::Char('3') if d.tabs.len() > 2 => d.tab = 2,
        KeyCode::Char('f') | KeyCode::Char('F') => d.follow = !d.follow,
        KeyCode::Up | KeyCode::Char('k') => {
            d.follow = false;
            d.scroll = d.scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            d.follow = false;
            d.scroll = d.scroll.saturating_add(1).min(d.wrap_len.saturating_sub(1));
        }
        KeyCode::PageUp => {
            d.follow = false;
            d.scroll = d.scroll.saturating_sub(16);
        }
        KeyCode::PageDown => {
            d.follow = false;
            d.scroll = d.scroll.saturating_add(16).min(d.wrap_len.saturating_sub(1));
        }
        KeyCode::Home | KeyCode::Char('g') => {
            d.follow = false;
            d.scroll = 0;
        }
        KeyCode::End | KeyCode::Char('G') => d.scroll = d.wrap_len.saturating_sub(1),
        _ => {}
    }
    false
}

async fn handle_main(state: &mut TopState, key: KeyEvent, home: &Path) -> Result<bool, String> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        return Ok(true);
    }
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
        KeyCode::Char('v') => {
            if crate::top::model::height_class(crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24))
                != crate::top::model::HeightClass::Short
            {
                state.preview = !state.preview;
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
    let _ = home;
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
            state.message = Some("agents run inside pi; stop from /tasks or ask the agent".into());
        }
        Some(n) if n.kind == NodeKind::Task => {
            state.message = Some(format!("{} is not running", n.id));
        }
        _ => state.message = Some("select a running sh_/mon_ task to stop".into()),
    }
}

async fn stop_task(home: &Path, task_id: &str) -> Result<(), String> {
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
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
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
