//! The `pi-famulus` command-line contract, declared with usage-rs.

use std::path::PathBuf;

use crate::VERSION;

/// Process management daemon for pi-famulus.
// usage-rs emits a literal spec version; runtime `VERSION` also carries the git SHA.
#[derive(usage::Cli)]
#[usage(
    bin = "pi-famulus",
    version = VERSION,
    version_spec = "0.1.0",
    about = "Process management daemon for pi-famulus",
    unknown_flags = "error",
    args_override_self = false,
    completion
)]
pub(crate) struct Cli {
    /// Base directory (`--home` > `PI_FAMULUS_HOME` > `~/.pi/agent/pi-famulus`).
    #[usage(long, global, display_order = 1)]
    pub(crate) home: Option<PathBuf>,
    /// Never page output (else `PI_FAMULUS_PAGER` / `PAGER` / `less -FRX` on a TTY).
    #[usage(long, global, display_order = 2)]
    pub(crate) no_pager: bool,
    #[usage(subcommand)]
    pub(crate) cmd: Sub,
}

#[derive(usage::Subcommands)]
pub(crate) enum ConfigAction {
    /// Read a setting.
    Get { key: String },
    /// Persist and immediately apply a setting.
    Set { key: String, value: String },
}

impl Sub {
    /// Listings and dumps a person reads; not a follow, not an action.
    pub(crate) fn pages(&self) -> bool {
        match self {
            Sub::Sessions { .. } | Sub::List { .. } | Sub::Show { .. } | Sub::Stats { .. } | Sub::Top { .. } => true,
            Sub::Agent { follow, .. } | Sub::Events { follow, .. } | Sub::Log { follow, .. } => {
                !follow
            }
            _ => false,
        }
    }
}

// Help sections follow docs/cli.md. `completion` stays with the built-in
// `help` under Commands: a heading equal to that default title would merge
// into it, and the ungrouped section is always rendered first.
#[derive(usage::Subcommands)]
pub(crate) enum Sub {
    /// Read or update runtime daemon configuration.
    #[usage(display_order = 165, help_heading = "Daemon")]
    Config { #[usage(subcommand)] action: ConfigAction },
    /// Version, protocol, uptime, sessions, task and agent counts.
    ///
    /// Never starts the daemon ("pi-famulus is not running", exit 1).
    #[usage(display_order = 10, help_heading = "Inspection")]
    Status {
        /// Machine-readable JSON on stdout.
        #[usage(long)]
        json: bool,
    },
    /// Connected pi sessions (and gone ones that still run work).
    ///
    /// Gone sessions' records are kept for `goneSessionRetention` (config.json,
    /// default 24h) and stay reachable via show/agent/events.
    #[usage(display_order = 20, help_heading = "Inspection")]
    Sessions {
        /// Machine-readable JSON on stdout.
        #[usage(long)]
        json: bool,
    },
    /// Running tasks and agents, newest first. Alias: `ls`.
    ///
    /// `--all` adds finished work of connected sessions after the running
    /// group. An agent's time is its last transcript message.
    #[usage(alias = "ls", display_order = 30, help_heading = "Inspection")]
    List {
        /// Also list finished work of connected sessions.
        #[usage(short, long)]
        all: bool,
        /// Only sessions whose id starts with this prefix.
        #[usage(long)]
        session: Option<String>,
        /// Only work whose cwd is this directory or below it.
        #[usage(long)]
        cwd: Option<String>,
        /// Only work active within this long (e.g. 30s, 10m, 2h, 1d).
        #[usage(long)]
        since: Option<String>,
        /// Machine-readable JSON on stdout.
        #[usage(long)]
        json: bool,
    },
    /// Everything about one task, monitor, agent (ch_…) or run (run_…).
    #[usage(display_order = 40, help_heading = "Inspection")]
    Show {
        /// Task, monitor, agent, or run id (fuzzy match ok).
        id: String,
        /// Machine-readable JSON on stdout.
        #[usage(long)]
        json: bool,
    },
    /// Cumulative and live task CPU (including monitors) by agent and work kind, with tokens.
    ///
    /// A plain-text snapshot; 100% NOW equals one CPU core.
    #[usage(display_order = 43, help_heading = "Inspection")]
    Top {
        /// Machine-readable JSON on stdout.
        #[usage(long)]
        json: bool,
    },
    /// Retained tasks' wall time and CPU, grouped by agent and/or work kind.
    ///
    /// Covers every retained task record, finished work of gone sessions
    /// included. AGENT is the subagent that ran the task (`main <session>`
    /// for the parent); KIND is the command's work kind (test-suite, test,
    /// build, lint/type, git, read/search, other, monitor), not shell/monitor.
    /// CPU sums measured tasks only; UNMEASURED counts the rest.
    #[usage(display_order = 45, help_heading = "Inspection")]
    Stats {
        /// Group by: agent, kind, or agent,kind.
        #[usage(long, default = "agent")]
        by: String,
        /// Only sessions whose id starts with this prefix.
        #[usage(long)]
        session: Option<String>,
        /// Only work whose cwd is this directory or below it.
        #[usage(long)]
        cwd: Option<String>,
        /// Only tasks running at some point within this long (e.g. 10m, 2h, 1d).
        #[usage(long)]
        since: Option<String>,
        /// Machine-readable JSON on stdout.
        #[usage(long)]
        json: bool,
    },
    /// Render an agent's transcript (preamble hidden unless --full).
    #[usage(display_order = 50, help_heading = "Inspection")]
    Agent {
        /// Agent id (`ch_…`; fuzzy match ok).
        id: String,
        /// Include the system prompt / agent preamble.
        #[usage(long)]
        full: bool,
        /// Keep following the transcript.
        #[usage(short = 'f', long)]
        follow: bool,
    },
    /// Event log, merged and time-ordered across sessions.
    #[usage(display_order = 60, help_heading = "Inspection")]
    Events {
        /// Keep following new events.
        #[usage(short = 'f', long)]
        follow: bool,
        /// Only sessions whose id starts with this prefix.
        #[usage(long)]
        session: Option<String>,
        /// Only events about this task/agent id.
        #[usage(long)]
        id: Option<String>,
        /// Only events within this long (e.g. 30s, 10m, 2h).
        #[usage(long)]
        since: Option<String>,
        /// One raw JSON object per line.
        #[usage(long)]
        json: bool,
    },
    /// Tail manager.log, or a task's output when an id is given.
    ///
    /// With an id: follows the merged `.output` file (use --stderr for the
    /// stderr-only sibling). Without: tails manager.log.
    #[usage(display_order = 70, help_heading = "Inspection")]
    Log {
        /// Optional task id; when set, tails that task's output instead of manager.log.
        task_id: Option<String>,
        /// Keep following new lines.
        #[usage(short = 'f', long)]
        follow: bool,
        /// Trailing lines to print before following (or as the whole dump).
        #[usage(short = 'n', long, default = "100")]
        lines: usize,
        /// Tail `<task>.stderr` instead of the merged output (requires a task id).
        #[usage(long)]
        stderr: bool,
    },
    /// Follow a task's output (shortcut for `log -f <id>`).
    ///
    /// `-f` is accepted for muscle memory (`tail -f ID`) and is always on.
    #[usage(display_order = 80, help_heading = "Inspection")]
    Tail {
        /// Task id (fuzzy match ok).
        task_id: String,
        /// Accepted and ignored (follow is always on for `tail`).
        #[usage(short = 'f', long)]
        follow: bool,
        /// Trailing lines to print before following.
        #[usage(short = 'n', long, default = "100")]
        lines: usize,
        /// Tail stderr only (`<task>.stderr`).
        #[usage(long)]
        stderr: bool,
    },
    /// Read a task's output through the protocol; -f follows.
    ///
    /// For an agent id, prints its result.
    #[usage(display_order = 90, help_heading = "Acting on tasks")]
    Output {
        /// Task or agent id (fuzzy match ok).
        task_id: String,
        /// Follow the output stream.
        #[usage(short = 'f', long)]
        follow: bool,
        /// Print at most this many bytes in total.
        #[usage(long)]
        max_bytes: Option<u64>,
    },
    /// Budget-wait on a task's exit.
    #[usage(display_order = 100, help_heading = "Acting on tasks")]
    Wait {
        /// Task id (fuzzy match ok).
        task_id: String,
        /// Milliseconds to wait before giving up (default 20000).
        #[usage(long, default = "20000")]
        budget_ms: u64,
    },
    /// Print a shell completion script (bash, zsh, or fish).
    #[usage(display_order = 110)]
    Completion {
        /// Which shell to generate for.
        #[usage(long, choices("bash", "zsh", "fish"))]
        shell: String,
    },
    /// Stop a task (SIGTERM group → 2s → SIGKILL).
    #[usage(display_order = 120, help_heading = "Acting on tasks")]
    Stop {
        /// Task id (fuzzy match ok).
        task_id: String,
    },
    /// Stop all running tasks of a session.
    #[usage(display_order = 130, help_heading = "Acting on tasks")]
    KillSession {
        /// Session id.
        session_id: String,
    },
    /// Start a task (scripting / smoke tests; extension-style session binding).
    #[usage(display_order = 140, help_heading = "Acting on tasks")]
    Start {
        /// Session that owns the task.
        #[usage(long, default = "cli")]
        session: String,
        /// Task kind: shell | monitor.
        #[usage(long, default = "shell")]
        kind: String,
        /// Working directory for the command.
        #[usage(long)]
        cwd: Option<String>,
        /// Hard kill ceiling in ms (omit for no limit).
        #[usage(long)]
        timeout_ms: Option<u64>,
        /// Semantic marker only; manager behaviour is unchanged.
        #[usage(long)]
        background: bool,
        /// Shell command string (run via `sh -c`).
        #[usage(double_dash = "automatic")]
        command: String,
    },
    /// Health checks; fixes stale socket/pid files. Exit 1 on any failure.
    ///
    /// Covers daemon, socket, config, protocol, stale records, orphan pids, disk use.
    #[usage(display_order = 150, help_heading = "Inspection")]
    Doctor,
    /// Gracefully shut the manager down (kills remaining tasks).
    #[usage(display_order = 160, help_heading = "Daemon")]
    Shutdown,
    /// Replace the running manager in place with the binary now on disk.
    ///
    /// Same pid, every task keeps running, clients reconnect. The daemon also
    /// does this by itself when that file changes.
    #[usage(display_order = 170, help_heading = "Daemon")]
    Upgrade,
    /// Run the manager daemon in the foreground (what clients spawn).
    #[usage(display_order = 180, help_heading = "Daemon")]
    Daemon {
        /// Also log to stderr (for debugging).
        #[usage(long)]
        foreground: bool,
        /// Internal: continue an in-place upgrade from this handover file.
        #[usage(long, hide)]
        handover: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::{Cli, Sub};
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn global_options_and_visible_alias_parse_with_usage() {
        let argv = ["--home", "/tmp/pi-famulus", "ls", "-a", "--json"].map(OsStr::new);
        let cli = Cli::parse_from(&argv).expect("valid list invocation");

        assert_eq!(cli.home.as_deref(), Some(Path::new("/tmp/pi-famulus")));
        assert!(matches!(
            cli.cmd,
            Sub::List {
                all: true,
                json: true,
                ..
            }
        ));
    }

    #[test]
    fn tail_accepts_dash_f_and_wait_keeps_its_budget_default() {
        // `tail -f` is muscle memory from `tail -f ID`: the flag must parse
        // even though tail always follows.
        let cli = Cli::parse_from(&[OsStr::new("tail"), OsStr::new("-f"), OsStr::new("task_1")])
            .expect("valid tail invocation");
        let Sub::Tail {
            task_id,
            lines,
            stderr,
            ..
        } = cli.cmd
        else {
            panic!("tail should be selected");
        };
        assert_eq!(task_id, "task_1");
        assert_eq!(lines, 100);
        assert!(!stderr);

        let cli = Cli::parse_from(&[OsStr::new("wait"), OsStr::new("task_1")])
            .expect("valid wait invocation");
        let Sub::Wait { budget_ms, .. } = cli.cmd else {
            panic!("wait should be selected");
        };
        assert_eq!(budget_ms, 20000);
    }

    #[test]
    fn start_keeps_its_single_shell_command_and_defaults() {
        let argv = ["start", "--", "-nasty command"].map(OsStr::new);
        let cli = Cli::parse_from(&argv).expect("valid start invocation");

        let Sub::Start {
            session,
            kind,
            command,
            ..
        } = cli.cmd
        else {
            panic!("start should be selected");
        };
        assert_eq!(session, "cli");
        assert_eq!(kind, "shell");
        assert_eq!(command, "-nasty command");
    }
}
