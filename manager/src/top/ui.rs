//! Ratatui drawing for interactive `top`. Pure function of [`TopState`] and
//! the frame `Rect`; no I/O.

use crate::fmt;
use crate::inspect::TranscriptTurn;
use crate::top::model::{
    self, agent_cols, compact_num, cpu_cell, header_height, height_class, now_cell, task_cols, tok_io_cell,
    tree_cols, wall_cell, width_class, Chat, Col, DetailBody, FoldHit, HeightClass, HitRect, Node, NodeKind, Tab,
    TopState, VisibleRow, WidthClass,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use ratatui::Frame;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Instant;

pub fn draw(frame: &mut Frame, state: &mut TopState) {
    let area = frame.area();
    let hclass = height_class(area.height);
    let wclass = width_class(area.width);
    let head_h = header_height(hclass);
    let preview_on = state.preview && hclass != HeightClass::Short && state.detail.is_none();
    let preview_h = if preview_on {
        let rest = area.height.saturating_sub(head_h + 2);
        ((rest as u32 * if hclass == HeightClass::Tall { 35 } else { 30 } / 100) as u16).max(3)
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(head_h),
            Constraint::Min(3),
            Constraint::Length(preview_h),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], state, hclass, wclass);
    draw_workspace(frame, chunks[1], state, wclass);
    if preview_h > 0 {
        draw_preview(frame, chunks[2], state);
    }
    draw_footer(frame, chunks[3], state);

    if state.help {
        draw_help(frame, area);
    }
    if let Some(d) = state.detail.as_mut() {
        draw_detail(frame, area, d, &mut state.mouse);
    } else {
        state.mouse.detail = HitRect::default();
        state.mouse.chat = HitRect::default();
        state.mouse.scrollbar = HitRect::default();
        state.mouse.dragging_bar = false;
    }
}

fn draw_header(frame: &mut Frame, area: Rect, state: &TopState, hclass: HeightClass, _wclass: WidthClass) {
    let h = &state.header;
    let live = if state.paused {
        "[PAUSED]".to_string()
    } else if h.live {
        format!("[LIVE {:.1}s]", state.interval_ms as f64 / 1000.0)
    } else {
        "[OFFLINE · DISK SNAPSHOT]".into()
    };
    let title = format!(
        " pi-famulus {}{} ── Sessions: {} conn / {} tot ─── {live} ",
        h.version,
        h.pid.map(|p| format!(" (pid {p} · up {})", h.uptime_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into()))).unwrap_or_default(),
        h.sessions_conn,
        h.sessions_tot,
    );
    if hclass == HeightClass::Short {
        let kinds: String = h
            .work_kinds
            .iter()
            .take(4)
            .map(|k| format!("{} {:.0}%", k.kind, k.cpu_now.unwrap_or(0.0)))
            .collect::<Vec<_>>()
            .join("  ");
        let line2 = format!(
            " Slots {}/{} · run {} · CPU {} · tok {}/{} · {kinds}",
            h.agents_used,
            h.agents_total,
            h.tasks_run,
            now_cell(h.cpu_now, false),
            compact_num(h.tokens_in.unwrap_or(0)),
            compact_num(h.tokens_out.unwrap_or(0)),
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(clip(&title, area.width), Style::default().add_modifier(Modifier::BOLD))),
                Line::from(clip(&line2, area.width)),
            ]),
            area,
        );
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);
    let bar = slot_bar(h.agents_used, h.agents_total, 8);
    let left = vec![
        Line::from(Span::styled(clip(&title, cols[0].width), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(clip(&format!(" Slots  [{bar}] {}/{} agents", h.agents_used, h.agents_total), cols[0].width)),
        Line::from(clip(
            &format!(" Tasks  {} run · {} sh · {} ag", h.tasks_run, h.tasks_sh, h.tasks_ag),
            cols[0].width,
        )),
        Line::from(clip(
            &format!(
                " CPU    {} now · {} tot",
                h.cpu_now.map(|p| format!("{p:.1}%")).unwrap_or_else(|| "-".into()),
                h.cpu_tot.map(fmt::human_duration).unwrap_or_else(|| "-".into())
            ),
            cols[0].width,
        )),
        Line::from(clip(
            &format!(
                " Tokens {} in / {} out ({})",
                compact_num(h.tokens_in.unwrap_or(0)),
                compact_num(h.tokens_out.unwrap_or(0)),
                h.tok_s.map(|v| format!("{v:.0}/s")).unwrap_or_else(|| "-/s".into())
            ),
            cols[0].width,
        )),
        Line::from(clip(
            &format!(
                " Wall   LLM {}% · Tool {}% · Q {}%",
                h.llm_share.unwrap_or(0),
                h.tool_share.unwrap_or(0),
                h.queue_share.unwrap_or(0)
            ),
            cols[0].width,
        )),
    ];
    frame.render_widget(Paragraph::new(left), cols[0]);
    let mut kind_lines = vec![Line::from(Span::styled(
        clip(" Work Kinds (Slots · Tasks · Wall · CPU · Now%)", cols[1].width),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for k in h.work_kinds.iter().take(area.height.saturating_sub(1) as usize) {
        kind_lines.push(Line::from(clip(
            &format!(
                " {:<11} {:>1}/{:<2} {:>5}t {:>7} {:>6} {:>6}",
                k.kind,
                k.used,
                k.total,
                k.tasks,
                fmt::human_duration(k.wall_ms),
                k.cpu_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into()),
                k.cpu_now.map(|p| format!("{p:.1}%")).unwrap_or_else(|| "-".into()),
            ),
            cols[1].width,
        )));
    }
    frame.render_widget(Paragraph::new(kind_lines), cols[1]);
}

fn draw_workspace(frame: &mut Frame, area: Rect, state: &mut TopState, wclass: WidthClass) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    let n_tree = state.count_tab(Tab::Tree);
    let n_agents = state.count_tab(Tab::Agents);
    let n_tasks = state.count_tab(Tab::Tasks);
    let sort = format!("{}{}", state.sort.label(), if state.sort_desc { "▼" } else { "▲" });
    let scope = state.scope.label();
    let tab_line = match state.tab {
        Tab::Tree => format!(" [1:Tree ({n_tree})]  2:Agents ({n_agents})  3:Tasks ({n_tasks})  4:Events ── Scope:{scope}(a) Sort:{sort}"),
        Tab::Agents => format!("  1:Tree ({n_tree})  [2:Agents ({n_agents})]  3:Tasks ({n_tasks})  4:Events ── Scope:{scope}(a) Sort:{sort}"),
        Tab::Tasks => format!("  1:Tree ({n_tree})  2:Agents ({n_agents})  [3:Tasks ({n_tasks})]  4:Events ── Scope:{scope}(a) Sort:{sort}"),
        Tab::Events => format!("  1:Tree ({n_tree})  2:Agents ({n_agents})  3:Tasks ({n_tasks})  [4:Events] ── Scope:{scope}(a) Sort:{sort}"),
    };
    state.mouse.tabs = hit_rect(chunks[0]);
    state.mouse.tab_line = tab_line.clone();
    frame.render_widget(Paragraph::new(clip(&tab_line, chunks[0].width)).style(Style::default().fg(Color::Cyan)), chunks[0]);

    let cols = match state.tab {
        Tab::Tree | Tab::Agents => tree_or_agent_cols(state.tab, wclass),
        Tab::Tasks => task_cols(wclass),
        Tab::Events => &[],
    };
    if state.tab == Tab::Events {
        draw_events(frame, chunks[1], state);
        return;
    }
    draw_table(frame, chunks[1], state, cols);
}

fn hit_rect(area: Rect) -> HitRect {
    HitRect::from_xywh(area.x, area.y, area.width, area.height)
}

fn tree_or_agent_cols(tab: Tab, w: WidthClass) -> &'static [Col] {
    if tab == Tab::Agents {
        agent_cols(w)
    } else {
        tree_cols(w)
    }
}

fn draw_table(frame: &mut Frame, area: Rect, state: &mut TopState, cols: &[Col]) {
    state.mouse.table = hit_rect(area);
    let page = area.height.saturating_sub(1) as usize;
    state.ensure_scroll(page);
    let widths = col_widths(cols, area.width);
    let header = Row::new(cols.iter().enumerate().map(|(i, c)| {
        Cell::from(clip(c.header(), widths[i])).style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
    }));
    let sel = state.selected.clone();
    let start = state.scroll;
    let end = (start + page).min(state.visible.len());
    let rows: Vec<Row> = state.visible[start..end]
        .iter()
        .map(|vr| {
            let selected = sel.as_deref() == Some(vr.id.as_str());
            let cells: Vec<Cell> = cols
                .iter()
                .enumerate()
                .map(|(i, c)| Cell::from(clip(&cell_text(state, vr, *c), widths[i])))
                .collect();
            let style = row_style(state, vr, selected);
            Row::new(cells).style(style)
        })
        .collect();
    let constraints: Vec<Constraint> = widths.iter().map(|w| Constraint::Length(*w)).collect();
    frame.render_widget(Table::new(rows, constraints).header(header), area);
}

fn draw_events(frame: &mut Frame, area: Rect, state: &mut TopState) {
    state.mouse.table = hit_rect(area);
    let page = area.height.saturating_sub(1) as usize;
    state.ensure_scroll(page);
    let start = state.scroll;
    let end = (start + page).min(state.visible.len());
    let sel = state.selected.clone();
    let mut lines = vec![Line::from(Span::styled(
        clip(" TIME                 SESS      SRC       TYPE               ID           DETAIL", area.width),
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ))];
    for vr in &state.visible[start..end] {
        let Some(&i) = state.event_index.get(&vr.id) else {
            continue;
        };
        let ev = &state.events[i];
        let text = format!(
            " {:<19} {:<9} {:<9} {:<18} {:<12} {}",
            fmt::datetime_ms(ev.ts),
            ev.session,
            ev.src,
            ev.ty,
            ev.id,
            ev.summary
        );
        let style = if sel.as_deref() == Some(vr.id.as_str()) {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(clip(&text, area.width), style)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_preview(frame: &mut Frame, area: Rect, state: &TopState) {
    let title = state
        .selected_node()
        .map(|n| format!(" Preview: {} · {} ", n.id, n.label))
        .unwrap_or_else(|| " Preview ".into());
    let block = Block::default().borders(Borders::TOP).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width.max(1) as usize;
    let mut wrapped = Vec::new();
    for (i, src) in state.preview_lines.iter().enumerate() {
        if src.starts_with("── ") && src.ends_with(" ──") {
            if i > 0 && wrapped.last().is_some_and(|s: &String| !s.is_empty()) {
                wrapped.push(String::new());
            }
            wrapped.push(src.clone());
        } else {
            wrapped.extend(fmt::wrap_width(src, width));
        }
    }
    let start = wrapped.len().saturating_sub(inner.height as usize);
    let lines: Vec<Line> = wrapped[start..]
        .iter()
        .map(|l| Line::from(paint_preview_line(l, inner.width)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn paint_preview_line(l: &str, width: u16) -> Span<'static> {
    let style = if l.starts_with("── ") && l.ends_with(" ──") {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    Span::styled(clip(l, width), style)
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &TopState) {
    let text = if let Some(id) = &state.confirm_stop {
        format!(" Stop {id}? [y/Enter] confirm  [n/Esc] cancel")
    } else if state.filter_editing {
        format!(" /{}_   Esc clear · Enter apply", state.filter)
    } else if !state.filter.is_empty() {
        format!(" filter: {}  ·  ↑↓/jk move · ←→ fold · Enter detail · v preview · a scope · s sort · / filter · x stop · q quit", state.filter)
    } else if let Some(m) = &state.message {
        format!(" {m}")
    } else {
        " ↑↓/jk move · ←→/hl fold · Enter detail · v preview · a scope · s sort · / filter · x stop · ? help · q quit".into()
    };
    frame.render_widget(
        Paragraph::new(clip(&text, area.width)).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let help = "\
1/2/3/4  Tab        views          a            cycle scope
↑↓ j k              move           s S < >      cycle sort
←→ h l              fold / unfold  I            invert sort
z                   expand all     /            filter
g G Home End        top / bottom   p Space      pause
+ -                 interval       v            preview
Enter               detail         e            events for selection
o / click           expand tools / thinking
x Ctrl+X F9         stop task      ?            this help
q Ctrl+C            quit           Esc          close overlay
click / wheel       select, fold,  tabs, sort";
    let rect = centered(area, 64, 18);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(help).block(Block::default().borders(Borders::ALL).title(" keys ")),
        rect,
    );
}

fn detail_frame(area: Rect) -> Rect {
    let h = if area.width < 50 { 1 } else { 2 };
    let v = if area.height < 16 { 1 } else { 2 };
    area.inner(ratatui::layout::Margin { horizontal: h, vertical: v })
}

fn draw_detail(frame: &mut Frame, area: Rect, d: &mut model::Detail, mouse: &mut model::MouseHit) {
    let rect = detail_frame(area);
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(format!(" {} ", d.title));
    let inner = block.inner(rect).inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    frame.render_widget(block, rect);
    let chat_meta = match &d.body {
        DetailBody::Conversation(c) => Some((c.stats.clone(), c.running)),
        _ => None,
    };
    let head_h = if d.tabs.is_empty() { 0 } else { 1 };
    let run_h = if chat_meta.as_ref().is_some_and(|(_, running)| *running) { 1 } else { 0 };
    let stats_h = if chat_meta.is_some() { 1 } else { 0 };
    let foot_gap = if run_h + stats_h > 0 { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(head_h),
            Constraint::Min(1),
            Constraint::Length(foot_gap),
            Constraint::Length(run_h),
            Constraint::Length(stats_h),
        ])
        .split(inner);
    if head_h > 0 {
        let tabs: String = d
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == d.tab {
                    format!("[{}:{}]", i + 1, t)
                } else {
                    format!(" {}:{} ", i + 1, t)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let follow = if d.follow { " follow" } else { "" };
        let head = format!("{tabs}{follow}  ·  f follow · Esc close");
        frame.render_widget(Paragraph::new(clip(&head, chunks[0].width)), chunks[0]);
    }
    let body = chunks[1];
    let bar_w = 1u16;
    let gap = 1u16;
    let text_w = body.width.saturating_sub(bar_w + gap).max(1);
    let width = text_w as usize;
    let height = body.height as usize;
    let painted = layout_detail(&d.body, width, &d.open, d.tools_expanded);
    d.wrap_len = painted.len();
    d.view_h = height;
    let (follow, scroll) = stick_scroll(d.follow, d.scroll, painted.len(), height);
    d.follow = follow;
    d.scroll = scroll;
    let start = d.scroll.min(painted.len());
    d.hits = painted[start..].iter().take(height).map(|p| p.fold).collect();
    let lines: Vec<Line> = painted[start..]
        .iter()
        .take(height)
        .map(|p| {
            let avail = (text_w as usize).saturating_sub(p.gutter).max(1);
            let mut spans = Vec::new();
            if p.gutter > 0 {
                spans.push(Span::raw(" ".repeat(p.gutter)));
            }
            spans.push(Span::styled(fit_width(&p.text, avail), p.style));
            Line::from(spans)
        })
        .collect();
    let chat = Rect { x: body.x, y: body.y, width: text_w, height: body.height };
    frame.render_widget(Clear, chat);
    let bar = Rect {
        x: body.x + text_w + gap,
        y: body.y,
        width: bar_w,
        height: body.height,
    };
    mouse.detail = hit_rect(rect);
    mouse.chat = hit_rect(chat);
    mouse.scrollbar = hit_rect(bar);
    frame.render_widget(Paragraph::new(lines), chat);
    draw_scrollbar(frame, bar, painted.len(), height, d.scroll);
    if run_h > 0 {
        frame.render_widget(
            Paragraph::new(clip(&format!("{} running", spin_glyph()), chunks[3].width))
                .style(Style::default().fg(Color::LightGreen).add_modifier(Modifier::ITALIC)),
            chunks[3],
        );
    }
    if let Some((stats, _)) = chat_meta {
        let hint = if d.tools_expanded { "  ·  o collapse" } else { "  ·  click ▸ / o" };
        frame.render_widget(
            Paragraph::new(clip(&format!("{stats}{hint}"), chunks[4].width))
                .style(Style::default().fg(Color::DarkGray)),
            chunks[4],
        );
    }
}

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const SPIN_MS: u128 = 80;
/// Left/right inset inside user bands and tool cards.
const BOX_PAD: usize = 2;
/// Tool cards sit further in than user / assistant messages.
const TOOL_INDENT: usize = 2;

fn spin_index(ms: u128) -> usize {
    (ms as usize / SPIN_MS as usize) % SPIN.len()
}

fn spin_glyph() -> &'static str {
    static START: OnceLock<Instant> = OnceLock::new();
    let t = START.get_or_init(Instant::now);
    SPIN[spin_index(t.elapsed().as_millis())]
}

/// Stick to the end while following; scrolling up unsticks, hitting the
/// bottom resticks. Scroll never jumps back while the user is reading.
fn stick_scroll(follow: bool, scroll: usize, len: usize, height: usize) -> (bool, usize) {
    let max = len.saturating_sub(height);
    if follow || scroll >= max {
        (true, max)
    } else {
        (false, scroll)
    }
}

fn draw_scrollbar(frame: &mut Frame, area: Rect, len: usize, height: usize, scroll: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let track = height.max(1);
    let marks = scrollbar_marks(len, height, scroll, track);
    let lines: Vec<Line> = marks
        .into_iter()
        .map(|c| {
            let style = if c == '█' {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(c.to_string(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn scrollbar_marks(len: usize, height: usize, scroll: usize, track: usize) -> Vec<char> {
    if track == 0 {
        return Vec::new();
    }
    if len <= height {
        return vec!['│'; track];
    }
    let thumb = ((height * track) / len).max(1).min(track);
    let max_scroll = len - height;
    let pos = if max_scroll == 0 {
        0
    } else {
        (scroll * (track - thumb) + max_scroll / 2) / max_scroll
    };
    let pos = pos.min(track - thumb);
    (0..track)
        .map(|i| if i >= pos && i < pos + thumb { '█' } else { '│' })
        .collect()
}

/// Pi dark-theme chat colors (okhsl approximations).
const USER_BG: Color = Color::Rgb(42, 54, 84);
const USER_FG: Color = Color::Rgb(220, 222, 228);
const TOOL_OK_BG: Color = Color::Rgb(28, 64, 52);
const TOOL_ERR_BG: Color = Color::Rgb(72, 36, 28);
const TOOL_PENDING_BG: Color = Color::Rgb(38, 40, 48);
const TOOL_OUT_FG: Color = Color::Rgb(150, 156, 168);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Assistant,
    Tool,
    Error,
    Body,
    Dim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Painted {
    style: Style,
    text: String,
    gutter: usize,
    fold: Option<FoldHit>,
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Assistant => Style::default().fg(Color::Green),
        Tone::Tool => Style::default().fg(Color::Yellow),
        Tone::Error => Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
        Tone::Dim => Style::default().fg(Color::DarkGray),
        Tone::Body => Style::default(),
    }
}

fn paint(tone: Tone, text: impl Into<String>) -> Painted {
    paint_style(tone_style(tone), text)
}

fn paint_style(style: Style, text: impl Into<String>) -> Painted {
    Painted { style, text: text.into(), gutter: 0, fold: None }
}

fn paint_fold(style: Style, text: impl Into<String>, gutter: usize, fold: Option<FoldHit>) -> Painted {
    Painted { style, text: text.into(), gutter, fold }
}

fn toggle_hit(id: usize, gutter: usize) -> FoldHit {
    let x0 = (gutter + BOX_PAD) as u16;
    FoldHit { id, x0, x1: x0.saturating_add(2) }
}

fn fit_width(text: &str, width: usize) -> String {
    let t = if fmt::display_width(text) > width {
        fmt::truncate_width(text, width)
    } else {
        text.to_string()
    };
    fmt::pad(&t, width)
}

fn fill_width(text: &str, width: usize) -> String {
    let w = fmt::display_width(text);
    if w >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - w))
    }
}

fn layout_detail(body: &DetailBody, width: usize, open: &HashSet<usize>, expand_all: bool) -> Vec<Painted> {
    match body {
        DetailBody::Empty => vec![paint(Tone::Dim, "(nothing to show)")],
        DetailBody::Conversation(chat) => layout_conversation(chat, width, open, expand_all),
        DetailBody::Log { header, lines } => {
            let mut out = Vec::new();
            for (i, h) in header.iter().enumerate() {
                let tone = if i == 0 { Tone::Dim } else if h.starts_with('$') { Tone::Tool } else { Tone::Assistant };
                for l in fmt::wrap_width(h, width) {
                    out.push(paint(tone, l));
                }
            }
            if !header.is_empty() {
                out.push(paint(Tone::Body, ""));
            }
            for line in lines {
                for l in fmt::wrap_width(line, width) {
                    out.push(paint(Tone::Body, l));
                }
            }
            out
        }
        DetailBody::Text(lines) => {
            let mut out = Vec::new();
            for line in lines {
                let tone = if line.contains("error") || line.starts_with("error") {
                    Tone::Error
                } else {
                    Tone::Body
                };
                for l in fmt::wrap_width(line, width) {
                    out.push(paint(tone, l));
                }
            }
            if out.is_empty() {
                out.push(paint(Tone::Dim, "(empty)"));
            }
            out
        }
    }
}

fn is_open(id: usize, open: &HashSet<usize>, expand_all: bool) -> bool {
    expand_all || open.contains(&id)
}

fn layout_conversation(chat: &Chat, width: usize, open: &HashSet<usize>, expand_all: bool) -> Vec<Painted> {
    if chat.turns.is_empty() {
        return vec![paint(Tone::Dim, "(no conversation yet)")];
    }
    let main = if chat.main.is_empty() { "main" } else { chat.main.as_str() };
    let agent = if chat.agent.is_empty() {
        "subagent".to_string()
    } else {
        format!("subagent · {}", chat.agent)
    };
    let mut out = Vec::new();
    let mut voice: Option<&str> = None;
    let mut fold = 0usize;
    let mut i = 0;
    while i < chat.turns.len() {
        let turn = &chat.turns[i];
        if turn.role == "user" {
            speak(main, true, turn.ts, &mut voice, width, &mut out);
            gap_if_boxed(&mut out);
            user_band(&turn.text, turn.ts, width, &mut out);
            i += 1;
            continue;
        }
        if turn.role == "thinking" {
            speak(&agent, false, turn.ts, &mut voice, width, &mut out);
            let id = fold;
            fold += 1;
            thinking_block(&turn.text, turn.ts, is_open(id, open, expand_all), id, width, &mut out);
            i += 1;
            continue;
        }
        if turn.tool.is_some() {
            speak(&agent, false, turn.ts, &mut voice, width, &mut out);
            let title = tool_card_title(turn, i.checked_sub(1).and_then(|j| chat.turns.get(j)));
            gap_if_boxed(&mut out);
            let id = fold;
            fold += 1;
            tool_card(&title, &turn.text, turn.ts, turn.error, false, is_open(id, open, expand_all), id, width, &mut out);
            i += 1;
            continue;
        }
        if turn.role == "assistant" || turn.role == "assistant error" {
            speak(&agent, false, turn.ts, &mut voice, width, &mut out);
            let segs = assistant_segments(&turn.text);
            let has_tools = segs.iter().any(|s| matches!(s, Seg::ToolCall { .. }));
            let mut seen_tool = false;
            i += 1;
            for seg in segs {
                match seg {
                    Seg::Text(text) => {
                        if has_tools && !seen_tool {
                            let id = fold;
                            fold += 1;
                            thinking_block(&text, turn.ts, is_open(id, open, expand_all), id, width, &mut out);
                        } else {
                            let style = if turn.error {
                                tone_style(Tone::Error)
                            } else {
                                Style::default()
                            };
                            for l in wrap_padded(text.trim(), width) {
                                out.push(paint_style(style, l));
                            }
                        }
                    }
                    Seg::ToolCall { name, summary } => {
                        seen_tool = true;
                        let title = format!("{name} {summary}").trim().to_string();
                        gap_if_boxed(&mut out);
                        let id = fold;
                        fold += 1;
                        if chat.turns.get(i).is_some_and(|n| n.tool.as_deref() == Some(name.as_str())) {
                            let result = &chat.turns[i];
                            tool_card(&title, &result.text, result.ts.or(turn.ts), result.error, false, is_open(id, open, expand_all), id, width, &mut out);
                            i += 1;
                        } else {
                            tool_card(&title, "", turn.ts, false, true, is_open(id, open, expand_all), id, width, &mut out);
                        }
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    if out.is_empty() {
        out.push(paint(Tone::Dim, "(no conversation yet)"));
    }
    out
}

fn speak<'a>(label: &'a str, is_main: bool, ts: Option<u64>, last: &mut Option<&'a str>, width: usize, out: &mut Vec<Painted>) {
    if last.as_deref() == Some(label) {
        return;
    }
    if !out.is_empty() {
        out.push(paint(Tone::Body, ""));
    }
    let style = if is_main {
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    };
    out.push(paint_style(style, titled(label, ts, width)));
    *last = Some(label);
}

fn thinking_block(text: &str, ts: Option<u64>, expanded: bool, fold: usize, width: usize, out: &mut Vec<Painted>) {
    let style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
    let label = if expanded { "▾ Thinking" } else { "▸ Thinking…" };
    out.push(paint_fold(style, titled(label, ts, width), 0, Some(toggle_hit(fold, 0))));
    if !expanded {
        return;
    }
    for l in wrap_padded(&fmt::scrub_tty(text.trim()), width) {
        out.push(paint_style(style, l));
    }
}

fn box_inner(width: usize) -> usize {
    width.saturating_sub(BOX_PAD * 2).max(1)
}

fn pad_text(text: &str, width: usize) -> String {
    fit_width(&format!("{}{}", " ".repeat(BOX_PAD), fmt::truncate_width(text, box_inner(width))), width)
}

fn clock(ts: Option<u64>) -> String {
    ts.map(fmt::datetime)
        .and_then(|s| s.split(' ').nth(1).map(|t| t.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn titled(label: &str, ts: Option<u64>, width: usize) -> String {
    let time = clock(ts);
    if time.is_empty() {
        return pad_text(label, width);
    }
    let inner = box_inner(width);
    let budget = inner.saturating_sub(time.len() + 3).max(1);
    let left = fmt::truncate_width(label, budget);
    pad_text(&format!("{left}  {time}"), width)
}

fn wrap_padded(text: &str, width: usize) -> Vec<String> {
    fmt::wrap_width(text, box_inner(width))
        .into_iter()
        .map(|l| fit_width(&format!("{}{l}", " ".repeat(BOX_PAD)), width))
        .collect()
}

fn gap_if_boxed(out: &mut Vec<Painted>) {
    if out.last().is_some_and(|p| p.style.bg.is_some()) {
        out.push(paint(Tone::Body, ""));
    }
}

enum Seg {
    Text(String),
    ToolCall { name: String, summary: String },
}

fn assistant_segments(text: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(rel) = text[i..].find("tool ") {
            let start = i + rel;
            if let Some((end, name, summary)) = parse_tool_call_at(&text[start..]) {
                let prev = text[i..start].trim();
                if !prev.is_empty() {
                    segs.push(Seg::Text(prev.to_string()));
                }
                segs.push(Seg::ToolCall { name, summary });
                i = start + end;
                continue;
            }
            i = start + 5;
            continue;
        }
        let rest = text[i..].trim();
        if !rest.is_empty() {
            segs.push(Seg::Text(rest.to_string()));
        }
        break;
    }
    segs
}

fn parse_tool_call_at(s: &str) -> Option<(usize, String, String)> {
    let rest = s.strip_prefix("tool ")?;
    let brace = rest.find('{')?;
    let name = rest[..brace].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let json_start = 5 + brace;
    let json = &s[json_start..];
    let json_len = take_json_object(json)?;
    let raw = &json[..json_len];
    Some((json_start + json_len, name.to_string(), summary_from_raw(name, raw)))
}

fn take_json_object(s: &str) -> Option<usize> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if in_str {
            if escape {
                escape = false;
                continue;
            }
            if c == '\\' {
                escape = true;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + c.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

fn summary_from_raw(name: &str, json: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        return pick_summary(name, &v);
    }
    extract_json_string(json, "path")
        .or_else(|| extract_json_string(json, "command"))
        .or_else(|| extract_json_string(json, "query"))
        .or_else(|| extract_json_string(json, "pattern"))
        .unwrap_or_default()
}

fn pick_summary(name: &str, v: &serde_json::Value) -> String {
    let _ = name;
    ["path", "command", "query", "pattern", "file"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string()
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let rest = json.split(&pat).nth(1)?;
    let mut out = String::new();
    let mut escape = false;
    for c in rest.chars() {
        if escape {
            out.push(c);
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '"' {
            break;
        }
        if c == '\n' {
            break;
        }
        out.push(c);
    }
    if out.is_empty() { None } else { Some(out) }
}

fn user_band(text: &str, _ts: Option<u64>, width: usize, out: &mut Vec<Painted>) {
    let style = Style::default().fg(USER_FG).bg(USER_BG);
    out.push(paint_style(style, fill_width(" ", width)));
    let body = if text.trim().is_empty() { " " } else { text.trim() };
    for l in wrap_padded(body, width) {
        out.push(paint_style(style, l));
    }
    out.push(paint_style(style, fill_width(" ", width)));
}

fn tool_card_title(turn: &TranscriptTurn, prev: Option<&TranscriptTurn>) -> String {
    let name = turn.tool.as_deref().unwrap_or("tool");
    if let Some(prev) = prev {
        if let Some((call, summary)) = tool_call_summary(&prev.text) {
            if call == name {
                return format!("{name} {summary}").trim().to_string();
            }
        }
    }
    if let Some(args) = turn.args.as_deref().filter(|s| !s.is_empty()) {
        let summary = summary_from_raw(name, args);
        if !summary.is_empty() {
            return format!("{name} {summary}");
        }
    }
    name.to_string()
}

fn tool_card(
    title: &str,
    output: &str,
    ts: Option<u64>,
    error: bool,
    pending: bool,
    expanded: bool,
    fold: usize,
    width: usize,
    out: &mut Vec<Painted>,
) {
    let bg = if error {
        TOOL_ERR_BG
    } else if pending {
        TOOL_PENDING_BG
    } else {
        TOOL_OK_BG
    };
    let title_style = Style::default().fg(USER_FG).bg(bg).add_modifier(Modifier::BOLD);
    let out_style = Style::default().fg(if error { Color::LightRed } else { TOOL_OUT_FG }).bg(bg);
    let mark = if pending {
        "·"
    } else if expanded || output.trim().is_empty() {
        "▾"
    } else {
        "▸"
    };
    let card_w = width.saturating_sub(TOOL_INDENT).max(8);
    out.push(paint_fold(
        title_style,
        titled(&format!("{mark} {title}"), ts, card_w),
        TOOL_INDENT,
        Some(toggle_hit(fold, TOOL_INDENT)),
    ));
    if output.trim().is_empty() || !expanded {
        return;
    }
    for para in fmt::scrub_tty(output).lines() {
        for l in wrap_padded(para, card_w) {
            out.push(paint_fold(out_style, l, TOOL_INDENT, None));
        }
    }
}

fn tool_call_summary(text: &str) -> Option<(String, String)> {
    parse_tool_call_at(text.trim()).map(|(_, name, summary)| (name, summary))
}

fn cell_text(state: &TopState, vr: &VisibleRow, col: Col) -> String {
    if state.tab == Tab::Tasks {
        if let Some(&i) = state.task_index.get(&vr.id) {
            return task_cell(state, &state.task_rows[i], col);
        }
    }
    let Some(n) = state.nodes.get(&vr.id) else {
        return String::new();
    };
    match col {
        Col::Node | Col::Id => node_label(vr, n),
        Col::CwdModel | Col::Cwd => n.extra.clone(),
        Col::Status => n.status.clone(),
        Col::Tasks => n.metrics.tasks.to_string(),
        Col::Wall | Col::Dur => wall_cell(n.metrics.wall_ms),
        Col::Cpu => cpu_cell(n.metrics.cpu_ms),
        Col::Now => now_cell(n.metrics.cpu_now, n.metrics.cpu_stale),
        Col::Cores => inspect_cores(n.metrics.cpu_ms, n.metrics.wall_ms),
        Col::TokIo => tok_io_cell(n.metrics.tokens_in, n.metrics.tokens_out),
        Col::TokS => n.metrics.tok_s.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into()),
        Col::Cache => tok_io_cell(n.metrics.cache_r, n.metrics.cache_w),
        Col::WallSplit => format!(
            "{}/{}/{}",
            n.metrics.llm_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into()),
            n.metrics.tool_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into()),
            n.metrics.queue_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into()),
        ),
        Col::Reason => n.reason.clone().unwrap_or_else(|| "-".into()),
        Col::Kind => match n.kind {
            NodeKind::Task => n.work_kind.clone().unwrap_or_else(|| "task".into()),
            NodeKind::Agent => "agent".into(),
            NodeKind::Session => "session".into(),
            NodeKind::Main => "main".into(),
            NodeKind::Run => "run".into(),
        },
        Col::Title => n.title.clone(),
        Col::Sess => n.session_id.chars().take(8).collect(),
        Col::Exit => n.exit.clone().unwrap_or_else(|| "-".into()),
        Col::WorkKind => n.work_kind.clone().unwrap_or_else(|| "-".into()),
        Col::Sample => "-".into(),
    }
}

fn task_cell(state: &TopState, r: &crate::inspect::Row, col: Col) -> String {
    let cpu = if r.running {
        crate::inspect::cpu_ms(r.live_cpu_user_ms, r.live_cpu_sys_ms)
    } else {
        crate::inspect::cpu_ms(r.cpu_user_ms, r.cpu_sys_ms)
    };
    match col {
        Col::Id => r.id.clone(),
        Col::Kind => r.kind.clone(),
        Col::Status => r.status.clone(),
        Col::Dur | Col::Wall => r.duration_ms.map(wall_cell).unwrap_or_else(|| "-".into()),
        Col::Cpu => cpu_cell(cpu),
        Col::Now => now_cell(r.live_cpu_percent, r.live_cpu_stale),
        Col::Title => r.title.clone(),
        Col::Sess => r.session_id.chars().take(8).collect(),
        Col::Cwd | Col::CwdModel => r.cwd.as_deref().map(fmt::tilde).unwrap_or_else(|| "-".into()),
        Col::Cores => inspect_cores(cpu, r.duration_ms.unwrap_or(0)),
        Col::Exit => r.signal.clone().or_else(|| r.exit_code.map(|c| c.to_string())).unwrap_or_else(|| "-".into()),
        Col::WorkKind => r.work_kind.clone().unwrap_or_else(|| "-".into()),
        Col::Sample => r.live_cpu_sampled_at.map(|t| fmt::short_time(t, state.now)).unwrap_or_else(|| "-".into()),
        Col::Reason => r.end_reason.clone().unwrap_or_else(|| "-".into()),
        Col::TokIo => state
            .nodes
            .get(&r.id)
            .map(|n| tok_io_cell(n.metrics.tokens_in, n.metrics.tokens_out))
            .unwrap_or_else(|| "-".into()),
        _ => String::new(),
    }
}

fn node_label(vr: &VisibleRow, n: &Node) -> String {
    let indent = "  ".repeat(vr.depth);
    let glyph = if vr.has_hidden_children {
        if vr.expanded { "▼" } else { "▸" }
    } else {
        " "
    };
    let mark = if n.metrics.running {
        "●"
    } else if n.status == "failed" || n.status == "killed" {
        "✕"
    } else if n.status == "connected" {
        "●"
    } else {
        "○"
    };
    format!("{indent}{glyph} {mark} {}", n.label)
}

fn row_style(state: &TopState, vr: &VisibleRow, selected: bool) -> Style {
    let mut s = Style::default();
    if let Some(n) = state.nodes.get(&vr.id) {
        if n.metrics.running || n.status == "connected" {
            s = s.fg(Color::LightGreen);
        } else if n.status == "failed" || n.status == "killed" {
            s = s.fg(Color::LightRed);
        }
    }
    if selected {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn inspect_cores(cpu: Option<u64>, wall: u64) -> String {
    crate::inspect::cores_text(cpu, wall)
}

fn col_widths(cols: &[Col], total: u16) -> Vec<u16> {
    let fixed: u16 = cols.iter().filter_map(|c| c.width()).sum();
    let gaps = cols.len().saturating_sub(1) as u16;
    let flex = total.saturating_sub(fixed.saturating_add(gaps)).max(8);
    cols.iter().map(|c| c.width().unwrap_or(flex)).collect()
}

fn slot_bar(used: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return " ".repeat(width);
    }
    let fill = ((used * width) + total - 1) / total;
    let fill = fill.min(width);
    format!("{}{}", "■".repeat(fill), "□".repeat(width - fill))
}

fn clip(s: &str, width: u16) -> String {
    fmt::truncate_width(s, width as usize)
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::{AgentRecord, SessionView, Snapshot};
    use crate::proto::{Origin, TaskKind, TaskRecord, TaskStatus};
    use crate::top::collector::open_detail;
    use crate::top::model::{width_class, TopState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::{BTreeMap, HashMap, HashSet};

    fn rec(id: &str, sid: &str, child: Option<&str>, cmd: &str, running: bool) -> TaskRecord {
        TaskRecord {
            task_id: id.into(),
            session_id: sid.into(),
            kind: TaskKind::Shell,
            command: cmd.into(),
            cwd: "/tmp/app".into(),
            pid: 1,
            status: if running { TaskStatus::Running } else { TaskStatus::Completed },
            exit_code: None,
            signal: None,
            started_at: 1_000,
            ended_at: (!running).then_some(2_000),
            output_path: String::new(),
            output_size: 0,
            origin: Some(Origin {
                via: "child-bash".into(),
                child_id: child.map(|c| c.into()),
                run_id: Some("run_1".into()),
            }),
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: Some(100),
            cpu_sys_ms: Some(20),
            max_rss_kb: Some(1),
            live_cpu_user_ms: running.then_some(100),
            live_cpu_sys_ms: running.then_some(20),
            live_cpu_percent: running.then_some(40.0),
            live_cpu_sampled_at: running.then_some(2_000),
            live_cpu_stale: false,
            label: None,
        }
    }

    fn snap() -> Snapshot {
        let mut sessions = BTreeMap::new();
        sessions.insert(
            "sessA".into(),
            SessionView {
                session_id: "sessA".into(),
                pi_pid: Some(7),
                cwd: Some("/tmp/app".into()),
                state: "connected".into(),
                since: Some(1),
                last_seen: Some(2),
                protocol: Some(4),
                extension_version: None,
            },
        );
        Snapshot {
            now: 10_000,
            daemon: None,
            tasks: vec![
                rec("sh_live", "sessA", Some("ch_a"), "pytest -n 4", true),
                rec("sh_old", "sessA", Some("ch_a"), "rg foo", false),
            ],
            agents: vec![AgentRecord {
                child_id: "ch_a".into(),
                run_id: Some("run_1".into()),
                session_id: "sessA".into(),
                name: "worker".into(),
                agent: "worker".into(),
                model: Some("haiku".into()),
                status: "running".into(),
                started_at: Some(1_000),
                ended_at: None,
                error: None,
                end_reason: None,
                prompt_head: None,
                result_tail: None,
                tool_calls: None,
                tokens_input: Some(100),
                tokens_output: Some(20),
                tokens_cache_read: None,
                tokens_cache_write: None,
                llm_ms: Some(1_000),
                tool_ms: None,
                queue_ms: None,
                wall_other_ms: None,
                wall_approximate: false,
                transcript: None,
                stale: false,
            }],
            sessions,
            connected: HashSet::from(["sessA".into()]),
        }
    }

    fn render(cols: u16, rows: u16, state: &mut TopState) -> String {
        let backend = TestBackend::new(cols, rows);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, state)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..rows {
            for x in 0..cols {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_without_panic_from_tiny_to_wide() {
        let snap = snap();
        for (c, r) in [(40, 10), (80, 24), (120, 30), (160, 40)] {
            let mut state = TopState::new(1_000, None, None);
            state.apply(&snap, &[], &HashMap::new());
            let text = render(c, r, &mut state);
            assert_eq!(text.lines().count(), r as usize, "{c}x{r}");
            for line in text.lines() {
                assert!(
                    fmt::display_width(line) <= c as usize,
                    "{c}x{r} wrapped: {line:?} width {}",
                    fmt::display_width(line)
                );
            }
            assert!(text.contains("Tree") || text.contains("pi-famulus"), "{c}x{r}: {text}");
        }
    }

    #[test]
    fn width_classes_match_the_plan() {
        assert_eq!(width_class(80), WidthClass::Compact);
        assert_eq!(width_class(120), WidthClass::Standard);
        assert_eq!(width_class(160), WidthClass::Wide);
    }

    fn mk_turn(role: &str, text: &str) -> TranscriptTurn {
        TranscriptTurn {
            role: role.into(),
            tool: None,
            args: None,
            text: text.into(),
            ts: None,
            error: false,
        }
    }

    fn chat_body(turns: Vec<TranscriptTurn>) -> DetailBody {
        DetailBody::Conversation(Chat {
            turns,
            main: "main".into(),
            agent: "worker".into(),
            stats: "tokens - · - tok/s · wall - · shells 0".into(),
            running: false,
        })
    }

    fn layout(body: &DetailBody, width: usize, expand_all: bool) -> Vec<Painted> {
        layout_detail(body, width, &HashSet::new(), expand_all)
    }

    #[test]
    fn conversation_detail_is_a_chat_not_role_headers() {
        let turns = vec![
            mk_turn("user", "inspect the loader please and then report"),
            mk_turn("assistant", r#"tool bash {"command":"rg loader"}"#),
            TranscriptTurn {
                role: "tool".into(),
                tool: Some("bash".into()),
                args: None,
                text: "src/loader.rs\nsrc/main.rs".into(),
                ts: None,
                error: false,
            },
        ];
        let painted = layout(&chat_body(turns), 40, false);
        let text: Vec<&str> = painted.iter().map(|p| p.text.trim()).collect();
        assert!(!text.iter().any(|l| l.starts_with("── ")), "{text:?}");
        assert!(text.iter().any(|l| *l == "main"), "{text:?}");
        assert!(text.iter().any(|l| *l == "subagent · worker"), "{text:?}");
        assert!(text.iter().any(|l| l.contains("inspect the loader")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("bash") && l.contains("rg loader")), "{text:?}");
        assert!(!text.iter().any(|l| l.contains("src/loader.rs")), "{text:?}");
        assert!(
            painted.iter().any(|p| p.style.bg == Some(USER_BG) && p.text.contains("inspect")),
            "user turn is a full-width band"
        );
        assert!(
            painted.iter().any(|p| p.style.bg == Some(TOOL_OK_BG)),
            "tool result is a card"
        );
        assert!(painted.iter().all(|p| fmt::display_width(&p.text) <= 40));
    }

    #[test]
    fn stacked_tools_have_gaps_and_one_line_titles() {
        let long = format!("bash {}", "x".repeat(80));
        let tools: Vec<TranscriptTurn> = (0..3)
            .map(|_| TranscriptTurn {
                role: "tool".into(),
                tool: Some("bash".into()),
                args: None,
                text: long.clone(),
                ts: None,
                error: false,
            })
            .collect();
        let painted = layout(&chat_body(tools), 48, false);
        assert!(painted.iter().all(|p| fmt::display_width(&p.text) <= 48));
        let mut runs = 0;
        let mut in_card = false;
        for p in &painted {
            let bg = p.style.bg == Some(TOOL_OK_BG);
            if bg && !in_card {
                runs += 1;
            }
            in_card = bg;
        }
        assert_eq!(runs, 3, "each tool is its own card, not one wall: {painted:?}");
        let titles: Vec<_> = painted
            .iter()
            .filter(|p| p.style.bg == Some(TOOL_OK_BG) && p.text.contains('▸'))
            .collect();
        assert_eq!(titles.len(), 3);
        for t in titles {
            assert!(!t.text.contains(&"x".repeat(20)), "title must truncate: {}", t.text);
            assert!(t.text.starts_with("  "), "title needs side pad: {:?}", t.text);
            assert_eq!(t.gutter, TOOL_INDENT, "tool cards indent past messages");
        }
    }

    #[test]
    fn expanded_tool_output_is_not_a_toggle_target() {
        let turn = TranscriptTurn {
            role: "tool".into(),
            tool: Some("bash".into()),
            args: Some(r#"{"command":"git log"}"#.into()),
            text: "\x1b[32mcdcdcfe\x1b[0m ok\rRebasing (2/2)\nDone".into(),
            ts: None,
            error: false,
        };
        let painted = layout(&chat_body(vec![turn]), 48, true);
        let text = painted.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("\x1b"), "{text}");
        assert!(!text.contains("[32m"), "{text}");
        assert!(text.contains("Rebasing (2/2)"), "{text}");
        assert!(!text.contains("Rebasing (1/"), "{text}");
        let headers = painted.iter().filter(|p| p.fold.is_some()).count();
        let body = painted.iter().filter(|p| p.text.contains("Done") || p.text.contains("ok")).count();
        assert_eq!(headers, 1, "only the ▸/▾ row is clickable");
        assert!(body > 0);
        assert!(painted.iter().any(|p| p.fold.is_some_and(|h| h.x0 > 0 && h.x1 > h.x0)));
    }

    #[test]
    fn write_tool_call_does_not_dump_file_content() {
        let dumped = concat!(
            r#"tool write{"path":"/tmp/assemble-policy-gate.py","content":"from pathlib import Path"#,
            "\nimport subprocess\nprint(1)\n\"}"
        );
        let painted = layout(&chat_body(vec![mk_turn("assistant", dumped)]), 60, false);
        let text = painted.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("write"), "{text}");
        assert!(text.contains("assemble-policy-gate.py"), "{text}");
        assert!(!text.contains("from pathlib"), "{text}");
        assert!(!text.contains("import subprocess"), "{text}");
        assert!(painted.iter().any(|p| p.fold.is_some() && p.gutter == TOOL_INDENT), "{painted:?}");
    }

    #[test]
    fn each_block_shows_a_clock() {
        let mut user = mk_turn("user", "look at src");
        user.ts = Some(1_774_000_000_000);
        let tool = TranscriptTurn {
            role: "tool".into(),
            tool: Some("bash".into()),
            args: Some(r#"{"command":"ls"}"#.into()),
            text: "ok".into(),
            ts: Some(1_774_000_030_000),
            error: false,
        };
        let painted = layout(&chat_body(vec![user, tool]), 48, false);
        let text = painted.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains(':'), "expected a clock in\n{text}");
    }

    #[test]
    fn tool_and_thinking_collapse_until_expanded() {
        let turn = TranscriptTurn {
            role: "tool".into(),
            tool: Some("bash".into()),
            args: None,
            text: (0..12).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n"),
            ts: None,
            error: false,
        };
        let thinking = mk_turn("thinking", "I should search the loader first.");
        let mixed = mk_turn(
            "assistant",
            "need the files\ntool bash {\"command\":\"rg loader\"}",
        );
        let collapsed = layout(&chat_body(vec![thinking.clone(), mixed, turn.clone()]), 40, false);
        let expanded = layout(&chat_body(vec![thinking, turn]), 40, true);
        let c = collapsed.iter().map(|p| p.text.trim()).collect::<Vec<_>>().join("\n");
        let e = expanded.iter().map(|p| p.text.trim()).collect::<Vec<_>>().join("\n");
        assert!(c.contains("Thinking…"), "{c}");
        assert!(!c.contains("I should search"), "{c}");
        assert!(!c.contains("need the files"), "{c}");
        assert!(!c.contains("line-0"), "{c}");
        assert!(!c.contains("line-11"), "{c}");
        assert!(c.contains("bash"), "{c}");
        assert!(e.contains("I should search"), "{e}");
        assert!(e.contains("line-0") && e.contains("line-11"), "{e}");
        assert!(!e.contains("Thinking…"), "{e}");
    }

    #[test]
    fn agent_detail_footer_is_stats_not_tabs() {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        state.selected = Some("ch_a".into());
        state.now = 1_200;
        let mut d = open_detail(&state).expect("agent opens");
        assert!(d.tabs.is_empty());
        assert!(d.title.contains("subagent"));
        d.body = chat_body(vec![mk_turn("user", "inspect the loader")]);
        if let DetailBody::Conversation(c) = &mut d.body {
            c.stats = "tokens 100 in / 20 out · 20.0 tok/s · wall LLM 1s / tool - / queue - · shells 1".into();
            c.running = true;
        }
        state.detail = Some(d);
        let text = render(80, 24, &mut state);
        assert!(!text.contains("1:Conversation"), "{text}");
        assert!(!text.contains("2:Result"), "{text}");
        assert!(!text.contains("3:Info"), "{text}");
        assert!(text.contains("tokens 100"), "{text}");
        assert!(text.contains("tok/s"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(text.contains("subagent · worker"), "{text}");
    }

    #[test]
    fn follow_resticks_at_the_bottom() {
        assert_eq!(stick_scroll(true, 3, 40, 10), (true, 30));
        assert_eq!(stick_scroll(false, 12, 40, 10), (false, 12));
        assert_eq!(stick_scroll(false, 30, 40, 10), (true, 30));
        assert_eq!(stick_scroll(false, 99, 40, 10), (true, 30));
    }

    #[test]
    fn scrollbar_thumb_moves_with_progress() {
        let top = scrollbar_marks(40, 10, 0, 10);
        let bottom = scrollbar_marks(40, 10, 30, 10);
        assert!(top.iter().any(|c| *c == '█'), "{top:?}");
        assert!(bottom.iter().any(|c| *c == '█'), "{bottom:?}");
        let top_pos = top.iter().position(|c| *c == '█').unwrap();
        let bot_pos = bottom.iter().position(|c| *c == '█').unwrap();
        assert!(top_pos < bot_pos, "top {top_pos} bottom {bot_pos}");
    }
}
