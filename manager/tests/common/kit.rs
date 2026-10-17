//! Support for the `harness = false` suites (`tests/platform.rs`,
//! `tests/resource_bench.rs`): a small parallel test runner, and `taskkit`,
//! the task program those suites run.
//!
//! Tasks run through the platform shell (`/bin/sh`, or bash / `cmd.exe` on
//! Windows), whose builtins differ. A task that needs to sleep, print its
//! pid, leave a child behind or write exact bytes runs this same test binary
//! instead (`"<exe>" taskkit <verb> ...`), which behaves the same everywhere.
//! Only a quoted program path, plain words and `&&` cross the shell.

use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type TestFn = fn();

/// Dispatch for a `harness = false` test binary: run `taskkit` when invoked
/// as a task, the helper client when re-executed as one, else the tests.
pub fn main(tests: &[(&'static str, TestFn)]) {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("taskkit") {
        std::process::exit(taskkit(&args[2..]));
    }
    super::helper_main();
    std::process::exit(run_tests(tests, &args[1..]));
}

/// `"<this test binary>" taskkit <args>` as a shell command.
pub fn kit(args: &[&str]) -> String {
    let exe = std::env::current_exe().expect("current_exe");
    format!("\"{}\" taskkit {}", exe.display(), args.join(" "))
}

/// What `taskkit bytes <n>` writes: 1 KiB lines of `x` ending in `\n`, the
/// last one shortened to fit.
pub fn bytes_pattern(n: usize) -> Vec<u8> {
    let mut v = vec![b'x'; n];
    for i in (1023..n).step_by(1024) {
        v[i] = b'\n';
    }
    if let Some(last) = v.last_mut() {
        *last = b'\n';
    }
    v
}

/// Accepts the libtest arguments cargo and CI pass (`--test-threads N`,
/// `--nocapture`, `--exact`, `-q`, …); positional arguments are name
/// filters (substring, or exact with `--exact`).
fn run_tests(tests: &[(&'static str, TestFn)], args: &[String]) -> i32 {
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let mut threads = std::env::var("RUST_TEST_THREADS").ok().and_then(|v| v.parse().ok());
    let (mut exact, mut list) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--test-threads" => threads = it.next().and_then(|v| v.parse().ok()),
            "--exact" => exact = true,
            "--list" => list = true,
            "--skip" => {
                if let Some(v) = it.next() {
                    skips.push(v.to_string());
                }
            }
            "--ignored" | "--include-ignored" => {}
            s if s.starts_with("--test-threads=") => threads = s["--test-threads=".len()..].parse().ok(),
            s if s.starts_with("--skip=") => skips.push(s["--skip=".len()..].to_string()),
            s if s.starts_with('-') => {}
            s => filters.push(s.to_string()),
        }
    }
    let selected: Vec<&(&str, TestFn)> = tests
        .iter()
        .filter(|(name, _)| {
            let pass = filters.is_empty() || filters.iter().any(|f| if exact { name == f } else { name.contains(f.as_str()) });
            let skipped = skips.iter().any(|s| if exact { name == s } else { name.contains(s.as_str()) });
            pass && !skipped
        })
        .collect();
    if list {
        for (name, _) in &selected {
            println!("{name}: test");
        }
        return 0;
    }
    let threads: usize = threads
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .clamp(1, selected.len().max(1));
    println!("\nrunning {} tests", selected.len());
    // Hold the MSYS runtime's shared state for the whole run (see MsysKeeper).
    let _msys = MsysKeeper::start();
    let next = AtomicUsize::new(0);
    let failed = Mutex::new(Vec::new());
    let started = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((name, f)) = selected.get(i) else { break };
                let t0 = Instant::now();
                super::enter_test_scope(i + 1);
                let ok = catch_unwind(AssertUnwindSafe(f)).is_ok();
                super::end_test_scope();
                let took = t0.elapsed().as_secs_f64();
                println!("test {name} ... {} ({took:.1}s)", if ok { "ok" } else { "FAILED" });
                if !ok {
                    failed.lock().unwrap().push(*name);
                }
            });
        }
    });
    let failed = failed.into_inner().unwrap();
    let took = started.elapsed().as_secs_f64();
    if failed.is_empty() {
        println!("\ntest result: ok. {} passed; 0 failed; finished in {took:.1}s\n", selected.len());
        0
    } else {
        println!("\nfailures:");
        for f in &failed {
            println!("    {f}");
        }
        println!(
            "\ntest result: FAILED. {} passed; {} failed; finished in {took:.1}s\n",
            selected.len() - failed.len(),
            failed.len()
        );
        101
    }
}

/// The task program. Verbs:
/// - `echo <words…>`: the words and a newline
/// - `argv <args…>`: each argument on its own line (shell quoting checks)
/// - `exit <code>`, `sleep <ms>`
/// - `pid`: own pid; `env <NAME>`: its value or `<unset>`; `cwd`
/// - `stderr <words…>`: the words on stderr
/// - `bytes <n>`: exactly n bytes of `x…x\n` lines (1 KiB each)
/// - `utf8`: multi-byte text written one byte per write, with pauses
/// - `tree <ms>`: start a child that sleeps `ms`, print `<own pid> <child pid>`, sleep `ms`
/// - `leave <ms>`: start a child that sleeps `ms` with stdio detached, print
///   its pid, exit 0 at once (a leftover, like `sleep 300 >/dev/null &`)
pub fn taskkit(args: &[String]) -> i32 {
    let mut out = std::io::stdout().lock();
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    let num = |i: usize| -> u64 { rest.get(i).and_then(|s| s.parse().ok()).unwrap_or(0) };
    match verb {
        "echo" => {
            let _ = writeln!(out, "{}", rest.join(" "));
        }
        "argv" => {
            for a in rest {
                let _ = writeln!(out, "{a}");
            }
        }
        "exit" => return num(0) as i32,
        "sleep" => std::thread::sleep(Duration::from_millis(num(0))),
        "pid" => {
            let _ = writeln!(out, "{}", std::process::id());
        }
        "env" => {
            let name = rest.first().map(String::as_str).unwrap_or("");
            let _ = writeln!(out, "{}", std::env::var(name).unwrap_or_else(|_| "<unset>".into()));
        }
        "cwd" => {
            let _ = writeln!(out, "{}", std::env::current_dir().unwrap().display());
        }
        "stderr" => eprintln!("{}", rest.join(" ")),
        "bytes" => {
            if out.write_all(&bytes_pattern(num(0) as usize)).is_err() {
                return 1;
            }
        }
        "utf8" => {
            for b in "aé中😀b\n".as_bytes() {
                let _ = out.write_all(&[*b]);
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        "tree" => {
            let ms = num(0);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["taskkit", "sleep", &ms.to_string()])
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn child");
            let _ = writeln!(out, "{} {}", std::process::id(), child.id());
            let _ = out.flush();
            std::thread::sleep(Duration::from_millis(ms));
            let _ = child.wait();
        }
        "leave" => {
            // The child is the leftover the caller asserts on. It has to
            // outlive this task program, so it is detached on purpose.
            let ms = num(0).to_string();
            #[allow(clippy::zombie_processes)]
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["taskkit", "sleep", &ms])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn child");
            let _ = writeln!(out, "{}", child.id());
            let _ = out.flush();
            std::mem::forget(child);
            return 0;
        }
        other => {
            eprintln!("taskkit: unknown verb {other:?}");
            return 2;
        }
    }
    let _ = out.flush();
    0
}

/// Keeps one MSYS process (Git Bash) alive while the tests run.
///
/// Every MSYS process maps a per-user shared memory section at startup; the
/// first one to map it builds the mount table. When many `bash.exe` cold
/// starts race to build it (the tests start shell tasks from every test
/// thread), a loser can spin for ~15s and die with
/// `fatal error - add_item ("\??\C:\Program Files\Git", "/", ...) failed,
/// errno 1`, or crash with STATUS_ACCESS_VIOLATION; while new starts keep
/// coming the section stays poisoned and every later bash hangs the same way
/// (git-for-windows/git#6368). One long-lived MSYS process that has finished
/// starting owns the section, so later starts only open it and never rebuild
/// it.
///
/// It exits by itself when its stdin closes: force-killing MSYS processes is
/// itself one of the ways to poison that state.
struct MsysKeeper {
    #[cfg(windows)]
    child: Option<std::process::Child>,
}

impl MsysKeeper {
    #[cfg(not(windows))]
    fn start() -> Self {
        MsysKeeper {}
    }

    #[cfg(windows)]
    fn start() -> Self {
        use std::io::{BufRead, BufReader};
        // Same resolution the daemon uses for task shells.
        let shell = pi_famulus_task_shell();
        let Some(bash) = shell else { return MsysKeeper { child: None } };
        let child = Command::new(&bash)
            .args(["-c", "echo ready; read _"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return MsysKeeper { child: None } };
        // "ready" means the runtime finished initializing its shared state.
        let mut line = String::new();
        if let Some(out) = child.stdout.take() {
            let _ = BufReader::new(out).read_line(&mut line);
        }
        if line.trim() != "ready" {
            eprintln!("warning: MSYS keeper did not start ({bash:?}); shell tasks may race");
        }
        MsysKeeper { child: Some(child) }
    }
}

#[cfg(windows)]
impl Drop for MsysKeeper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take()); // EOF ends `read`; bash exits on its own.
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// The POSIX shell the daemon runs tasks in on Windows, mirroring
/// `sys::windows::resolve_task_shell`; None when tasks would use `cmd.exe`.
#[cfg(windows)]
fn pi_famulus_task_shell() -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    if let Some(s) = std::env::var_os("PI_FAMULUS_SHELL").filter(|s| !s.is_empty()) {
        let p = PathBuf::from(s);
        let cmd = p.file_stem().map(|s| s.to_string_lossy().eq_ignore_ascii_case("cmd")).unwrap_or(false);
        return if cmd { None } else { Some(p) };
    }
    for root in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(root) {
            let bash = Path::new(&dir).join("Git").join("bin").join("bash.exe");
            if bash.is_file() {
                return Some(bash);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("bash.exe")).find(|b| {
        let lower = b.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
        let wsl = lower.ends_with("\\system32\\bash.exe") || lower.ends_with("\\sysnative\\bash.exe");
        !wsl && b.is_file()
    })
}
