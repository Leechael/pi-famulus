//! `stats`: where the shell time and CPU went, by agent and/or work kind.
//!
//! Every retained task record counts, of connected and gone sessions alike
//! (unlike `ls`, which lists only what runs plus connected sessions'
//! finished work): the question is retrospective. A task belongs to the
//! subagent in its `origin.child_id`, named from that agent's record;
//! tasks without one belong to their session's main agent.
//!
//! Only measured tasks add CPU (`TaskRecord.cpu_*`, see `crate::runner`),
//! and CORES divides that CPU by the wall time of the same measured tasks.
//! Unmeasured tasks (running, SIGKILLed with their runner, older records)
//! still count in TASKS/WALL and are shown in their own column, so a
//! group's CPU is never silently a partial sum.
//!
//! deferred: live sampling of a running task's process-group CPU (Linux
//! `/proc/<pid>/stat` utime+stime+cutime+cstime per member; macOS
//! `proc_listpgrppids` + `proc_pid_rusage`, whose time units on Apple
//! Silicon must be checked against getrusage first) | impact: running
//! tasks show `-`, and tasks whose runner is SIGKILLed (`timeout_ms`, a
//! stop past its grace) stay UNMEASURED; in the 2026-10-05 run the 22
//! killed tasks held 4h11m of wall time, though a stop whose SIGTERM the
//! command obeys is measured | trigger: a run where UNMEASURED tasks hold
//! a large share of a group's wall time, or someone needs CPU of a task
//! while it still runs.

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
        if let (Some(u), Some(s)) = (t.cpu_user_ms, t.cpu_sys_ms) {
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

pub async fn cmd_stats(home: &Path, o: StatsOpts) -> Result<(), String> {
    let by = By::parse(&o.by)?;
    let snap = inspect::snapshot(home, Live::IfRunning).await?;
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
