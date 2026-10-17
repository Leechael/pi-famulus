//! `pi-famulus __rpc <argv...>`: process-group leader for a hosted subagent.
//!
//! The daemon owns this process's stdin and stdout and speaks pi's RPC JSONL
//! through them. This process copies those streams to the real `pi` child and
//! watches the lifeline (fd 3 on Unix). When the daemon dies, the lifeline
//! closes and the whole group is stopped, the same way `__run` stops shells.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use crate::sys::RUNNER_LIFELINE_FD;

pub fn main(argv: Vec<String>) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        eprintln!("pi-famulus: __rpc needs a command");
        return 127;
    };
    #[cfg(unix)]
    {
        let _ = crate::sys::set_cloexec(RUNNER_LIFELINE_FD);
        let _ = crate::sys::block_signal(libc::SIGTERM);
        let me = crate::sys::getpid();
        thread::spawn(move || watch_lifeline(me));
    }
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pi-famulus: cannot run {program}: {e}");
            return 127;
        }
    };
    let mut child_in = child.stdin.take().expect("stdin");
    let mut child_out = child.stdout.take().expect("stdout");
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if child_in.write_all(&buf[..n]).and_then(|_| child_in.flush()).is_err() {
                        break;
                    }
                }
            }
        }
        drop(child_in);
    });
    let copy = thread::spawn(move || {
        let mut stdout = io::stdout();
        let mut buf = [0u8; 8192];
        loop {
            match child_out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).and_then(|_| stdout.flush()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let status = child.wait();
    let _ = copy.join();
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
    }
}

#[cfg(unix)]
fn watch_lifeline(leader: u32) {
    let mut buf = [0u8; 8];
    unsafe {
        libc::read(RUNNER_LIFELINE_FD, buf.as_mut_ptr().cast(), buf.len());
    }
    let _ = crate::sys::signal_group(leader, crate::sys::SIGTERM);
    thread::sleep(Duration::from_secs(2));
    let _ = crate::sys::signal_group(leader, crate::sys::SIGKILL);
    std::process::exit(128 + 15);
}
