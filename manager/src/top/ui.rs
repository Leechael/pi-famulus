//! Ratatui drawing for interactive `top`. Pure function of [`TopState`] and
//! the frame `Rect`; no I/O.
//!
//! Visual language (btop-like): rounded boxes with colored borders, titles
//! and hotkeys cut into the border as `┐title┌`, key hints as `┘key label└`
//! in the bottom border, dim labels beside bright values, and color kept for
//! status and load. The agent chat is a thread: parent messages hang off an
//! accent rail, the child's output opens with `● agent`, and tools / thinking
//! are one-line rows that unfold in place under a thin rule.

use crate::fmt;
use crate::inspect::TranscriptTurn;
use crate::top::model::{
    self, agent_cols, compact_num, cpu_cell, header_height, height_class, now_cell, task_cols, tok_io_cell, tree_cols,
    wall_cell, width_class, AgentComposer, Chat, Col, DetailBody, FoldHit, HeightClass, HitRect, Node, NodeKind, Tab, TabHit,
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

/// `┘key label└`: the one key-hint style, used in every bottom border.
/// First 12 columns of an id for compact labels.
fn short(id: &str) -> String {
    fmt::truncate_width(id, 12)
}

/// `1 line`, `24 lines`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn key_seg(k: &str, l: &str) -> Vec<Span<'static>> {
    seg_bottom(BOX_MAIN, vec![sp(fg(TEXT).add_modifier(Modifier::BOLD), k.to_string()), sp(fg(MUTED), format!(" {l}"))])
}

/// A hint and its priority: 0 always stays, higher numbers drop first.
type Key<'a> = (&'a str, &'a str, u8);

/// `prefix` (a prompt or message) then as many key hints as fit in `room`,
/// dropping the least important first but keeping display order.
fn fit_keys(prefix: Vec<Span<'static>>, keys: &[Key], room: usize) -> Vec<Span<'static>> {
    let mut keep: Vec<bool> = vec![true; keys.len()];
    let width = |keep: &[bool]| {
        spans_width(&prefix)
            + keys.iter().zip(keep).filter(|(_, k)| **k).map(|((k, l, _), _)| spans_width(&key_seg(k, l))).sum::<usize>()
    };
    while width(&keep) > room {
        let drop = keys
            .iter()
            .enumerate()
            .filter(|(i, (_, _, p))| keep[*i] && *p > 0)
            .max_by_key(|(i, (_, _, p))| (*p, *i))
            .map(|(i, _)| i);
        match drop {
            Some(i) => keep[i] = false,
            None => break,
        }
    }
    let mut out = prefix;
    for ((k, l, _), on) in keys.iter().zip(&keep) {
        if *on {
            out.extend(key_seg(k, l));
        }
    }
    fit_spans(out, room)
}

/// Right-hand pill on a detail: scroll position when scrolled back,
/// `● following` while live at the end, nothing otherwise.
fn pos_pill(follow: bool, running: bool, len: usize, height: usize, scroll: usize) -> Vec<Span<'static>> {
    if !follow && len > height {
        let max = len - height;
        trail(BOX_MAIN, seg(BOX_MAIN, vec![sp(fg(DIM), format!("{}%", scroll * 100 / max.max(1)))]))
    } else if follow && running {
        trail(BOX_MAIN, seg(BOX_MAIN, vec![sp(fg(OK), "● "), sp(fg(MUTED), "following")]))
    } else {
        Vec::new()
    }
}

/// Full-screen detail frame: title, pill (dropped when it would collide),
/// and the bottom-border keys.
fn detail_block(title: &str, pill: Vec<Span<'static>>, prefix: Vec<Span<'static>>, keys: &[Key], width: u16) -> Block<'static> {
    let w = width as usize;
    let block = panel(title);
    let title_w = fmt::display_width(title) + 6;
    let block = if !pill.is_empty() && title_w + spans_width(&pill) + 2 <= w {
        block.title_top(Line::from(pill).right_aligned())
    } else {
        block
    };
    let bottom = lead(BOX_MAIN, fit_keys(prefix, keys, w.saturating_sub(4)));
    block.title_bottom(Line::from(bottom))
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
        ((rest as u32 * if hclass == HeightClass::Tall { 35 } else { 30 } / 100) as u16).max(4)
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(head_h), Constraint::Min(4), Constraint::Length(preview_h)])
        .split(area);

    draw_header(frame, chunks[0], state, hclass);
    draw_workspace(frame, chunks[1], state, wclass);
    if preview_h > 0 {
        draw_preview(frame, chunks[2], state);
    }

    if state.help {
        draw_help(frame, area);
    }
    let composer = state.composer.clone();
    if let Some(d) = state.detail.as_mut() {
        draw_detail(frame, area, d, composer.as_ref(), &mut state.mouse);
    } else {
        state.mouse.detail = HitRect::default();
        state.mouse.chat = HitRect::default();
        state.mouse.scrollbar = HitRect::default();
        state.mouse.dragging_bar = false;
    }
}

// ── boxes ──────────────────────────────────────────────────────────────────

/// Border colors per panel, muted so the content stays brighter.
const BOX_OVERVIEW: Color = Color::Rgb(86, 124, 98);
const BOX_KINDS: Color = Color::Rgb(128, 122, 80);
const BOX_MAIN: Color = Color::Rgb(98, 94, 156);
const BOX_PREVIEW: Color = Color::Rgb(136, 90, 96);
/// Hotkey digits in box titles.
const HOT: Color = Color::Rgb(238, 112, 112);

/// `┐title┌` cut into a border line.
fn seg(color: Color, body: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = vec![sp(fg(color), "┐")];
    out.extend(body);
    out.push(sp(fg(color), "┌"));
    out
}

/// `┘key label└` for bottom borders.
fn seg_bottom(color: Color, body: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = vec![sp(fg(color), "┘")];
    out.extend(body);
    out.push(sp(fg(color), "└"));
    out
}

/// Box title: superscript hotkey digit, then the name in bold.
fn titled_seg(color: Color, hot: &str, name: &str) -> Vec<Span<'static>> {
    let mut body = Vec::new();
    if !hot.is_empty() {
        body.push(sp(fg(HOT), hot.to_string()));
    }
    body.push(sp(fg(TEXT).add_modifier(Modifier::BOLD), name.to_string()));
    seg(color, body)
}

/// One `─` of border before a left title / after a right one, so segments
/// sit off the rounded corner like btop's `╭─┐cpu┌`.
fn lead(color: Color, mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans.insert(0, sp(fg(color), "─"));
    spans
}

fn trail(color: Color, mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans.push(sp(fg(color), "─"));
    spans
}

fn panel_box(color: Color) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded).border_style(fg(color))
}

/// Linear blend between two RGB colors.
fn mix(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let l = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t.clamp(0.0, 1.0)).round() as u8;
            Color::Rgb(l(r1, r2), l(g1, g2), l(b1, b2))
        }
        _ => a,
    }
}

const GRAD_MID: Color = Color::Rgb(226, 196, 92);

/// Load gradient: green → yellow → red as `t` goes 0 → 1.
fn grad(t: f64) -> Color {
    if t < 0.5 {
        mix(OK, GRAD_MID, t * 2.0)
    } else {
        mix(GRAD_MID, ERR, (t - 0.5) * 2.0)
    }
}

/// `■■■■□□□□` with each filled cell colored by its position on the gradient.
fn grad_meter(used: usize, total: usize, cells: usize) -> Vec<Span<'static>> {
    let fill = (used * cells).checked_div(total).map_or(0, |_| (used * cells).div_ceil(total)).min(cells);
    let mut out: Vec<Span> = (0..fill)
        .map(|i| sp(fg(grad(i as f64 / (cells.max(2) - 1) as f64)), "■"))
        .collect();
    out.push(sp(fg(RULE), "■".repeat(cells - fill)));
    out
}

const SPARK: [&str; 5] = ["⠀", "⣀", "⣤", "⣶", "⣿"];

/// One-row braille graph of the newest `cells` samples, scaled to `ceil`
/// (or the window's max when that is higher), colored by level.
fn sparkline(hist: &[f64], cells: usize, ceil: f64) -> Vec<Span<'static>> {
    let start = hist.len().saturating_sub(cells);
    let window = &hist[start..];
    let max = window.iter().copied().fold(ceil, f64::max).max(f64::EPSILON);
    let mut out = vec![sp(fg(RULE), "⠀".repeat(cells - window.len()))];
    for v in window {
        let t = (v / max).clamp(0.0, 1.0);
        let level = if *v <= 0.0 { 0 } else { 1 + ((t * 3.0).round() as usize).min(3) };
        let color = if level == 0 { RULE } else { grad(t) };
        out.push(sp(fg(color), SPARK[level]));
    }
    out
}

// ── header ─────────────────────────────────────────────────────────────────

fn live_pill(state: &TopState) -> Vec<Span<'static>> {
    let at = clock((state.now > 0).then_some(state.now));
    let at = if at.is_empty() { String::new() } else { format!(" · {at}") };
    if state.paused {
        vec![sp(fg(WARN), "‖ paused"), sp(fg(DIM), at)]
    } else if state.header.live {
        vec![sp(fg(OK), "● "), sp(fg(TEXT), format!("live {}", interval_text(state.interval_ms))), sp(fg(DIM), at)]
    } else {
        vec![sp(fg(DIM), format!("○ offline · disk snapshot{at}"))]
    }
}

fn interval_text(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Compact two-line header for short terminals: no boxes.
fn brand_line(state: &TopState, width: usize) -> Line<'static> {
    let h = &state.header;
    // The version goes first when the live pill would not fit.
    let version = if 50 + h.version.len() <= width { format!(" {}", h.version) } else { String::new() };
    let mut left = vec![
        sp(fg(ACCENT).add_modifier(Modifier::BOLD), " ◆ "),
        sp(bold(), "pi-famulus"),
        sp(fg(DIM), version),
        sp(fg(DIM), "   sessions "),
        sp(fg(MUTED), format!("{}", h.sessions_conn)),
        sp(fg(DIM), format!("/{}", h.sessions_tot)),
    ];
    left.push(Span::raw(" "));
    let mut right = live_pill(state);
    right.push(Span::raw(" "));
    Line::from(split_line(left, right, width))
}

fn stat_row(label: &str, value: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = vec![sp(fg(MUTED), format!(" {label:<8}"))];
    out.extend(value);
    out
}

fn val(text: impl Into<String>) -> Span<'static> {
    let t: String = text.into();
    if t == "-" || t == "stale" {
        sp(fg(DIM), t)
    } else {
        sp(fg(TEXT), t)
    }
}

fn unit(text: &str) -> Span<'static> {
    sp(fg(DIM), text.to_string())
}

fn running_count(n: usize) -> Span<'static> {
    if n > 0 {
        sp(fg(OK).add_modifier(Modifier::BOLD), n.to_string())
    } else {
        sp(fg(DIM), "0")
    }
}

/// Stacked LLM / tool / queue share bar.
fn wall_bar(llm: u64, tool: u64, queue: u64, cells: usize) -> Vec<Span<'static>> {
    let total = llm + tool + queue;
    if total == 0 {
        return vec![sp(fg(RULE), "■".repeat(cells))];
    }
    let part = |v: u64| ((v as usize * cells) + total as usize / 2) / total as usize;
    let l = part(llm).min(cells);
    let t = part(tool).min(cells - l);
    let q = cells - l - t;
    vec![sp(fg(ACCENT), "■".repeat(l)), sp(fg(OK), "■".repeat(t)), sp(fg(WARN), "■".repeat(q))]
}

fn draw_header(frame: &mut Frame, area: Rect, state: &TopState, hclass: HeightClass) {
    let h = &state.header;
    let width = area.width as usize;
    if hclass == HeightClass::Short {
        let mut line2 = vec![sp(fg(MUTED), "   agents ")];
        line2.extend(grad_meter(h.agents_used, h.agents_total, 4));
        line2.push(val(format!(" {}/{}", h.agents_used, h.agents_total)));
        line2.push(unit("  running "));
        line2.push(running_count(h.tasks_run));
        line2.push(unit("  cpu "));
        line2.extend(sparkline(&state.cpu_hist, 8, 100.0));
        line2.push(val(format!(" {}", now_cell(h.cpu_now, false))));
        line2.push(unit("  tok "));
        line2.push(val(compact_num(h.tokens_in.unwrap_or(0))));
        line2.push(unit("/"));
        line2.push(val(compact_num(h.tokens_out.unwrap_or(0))));
        frame.render_widget(
            Paragraph::new(vec![brand_line(state, width), Line::from(fit_spans(line2, width))]),
            area,
        );
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
        .split(area);
    draw_overview(frame, cols[0], state);
    draw_kinds(frame, cols[1], state);
}

fn draw_overview(frame: &mut Frame, area: Rect, state: &TopState) {
    let h = &state.header;
    // Titles share one border row: drop the version, then the live pill,
    // rather than let them overwrite each other.
    let room = (area.width as usize).saturating_sub(4);
    let pill = trail(BOX_OVERVIEW, seg(BOX_OVERVIEW, live_pill(state)));
    let mut title = lead(BOX_OVERVIEW, titled_seg(BOX_OVERVIEW, "", "pi-famulus"));
    let versioned = {
        let mut t = title.clone();
        t.insert(t.len() - 1, sp(fg(DIM), format!(" {}", h.version)));
        t
    };
    if spans_width(&versioned) + spans_width(&pill) <= room {
        title = versioned;
    }
    let mut block = panel_box(BOX_OVERVIEW).title(Line::from(title.clone()));
    if spans_width(&title) + spans_width(&pill) <= room {
        block = block.title(Line::from(pill).right_aligned());
    }
    if let Some(pid) = h.pid {
        let up = h.uptime_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into());
        block = block.title_bottom(Line::from(lead(
            BOX_OVERVIEW,
            seg_bottom(BOX_OVERVIEW, vec![sp(fg(DIM), format!("pid {pid} · up {up}"))]),
        )));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let w = inner.width as usize;
    let graph_w = w.saturating_sub(40).clamp(8, 32);

    let mut rows = Vec::new();
    let mut agents = grad_meter(h.agents_used, h.agents_total, 10);
    agents.push(val(format!(" {}/{}", h.agents_used, h.agents_total)));
    agents.push(unit("   sessions "));
    agents.push(val(h.sessions_conn.to_string()));
    agents.push(unit(format!("/{}", h.sessions_tot).as_str()));
    rows.push(stat_row("agents", agents));

    let narrow = w < 70;
    let mut tasks = sparkline(&state.run_hist, graph_w, 1.0);
    tasks.extend([
        Span::raw(" "),
        running_count(h.tasks_run),
        unit(if narrow { " run · " } else { " running · " }),
        val(h.tasks_sh.to_string()),
        unit(match (narrow, h.tasks_sh == 1) {
            (true, _) => " sh · ",
            (false, true) => " shell · ",
            (false, false) => " shells · ",
        }),
        val(h.tasks_mon.to_string()),
        unit(match (narrow, h.tasks_mon == 1) {
            (true, _) => " mon · ",
            (false, true) => " monitor · ",
            (false, false) => " monitors · ",
        }),
        val(h.tasks_ag.to_string()),
        unit(match (narrow, h.tasks_ag == 1) {
            (true, _) => " ag",
            (false, true) => " agent",
            (false, false) => " agents",
        }),
    ]);
    rows.push(stat_row("tasks", tasks));

    let mut cpu = sparkline(&state.cpu_hist, graph_w, 100.0);
    cpu.extend([
        Span::raw(" "),
        val(h.cpu_now.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "-".into())),
        unit(" now · "),
        val(h.cpu_tot.map(fmt::human_duration).unwrap_or_else(|| "-".into())),
        unit(" total"),
    ]);
    rows.push(stat_row("cpu", cpu));

    rows.push(stat_row(
        "tokens",
        vec![
            // `-` when no agent in scope recorded usage, not a fake 0.
            val(h.tokens_in.map(compact_num).unwrap_or_else(|| "-".into())),
            unit(" in · "),
            val(h.tokens_out.map(compact_num).unwrap_or_else(|| "-".into())),
            unit(" out · "),
            val(h.tok_s.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into())),
            unit(if narrow { " tok/s" } else { " tok/s per agent" }),
        ],
    ));
    let (llm, tool, queue) = (h.llm_share.unwrap_or(0), h.tool_share.unwrap_or(0), h.queue_share.unwrap_or(0));
    let mut wall = wall_bar(llm, tool, queue, 10);
    wall.extend([
        sp(fg(ACCENT), " llm "),
        val(format!("{llm}%")),
        sp(fg(OK), "  tool "),
        val(format!("{tool}%")),
        sp(fg(WARN), "  queue "),
        val(format!("{queue}%")),
    ]);
    rows.push(stat_row("wall", wall));

    let lines: Vec<Line> = rows.into_iter().take(inner.height as usize).map(|r| Line::from(fit_spans(r, w))).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_kinds(frame: &mut Frame, area: Rect, state: &TopState) {
    let block = panel_box(BOX_KINDS).title(Line::from(lead(BOX_KINDS, titled_seg(BOX_KINDS, "", "work kinds"))));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let w = inner.width as usize;
    // Columns drop whole, least useful first, rather than clipping mid-word:
    // kind 12 + slots 10 always; now 7, tasks 6, wall 8, cpu 7 while they fit.
    let mut room = w.saturating_sub(22);
    let mut take = |n: usize| {
        let ok = room >= n;
        if ok {
            room -= n;
        }
        ok
    };
    let (show_now, show_tasks, show_wall, show_cpu) = (take(7), take(6), take(8), take(7));
    let kinds = &state.header.work_kinds;
    let mut head = format!(" {:<11} {:<9}", "kind", "slots");
    if show_tasks {
        head.push_str(&format!("{:>5}", "tasks"));
    }
    if show_wall {
        head.push_str(&format!(" {:>7}", "wall"));
    }
    if show_cpu {
        head.push_str(&format!(" {:>6}", "cpu"));
    }
    if show_now {
        head.push_str(&format!(" {:>6}", "now"));
    }
    let mut lines = vec![Line::from(fit_spans(vec![sp(fg(MUTED), head)], w))];
    let rows_room = (inner.height as usize).saturating_sub(1);
    let shown = if kinds.len() > rows_room { rows_room.saturating_sub(1) } else { kinds.len() };
    for k in kinds.iter().take(shown) {
        let mut row = vec![sp(fg(if k.used > 0 { TEXT } else { MUTED }), format!(" {:<11} ", fmt::truncate_width(&k.kind, 11)))];
        if k.total > 0 {
            row.extend(grad_meter(k.used, k.total, 4));
            row.push(sp(fg(if k.used > 0 { TEXT } else { DIM }), format!(" {:<4}", format!("{}/{}", k.used, k.total))));
        } else {
            // No budget: monitors never wait for a slot.
            row.push(sp(fg(DIM), format!("{:<9}", "-")));
        }
        if show_tasks {
            row.push(val(format!("{:>5}", k.tasks)));
        }
        if show_wall {
            row.push(val(format!(" {:>7}", fmt::human_duration(k.wall_ms))));
        }
        if show_cpu {
            row.push(match k.cpu_ms {
                Some(c) => val(format!(" {:>6}", fmt::human_duration(c))),
                None => sp(fg(DIM), format!(" {:>6}", "-")),
            });
        }
        if show_now {
            row.push(match k.cpu_now.filter(|p| *p > 0.0) {
                Some(p) => sp(fg(grad((p / 400.0).min(1.0))), format!(" {:>6}", format!("{p:.0}%"))),
                None => sp(fg(DIM), format!(" {:>6}", "-")),
            });
        }
        lines.push(Line::from(fit_spans(row, w)));
    }
    if shown < kinds.len() {
        lines.push(Line::from(sp(fg(DIM), format!(" +{} more", kinds.len() - shown))));
    }
    if kinds.is_empty() {
        lines.push(Line::from(sp(fg(DIM), " (no work yet)")));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── main box ───────────────────────────────────────────────────────────────

/// Tabs cut into the main box's top border, with click ranges relative to
/// the box's left edge.
fn tab_segments(state: &TopState) -> (Vec<Span<'static>>, Vec<(u16, u16, TabHit)>) {
    let mut spans = vec![sp(fg(BOX_MAIN), "─")];
    let mut hits = Vec::new();
    let mut x = 2usize; // after the corner and the lead dash
    for (i, tab) in [Tab::Tree, Tab::Agents, Tab::Tasks, Tab::Events].into_iter().enumerate() {
        let (name, count) = match tab {
            Tab::Tree => ("tree", Some(state.count_tab(Tab::Tree))),
            Tab::Agents => ("agents", Some(state.count_tab(Tab::Agents))),
            Tab::Tasks => ("tasks", Some(state.count_tab(Tab::Tasks))),
            Tab::Events => ("events", None),
        };
        // `e` narrows events to one id; say so, Esc clears it.
        let name = match (&state.events_id, tab) {
            (Some(id), Tab::Events) => format!("events for {}", short(id)),
            _ => name.to_string(),
        };
        let name = name.as_str();
        let active = state.tab == tab;
        let mut body = vec![sp(fg(HOT), ["¹", "²", "³", "⁴"][i].to_string())];
        body.push(if active {
            sp(fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED), name)
        } else {
            sp(fg(MUTED), name)
        });
        if let Some(c) = count {
            body.push(sp(fg(if active { ACCENT } else { DIM }), format!(" {c}")));
        }
        let s = seg(BOX_MAIN, body);
        let w = spans_width(&s);
        hits.push((x as u16, (x + w) as u16, TabHit::Tab(tab)));
        x += w;
        spans.extend(s);
    }
    (spans, hits)
}

fn scope_sort_segments(state: &TopState) -> (Vec<Span<'static>>, usize, usize) {
    let scope = seg(BOX_MAIN, vec![sp(fg(DIM), "scope "), sp(fg(TEXT), state.scope.label().to_lowercase())]);
    let sort = seg(
        BOX_MAIN,
        vec![
            sp(fg(DIM), "sort "),
            sp(fg(TEXT), state.sort.label().to_lowercase()),
            sp(fg(ACCENT), if state.sort_desc { " ↓" } else { " ↑" }),
        ],
    );
    let (sw, tw) = (spans_width(&scope), spans_width(&sort));
    let mut spans = scope;
    spans.extend(sort);
    // Trailing dash: hit ranges cover the segments, not the dash.
    (trail(BOX_MAIN, spans), sw, tw)
}

/// Bottom border: the live prompt (stop confirm / filter / message) and
/// the keys that apply to this tab, fitted to `room`.
fn bottom_segments(state: &TopState, room: usize) -> Vec<Span<'static>> {
    if let Some(id) = &state.confirm_stop {
        let prefix = seg_bottom(BOX_MAIN, vec![sp(fg(ERR).add_modifier(Modifier::BOLD), format!("■ stop {id}?"))]);
        return fit_keys(prefix, &[("y ⏎", "confirm", 0), ("n esc", "cancel", 0)], room);
    }
    if state.filter_editing {
        let prefix = seg_bottom(
            BOX_MAIN,
            vec![sp(fg(ACCENT).add_modifier(Modifier::BOLD), "/ "), sp(fg(TEXT), state.filter.clone()), sp(fg(ACCENT), "▏")],
        );
        return fit_keys(prefix, &[("enter", "apply", 0), ("esc", "clear", 0)], room);
    }
    let mut prefix = Vec::new();
    if !state.filter.is_empty() {
        prefix.extend(seg_bottom(BOX_MAIN, vec![sp(fg(ACCENT), "/ "), sp(fg(TEXT), state.filter.clone())]));
    }
    if let Some(m) = &state.message {
        prefix.extend(seg_bottom(BOX_MAIN, vec![sp(fg(WARN), m.clone())]));
    }
    let mut keys: Vec<Key> = vec![("↑↓", "select", 3)];
    if state.tab != Tab::Events {
        keys.push(("⏎", "open", 1));
    }
    if state.tab == Tab::Tree {
        keys.push(("←→", "fold", 4));
    }
    if state.tab != Tab::Events {
        keys.push(("v", "preview", 6));
    }
    keys.extend([("a", "scope", 5), ("s", "sort", 5), ("/", "filter", 2)]);
    if state.tab != Tab::Events {
        keys.push(("x", "stop", 6));
    }
    keys.extend([("?", "help", 0), ("q", "quit", 0)]);
    fit_keys(prefix, &keys, room)
}

fn position(state: &TopState) -> Vec<Span<'static>> {
    let total = state.visible.len();
    let at = state
        .selected
        .as_deref()
        .and_then(|id| state.visible.iter().position(|v| v.id == id))
        .map_or(0, |i| i + 1);
    seg_bottom(BOX_MAIN, vec![sp(fg(TEXT), format!("{at}")), sp(fg(DIM), format!("/{total}"))])
}

fn draw_workspace(frame: &mut Frame, area: Rect, state: &mut TopState, wclass: WidthClass) {
    let (tabs, hits) = tab_segments(state);
    let (right, scope_w, sort_w) = scope_sort_segments(state);
    let width = area.width as usize;
    let mut hits = hits;
    let tabs_w = spans_width(&tabs);
    let right_w = scope_w + sort_w + 1;
    let mut block = panel_box(BOX_MAIN).title(Line::from(tabs));
    if tabs_w + right_w + 4 <= width {
        let rx = width - 1 - right_w;
        hits.push((rx as u16, (rx + scope_w) as u16, TabHit::Scope));
        hits.push(((rx + scope_w) as u16, (rx + scope_w + sort_w) as u16, TabHit::Sort));
        block = block.title(Line::from(right).right_aligned());
    }
    // The position always shows; hints fit in what is left.
    let pos = trail(BOX_MAIN, position(state));
    let room = width.saturating_sub(spans_width(&pos) + 5);
    let bottom = lead(BOX_MAIN, bottom_segments(state, room));
    block = block.title_bottom(Line::from(pos).right_aligned());
    block = block.title_bottom(Line::from(bottom));
    state.mouse.tabs = HitRect::from_xywh(area.x, area.y, area.width, 1);
    state.mouse.tab_hits = hits;
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.tab == Tab::Events {
        draw_events(frame, inner, state);
        return;
    }
    let cols = match state.tab {
        Tab::Agents => agent_cols(wclass),
        Tab::Tasks => task_cols(wclass),
        _ => tree_cols(wclass),
    };
    draw_table(frame, inner, state, cols);
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

/// Placeholder cells: a column with nothing else in view is dropped.
fn is_blank_cell(text: &str) -> bool {
    matches!(text.trim(), "" | "-" | "-/-/-" | "stale")
}

/// Columns worth showing for the rows in view. The label, status, and
/// title columns always stay; any other column that is blank on every
/// visible row gives its width back to the flex column.
fn live_cols(state: &TopState, rows: &[VisibleRow], cols: &[Col]) -> Vec<Col> {
    cols.iter()
        .copied()
        .filter(|c| {
            matches!(c, Col::Node | Col::Id | Col::Status | Col::Title)
                || rows.is_empty()
                || rows.iter().any(|vr| !is_blank_cell(&cell_text(state, vr, *c)))
        })
        .collect()
}

fn draw_table(frame: &mut Frame, area: Rect, state: &mut TopState, cols: &[Col]) {
    state.mouse.table = hit_rect(area);
    let page = area.height.saturating_sub(1) as usize;
    state.ensure_scroll(page);
    let start = state.scroll;
    let end = (start + page).min(state.visible.len());
    let window: Vec<VisibleRow> = state.visible[start..end].to_vec();
    let cols = live_cols(state, &window, cols);
    let widths = col_widths(&cols, area.width);
    let header = Row::new(cols.iter().enumerate().map(|(i, c)| {
        let label = if i == 0 { format!(" {}", c.header().to_lowercase()) } else { c.header().to_lowercase() };
        Cell::from(Line::from(sp(fg(MUTED).add_modifier(Modifier::BOLD), fmt::truncate_width(&label, widths[i] as usize))))
    }));
    let sel = state.selected.clone();
    let rows: Vec<Row> = window
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
                row.style(Style::default().bg(SEL_BG).fg(TEXT).add_modifier(Modifier::BOLD))
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
            fg(MUTED).add_modifier(Modifier::BOLD),
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
            sp(fg(TEXT), format!("{:<12} ", ev.id)),
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

fn draw_preview(frame: &mut Frame, area: Rect, state: &TopState) {
    let mut body = vec![sp(fg(TEXT).add_modifier(Modifier::BOLD), "preview")];
    if let Some(n) = state.selected_node() {
        body.push(sp(fg(DIM), " · "));
        body.push(sp(fg(MUTED), format!("{} · {}", n.id, n.label)));
    }
    let block = panel_box(BOX_PREVIEW)
        .title(Line::from(fit_spans(lead(BOX_PREVIEW, seg(BOX_PREVIEW, body)), area.width.saturating_sub(2) as usize)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let painted = layout_detail(&state.preview_body, inner.width as usize, &HashSet::new(), false);
    let start = painted.len().saturating_sub(inner.height as usize);
    let lines: Vec<Line> = painted[start..]
        .iter()
        .map(|p| Line::from(fit_spans(p.spans.clone(), inner.width as usize)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Help rows; a row with an empty key is a section heading.
const HELP: &[(&str, &str)] = &[
    ("", "main"),
    ("1 2 3 4  Tab", "tree · agents · tasks · events"),
    ("↑↓ j k  PgUp PgDn", "move"),
    ("g G  Home End", "top / bottom"),
    ("←→ h l  z", "fold · unfold · all"),
    ("Enter  dbl-click", "open detail"),
    ("v", "preview pane"),
    ("e", "events for selection"),
    ("a", "cycle scope"),
    ("s >  S <  I", "sort next · prev · invert"),
    ("/", "filter rows"),
    ("p Space  + -", "pause · refresh rate"),
    ("x  Ctrl+X  F9", "stop task (y or ⏎)"),
    ("Esc", "clear filter / events"),
    ("q  Ctrl+C", "quit"),
    ("", "detail"),
    ("↑↓ j k  PgUp PgDn", "scroll"),
    ("g G", "top / end (follows)"),
    ("f", "follow live output"),
    ("o  click ▸", "unfold tools & thinking"),
    ("Esc  q", "close"),
    ("", "task output"),
    ("1 2  click", "output · stderr"),
    ("/  n N", "search · next · prev"),
    ("&  Tab", "filter or highlight"),
    ("b", "show blank lines"),
    ("", "mouse"),
    ("click  wheel", "select · scroll"),
    ("tabs · scope · sort", "click to switch"),
    ("scrollbar", "drag in detail"),
];

/// Overlay panel in the main box's color, title cut into the border.
fn panel(title: &str) -> Block<'static> {
    panel_box(BOX_MAIN).title(Line::from(lead(
        BOX_MAIN,
        seg(BOX_MAIN, vec![sp(fg(ACCENT).add_modifier(Modifier::BOLD), "◆ "), sp(fg(TEXT).add_modifier(Modifier::BOLD), title.to_string())]),
    )))
}

fn help_lines(rows: &[(&str, &str)], width: usize) -> Vec<Line<'static>> {
    rows.iter()
        .map(|(k, d)| {
            if k.is_empty() {
                Line::from(sp(fg(MUTED).add_modifier(Modifier::BOLD), d.to_string()))
            } else {
                Line::from(fit_spans(vec![sp(fg(TEXT).add_modifier(Modifier::BOLD), format!("  {k:<19}")), sp(fg(MUTED), d.to_string())], width))
            }
        })
        .collect()
}

/// Keys in two columns when the terminal is wide, one otherwise.
fn draw_help(frame: &mut Frame, area: Rect) {
    let two = area.width >= 104;
    let split = HELP.iter().position(|(k, d)| k.is_empty() && *d == "detail").unwrap_or(HELP.len());
    let rows = if two { split.max(HELP.len() - split) } else { HELP.len() };
    let rect = centered(area, if two { 100 } else { 52 }, rows as u16 + 4);
    frame.render_widget(Clear, rect);
    let block = panel("keys").title_bottom(Line::from(lead(BOX_MAIN, key_seg("esc ? q", "close"))));
    let inner = block.inner(rect).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, rect);
    if two {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        frame.render_widget(Paragraph::new(help_lines(&HELP[..split], cols[0].width as usize)), cols[0]);
        frame.render_widget(Paragraph::new(help_lines(&HELP[split..], cols[1].width as usize)), cols[1]);
    } else {
        frame.render_widget(Paragraph::new(help_lines(HELP, inner.width as usize)), inner);
    }
}

// ── detail overlay ─────────────────────────────────────────────────────────

fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    d: &mut model::Detail,
    composer: Option<&AgentComposer>,
    mouse: &mut model::MouseHit,
) {
    // Tasks always use the task view, also before their first body arrives.
    if !d.tabs.is_empty() || matches!(d.body, DetailBody::Log { .. }) {
        draw_task_detail(frame, area, d, mouse);
        return;
    }
    frame.render_widget(Clear, area);
    let inner = Block::bordered().inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    let chat = match &d.body {
        DetailBody::Conversation(c) => Some((c.stats.clone(), c.running, c.note.clone())),
        _ => None,
    };
    let live = composer.filter(|c| c.child_id == d.id);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if live.is_some() {
            vec![Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]
        } else {
            vec![Constraint::Min(1), Constraint::Length(1)]
        })
        .split(inner);
    let body = chunks[0];
    let text_w = body.width.saturating_sub(2).max(1);
    let width = text_w as usize;
    let height = body.height as usize;
    let painted = layout_detail(&d.body, width, &d.open, d.tools_expanded);
    d.wrap_len = painted.len();
    d.view_h = height;
    d.fold_count = painted.iter().filter_map(|p| p.fold.map(|f| f.id + 1)).max().unwrap_or(0);
    let (follow, scroll) = stick_scroll(d.follow, d.hold, d.scroll, painted.len(), height);
    d.follow = follow;
    d.scroll = scroll;
    let start = d.scroll.min(painted.len());
    d.hits = painted[start..].iter().take(height).map(|p| p.fold).collect();
    let lines: Vec<Line> =
        painted[start..].iter().take(height).map(|p| Line::from(fit_spans(p.spans.clone(), width))).collect();
    let chat_rect = Rect { x: body.x, y: body.y, width: text_w, height: body.height };
    let bar = Rect { x: body.x + body.width.saturating_sub(1), y: body.y, width: 1, height: body.height };
    mouse.detail = hit_rect(area);
    mouse.chat = hit_rect(chat_rect);
    mouse.scrollbar = hit_rect(bar);
    frame.render_widget(Paragraph::new(lines), chat_rect);
    draw_scrollbar(frame, bar, painted.len(), height, d.scroll);

    // Status line: live spinner, then usage (or why there is none).
    let running = chat.as_ref().is_some_and(|c| c.1);
    let mut left = Vec::new();
    if let Some((stats, running, note)) = &chat {
        if *running {
            left.push(sp(fg(ACCENT), format!("{} ", spin_glyph())));
            left.push(sp(fg(ACCENT).add_modifier(Modifier::ITALIC), "working   "));
        }
        // Without recorded usage only the shell count (from tasks) is real.
        let shown: &[(String, String)] = match note {
            Some(n) => {
                left.push(sp(fg(WARN), n.clone()));
                left.push(sp(fg(RULE), " · "));
                stats.last().map(std::slice::from_ref).unwrap_or(&[])
            }
            None => stats,
        };
        for (i, (v, l)) in shown.iter().enumerate() {
            if i > 0 {
                left.push(sp(fg(RULE), " · "));
            }
            left.push(val(v.clone()));
            left.push(unit(&format!(" {l}")));
        }
    }
    let status_row = if live.is_some() { chunks[2] } else { chunks[1] };
    frame.render_widget(Paragraph::new(Line::from(fit_spans(left, status_row.width as usize))), status_row);
    if let Some(c) = live {
        let facts = [c.activity.as_str(), c.residency.as_str(), c.model.as_str(), c.cache.as_str(), c.context.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let width = chunks[1].width as usize;
        let suffix = if facts.is_empty() { String::new() } else { format!("  {facts}") };
        let budget = width.saturating_sub(2 + suffix.chars().count() + 1);
        let shown = tail_columns(&c.input, budget);
        let prompt = format!("› {shown}▏{suffix}");
        frame.render_widget(Paragraph::new(Line::from(sp(fg(ACCENT), prompt))), chunks[1]);
    }

    let keys: Vec<Key> = if live.is_some() {
        vec![("q", "close", 0), ("enter", "send", 1), ("alt-enter", "later", 2), ("esc esc", "abort", 3), ("↑↓", "scroll", 4)]
    } else {
        let mut keys = vec![("esc", "close", 0), ("o", if d.tools_expanded { "fold all" } else { "unfold all" }, 1)];
        if running {
            keys.push(("f", if d.follow { "unfollow" } else { "follow" }, 2));
        }
        keys.extend([("↑↓", "scroll", 3), ("g G", "top / end", 4)]);
        keys
    };
    let pill = pos_pill(d.follow, running, painted.len(), height, d.scroll);
    frame.render_widget(detail_block(&d.title, pill, Vec::new(), &keys, area.width), area);
}

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const SPIN_MS: u128 = 80;
/// Left margin for assistant text and activity rows; the user rail sits in it.
const MARGIN: usize = 2;
/// Unfolded tool output / thinking hangs off a rule at this column.
const HANG: usize = MARGIN + 2;

/// Keep the end of `text` inside `width` columns. Code points above Latin-1 count as two.
fn tail_columns(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut used = 0usize;
    let mut start = text.len();
    for (idx, ch) in text.char_indices().rev() {
        let w = if (ch as u32) > 0xFF { 2 } else { 1 };
        if used + w > width {
            break;
        }
        used += w;
        start = idx;
    }
    text[start..].to_string()
}

fn spin_index(ms: u128) -> usize {
    (ms as usize / SPIN_MS as usize) % SPIN.len()
}

fn spin_glyph() -> &'static str {
    static START: OnceLock<Instant> = OnceLock::new();
    let t = START.get_or_init(Instant::now);
    SPIN[spin_index(t.elapsed().as_millis())]
}

/// Stick to the end while following; scrolling up unsticks, hitting the
/// bottom resticks (unless `f` turned follow off there). Scroll never jumps
/// back while the user is reading.
fn stick_scroll(follow: bool, hold: bool, scroll: usize, len: usize, height: usize) -> (bool, usize) {
    let max = len.saturating_sub(height);
    if follow || (scroll >= max && !hold) {
        (true, max)
    } else {
        (false, scroll.min(max))
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
        DetailBody::Log { lines, .. } => layout_log(lines, width, &model::LogView::default(), None).lines,
    }
}

// ── task output ────────────────────────────────────────────────────────────

/// Parsed search: `a|b` alternatives, case-insensitive unless the query has
/// an uppercase letter (ASCII folding keeps byte offsets stable).
struct Query {
    terms: Vec<String>,
    fold: bool,
}

fn parse_query(q: &str) -> Option<Query> {
    let fold = !q.chars().any(|c| c.is_ascii_uppercase());
    let terms: Vec<String> = q
        .split('|')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| if fold { t.to_ascii_lowercase() } else { t.to_string() })
        .collect();
    (!terms.is_empty()).then_some(Query { terms, fold })
}

/// Merged byte ranges of every match in `line`, left to right.
fn match_ranges(line: &str, q: &Query) -> Vec<(usize, usize)> {
    let hay = if q.fold { line.to_ascii_lowercase() } else { line.to_string() };
    let mut found = Vec::new();
    for t in &q.terms {
        let mut from = 0;
        while let Some(i) = hay[from..].find(t.as_str()) {
            found.push((from + i, from + i + t.len()));
            from += i + t.len();
        }
    }
    found.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in found {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

const HIT: Color = Color::Rgb(116, 92, 24);
const HIT_CURRENT: Color = Color::Rgb(226, 170, 60);

fn highlighted(seg: &str, q: Option<&Query>, current: bool) -> (Vec<Span<'static>>, bool) {
    let ranges = q.map(|q| match_ranges(seg, q)).unwrap_or_default();
    if ranges.is_empty() {
        return (vec![sp(Style::default(), seg.to_string())], false);
    }
    let hit = if current {
        Style::default().bg(HIT_CURRENT).fg(Color::Rgb(20, 20, 24)).add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(HIT).fg(TEXT).add_modifier(Modifier::BOLD)
    };
    let mut spans = Vec::new();
    let mut at = 0;
    for (s, e) in ranges {
        if s > at {
            spans.push(sp(Style::default(), seg[at..s].to_string()));
        }
        spans.push(sp(hit, seg[s..e].to_string()));
        at = e;
    }
    if at < seg.len() {
        spans.push(sp(Style::default(), seg[at..].to_string()));
    }
    (spans, true)
}

/// Painted output plus what the chrome reports about it.
struct LogLayout {
    lines: Vec<Painted>,
    /// Painted rows holding a match.
    matches: Vec<usize>,
    /// Source lines shown / in the tail.
    shown: usize,
    total: usize,
}

/// Task output: control sequences scrubbed, blank lines hidden unless asked
/// for, matches highlighted (or the rest filtered out).
fn layout_log(lines: &[String], width: usize, view: &model::LogView, current_row: Option<usize>) -> LogLayout {
    let inner = width.saturating_sub(1).max(1);
    let q = parse_query(&view.query);
    let placeholder = lines.len() == 1 && lines[0].starts_with('(');
    let mut out = LogLayout { lines: Vec::new(), matches: Vec::new(), shown: 0, total: lines.len() };
    for line in lines {
        // A trailing `\r` (CRLF from a pty) is a line ending, not CR progress.
        let clean = fmt::scrub_tty(line.trim_end_matches('\r'));
        if placeholder {
            out.lines.push(Painted::new(vec![Span::raw(" "), sp(fg(DIM), clean)]));
            out.shown = 1;
            break;
        }
        if !view.show_blank && clean.trim().is_empty() {
            continue;
        }
        if view.filter && q.as_ref().is_some_and(|q| match_ranges(&clean, q).is_empty()) {
            continue;
        }
        out.shown += 1;
        for seg in fmt::wrap_width(&clean, inner) {
            let row = out.lines.len();
            let (spans, hit) = highlighted(&seg, q.as_ref(), current_row == Some(row));
            if hit {
                out.matches.push(row);
            }
            let mut all = vec![Span::raw(" ")];
            all.extend(spans);
            out.lines.push(Painted::new(all));
        }
    }
    if out.lines.is_empty() {
        let why = if q.is_some() && view.filter { "(no line matches)" } else { "(only blank lines)" };
        out.lines.push(Painted::new(vec![Span::raw(" "), sp(fg(DIM), why)]));
    }
    out
}

/// Pinned panel above a task's output: state, cost, command, and where.
fn info_card(info: &model::TaskInfo, width: usize) -> Vec<Line<'static>> {
    let color = glyph_color(&info.status, info.running);
    let glyph = if info.running { spin_glyph() } else { status_glyph(&info.status, false) };
    let pair = |label: &str, value: String| vec![sp(fg(DIM), format!("   {label} ")), val(value)];
    let mut l1 = vec![
        sp(fg(color), format!("{glyph} ")),
        sp(fg(color).add_modifier(Modifier::BOLD), info.status.clone()),
        sp(fg(MUTED), format!("   {} · {}", info.kind, info.work_kind)),
    ];
    l1.extend(pair("wall", fmt::human_duration(info.wall_ms)));
    l1.extend(pair("cpu", info.cpu_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into())));
    if info.running {
        let now = now_cell(info.cpu_now, info.cpu_stale);
        let now = if now.parse::<f64>().is_ok() { format!("{now}%") } else { now };
        l1.extend(pair("now", now));
    }
    if let Some(exit) = &info.exit {
        let bad = exit != "0";
        l1.push(sp(fg(DIM), "   exit "));
        l1.push(sp(fg(if bad { ERR } else { OK }).add_modifier(Modifier::BOLD), exit.clone()));
    }
    if let Some(reason) = info.reason.as_ref().filter(|r| r.as_str() != "exited") {
        l1.extend(pair("ended", reason.clone()));
    }
    let more = if info.command.trim_end().contains('\n') { " …" } else { "" };
    let l2 = vec![
        sp(fg(ACCENT).add_modifier(Modifier::BOLD), "$ "),
        sp(fg(TEXT).add_modifier(Modifier::BOLD), fmt::first_line(&info.command).to_string()),
        sp(fg(DIM), more),
    ];
    let started = fmt::datetime(info.started_at);
    let mut l3 = vec![
        sp(fg(DIM), "cwd "),
        sp(fg(MUTED), fmt::tilde(&info.cwd)),
        sp(fg(DIM), "   pid "),
        sp(fg(MUTED), info.pid.to_string()),
        sp(fg(DIM), "   started "),
        sp(fg(MUTED), started),
        sp(fg(DIM), "   session "),
        sp(fg(MUTED), info.session.chars().take(8).collect::<String>()),
    ];
    let used = spans_width(&l3) + 5;
    if !info.output.is_empty() && used + 12 < width {
        l3.push(sp(fg(DIM), "   "));
        l3.push(sp(fg(DIM), fmt::truncate_width_left(&info.output, width - used)));
    }
    [l1, l2, l3].into_iter().map(|l| Line::from(fit_spans(l, width))).collect()
}

fn draw_task_detail(frame: &mut Frame, area: Rect, d: &mut model::Detail, mouse: &mut model::MouseHit) {
    frame.render_widget(Clear, area);
    let (info, lines) = match &d.body {
        DetailBody::Log { info, lines } => ((**info).clone(), lines.clone()),
        // First frame, before the collector delivers the body.
        _ => (
            model::TaskInfo { id: d.id.clone(), status: "loading".into(), ..Default::default() },
            vec!["(loading…)".to_string()],
        ),
    };
    let inner = Block::bordered().inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let w = inner.width as usize;
    frame.render_widget(Paragraph::new(info_card(&info, w)), chunks[0]);

    // Body first: the tab row reports what the body ended up showing.
    let body = chunks[2];
    let text_w = body.width.saturating_sub(2).max(1);
    let height = body.height as usize;
    let current_row = d.log.matches.get(d.log.current).copied();
    let laid = layout_log(&lines, text_w as usize, &d.log, current_row);
    d.wrap_len = laid.lines.len();
    d.view_h = height;
    d.log.matches = laid.matches.clone();
    if d.log.seek && !d.log.matches.is_empty() {
        let next = d.log.matches.iter().position(|&m| m >= d.scroll).unwrap_or(d.log.matches.len() - 1);
        d.log.current = next;
        d.follow = false;
        d.scroll = d.log.matches[next].saturating_sub(height / 3);
    }
    d.log.seek = false;
    d.log.current = d.log.current.min(d.log.matches.len().saturating_sub(1));
    let (follow, scroll) = stick_scroll(d.follow, d.hold, d.scroll, laid.lines.len(), height);
    d.follow = follow;
    d.scroll = scroll;
    d.hits.clear();
    let start = d.scroll.min(laid.lines.len());
    let rows: Vec<Line> = laid.lines[start..]
        .iter()
        .take(height)
        .map(|p| Line::from(fit_spans(p.spans.clone(), text_w as usize)))
        .collect();
    let text = Rect { x: body.x, y: body.y, width: text_w, height: body.height };
    let bar = Rect { x: body.x + body.width.saturating_sub(1), y: body.y, width: 1, height: body.height };
    frame.render_widget(Paragraph::new(rows), text);
    draw_scrollbar(frame, bar, laid.lines.len(), height, d.scroll);

    // Tab row: `─┐¹output┌┐²stderr┌──────┐17 of 1203 lines┌─`.
    // The tab row joins the box border: `├─┐¹output┌┐²stderr┌───┐24 lines┌─┤`.
    let row = Rect { x: area.x, y: chunks[1].y, width: area.width, height: 1 };
    let w = row.width as usize;
    let mut tabs = vec![sp(fg(BOX_MAIN), "├─")];
    let mut hits = Vec::new();
    for (i, t) in d.tabs.iter().enumerate() {
        let name = if i == d.tab {
            sp(fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED), t.clone())
        } else {
            sp(fg(MUTED), t.clone())
        };
        let s = seg(BOX_MAIN, vec![sp(fg(HOT), ["¹", "²", "³"][i.min(2)].to_string()), name]);
        let x0 = spans_width(&tabs);
        hits.push((row.x + x0 as u16, row.x + (x0 + spans_width(&s)) as u16, i));
        tabs.extend(s);
    }
    let q = !d.log.query.is_empty();
    let count = if q && d.log.filter {
        vec![sp(fg(TEXT), laid.shown.to_string()), sp(fg(DIM), format!(" of {}", plural(laid.total, "line")))]
    } else if q {
        let n = laid.matches.len();
        let at = if n == 0 { 0 } else { d.log.current + 1 };
        vec![sp(fg(if n == 0 { ERR } else { TEXT }), format!("{at}/{n}")), sp(fg(DIM), " matching rows")]
    } else {
        let hidden = laid.total - laid.shown;
        let mut v = vec![sp(fg(TEXT), laid.total.to_string()), sp(fg(DIM), if laid.total == 1 { " line" } else { " lines" })];
        if hidden > 0 && !d.log.show_blank {
            v.push(sp(fg(DIM), format!(" · {hidden} blank hidden")));
        }
        v
    };
    let mut right = seg(BOX_MAIN, count);
    right.push(sp(fg(BOX_MAIN), "─┤"));
    let fill = w.saturating_sub(spans_width(&tabs) + spans_width(&right));
    tabs.push(sp(fg(BOX_MAIN), "─".repeat(fill)));
    tabs.extend(right);
    d.log.tab_hits = hits;
    d.log.tab_y = row.y;

    // Frame: title, follow state, and keys / search prompt in the bottom border.
    // Keys name the mode they switch to.
    let mode = if d.log.filter { "highlight" } else { "filter" };
    let (prefix, keys): (Vec<Span<'static>>, Vec<Key>) = if d.log.editing {
        (
            seg_bottom(
                BOX_MAIN,
                vec![sp(fg(ACCENT).add_modifier(Modifier::BOLD), "/ "), sp(fg(TEXT), d.log.query.clone()), sp(fg(ACCENT), "▏")],
            ),
            vec![("enter", "apply", 0), ("tab", mode, 1), ("esc", "clear", 0)],
        )
    } else {
        let mut keys: Vec<Key> = Vec::new();
        let prefix = if q {
            keys.push(("n N", "next", 1));
            seg_bottom(BOX_MAIN, vec![sp(fg(ACCENT), "/ "), sp(fg(TEXT), d.log.query.clone())])
        } else {
            keys.push(("/", "search", 1));
            Vec::new()
        };
        keys.push(("&", mode, 2));
        keys.push(("b", if d.log.show_blank { "hide blank" } else { "show blank" }, 4));
        keys.push(("1 2", "stream", 5));
        if info.running {
            keys.push(("f", if d.follow { "unfollow" } else { "follow" }, 3));
        }
        keys.push(("esc", if q { "clear" } else { "close" }, 0));
        (prefix, keys)
    };
    let pill = pos_pill(d.follow, info.running, laid.lines.len(), height, d.scroll);
    frame.render_widget(detail_block(&d.title, pill, prefix, &keys, area.width), area);
    frame.render_widget(Paragraph::new(Line::from(fit_spans(tabs, w))), row);
    mouse.detail = hit_rect(area);
    mouse.chat = HitRect::default();
    mouse.scrollbar = hit_rect(bar);
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

struct Thread<'a> {
    out: Vec<Painted>,
    last: Option<Group>,
    fold: usize,
    /// Child display name for speaker headers.
    agent: &'a str,
    width: usize,
}

impl Thread<'_> {
    /// Space the block, and open a `● agent` header when the subagent
    /// starts speaking after main (or at the top of the thread).
    fn begin(&mut self, kind: Group) {
        if self.last.is_some() && !(kind == Group::Activity && self.last == Some(Group::Activity)) {
            self.out.push(Painted::blank());
        }
        if kind != Group::User && matches!(self.last, None | Some(Group::User)) {
            let left = vec![sp(fg(OK), "● "), sp(fg(OK).add_modifier(Modifier::BOLD), self.agent.to_string())];
            self.out.push(Painted::new(fit_spans(left, self.width)));
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
    let agent = if chat.agent.is_empty() { "subagent" } else { chat.agent.as_str() };
    let mut t = Thread { out: Vec::new(), last: None, fold: 0, agent, width };
    let mut i = 0;
    while i < chat.turns.len() {
        let turn = &chat.turns[i];
        if turn.role == "user" {
            t.begin(Group::User);
            user_block(&turn.text, turn.ts, agent, width, &mut t.out);
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
            let args = turn.args.as_deref().unwrap_or("");
            tool_row(
                ToolRow { name, summary, args, output: &turn.text, ts: turn.ts, error: turn.error, pending: false },
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
            // Reasoning arrives as its own `thinking` turn; text beside a
            // tool call is what the agent said, so it stays visible.
            let mut segs = assistant_segments(&turn.text);
            let mut call_ts = vec![turn.ts; segs.len()];
            i += 1;
            // Parallel calls are logged one per line, then their results in
            // call order: take the run of call-only turns, then the run of
            // results, and pair them by name, first unused result first.
            let has_calls = segs.iter().any(|s| matches!(s, Seg::ToolCall { .. }));
            if has_calls {
                while let Some(next) = chat.turns.get(i).filter(|n| n.role == "assistant" && n.tool.is_none() && !n.error) {
                    let more = assistant_segments(&next.text);
                    if more.is_empty() || !more.iter().all(|s| matches!(s, Seg::ToolCall { .. })) {
                        break;
                    }
                    call_ts.extend(std::iter::repeat_n(next.ts, more.len()));
                    segs.extend(more);
                    i += 1;
                }
            }
            let first_result = i;
            while has_calls && chat.turns.get(i).is_some_and(|n| n.tool.is_some()) {
                i += 1;
            }
            let mut used = vec![false; i - first_result];
            for (seg, ts) in segs.into_iter().zip(call_ts) {
                match seg {
                    Seg::Text(text) => {
                        t.begin(Group::Text);
                        assistant_text(&text, error, width, &mut t.out);
                    }
                    Seg::ToolCall { name, args } => {
                        let summary = summary_from_raw(&args);
                        t.begin(Group::Activity);
                        let id = t.next_fold();
                        let hit = (first_result..i)
                            .find(|&k| !used[k - first_result] && chat.turns[k].tool.as_deref() == Some(name.as_str()));
                        let row = match hit {
                            Some(k) => {
                                used[k - first_result] = true;
                                let result = &chat.turns[k];
                                ToolRow {
                                    name: &name,
                                    summary: &summary,
                                    args: &args,
                                    output: &result.text,
                                    ts: result.ts.or(ts),
                                    error: result.error,
                                    pending: false,
                                }
                            }
                            None => ToolRow { name: &name, summary: &summary, args: &args, output: "", ts, error: false, pending: true },
                        };
                        tool_row(row, is_open(id, open, expand_all), id, width, &mut t.out);
                    }
                }
            }
            // A result no call claimed (its call fell outside the window).
            for k in (first_result..i).filter(|&k| !used[k - first_result]) {
                let r = &chat.turns[k];
                t.begin(Group::Activity);
                let id = t.next_fold();
                let name = r.tool.as_deref().unwrap_or("tool");
                let args = r.args.as_deref().unwrap_or("");
                let row = ToolRow { name, summary: "", args, output: &r.text, ts: r.ts, error: r.error, pending: false };
                tool_row(row, is_open(id, open, expand_all), id, width, &mut t.out);
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

fn clock_spans(ts: Option<u64>) -> Vec<Span<'static>> {
    match clock(ts) {
        t if t.is_empty() => Vec::new(),
        t => vec![sp(fg(DIM), t)],
    }
}

/// Message from main to the child: an accent rail down the left edge under a
/// `main → agent` header that carries the clock.
fn user_block(text: &str, ts: Option<u64>, agent: &str, width: usize, out: &mut Vec<Painted>) {
    let head = vec![
        sp(fg(ACCENT), "┃ "),
        sp(fg(ACCENT).add_modifier(Modifier::BOLD), "main"),
        sp(fg(DIM), format!(" → {agent}")),
    ];
    out.push(Painted::new(split_line(head, clock_spans(ts), width)));
    let inner = width.saturating_sub(MARGIN).max(1);
    let body = if text.trim().is_empty() { " " } else { text.trim() };
    for l in fmt::wrap_width(body, inner) {
        out.push(Painted::new(vec![sp(fg(ACCENT), "┃ "), sp(Style::default(), l)]));
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
    out.push(Painted { spans: split_line(left, clock_spans(ts), width), fold: Some(fold_hit(fold)) });
    if expanded {
        hang(&fmt::scrub_tty(text.trim()), style, width, out);
    }
}

struct ToolRow<'a> {
    name: &'a str,
    summary: &'a str,
    /// Raw call arguments (JSON, maybe cut), for write / edit previews.
    args: &'a str,
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
    let preview = call_preview(row.name, row.args);
    let output = row.output.trim_end();
    let has_body = preview.is_some() || (!row.pending && !output.trim().is_empty());
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
    // The stat sits with the clock so a long path truncates, not the stat.
    let mut right = preview.as_ref().map(|p| p.stat.clone()).unwrap_or_default();
    let time = clock(row.ts);
    if !time.is_empty() {
        if !right.is_empty() {
            right.push(Span::raw("  "));
        }
        right.push(sp(fg(DIM), time));
    }
    let room = width.saturating_sub(spans_width(&right) + 2);
    let spans = split_line(fit_spans(left, room), right, width);
    out.push(Painted { spans, fold: has_body.then(|| fold_hit(fold)) });
    if !(has_body && expanded) {
        return;
    }
    if let Some(p) = &preview {
        let gutter_w = p.body.iter().map(|(g, _, _)| fmt::display_width(g)).max().unwrap_or(0);
        let inner = width.saturating_sub(HANG + gutter_w).max(1);
        let mut rows = 0;
        'outer: for (gutter, style, text) in &p.body {
            for (j, seg) in fmt::wrap_width(&fmt::scrub_tty(text), inner).into_iter().enumerate() {
                if rows == PREVIEW_ROWS {
                    break 'outer;
                }
                // Headers (no gutter) sit flush; gutters align under each other.
                let g = if gutter.is_empty() {
                    String::new()
                } else if j == 0 {
                    format!("{gutter:>gutter_w$}")
                } else {
                    " ".repeat(gutter_w)
                };
                let gstyle = if gutter.trim().chars().all(|c| c.is_ascii_digit()) { fg(DIM) } else { *style };
                out.push(Painted::new(vec![
                    margin(),
                    sp(fg(RULE), "│ "),
                    sp(gstyle, g),
                    sp(*style, seg),
                ]));
                rows += 1;
            }
        }
        let more = p.body.len().saturating_sub(rows.min(p.body.len()));
        if rows == PREVIEW_ROWS && more > 0 {
            out.push(Painted::new(vec![margin(), sp(fg(RULE), "│ "), sp(fg(DIM), format!("… {} more", plural(more, "line")))]));
        }
        if p.cut {
            out.push(Painted::new(vec![margin(), sp(fg(RULE), "│ "), sp(fg(DIM), "… cut short in the transcript")]));
        }
        if !row.pending && !output.trim().is_empty() {
            let style = if row.error { fg(ERR) } else { fg(DIM) };
            hang(&fmt::scrub_tty(output), style, width, out);
        }
        return;
    }
    let style = if row.error { fg(ERR) } else { fg(MUTED) };
    hang(&fmt::scrub_tty(output), style, width, out);
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
    /// `args` is the raw JSON, possibly cut by the transcript's line cap.
    ToolCall { name: String, args: String },
}

fn assistant_segments(text: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(rel) = text[i..].find("tool ") {
            let start = i + rel;
            if let Some((end, name, args)) = parse_tool_call_at(&text[start..]) {
                let prev = text[i..start].trim();
                if !prev.is_empty() {
                    segs.push(Seg::Text(prev.to_string()));
                }
                segs.push(Seg::ToolCall { name, args });
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
    match take_json_object(json) {
        Some(json_len) => Some((json_start + json_len, name.to_string(), json[..json_len].to_string())),
        // The transcript caps long lines (`… (N chars)`), cutting big write /
        // edit payloads mid-JSON: keep what is there and read it leniently.
        None => Some((s.len(), name.to_string(), strip_cap_suffix(json).to_string())),
    }
}

/// Drop the transcript's `… (3666 chars)` cap marker.
fn strip_cap_suffix(s: &str) -> &str {
    match s.rfind("… (") {
        Some(i) if s[i..].trim_end().ends_with(" chars)") => &s[..i],
        _ => s,
    }
}

/// Every string value of `key`, in order, from JSON that may be cut short.
/// Escapes are decoded; the flag says whether the string was closed.
fn json_strings(json: &str, key: &str) -> Vec<(String, bool)> {
    let pat = format!("\"{key}\"");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = json[from..].find(&pat) {
        let mut rest = json[from + at + pat.len()..].trim_start();
        from += at + pat.len();
        let Some(r) = rest.strip_prefix(':') else { continue };
        rest = r.trim_start();
        let Some(r) = rest.strip_prefix('"') else { continue };
        let mut value = String::new();
        let mut closed = false;
        let mut chars = r.char_indices();
        while let Some((_, c)) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => match chars.next().map(|(_, e)| e) {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some('r') => {}
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).map(|(_, h)| h).collect();
                        if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            value.push(ch);
                        }
                    }
                    Some(other) => value.push(other),
                    None => break,
                },
                c => value.push(c),
            }
        }
        out.push((value, closed));
    }
    out
}

/// What a write / edit call changes, for its row and unfolded preview.
struct CallPreview {
    /// Right-hand stat: `+62 lines`, `−3 +5`.
    stat: Vec<Span<'static>>,
    /// Unfolded body rows: (gutter, style, text).
    body: Vec<(String, Style, String)>,
    /// The transcript cut the payload short.
    cut: bool,
}

const PREVIEW_ROWS: usize = 60;

fn call_preview(name: &str, args: &str) -> Option<CallPreview> {
    match name {
        "write" => {
            let (content, closed) = json_strings(args, "content").into_iter().next()?;
            let lines: Vec<&str> = content.lines().collect();
            let digits = lines.len().max(1).to_string().len();
            let body = lines
                .iter()
                .enumerate()
                .map(|(i, l)| (format!("{:>digits$} ", i + 1), Style::default(), l.to_string()))
                .collect();
            let n = if closed { format!("+{}", plural(lines.len(), "line")) } else { format!("+{}+ lines", lines.len()) };
            Some(CallPreview { stat: vec![sp(fg(OK), n)], body, cut: !closed })
        }
        "edit" => {
            let olds = json_strings(args, "oldText");
            let news = json_strings(args, "newText");
            if olds.is_empty() && news.is_empty() {
                return None;
            }
            let pairs = olds.len().max(news.len());
            let (mut minus, mut plus, mut cut) = (0, 0, false);
            let mut body = Vec::new();
            for k in 0..pairs {
                if pairs > 1 {
                    body.push((String::new(), fg(DIM), format!("@@ edit {}/{pairs}", k + 1)));
                }
                if let Some((old, closed)) = olds.get(k) {
                    cut |= !closed;
                    for l in old.lines() {
                        minus += 1;
                        body.push(("- ".into(), fg(ERR), l.to_string()));
                    }
                }
                if let Some((new, closed)) = news.get(k) {
                    cut |= !closed;
                    for l in new.lines() {
                        plus += 1;
                        body.push(("+ ".into(), fg(OK), l.to_string()));
                    }
                }
            }
            let mut stat = vec![sp(fg(ERR), format!("−{minus}")), sp(fg(OK), format!(" +{plus}"))];
            if pairs > 1 {
                stat.push(sp(fg(DIM), format!(" · {pairs} edits")));
            }
            Some(CallPreview { stat, body, cut })
        }
        _ => None,
    }
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

fn summary_from_raw(json: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        return pick_summary(&v);
    }
    extract_json_string(json, "path")
        .or_else(|| extract_json_string(json, "command"))
        .or_else(|| extract_json_string(json, "query"))
        .or_else(|| extract_json_string(json, "pattern"))
        .unwrap_or_default()
}

fn pick_summary(v: &serde_json::Value) -> String {
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
        let summary = summary_from_raw(args);
        if !summary.is_empty() {
            return format!("{name} {summary}");
        }
    }
    name.to_string()
}

fn tool_call_summary(text: &str) -> Option<(String, String)> {
    parse_tool_call_at(text.trim()).map(|(_, name, args)| (name, summary_from_raw(&args)))
}

/// The one status mapping, shared with the pi extension's `statusGlyph`:
/// running ● green, pending ○ dim, done ✓, failed ✗ red, stopped / timeout ■
/// amber; sessions: connected ● accent. Finished rows keep a quiet word.
fn status_color(status: &str, running: bool) -> Color {
    if running || status == "running" {
        OK
    } else if matches!(status, "failed" | "killed" | "orphaned" | "interrupted") {
        ERR
    } else if status == "connected" {
        ACCENT
    } else if matches!(status, "timeout" | "stopped" | "partial") || status.starts_with("stopped:") {
        WARN
    } else {
        DIM
    }
}

/// Glyph color: like the word, except a done ✓ is green (the word stays dim).
fn glyph_color(status: &str, running: bool) -> Color {
    if status == "completed" && !running {
        OK
    } else {
        status_color(status, running)
    }
}

fn status_glyph(status: &str, running: bool) -> &'static str {
    if running || status == "running" || status == "connected" {
        "●"
    } else if matches!(status, "failed" | "killed" | "orphaned" | "interrupted") {
        "✗"
    } else if status == "completed" {
        "✓"
    } else if matches!(status, "timeout" | "stopped" | "partial") || status.starts_with("stopped:") {
        "■"
    } else {
        "○"
    }
}

/// Styled cell: the tree label gets chevron + status glyph, status words are
/// colored, and placeholders are dim.
fn cell_spans(state: &TopState, vr: &VisibleRow, col: Col) -> Vec<Span<'static>> {
    // Flat task table: the id carries the same status glyph as the tree.
    if state.tab == Tab::Tasks && col == Col::Id {
        if let Some(&i) = state.task_index.get(&vr.id) {
            let r = &state.task_rows[i];
            let glyph = if r.running { spin_glyph() } else { status_glyph(&r.status, false) };
            return vec![sp(fg(glyph_color(&r.status, r.running)), format!("{glyph} ")), sp(fg(MUTED), r.id.clone())];
        }
    }
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
        sp(fg(glyph_color(&n.status, n.metrics.running)), format!("{} ", row_glyph(n))),
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
            assert!(text.contains("tree") || text.contains("pi-famulus"), "{c}x{r}: {text}");
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
            agent: "worker".into(),
            note: None,
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
    fn parallel_calls_pair_with_their_own_results() {
        let turns = vec![
            mk_turn("assistant", r#"tool read {"path":"a.rs"}"#),
            mk_turn("assistant", r#"tool read {"path":"b.rs"}"#),
            mk_turn("assistant", r#"tool bash {"command":"ls"}"#),
            tool_turn("read", None, "AAA"),
            tool_turn("bash", None, "LS"),
            tool_turn("read", None, "BBB"),
        ];
        let text = texts(&layout(&chat_body(turns), 60, true));
        let pos = |needle: &str| text.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle}: {text:?}"));
        assert!(pos("a.rs") < pos("AAA") && pos("AAA") < pos("b.rs"), "{text:?}");
        assert!(pos("b.rs") < pos("BBB") && pos("BBB") < pos("bash"), "{text:?}");
        assert!(pos("bash") < pos("LS"), "{text:?}");
        assert!(!text.iter().any(|l| l.contains("running")), "no call left pending: {text:?}");
        assert_eq!(text.iter().filter(|l| l.contains('✓')).count(), 3, "{text:?}");
    }

    #[test]
    fn stacked_tools_are_tight_one_line_rows() {
        let long = "x".repeat(80);
        let args = format!(r#"{{"command":"{long}"}}"#);
        let tools: Vec<TranscriptTurn> = (0..3).map(|_| tool_turn("bash", Some(&args), "ok")).collect();
        let painted = layout(&chat_body(tools), 48, false);
        assert_fits(&painted, 48);
        assert_eq!(painted.len(), 4, "no gaps between activity rows: {:?}", texts(&painted));
        assert_eq!(painted[0].text(), "● worker");
        for p in &painted[1..] {
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
        assert_eq!(
            text,
            vec!["┃ main → worker", "┃ go", "", "● worker", "  ▸ ∴ Thinking", "  ▸ ✓ read  a.rs", "", "  done"],
        );
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
        let body: Vec<_> = painted.iter().skip(2).map(|p| p.text()).collect();
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
        assert!(painted.iter().any(|p| p.fold.is_some()), "the cut write still unfolds to its content");
        let open = layout(&chat_body(vec![mk_turn("assistant", dumped)]), 60, true);
        let text = texts(&open).join("\n");
        assert!(text.contains("from pathlib import Path") && text.contains("import subprocess"), "{text}");
    }

    #[test]
    fn capped_write_and_multi_edit_show_stats_and_previews() {
        // The transcript cut this write mid-content (see `capText`).
        let cut = format!(
            "tool write {{\"path\":\"storage/final_verify.py\",\"content\":\"from pathlib import Path\\nprint(1)\\n{}… (3666 chars)",
            "x = 1\\n".repeat(3)
        );
        let painted = layout(&chat_body(vec![mk_turn("assistant", &cut)]), 80, false);
        let row = texts(&painted).into_iter().find(|l| l.contains("write")).expect("write row");
        assert!(row.contains("storage/final_verify.py") && row.contains("+5+ lines"), "{row}");
        assert!(!row.contains("pathlib"), "collapsed rows never dump content: {row}");
        let open = texts(&layout(&chat_body(vec![mk_turn("assistant", &cut)]), 80, true)).join("\n");
        assert!(open.contains("1 from pathlib import Path") && open.contains("cut short"), "{open}");

        let edit = r#"tool edit {"path":"src/a.rs","edits":[{"oldText":"let a = 1;","newText":"let a = 2;\nlet b = 3;"},{"oldText":"x","newText":"y"}]}"#;
        let result = tool_turn("edit", None, "Successfully replaced 2 blocks");
        let painted = layout(&chat_body(vec![mk_turn("assistant", edit), result]), 80, true);
        let text = texts(&painted);
        let row = text.iter().find(|l| l.contains("edit")).unwrap();
        assert!(row.contains("src/a.rs") && row.contains("−2 +3") && row.contains("2 edits"), "{row}");
        assert!(text.iter().any(|l| l == "  │ @@ edit 1/2"), "headers sit flush: {text:?}");
        assert!(text.iter().any(|l| l.contains("- let a = 1;")) && text.iter().any(|l| l.contains("+ let b = 3;")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("Successfully replaced")), "{text:?}");
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
        assert!(text[0].starts_with("┃ main → worker") && text[0].contains(':'), "{text:?}");
        assert_eq!(text[1], "┃ look at src");
        assert_eq!(text[3], "● worker", "the agent header does not repeat the clock");
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
        assert!(c.contains("need the files"), "prose beside a tool call stays visible: {c}");
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
        assert_eq!(lines[0], "● worker");
        assert_eq!(lines[1], "  Plan");
        assert_eq!(lines[2], "  • run cargo test now");
        assert_eq!(lines[3], "  • ship it");
        assert_eq!(lines[4], "  │ fn main() {}");
        let code = painted[2].spans.iter().find(|s| s.content == "cargo test").expect("code span");
        assert_eq!(code.style.fg, Some(CODE));
        let strong = painted[3].spans.iter().find(|s| s.content == "ship").expect("bold span");
        assert!(strong.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn assistant_error_is_marked() {
        let painted = layout(&chat_body(vec![mk_turn("assistant error", "529 overloaded")]), 40, false);
        assert_eq!(texts(&painted), vec!["● worker".to_string(), "  ✗ 529 overloaded".to_string()]);
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
        assert_eq!(d.title, "ch_a · worker", "detail titles are `id · label`");
        d.body = DetailBody::Conversation(Chat {
            turns: vec![mk_turn("user", "inspect the loader")],
            agent: "worker".into(),
            note: None,
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
    fn footer_explains_missing_usage_instead_of_dashes() {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        state.selected = Some("ch_a".into());
        let mut d = open_detail(&state).expect("agent opens");
        d.body = DetailBody::Conversation(Chat {
            turns: vec![mk_turn("user", "go")],
            agent: "worker".into(),
            note: Some("usage not recorded: this session runs extension 0.1.3".into()),
            stats: vec![("-".into(), "in".into()), ("12".into(), "shells".into())],
            running: false,
        });
        state.detail = Some(d);
        let text = render(120, 24, &mut state);
        assert!(text.contains("usage not recorded") && text.contains("12 shells"), "{text}");
        assert!(!text.contains("- in"), "{text}");
    }

    fn log_lines() -> Vec<String> {
        vec![
            "GET /api/apps STATUS=200 DUR_MS=2843".into(),
            "".into(),
            "POST /api/verify STATUS=500 DUR_MS=130617".into(),
            "".into(),
            "\x1b[32mGET\x1b[0m /api/apps STATUS=200 DUR_MS=8038\r".into(),
        ]
    }

    #[test]
    fn search_is_smart_case_with_alternatives() {
        let q = parse_query("verify|status=500").unwrap();
        assert_eq!(match_ranges("POST /api/verify STATUS=500", &q), vec![(10, 16), (17, 27)]);
        let strict = parse_query("Verify").unwrap();
        assert!(match_ranges("POST /api/verify", &strict).is_empty(), "uppercase makes it case-sensitive");
        assert_eq!(match_ranges("aaaa", &parse_query("aa|aaa").unwrap()), vec![(0, 4)], "overlaps merge");
        assert!(parse_query(" | ").is_none());
    }

    #[test]
    fn task_output_hides_blank_lines_and_filters_or_highlights() {
        let lines = log_lines();
        let mut view = model::LogView::default();
        let laid = layout_log(&lines, 80, &view, None);
        let text: Vec<String> = laid.lines.iter().map(|p| p.text()).collect();
        assert_eq!(laid.lines.len(), 3, "blank lines hidden: {text:?}");
        assert!(text[2].starts_with(" GET /api/apps") && !text[2].contains('\x1b'), "{text:?}");
        assert_eq!((laid.shown, laid.total), (3, 5));

        view.show_blank = true;
        assert_eq!(layout_log(&lines, 80, &view, None).lines.len(), 5);

        view.show_blank = false;
        view.query = "get".into();
        let laid = layout_log(&lines, 80, &view, None);
        assert_eq!(laid.matches, vec![0, 2], "highlight keeps every line, marks matching rows");
        let hit = laid.lines[0].spans.iter().find(|s| s.content == "GET").expect("match span");
        assert_eq!(hit.style.bg, Some(HIT));

        view.filter = true;
        let laid = layout_log(&lines, 80, &view, Some(0));
        assert_eq!(laid.lines.len(), 2, "filter drops non-matching lines");
        assert_eq!(laid.lines[0].spans.iter().find(|s| s.content == "GET").unwrap().style.bg, Some(HIT_CURRENT));

        view.query = "nothing-like-this".into();
        assert_eq!(layout_log(&lines, 80, &view, None).lines[0].text(), " (no line matches)");
    }

    fn task_detail(lines: Vec<String>) -> TopState {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        state.selected = Some("sh_live".into());
        let mut d = open_detail(&state).expect("task opens");
        let info = crate::top::collector::task_info_for_test(&snap.tasks[0], 10_000);
        d.body = DetailBody::Log { info: Box::new(info), lines };
        state.detail = Some(d);
        state
    }

    #[test]
    fn task_detail_pins_info_and_shows_stream_tabs() {
        let mut state = task_detail(log_lines());
        let text = render(110, 20, &mut state);
        assert!(!text.contains("Info"), "info is pinned, not a tab: {text}");
        assert!(text.contains("¹output") && text.contains("²stderr"), "{text}");
        assert!(text.contains("running") && text.contains("$ pytest -n 4") && text.contains("cwd /tmp/app"), "{text}");
        assert!(text.contains("5 lines · 2 blank hidden"), "{text}");
        assert!(text.contains("/ search") && text.contains("& filter"), "{text}");
        let d = state.detail.as_ref().unwrap();
        assert_eq!(d.log.tab_hits.len(), 2);
        let row: Vec<&str> = text.lines().collect();
        let tab_line = row[d.log.tab_y as usize];
        let col = fmt::display_width(&tab_line[..tab_line.find("stderr").unwrap()]) as u16;
        let (x0, x1, tab) = d.log.tab_hits[1];
        assert!(x0 <= col && col < x1 && tab == 1, "{tab_line:?} {:?}", d.log.tab_hits);
    }

    #[test]
    fn search_seeks_to_the_first_match_and_reports_counts() {
        let mut lines: Vec<String> = (0..60).map(|i| format!("line {i} ok")).collect();
        lines[42] = "line 42 FAILED here".into();
        let mut state = task_detail(lines);
        {
            let d = state.detail.as_mut().unwrap();
            d.follow = false;
            d.scroll = 0;
            d.log.query = "failed".into();
            d.log.seek = true;
        }
        let text = render(100, 20, &mut state);
        let d = state.detail.as_ref().unwrap();
        assert_eq!(d.log.matches, vec![42]);
        assert!(!d.follow && d.scroll <= 42 && 42 < d.scroll + d.view_h, "match in view: scroll {}", d.scroll);
        assert!(text.contains("1/1 matching rows") && text.contains("/ failed"), "{text}");
    }

    #[test]
    fn narrow_footers_keep_help_quit_esc_and_position() {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        state.message = Some("stopped sh_x".into());
        let text = render(80, 24, &mut state);
        let bottom = text.lines().last().unwrap();
        assert!(bottom.contains("? help") && bottom.contains("q quit") && bottom.contains("1/4"), "{bottom}");
        assert!(bottom.contains("stopped sh_x"), "messages sit beside the keys: {bottom}");
        for (w, h) in [(40u16, 12u16), (60, 20)] {
            let mut state = task_detail(log_lines());
            let text = render(w, h, &mut state);
            let bottom = text.lines().last().unwrap();
            assert!(bottom.contains("esc"), "{w}x{h}: {bottom}");
            assert!(!text.lines().next().unwrap().contains("┌ following"), "{w}x{h}: pill collides with the title");
        }
    }

    #[test]
    fn fit_keys_drops_least_important_first() {
        let keys: Vec<Key> = vec![("a", "one", 3), ("b", "two", 0), ("c", "three", 1)];
        let all: String = fit_keys(Vec::new(), &keys, 200).iter().map(|s| s.content.to_string()).collect();
        assert!(all.contains("a one") && all.contains("c three"));
        let some: String = fit_keys(Vec::new(), &keys, 22).iter().map(|s| s.content.to_string()).collect();
        assert!(!some.contains("a one") && some.contains("b two") && some.contains("c three"), "{some}");
    }

    #[test]
    fn tab_bar_clicks_land_on_tabs_scope_and_sort() {
        let snap = snap();
        let mut state = TopState::new(1_000, None, None);
        state.apply(&snap, &[], &HashMap::new());
        let text = render(120, 30, &mut state);
        assert!(text.contains("tree") && text.contains("agents") && text.contains("scope"), "{text}");
        let hits = state.mouse.tab_hits.clone();
        let agents = hits.iter().find(|h| h.2 == TabHit::Tab(Tab::Agents)).expect("agents hit");
        let scope = hits.iter().find(|h| h.2 == TabHit::Scope).expect("scope hit");
        let sort = hits.iter().find(|h| h.2 == TabHit::Sort).expect("sort hit");
        assert!(agents.0 < agents.1 && agents.1 <= scope.0 && scope.1 <= sort.0, "{hits:?}");
        let line = text.lines().find(|l| l.contains("¹tree")).unwrap();
        let col = |needle: &str| fmt::display_width(&line[..line.find(needle).unwrap()]) as u16;
        assert!(agents.0 <= col("agents") && col("agents") < agents.1, "{line:?} {hits:?}");
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
        assert!(state.wants_anim(), "the task table spins running ids too");
        state.set_tab(Tab::Events);
        assert!(!state.wants_anim(), "events have no glyphs to spin");
    }

    #[test]
    fn follow_resticks_at_the_bottom() {
        assert_eq!(stick_scroll(true, false, 3, 40, 10), (true, 30));
        assert_eq!(stick_scroll(false, false, 12, 40, 10), (false, 12));
        assert_eq!(stick_scroll(false, false, 30, 40, 10), (true, 30));
        assert_eq!(stick_scroll(false, false, 99, 40, 10), (true, 30));
        // `f` at the end holds there instead of re-following on the next draw.
        assert_eq!(stick_scroll(false, true, 30, 40, 10), (false, 30));
        assert_eq!(stick_scroll(false, true, 99, 40, 10), (false, 30));
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
