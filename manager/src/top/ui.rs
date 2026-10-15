//! Ratatui drawing for interactive `top`. Pure function of [`TopState`] and
//! the frame `Rect`; no I/O.

use crate::fmt;
use crate::top::model::{
    self, agent_cols, compact_num, cpu_cell, header_height, height_class, now_cell, task_cols, tok_io_cell,
    tree_cols, wall_cell, width_class, Col, HeightClass, Node, NodeKind, Tab, TopState, VisibleRow, WidthClass,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use ratatui::Frame;

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
    if let Some(d) = &state.detail {
        draw_detail(frame, area, d);
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
    let tabs = format!(
        " [1:Tree ({})]  2:Agents ({})  3:Tasks ({})  4:Events ── Scope:{}(a) Sort:{}{}",
        count_tab(state, Tab::Tree),
        count_tab(state, Tab::Agents),
        count_tab(state, Tab::Tasks),
        state.scope.label(),
        state.sort.label(),
        if state.sort_desc { "▼" } else { "▲" },
    );
    let marked = match state.tab {
        Tab::Tree => tabs.replacen(" [1:", " [", 1),
        Tab::Agents => tabs.replace("  2:", " [2:").replace("  3:", "]  3:"),
        Tab::Tasks => tabs.replace("  3:", " [3:").replace("  4:", "]  4:"),
        Tab::Events => tabs.replace("  4:", " [4:"),
    };
    // Highlight the active tab by wrapping its label; keep it one line.
    let tab_line = match state.tab {
        Tab::Tree => format!(" [1:Tree ({})]  2:Agents ({})  3:Tasks ({})  4:Events ── Scope:{}(a) Sort:{}{}", count_tab(state, Tab::Tree), count_tab(state, Tab::Agents), count_tab(state, Tab::Tasks), state.scope.label(), state.sort.label(), if state.sort_desc { "▼" } else { "▲" }),
        Tab::Agents => format!("  1:Tree ({})  [2:Agents ({})]  3:Tasks ({})  4:Events ── Scope:{}(a) Sort:{}{}", count_tab(state, Tab::Tree), count_tab(state, Tab::Agents), count_tab(state, Tab::Tasks), state.scope.label(), state.sort.label(), if state.sort_desc { "▼" } else { "▲" }),
        Tab::Tasks => format!("  1:Tree ({})  2:Agents ({})  [3:Tasks ({})]  4:Events ── Scope:{}(a) Sort:{}{}", count_tab(state, Tab::Tree), count_tab(state, Tab::Agents), count_tab(state, Tab::Tasks), state.scope.label(), state.sort.label(), if state.sort_desc { "▼" } else { "▲" }),
        Tab::Events => format!("  1:Tree ({})  2:Agents ({})  3:Tasks ({})  [4:Events] ── Scope:{}(a) Sort:{}{}", count_tab(state, Tab::Tree), count_tab(state, Tab::Agents), count_tab(state, Tab::Tasks), state.scope.label(), state.sort.label(), if state.sort_desc { "▼" } else { "▲" }),
    };
    let _ = marked;
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

fn tree_or_agent_cols(tab: Tab, w: WidthClass) -> &'static [Col] {
    if tab == Tab::Agents {
        agent_cols(w)
    } else {
        tree_cols(w)
    }
}

fn count_tab(state: &TopState, tab: Tab) -> usize {
    match tab {
        Tab::Tree => state.roots.len(),
        Tab::Agents => state.nodes.values().filter(|n| matches!(n.kind, NodeKind::Agent | NodeKind::Main)).count(),
        Tab::Tasks => state.task_rows.len(),
        Tab::Events => state.events.len(),
    }
}

fn draw_table(frame: &mut Frame, area: Rect, state: &mut TopState, cols: &[Col]) {
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
        let Some(ev) = state.events.iter().find(|e| format!("{}:{}:{}", e.ts, e.ty, e.id) == vr.id) else {
            continue;
        };
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
    let start = state.preview_lines.len().saturating_sub(inner.height as usize);
    let lines: Vec<Line> = state.preview_lines[start..]
        .iter()
        .map(|l| Line::from(clip(l, inner.width)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
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
x Ctrl+X F9         stop task      ?            this help
q Ctrl+C            quit           Esc          close overlay";
    let rect = centered(area, 64, 16);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(help).block(Block::default().borders(Borders::ALL).title(" keys ")),
        rect,
    );
}

fn draw_detail(frame: &mut Frame, area: Rect, d: &model::Detail) {
    let rect = area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 1 });
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(format!(" {} ", d.title));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
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
    let head = format!(" {tabs}{follow}  ·  f toggle follow · Esc close");
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(Paragraph::new(clip(&head, chunks[0].width)), chunks[0]);
    let start = d.scroll.min(d.lines.len());
    let lines: Vec<Line> = d.lines[start..]
        .iter()
        .take(chunks[1].height as usize)
        .map(|l| Line::from(clip(l, chunks[1].width)))
        .collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

fn cell_text(state: &TopState, vr: &VisibleRow, col: Col) -> String {
    if state.tab == Tab::Tasks {
        if let Some(r) = state.task_rows.iter().find(|r| r.id == vr.id) {
            return task_cell(state, r, col);
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
}
