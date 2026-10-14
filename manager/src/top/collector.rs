//! Background snapshot + preview tail for interactive `top`.
//!
//! Always uses [`crate::inspect::Live::IfRunning`]: this command never
//! starts the daemon. Preview/detail reads are capped at 64 KiB.

use crate::events::EventLine;
use crate::inspect::{self, Live, Snapshot};
use crate::task;
use crate::top::model::{Detail, NodeKind, TopState};
use std::collections::HashMap;
use std::path::Path;

const TAIL: u64 = 64 * 1024;

pub struct Tick {
    pub snap: Snapshot,
    pub events: Vec<EventLine>,
    pub kind_slots: HashMap<String, (usize, usize)>,
    pub preview_lines: Vec<String>,
    pub detail_lines: Option<Vec<String>>,
}

pub async fn collect(home: &Path, state: &TopState) -> Result<Tick, String> {
    let snap = inspect::snapshot(home, Live::IfRunning).await?;
    let (events, _bad) = inspect::load_events(home, state.session_prefix.as_deref());
    let kind_slots = kind_slots(home, &snap);
    let preview_lines = if state.preview {
        tail_for(home, &snap, state.selected.as_deref(), 0, false)
    } else {
        Vec::new()
    };
    let detail_lines = state.detail.as_ref().map(|d| tail_for(home, &snap, Some(&d.id), d.tab, d.follow));
    Ok(Tick {
        snap,
        events,
        kind_slots,
        preview_lines,
        detail_lines,
    })
}

fn kind_slots(home: &Path, snap: &Snapshot) -> HashMap<String, (usize, usize)> {
    if let Some(cap) = snap.daemon.as_ref().and_then(|d| d.agent_capacity.as_ref()) {
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

fn tail_for(home: &Path, snap: &Snapshot, id: Option<&str>, tab: usize, _follow: bool) -> Vec<String> {
    let Some(id) = id else {
        return Vec::new();
    };
    if let Some(t) = snap.tasks.iter().find(|t| t.task_id == id) {
        return match tab {
            1 => tail_file(&task::stderr_path_for(Path::new(&t.output_path))),
            2 => info_task(t, snap.now),
            _ => tail_file(Path::new(&t.output_path)),
        };
    }
    if let Some(a) = snap.agents.iter().find(|a| a.child_id == id) {
        return match tab {
            1 => a.result_tail.as_deref().unwrap_or("(no result)").lines().map(|s| s.to_string()).collect(),
            2 => info_agent(a, snap),
            _ => tail_transcript(home, a),
        };
    }
    Vec::new()
}

fn tail_file(path: &Path) -> Vec<String> {
    let Ok(meta) = std::fs::metadata(path) else {
        return vec![format!("(no file {})", path.display())];
    };
    let off = meta.len().saturating_sub(TAIL);
    match task::read_file_range(path, off, TAIL as usize) {
        Ok((bytes, _)) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
            if lines.is_empty() {
                vec!["(empty)".into()]
            } else {
                lines
            }
        }
        Err(e) => vec![format!("(read error: {e})")],
    }
}

fn tail_transcript(home: &Path, a: &inspect::AgentRecord) -> Vec<String> {
    let path = a.transcript_path(home);
    let raw = tail_file(&path);
    let mut first_user = true;
    let mut out = Vec::new();
    for line in raw {
        if line.starts_with("(no file") || line == "(empty)" {
            out.push(line);
            continue;
        }
        if let Some(rendered) = inspect::render_transcript_line(&line, false, a.prompt_head.as_deref(), &mut first_user) {
            out.push(rendered);
        }
    }
    if out.is_empty() {
        out.push("(empty transcript)".into());
    }
    out
}

fn info_task(t: &crate::proto::TaskRecord, now: u64) -> Vec<String> {
    let wall = t.ended_at.unwrap_or(now).saturating_sub(t.started_at);
    let cpu = if t.status == crate::proto::TaskStatus::Running {
        inspect::cpu_ms(t.live_cpu_user_ms, t.live_cpu_sys_ms)
    } else {
        inspect::cpu_ms(t.cpu_user_ms, t.cpu_sys_ms)
    };
    vec![
        format!("id           {}", t.task_id),
        format!("session      {}", t.session_id),
        format!("status       {:?}", t.status),
        format!("cwd          {}", t.cwd),
        format!("pid          {}", t.pid),
        format!("wall         {}", crate::fmt::human_duration(wall)),
        format!("cpu          {}", inspect::cpu_text(cpu)),
        format!("now          {}", t.live_cpu_percent.map(|p| format!("{p:.1}%")).unwrap_or_else(|| "-".into())),
        format!("reason       {}", t.end_reason.as_deref().unwrap_or("-")),
        format!("work kind    {}", inspect::work_kind(t)),
        format!("command      {}", crate::fmt::first_line(&t.command)),
        format!("output       {}", t.output_path),
    ]
}

fn info_agent(a: &inspect::AgentRecord, snap: &Snapshot) -> Vec<String> {
    let shells = snap
        .tasks
        .iter()
        .filter(|t| t.origin.as_ref().and_then(|o| o.child_id.as_deref()) == Some(a.child_id.as_str()))
        .count();
    vec![
        format!("id           {}", a.child_id),
        format!("name         {}", a.name),
        format!("agent        {}", a.agent),
        format!("model        {}", a.model.as_deref().unwrap_or("-")),
        format!("status       {}", a.status),
        format!("run          {}", a.run_id.as_deref().unwrap_or("-")),
        format!("session      {}", a.session_id),
        format!("tokens       {} in / {} out", a.tokens_input.map(|n| n.to_string()).unwrap_or_else(|| "-".into()), a.tokens_output.map(|n| n.to_string()).unwrap_or_else(|| "-".into())),
        format!("tok/s        {}", inspect::agent_output_tokens_per_second(a.tokens_output, a.llm_ms).map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into())),
        format!("wall         LLM {} / tool {} / queue {}", dur(a.llm_ms), dur(a.tool_ms), dur(a.queue_ms)),
        format!("shells       {shells}"),
        format!("error        {}", a.error.as_deref().unwrap_or("-")),
        format!("prompt       {}", a.prompt_head.as_deref().unwrap_or("-")),
    ]
}

fn dur(ms: Option<u64>) -> String {
    ms.map(crate::fmt::human_duration).unwrap_or_else(|| "-".into())
}

pub fn detail_tabs(kind: NodeKind) -> Vec<String> {
    match kind {
        NodeKind::Task => vec!["Output".into(), "Stderr".into(), "Info".into()],
        NodeKind::Agent => vec!["Transcript".into(), "Result".into(), "Info".into()],
        _ => vec!["Info".into()],
    }
}

pub fn open_detail(state: &TopState) -> Option<Detail> {
    let n = state.selected_node()?;
    if matches!(n.kind, NodeKind::Session | NodeKind::Main | NodeKind::Run) {
        return None;
    }
    Some(Detail {
        id: n.id.clone(),
        tab: 0,
        follow: true,
        scroll: 0,
        title: format!("{} · {}", n.id, n.label),
        tabs: detail_tabs(n.kind),
        lines: Vec::new(),
    })
}
