//! Which executable a client runs to start the manager.
//!
//! The manager is `pi-famulus --home <home> daemon`. A client inside the
//! `pi-famulus` binary runs its own executable. Any other program that links
//! this library (the desktop GUI, a test binary) must never do that: its
//! executable does not understand `daemon`, so "starting the daemon" would
//! launch another copy of that program. So nothing is started unless the
//! program has said which executable is the daemon binary:
//!
//! - the `pi-famulus` binary declares itself, first thing in `main`;
//! - a frontend looks for the installed binary ([`find`]) and names it;
//! - a program that says nothing only attaches to a manager that already
//!   runs, and `client::connect` fails with [`NO_DAEMON`] when there is none.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The executable that is run with `--home <home> daemon`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonBinary {
    /// This process's own executable: only for the `pi-famulus` binary.
    ThisExecutable,
    /// The `pi-famulus` binary at this path. Refused at spawn time if it
    /// turns out to be this process's own executable.
    At(PathBuf),
}

/// What a client is told when no manager runs and none may be started.
pub const NO_DAEMON: &str =
    "no pi-famulus daemon is running, and this program has no daemon binary to start one";

static DECLARED: Mutex<Option<DaemonBinary>> = Mutex::new(None);

/// Declares the daemon binary for this process, or (`None`) that there is
/// none, which is also the state before the first call. A frontend may call
/// it again when what it finds changes.
pub fn declare(binary: Option<DaemonBinary>) {
    *DECLARED.lock().unwrap_or_else(|p| p.into_inner()) = binary;
}

/// The declared daemon binary, if any.
pub fn declared() -> Option<DaemonBinary> {
    DECLARED.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// The path to run for `declared`, or why nothing may be run.
pub(crate) fn executable(declared: Option<DaemonBinary>) -> Result<PathBuf, String> {
    match declared {
        None => Err(NO_DAEMON.into()),
        Some(DaemonBinary::ThisExecutable) => {
            #[cfg(target_os = "linux")]
            let exe = PathBuf::from("/proc/self/exe");
            #[cfg(not(target_os = "linux"))]
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            Ok(exe)
        }
        Some(DaemonBinary::At(path)) => {
            let this = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            if same_file(&path, &this) {
                return Err(format!(
                    "{} is this program, not the pi-famulus daemon binary; not starting it",
                    path.display()
                ));
            }
            Ok(path)
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// File name of the binary in `<home>/bin` and on `PATH`.
pub const FILE_NAME: &str = if cfg!(windows) {
    "pi-famulus.exe"
} else {
    "pi-famulus"
};

/// The npm package that carries the binary for `os` / `arch` (Rust's
/// `target_os` / `target_arch` names), as the extension names it
/// (`nativePackageName`): `pi-famulus-<linux|darwin|win>-<x64|arm64>`.
/// `None` for a platform no package is published for.
pub fn native_package_name(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("pi-famulus-{os}-{arch}"))
}

/// Where the native npm package of this platform may be installed, in the
/// extension's order: next to the extension in pi's npm tree
/// (`<pi agent dir>/npm/node_modules`), then the extension's per-version
/// user cache for `version`.
pub fn native_package_dirs(
    agent_dir: &Path,
    user_home: &Path,
    version: &str,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let Some(name) = native_package_name(std::env::consts::OS, std::env::consts::ARCH) else {
        return Vec::new();
    };
    let var = |key: &str| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    // The cache directory is named with Node's platform (`win32`), the
    // package with npm's (`win`).
    let (cache_base, node_os) = match std::env::consts::OS {
        "windows" => (
            var("LOCALAPPDATA").unwrap_or_else(|| user_home.join("AppData").join("Local")),
            "win32",
        ),
        "macos" => (
            var("XDG_CACHE_HOME").unwrap_or_else(|| user_home.join(".cache")),
            "darwin",
        ),
        other => (
            var("XDG_CACHE_HOME").unwrap_or_else(|| user_home.join(".cache")),
            other,
        ),
    };
    let node_arch = name.rsplit('-').next().unwrap_or_default().to_string();
    vec![
        agent_dir.join("npm").join("node_modules").join(&name),
        cache_base
            .join("pi-famulus")
            .join("native")
            .join(version)
            .join(format!("{node_os}-{node_arch}"))
            .join("node_modules")
            .join(&name),
    ]
}

/// Where [`find`] looks, in order: the order the extension uses
/// (docs/configuration.md, "Manager binary discovery"), then the usual
/// install directories. The extension finds its npm package by walking up
/// from its own files; here that step is the two places such an install
/// puts it ([`native_package_dirs`]).
#[derive(Clone, Debug, Default)]
pub struct Search {
    /// `managerPath` of `<home>/config.json`.
    pub config_path: Option<PathBuf>,
    /// `PI_FAMULUS_MANAGER_PATH`.
    pub env_path: Option<PathBuf>,
    /// Directories of the native npm package (`<dir>/bin/pi-famulus`).
    pub native_dirs: Vec<PathBuf>,
    /// `<home>/bin`.
    pub home_bin: PathBuf,
    /// The directories of `PATH`.
    pub path_dirs: Vec<PathBuf>,
    /// Usual install directories, for a program started without a shell's
    /// `PATH` (an app opened from the Finder or a desktop launcher).
    pub fallback_dirs: Vec<PathBuf>,
    /// This process's executable, which is never an answer.
    pub this_exe: Option<PathBuf>,
}

impl Search {
    /// The search for `home` in this process's environment.
    pub fn from_env(home: &Path) -> Search {
        let config_path = std::fs::read(home.join("config.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|v| {
                v.get("managerPath")
                    .and_then(|p| p.as_str())
                    .map(PathBuf::from)
            })
            .filter(|p| !p.as_os_str().is_empty());
        let env_path = std::env::var_os("PI_FAMULUS_MANAGER_PATH")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        let path_dirs = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|d| !d.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Search {
            config_path,
            env_path,
            native_dirs: native_package_dirs(
                &crate::lifecycle::pi_agent_dir(),
                &crate::lifecycle::user_home(),
                env!("CARGO_PKG_VERSION"),
                |key| std::env::var_os(key),
            ),
            home_bin: home.join("bin"),
            path_dirs,
            fallback_dirs: fallback_dirs(std::env::var_os("HOME").as_deref()),
            this_exe: std::env::current_exe().ok(),
        }
    }

    /// Every place looked at, in order, for a "not found" message.
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        out.extend(self.config_path.clone());
        out.extend(self.env_path.clone());
        out.extend(
            self.native_dirs
                .iter()
                .map(|d| d.join("bin").join(FILE_NAME)),
        );
        out.push(self.home_bin.join(FILE_NAME));
        out.extend(
            self.path_dirs
                .iter()
                .chain(&self.fallback_dirs)
                .map(|d| d.join(FILE_NAME)),
        );
        out
    }

    /// The first candidate that is an executable file other than this
    /// process's own executable.
    pub fn find(&self) -> Option<PathBuf> {
        self.candidates().into_iter().find(|p| self.usable(p))
    }

    fn usable(&self, path: &Path) -> bool {
        is_executable_file(path)
            && !self
                .this_exe
                .as_deref()
                .is_some_and(|this| same_file(path, this))
    }
}

fn fallback_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    if cfg!(windows) {
        return Vec::new();
    }
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = home.filter(|h| !h.is_empty()) {
        dirs.push(Path::new(home).join(".local/bin"));
    }
    dirs
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// The installed `pi-famulus` binary a frontend may start for `home`:
/// `managerPath` of `<home>/config.json`, `PI_FAMULUS_MANAGER_PATH`, the
/// native npm package, `<home>/bin`, `PATH`, then the usual install
/// directories.
pub fn find(home: &Path) -> Option<PathBuf> {
    Search::from_env(home).find()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let path = std::env::temp_dir().join(format!(
                "pi-famulus-daemon-binary-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
        /// An executable file at `rel`.
        fn exe(&self, rel: &str) -> PathBuf {
            let path = self.file(rel);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        /// A file at `rel` that is not executable.
        fn file(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, rel).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            path
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// This platform's package, as the extension and `npm/` name it.
    fn package() -> String {
        native_package_name(std::env::consts::OS, std::env::consts::ARCH)
            .expect("a published platform")
    }

    /// Every step present: config, env, the npm package in pi's tree and in
    /// the cache, home/bin, two PATH dirs, a fallback.
    fn full(dir: &Dir) -> (Search, [PathBuf; 8]) {
        let pkg = package();
        let native =
            native_package_dirs(&dir.0.join("agent"), &dir.0.join("user"), "9.9.9", |_| None);
        let places = [
            dir.exe("config/pi-famulus-custom"),
            dir.exe("env/pi-famulus-env"),
            dir.exe(&format!("agent/npm/node_modules/{pkg}/bin/pi-famulus")),
            dir.exe(&format!(
                "user/.cache/pi-famulus/native/9.9.9/{}/node_modules/{pkg}/bin/pi-famulus",
                pkg.strip_prefix("pi-famulus-").unwrap()
            )),
            dir.exe("home/bin/pi-famulus"),
            dir.exe("path1/pi-famulus"),
            dir.exe("path2/pi-famulus"),
            dir.exe("fallback/pi-famulus"),
        ];
        let search = Search {
            config_path: Some(places[0].clone()),
            env_path: Some(places[1].clone()),
            native_dirs: native,
            home_bin: dir.0.join("home/bin"),
            path_dirs: vec![dir.0.join("path1"), dir.0.join("path2")],
            fallback_dirs: vec![dir.0.join("fallback")],
            this_exe: None,
        };
        (search, places)
    }

    #[test]
    fn the_npm_package_is_named_and_placed_as_the_extension_does() {
        // The six published packages, by Rust's names for the platform.
        let published = [
            ("linux", "x86_64", "npm/linux-x64", "pi-famulus-linux-x64"),
            (
                "linux",
                "aarch64",
                "npm/linux-arm64",
                "pi-famulus-linux-arm64",
            ),
            ("macos", "x86_64", "npm/darwin-x64", "pi-famulus-darwin-x64"),
            (
                "macos",
                "aarch64",
                "npm/darwin-arm64",
                "pi-famulus-darwin-arm64",
            ),
            ("windows", "x86_64", "npm/win32-x64", "pi-famulus-win-x64"),
            (
                "windows",
                "aarch64",
                "npm/win32-arm64",
                "pi-famulus-win-arm64",
            ),
        ];
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for (os, arch, directory, name) in published {
            assert_eq!(native_package_name(os, arch).as_deref(), Some(name));
            // The name and the binary's place are the package's own.
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(repo.join(directory).join("package.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["name"], name, "{directory}");
            let binary = if os == "windows" {
                "./bin/pi-famulus.exe"
            } else {
                "./bin/pi-famulus"
            };
            assert_eq!(manifest["main"], binary, "{directory}");
        }
        assert_eq!(native_package_name("freebsd", "x86_64"), None);
        assert_eq!(native_package_name("linux", "riscv64"), None);

        // This build's platform: pi's npm tree first, then the cache.
        let pkg = package();
        let id = pkg.strip_prefix("pi-famulus-").unwrap();
        let dirs = native_package_dirs(Path::new("/agent"), Path::new("/user"), "1.2.3", |_| None);
        assert_eq!(
            dirs,
            [
                PathBuf::from(format!("/agent/npm/node_modules/{pkg}")),
                PathBuf::from(format!(
                    "/user/.cache/pi-famulus/native/1.2.3/{id}/node_modules/{pkg}"
                )),
            ]
        );
        let dirs = native_package_dirs(Path::new("/agent"), Path::new("/user"), "1.2.3", |key| {
            (key == "XDG_CACHE_HOME").then(|| "/xdg".into())
        });
        assert_eq!(
            dirs[1],
            PathBuf::from(format!(
                "/xdg/pi-famulus/native/1.2.3/{id}/node_modules/{pkg}"
            ))
        );
    }

    #[test]
    fn the_binary_is_looked_for_in_the_documented_order() {
        let dir = Dir::new("order");
        let (search, places) = full(&dir);
        assert_eq!(
            search.candidates(),
            places,
            "config, env, npm package, cache, home/bin, PATH in order, fallbacks"
        );
        // Remove the winner each time: the next place in the order answers.
        for (i, place) in places.iter().enumerate() {
            assert_eq!(search.find().as_ref(), Some(place), "step {i}");
            std::fs::remove_file(place).unwrap();
        }
        assert_eq!(
            search.find(),
            None,
            "nothing left: not found, never a guess"
        );
    }

    #[test]
    fn a_candidate_that_cannot_be_run_is_passed_over() {
        let dir = Dir::new("unusable");
        let (mut search, places) = full(&dir);
        search.config_path = Some(dir.file("config/not-executable"));
        search.env_path = Some(dir.0.join("env"));
        assert_eq!(
            search.find().as_ref(),
            Some(&places[2]),
            "a plain file and a directory are skipped"
        );
        search.env_path = Some(dir.0.join("env/missing"));
        assert_eq!(
            search.find().as_ref(),
            Some(&places[2]),
            "so is a path that does not exist"
        );
    }

    #[test]
    fn this_programs_own_executable_is_never_the_daemon_binary() {
        let dir = Dir::new("self");
        let (mut search, places) = full(&dir);
        // The program itself is what the config names, and (through a
        // symlink) what the environment names.
        search.this_exe = Some(places[0].clone());
        let link = dir.0.join("env/link-to-self");
        std::os::unix::fs::symlink(&places[0], &link).unwrap();
        search.env_path = Some(link);
        assert_eq!(
            search.find().as_ref(),
            Some(&places[2]),
            "both spellings of this executable are skipped"
        );
        for place in &places[2..] {
            std::fs::remove_file(place).unwrap();
        }
        assert_eq!(
            search.find(),
            None,
            "with only itself to find, nothing is found"
        );

        // And if a program names itself anyway, the spawn refuses it.
        let this = std::env::current_exe().unwrap();
        let error = executable(Some(DaemonBinary::At(this.clone()))).unwrap_err();
        assert!(
            error.contains("is this program, not the pi-famulus daemon binary"),
            "{error}"
        );
        let link = dir.0.join("link-to-this-test");
        std::os::unix::fs::symlink(&this, &link).unwrap();
        assert!(
            executable(Some(DaemonBinary::At(link))).is_err(),
            "a symlink to itself is refused too"
        );
        assert_eq!(
            executable(Some(DaemonBinary::At(places[1].clone()))).unwrap(),
            places[1]
        );
        assert_eq!(
            executable(None).unwrap_err(),
            NO_DAEMON,
            "nothing declared: nothing to run"
        );
    }
}
