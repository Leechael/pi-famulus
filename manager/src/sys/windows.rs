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
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
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
    GetCurrentProcess, OpenProcess, TerminateProcess, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB,
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

/// Same numbers as POSIX so the protocol (`"SIGTERM"` / `"SIGKILL"`) is unchanged.
pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

const STILL_ACTIVE: u32 = 259;
const WINDOWS_DETACH_FLAGS: u32 =
    CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB | CREATE_UNICODE_ENVIRONMENT;

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
    cmd.creation_flags(WINDOWS_DETACH_FLAGS);
}

pub fn apply_runner_setup_tokio(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(WINDOWS_DETACH_FLAGS);
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
            job.0,
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
    Ok(list.pids[..n].iter().map(|p| *p as u32).filter(|p| *p != 0).collect())
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

/// Duplicate a handle into an owned `std::fs::File` (async-capable via tokio).
pub fn file_from_handle(handle: RawHandle) -> io::Result<std::fs::File> {
    unsafe {
        let mut dup: HANDLE = std::ptr::null_mut();
        let ok = DuplicateHandle(
            GetCurrentProcess(),
            handle as HANDLE,
            GetCurrentProcess(),
            &mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::fs::File::from(OwnedHandle::from_raw_handle(dup as RawHandle)))
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
