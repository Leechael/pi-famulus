//! The root view: a header and the task list, refreshed from the same
//! `Collector` the `top` TUI uses.
//!
//! Threading: `spawn` runs on NSH's engine thread and owns the signals;
//! `background` is `tokio::spawn` on NSH's multi-thread runtime, where the
//! collector's IPC and disk reads run. The collector moves into each
//! background tick and comes back with its result, so no lock is needed.

use std::time::Duration;

use nsh_base::theme;
use nsh_core::prelude::*;
use pi_famulus::proto::{TaskRecord, TaskStatus};
use pi_famulus::top::collector::{CollectView, Collector};

const REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone, PartialEq)]
struct Row {
    id: String,
    status: TaskStatus,
    kind: String,
    age: String,
    command: String,
}

#[derive(Clone, PartialEq, Default)]
struct Summary {
    daemon: bool,
    running: usize,
    total: usize,
    agents: usize,
    error: Option<String>,
}

#[component]
pub fn App() -> impl IntoElement {
    let rows = signal(Vec::<Row>::new());
    let summary = signal(Summary::default());
    let loaded = signal(false);

    spawn(async move {
        let home = pi_famulus::lifecycle::resolve_home(None);
        let mut collector = Some(Collector::new(home));
        let view = CollectView { session_prefix: None, preview: false, selected: None, detail: None };
        loop {
            let mut c = collector.take().expect("collector returns from every tick");
            let v = view.clone();
            let joined = background(async move {
                if c.disk_stale() {
                    c.refresh_disk().await;
                }
                let tick = c.tick(&v).await;
                (c, tick)
            })
            .await;
            match joined {
                Ok((c, Ok(tick))) => {
                    collector = Some(c);
                    if let Some(snap) = tick.snap {
                        summary.set(Summary {
                            daemon: snap.daemon.is_some(),
                            running: snap.tasks.iter().filter(|t| t.status == TaskStatus::Running).count(),
                            total: snap.tasks.len(),
                            agents: snap.agents.len(),
                            error: None,
                        });
                        rows.set(to_rows(snap.tasks, snap.now));
                    }
                }
                Ok((c, Err(e))) => {
                    collector = Some(c);
                    summary.update(|s| s.error = Some(e));
                }
                Err(join) => {
                    summary.update(|s| s.error = Some(format!("collector panicked: {join}")));
                    return;
                }
            }
            loaded.set(true);
            timer(REFRESH).await;
        }
    });

    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(header(summary, loaded))
        .child(
            scroll().flex_1().child(
                view()
                    .px_4()
                    .child(column_titles())
                    .child(each(move || rows.get(), |r: &Row| r.id.clone(), task_row)),
            ),
        )
}

fn header(summary: Signal<Summary>, loaded: Signal<bool>) -> impl IntoElement {
    view()
        .flex_row()
        .items_center()
        .gap_4()
        .px_4()
        .py_3()
        .border_b(px(1.0))
        .border_color(move || theme().border)
        .child(text("pi-famulus").text_size(px(17.0)).font_weight(FontWeight::SEMIBOLD))
        .child(
            text(move || {
                if !loaded.get() {
                    return "loading…".to_owned();
                }
                let s = summary.get();
                let daemon = if s.daemon { "daemon up" } else { "daemon not running" };
                let mut line =
                    format!("{daemon} · {} running / {} tasks · {} agents", s.running, s.total, s.agents);
                if let Some(e) = s.error {
                    line.push_str(&format!(" · error: {e}"));
                }
                line
            })
            .text_color(move || theme().muted_foreground)
            .tabular_nums(),
        )
}

fn column_titles() -> impl IntoElement {
    cells(
        text("STATUS"),
        text("KIND"),
        text("AGE"),
        text("COMMAND"),
    )
    .py_2()
    .text_color(move || theme().muted_foreground)
    .font_weight(FontWeight::SEMIBOLD)
}

fn task_row(row: ReadSignal<Row>, _index: ReadSignal<usize>) -> impl IntoElement {
    cells(
        text(move || status_label(row.get().status)).text_color(move || status_color(row.get().status)),
        text(move || row.get().kind),
        text(move || row.get().age).tabular_nums(),
        text(move || row.get().command).font_family("monospace").truncate(),
    )
    .py_1()
    .border_b(px(1.0))
    .border_color(move || theme().border)
}

fn cells(
    status: impl IntoElement,
    kind: impl IntoElement,
    age: impl IntoElement,
    command: impl IntoElement,
) -> View {
    view()
        .flex_row()
        .gap_3()
        .child(view().w(px(90.0)).child(status))
        .child(view().w(px(80.0)).child(kind))
        .child(view().w(px(70.0)).child(age))
        .child(view().flex_1().child(command))
}

fn to_rows(mut tasks: Vec<TaskRecord>, now: u64) -> Vec<Row> {
    // Running first, then newest.
    tasks.sort_by(|a, b| {
        (b.status == TaskStatus::Running)
            .cmp(&(a.status == TaskStatus::Running))
            .then(b.started_at.cmp(&a.started_at))
    });
    tasks
        .into_iter()
        .map(|t| Row {
            age: age(t.ended_at.unwrap_or(now).saturating_sub(t.started_at)),
            kind: format!("{:?}", t.kind).to_lowercase(),
            id: t.task_id,
            status: t.status,
            command: t.command.lines().next().unwrap_or_default().to_owned(),
        })
        .collect()
}

fn age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{}s", s / 60, s % 60),
        _ => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

fn status_label(s: TaskStatus) -> String {
    format!("{s:?}").to_lowercase()
}

fn status_color(s: TaskStatus) -> AdaptiveColor {
    let t = theme();
    match s {
        TaskStatus::Running => t.primary,
        TaskStatus::Failed | TaskStatus::Orphaned => t.destructive,
        TaskStatus::Completed | TaskStatus::Killed => t.muted_foreground,
    }
}
