//! Windows process-control seam: Job Objects, named pipes, no POSIX signals.
//!
//! Soft stop (`SIGTERM` on the wire) and hard kill (`SIGKILL`) both terminate
//! the task's Job Object, using POSIX-shaped exit codes `128+15` / `128+9`
//! so the rest of the daemon can keep talking about SIGTERM/SIGKILL.
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the lifeline: if this process
//! dies, every assigned tree dies with it (design §3.2 / §3.4).

use super::LocalTime;
use std::collections::HashMap;
use std::io;
use std::os::windows::io::{OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, DUPLICATE_SAME_ACCESS,
};
use windows_sys::Win32::System::Console::{
    GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

/// Same numbers as POSIX so the protocol (`"SIGTERM"` / `"SIGKILL"`) is unchanged.
pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

const STILL_ACTIVE: u32 = 259;
const ERROR_ACCESS_DENIED: i32 = 5;
const DETACH_FLAGS: u32 = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;

struct Job(HANDLE);

// HANDLE is a process-wide kernel object; the map is mutex-protected.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn jobs() -> &'static Mutex<HashMap<u32, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn apply_new_session_std(cmd: &mut std::process::Command) {
    cmd.creation_flags(DETACH_FLAGS | CREATE_BREAKAWAY_FROM_JOB);
}

/// Spawn the daemon out of the caller's job (a terminal or IDE may kill its
/// job when it closes), or inside it when that job forbids breakaway.
pub fn spawn_detached_std(cmd: &mut std::process::Command) -> io::Result<std::process::Child> {
    apply_new_session_std(cmd);
    match cmd.spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            cmd.creation_flags(DETACH_FLAGS);
            cmd.spawn()
        }
        r => r,
    }
}

/// Runners stay in the daemon's job (if any): the daemon puts each one in a
/// nested job of its own right after spawn.
pub fn apply_runner_setup_tokio(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(DETACH_FLAGS);
}

pub fn getpid() -> u32 {
    std::process::id()
}

/// Assign `process` to a new kill-on-close job keyed by `pid`.
pub fn assign_job(pid: u32, process: RawHandle) -> io::Result<()> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() || job == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            let e = io::Error::last_os_error();
            CloseHandle(job);
            return Err(e);
        }
        if AssignProcessToJobObject(job, process as HANDLE) == 0 {
            let e = io::Error::last_os_error();
            CloseHandle(job);
            return Err(e);
        }
        jobs().lock().unwrap().insert(pid, Job(job));
        Ok(())
    }
}

pub fn drop_job(pid: u32) {
    jobs().lock().unwrap().remove(&pid);
}

pub fn group_members(pgid: u32) -> io::Result<Vec<u32>> {
    let jobs = jobs().lock().unwrap();
    let Some(job) = jobs.get(&pgid) else {
        return Ok(Vec::new());
    };
    job_members(job.0)
}

/// Processes of the job this process is in (the innermost one when nested):
/// how a runner sees its own task tree.
pub fn own_job_members() -> io::Result<Vec<u32>> {
    job_members(std::ptr::null_mut())
}

/// The processes of `job`, minus console hosts: every console program in
/// the tree gets a `conhost.exe` in the same job, which lives as long as the
/// console and is not part of the task.
fn job_members(job: HANDLE) -> io::Result<Vec<u32>> {
    // NumberOfAssignedProcesses + NumberOfProcessIdsInList + up to 256 ids.
    #[repr(C)]
    struct List {
        assigned: u32,
        in_list: u32,
        pids: [usize; 256],
    }
    let mut list = List {
        assigned: 0,
        in_list: 0,
        pids: [0; 256],
    };
    let mut ret = 0u32;
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            std::ptr::from_mut(&mut list).cast(),
            std::mem::size_of::<List>() as u32,
            &mut ret,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let n = list.in_list.min(256) as usize;
    Ok(list.pids[..n]
        .iter()
        .map(|p| *p as u32)
        .filter(|p| *p != 0 && !is_console_host(*p))
        .collect())
}

fn is_console_host(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or("");
        name.eq_ignore_ascii_case("conhost.exe")
    }
}

/// Give `process` its own copy of `handle` (not inheritable, so it does not
/// leak into what that process starts), closing ours. Returns the handle
/// value as seen by `process`.
pub fn hand_over(handle: OwnedHandle, process: RawHandle) -> io::Result<usize> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::DUPLICATE_CLOSE_SOURCE;
    let mut theirs: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.into_raw_handle() as HANDLE,
            process as HANDLE,
            &mut theirs,
            0,
            0,
            DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(theirs as usize)
}

/// How a task's command line is run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellKind {
    /// `<shell> -c <command>`: bash and other POSIX shells.
    Posix,
    /// `cmd /d /s /c "<command>"`, the command line passed through verbatim.
    Cmd,
}

#[derive(Clone, Debug)]
pub struct TaskShell {
    pub program: std::path::PathBuf,
    pub kind: ShellKind,
}

/// The shell tasks run in, resolved once per daemon. Commands come from pi's
/// bash tool, so this follows pi's own choice on Windows: `PI_FAMULUS_SHELL`
/// if set, else Git Bash where Git for Windows installs it, else `bash.exe`
/// on PATH (not WSL's `System32\bash.exe`, which runs Linux processes the
/// task's job cannot see), else `cmd.exe`.
pub fn task_shell() -> &'static TaskShell {
    static SHELL: OnceLock<TaskShell> = OnceLock::new();
    SHELL.get_or_init(|| resolve_task_shell(|k| std::env::var_os(k), |p| p.is_file()))
}

pub fn resolve_task_shell(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> TaskShell {
    use std::path::{Path, PathBuf};
    let kind_of = |p: &Path| {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase());
        if stem.as_deref() == Some("cmd") { ShellKind::Cmd } else { ShellKind::Posix }
    };
    if let Some(s) = var("PI_FAMULUS_SHELL").filter(|s| !s.is_empty()) {
        let program = PathBuf::from(s);
        return TaskShell { kind: kind_of(&program), program };
    }
    for root in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = var(root) {
            let bash = Path::new(&dir).join("Git").join("bin").join("bash.exe");
            if exists(&bash) {
                return TaskShell { program: bash, kind: ShellKind::Posix };
            }
        }
    }
    if let Some(path) = var("PATH") {
        for dir in std::env::split_paths(&path) {
            let bash = dir.join("bash.exe");
            let lower = bash.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
            let wsl = lower.ends_with("\\system32\\bash.exe") || lower.ends_with("\\sysnative\\bash.exe");
            if !wsl && exists(&bash) {
                return TaskShell { program: bash, kind: ShellKind::Posix };
            }
        }
    }
    let program = var("COMSPEC").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("cmd.exe"));
    TaskShell { program, kind: ShellKind::Cmd }
}

pub fn group_has_others(pgid: u32) -> bool {
    match group_members(pgid) {
        Ok(pids) => pids.iter().any(|p| *p != pgid),
        Err(_) => group_alive(pgid),
    }
}

pub fn group_alive(pgid: u32) -> bool {
    group_members(pgid).map(|p| !p.is_empty()).unwrap_or(false)
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut code = 0u32;
        let ok = windows_sys::Win32::System::Threading::GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

pub fn kill_pid(pid: u32, sig: i32) -> io::Result<()> {
    let code = exit_code_for(sig);
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(87) || e.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            };
        }
        let _ = TerminateProcess(h, code);
        CloseHandle(h);
        Ok(())
    }
}

pub fn signal_group(pid: u32, sig: i32) -> io::Result<()> {
    let code = exit_code_for(sig);
    let jobs = jobs().lock().unwrap();
    if let Some(job) = jobs.get(&pid) {
        let _ = unsafe { TerminateJobObject(job.0, code) };
        return Ok(());
    }
    drop(jobs);
    kill_pid(pid, sig)
}

fn exit_code_for(sig: i32) -> u32 {
    128u32.saturating_add(sig as u32)
}

/// Non-blocking wait: Windows has no waitpid; poll with timeout 0.
pub fn waitpid_nohang(pid: u32) -> io::Result<Option<std::process::ExitStatus>> {
    use std::os::windows::process::ExitStatusExt;
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Ok(None);
        }
        let wr = WaitForSingleObject(h, 0);
        if wr != WAIT_OBJECT_0 {
            CloseHandle(h);
            return Ok(None);
        }
        let mut code = 0u32;
        let _ = windows_sys::Win32::System::Threading::GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        Ok(Some(std::process::ExitStatus::from_raw(code)))
    }
}

pub fn localtime(secs: i64) -> LocalTime {
    // Prefer UTC for the CLI clock display when local conversion is unavailable.
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    // Civil date from days since 1970-01-01 (Howard Hinnant algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    LocalTime {
        year: y as i32,
        month: m,
        day: d,
        hour,
        min,
        sec,
    }
}

pub fn stdout_tty_columns() -> Option<usize> {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &mut info) == 0 {
            return None;
        }
        let cols = (info.srWindow.Right.saturating_sub(info.srWindow.Left) as i32) + 1;
        if cols > 0 {
            Some(cols as usize)
        } else {
            Some(80)
        }
    }
}

pub fn random_bytes(buf: &mut [u8]) -> bool {
    use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        ) == 0
    }
}

#[cfg(test)]
pub fn max_rss_bytes() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn resolve(vars: &[(&str, &str)], files: &[&str]) -> TaskShell {
        let vars: HashMap<String, OsString> = vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        resolve_task_shell(|k| vars.get(k).cloned(), |p: &Path| files.iter().any(|f| f == p))
    }

    #[test]
    fn task_shell_follows_pi() {
        let git = r"C:\Program Files\Git\bin\bash.exe";
        let s = resolve(&[("ProgramFiles", r"C:\Program Files"), ("PATH", r"C:\msys64\usr\bin")], &[git, r"C:\msys64\usr\bin\bash.exe"]);
        assert_eq!((s.program, s.kind), (PathBuf::from(git), ShellKind::Posix));

        let s = resolve(&[("PATH", r"C:\Windows\System32;C:\msys64\usr\bin")], &[r"C:\Windows\System32\bash.exe", r"C:\msys64\usr\bin\bash.exe"]);
        assert_eq!(s.program, PathBuf::from(r"C:\msys64\usr\bin\bash.exe"), "WSL bash must be skipped");

        let s = resolve(&[("COMSPEC", r"C:\Windows\system32\cmd.exe"), ("PATH", r"C:\Windows\System32")], &[r"C:\Windows\System32\bash.exe"]);
        assert_eq!((s.program, s.kind), (PathBuf::from(r"C:\Windows\system32\cmd.exe"), ShellKind::Cmd));

        let s = resolve(&[("PI_FAMULUS_SHELL", "cmd.exe"), ("ProgramFiles", r"C:\Program Files")], &[git]);
        assert_eq!((s.program, s.kind), (PathBuf::from("cmd.exe"), ShellKind::Cmd));
        let s = resolve(&[("PI_FAMULUS_SHELL", r"D:\tools\sh.exe")], &[]);
        assert_eq!((s.program, s.kind), (PathBuf::from(r"D:\tools\sh.exe"), ShellKind::Posix));
    }
}
