//! `stats`: where the shell time and CPU went, by agent and/or work kind.
//!
//! Every retained task record counts, of connected and gone sessions alike
//! (unlike `ls`, which lists only what runs plus connected sessions'
//! finished work): the question is retrospective. A task belongs to the
//! subagent in its `origin.child_id`, named from that agent's record;
//! tasks without one belong to their session's main agent.
//!
//! Finished tasks use runner-reported CPU (`TaskRecord.cpu_*`); running tasks
//! use the daemon's best-effort process-group snapshot. CORES divides the
//! cumulative CPU by measured task wall. Unmeasured tasks still count in
//! TASKS/WALL and are shown in their own column.
//!
//! deferred | live process-group sample completeness on other platforms |
//! impact | samples can miss descendants that exit between polls and are
//! unavailable without Linux `/proc`; hard-killed runners remain unmeasured |
//! trigger | observed under-reporting materially changes a CPU comparison,
//! or live sampling is needed on macOS.

use crate::fmt;
use crate::inspect::{self, Live};
use crate::outln;
use crate::proto::{TaskRecord, TaskStatus};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Agent,
    Kind,
    AgentKind,
}

impl By {
    pub fn parse(s: &str) -> Result<By, String> {
        match s {
            "agent" => Ok(By::Agent),
            "kind" => Ok(By::Kind),
            "agent,kind" | "kind,agent" => Ok(By::AgentKind),
            _ => Err(format!("bad --by {s:?} (expected agent, kind, or agent,kind)")),
        }
    }
}

pub struct StatsOpts {
    pub by: String,
    pub session: Option<String>,
    pub cwd: Option<String>,
    pub since: Option<String>,
    pub json: bool,
}

/// One group's totals.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Group {
    /// Agent label: `name (ch_…)`, or `main <session>` for the parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub tasks: u64,
    pub wall_ms: u64,
    pub cpu_user_ms: u64,
    pub cpu_sys_ms: u64,
    /// Tasks with a CPU measurement, and their wall time (CORES' divisor).
    pub measured: u64,
    pub measured_wall_ms: u64,
    /// Recent CPU percentage, summed across running tasks with live samples.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_now_percent: Option<f64>,
    pub cpu_now_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_sampled_at: Option<u64>,
    pub killed: u64,
    pub killed_wall_ms: u64,
}

impl Group {
    pub fn cpu_ms(&self) -> u64 {
        self.cpu_user_ms + self.cpu_sys_ms
    }

    fn add(&mut self, t: &TaskRecord, now: u64) {
        let wall = t.ended_at.unwrap_or(now).saturating_sub(t.started_at);
        self.tasks += 1;
        self.wall_ms += wall;
        let cpu = if t.status == TaskStatus::Running {
            if let Some(sampled_at) = t.live_cpu_sampled_at {
                self.cpu_sampled_at = Some(self.cpu_sampled_at.unwrap_or(0).max(sampled_at));
            }
            if t.live_cpu_stale {
                self.cpu_now_stale = true;
                self.cpu_now_percent = None;
            } else if !self.cpu_now_stale {
                if let Some(percent) = t.live_cpu_percent {
                    self.cpu_now_percent = Some(self.cpu_now_percent.unwrap_or(0.0) + percent);
                }
            }
            (t.live_cpu_user_ms, t.live_cpu_sys_ms)
        } else {
            (t.cpu_user_ms, t.cpu_sys_ms)
        };
        if let (Some(u), Some(s)) = cpu {
            self.cpu_user_ms += u;
            self.cpu_sys_ms += s;
            self.measured += 1;
            self.measured_wall_ms += wall;
        }
        if t.status == TaskStatus::Killed {
            self.killed += 1;
            self.killed_wall_ms += wall;
        }
    }
}

#[derive(Serialize)]
struct GroupJson<'a> {
    #[serde(flatten)]
    g: &'a Group,
    cpu_ms: u64,
    /// CPU / measured wall; absent when nothing was measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    avg_cores: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopAgent {
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_id: Option<String>,
    pub tasks: u64,
    /// Cumulative retained task CPU time, including monitors (live estimate for running tasks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    /// Sum of the daemon's most recent process-group CPU rates; 100% = one core.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_now_percent: Option<f64>,
    pub cpu_now_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_sampled_at: Option<u64>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub tokens_cache_write: Option<u64>,
    pub llm_ms: Option<u64>,
    pub tool_ms: Option<u64>,
    pub queue_ms: Option<u64>,
    pub wall_other_ms: Option<u64>,
    pub wall_approximate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_per_second: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopWorkKind {
    pub kind: String,
    pub tasks: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_now_percent: Option<f64>,
    pub cpu_now_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_sampled_at: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct TopReport {
    pub agents: Vec<TopAgent>,
    pub work_kinds: Vec<TopWorkKind>,
    pub cpu_note: &'static str,
    pub output_tokens_per_second_note: &'static str,
}

fn cpu_now_label(percent: Option<f64>, stale: bool) -> String {
    if stale {
        "unavailable".into()
    } else {
        percent.map(|v| format!("{v:.1}%")).unwrap_or_else(|| "-".into())
    }
}

fn cpu_sample_label(sampled_at: Option<u64>) -> String {
    sampled_at.map(fmt::datetime).unwrap_or_else(|| "never".into())
}

/// Group `tasks` (already filtered). `agent_names`: child_id -> name.
pub fn group(
    tasks: &[&TaskRecord],
    by: By,
    agent_names: &HashMap<String, String>,
    prefixes: &HashMap<String, String>,
    now: u64,
) -> Vec<Group> {
    let mut groups: BTreeMap<(Option<String>, Option<String>, Option<String>), Group> = BTreeMap::new();
    for t in tasks {
        let child = t.origin.as_ref().and_then(|o| o.child_id.clone());
        let (agent, child_id, session_id) = if by == By::Kind {
            (None, None, None)
        } else {
            match &child {
                Some(c) => {
                    let label = match agent_names.get(c) {
                        Some(n) if !n.is_empty() => format!("{n} ({c})"),
                        _ => c.clone(),
                    };
                    (Some(label), Some(c.clone()), Some(t.session_id.clone()))
                }
                None => {
                    let p = prefixes.get(&t.session_id).cloned().unwrap_or_else(|| t.session_id.clone());
                    (Some(format!("main {p}")), None, Some(t.session_id.clone()))
                }
            }
        };
        let kind = (by != By::Agent).then(|| inspect::work_kind(t).to_string());
        let key = (session_id.clone(), child_id.clone(), kind.clone());
        let g = groups.entry(key).or_insert_with(|| Group {
            agent,
            child_id,
            session_id,
            kind,
            ..Default::default()
        });
        g.add(t, now);
    }
    let mut out: Vec<Group> = groups.into_values().collect();
    out.sort_by(|a, b| (b.cpu_ms(), b.wall_ms).cmp(&(a.cpu_ms(), a.wall_ms)));
    out
}

pub fn total(groups: &[Group]) -> Group {
    let mut t = Group::default();
    for g in groups {
        t.tasks += g.tasks;
        t.wall_ms += g.wall_ms;
        t.cpu_user_ms += g.cpu_user_ms;
        t.cpu_sys_ms += g.cpu_sys_ms;
        t.measured += g.measured;
        t.measured_wall_ms += g.measured_wall_ms;
        if let Some(sampled_at) = g.cpu_sampled_at {
            t.cpu_sampled_at = Some(t.cpu_sampled_at.unwrap_or(0).max(sampled_at));
        }
        if g.cpu_now_stale {
            t.cpu_now_stale = true;
            t.cpu_now_percent = None;
        } else if !t.cpu_now_stale {
            if let Some(percent) = g.cpu_now_percent {
                t.cpu_now_percent = Some(t.cpu_now_percent.unwrap_or(0.0) + percent);
            }
        }
        t.killed += g.killed;
        t.killed_wall_ms += g.killed_wall_ms;
    }
    t
}

fn cpu_of(g: &Group) -> Option<u64> {
    (g.measured > 0).then(|| g.cpu_ms())
}

pub fn render(groups: &[Group], by: By) -> Vec<String> {
    let mut head: Vec<&str> = Vec::new();
    if by != By::Kind {
        head.push("AGENT");
    }
    if by != By::Agent {
        head.push("KIND");
    }
    let lead = head.len();
    head.extend(["TASKS", "WALL", "CPU", "CORES", "UNMEASURED", "KILLED", "KILLED-WALL"]);
    let row = |g: &Group, label: Option<&str>| -> Vec<String> {
        let mut r = Vec::new();
        if let Some(l) = label {
            r.push(l.to_string());
            for _ in 1..lead {
                r.push(String::new());
            }
        } else {
            if by != By::Kind {
                r.push(g.agent.clone().unwrap_or_default());
            }
            if by != By::Agent {
                r.push(g.kind.clone().unwrap_or_default());
            }
        }
        r.extend([
            g.tasks.to_string(),
            fmt::human_duration(g.wall_ms),
            inspect::cpu_text(cpu_of(g)),
            inspect::cores_text(cpu_of(g), g.measured_wall_ms),
            (g.tasks - g.measured).to_string(),
            g.killed.to_string(),
            fmt::human_duration(g.killed_wall_ms),
        ]);
        r
    };
    let mut cells: Vec<Vec<String>> = vec![head.iter().map(|s| s.to_string()).collect()];
    cells.extend(groups.iter().map(|g| row(g, None)));
    cells.push(row(&total(groups), Some("TOTAL")));
    let mut widths = vec![0usize; head.len()];
    for c in &cells {
        for (i, v) in c.iter().enumerate() {
            widths[i] = widths[i].max(fmt::display_width(v));
        }
    }
    cells
        .iter()
        .map(|c| {
            let mut s = String::new();
            for (i, v) in c.iter().enumerate() {
                // Labels left-aligned, numbers right-aligned.
                if i < lead {
                    s.push_str(&fmt::pad(v, widths[i]));
                } else {
                    s.push_str(&" ".repeat(widths[i] - fmt::display_width(v)));
                    s.push_str(v);
                }
                s.push_str("  ");
            }
            s.trim_end().to_string()
        })
        .collect()
}

pub async fn cmd_top(home: &Path, json: bool) -> Result<(), String> {
    let snap = inspect::snapshot(home, Live::IfRunning).await?;
    inspect::warn_if_older_daemon(&snap);
    let tasks: Vec<&TaskRecord> = snap.tasks.iter().collect();
    let names: HashMap<String, String> = snap.agents.iter().map(|a| (a.child_id.clone(), a.name.clone())).collect();
    let prefixes = inspect::session_prefixes(snap.sessions.keys().map(|s| s.as_str()).chain(tasks.iter().map(|t| t.session_id.as_str())));
    let agent_groups = group(&tasks, By::Agent, &names, &prefixes, snap.now);
    let kind_groups = group(&tasks, By::Kind, &names, &prefixes, snap.now);
    let groups_by_key: HashMap<(String, Option<String>), &Group> = agent_groups
        .iter()
        .filter_map(|g| g.session_id.as_ref().map(|sid| ((sid.clone(), g.child_id.clone()), g)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut agents = Vec::new();
    for record in &snap.agents {
        let key = (record.session_id.clone(), Some(record.child_id.clone()));
        seen.insert(key.clone());
        agents.push(top_agent(
            format!("{} ({})", record.name, record.child_id),
            Some(record.child_id.clone()),
            groups_by_key.get(&key).copied(),
            Some(record),
        ));
    }
    for g in &agent_groups {
        let key = (g.session_id.clone().unwrap_or_default(), g.child_id.clone());
        if seen.insert(key) {
            agents.push(top_agent(g.agent.clone().unwrap_or_else(|| "unknown agent".into()), g.child_id.clone(), Some(g), None));
        }
    }
    agents.sort_by(|a, b| b.cpu_ms.unwrap_or(0).cmp(&a.cpu_ms.unwrap_or(0)).then_with(|| a.agent.cmp(&b.agent)));
    let work_kinds = kind_groups
        .iter()
        .map(|g| TopWorkKind {
            kind: g.kind.clone().unwrap_or_else(|| "unknown".into()),
            tasks: g.tasks,
            cpu_ms: (g.measured > 0).then(|| g.cpu_ms()),
            cpu_now_percent: g.cpu_now_percent,
            cpu_now_stale: g.cpu_now_stale,
            cpu_sampled_at: g.cpu_sampled_at,
        })
        .collect();
    let report = TopReport {
        agents,
        work_kinds,
        cpu_note: "CPU totals include retained tasks attributed to each agent (including monitors): final runner measurements for ended tasks and best-effort process-group estimates for running tasks. NOW is the latest daemon sampling interval; 100% equals one core. cpu_now_stale marks failed sampling; cpu_sampled_at is the last successful sample time.",
        output_tokens_per_second_note: "Cumulative output tokens divided by observed assistant-message LLM wall milliseconds; not divided by total elapsed wall time.",
    };
    if json {
        outln!("{}", serde_json::to_string_pretty(&report).unwrap());
        return Ok(());
    }
    outln!("pi-famulus top — CPU totals are per-agent task CPU, including monitors; NOW is the latest sampled CPU rate (100% = one core).");
    outln!("AGENTS");
    if report.agents.is_empty() {
        outln!("  none");
    }
    for a in &report.agents {
        let cpu = a.cpu_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into());
        let now = cpu_now_label(a.cpu_now_percent, a.cpu_now_stale);
        let sampled = cpu_sample_label(a.cpu_sampled_at);
        let rate = a.output_tokens_per_second.map(|v| format!("{v:.1} output tok/s")).unwrap_or_else(|| "- tok/s".into());
        let approximate = if a.wall_approximate { " (approximate)" } else { "" };
        outln!(
            "  {} | CPU {} total, {} now (sample {}) | {} task(s) | tokens {} in / {} out (cache read {} / write {}), {} | wall LLM {} / tool {} / queue {} / unclassified {}{}",
            a.agent,
            cpu,
            now,
            sampled,
            a.tasks,
            token_count_label(a.tokens_input),
            token_count_label(a.tokens_output),
            token_count_label(a.tokens_cache_read),
            token_count_label(a.tokens_cache_write),
            rate,
            duration_label(a.llm_ms),
            duration_label(a.tool_ms),
            duration_label(a.queue_ms),
            duration_label(a.wall_other_ms),
            approximate,
        );
    }
    outln!("WORK KINDS");
    if report.work_kinds.is_empty() {
        outln!("  none");
    }
    for k in &report.work_kinds {
        let cpu = k.cpu_ms.map(fmt::human_duration).unwrap_or_else(|| "-".into());
        let now = cpu_now_label(k.cpu_now_percent, k.cpu_now_stale);
        let sampled = cpu_sample_label(k.cpu_sampled_at);
        outln!("  {} | {} task(s) | CPU {} total, {} now (sample {})", k.kind, k.tasks, cpu, now, sampled);
    }
    Ok(())
}

fn token_count_label(value: Option<u64>) -> String {
    value.map(|n| n.to_string()).unwrap_or_else(|| "unavailable".into())
}

fn duration_label(value: Option<u64>) -> String {
    value.map(fmt::human_duration).unwrap_or_else(|| "unavailable".into())
}

fn top_agent(agent: String, child_id: Option<String>, group: Option<&Group>, record: Option<&inspect::AgentRecord>) -> TopAgent {
    let tokens_input = record.and_then(|a| a.tokens_input);
    let tokens_output = record.and_then(|a| a.tokens_output);
    let tokens_cache_read = record.and_then(|a| a.tokens_cache_read);
    let tokens_cache_write = record.and_then(|a| a.tokens_cache_write);
    let llm_ms = record.and_then(|a| a.llm_ms);
    TopAgent {
        agent,
        child_id,
        tasks: group.map_or(0, |g| g.tasks),
        cpu_ms: group.and_then(|g| (g.measured > 0).then(|| g.cpu_ms())),
        cpu_now_percent: group.and_then(|g| g.cpu_now_percent),
        cpu_now_stale: group.is_some_and(|g| g.cpu_now_stale),
        cpu_sampled_at: group.and_then(|g| g.cpu_sampled_at),
        tokens_input,
        tokens_output,
        tokens_cache_read,
        tokens_cache_write,
        llm_ms,
        tool_ms: record.and_then(|a| a.tool_ms),
        queue_ms: record.and_then(|a| a.queue_ms),
        wall_other_ms: record.and_then(|a| a.wall_other_ms),
        wall_approximate: record.is_some_and(|a| a.wall_approximate),
        output_tokens_per_second: inspect::agent_output_tokens_per_second(tokens_output, llm_ms),
    }
}

pub async fn cmd_stats(home: &Path, o: StatsOpts) -> Result<(), String> {
    let by = By::parse(&o.by)?;
    let snap = inspect::snapshot(home, Live::IfRunning).await?;
    inspect::warn_if_older_daemon(&snap);
    let since = match &o.since {
        Some(s) => Some(snap.now.saturating_sub(fmt::parse_duration(s)?)),
        None => None,
    };
    let dir = o.cwd.as_deref().map(inspect::normalize_dir);
    let tasks: Vec<&TaskRecord> = snap
        .tasks
        .iter()
        .filter(|t| o.session.as_ref().map_or(true, |p| t.session_id.starts_with(p.as_str())))
        .filter(|t| dir.as_ref().map_or(true, |d| inspect::under_dir(&t.cwd, d)))
        .filter(|t| since.map_or(true, |s| t.ended_at.unwrap_or(snap.now) >= s))
        .collect();
    let names: HashMap<String, String> = snap.agents.iter().map(|a| (a.child_id.clone(), a.name.clone())).collect();
    let prefixes = inspect::session_prefixes(snap.sessions.keys().map(|s| s.as_str()).chain(tasks.iter().map(|t| t.session_id.as_str())));
    let groups = group(&tasks, by, &names, &prefixes, snap.now);
    if o.json {
        let rows: Vec<GroupJson> = groups
            .iter()
            .map(|g| GroupJson {
                g,
                cpu_ms: g.cpu_ms(),
                avg_cores: (g.measured > 0 && g.measured_wall_ms > 0).then(|| g.cpu_ms() as f64 / g.measured_wall_ms as f64),
            })
            .collect();
        outln!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return Ok(());
    }
    if groups.is_empty() {
        outln!("no tasks");
        return Ok(());
    }
    for l in render(&groups, by) {
        outln!("{l}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Origin, TaskKind};

    fn rec(id: &str, sid: &str, child: Option<&str>, cmd: &str, wall: u64, cpu: Option<(u64, u64)>, status: TaskStatus) -> TaskRecord {
        TaskRecord {
            task_id: id.into(),
            session_id: sid.into(),
            kind: TaskKind::Shell,
            command: cmd.into(),
            cwd: "/tmp".into(),
            pid: 1,
            status,
            exit_code: None,
            signal: None,
            started_at: 1_000,
            ended_at: Some(1_000 + wall),
            output_path: String::new(),
            output_size: 0,
            origin: child.map(|c| Origin { via: "child-bash".into(), child_id: Some(c.into()), run_id: None }),
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: cpu.map(|c| c.0),
            cpu_sys_ms: cpu.map(|c| c.1),
            max_rss_kb: cpu.map(|_| 1),
            live_cpu_user_ms: None,
            live_cpu_sys_ms: None,
            live_cpu_percent: None,
            live_cpu_sampled_at: None,
            live_cpu_stale: false,
        }
    }

    /// Attribution and arithmetic: tasks land on their child (named from
    /// its record) or on the session's main agent; CPU sums only measured
    /// tasks and CORES divides by their wall alone; killed time is kept.
    #[test]
    fn groups_by_agent_and_kind() {
        use TaskStatus::*;
        let t = [
            rec("sh_1", "s1", Some("ch_a"), "pytest -n 4", 10_000, Some((30_000, 2_000)), Completed),
            rec("sh_2", "s1", Some("ch_a"), "pytest tests/x.py", 4_000, None, Killed),
            rec("sh_3", "s1", Some("ch_a"), "rg foo", 100, Some((50, 10)), Completed),
            rec("sh_4", "s1", None, "git status", 200, Some((20, 20)), Completed),
            rec("sh_5", "s1", Some("ch_b"), "cargo build", 2_000, Some((1_000, 0)), Completed),
        ];
        let refs: Vec<&TaskRecord> = t.iter().collect();
        let names = HashMap::from([("ch_a".to_string(), "alpha".to_string())]);
        let prefixes = HashMap::from([("s1".to_string(), "s1".to_string())]);

        let g = group(&refs, By::Agent, &names, &prefixes, 0);
        let labels: Vec<&str> = g.iter().map(|g| g.agent.as_deref().unwrap()).collect();
        assert_eq!(labels, ["alpha (ch_a)", "ch_b", "main s1"], "sorted by CPU; unnamed child keeps its id");
        let a = &g[0];
        assert_eq!((a.tasks, a.wall_ms, a.cpu_ms(), a.measured, a.measured_wall_ms), (3, 14_100, 32_060, 2, 10_100));
        assert_eq!((a.killed, a.killed_wall_ms), (1, 4_000));
        let lines = render(&g, By::Agent);
        let alpha: Vec<&str> = lines.iter().find(|l| l.starts_with("alpha")).unwrap().split_whitespace().collect();
        // alpha (ch_a) | TASKS WALL CPU CORES UNMEASURED KILLED KILLED-WALL
        assert_eq!(alpha[2..], ["3", "14s", "32.1s", "3.2", "1", "1", "4s"], "the unmeasured kill must not dilute CORES");

        let g = group(&refs, By::AgentKind, &names, &prefixes, 0);
        let pairs: Vec<(&str, &str)> = g.iter().map(|g| (g.agent.as_deref().unwrap(), g.kind.as_deref().unwrap())).collect();
        assert_eq!(pairs[0], ("alpha (ch_a)", "test-suite"));
        assert!(pairs.contains(&("alpha (ch_a)", "test")) && pairs.contains(&("main s1", "git")), "{pairs:?}");
        let targeted = g.iter().find(|g| g.kind.as_deref() == Some("test")).unwrap();
        assert_eq!((targeted.measured, targeted.killed), (0, 1));

        let g = group(&refs, By::Kind, &names, &prefixes, 0);
        assert!(g.iter().all(|g| g.agent.is_none() && g.child_id.is_none()));
        assert_eq!(total(&g).tasks, 5);
        assert_eq!(total(&g).cpu_ms(), 33_100);
    }

    #[test]
    fn running_tasks_use_live_cpu_totals_and_recent_rate() {
        let mut running = rec("sh_live", "s1", Some("ch_live"), "pdm run test", 1_000, None, TaskStatus::Running);
        running.ended_at = None;
        running.live_cpu_user_ms = Some(1_200);
        running.live_cpu_sys_ms = Some(300);
        running.live_cpu_percent = Some(75.5);
        let g = group(&[&running], By::Agent, &HashMap::new(), &HashMap::new(), 2_000);
        assert_eq!((g[0].cpu_ms(), g[0].measured, g[0].measured_wall_ms), (1_500, 1, 1_000));
        assert_eq!(g[0].cpu_now_percent, Some(75.5));
        let all = total(&g);
        assert_eq!(all.cpu_now_percent, Some(75.5));
    }

    #[test]
    fn failed_live_cpu_sample_is_visible_as_stale_with_last_sample_time() {
        let sampled_at = 1_700_000_000_000;
        let now = sampled_at + 10_000;
        let mut running = rec("sh_stale", "s1", Some("ch_stale"), "sleep 10", 0, None, TaskStatus::Running);
        running.ended_at = None;
        running.live_cpu_user_ms = Some(1_200);
        running.live_cpu_sys_ms = Some(300);
        running.live_cpu_sampled_at = Some(sampled_at);
        running.live_cpu_stale = true;

        let groups = group(&[&running], By::Agent, &HashMap::new(), &HashMap::new(), now);
        assert!(groups[0].cpu_now_stale);
        assert_eq!(groups[0].cpu_now_percent, None);
        assert_eq!(groups[0].cpu_sampled_at, Some(sampled_at));
        let agent = top_agent("worker".into(), Some("ch_stale".into()), Some(&groups[0]), None);
        let top = serde_json::to_value(agent).unwrap();
        assert_eq!(top["cpu_now_stale"], true);
        assert_eq!(top["cpu_now_percent"], serde_json::Value::Null);
        assert_eq!(top["cpu_sampled_at"], sampled_at);
        assert_eq!(cpu_now_label(None, true), "unavailable");
        assert_eq!(cpu_sample_label(Some(sampled_at)), fmt::datetime(sampled_at));

        let row = inspect::task_row(&running, now);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["live_cpu_stale"], true);
        assert_eq!(json["live_cpu_sampled_at"], sampled_at);
        let prefixes = HashMap::from([("s1".into(), "s1".into())]);
        let ls = inspect::render_ls(&[row], &prefixes, now, Some(200));
        assert!(ls[0].contains("SAMPLE"));
        assert!(ls[1].contains("stale") && ls[1].contains(&fmt::short_time(sampled_at, now)), "{ls:?}");
    }

    #[test]
    fn unmeasured_group_shows_dashes_not_zero() {
        let t = [rec("sh_1", "s1", None, "sleep 9", 9_000, None, TaskStatus::Killed)];
        let refs: Vec<&TaskRecord> = t.iter().collect();
        let g = group(&refs, By::Kind, &HashMap::new(), &HashMap::new(), 0);
        let lines = render(&g, By::Kind);
        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["KIND", "TASKS", "WALL", "CPU", "CORES", "UNMEASURED", "KILLED", "KILLED-WALL"]);
        assert_eq!(lines[1].split_whitespace().collect::<Vec<_>>(), ["other", "1", "9s", "-", "-", "1", "1", "9s"]);
        assert!(lines[2].starts_with("TOTAL"), "{lines:?}");
    }

    #[test]
    fn by_parses_the_documented_values() {
        assert_eq!(By::parse("agent,kind"), Ok(By::AgentKind));
        assert_eq!(By::parse("kind,agent"), Ok(By::AgentKind));
        assert!(By::parse("model").is_err());
    }
}
