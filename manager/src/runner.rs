//! `pi-famulus __run <command>`: the leader of every task's process group
//! (design doc §3.2 / §3.4).
//!
//! The daemon spawns this runner as a session leader instead of `sh -c`
//! directly. It starts with two extra descriptors:
//!
//! - fd 3, the LIFELINE: the read end of a pipe whose write end only the
//!   daemon holds. When the daemon ends by any means (`shutdown`, `kill -9`,
//!   a panic), the kernel closes that write end and this read returns EOF.
//!   The runner then SIGTERMs its process group, waits the 2 s grace, and
//!   SIGKILLs it: no task outlives its manager.
//! - fd 4, STATUS: the write end of this task's status pipe. When the
//!   command exits, the runner writes one line with its real status, then
//!   exits too if nothing else is left in the group. If the command left
//!   descendants behind (`cmd &`), the runner stays as the group's guardian,
//!   still holding the lifeline, until the group empties. The daemon reads
//!   "the group is empty" from the runner's own exit.
//!
//! Status line (the daemon/runner contract; keep it stable, since a daemon
//! may spawn a newer runner binary after an in-place upgrade):
//!   `exit <code> <alone|linger>[ <key>=<value>...]\n` or
//!   `signal <n> <alone|linger>[ <key>=<value>...]\n`
//! The optional tail carries the command's resource usage
//! (`cpu_user_us=`, `cpu_sys_us=`, `max_rss_kb=`). Readers ignore keys they
//! do not know, and a reader that predates the tail stops after the third
//! token, so either side may be the newer binary.
//!
//! Resource usage is `getrusage(RUSAGE_CHILDREN)` taken right after `sh` is
//! reaped. The runner has exactly one child, so that is `sh` plus every
//! descendant that was waited for by its own parent (pytest reaping its
//! xdist workers, make reaping its compilers). Not counted: a process that
//! escaped the wait chain (`setsid`, `cmd &` never waited for, a worker
//! orphaned because its parent died first; init reaps those), anything
//! still running when `sh` exits (the `linger` set), and every task whose
//! runner is SIGKILLed with its group (stop after the grace, `timeout_ms`):
//! that runner writes no status line at all. On macOS only, a process that
//! reaped children and then called exec loses those children's times
//! (measured; POSIX asks exec to keep them, and the Linux branch of
//! `task::runner_usage_across_exec_is_platform_dependent` checks it), so
//! `make; exec foo` reports only `foo`. macOS `/bin/sh` does not exec the
//! last command of a list (`a; b`), only a lone simple command, so this
//! needs an explicit `exec` or a program that execs after waiting.
//!
//! The runner blocks SIGTERM, so a group SIGTERM (stop, shutdown) reaches
//! the command while the runner lives to report how the command ended. The
//! child unblocks it right before exec. A handler would not do: a SIGTERM
//! landing between the fork and the exec of `sh` would run the inherited
//! handler in the child and be lost; blocked, it stays pending and acts the
//! moment the child unblocks it. SIGKILL takes the runner down with the
//! group; the daemon then falls back to the runner's own wait status.

use crate::sys;
#[cfg(unix)]
use crate::sys::{RUNNER_LIFELINE_FD, RUNNER_STATUS_FD};
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

/// Grace between the group SIGTERM and SIGKILL when the lifeline breaks
/// (the same 2 s as the daemon's stop/shutdown, §3.2).
const LIFELINE_GRACE: Duration = Duration::from_secs(2);
/// How often a guardian looks for remaining group members. Every group
/// enumeration walks the whole process table (on Linux, all of /proc), so
/// once the group has held stable for a second the poll backs off.
const GUARD_POLL: Duration = Duration::from_millis(100);
/// Slow poll after `GUARD_STABLE_FOR` of a stable, non-empty group.
const GUARD_POLL_SLOW: Duration = Duration::from_secs(1);
/// How long the group must stay non-empty before the poll backs off.
const GUARD_STABLE_FOR: Duration = Duration::from_secs(1);

pub fn main(command: &OsStr) -> i32 {
    #[cfg(unix)]
    {
        unix_main(command)
    }
    #[cfg(windows)]
    {
        windows_main(command)
    }
}

#[cfg(unix)]
fn unix_main(command: &OsStr) -> i32 {
    // Neither descriptor may reach the command.
    let _ = sys::set_cloexec(RUNNER_LIFELINE_FD);
    let _ = sys::set_cloexec(RUNNER_STATUS_FD);
    // Before any thread exists, so every thread inherits the mask.
    let _ = sys::block_signal(libc::SIGTERM);

    let me = sys::getpid();
    std::thread::spawn(move || watch_lifeline(me));

    // Same process group (the runner leads it); fds 0-2 are inherited.
    let mut sh = std::process::Command::new("/bin/sh");
    sh.arg("-c").arg(command);
    sys::unblock_in_child(&mut sh, libc::SIGTERM);
    let mut child = match sh.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pi-famulus: cannot run /bin/sh: {e}");
            report("exit 127", alone(me));
            return 127;
        }
    };
    // A group SIGTERM (an early stop) that reached only us, before `sh`
    // existed, is pending here: pass it on now that `sh` is in the group.
    // If `sh` got it too, a second SIGTERM changes nothing.
    if sys::signal_pending(libc::SIGTERM) {
        let _ = sys::signal_group(me, sys::SIGTERM);
    }
    let status = match child.wait() {
        Ok(s) => s,
        Err(_) => {
            report("exit 0", alone(me));
            return 0;
        }
    };
    let what = match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        (None, None) => "exit 0".to_string(),
    };
    // `sh` is our only child and is reaped now: this is its whole tree.
    let usage = sys::children_usage().ok().map(|(cpu_user_us, cpu_sys_us, max_rss_kb)| Usage {
        cpu_user_us,
        cpu_sys_us,
        max_rss_kb,
    });
    let alone_now = alone(me);
    report_usage(&what, alone_now, usage);
    if !alone_now {
        // Guardian: hold the lifeline until everything the command left
        // behind is gone (or the lifeline breaks and takes the group down).
        let mut stable = Duration::ZERO;
        while !alone(me) {
            let poll = if stable >= GUARD_STABLE_FOR { GUARD_POLL_SLOW } else { GUARD_POLL };
            std::thread::sleep(poll);
            stable += poll;
        }
    }
    0
}

/// What the daemon writes to a Windows runner's stdin once the runner is in
/// its task's job: `<status handle>\n<posix|cmd>\n<shell program>\n`, then
/// EOF. Until then the runner starts nothing, so every process of the task
/// is born inside the job (a child started before the assignment would
/// escape both stop and the kill-on-close lifeline).
#[cfg(windows)]
struct Gate {
    status: usize,
    shell: sys::TaskShell,
}

#[cfg(windows)]
fn parse_gate(text: &str) -> Option<Gate> {
    let mut lines = text.lines();
    let status = lines.next()?.trim().parse().ok()?;
    let kind = match lines.next()?.trim() {
        "posix" => sys::ShellKind::Posix,
        "cmd" => sys::ShellKind::Cmd,
        _ => return None,
    };
    let program = lines.next().filter(|p| !p.is_empty())?.into();
    Some(Gate { status, shell: sys::TaskShell { program, kind } })
}

/// Windows: the task's Job Object stands in for the process group and its
/// kill-on-close for the lifeline; the status pipe arrives through the gate.
#[cfg(windows)]
fn windows_main(command: &OsStr) -> i32 {
    use std::io::{Read, Write};
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let mut text = String::new();
    let _ = std::io::stdin().lock().read_to_string(&mut text);
    let Some(gate) = parse_gate(&text) else {
        eprintln!("pi-famulus: __run is started by the manager");
        return 127;
    };
    // SAFETY: the daemon duplicated this handle into us for our sole use.
    let mut status_file = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(gate.status as RawHandle) });
    let mut report = |what: &str, alone: bool| {
        let line = format!("{what} {}\n", if alone { "alone" } else { "linger" });
        let _ = status_file.write_all(line.as_bytes());
        let _ = status_file.flush();
    };

    let me = sys::getpid();
    let mut cmd = std::process::Command::new(&gate.shell.program);
    match gate.shell.kind {
        sys::ShellKind::Posix => {
            cmd.arg("-c").arg(command);
        }
        sys::ShellKind::Cmd => {
            let mut line = std::ffi::OsString::from("/d /s /c \"");
            line.push(command);
            line.push("\"");
            cmd.raw_arg(line);
        }
    }
    cmd.stdin(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pi-famulus: cannot run {}: {e}", gate.shell.program.display());
            report("exit 127", alone(me));
            return 127;
        }
    };
    let st = match child.wait() {
        Ok(s) => s,
        Err(_) => {
            report("exit 0", alone(me));
            return 0;
        }
    };
    // A waited child that exited 137 or 143 (for example `exit /b 143`)
    // exited; it was not signaled. Manager-initiated job termination kills
    // the runner with the job, so the daemon reports that from the runner's
    // own wait status (`Outcome::of`) and never sees this line.
    let what = child_exit_word(st.code());
    let alone_now = alone(me);
    report(&what, alone_now);
    if !alone_now {
        let mut stable = Duration::ZERO;
        while !alone(me) {
            let poll = if stable >= GUARD_STABLE_FOR { GUARD_POLL_SLOW } else { GUARD_POLL };
            std::thread::sleep(poll);
            stable += poll;
        }
    }
    0
}

#[cfg(unix)]
fn report(what: &str, alone: bool) {
    report_usage(what, alone, None);
}

#[cfg(unix)]
fn report_usage(what: &str, alone: bool, usage: Option<Usage>) {
    let _ = sys::write_raw(RUNNER_STATUS_FD, status_line(what, alone, usage).as_bytes());
}

fn status_line(what: &str, alone: bool, usage: Option<Usage>) -> String {
    let mut line = format!("{what} {}", if alone { "alone" } else { "linger" });
    if let Some(u) = usage {
        line.push_str(&format!(
            " cpu_user_us={} cpu_sys_us={} max_rss_kb={}",
            u.cpu_user_us, u.cpu_sys_us, u.max_rss_kb
        ));
    }
    line.push('\n');
    line
}

/// No member of our group but us. If the group cannot be enumerated the
/// runner cannot prove the group empty, so it says "not alone": it keeps
/// guarding (and the daemon keeps probing the group) rather than let a
/// leftover escape both watchers.
fn alone(me: u32) -> bool {
    #[cfg(unix)]
    let members = sys::group_members(me);
    #[cfg(windows)]
    let members = sys::own_job_members();
    match members {
        Ok(pids) => pids.iter().all(|p| *p == me),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn watch_lifeline(me: u32) {
    let mut buf = [0u8; 64];
    loop {
        match sys::read_raw(RUNNER_LIFELINE_FD, &mut buf) {
            Ok(0) => break,
            Ok(_) => continue, // nobody writes; ignore
            Err(_) => break,
        }
    }
    // The daemon is gone: take the group down. SIGTERM reaches everyone
    // but us (we block it).
    let _ = sys::signal_group(me, sys::SIGTERM);
    std::thread::sleep(LIFELINE_GRACE);
    // Then SIGKILL every other member, one by one, until we are alone: a
    // process forked while a group signal is delivered can miss it. The
    // daemon is gone, so this runner is the last cleanup owner the group
    // has: it must not exit (abandoning survivors) while members remain.
    loop {
        match sys::group_members(me) {
            Ok(pids) => {
                let others: Vec<u32> = pids.into_iter().filter(|p| *p != me).collect();
                if others.is_empty() {
                    break;
                }
                for p in others {
                    let _ = sys::kill_pid(p, sys::SIGKILL);
                }
            }
            Err(_) => {
                // Cannot enumerate: group-SIGKILL and probe liveness; the
                // group is gone when even the leader is.
                let _ = sys::signal_group(me, sys::SIGKILL);
                if !sys::group_alive(me) {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    std::process::exit(137);
}

/// The command's resource usage as the runner measured it (see the module
/// doc for what it covers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub cpu_user_us: u64,
    pub cpu_sys_us: u64,
    /// Peak RSS of the single largest process measured, in KiB.
    pub max_rss_kb: u64,
}

/// Parsed status line. `None` for anything malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reported {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// The command left other processes in the group.
    pub linger: bool,
    /// Absent from a runner that predates it, or when any of its three
    /// values is missing or malformed. Never makes the line itself invalid:
    /// the exit status is what the daemon cannot do without.
    pub usage: Option<Usage>,
}

/// Status word for a waited child. Exit codes 137 and 143 stay exit codes.
#[cfg(any(windows, test))]
fn child_exit_word(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit {c}"),
        None => "exit 0".to_string(),
    }
}

pub fn parse_status(line: &str) -> Option<Reported> {
    let mut it = line.split_whitespace();
    let kind = it.next()?;
    let n: i32 = it.next()?.parse().ok()?;
    let linger = match it.next()? {
        "alone" => false,
        "linger" => true,
        _ => return None,
    };
    let (mut user, mut sys, mut rss) = (None, None, None);
    for tok in it {
        let Some((k, v)) = tok.split_once('=') else { continue };
        let v = v.parse::<u64>().ok();
        match k {
            "cpu_user_us" => user = v,
            "cpu_sys_us" => sys = v,
            "max_rss_kb" => rss = v,
            _ => {}
        }
    }
    let usage = match (user, sys, rss) {
        (Some(cpu_user_us), Some(cpu_sys_us), Some(max_rss_kb)) => Some(Usage { cpu_user_us, cpu_sys_us, max_rss_kb }),
        _ => None,
    };
    match kind {
        "exit" => Some(Reported { code: Some(n), signal: None, linger, usage }),
        "signal" => Some(Reported { code: None, signal: Some(n), linger, usage }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        assert_eq!(parse_status("exit 3 alone\n"), Some(Reported { code: Some(3), signal: None, linger: false, usage: None }));
        assert_eq!(parse_status("signal 9 linger"), Some(Reported { code: None, signal: Some(9), linger: true, usage: None }));
        assert_eq!(parse_status("exit x alone"), None);
        assert_eq!(parse_status("exit 1"), None);
        assert_eq!(parse_status("boom 1 alone"), None);
    }

    /// The usage tail is optional and lenient: a line without it (a runner
    /// from before it existed, carried across an in-place upgrade) and a
    /// line with a broken tail both keep their exit status.
    #[test]
    fn status_lines_with_usage() {
        let u = Usage { cpu_user_us: 1_500_000, cpu_sys_us: 20_000, max_rss_kb: 4096 };
        let line = status_line("exit 0", true, Some(u));
        assert_eq!(line, "exit 0 alone cpu_user_us=1500000 cpu_sys_us=20000 max_rss_kb=4096\n");
        assert_eq!(parse_status(&line), Some(Reported { code: Some(0), signal: None, linger: false, usage: Some(u) }));
        assert_eq!(status_line("signal 15", false, None), "signal 15 linger\n");
        // Unknown keys (a newer runner) are ignored; order does not matter.
        let r = parse_status("exit 1 linger max_rss_kb=7 future=x cpu_sys_us=2 cpu_user_us=3").unwrap();
        assert_eq!((r.code, r.linger), (Some(1), true));
        assert_eq!(r.usage, Some(Usage { cpu_user_us: 3, cpu_sys_us: 2, max_rss_kb: 7 }));
        // Missing or malformed values: no usage, status intact.
        for bad in [
            "exit 2 alone cpu_user_us=3 cpu_sys_us=2",
            "exit 2 alone cpu_user_us=x cpu_sys_us=2 max_rss_kb=1",
            "exit 2 alone junk",
        ] {
            let r = parse_status(bad).unwrap_or_else(|| panic!("{bad}: status lost"));
            assert_eq!((r.code, r.usage), (Some(2), None), "{bad}");
        }
    }

    /// An older daemon reads the line with the parser it has, which takes
    /// the first three tokens: they must not change.
    #[test]
    fn usage_tail_keeps_the_first_three_tokens() {
        let line = status_line("signal 9", false, Some(Usage { cpu_user_us: 1, cpu_sys_us: 2, max_rss_kb: 3 }));
        let head: Vec<&str> = line.split_whitespace().take(3).collect();
        assert_eq!(head, ["signal", "9", "linger"]);
    }

    #[test]
    fn waited_child_exit_137_and_143_stay_exit_codes() {
        for code in [0, 3, 137, 143] {
            assert_eq!(child_exit_word(Some(code)), format!("exit {code}"));
        }
        assert_eq!(child_exit_word(None), "exit 0");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn gate_lines() {
        let g = parse_gate("1234\nposix\nC:\\Program Files\\Git\\bin\\bash.exe\n").unwrap();
        assert_eq!(g.status, 1234);
        assert_eq!(g.shell.kind, sys::ShellKind::Posix);
        assert_eq!(g.shell.program, std::path::PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"));
        assert_eq!(parse_gate("8\ncmd\ncmd.exe").unwrap().shell.kind, sys::ShellKind::Cmd);
        for bad in ["", "x\nposix\nbash\n", "8\nzsh\nbash\n", "8\nposix\n\n", "8\nposix\n"] {
            assert!(parse_gate(bad).is_none(), "{bad:?}");
        }
    }
}
