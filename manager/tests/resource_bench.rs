//! Resource-usage benchmark of the real daemon, on every CI platform.
//!
//! Opt-in (`PI_FAMULUS_BENCH=1`); without it the binary reports nothing to
//! do, so plain `cargo test` stays fast. CI runs it on a release build:
//!
//! ```bash
//! PI_FAMULUS_BENCH=1 cargo test --release --test resource_bench
//! ```
//!
//! Each scenario samples the daemon process from outside (memory, open
//! handles / fds, CPU time) and asserts a budget. Budgets are generous
//! ceilings that catch regressions of kind, not of degree: a busy loop, a
//! per-task handle leak, unbounded buffering of output. The numbers go to
//! stdout, to `$PI_FAMULUS_BENCH_OUT` as JSON, and to
//! `$GITHUB_STEP_SUMMARY` as a table.
//!
//! `harness = false` for the same reason as `tests/platform.rs`: the binary
//! is also the `taskkit` task program.

mod common;

use common::kit::{self, kit};
use common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const S: fn(u64) -> Duration = Duration::from_secs;
const MS: fn(u64) -> Duration = Duration::from_millis;
const MIB: f64 = 1024.0 * 1024.0;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("taskkit") && std::env::var_os("PI_FAMULUS_BENCH").is_none() {
        println!("resource_bench: set PI_FAMULUS_BENCH=1 to run");
        return;
    }
    kit::main(&[("resource_usage", resource_usage)]);
}

struct Sample {
    rss: u64,
    handles: u64,
    cpu_ms: u64,
}

fn sample(pid: u32) -> Sample {
    Sample {
        rss: rss_bytes(pid).expect("rss probe failed"),
        handles: open_handles(pid).expect("handle probe failed"),
        cpu_ms: cpu_time_ms(pid).expect("cpu probe failed"),
    }
}

/// CPU milliseconds `pid` burns over `d` of wall time.
fn cpu_over(pid: u32, d: Duration) -> u64 {
    let a = cpu_time_ms(pid).expect("cpu probe failed");
    std::thread::sleep(d);
    cpu_time_ms(pid).expect("cpu probe failed").saturating_sub(a)
}

/// Wait until the daemon's handle count stops moving (exit watchers, pumps
/// and job handles are released asynchronously), then return it.
fn settled_handles(pid: u32) -> u64 {
    let mut last = open_handles(pid).expect("handle probe failed");
    for _ in 0..20 {
        std::thread::sleep(MS(250));
        let now = open_handles(pid).expect("handle probe failed");
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

/// Handles the daemon keeps once its idle tokio blocking threads have exited
/// (10 s keep-alive). On Windows every pipe read of a running task holds a
/// blocking thread, and every thread holds handles: right after a burst the
/// count reflects the pool, not a leak.
fn handles_after_pool_idle(pid: u32) -> u64 {
    std::thread::sleep(S(11));
    settled_handles(pid)
}

fn percentile(v: &mut [u64], p: f64) -> u64 {
    v.sort_unstable();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

struct Report {
    rows: Vec<(String, String, String, bool)>,
    json: serde_json::Map<String, Value>,
}

impl Report {
    fn metric(&mut self, key: &str, value: f64, unit: &str, budget: Option<f64>) {
        let ok = budget.is_none_or(|b| value <= b);
        let shown = if value.fract() == 0.0 { format!("{value:.0} {unit}") } else { format!("{value:.2} {unit}") };
        let limit = budget.map(|b| format!("≤ {b} {unit}")).unwrap_or_default();
        println!("{key:<40} {shown:>16} {limit:>16} {}", if ok { "" } else { "OVER BUDGET" });
        self.rows.push((key.to_string(), shown, limit, ok));
        self.json.insert(key.to_string(), json!({"value": value, "unit": unit, "budget": budget, "ok": ok}));
    }
}

fn resource_usage() {
    let home = Home::new_real("bench");
    let _d = home.start_daemon();
    let pid = home.pidfile_pid().unwrap();
    let mut c = home.connect();
    assert_eq!(c.hello_ext("bench")["ok"], true);
    let mut r = Report { rows: Vec::new(), json: serde_json::Map::new() };
    r.json.insert("os".into(), json!(std::env::consts::OS));
    r.json.insert("arch".into(), json!(std::env::consts::ARCH));

    // Idle: a connected client, no tasks.
    std::thread::sleep(S(1));
    let idle = sample(pid);
    let base_handles = settled_handles(pid);
    r.metric("idle.rss", idle.rss as f64 / MIB, "MiB", Some(48.0));
    r.metric("idle.handles", base_handles as f64, "", None);
    r.metric("idle.cpu_3s", cpu_over(pid, S(3)) as f64, "ms", Some(60.0));

    // Spawn latency: start -> exit observed, one task at a time.
    let mut lat = Vec::new();
    for _ in 0..40 {
        let t0 = Instant::now();
        let (id, _) = c.start(&kit(&["exit", "0"]));
        let w = c.request_ok(json!({"type":"wait","task_id":id,"budget_ms":10000}));
        assert_eq!(w["done"], true, "{w}");
        lat.push(t0.elapsed().as_millis() as u64);
    }
    r.metric("spawn_to_exit.p50", percentile(&mut lat, 0.5) as f64, "ms", Some(500.0));
    r.metric("spawn_to_exit.p95", percentile(&mut lat, 0.95) as f64, "ms", Some(1500.0));

    // 20 sleeping tasks: what each costs the daemon, and that nothing polls
    // hard while they run.
    let n = 20u64;
    let before = sample(pid);
    let mut running = Vec::new();
    for _ in 0..n {
        running.push(c.start(&kit(&["sleep", "120000"])));
    }
    std::thread::sleep(S(1));
    let busy = sample(pid);
    let runner_rss: u64 = running.iter().map(|(_, p)| rss_bytes(*p).expect("runner rss probe failed")).sum::<u64>() / n;
    r.metric("running20.daemon_rss", busy.rss as f64 / MIB, "MiB", Some(64.0));
    r.metric(
        "running20.daemon_handles_per_task",
        busy.handles.saturating_sub(before.handles) as f64 / n as f64,
        "",
        Some(16.0),
    );
    r.metric("running20.runner_rss_avg", runner_rss as f64 / MIB, "MiB", Some(16.0));
    r.metric("running20.daemon_cpu_3s", cpu_over(pid, S(3)) as f64, "ms", Some(150.0));
    let runners_cpu: u64 = running.iter().map(|(_, p)| cpu_time_ms(*p).expect("runner cpu probe failed")).sum();
    std::thread::sleep(S(3));
    let runners_cpu_after: u64 = running.iter().map(|(_, p)| cpu_time_ms(*p).expect("runner cpu probe failed")).sum();
    r.metric("running20.runners_cpu_3s", runners_cpu_after.saturating_sub(runners_cpu) as f64, "ms", Some(150.0));
    for (id, _) in &running {
        c.request_ok(json!({"type":"stop","task_id":id}));
    }
    for (id, _) in &running {
        assert!(c.wait_terminal(id, S(10)).is_some(), "{id} did not stop");
    }

    // Churn: 200 short tasks, then leftovers that exit by themselves. The
    // daemon must give back every handle / fd they took.
    for chunk in 0..20 {
        let ids: Vec<String> = (0..10).map(|_| c.start(&kit(&["exit", "0"])).0).collect();
        for id in &ids {
            assert!(c.wait_terminal(id, S(20)).is_some(), "churn {chunk}: {id} did not finish");
        }
    }
    let warm = settled_handles(pid);
    r.metric("churn200.handle_growth_warm", warm.saturating_sub(base_handles) as f64, "", None);
    let after_churn = handles_after_pool_idle(pid);
    r.metric("churn200.handle_growth", after_churn.saturating_sub(base_handles) as f64, "", Some(24.0));
    let ids: Vec<String> = (0..20).map(|_| c.start(&kit(&["leave", "300"])).0).collect();
    for id in &ids {
        assert!(c.wait_terminal(id, S(20)).is_some(), "{id} did not finish");
    }
    std::thread::sleep(S(2));
    let after_leftovers = handles_after_pool_idle(pid);
    r.metric("leftovers20.handle_growth", after_leftovers.saturating_sub(base_handles) as f64, "", Some(24.0));
    r.metric("churn.rss_after", sample(pid).rss as f64 / MIB, "MiB", Some(64.0));

    // Large output: throughput, and the daemon's memory stays bounded while
    // 128 MiB stream through it (the ring is 64 KiB; the file keeps it all).
    const BIG: u64 = 128 << 20;
    let rss0 = sample(pid).rss;
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (secs, peak) = std::thread::scope(|s| {
        let sampler = s.spawn(|| {
            let mut peak = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                peak = peak.max(rss_bytes(pid).expect("rss probe failed"));
                std::thread::sleep(MS(20));
            }
            peak
        });
        // Always release the sampler, even if wait_terminal panics/times out.
        struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _stop_guard = Stop(&stop);
        let t0 = Instant::now();
        let (id, _) = c.start(&kit(&["bytes", &BIG.to_string()]));
        let t = c.wait_terminal(&id, S(120)).expect("large output task did not finish");
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(t["output_size"], BIG, "{t}");
        // The guard drops only after this expression is evaluated, so join
        // would wait forever unless the sampler is stopped first.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        (secs, sampler.join().unwrap())
    });
    r.metric("output128m.throughput", (BIG as f64 / MIB) / secs, "MiB/s", None);
    r.metric("output128m.rss_growth_peak", peak.saturating_sub(rss0) as f64 / MIB, "MiB", Some(48.0));
    let cpu_total = sample(pid).cpu_ms.saturating_sub(idle.cpu_ms);
    r.metric("daemon.cpu_total", cpu_total as f64, "ms", None);

    write_report(&r);
    let over: Vec<&str> = r.rows.iter().filter(|x| !x.3).map(|x| x.0.as_str()).collect();
    assert!(over.is_empty(), "over budget: {over:?}");
}

fn write_report(r: &Report) {
    if let Some(p) = std::env::var_os("PI_FAMULUS_BENCH_OUT") {
        let _ = std::fs::write(p, serde_json::to_vec_pretty(&Value::Object(r.json.clone())).unwrap());
    }
    if let Some(p) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let mut md = format!(
            "### pi-famulus resource usage ({} {})\n\n| metric | value | budget | |\n|---|---:|---:|---|\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        for (k, v, b, ok) in &r.rows {
            md.push_str(&format!("| `{k}` | {v} | {b} | {} |\n", if *ok { "ok" } else { "**over**" }));
        }
        md.push('\n');
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(p) {
            let _ = f.write_all(md.as_bytes());
        }
    }
}
