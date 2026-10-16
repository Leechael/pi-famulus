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

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FATAL: &[u8] = b"     24 [main] bash (10372) C:\\Program Files\\Git\\bin\\..\\usr\\bin\\bash.exe: *** fatal error - add_item (\"\\??\\C:\\Program Files\\Git\", \"/\", ...) failed, errno 1\n";

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
