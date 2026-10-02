//! Page long CLI output, like git: when stdout is a terminal, listings go
//! through `PI_FAMULUS_PAGER`, else `PAGER`, else `less`. A bare `less` runs as
//! `less -FRX` (on top of any LESS the person set, e.g. `-R`), so output that
//! fits one screen prints as if no pager ran; a pager configured with its own
//! arguments runs as given. An empty value is treated as unset and falls
//! through to the next choice; `cat` means no pager, so does `--no-pager`.

#[cfg(unix)]
use std::process::{Child, Command, Stdio};
#[cfg(windows)]
use std::process::Child;
#[cfg(unix)]
use std::os::fd::AsRawFd;

pub struct Pager {
    child: Child,
}

/// The pager command line, or None for no pager; `env` reads a variable.
///
/// An empty `PI_FAMULUS_PAGER` is treated like an unset one and falls through to
/// `PAGER`, so `PI_FAMULUS_PAGER=` lets `PAGER` take over rather than disabling
/// paging outright; `cat` (from either variable) is the explicit way to
/// disable it.
fn command(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let chosen = env("PI_FAMULUS_PAGER")
        .filter(|v| !v.is_empty())
        .or_else(|| env("PAGER").filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "less".into());
    match chosen.trim() {
        "cat" => None,
        "less" => Some("less -FRX".into()),
        _ => Some(chosen),
    }
}

/// Whether `cmd`'s program (its first word) is runnable: an absolute or
/// relative path that exists, or a bare name found on `PATH`. A pager that
/// isn't runnable would still let `/bin/sh -c` start and then exit almost
/// immediately, at which point stdout writes look like a broken pipe and
/// the listing silently vanishes (see [`crate::out::bytes`]); checking here
/// keeps that case as "no pager ran" instead.
///
/// This only ever looks at the first word, so anything sh would treat
/// differently is left alone rather than guessed at: an env-assignment
/// prefix (`LESS=FRX less`), a `~` path (sh expands it, `is_file` doesn't),
/// a shell keyword handing off to another command (`exec less`, `command
/// less`), or shell metacharacters we're not going to parse. Those cases
/// return true — "assume it runs, let sh find out" — so a valid pager isn't
/// mistaken for a missing one.
fn program_exists(cmd: &str) -> bool {
    if cmd.contains(['$', '`', '|', ';', '&', '<', '>', '(', ')', '\'', '"']) {
        return true;
    }
    let Some(program) = cmd.split_whitespace().next() else {
        return false;
    };
    if program.contains('=') || program.starts_with('~') {
        return true;
    }
    if matches!(program, "exec" | "command" | "builtin" | "eval") {
        return true;
    }
    if program.contains('/') || (cfg!(windows) && program.contains('\\')) {
        return std::path::Path::new(program).is_file();
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return true;
            }
            #[cfg(windows)]
            {
                // PATHEXT: bare `sh` must match `sh.exe` / `sh.cmd` / …
                let pathext = std::env::var_os("PATHEXT")
                    .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
                for ext in std::env::split_paths(&pathext) {
                    let mut with_ext = candidate.as_os_str().to_owned();
                    with_ext.push(ext);
                    if std::path::Path::new(&with_ext).is_file() {
                        return true;
                    }
                }
            }
            false
        })
    })
}

/// Point stdout at a pager when stdout is a terminal. Returns None when no
/// pager runs (not a terminal, disabled, or it failed to start).
pub fn start() -> Option<Pager> {
    #[cfg(windows)]
    {
        let _ = command(|k| std::env::var(k).ok());
        return None;
    }
    #[cfg(unix)]
    {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(1) } != 1 {
        return None;
    }
    let cmd = command(|k| std::env::var(k).ok())?;
    if !program_exists(&cmd) {
        return None;
    }
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(&cmd).stdin(Stdio::piped());
    let mut child = c.spawn().ok()?;
    let stdin = child.stdin.take()?;
    // SAFETY: fd 1 becomes a second reference to the pipe; `stdin` is then
    // dropped, leaving fd 1 as the only write end.
    if unsafe { libc::dup2(stdin.as_raw_fd(), 1) } < 0 {
        return None;
    }
    drop(stdin);
    // The pager owns the terminal now: Ctrl-C is for it (less ignores it),
    // and this process finishes its output or stops at the closed pipe.
    // SAFETY: setting a signal disposition to SIG_IGN.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    Some(Pager { child })
    }
}

impl Pager {
    /// Close stdout so the pager sees EOF, and wait for the person to leave it.
    pub fn finish(mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        #[cfg(unix)]
        if let Ok(null) = std::fs::File::open("/dev/null") {
            // SAFETY: replaces fd 1 (the pipe's last write end) with /dev/null.
            unsafe { libc::dup2(null.as_raw_fd(), 1) };
        }
        #[cfg(windows)]
        {
            let _ = std::fs::File::open("NUL");
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_less_quits_on_one_screen_a_configured_pager_runs_as_given() {
        let with = |famulus: Option<&str>, pager: Option<&str>| {
            command(|k| match k {
                "PI_FAMULUS_PAGER" => famulus.map(String::from),
                "PAGER" => pager.map(String::from),
                _ => None,
            })
        };
        assert_eq!(with(None, None).as_deref(), Some("less -FRX"));
        assert_eq!(with(None, Some("less")).as_deref(), Some("less -FRX"));
        assert_eq!(with(Some(""), Some("less -S")).as_deref(), Some("less -S"));
        assert_eq!(with(Some("most"), Some("less")).as_deref(), Some("most"));
        assert_eq!(with(Some("cat"), Some("less")), None);
    }

    #[test]
    fn program_exists_checks_the_pager_will_actually_run() {
        #[cfg(unix)]
        assert!(program_exists("sh -c whatever"));
        #[cfg(windows)]
        assert!(program_exists("cmd /c echo"));
        assert!(!program_exists("pi-famulus-pager-does-not-exist-anywhere -R"));
        assert!(!program_exists(""));
    }

    #[test]
    fn program_exists_defers_to_sh_on_syntax_it_cannot_resolve_itself() {
        assert!(program_exists("LESS=FRX less"));
        assert!(program_exists("~/bin/mypager"));
        assert!(program_exists("exec less"));
        assert!(program_exists("command less"));
        assert!(program_exists("less | cat"));
    }
}
