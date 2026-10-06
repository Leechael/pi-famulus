//! pi-famulus — process management daemon for pi-famulus.
//! Single binary: `daemon` runs the manager; every other subcommand is a
//! socket client (design doc §3.5).

mod cli;
mod capacity;
mod out;

mod client;
mod clock;
mod daemon;
mod events;
mod fmt;
mod gc;
mod handover;
mod inspect;
mod lifecycle;
mod pager;
mod proto;
mod registry;
mod runner;
mod stats;
mod sys;
mod task;
mod workkind;

use cli::{Cli, Sub};

/// Package version plus the commit it was built from ("0.1.0+066598ae00"):
/// every build of 0.1.0 would otherwise look the same in `status`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("PI_FAMULUS_GIT_SHA"));

fn main() {
    // `__run` is every task's process-group leader (`runner`): plain
    // threads, no async runtime, and not a user-facing subcommand.
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("__run") => {
            let command = args.next().unwrap_or_default();
            std::process::exit(runner::main(std::ffi::OsStr::new(&command)));
        }
        // An in-place upgrade asks the new binary this before exec'ing it.
        Some(handover::CHECK_ARG) => {
            println!("{}", handover::check_line());
            std::process::exit(0);
        }
        _ => {}
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async_main());
}

async fn async_main() {
    let cli = Cli::parse();
    let home = lifecycle::resolve_home(cli.home.as_deref());
    let pager = if !cli.no_pager && cli.cmd.pages() {
        pager::start()
    } else {
        None
    };
    let code = match cli.cmd {
        Sub::Daemon {
            foreground,
            handover,
        } => daemon::run(home, foreground, handover).await,
        Sub::Status { json } => run_client(inspect::cmd_status(&home, json)).await,
        Sub::Config { action } => match action {
            cli::ConfigAction::Get { key } => {
                if key != "max-agents" { eprintln!("unknown config key: {key}"); 1 }
                else { println!("{}", capacity::max_agents(&home)); 0 }
            }
            cli::ConfigAction::Set { key, value } => {
                if key != "max-agents" { eprintln!("unknown config key: {key}"); 1 }
                else { match value.parse::<usize>() {
                    Ok(n) => {
                        let old = capacity::max_agents(&home);
                        match capacity::set_max_agents(&home, n) {
                            Ok(()) => {
                                events::emit(&home, None, "capacity.changed", None, serde_json::json!({"budget":"max-agents", "previous":old, "total":n}));
                                0
                            }
                            Err(e) => { eprintln!("pi-famulus: {e}"); 1 }
                        }
                    },
                    Err(_) => { eprintln!("max-agents must be a positive integer"); 1 }
                }}
            }
        },
        Sub::Sessions { json } => run_client(inspect::cmd_sessions(&home, json)).await,
        Sub::List {
            all,
            session,
            cwd,
            since,
            json,
        } => {
            let opts = inspect::LsOpts {
                all,
                session,
                cwd,
                since,
                json,
            };
            run_client(inspect::cmd_ls(&home, opts)).await
        }
        Sub::Show { id, json } => run_client(inspect::cmd_show(&home, &id, json)).await,
        Sub::Stats {
            by,
            session,
            cwd,
            since,
            json,
        } => {
            let opts = stats::StatsOpts {
                by,
                session,
                cwd,
                since,
                json,
            };
            run_client(stats::cmd_stats(&home, opts)).await
        }
        Sub::Agent { id, full, follow } => {
            run_client(inspect::cmd_agent(&home, &id, full, follow)).await
        }
        Sub::Events {
            follow,
            session,
            id,
            since,
            json,
        } => {
            let opts = inspect::EventsOpts {
                follow,
                session,
                id,
                since,
                json,
            };
            run_client(inspect::cmd_events(&home, opts)).await
        }
        Sub::Output {
            task_id,
            follow,
            max_bytes,
        } => run_client(client::cmd_output(&home, &task_id, follow, max_bytes)).await,
        Sub::Stop { task_id } => run_client(client::cmd_stop(&home, &task_id)).await,
        Sub::KillSession { session_id } => {
            run_client(client::cmd_kill_session(&home, &session_id)).await
        }
        Sub::Doctor => client::cmd_doctor(&home).await,
        Sub::Shutdown => run_client(client::cmd_shutdown(&home)).await,
        Sub::Upgrade => run_client(client::cmd_upgrade(&home)).await,
        Sub::Log {
            task_id,
            follow,
            lines,
            stderr,
        } => {
            run_client(client::cmd_log(
                &home,
                follow,
                lines,
                task_id.as_deref(),
                stderr,
            ))
            .await
        }
        Sub::Tail {
            task_id,
            follow,
            lines,
            stderr,
        } => {
            let _ = follow; // Accepted for `tail -f`; tail always follows.
            run_client(client::cmd_log(
                &home,
                true,
                lines,
                Some(task_id.as_str()),
                stderr,
            ))
            .await
        }
        Sub::Start {
            session,
            kind,
            cwd,
            timeout_ms,
            background,
            command,
        } => {
            run_client(client::cmd_start(
                &home, &session, &kind, cwd, timeout_ms, background, &command,
            ))
            .await
        }
        Sub::Wait { task_id, budget_ms } => {
            run_client(client::cmd_wait(&home, &task_id, budget_ms)).await
        }
        Sub::Completion { shell } => {
            let shell = match shell.as_str() {
                "bash" => usage::complete::Shell::Bash,
                "zsh" => usage::complete::Shell::Zsh,
                "fish" => usage::complete::Shell::Fish,
                _ => unreachable!("shell is constrained by the CLI definition"),
            };
            out::bytes(Cli::completion_script(shell).as_bytes());
            0
        }
    };
    if let Some(p) = pager {
        p.finish();
    }
    std::process::exit(code);
}

async fn run_client(f: impl std::future::Future<Output = Result<(), String>>) -> i32 {
    match f.await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("pi-famulus: {e}");
            1
        }
    }
}
