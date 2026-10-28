//! The environment every test binary runs in (unit tests: `main.rs`;
//! integration tests: `tests/common/mod.rs` includes this file).
//!
//! The test setup decides the environment, not the developer's machine.
//! Before `main`, a Unix test binary re-executes itself (same pid, same
//! arguments) with an environment built here from scratch:
//! - `HOME` is an empty directory of its own, so `~/.claude`, `~/.codex`,
//!   `~/.config/herdr/herdr.sock` and the like resolve to nothing;
//! - `PATH` is the host PATH without `claude`, `pi`, `herdr` or `codex`
//!   ([`HOST_TOOLS`]; everything else, `sh`, `python3`, `openssl`..., stays);
//! - `TZ=UTC`, `LC_ALL=C`;
//! - of the rest, only [`KEPT`] passes: no proxy, no `XDG_*`, no
//!   `HERDR_*`/`CODEX_*`/`CLAUDE_*` endpoint, no `PI_FAMULUS_HOME`, no
//!   `TERM`/`COLUMNS`/`NO_COLOR`.
//!
//! Every reader of the environment, product code included, then sees these
//! values without a per-site test branch, and so does every child a test
//! starts. `std::env::set_var` is never called: the process is started in
//! this environment instead of having its own changed under running threads.
//!
//! A test that genuinely needs a host resource opts in explicitly: the host
//! PATH is kept as [`HOST_PATH_ENV`] (see [`host_path`]).
//!
//! Windows has no `exec`, and a wrapper process would sit between the test
//! and its job object; there, `tests/common` hides the tools from children
//! and HTTP clients under test ignore proxies (`intercom::config::http_client`).

/// Set in the re-executed process; its presence means "already hermetic".
pub const MARKER_ENV: &str = "PI_FAMULUS_TEST_HERMETIC";
/// The host PATH, for a test that opts in to host tools.
pub const HOST_PATH_ENV: &str = "PI_FAMULUS_TEST_HOST_PATH";
/// Programs a test must never reach on the host.
pub const HOST_TOOLS: [&str; 4] = ["claude", "pi", "herdr", "codex"];
/// Host variables a test binary keeps: the toolchain's and CI's own knobs,
/// where scratch files go, and who the user is.
pub const KEPT: [&str; 11] = [
    "TMPDIR",
    "USER",
    "LOGNAME",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "LLVM_PROFILE_FILE",
    "GITHUB_STEP_SUMMARY",
    "PI_FAMULUS_TEST_ARTIFACTS",
    "PI_FAMULUS_BENCH",
    "PI_FAMULUS_BENCH_OUT",
];
/// Kept by prefix: `RUST_BACKTRACE`, `RUST_TEST_THREADS`, `CARGO_*`...
pub const KEPT_PREFIXES: [&str; 2] = ["RUST_", "CARGO"];

/// Whether a host variable passes into the test environment.
pub fn kept(key: &str) -> bool {
    KEPT.contains(&key) || KEPT_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// The host PATH, for a test that opts in to host tools.
#[allow(dead_code)]
pub fn host_path() -> Option<std::ffi::OsString> {
    // Windows test binaries run in the host environment itself.
    std::env::var_os(HOST_PATH_ENV).or_else(|| std::env::var_os("PATH"))
}

#[cfg(unix)]
mod reexec {
    use std::ffi::{OsStr, OsString};
    use std::os::raw::{c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};

    // Runs before `main`. dyld passes argc/argv/envp to initializers; on
    // Linux only glibc does (musl passes nothing), so there the arguments
    // come from /proc instead.
    #[used]
    #[cfg_attr(target_vendor = "apple", link_section = "__DATA,__mod_init_func")]
    #[cfg_attr(not(target_vendor = "apple"), link_section = ".init_array")]
    static ENTER: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = enter;

    extern "C" fn enter(argc: c_int, argv: *const *const c_char, _envp: *const *const c_char) {
        if std::env::var_os(super::MARKER_ENV).is_some() {
            return;
        }
        #[cfg(target_vendor = "apple")]
        let args: Vec<OsString> = if argv.is_null() {
            Vec::new()
        } else {
            // SAFETY: dyld passes the process's own argc/argv.
            (0..argc.max(0) as usize)
                .filter_map(|i| unsafe { argv.add(i).read().as_ref() })
                .map(|arg| OsStr::from_bytes(unsafe { std::ffi::CStr::from_ptr(arg) }.to_bytes()).to_owned())
                .collect()
        };
        #[cfg(not(target_vendor = "apple"))]
        let args: Vec<OsString> = {
            let _ = (argc, argv);
            let cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_default();
            let cmdline = cmdline.strip_suffix(&[0]).unwrap_or(&cmdline);
            cmdline.split(|b| *b == 0).map(|arg| OsStr::from_bytes(arg).to_owned()).collect()
        };
        // A test binary started as a task program (`taskkit`, tests/common/
        // kit.rs) runs with exactly the task environment a test asked for.
        if args.get(1).is_some_and(|arg| arg == "taskkit") {
            return;
        }
        let error = match std::env::current_exe() {
            Ok(exe) => {
                let mut cmd = std::process::Command::new(exe);
                if let Some((arg0, rest)) = args.split_first() {
                    cmd.arg0(arg0).args(rest);
                }
                cmd.env_clear().envs(environment()).exec()
            }
            Err(error) => error,
        };
        eprintln!("pi-famulus tests: cannot re-execute in the test environment: {error}");
        std::process::exit(101);
    }

    fn environment() -> Vec<(OsString, OsString)> {
        let root = std::env::temp_dir().join(format!("pfu-{}", std::process::id()));
        remove_dead_roots(&root);
        let home = root.join("home");
        if let Err(error) = std::fs::create_dir_all(&home) {
            eprintln!("pi-famulus tests: create {}: {error}", home.display());
            std::process::exit(101);
        }
        let host_path = std::env::var_os("PATH").unwrap_or_default();
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(key, _)| key.to_str().is_some_and(super::kept))
            .collect();
        env.extend([
            ("HOME".into(), home.into_os_string()),
            ("PATH".into(), without_tools(&host_path, &root.join("path"))),
            ("TZ".into(), "UTC".into()),
            ("LC_ALL".into(), "C".into()),
            (super::HOST_PATH_ENV.into(), host_path),
            (super::MARKER_ENV.into(), "1".into()),
        ]);
        env
    }

    /// `path` with every directory that holds a [`super::HOST_TOOLS`] name
    /// replaced, in place, by a directory of symlinks to everything else in
    /// it (dropping the directory would also drop `sh`, `python3`... that a
    /// package manager installed beside the tool). Names match
    /// case-insensitively, as on the default macOS file system.
    fn without_tools(path: &OsStr, mirrors: &Path) -> OsString {
        let is_tool = |name: &OsStr| {
            name.to_str()
                .is_some_and(|name| super::HOST_TOOLS.iter().any(|tool| name.eq_ignore_ascii_case(tool)))
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for (index, dir) in std::env::split_paths(path).enumerate() {
            let entries: Vec<OsString> = match std::fs::read_dir(&dir) {
                Ok(entries) => entries.flatten().map(|e| e.file_name()).collect(),
                Err(_) => {
                    dirs.push(dir);
                    continue;
                }
            };
            if !entries.iter().any(|name| is_tool(name)) {
                dirs.push(dir);
                continue;
            }
            if !dir.is_absolute() {
                continue;
            }
            let mirror = mirrors.join(index.to_string());
            if std::fs::create_dir_all(&mirror).is_err() {
                continue;
            }
            for name in entries.iter().filter(|name| !is_tool(name)) {
                let _ = std::os::unix::fs::symlink(dir.join(name), mirror.join(name));
            }
            dirs.push(mirror);
        }
        std::env::join_paths(dirs).unwrap_or_default()
    }

    /// Roots left by test processes that have exited.
    fn remove_dead_roots(own: &Path) {
        let (Some(parent), Some(own_name)) = (own.parent(), own.file_name()) else { return };
        let Ok(entries) = std::fs::read_dir(parent) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("pfu-")) else { continue };
            let Ok(pid) = pid.parse::<i32>() else { continue };
            // SAFETY: signal 0 only checks that the pid exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if name != own_name && !alive {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// The checks behind the hermetic-environment guards: the variables a
/// Unix test process sees are exactly the ones this module decided on.
#[allow(dead_code)]
pub fn violations() -> Vec<String> {
    let mut found = Vec::new();
    if cfg!(windows) {
        return found;
    }
    if std::env::var_os(MARKER_ENV).is_none() {
        found.push(format!("{MARKER_ENV} unset: the test binary did not re-execute"));
    }
    // macOS adds __CF_USER_TEXT_ENCODING to every process it starts.
    let set = ["HOME", "PATH", "TZ", "LC_ALL", HOST_PATH_ENV, MARKER_ENV, "__CF_USER_TEXT_ENCODING"];
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if !set.contains(&key.as_ref()) && !kept(&key) {
            found.push(format!("host variable {key} reached the test process"));
        }
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    if !home.ends_with(format!("pfu-{}/home", std::process::id())) {
        found.push(format!("HOME is {}, not this process's own", home.display()));
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for tool in HOST_TOOLS {
            if dir.join(tool).exists() {
                found.push(format!("{tool} is on PATH in {}", dir.display()));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    /// Runs in the unit-test binary and in every integration-test binary
    /// with a harness: none of them may see the host's environment.
    #[test]
    fn test_process_environment_is_hermetic() {
        let violations = super::violations();
        assert!(violations.is_empty(), "{}", violations.join("\n"));
    }
}
