//! In-place upgrade (design doc §3.2).
//!
//! Unix: `exec` the new binary while keeping the same pid and inherited fds.
//! Windows: not supported — clients must restart the manager after replacing
//! the binary (there is no same-pid `exec`).

#[cfg(windows)]
use crate::daemon::Shared;
#[cfg(windows)]
use crate::proto::*;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Version of `handover.json` and the fd contract. The new binary must
/// speak it (`__handover-check`).
pub const FORMAT: u32 = 1;
/// Hidden subcommand answering the preflight.
pub const CHECK_ARG: &str = "__handover-check";
pub(crate) const CHECK_PREFIX: &str = "pi-famulus-handover";

/// The install executable path used for upgrades. Re-resolve it when npm
/// retires its directory so a newly installed stable sibling becomes usable.
pub fn exe_path() -> std::io::Result<PathBuf> {
    static INSTALL_EXE: std::sync::OnceLock<Mutex<Option<PathBuf>>> = std::sync::OnceLock::new();
    let mut cached = INSTALL_EXE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some(path) = cached.as_ref() {
        // The stable sibling may have appeared since the daemon first started.
        let path = non_retired_path(path.clone());
        if path.is_file() {
            *cached = Some(path.clone());
            return Ok(path);
        }
    }
    let path = invoked_exe_path().or_else(current_exe_path).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "cannot locate executable")
    })?;
    let path = non_retired_path(path);
    *cached = Some(path.clone());
    Ok(path)
}

fn invoked_exe_path() -> Option<PathBuf> {
    let invoked = PathBuf::from(std::env::args_os().next()?);
    canonicalize_invoked_exe(&invoked)
}

pub(crate) fn canonicalize_invoked_exe(invoked: &Path) -> Option<PathBuf> {
    has_invoked_path_component(invoked)
        .then(|| std::fs::canonicalize(invoked).ok())
        .flatten()
}

pub(crate) fn has_invoked_path_component(invoked: &Path) -> bool {
    invoked.components().count() > 1
}

fn current_exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok().map(strip_deleted_suffix)
}

pub(crate) fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let Some(original) = path.as_os_str().as_bytes().strip_suffix(b" (deleted)") else {
            return path;
        };
        return PathBuf::from(std::ffi::OsString::from_vec(original.to_vec()));
    }
    #[cfg(not(unix))]
    {
        path
    }
}

pub(crate) fn non_retired_path(path: PathBuf) -> PathBuf {
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            ancestor = dir.parent();
            continue;
        };
        if let Some(stable_name) = retired_component_stable_name(name) {
            let suffix = path.strip_prefix(dir).expect("ancestor prefix");
            let candidate = dir.parent().unwrap_or(dir).join(stable_name).join(suffix);
            if candidate.is_file() {
                return candidate;
            }
        }
        ancestor = dir.parent();
    }
    path
}

pub(crate) fn retired_component_stable_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(".pi-famulus-")?;
    let (platform, nonce) = rest.rsplit_once('-')?;
    let valid_platform = !platform.is_empty()
        && platform
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    // npm's retire-path hashes the original path, removes non-alphanumerics
    // from its base64 form, then takes the first eight characters.
    let valid_nonce = nonce.len() == 8 && nonce.bytes().all(|b| b.is_ascii_alphanumeric());
    (valid_platform && valid_nonce).then_some(&name[1..name.len() - nonce.len() - 1])
}

pub fn check_line() -> String {
    format!("{CHECK_PREFIX} {FORMAT} {}", crate::VERSION)
}

pub fn file_path(home: &Path) -> PathBuf {
    home.join("handover.json")
}

/// A preflighted upgrade, waiting for the accept loop.
pub struct Ready {
    pub exe: PathBuf,
    pub to_version: String,
    pub trigger: String,
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

/// Record that an upgrade was requested on a platform that cannot do it.
#[cfg(windows)]
pub fn fail_unsupported(state: &Shared, ready: Ready) -> String {
    let from = crate::VERSION.to_string();
    let msg = "in-place upgrade is not supported on Windows".to_string();
    let mut st = state.lock().unwrap();
    crate::lifecycle::log_line(&st.home, &format!("upgrade failed ({}): {msg}", ready.trigger));
    st.last_upgrade = Some(UpgradeInfo {
        at: now_ms(),
        ok: false,
        from_version: from,
        to_version: Some(ready.to_version),
        error: Some(msg.clone()),
        trigger: ready.trigger,
    });
    st.upgrade_pending = false;
    msg
}

#[cfg(windows)]
pub fn request(_state: &Shared, _trigger: &str) -> bool {
    false
}

#[cfg(test)]
mod path_tests {
    use crate::handover::{
        canonicalize_invoked_exe, has_invoked_path_component, non_retired_path,
        retired_component_stable_name, strip_deleted_suffix,
    };
    use std::path::PathBuf;

    #[test]
    fn npm_retirement_names_require_the_actual_nonce_shape() {
        assert_eq!(
            retired_component_stable_name(".pi-famulus-linux-x64-AWM9wakS"),
            Some("pi-famulus-linux-x64")
        );
        for name in [
            ".pi-famulus-linux-x64",
            ".pi-famulus-linux-x64-custom1",
            ".pi-famulus-linux-x64-1234567",
            ".pi-famulus-linux-x64-1234567_",
        ] {
            assert_eq!(retired_component_stable_name(name), None, "{name}");
        }
    }

    #[test]
    fn slashless_invoked_names_are_not_canonicalized_from_the_working_directory() {
        assert!(!has_invoked_path_component(std::path::Path::new("pi-famulus")));
        assert!(has_invoked_path_component(std::path::Path::new("./pi-famulus")));
        assert_eq!(
            canonicalize_invoked_exe(std::path::Path::new("pi-famulus")),
            None
        );
    }

    #[test]
    fn current_executable_deleted_suffix_is_removed() {
        assert_eq!(
            strip_deleted_suffix(PathBuf::from("/tmp/pi-famulus (deleted)")),
            PathBuf::from("/tmp/pi-famulus")
        );
        assert_eq!(
            strip_deleted_suffix(PathBuf::from("/tmp/pi-famulus")),
            PathBuf::from("/tmp/pi-famulus")
        );
    }

    #[test]
    fn legitimate_hidden_package_names_are_not_remapped() {
        let modules = std::env::temp_dir().join(format!(
            "pi-famulus-handover-path-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&modules);
        let stable = modules.join("pi-famulus-linux-x64/bin/pi-famulus");
        std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
        std::fs::write(&stable, b"stable").unwrap();
        let hidden = modules.join(".pi-famulus-linux-x64-custom1/bin/pi-famulus");
        std::fs::create_dir_all(hidden.parent().unwrap()).unwrap();
        std::fs::write(&hidden, b"hidden").unwrap();

        assert_eq!(non_retired_path(hidden.clone()), hidden);
        std::fs::remove_dir_all(modules).unwrap();
    }
}

