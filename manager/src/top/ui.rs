//! Ratatui drawing for interactive `top`. Pure function of [`TopState`] and
//! the frame `Rect`; no I/O.
//!
//! Visual language: no boxed panels on the main screen, dim labels next to
//! bright values, color reserved for status glyphs and the accent. The agent
//! chat is a thread: user turns hang off an accent rail, assistant text is
//! plain, and tools / thinking are one-line rows that unfold in place under a
//! thin rule.

use crate::fmt;
use crate::inspect::TranscriptTurn;
use crate::top::model::{
    self, agent_cols, compact_num, cpu_cell, header_height, height_class, now_cell, task_cols, tok_io_cell, tree_cols,
    wall_cell, width_class, Chat, Col, DetailBody, FoldHit, HeightClass, HitRect, Node, NodeKind, Tab, TabHit,
    TopState, VisibleRow, WidthClass,
};
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table};
use ratatui::Frame;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Instant;

// ── palette ────────────────────────────────────────────────────────────────

const ACCENT: Color = Color::Rgb(130, 170, 255);
const OK: Color = Color::Rgb(134, 200, 120);
const ERR: Color = Color::Rgb(240, 110, 120);
const WARN: Color = Color::Rgb(230, 180, 90);
const CODE: Color = Color::Rgb(214, 186, 140);
const TEXT: Color = Color::Rgb(222, 225, 232);
const MUTED: Color = Color::Rgb(150, 156, 172);
const DIM: Color = Color::Rgb(100, 106, 124);
const RULE: Color = Color::Rgb(62, 66, 80);
const SEL_BG: Color = Color::Rgb(38, 42, 56);

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn sp(style: Style, text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| fmt::display_width(&s.content)).sum()
}

/// Clip a run of spans to `width` columns, ending with "…" when cut.
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= width {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let w = fmt::display_width(&s.content);
        if used + w < width {
            used += w;
            out.push(s);
            continue;
        }
        let room = width - used;
        if room > 0 {
            out.push(Span::styled(fmt::truncate_width(&s.content, room), s.style));
        }
        break;
    }
    out
}

/// `left` then `right` flush to the right edge; `right` is dropped when the
/// two would touch.
fn split_line(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let rw = spans_width(&right);
    let lw = spans_width(&left);
    if rw == 0 || lw + rw + 2 > width {
        return fit_spans(left, width);
    }
    left.push(Span::raw(" ".repeat(width - lw - rw)));
    left.extend(right);
    left
}

/// `key label` pairs for footers: key bright, label dim.
fn hints(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (i, (k, l)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(Span::raw("  "));
        }
        out.push(sp(fg(MUTED).add_modifier(Modifier::BOLD), *k));
        out.push(sp(fg(DIM), format!(" {l}")));
    }
    out
}

// ── screen ─────────────────────────────────────────────────────────────────

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

    draw_header(frame, chunks[0], state, hclass);
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

/// Refresh state plus the clock of the last snapshot, so each refresh is
/// visible even when no number on screen changed.
fn live_pill(state: &TopState) -> Vec<Span<'static>> {
    let at = clock((state.now > 0).then_some(state.now));
    let at = if at.is_empty() { String::new() } else { format!(" · {at}") };
    if state.paused {
        vec![sp(fg(WARN), "‖ paused"), sp(fg(DIM), format!("{at} "))]
    } else if state.header.live {
        vec![
            sp(fg(OK), "● "),
            sp(fg(MUTED), format!("live {}", interval_text(state.interval_ms))),
            sp(fg(DIM), format!("{at} ")),
        ]
    } else {
        vec![sp(fg(DIM), format!("○ offline · disk snapshot{at} "))]
    }
}

fn interval_text(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn brand_line(state: &TopState, width: usize) -> Line<'static> {
    let h = &state.header;
    let mut left = vec![
        sp(fg(ACCENT).add_modifier(Modifier::BOLD), " ◆ "),
        sp(bold(), "pi-famulus"),
        sp(fg(DIM), format!(" {}", h.version)),
    ];
    if let Some(pid) = h.pid {
        left.push(sp(fg(DIM), "   pid "));
        left.push(sp(fg(MUTED), pid.to_string()));
        left.push(sp(fg(DIM), " · up "));
        left.push(sp(fg(MUTED), h.uptime_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into())));
    }
    left.push(sp(fg(DIM), "   sessions "));
    left.push(sp(fg(MUTED), format!("{}", h.sessions_conn)));
    left.push(sp(fg(DIM), format!("/{}", h.sessions_tot)));
    Line::from(split_line(left, live_pill(state), width))
}

/// `▰▰▱▱▱▱▱▱` meter; filled cells in `color`.
fn meter(used: usize, total: usize, cells: usize, color: Color) -> Vec<Span<'static>> {
    let fill = (used * cells).checked_div(total).map_or(0, |_| (used * cells).div_ceil(total)).min(cells);
    vec![sp(fg(color), "▰".repeat(fill)), sp(fg(RULE), "▱".repeat(cells - fill))]
}

/// One bar split into LLM / tool / queue shares.
fn wall_bar(llm: u64, tool: u64, queue: u64, cells: usize) -> Vec<Span<'static>> {
    let total = (llm + tool + queue).max(1);
    let l = (llm as usize * cells + total as usize / 2) / total as usize;
    let t = (tool as usize * cells + total as usize / 2) / total as usize;
    let l = l.min(cells);
    let t = t.min(cells - l);
    let q = if llm + tool + queue == 0 { 0 } else { cells - l - t };
    let rest = cells - l - t - q;
    vec![
        sp(fg(ACCENT), "▰".repeat(l)),
        sp(fg(OK), "▰".repeat(t)),
        sp(fg(WARN), "▰".repeat(q)),
        sp(fg(RULE), "▱".repeat(rest)),
    ]
}

fn stat_row(label: &str, value: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = vec![sp(fg(DIM), format!("   {label:<8}"))];
    out.extend(value);
    out
}

fn val(text: impl Into<String>) -> Span<'static> {
    let t: String = text.into();
    if t == "-" {
        sp(fg(DIM), t)
    } else {
        sp(Style::default(), t)
    }
}

fn unit(text: &str) -> Span<'static> {
    sp(fg(DIM), text.to_string())
}

fn draw_header(frame: &mut Frame, area: Rect, state: &TopState, hclass: HeightClass) {
    let h = &state.header;
    let width = area.width as usize;
    if hclass == HeightClass::Short {
        let mut line2 = vec![sp(fg(DIM), "   agents ")];
        line2.extend(meter(h.agents_used, h.agents_total, 4, ACCENT));
        line2.push(val(format!(" {}/{}", h.agents_used, h.agents_total)));
        line2.push(unit("  running "));
        line2.push(running_count(h.tasks_run));
        line2.push(unit("  cpu "));
        line2.push(val(now_cell(h.cpu_now, false)));
        line2.push(unit("  tok "));
        line2.push(val(compact_num(h.tokens_in.unwrap_or(0))));
        line2.push(unit("/"));
        line2.push(val(compact_num(h.tokens_out.unwrap_or(0))));
        for k in h.work_kinds.iter().take(4) {
            line2.push(unit(format!("  {} ", k.kind).as_str()));
            line2.push(val(format!("{:.0}%", k.cpu_now.unwrap_or(0.0))));
        }
        frame.render_widget(Paragraph::new(vec![brand_line(state, width), Line::from(fit_spans(line2, width))]), area);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    frame.render_widget(Paragraph::new(brand_line(state, width)), rows[0]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(rows[1]);
    let lw = cols[0].width as usize;
    let mut left = Vec::new();
    let mut slots = meter(h.agents_used, h.agents_total, 8, ACCENT);
    slots.push(val(format!(" {}/{}", h.agents_used, h.agents_total)));
    left.push(stat_row("agents", slots));
    left.push(stat_row(
        "tasks",
        vec![
            running_count(h.tasks_run),
            unit(" running · "),
            val(h.tasks_sh.to_string()),
            unit(" shells · "),
            val(h.tasks_ag.to_string()),
            unit(" agents"),
        ],
    ));
    left.push(stat_row(
        "cpu",
        vec![
            val(h.cpu_now.map(|p| format!("{p:.1}%")).unwrap_or_else(|| "-".into())),
            unit(" now · "),
            val(h.cpu_tot.map(fmt::human_duration).unwrap_or_else(|| "-".into())),
            unit(" total"),
        ],
    ));
    left.push(stat_row(
        "tokens",
        vec![
            val(compact_num(h.tokens_in.unwrap_or(0))),
            unit(" in · "),
            val(compact_num(h.tokens_out.unwrap_or(0))),
            unit(" out · "),
            val(h.tok_s.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into())),
            unit(" tok/s"),
        ],
    ));
    let (llm, tool, queue) = (h.llm_share.unwrap_or(0), h.tool_share.unwrap_or(0), h.queue_share.unwrap_or(0));
    let mut wall = wall_bar(llm, tool, queue, 8);
    wall.extend([
        sp(fg(ACCENT), " llm "),
        val(format!("{llm}%")),
        sp(fg(OK), " tool "),
        val(format!("{tool}%")),
        sp(fg(WARN), " queue "),
        val(format!("{queue}%")),
    ]);
    left.push(stat_row("wall", wall));
    let left: Vec<Line> =
        left.into_iter().take(cols[0].height as usize).map(|l| Line::from(fit_spans(l, lw))).collect();
    frame.render_widget(Paragraph::new(left), cols[0]);

    let rw = cols[1].width as usize;
    let mut kind_lines = vec![Line::from(fit_spans(
        vec![sp(
            fg(DIM),
            format!(" {:<11} {:>5} {:>6} {:>7} {:>7} {:>6}", "work kind", "slots", "tasks", "wall", "cpu", "now"),
        )],
        rw,
    ))];
    let room = (cols[1].height as usize).saturating_sub(1);
    let kinds = &h.work_kinds;
    let shown = if kinds.len() > room { room.saturating_sub(1) } else { kinds.len() };
    for k in kinds.iter().take(shown) {
        let busy = k.used > 0;
        let now = k.cpu_now.filter(|p| *p > 0.0);
        let row = vec![
            sp(if busy { Style::default() } else { fg(MUTED) }, format!(" {:<11}", fmt::truncate_width(&k.kind, 11))),
            sp(if busy { fg(ACCENT) } else { fg(DIM) }, format!(" {:>5}", format!("{}/{}", k.used, k.total))),
            val(format!(" {:>5}", k.tasks)),
            val(format!(" {:>7}", fmt::human_duration(k.wall_ms))),
            match k.cpu_ms {
                Some(c) => val(format!(" {:>7}", fmt::human_duration(c))),
                None => sp(fg(DIM), format!(" {:>7}", "-")),
            },
            match now {
                Some(p) => sp(fg(OK), format!(" {:>6}", format!("{p:.1}%"))),
                None => sp(fg(DIM), format!(" {:>6}", "-")),
            },
        ];
        kind_lines.push(Line::from(fit_spans(row, rw)));
    }
    if shown < kinds.len() {
        kind_lines.push(Line::from(sp(fg(DIM), format!(" +{} more", kinds.len() - shown))));
    }
    frame.render_widget(Paragraph::new(kind_lines), cols[1]);
}

fn running_count(n: usize) -> Span<'static> {
    if n > 0 {
        sp(fg(OK).add_modifier(Modifier::BOLD), n.to_string())
    } else {
        sp(fg(DIM), "0")
    }
}

fn tab_bar(state: &TopState, width: usize) -> (Vec<Span<'static>>, Vec<(u16, u16, TabHit)>) {
    let mut spans = vec![Span::raw(" ")];
    let mut hits = Vec::new();
    let mut x = 1usize;
    for tab in [Tab::Tree, Tab::Agents, Tab::Tasks, Tab::Events] {
        let (name, count) = match tab {
            Tab::Tree => ("Tree", Some(state.count_tab(Tab::Tree))),
            Tab::Agents => ("Agents", Some(state.count_tab(Tab::Agents))),
            Tab::Tasks => ("Tasks", Some(state.count_tab(Tab::Tasks))),
            Tab::Events => ("Events", None),
        };
        let active = state.tab == tab;
        let start = x;
        let name_style =
            if active { fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { fg(MUTED) };
        spans.push(sp(name_style, name));
        x += fmt::display_width(name);
        if let Some(c) = count {
            let t = format!(" {c}");
            x += t.len();
            spans.push(sp(if active { fg(ACCENT) } else { fg(DIM) }, t));
        }
        hits.push((start as u16, x as u16, TabHit::Tab(tab)));
        spans.push(Span::raw("   "));
        x += 3;
    }
    let scope = state.scope.label().to_lowercase();
    let sort = format!("{} {}", state.sort.label().to_lowercase(), if state.sort_desc { "↓" } else { "↑" });
    let right = vec![
        sp(fg(DIM), "scope "),
        sp(fg(MUTED), scope.clone()),
        sp(fg(DIM), "   sort "),
        sp(fg(MUTED), sort.clone()),
        Span::raw(" "),
    ];
    let rw = spans_width(&right);
    if x + rw + 2 <= width {
        let rx = width - rw;
        let scope_end = rx + "scope ".len() + scope.len();
        hits.push((rx as u16, scope_end as u16, TabHit::Scope));
        hits.push((scope_end as u16, width as u16, TabHit::Sort));
    }
    (split_line(spans, right, width), hits)
}

fn draw_workspace(frame: &mut Frame, area: Rect, state: &mut TopState, wclass: WidthClass) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    let (bar, hits) = tab_bar(state, chunks[0].width as usize);
    state.mouse.tabs = hit_rect(chunks[0]);
    state.mouse.tab_hits = hits;
    frame.render_widget(Paragraph::new(Line::from(bar)), chunks[0]);

    if state.tab == Tab::Events {
        draw_events(frame, chunks[1], state);
        return;
    }
    let cols = match state.tab {
        Tab::Agents => agent_cols(wclass),
        Tab::Tasks => task_cols(wclass),
        _ => tree_cols(wclass),
    };
    draw_table(frame, chunks[1], state, cols);
}

fn hit_rect(area: Rect) -> HitRect {
    HitRect::from_xywh(area.x, area.y, area.width, area.height)
}

/// Selection marker that leads each row; `▌` in the accent when selected.
fn marker(selected: bool) -> Span<'static> {
    if selected {
        sp(fg(ACCENT), "▌")
    } else {
        Span::raw(" ")
    }
}

fn draw_table(frame: &mut Frame, area: Rect, state: &mut TopState, cols: &[Col]) {
    state.mouse.table = hit_rect(area);
    let page = area.height.saturating_sub(1) as usize;
    state.ensure_scroll(page);
    let widths = col_widths(cols, area.width);
    let header = Row::new(cols.iter().enumerate().map(|(i, c)| {
        let label = if i == 0 { format!(" {}", c.header().to_lowercase()) } else { c.header().to_lowercase() };
        Cell::from(Line::from(sp(fg(DIM), fmt::truncate_width(&label, widths[i] as usize))))
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
                .map(|(i, c)| {
                    let w = widths[i] as usize;
                    let mut spans = Vec::new();
                    let mut room = w;
                    if i == 0 {
                        spans.push(marker(selected));
                        room = w.saturating_sub(1);
                    }
                    spans.extend(fit_spans(cell_spans(state, vr, *c), room));
                    Cell::from(Line::from(spans))
                })
                .collect();
            let row = Row::new(cells);
            if selected {
                row.style(Style::default().bg(SEL_BG).fg(TEXT))
            } else {
                row
            }
        })
        .collect();
    let constraints: Vec<Constraint> = widths.iter().map(|w| Constraint::Length(*w)).collect();
    frame.render_widget(Table::new(rows, constraints).header(header), area);
}

fn event_color(ty: &str) -> Color {
    if ty.contains("exit") || ty.contains("settle") {
        OK
    } else if ty.starts_with("task.") {
        ACCENT
    } else if ty.starts_with("agent.") {
        Color::Rgb(186, 150, 240)
    } else if ty.starts_with("wake.") {
        WARN
    } else {
        MUTED
    }
}

fn draw_events(frame: &mut Frame, area: Rect, state: &mut TopState) {
    state.mouse.table = hit_rect(area);
    let page = area.height.saturating_sub(1) as usize;
    state.ensure_scroll(page);
    let start = state.scroll;
    let end = (start + page).min(state.visible.len());
    let sel = state.selected.clone();
    let width = area.width as usize;
    let mut lines = vec![Line::from(fit_spans(
        vec![sp(
            fg(DIM),
            format!("  {:<19} {:<9} {:<9} {:<18} {:<12} {}", "time", "session", "source", "type", "id", "detail"),
        )],
        width,
    ))];
    for vr in &state.visible[start..end] {
        let Some(&i) = state.event_index.get(&vr.id) else {
            continue;
        };
        let ev = &state.events[i];
        let selected = sel.as_deref() == Some(vr.id.as_str());
        let spans = vec![
            marker(selected),
            Span::raw(" "),
            sp(fg(DIM), format!("{:<19} ", fmt::datetime_ms(ev.ts))),
            sp(fg(MUTED), format!("{:<9} ", ev.session)),
            sp(fg(MUTED), format!("{:<9} ", ev.src)),
            sp(fg(event_color(&ev.ty)), format!("{:<18} ", ev.ty)),
            sp(Style::default(), format!("{:<12} ", ev.id)),
            sp(fg(MUTED), ev.summary.clone()),
        ];
        let mut line = Line::from(fit_spans(spans, width));
        if selected {
            line = line.style(Style::default().bg(SEL_BG).fg(TEXT));
            let pad = width.saturating_sub(line.width());
            line.spans.push(Span::raw(" ".repeat(pad)));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// `── label ─────` rule across `width`.
fn rule(label: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut spans = vec![sp(fg(RULE), "── ")];
    spans.extend(label);
    spans.push(Span::raw(" "));
    let used = spans_width(&spans);
    if used < width {
        spans.push(sp(fg(RULE), "─".repeat(width - used)));
    }
    Line::from(fit_spans(spans, width))
}

fn draw_preview(frame: &mut Frame, area: Rect, state: &TopState) {
    let width = area.width as usize;
    let label = match state.selected_node() {
        Some(n) => {
            vec![sp(fg(MUTED), "preview "), sp(Style::default(), n.label.clone()), sp(fg(DIM), format!("  {}", n.id))]
        }
        None => vec![sp(fg(MUTED), "preview")],
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    frame.render_widget(Paragraph::new(rule(label, width)), rows[0]);
    let body = rows[1];
    let painted = layout_detail(&state.preview_body, body.width as usize, &HashSet::new(), false);
    let start = painted.len().saturating_sub(body.height as usize);
    let lines: Vec<Line> =
        painted[start..].iter().map(|p| Line::from(fit_spans(p.spans.clone(), body.width as usize))).collect();
    frame.render_widget(Paragraph::new(lines), body);
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &TopState) {
    let width = area.width as usize;
    let spans = if let Some(id) = &state.confirm_stop {
        let mut s = vec![sp(fg(ERR).add_modifier(Modifier::BOLD), format!(" ■ stop {id}?  "))];
        s.extend(hints(&[("y", "confirm"), ("n", "cancel")]));
        s
    } else if state.filter_editing {
        let mut s = vec![
            sp(fg(ACCENT).add_modifier(Modifier::BOLD), " / "),
            sp(Style::default(), state.filter.clone()),
            sp(fg(ACCENT), "▏"),
            Span::raw("   "),
        ];
        s.extend(hints(&[("enter", "apply"), ("esc", "clear")]));
        s
    } else {
        let mut left = vec![Span::raw(" ")];
        if !state.filter.is_empty() {
            left.push(sp(fg(ACCENT), "/ "));
            left.push(sp(Style::default(), state.filter.clone()));
            left.push(Span::raw("   "));
        }
        left.extend(hints(&[
            ("↑↓", "move"),
            ("←→", "fold"),
            ("⏎", "open"),
            ("v", "preview"),
            ("a", "scope"),
            ("s", "sort"),
            ("/", "filter"),
            ("x", "stop"),
            ("?", "help"),
            ("q", "quit"),
        ]));
        let right = match &state.message {
            Some(m) => vec![sp(fg(WARN), format!("{m} "))],
            None => Vec::new(),
        };
        if right.is_empty() {
            left
        } else if spans_width(&left) + spans_width(&right) + 2 > width {
            let mut s = vec![Span::raw(" ")];
            s.extend(right);
            s
        } else {
            split_line(left, right, width)
        }
    };
    frame.render_widget(Paragraph::new(Line::from(fit_spans(spans, width))), area);
}

const HELP: &[(&str, &str)] = &[
    ("1 2 3 4  Tab", "switch view"),
    ("↑↓  j k", "move"),
    ("←→  h l", "fold / unfold"),
    ("z", "expand all"),
    ("g G  Home End", "top / bottom"),
    ("Enter", "open detail"),
    ("v", "preview pane"),
    ("e", "events for selection"),
    ("a", "cycle scope"),
    ("s S  < >", "cycle sort"),
    ("I", "invert sort"),
    ("/", "filter"),
    ("p  Space", "pause"),
    ("+ -", "refresh interval"),
    ("x  Ctrl+X  F9", "stop task"),
    ("o  click ▸", "unfold tools & thinking"),
    ("Esc", "close overlay"),
    ("q  Ctrl+C", "quit"),
];

fn panel(title: &str) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded).border_style(fg(RULE)).title(Line::from(vec![
        sp(fg(ACCENT).add_modifier(Modifier::BOLD), " ◆ "),
        sp(fg(Color::Reset).add_modifier(Modifier::BOLD), format!("{title} ")),
    ]))
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let rect = centered(area, 50, HELP.len() as u16 + 4);
    frame.render_widget(Clear, rect);
    let block = panel("keys");
    let inner = block.inner(rect).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, rect);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, d)| {
            Line::from(fit_spans(
                vec![sp(fg(ACCENT), format!("{:<16}", k)), sp(fg(MUTED), d.to_string())],
                inner.width as usize,
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── detail overlay ─────────────────────────────────────────────────────────

fn detail_tabs_line(d: &model::Detail) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, t) in d.tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(sp(fg(DIM), format!("{} ", i + 1)));
        if i == d.tab {
            spans.push(sp(fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED), t.clone()));
        } else {
            spans.push(sp(fg(MUTED), t.clone()));
        }
    }
    Line::from(spans)
}

fn draw_detail(frame: &mut Frame, area: Rect, d: &mut model::Detail, mouse: &mut model::MouseHit) {
    // Full screen: the table keys in the main footer do not apply here.
    let rect = area;
    frame.render_widget(Clear, rect);
    let roomy = rect.height >= 18;
    let inner = Block::bordered().inner(rect).inner(Margin { horizontal: 1, vertical: if roomy { 1 } else { 0 } });
    let chat = match &d.body {
        DetailBody::Conversation(c) => Some((c.stats.clone(), c.running)),
        _ => None,
    };
    let tabs_h = if d.tabs.is_empty() { 0 } else { 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tabs_h),
            Constraint::Min(1),
            Constraint::Length(if roomy { 1 } else { 0 }),
            Constraint::Length(1),
        ])
        .split(inner);
    if tabs_h > 0 {
        frame.render_widget(Paragraph::new(detail_tabs_line(d)), chunks[0]);
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
    let lines: Vec<Line> =
        painted[start..].iter().take(height).map(|p| Line::from(fit_spans(p.spans.clone(), width))).collect();

    let scrolled = painted.len() > height && !d.follow;
    let pos = if scrolled {
        let max = painted.len() - height;
        vec![sp(fg(DIM), format!(" {}% ", d.scroll * 100 / max.max(1))), sp(fg(RULE), "─")]
    } else if d.follow && chat.as_ref().is_some_and(|(_, running)| *running) {
        vec![sp(fg(OK), " ● "), sp(fg(MUTED), "following "), sp(fg(RULE), "─")]
    } else {
        Vec::new()
    };
    let block = panel(&d.title).title_top(Line::from(pos).right_aligned());
    frame.render_widget(block, rect);

    let chat_rect = Rect { x: body.x, y: body.y, width: text_w, height: body.height };
    let bar = Rect { x: body.x + text_w + gap, y: body.y, width: bar_w, height: body.height };
    mouse.detail = hit_rect(rect);
    mouse.chat = hit_rect(chat_rect);
    mouse.scrollbar = hit_rect(bar);
    frame.render_widget(Paragraph::new(lines), chat_rect);
    draw_scrollbar(frame, bar, painted.len(), height, d.scroll);

    let foot_w = chunks[3].width as usize;
    let mut left = Vec::new();
    let keys: Vec<(&str, &str)> = match &chat {
        Some((stats, running)) => {
            if *running {
                left.push(sp(fg(ACCENT), format!("{} ", spin_glyph())));
                left.push(sp(fg(ACCENT).add_modifier(Modifier::ITALIC), "working   "));
            }
            for (i, (v, l)) in stats.iter().enumerate() {
                if i > 0 {
                    left.push(sp(fg(RULE), " · "));
                }
                left.push(val(v.clone()));
                left.push(unit(&format!(" {l}")));
            }
            vec![("o", if d.tools_expanded { "fold all" } else { "unfold all" }), ("esc", "close")]
        }
        None => {
            if d.tabs.len() > 1 {
                vec![("1-3", "tab"), ("f", if d.follow { "unfollow" } else { "follow" }), ("esc", "close")]
            } else {
                vec![("f", if d.follow { "unfollow" } else { "follow" }), ("esc", "close")]
            }
        }
    };
    let right = hints(&keys);
    let line = if left.is_empty() {
        let mut l = vec![Span::raw("")];
        let pad = foot_w.saturating_sub(spans_width(&right));
        l.push(Span::raw(" ".repeat(pad)));
        l.extend(right);
        fit_spans(l, foot_w)
    } else {
        split_line(left, right, foot_w)
    };
    frame.render_widget(Paragraph::new(Line::from(line)), chunks[3]);
}

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const SPIN_MS: u128 = 80;
/// Left margin for assistant text and activity rows; the user rail sits in it.
const MARGIN: usize = 2;
/// Unfolded tool output / thinking hangs off a rule at this column.
const HANG: usize = MARGIN + 2;

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
            let style = fg(MUTED);
            Line::from(Span::styled(c.to_string(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

const THUMB: char = '┃';

/// Thumb glyphs top to bottom; the track itself is blank so it does not
/// double the panel border.
fn scrollbar_marks(len: usize, height: usize, scroll: usize, track: usize) -> Vec<char> {
    if track == 0 {
        return Vec::new();
    }
    if len <= height {
        return vec![' '; track];
    }
    let thumb = ((height * track) / len).max(1).min(track);
    let max_scroll = len - height;
    let pos = (scroll * (track - thumb) + max_scroll / 2).checked_div(max_scroll).unwrap_or(0);
    let pos = pos.min(track - thumb);
    (0..track).map(|i| if i >= pos && i < pos + thumb { THUMB } else { ' ' }).collect()
}

// ── chat layout ────────────────────────────────────────────────────────────

/// One painted row of the detail body.
#[derive(Debug, Clone, PartialEq)]
struct Painted {
    spans: Vec<Span<'static>>,
    /// Click target for a fold chevron on this row, relative to the pane.
    fold: Option<FoldHit>,
}

impl Painted {
    fn new(spans: Vec<Span<'static>>) -> Painted {
        Painted { spans, fold: None }
    }

    fn blank() -> Painted {
        Painted::new(Vec::new())
    }

    #[cfg(test)]
    fn text(&self) -> String {
        self.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}

fn margin() -> Span<'static> {
    Span::raw(" ".repeat(MARGIN))
}

fn dim_line(text: &str) -> Painted {
    Painted::new(vec![margin(), sp(fg(DIM), text.to_string())])
}

fn layout_detail(body: &DetailBody, width: usize, open: &HashSet<usize>, expand_all: bool) -> Vec<Painted> {
    match body {
        DetailBody::Empty => vec![dim_line("(nothing to show)")],
        DetailBody::Conversation(chat) => layout_conversation(chat, width, open, expand_all),
        DetailBody::Log { header, lines } => layout_log(header, lines, width),
        DetailBody::Text(lines) => layout_info(lines, width),
    }
}

fn layout_log(header: &[String], lines: &[String], width: usize) -> Vec<Painted> {
    let mut out = Vec::new();
    let inner = width.saturating_sub(MARGIN).max(1);
    for (i, h) in header.iter().enumerate() {
        if let Some(cmd) = h.strip_prefix('$') {
            for (j, l) in fmt::wrap_width(cmd.trim_start(), inner.saturating_sub(2).max(1)).into_iter().enumerate() {
                let lead = if j == 0 { sp(fg(ACCENT).add_modifier(Modifier::BOLD), "$ ") } else { Span::raw("  ") };
                out.push(Painted::new(vec![margin(), lead, sp(bold(), l)]));
            }
        } else {
            let style = if i == 0 { fg(MUTED) } else { fg(DIM) };
            for l in fmt::wrap_width(h, inner) {
                out.push(Painted::new(vec![margin(), sp(style, l)]));
            }
        }
    }
    if !header.is_empty() {
        out.push(Painted::blank());
    }
    for line in lines {
        for l in fmt::wrap_width(line, inner) {
            out.push(Painted::new(vec![margin(), sp(Style::default(), l)]));
        }
    }
    if lines.is_empty() {
        out.push(dim_line("(empty)"));
    }
    out
}

/// `key      value` rows: key dim, value bright.
fn layout_info(lines: &[String], width: usize) -> Vec<Painted> {
    let mut out = Vec::new();
    let inner = width.saturating_sub(MARGIN).max(1);
    for line in lines {
        let err = line.starts_with("error") || line.contains(" error");
        let (key, value) = match line.find("  ") {
            Some(i) if i > 0 && !line[..i].contains(' ') => {
                let rest = line[i..].trim_start();
                let kw = line.len() - rest.len();
                (&line[..kw], rest)
            }
            _ => ("", line.as_str()),
        };
        let vstyle = if err { fg(ERR) } else { Style::default() };
        let kw = fmt::display_width(key);
        for (j, l) in fmt::wrap_width(value, inner.saturating_sub(kw).max(1)).into_iter().enumerate() {
            let k = if j == 0 { sp(fg(DIM), key.to_string()) } else { Span::raw(" ".repeat(kw)) };
            out.push(Painted::new(vec![margin(), k, sp(vstyle, l)]));
        }
    }
    if out.is_empty() {
        out.push(dim_line("(empty)"));
    }
    out
}

fn is_open(id: usize, open: &HashSet<usize>, expand_all: bool) -> bool {
    expand_all || open.contains(&id)
}

/// Spacing groups: consecutive activity rows (tools, thinking) stack tight;
/// any other change of block kind gets one blank line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    User,
    Text,
    Activity,
}

struct Thread {
    out: Vec<Painted>,
    last: Option<Group>,
    fold: usize,
}

impl Thread {
    fn begin(&mut self, kind: Group) {
        if self.last.is_some() && !(kind == Group::Activity && self.last == Some(Group::Activity)) {
            self.out.push(Painted::blank());
        }
        self.last = Some(kind);
    }

    fn next_fold(&mut self) -> usize {
        let id = self.fold;
        self.fold += 1;
        id
    }
}

fn layout_conversation(chat: &Chat, width: usize, open: &HashSet<usize>, expand_all: bool) -> Vec<Painted> {
    if chat.turns.is_empty() {
        return vec![dim_line("(no conversation yet)")];
    }
    let mut t = Thread { out: Vec::new(), last: None, fold: 0 };
    let mut i = 0;
    while i < chat.turns.len() {
        let turn = &chat.turns[i];
        if turn.role == "user" {
            t.begin(Group::User);
            user_block(&turn.text, turn.ts, width, &mut t.out);
            i += 1;
            continue;
        }
        if turn.role == "thinking" {
            t.begin(Group::Activity);
            let id = t.next_fold();
            thinking_row(&turn.text, turn.ts, is_open(id, open, expand_all), id, width, &mut t.out);
            i += 1;
            continue;
        }
        if turn.tool.is_some() {
            t.begin(Group::Activity);
            let title = tool_card_title(turn, i.checked_sub(1).and_then(|j| chat.turns.get(j)));
            let id = t.next_fold();
            let (name, summary) = split_title(&title);
            tool_row(
                ToolRow { name, summary, output: &turn.text, ts: turn.ts, error: turn.error, pending: false },
                is_open(id, open, expand_all),
                id,
                width,
                &mut t.out,
            );
            i += 1;
            continue;
        }
        if turn.role == "assistant" || turn.role == "assistant error" {
            let error = turn.error || turn.role == "assistant error";
            let segs = assistant_segments(&turn.text);
            let has_tools = segs.iter().any(|s| matches!(s, Seg::ToolCall { .. }));
            let mut seen_tool = false;
            i += 1;
            for seg in segs {
                match seg {
                    Seg::Text(text) => {
                        if has_tools && !seen_tool {
                            t.begin(Group::Activity);
                            let id = t.next_fold();
                            thinking_row(&text, turn.ts, is_open(id, open, expand_all), id, width, &mut t.out);
                        } else {
                            t.begin(Group::Text);
                            assistant_text(&text, error, width, &mut t.out);
                        }
                    }
                    Seg::ToolCall { name, summary } => {
                        seen_tool = true;
                        t.begin(Group::Activity);
                        let id = t.next_fold();
                        let paired = chat.turns.get(i).is_some_and(|n| n.tool.as_deref() == Some(name.as_str()));
                        let row = if paired {
                            let result = &chat.turns[i];
                            i += 1;
                            ToolRow {
                                name: &name,
                                summary: &summary,
                                output: &result.text,
                                ts: result.ts.or(turn.ts),
                                error: result.error,
                                pending: false,
                            }
                        } else {
                            ToolRow {
                                name: &name,
                                summary: &summary,
                                output: "",
                                ts: turn.ts,
                                error: false,
                                pending: true,
                            }
                        };
                        tool_row(row, is_open(id, open, expand_all), id, width, &mut t.out);
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    if t.out.is_empty() {
        t.out.push(dim_line("(no conversation yet)"));
    }
    t.out
}

fn clock(ts: Option<u64>) -> String {
    ts.map(fmt::datetime)
        .and_then(|s| s.split(' ').nth(1).map(|t| t.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

/// User turn: an accent rail down the left edge, text beside it, clock on
/// the first line.
fn user_block(text: &str, ts: Option<u64>, width: usize, out: &mut Vec<Painted>) {
    let time = clock(ts);
    let reserve = if time.is_empty() { 0 } else { time.len() + 2 };
    let inner = width.saturating_sub(MARGIN + reserve).max(1);
    let body = if text.trim().is_empty() { " " } else { text.trim() };
    for (i, l) in fmt::wrap_width(body, inner).into_iter().enumerate() {
        let mut spans = vec![sp(fg(ACCENT), "┃ "), sp(Style::default(), l)];
        if i == 0 && !time.is_empty() {
            spans = split_line(spans, vec![sp(fg(DIM), time.clone())], width);
        }
        out.push(Painted::new(spans));
    }
}

/// Rows hanging off the `│` rule under a fold chevron.
fn hang(text: &str, style: Style, width: usize, out: &mut Vec<Painted>) {
    let inner = width.saturating_sub(HANG).max(1);
    for l in fmt::wrap_width(text, inner) {
        out.push(Painted::new(vec![margin(), sp(fg(RULE), "│ "), sp(style, l)]));
    }
}

fn chevron(open: bool) -> &'static str {
    if open {
        "▾"
    } else {
        "▸"
    }
}

fn fold_hit(id: usize) -> FoldHit {
    // Chevron, space, glyph.
    FoldHit { id, x0: MARGIN as u16, x1: MARGIN as u16 + 3 }
}

fn thinking_row(text: &str, ts: Option<u64>, expanded: bool, fold: usize, width: usize, out: &mut Vec<Painted>) {
    let style = fg(DIM).add_modifier(Modifier::ITALIC);
    let left =
        vec![margin(), sp(fg(DIM), format!("{} ", chevron(expanded))), sp(fg(MUTED), "∴ "), sp(style, "Thinking")];
    let right = match clock(ts) {
        t if t.is_empty() => Vec::new(),
        t => vec![sp(fg(DIM), t)],
    };
    out.push(Painted { spans: split_line(left, right, width), fold: Some(fold_hit(fold)) });
    if expanded {
        hang(&fmt::scrub_tty(text.trim()), style, width, out);
    }
}

struct ToolRow<'a> {
    name: &'a str,
    summary: &'a str,
    output: &'a str,
    ts: Option<u64>,
    error: bool,
    pending: bool,
}

fn split_title(title: &str) -> (&str, &str) {
    match title.split_once(' ') {
        Some((n, s)) => (n, s.trim()),
        None => (title, ""),
    }
}

/// `▸ ✓ bash  rg loader                     12:01:03`
fn tool_row(row: ToolRow, expanded: bool, fold: usize, width: usize, out: &mut Vec<Painted>) {
    let has_body = !row.pending && !row.output.trim().is_empty();
    let (glyph, gstyle) = if row.pending {
        (spin_glyph(), fg(ACCENT))
    } else if row.error {
        ("✗", fg(ERR))
    } else {
        ("✓", fg(OK))
    };
    let mark = if has_body { chevron(expanded) } else { " " };
    let mut left = vec![
        margin(),
        sp(fg(DIM), format!("{mark} ")),
        sp(gstyle, format!("{glyph} ")),
        sp(bold(), row.name.to_string()),
    ];
    if !row.summary.is_empty() {
        left.push(sp(fg(MUTED), format!("  {}", fmt::first_line(row.summary))));
    }
    let time = clock(row.ts);
    let right = if time.is_empty() { Vec::new() } else { vec![sp(fg(DIM), time.clone())] };
    let room = if time.is_empty() { width } else { width.saturating_sub(time.len() + 2) };
    let spans = split_line(fit_spans(left, room), right, width);
    out.push(Painted { spans, fold: has_body.then(|| fold_hit(fold)) });
    if has_body && expanded {
        let style = if row.error { fg(ERR) } else { fg(MUTED) };
        hang(&fmt::scrub_tty(row.output.trim_end()), style, width, out);
    }
}

fn assistant_text(text: &str, error: bool, width: usize, out: &mut Vec<Painted>) {
    if error {
        let inner = width.saturating_sub(MARGIN + 2).max(1);
        for (i, l) in fmt::wrap_width(text.trim(), inner).into_iter().enumerate() {
            let lead = if i == 0 { sp(fg(ERR), "✗ ") } else { Span::raw("  ") };
            out.push(Painted::new(vec![margin(), lead, sp(fg(ERR), l)]));
        }
        return;
    }
    markdown(text.trim(), width, out);
}

/// Just enough markdown for agent prose: headings, bullets, quotes, fenced
/// code, `inline code`, and **bold**.
fn markdown(text: &str, width: usize, out: &mut Vec<Painted>) {
    let inner = width.saturating_sub(MARGIN).max(1);
    let mut fence = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            for l in fmt::wrap_width(&fmt::scrub_tty(raw), inner.saturating_sub(2).max(1)) {
                out.push(Painted::new(vec![margin(), sp(fg(RULE), "│ "), sp(fg(CODE), l)]));
            }
            continue;
        }
        if raw.trim().is_empty() {
            out.push(Painted::blank());
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && trimmed[hashes..].starts_with(' ') {
            let title = trimmed[hashes..].trim();
            for l in fmt::wrap_width(title, inner) {
                out.push(Painted::new(vec![margin(), sp(bold(), l)]));
            }
            continue;
        }
        let indent = raw.len() - trimmed.len();
        let (lead, rest, lead_style): (String, &str, Style) =
            if let Some(r) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
                (format!("{}• ", " ".repeat(indent)), r, fg(MUTED))
            } else if let Some(r) = trimmed.strip_prefix("> ") {
                ("│ ".into(), r, fg(RULE))
            } else if let Some(n) = ordered_prefix(trimmed) {
                (format!("{}{}", " ".repeat(indent), &trimmed[..n]), &trimmed[n..], fg(MUTED))
            } else {
                (" ".repeat(indent.min(8)), trimmed, Style::default())
            };
        let quote = lead.starts_with('│');
        let lw = fmt::display_width(&lead);
        let base = if quote { fg(MUTED) } else { Style::default() };
        let mut st = Inline::default();
        for (i, l) in fmt::wrap_width(rest, inner.saturating_sub(lw).max(1)).into_iter().enumerate() {
            let mut spans = vec![margin()];
            if i == 0 || quote {
                spans.push(sp(lead_style, lead.clone()));
            } else {
                spans.push(Span::raw(" ".repeat(lw)));
            }
            spans.extend(inline_spans(&l, base, &mut st));
            out.push(Painted::new(spans));
        }
    }
}

fn ordered_prefix(s: &str) -> Option<usize> {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &s[digits..];
    (rest.starts_with(". ") || rest.starts_with(") ")).then_some(digits + 2)
}

#[derive(Default)]
struct Inline {
    code: bool,
    bold: bool,
}

/// Split one wrapped line on backticks and `**`, carrying state across the
/// wrapped lines of a paragraph.
fn inline_spans(line: &str, base: Style, st: &mut Inline) -> Vec<Span<'static>> {
    let style = |st: &Inline| {
        if st.code {
            fg(CODE)
        } else if st.bold {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    let mut spans = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        let toggle = if c == '`' {
            Some(true)
        } else if c == '*' && !st.code && chars.peek() == Some(&'*') {
            chars.next();
            Some(false)
        } else {
            None
        };
        match toggle {
            Some(code) => {
                if !cur.is_empty() {
                    spans.push(sp(style(st), std::mem::take(&mut cur)));
                }
                if code {
                    st.code = !st.code;
                } else {
                    st.bold = !st.bold;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        spans.push(sp(style(st), cur));
    }
    spans
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
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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

fn tool_call_summary(text: &str) -> Option<(String, String)> {
    parse_tool_call_at(text.trim()).map(|(_, name, summary)| (name, summary))
}

fn status_color(status: &str, running: bool) -> Color {
    if running || status == "running" || status == "pending" {
        OK
    } else if matches!(status, "failed" | "killed" | "orphaned" | "interrupted") {
        ERR
    } else if status == "connected" {
        ACCENT
    } else if matches!(status, "timeout" | "stopped" | "partial") {
        WARN
    } else {
        DIM
    }
}

fn status_glyph(status: &str, running: bool) -> &'static str {
    if running || status == "running" || status == "pending" || status == "connected" {
        "●"
    } else if matches!(status, "failed" | "killed" | "orphaned" | "interrupted") {
        "✗"
    } else if status == "completed" {
        "✓"
    } else {
        "○"
    }
}

/// Styled cell: the tree label gets chevron + status glyph, status words are
/// colored, and placeholders are dim.
fn cell_spans(state: &TopState, vr: &VisibleRow, col: Col) -> Vec<Span<'static>> {
    if state.tab != Tab::Tasks || !state.task_index.contains_key(&vr.id) {
        if let Some(n) = state.nodes.get(&vr.id) {
            if matches!(col, Col::Node | Col::Id) {
                return node_spans(vr, n);
            }
            if col == Col::Status {
                return vec![sp(fg(status_color(&n.status, n.metrics.running)), n.status.clone())];
            }
        }
    }
    let text = cell_text(state, vr, col);
    let style = if text == "-" {
        fg(DIM)
    } else {
        match col {
            Col::Status => fg(status_color(&text, false)),
            Col::Id | Col::Sess | Col::Kind | Col::WorkKind | Col::CwdModel | Col::Cwd | Col::Reason | Col::Sample => {
                fg(MUTED)
            }
            Col::Now if text != "0.0%" => fg(OK),
            _ => Style::default(),
        }
    };
    vec![sp(style, text)]
}

/// Running tasks and agents spin; everything else keeps its status glyph.
fn row_glyph(n: &Node) -> &'static str {
    if n.metrics.running && matches!(n.kind, NodeKind::Task | NodeKind::Agent) {
        spin_glyph()
    } else {
        status_glyph(&n.status, n.metrics.running)
    }
}

fn node_spans(vr: &VisibleRow, n: &Node) -> Vec<Span<'static>> {
    let chevron = if vr.has_hidden_children {
        if vr.expanded {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        "  "
    };
    let label_style = match n.kind {
        NodeKind::Session => bold(),
        NodeKind::Main | NodeKind::Run => fg(MUTED),
        _ => Style::default(),
    };
    vec![
        Span::raw("  ".repeat(vr.depth)),
        sp(fg(DIM), chevron),
        sp(fg(status_color(&n.status, n.metrics.running)), format!("{} ", row_glyph(n))),
        sp(label_style, n.label.clone()),
    ]
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
        Col::Node | Col::Id => n.label.clone(),
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

fn inspect_cores(cpu: Option<u64>, wall: u64) -> String {
    crate::inspect::cores_text(cpu, wall)
}

fn col_widths(cols: &[Col], total: u16) -> Vec<u16> {
    let fixed: u16 = cols.iter().filter_map(|c| c.width()).sum();
    let gaps = cols.len().saturating_sub(1) as u16;
    let flex = total.saturating_sub(fixed.saturating_add(gaps)).max(8);
    cols.iter().map(|c| c.width().unwrap_or(flex)).collect()
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
        TranscriptTurn { role: role.into(), tool: None, args: None, text: text.into(), ts: None, error: false }
    }

    fn tool_turn(name: &str, args: Option<&str>, text: &str) -> TranscriptTurn {
        TranscriptTurn {
            role: "tool".into(),
            tool: Some(name.into()),
            args: args.map(|a| a.into()),
            text: text.into(),
            ts: None,
            error: false,
        }
    }

    fn chat_body(turns: Vec<TranscriptTurn>) -> DetailBody {
        DetailBody::Conversation(Chat {
            turns,
            stats: vec![("-".into(), "in".into()), ("0".into(), "shells".into())],
            running: false,
        })
    }

    fn layout(body: &DetailBody, width: usize, expand_all: bool) -> Vec<Painted> {
        layout_detail(body, width, &HashSet::new(), expand_all)
    }

    fn texts(painted: &[Painted]) -> Vec<String> {
        painted.iter().map(|p| p.text()).collect()
    }

    fn assert_fits(painted: &[Painted], width: usize) {
        for p in painted {
            assert!(fmt::display_width(&p.text()) <= width, "too wide: {:?}", p.text());
        }
    }

    #[test]
    fn conversation_is_a_thread_not_role_headers() {
        let turns = vec![
            mk_turn("user", "inspect the loader please and then report back"),
            mk_turn("assistant", r#"tool bash {"command":"rg loader"}"#),
            tool_turn("bash", None, "src/loader.rs\nsrc/main.rs"),
            mk_turn("assistant", "Found it."),
        ];
        let painted = layout(&chat_body(turns), 40, false);
        let text = texts(&painted);
        assert_fits(&painted, 40);
        assert!(!text.iter().any(|l| l.trim_start().starts_with("── ")), "{text:?}");
        let user: Vec<_> = text.iter().filter(|l| l.starts_with("┃ ")).collect();
        assert!(user.len() >= 2, "user turn wraps on a rail: {text:?}");
        assert!(user.iter().any(|l| l.contains("inspect the loader")), "{text:?}");
        let tool = text.iter().find(|l| l.contains("bash")).expect("tool row");
        assert!(tool.contains('✓') && tool.contains("rg loader"), "{tool}");
        assert!(!text.iter().any(|l| l.contains("src/loader.rs")), "output folded: {text:?}");
        assert!(text.iter().any(|l| l == "  Found it."), "{text:?}");
        let rail = painted.iter().find(|p| p.text().starts_with('┃')).unwrap();
        assert_eq!(rail.spans[0].style.fg, Some(ACCENT));
    }

    #[test]
    fn stacked_tools_are_tight_one_line_rows() {
        let long = "x".repeat(80);
        let args = format!(r#"{{"command":"{long}"}}"#);
        let tools: Vec<TranscriptTurn> = (0..3).map(|_| tool_turn("bash", Some(&args), "ok")).collect();
        let painted = layout(&chat_body(tools), 48, false);
        assert_fits(&painted, 48);
        assert_eq!(painted.len(), 3, "no gaps between activity rows: {:?}", texts(&painted));
        for p in &painted {
            let t = p.text();
            assert!(t.starts_with("  ▸ ✓ bash"), "{t:?}");
            assert!(t.ends_with('…'), "summary truncates: {t:?}");
            assert_eq!(p.fold, Some(FoldHit { id: p.fold.unwrap().id, x0: MARGIN as u16, x1: MARGIN as u16 + 3 }));
        }
    }

    #[test]
    fn blocks_of_different_kinds_get_one_blank_line() {
        let turns = vec![
            mk_turn("user", "go"),
            mk_turn("thinking", "plan"),
            tool_turn("read", Some(r#"{"path":"a.rs"}"#), "body"),
            mk_turn("assistant", "done"),
        ];
        let text = texts(&layout(&chat_body(turns), 60, false));
        assert_eq!(text.len(), 6, "{text:?}");
        assert!(text[0].starts_with("┃ go"));
        assert_eq!(text[1], "");
        assert!(text[2].contains("Thinking"));
        assert!(text[3].contains("read") && text[3].contains("a.rs"));
        assert_eq!(text[4], "");
        assert_eq!(text[5], "  done");
    }

    #[test]
    fn expanded_tool_output_hangs_off_a_rule_and_is_not_a_toggle_target() {
        let turn =
            tool_turn("bash", Some(r#"{"command":"git log"}"#), "\x1b[32mcdcdcfe\x1b[0m ok\rRebasing (2/2)\nDone");
        let painted = layout(&chat_body(vec![turn]), 48, true);
        let text = texts(&painted).join("\n");
        assert!(!text.contains('\x1b'), "{text}");
        assert!(!text.contains("[32m"), "{text}");
        assert!(text.contains("Rebasing (2/2)"), "{text}");
        assert!(!text.contains("Rebasing (1/"), "{text}");
        assert!(text.contains("▾"), "{text}");
        assert_eq!(painted.iter().filter(|p| p.fold.is_some()).count(), 1, "only the chevron row is clickable");
        let body: Vec<_> = painted.iter().skip(1).map(|p| p.text()).collect();
        assert!(body.iter().all(|l| l.starts_with("  │ ")), "{body:?}");
    }

    #[test]
    fn write_tool_call_does_not_dump_file_content() {
        let dumped = concat!(
            r#"tool write{"path":"/tmp/assemble-policy-gate.py","content":"from pathlib import Path"#,
            "\nimport subprocess\nprint(1)\n\"}"
        );
        let painted = layout(&chat_body(vec![mk_turn("assistant", dumped)]), 60, false);
        let text = texts(&painted).join("\n");
        assert!(text.contains("write"), "{text}");
        assert!(text.contains("assemble-policy-gate.py"), "{text}");
        assert!(!text.contains("from pathlib"), "{text}");
        assert!(!text.contains("import subprocess"), "{text}");
        assert!(painted.iter().all(|p| p.fold.is_none()), "a pending call has nothing to unfold");
    }

    #[test]
    fn user_turns_and_tools_show_a_clock() {
        let mut user = mk_turn("user", "look at src");
        user.ts = Some(1_774_000_000_000);
        let mut tool = tool_turn("bash", Some(r#"{"command":"ls"}"#), "ok");
        tool.ts = Some(1_774_000_030_000);
        let painted = layout(&chat_body(vec![user, tool]), 48, false);
        assert_fits(&painted, 48);
        let text = texts(&painted);
        assert!(text[0].starts_with("┃ look at src") && text[0].contains(':'), "{text:?}");
        assert!(text.last().unwrap().contains(':'), "{text:?}");
    }

    #[test]
    fn tool_and_thinking_collapse_until_expanded() {
        let turn = tool_turn("bash", None, &(0..12).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n"));
        let thinking = mk_turn("thinking", "I should search the loader first.");
        let mixed = mk_turn("assistant", "need the files\ntool bash {\"command\":\"rg loader\"}");
        let collapsed = layout(&chat_body(vec![thinking.clone(), mixed, turn.clone()]), 40, false);
        let expanded = layout(&chat_body(vec![thinking, turn]), 40, true);
        let c = texts(&collapsed).join("\n");
        let e = texts(&expanded).join("\n");
        assert!(c.contains("▸ ∴ Thinking"), "{c}");
        assert!(!c.contains("I should search"), "{c}");
        assert!(!c.contains("need the files"), "{c}");
        assert!(!c.contains("line-0"), "{c}");
        assert!(c.contains("bash"), "{c}");
        assert!(e.contains("▾ ∴ Thinking"), "{e}");
        assert!(e.contains("I should search"), "{e}");
        assert!(e.contains("line-0") && e.contains("line-11"), "{e}");
    }

    #[test]
    fn assistant_markdown_styles_code_bullets_and_headings() {
        let text = "## Plan\n- run `cargo test` now\n- **ship** it\n```\nfn main() {}\n```";
        let painted = layout(&chat_body(vec![mk_turn("assistant", text)]), 60, false);
        let lines = texts(&painted);
        assert_eq!(lines[0], "  Plan");
        assert_eq!(lines[1], "  • run cargo test now");
        assert_eq!(lines[2], "  • ship it");
        assert_eq!(lines[3], "  │ fn main() {}");
        let code = painted[1].spans.iter().find(|s| s.content == "cargo test").expect("code span");
        assert_eq!(code.style.fg, Some(CODE));
        let strong = painted[2].spans.iter().find(|s| s.content == "ship").expect("bold span");
        assert!(strong.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn assistant_error_is_marked() {
        let painted = layout(&chat_body(vec![mk_turn("assistant error", "529 overloaded")]), 40, false);
        assert_eq!(texts(&painted), vec!["  ✗ 529 overloaded".to_string()]);
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
        d.body = DetailBody::Conversation(Chat {
            turns: vec![mk_turn("user", "inspect the loader")],
            stats: vec![
                ("100".into(), "in".into()),
                ("20".into(), "out".into()),
                ("20.0".into(), "tok/s".into()),
                ("1".into(), "shell".into()),
            ],
            running: true,
        });
        state.detail = Some(d);
        let text = render(80, 24, &mut state);
        assert!(!text.contains("Result"), "{text}");
        assert!(!text.contains("Info"), "{text}");
        assert!(text.contains("100 in"), "{text}");
        assert!(text.contains("20.0 tok/s"), "{text}");
        assert!(text.contains("working"), "{text}");
        assert!(text.contains("following"), "{text}");
        assert!(text.contains("┃ inspect the loader"), "{text}");
        assert!(text.contains("╭"), "rounded panel: {text}");
    }

    #[test]
    fn tab_bar_clicks_land_on_tabs_scope_and_sort() {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        let text = render(120, 30, &mut state);
        assert!(text.contains("Tree") && text.contains("Agents") && text.contains("scope"), "{text}");
        let hits = state.mouse.tab_hits.clone();
        let agents = hits.iter().find(|h| h.2 == TabHit::Tab(Tab::Agents)).expect("agents hit");
        let scope = hits.iter().find(|h| h.2 == TabHit::Scope).expect("scope hit");
        let sort = hits.iter().find(|h| h.2 == TabHit::Sort).expect("sort hit");
        assert!(agents.0 < agents.1 && agents.1 <= scope.0 && scope.1 <= sort.0, "{hits:?}");
        let line = text.lines().find(|l| l.contains("Agents")).unwrap();
        let col = |needle: &str| fmt::display_width(&line[..line.find(needle).unwrap()]) as u16;
        assert!(agents.0 <= col("Agents") && col("Agents") < agents.1, "{line:?} {hits:?}");
        assert!(scope.0 <= col("active") && col("active") < scope.1, "{line:?} {hits:?}");
    }

    #[test]
    fn running_rows_animate_and_the_header_shows_the_refresh_clock() {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        state.header.live = true;
        assert!(state.wants_anim(), "a visible running task keeps the table redrawing");
        let text = render(120, 30, &mut state);
        let clock = clock(Some(snap.now));
        assert!(!clock.is_empty());
        let header = text.lines().next().unwrap();
        assert!(header.contains("live 1s") && header.contains(&clock), "{header}");
        let row = text.lines().find(|l| l.contains("pytest")).unwrap();
        assert!(SPIN.iter().any(|g| row.contains(g)), "running task spins: {row}");
        state.paused = true;
        assert!(!state.wants_anim(), "paused stays still");
        state.paused = false;
        state.set_tab(Tab::Tasks);
        assert!(!state.wants_anim(), "the task table has no glyphs to spin");
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
        assert!(top.contains(&THUMB), "{top:?}");
        assert!(bottom.contains(&THUMB), "{bottom:?}");
        let top_pos = top.iter().position(|c| *c == THUMB).unwrap();
        let bot_pos = bottom.iter().position(|c| *c == THUMB).unwrap();
        assert!(top_pos < bot_pos, "top {top_pos} bottom {bot_pos}");
    }
}
