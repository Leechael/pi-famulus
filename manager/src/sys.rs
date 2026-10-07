//! Tiny Unix process-control seam (design doc §3.4).
//!
//! All production `unsafe` for this crate lives here. Callers use the safe
//! wrappers below; the only unavoidable `unsafe` is `CommandExt::pre_exec`,
//! which the std/tokio API marks unsafe because the closure runs between
//! fork and exec (async-signal-safe only).
//!
//! Invariants documented per function.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;

/// POSIX SIGTERM — process-group stop (§3.2 / §3.4).
pub const SIGTERM: i32 = libc::SIGTERM;
/// POSIX SIGKILL — hard kill after grace (§3.2 / §3.4).
pub const SIGKILL: i32 = libc::SIGKILL;

/// Make the child a session leader (`setsid`) so `pgid == pid` and
/// [`signal_group`] can address the whole tree with `kill(-pgid, …)`, and
/// let it inherit only fds 0, 1 and 2 (see [`child_setup`]). Used when
/// detaching the daemon itself from a short-lived CLI.
///
/// # Safety boundary
/// `pre_exec` is the only `unsafe` call site. The closure calls only
/// `setsid`, `fcntl` and (Linux) the `close_range` syscall, all
/// async-signal-safe, and allocates nothing: the fd bound is computed here,
/// before fork.
pub fn apply_new_session_std(cmd: &mut std::process::Command) {
    let limit = fd_scan_limit();
    // SAFETY: see above; `limit` is a captured integer.
    unsafe {
        cmd.pre_exec(move || child_setup(limit));
    }
}

/// Runs in the forked child, before exec.
///
/// Every fd ≥ 3 is marked close-on-exec, so the new program starts with
/// stdin/stdout/stderr only. Rust sets FD_CLOEXEC on its descriptors, but
/// on macOS only after `socket()`/`accept()` return. A fork in that window
/// (a task starting while a client connects) would otherwise give the task
/// a copy of a client connection. That client would then see no EOF when
/// the daemon closes the connection, until the task exits. The same applies
/// to anything the daemon inherited without the flag.
///
/// The fds are marked, not closed: std's fork path reports exec failures to
/// the parent over a close-on-exec pipe, which must stay open until exec.
/// The effect after exec is the same.
fn child_setup(limit: i32) -> io::Result<()> {
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Linux ≥ 5.11: one syscall. Older kernels (ENOSYS / EINVAL) fall back.
    #[cfg(target_os = "linux")]
    {
        let r = unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) };
        if r == 0 {
            return Ok(());
        }
    }
    for fd in 3..limit {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
    Ok(())
}

/// Fixed descriptor numbers a task runner (`pi-famulus __run`) starts with:
/// the read end of the daemon's lifeline and the write end of its own status
/// pipe (see `crate::runner`).
pub const RUNNER_LIFELINE_FD: i32 = 3;
pub const RUNNER_STATUS_FD: i32 = 4;

/// Spawn setup for a task runner: session leader and stdio-only fds as in
/// [`apply_new_session_std`], and then `lifeline` and `status`
/// at [`RUNNER_LIFELINE_FD`] / [`RUNNER_STATUS_FD`] without close-on-exec.
/// Both sources must be ≥ 5 (see [`dup_cloexec_high`]) so neither `dup2`
/// can land on the other's source.
///
/// # Safety boundary
/// As for [`apply_new_session_std`]; the closure adds two `dup2` calls,
/// which are async-signal-safe.
pub fn apply_runner_setup_tokio(cmd: &mut tokio::process::Command, lifeline: i32, status: i32) {
    let limit = fd_scan_limit();
    // SAFETY: see above; only integers are captured.
    unsafe {
        cmd.pre_exec(move || {
            child_setup(limit)?;
            if libc::dup2(lifeline, RUNNER_LIFELINE_FD) < 0 || libc::dup2(status, RUNNER_STATUS_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// A pipe with close-on-exec on both ends: (read, write).
pub fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe` fills the two-element array we own.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds were just created and are owned by nobody else.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(r.as_raw_fd())?;
    set_cloexec(w.as_raw_fd())?;
    Ok((r, w))
}

/// A close-on-exec duplicate of `fd` numbered ≥ 10, clear of the fixed
/// runner slots 3 and 4.
pub fn dup_cloexec_high(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC on a valid fd returns a new fd we then own.
    let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `n` is a fresh descriptor owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

/// Set FD_CLOEXEC on a raw descriptor.
pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an fd number; an invalid fd yields EBADF.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Block `sig` in the calling thread (threads spawned later inherit the
/// mask). A blocked signal stays pending instead of acting. Children inherit
/// the mask too; see [`unblock_in_child`].
pub fn block_signal(sig: i32) -> io::Result<()> {
    // SAFETY: builds a signal set we own and changes only this thread's mask.
    let rc = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut())
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Blocking `read` on a raw fd, retrying EINTR. 0 = EOF.
pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: reads into the buffer we own, at most its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Write all of `data` to a raw fd, retrying EINTR.
pub fn write_raw(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: writes from a buffer we own, at most its length.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        data = &data[n as usize..];
    }
    Ok(())
}

pub fn getpid() -> u32 {
    std::process::id()
}

/// Pids in process group `pgid` (zombies included, which is fine: they are
/// reaped by their parent within moments).
#[cfg(target_os = "macos")]
pub fn group_members(pgid: u32) -> io::Result<Vec<u32>> {
    let mut buf: Vec<libc::pid_t> = vec![0; 256];
    loop {
        let bytes = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the kernel writes at most `bytes` into `buf`. libproc
        // returns a pid count here (it divides the byte count itself).
        let n = unsafe { libc::proc_listpgrppids(pgid as libc::pid_t, buf.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = n as usize;
        if count < buf.len() {
            return Ok(buf[..count].iter().filter(|p| **p > 0).map(|p| *p as u32).collect());
        }
        buf.resize(buf.len() * 2, 0);
    }
}

/// Pids in process group `pgid`, from `/proc/<pid>/stat` (field 5).
#[cfg(not(target_os = "macos"))]
pub fn group_members(pgid: u32) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc")?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid pgrp …": comm may contain spaces/parens.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        if rest.split_whitespace().nth(2).and_then(|g| g.parse::<u32>().ok()) == Some(pgid) {
            out.push(pid);
        }
    }
    Ok(out)
}

/// One `/proc` walk for all requested process groups, returning cumulative
/// user/system CPU ticks by process group. Unreadable or racing processes are
/// skipped; a missing group entry means no member was readable.
#[cfg(target_os = "linux")]
pub fn group_cpu_ticks_by_pgid(pgids: &[u32]) -> io::Result<HashMap<u32, (u64, u64)>> {
    let wanted: HashSet<u32> = pgids.iter().copied().collect();
    let mut totals = HashMap::new();
    if wanted.is_empty() {
        return Ok(totals);
    }
    for entry in std::fs::read_dir("/proc")?.flatten() {
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `comm` can contain spaces and parentheses. Fields after its last
        // `)` begin with state (field 3): pgrp is index 2, utime/stime 11/12.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let (Some(pgrp), Some(user), Some(system)) =
            (fields.get(2), fields.get(11), fields.get(12))
        else {
            continue;
        };
        let (Ok(pgrp), Ok(user), Ok(system)) = (
            pgrp.parse::<u32>(),
            user.parse::<u64>(),
            system.parse::<u64>(),
        ) else {
            continue;
        };
        if wanted.contains(&pgrp) {
            let total = totals.entry(pgrp).or_insert((0u64, 0u64));
            total.0 = total.0.saturating_add(user);
            total.1 = total.1.saturating_add(system);
        }
    }
    Ok(totals)
}

#[cfg(not(target_os = "linux"))]
pub fn group_cpu_ticks_by_pgid(_pgids: &[u32]) -> io::Result<HashMap<u32, (u64, u64)>> {
    Ok(HashMap::new())
}

/// Linux clock ticks per second, kept behind the sys module's unsafe boundary.
#[cfg(target_os = "linux")]
pub fn clock_ticks_per_second() -> Option<u64> {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (ticks > 0).then_some(ticks as u64)
}

#[cfg(not(target_os = "linux"))]
pub fn clock_ticks_per_second() -> Option<u64> {
    None
}

/// Is `sig` pending (blocked and delivered) for this process?
pub fn signal_pending(sig: i32) -> bool {
    // SAFETY: fills a signal set we own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigpending(&mut set) == 0 && libc::sigismember(&set, sig) == 1
    }
}

/// Unblock `sig` in `cmd`'s child just before exec. A `sig` that reached
/// the child while blocked (between fork and exec) was kept pending and is
/// delivered at this point, with the default action.
///
/// # Safety boundary
/// `pre_exec` closure calls only `sigemptyset`/`sigaddset`/`pthread_sigmask`
/// on a stack set: async-signal-safe, no allocation.
pub fn unblock_in_child(cmd: &mut std::process::Command, sig: i32) {
    // SAFETY: see above.
    unsafe {
        cmd.pre_exec(move || {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            let rc = libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            Ok(())
        });
    }
}

/// Does group `pgid` have a member other than its leader (pid == pgid)?
/// The leader of a task's group is its runner, which is not a leftover:
/// neither alive (guarding) nor as an unreaped zombie. Falls back to the
/// `kill(-pgid, 0)` probe if the group cannot be enumerated.
pub fn group_has_others(pgid: u32) -> bool {
    match group_members(pgid) {
        Ok(pids) => pids.iter().any(|p| *p != pgid),
        Err(_) => group_alive(pgid),
    }
}

/// Clear FD_CLOEXEC so `fd` survives an exec (the in-place upgrade hands
/// these descriptors to the new image).
pub fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an integer fd; no memory is touched.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Non-blocking `waitpid` on one child: `Ok(None)` while it runs,
/// `Ok(Some(status))` once reaped. Used for runners the process did not
/// spawn itself in this image (after an in-place upgrade they are still its
/// children, but no tokio `Child` exists for them).
///
/// # Safety boundary
/// `waitpid` writes only into the `status` integer we own.
pub fn waitpid_nohang(pid: u32) -> io::Result<Option<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: plain pid, pointer to a local integer.
        let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if rc == 0 {
            return Ok(None);
        }
        if rc > 0 {
            return Ok(Some(std::process::ExitStatus::from_raw(status)));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(e);
    }
}

/// `kill(pid, sig)`; `ESRCH` (already gone) is success.
pub fn kill_pid(pid: u32, sig: i32) -> io::Result<()> {
    // SAFETY: plain integers; a positive pid addresses one process.
    let rc = unsafe { libc::kill(pid as i32, sig) };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(e)
    }
}

/// Upper bound for the fd scan in [`child_setup`], computed in the parent
/// before fork (the child must not allocate).
///
/// The highest fd open right now (from `/dev/fd`), plus slack for fds other
/// threads open before the fork. A new fd takes the lowest free number, so
/// it lands at most one past the current highest per concurrent open.
/// Never above the soft RLIMIT_NOFILE, where no fd can exist. That limit
/// alone is not used as the bound, because it is often 10^6 (raised by
/// shells and cargo): even capped at 65536 it made every spawn cost ~65k
/// syscalls. Without `/dev/fd`, the limit (capped) is the bound.
fn fd_scan_limit() -> i32 {
    const CAP: i64 = 1 << 16;
    const SLACK: i64 = 64;
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        i64::try_from(rl.rlim_cur).unwrap_or(CAP).min(CAP)
    } else {
        CAP
    };
    let highest = std::fs::read_dir("/dev/fd").ok().and_then(|d| {
        d.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i64>().ok())
            .max()
    });
    match highest {
        Some(h) => (h + 1 + SLACK).min(soft) as i32,
        None => soft as i32,
    }
}

/// Signal the process group led by `pid` (session leader ⇒ `pgid == pid`).
/// `ESRCH` (already dead) is treated as success.
///
/// # Safety boundary
/// `kill` with a negative pid is a single libc call; no fd ownership.
pub fn signal_group(pid: u32, sig: i32) -> io::Result<()> {
    // SAFETY: `pid`/`sig` are plain integers; negative pid means process group.
    let rc = unsafe { libc::kill(-(pid as i32), sig) };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(e)
    }
}

/// Local broken-down time for epoch seconds (`localtime_r`).
///
/// # Safety boundary
/// `localtime_r` writes only into the `tm` we own.
pub fn localtime(secs: i64) -> libc::tm {
    let t = secs as libc::time_t;
    // SAFETY: both pointers are valid for the duration of the call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// Columns of the terminal on stdout, or None when stdout is not a tty.
///
/// # Safety boundary
/// `isatty` and `ioctl(TIOCGWINSZ)` only read fd 1 and fill our `winsize`.
pub fn stdout_tty_columns() -> Option<usize> {
    // SAFETY: isatty on a valid fd number has no side effects.
    if unsafe { libc::isatty(1) } != 1 {
        return None;
    }
    // SAFETY: TIOCGWINSZ fills the winsize we pass.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return Some(ws.ws_col as usize);
        }
    }
    Some(80)
}

/// `kill(-pgid, 0)`: does any process remain in the group led by `pgid`?
/// POSIX does not reuse a pid while a process group with that id exists, so
/// a group we have watched continuously is still ours while this is true.
pub fn group_alive(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    // SAFETY: signal 0 to a negative pid is a pure existence check.
    let rc = unsafe { libc::kill(-(pgid as i32), 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `kill(pid, 0)` liveness probe. `EPERM` counts as alive (process exists
/// but we lack permission to signal it).
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 is a pure existence check; no side effects on success.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// CPU and peak memory of every child this process has waited for, and of
/// every descendant those children waited for in turn (`RUSAGE_CHILDREN`).
/// Returns `(user_us, sys_us, max_rss_kb)`. `max_rss_kb` is the peak of the
/// single largest such process, not a sum. A process nobody waited for (one
/// that was orphaned and reaped by init, or still runs) is not included.
/// `ru_maxrss` is KiB on Linux and bytes on Darwin; this returns KiB.
pub fn children_usage() -> io::Result<(u64, u64, u64)> {
    // SAFETY: getrusage fills a stack-allocated rusage; no aliasing.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) != 0 {
            return Err(io::Error::last_os_error());
        }
        usage
    };
    let us = |tv: libc::timeval| (tv.tv_sec.max(0) as u64) * 1_000_000 + tv.tv_usec.max(0) as u64;
    let rss = usage.ru_maxrss.max(0) as u64;
    let rss_kb = if cfg!(target_os = "macos") || cfg!(target_os = "ios") { rss / 1024 } else { rss };
    Ok((us(usage.ru_utime), us(usage.ru_stime), rss_kb))
}

/// Peak resident set size for this process (bytes on Darwin, KiB on Linux).
/// Used only by optional leak/RSS regression tests.
#[cfg(test)]
pub fn max_rss_raw() -> u64 {
    // SAFETY: getrusage fills a stack-allocated rusage; no aliasing.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0;
        }
        usage.ru_maxrss as u64
    }
}

/// Normalize [`max_rss_raw`] to bytes on both Darwin and Linux.
#[cfg(test)]
pub fn max_rss_bytes() -> u64 {
    let raw = max_rss_raw();
    if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        raw // Darwin: bytes
    } else {
        raw.saturating_mul(1024) // Linux: kilobytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process we start in a fresh group shows up as that group's member
    /// (and only it); once it is gone the group is empty.
    #[test]
    fn group_members_lists_a_real_group() {
        let mut child = std::process::Command::new("/bin/sleep");
        child.arg("30");
        apply_new_session_std(&mut child);
        let mut child = child.spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while group_members(pid).unwrap() != vec![pid] {
            assert!(std::time::Instant::now() < deadline, "members: {:?}", group_members(pid));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        signal_group(pid, SIGKILL).unwrap();
        child.wait().unwrap();
        assert!(group_members(pid).unwrap().is_empty());
    }
}
