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
use crate::top::collector::{open_detail, Tick};
use crate::top::model::{NodeKind, Tab, TopState};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

pub fn wants_interactive(json: bool, once: bool) -> bool {
    !json && !once && crate::sys::stdio_interactive()
}

pub async fn run(home: &Path, interval_ms: u64, session: Option<String>, cwd: Option<String>) -> Result<(), String> {
    let mut state = TopState::new(interval_ms, session, cwd);
    let first = collector::collect(home, &state).await?;
    apply_tick(&mut state, first);

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

    let mut interval = tokio::time::interval(Duration::from_millis(state.interval_ms.max(100)));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.tick().await;

    loop {
        terminal.draw(|f| ui::draw(f, &mut state)).map_err(|e| e.to_string())?;
        tokio::select! {
            _ = interval.tick(), if !state.paused => {
                match collector::collect(home, &state).await {
                    Ok(tick) => apply_tick(&mut state, tick),
                    Err(e) => state.message = Some(e),
                }
            }
            Some(ev) = ev_rx.recv() => {
                let prev = state.interval_ms;
                if handle_event(&mut state, ev, home).await? {
                    break;
                }
                if state.interval_ms != prev {
                    interval = tokio::time::interval(Duration::from_millis(state.interval_ms.max(100)));
                    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
                }
            }
        }
    }
    Ok(())
}

fn apply_tick(state: &mut TopState, tick: Tick) {
    state.apply(&tick.snap, &tick.events, &tick.kind_slots);
    state.preview_lines = tick.preview_lines;
    if let (Some(d), Some(lines)) = (state.detail.as_mut(), tick.detail_lines) {
        d.lines = lines;
        if d.follow {
            d.scroll = d.lines.len().saturating_sub(8);
        }
    }
}

async fn handle_event(state: &mut TopState, ev: Event, home: &Path) -> Result<bool, String> {
    let Event::Key(key) = ev else {
        if let Event::Resize(_, _) = ev {
            return Ok(false);
        }
        return Ok(false);
    };
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
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.confirm_stop = None;
        }
        _ => {}
    }
    Ok(false)
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
        KeyCode::Up | KeyCode::Char('k') => d.scroll = d.scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => d.scroll = d.scroll.saturating_add(1).min(d.lines.len()),
        KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(16),
        KeyCode::PageDown => d.scroll = d.scroll.saturating_add(16).min(d.lines.len()),
        KeyCode::Home | KeyCode::Char('g') => d.scroll = 0,
        KeyCode::End | KeyCode::Char('G') => d.scroll = d.lines.len().saturating_sub(1),
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
        execute!(stdout(), EnterAlternateScreen)?;
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
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}
