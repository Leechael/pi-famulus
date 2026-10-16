//! Git Bash (MSYS2 runtime) start-up failures on Windows.
//!
//! Every MSYS process (`bash.exe`, `sh.exe`, ...) maps a per-user shared
//! memory section at start-up, and the first one to map it builds the mount
//! table. When several cold starts race to build it, a loser spins for ~15 s
//! and dies with
//! `fatal error - add_item ("\??\C:\Program Files\Git", "/", ...) failed, errno 1`
//! or STATUS_ACCESS_VIOLATION (0xC0000005), and while new starts keep coming
//! the section stays poisoned: later bash processes hang at start-up with no
//! output (git-for-windows/git#6368, msys2/msys2-runtime#333). The user's
//! command never ran in either case.
//!
//! This module holds the platform-independent classification; the daemon
//! applies it on Windows only (`finalize_exit`, the start-up watchdog).
#![cfg_attr(not(windows), allow(dead_code))]

/// A task the shell crashed under at start-up (exit code was a Windows crash
/// status and the output is the MSYS runtime's fatal error).
pub const SHELL_START_FAILED: &str = "shell-start-failed";
/// end_reason of a task killed because its shell never got past start-up
/// (the race's hang case: no output, nothing but the shell in the job).
pub const SHELL_START_HUNG: &str = "shell-start-hung";
/// How long a job may hold nothing but the shell, with no output, before it
/// counts as hung. The race's losers spin ~15 s before failing; a working
/// shell runs the command in milliseconds.
pub const START_HANG_AFTER_MS: u64 = 20_000;

/// Output larger than this is a command that ran and printed; a start-up
/// failure prints one fatal line before the command could run.
pub const MAX_STARTUP_FAILURE_OUTPUT: u64 = 4096;

/// A system NTSTATUS error (severity error, facility 0), the exit code of a
/// process that died of an exception such as STATUS_ACCESS_VIOLATION
/// (0xC0000005). `exit -1` (0xFFFFFFFF) and other program-chosen codes
/// are not.
pub fn is_windows_crash_code(code: i32) -> bool {
    let c = code as u32;
    c & 0xF000_0000 == 0xC000_0000 && c & 0x0FFF_0000 == 0
}

/// The MSYS runtime's start-up fatal error text.
pub fn mentions_msys_startup_failure(output: &[u8]) -> bool {
    contains(output, b"add_item (") || contains(output, b"msys-2.0.dll")
}

/// Did the shell itself fail to start (so the command never ran)?
/// `output` is the task's merged output (or its tail), `output_size` its
/// full size.
pub fn is_startup_crash(exit_code: Option<i32>, output_size: u64, output: &[u8]) -> bool {
    matches!(exit_code, Some(c) if is_windows_crash_code(c))
        && output_size <= MAX_STARTUP_FAILURE_OUTPUT
        && mentions_msys_startup_failure(output)
}

/// Executable names that are the posix shell itself rather than the
/// command: the configured shell (Git's `bin\bash.exe` is a launcher that
/// starts `usr\bin\bash.exe`, same name), `bash.exe` and `sh.exe`.
pub fn is_shell_image(name: &str, shell_program: &str) -> bool {
    let shell = shell_program.rsplit(['\\', '/']).next().unwrap_or(shell_program);
    name.eq_ignore_ascii_case(shell)
        || name.eq_ignore_ascii_case("bash.exe")
        || name.eq_ignore_ascii_case("sh.exe")
}

/// Where a task's start-up stands, from its job's processes other than the
/// runner (`names`: image names, None when one could not be queried).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StartPhase {
    /// No shell process yet (the runner has not started it).
    NotStarted,
    /// Only shell processes: MSYS is still initializing, or the command is
    /// shell builtins only.
    ShellOnly,
    /// A process other than the shell runs: the shell is up and ran the
    /// command (or one could not be identified; assume it is past start-up).
    Running,
}

pub fn start_phase(names: &[Option<String>], shell_program: &str) -> StartPhase {
    if names.is_empty() {
        return StartPhase::NotStarted;
    }
    let only_shell = names
        .iter()
        .all(|n| n.as_deref().is_some_and(|n| is_shell_image(n, shell_program)));
    if only_shell {
        StartPhase::ShellOnly
    } else {
        StartPhase::Running
    }
}

/// Start-up watchdog decision, fed one observation at a time.
#[derive(Debug, Default)]
pub struct HangWatch {
    shell_only_since_ms: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HangVerdict {
    /// Keep watching.
    Wait,
    /// Start-up is over (output, a real process, or the job is gone).
    Done,
    /// No output and only the shell for `START_HANG_AFTER_MS`: kill it.
    Hung,
}

impl HangWatch {
    /// `phase`: None once the job has no process left.
    pub fn observe(&mut self, now_ms: u64, output_size: u64, phase: Option<StartPhase>) -> HangVerdict {
        if output_size > 0 {
            return HangVerdict::Done;
        }
        match phase {
            None | Some(StartPhase::Running) => HangVerdict::Done,
            Some(StartPhase::NotStarted) => {
                self.shell_only_since_ms = None;
                HangVerdict::Wait
            }
            Some(StartPhase::ShellOnly) => {
                let since = *self.shell_only_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= START_HANG_AFTER_MS {
                    HangVerdict::Hung
                } else {
                    HangVerdict::Wait
                }
            }
        }
    }
}

/// Windows: one posix-shell start at a time.
///
/// The race needs two MSYS processes initializing the shared section at
/// once, so a task's shell is only released (its gate written) when no other
/// task's shell is still starting. A start counts as done when a process
/// other than the shell appears in its job (the shell ran the command), the
/// shell exits, or `START_HOLD_MAX` passes (a builtin-only command, or a
/// start that hangs anyway, must not stall every later task).
#[cfg(windows)]
pub mod serialize {
    use super::{start_phase, StartPhase};
    use std::time::{Duration, Instant};

    /// Per logon session, shared by every pi-famulus daemon of the user.
    const START_LOCK: &str = "Local\\pi-famulus-shell-start";
    const START_LOCK_WAIT_MAX_MS: u32 = 30_000;
    /// Longest one start holds back the next.
    pub const START_HOLD_MAX: Duration = Duration::from_millis(500);
    const POLL: Duration = Duration::from_millis(10);

    /// Start-up phase of the task led by `runner_pid`, or None once its job
    /// has no process left.
    pub fn phase(runner_pid: u32) -> Option<StartPhase> {
        let members = crate::sys::group_members(runner_pid).ok()?;
        if members.is_empty() {
            return None;
        }
        let names: Vec<Option<String>> = members
            .into_iter()
            .filter(|p| *p != runner_pid)
            .map(crate::sys::image_name)
            .collect();
        Some(start_phase(&names, &crate::sys::task_shell().program.to_string_lossy()))
    }

    /// Run `release` (writes the runner's gate, which starts the shell) once
    /// no other start is in progress, then hold the turn until this start is
    /// done.
    ///
    /// The turn is a named mutex, not an in-process lock: the MSYS shared
    /// section is per user, so shells started by another pi-famulus daemon
    /// (another `PI_FAMULUS_HOME`, a test suite running daemons in
    /// parallel) race with ours just the same. Mutex ownership is per
    /// thread, so one OS thread takes it, waits for this start to finish,
    /// and releases it.
    pub async fn start(runner_pid: u32, release: impl std::future::Future<Output = ()>) {
        let (taken_tx, taken_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            // Waiting longer than this means a holder is stuck: go ahead
            // rather than stall every task.
            let held = crate::sys::lock_named_mutex(START_LOCK, START_LOCK_WAIT_MAX_MS);
            let _ = taken_tx.send(());
            let _ = done_rx.recv(); // Err once `done_tx` is dropped
            drop(held);
        });
        let _ = taken_rx.await;
        let _turn = done_tx;
        release.await;
        let t0 = Instant::now();
        let mut seen_shell = false;
        while t0.elapsed() < START_HOLD_MAX {
            match phase(runner_pid) {
                None | Some(StartPhase::Running) => return,
                Some(StartPhase::ShellOnly) => seen_shell = true,
                Some(StartPhase::NotStarted) if seen_shell => return, // shell exited
                Some(StartPhase::NotStarted) => {}
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FATAL: &[u8] = b"     24 [main] bash (10372) C:\\Program Files\\Git\\bin\\..\\usr\\bin\\bash.exe: *** fatal error - add_item (\"\\??\\C:\\Program Files\\Git\", \"/\", ...) failed, errno 1\n";

    #[test]
    fn hang_watch() {
        use HangVerdict::*;
        let sh = Some(StartPhase::ShellOnly);
        let mut w = HangWatch::default();
        assert_eq!(w.observe(0, 0, Some(StartPhase::NotStarted)), Wait);
        assert_eq!(w.observe(1_000, 0, sh), Wait);
        assert_eq!(w.observe(1_000 + START_HANG_AFTER_MS - 1, 0, sh), Wait);
        assert_eq!(w.observe(1_000 + START_HANG_AFTER_MS, 0, sh), Hung);
        // Output, a real process, or a gone job end the watch.
        assert_eq!(HangWatch::default().observe(0, 1, sh), Done);
        assert_eq!(HangWatch::default().observe(0, 0, Some(StartPhase::Running)), Done);
        assert_eq!(HangWatch::default().observe(0, 0, None), Done);
    }

    #[test]
    fn start_phases() {
        let git = r"C:\Program Files\Git\bin\bash.exe";
        let n = |v: &[&str]| v.iter().map(|s| Some(s.to_string())).collect::<Vec<_>>();
        assert_eq!(start_phase(&[], git), StartPhase::NotStarted);
        assert_eq!(start_phase(&n(&["bash.exe", "BASH.EXE"]), git), StartPhase::ShellOnly);
        assert_eq!(start_phase(&n(&["bash.exe", "python.exe"]), git), StartPhase::Running);
        assert_eq!(start_phase(&[Some("bash.exe".into()), None], git), StartPhase::Running);
        assert_eq!(start_phase(&n(&["zsh.exe"]), r"C:\tools\zsh.exe"), StartPhase::ShellOnly);
    }

    #[test]
    fn crash_codes() {
        assert!(is_windows_crash_code(-1073741819)); // 0xC0000005, seen on CI
        assert!(is_windows_crash_code(0xC000_0409_u32 as i32));
        assert!(!is_windows_crash_code(1));
        assert!(!is_windows_crash_code(127));
        assert!(!is_windows_crash_code(-1));
        assert!(!is_windows_crash_code(0x8000_0003_u32 as i32));
    }

    #[test]
    fn startup_crash_needs_crash_code_fatal_text_and_little_output() {
        assert!(is_startup_crash(Some(-1073741819), FATAL.len() as u64, FATAL));
        assert!(!is_startup_crash(Some(1), FATAL.len() as u64, FATAL));
        assert!(!is_startup_crash(None, FATAL.len() as u64, FATAL));
        assert!(!is_startup_crash(Some(-1073741819), 3, b"boom"));
        // A command that ran, printed a lot and then crashed is not this.
        assert!(!is_startup_crash(Some(-1073741819), 1 << 20, FATAL));
        assert!(is_startup_crash(Some(-1073741819), 80, b"x: error while loading msys-2.0.dll"));
    }
}
