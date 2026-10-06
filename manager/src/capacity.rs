//! Machine-wide agent-slot budget. CPU tokens and per-provider in-flight
//! limits are deferred: add them when resource-aware scheduling lands.
use serde_json::{Map, Value};
use std::{fs, path::Path};

pub const DEFAULT_MAX_AGENTS: usize = 8;
pub const DEFAULT_MAX_TEST: usize = 2;
pub const WORK_KINDS: &[&str] = &[
    "test-suite",
    "test",
    "build",
    "lint/type",
    "other",
    "git",
    "read/search",
];

fn kind_setting(kind: &str) -> Option<(&'static str, usize)> {
    match kind {
        "test-suite" => Some(("maxTestSuite", DEFAULT_MAX_TEST)),
        "test" => Some(("maxTest", DEFAULT_MAX_TEST)),
        "build" => Some(("maxBuild", DEFAULT_MAX_AGENTS)),
        "lint/type" => Some(("maxLintType", DEFAULT_MAX_AGENTS)),
        "other" => Some(("maxOther", DEFAULT_MAX_AGENTS)),
        "git" => Some(("maxGit", DEFAULT_MAX_AGENTS)),
        "read/search" => Some(("maxReadSearch", DEFAULT_MAX_AGENTS)),
        _ => None,
    }
}

pub fn is_work_kind(kind: &str) -> bool {
    kind_setting(kind).is_some()
}

/// Return a kind budget. Unconfigured kinds inherit the machine-wide limit;
/// explicit test budgets default to two slots.
pub fn max_kind(home: &Path, kind: &str) -> Result<usize, String> {
    let value = config(&home.join("config.json"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "config.json must contain a JSON object".to_string())?;
    let Some((key, default)) = kind_setting(kind) else {
        return max_agents_from(&value);
    };
    let default = if default == DEFAULT_MAX_AGENTS {
        max_agents_from(&value)?
    } else {
        default
    };
    max_kind_from(object, key, default)
}

/// Set a per-kind budget under the cross-process config lock.
pub fn set_max_kind(home: &Path, kind: &str, count: usize) -> Result<usize, String> {
    let Some((key, _default)) = kind_setting(kind) else {
        return Err(format!("unknown work kind: {kind}"));
    };
    if count == 0 {
        return Err(format!("max-{kind} must be at least 1"));
    }
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let _lock = ConfigLock::acquire(home)?;
    let path = home.join("config.json");
    let mut value = config(&path)?;
    let previous = max_kind(home, kind)?;
    let object = value
        .as_object_mut()
        .ok_or("config.json must contain a JSON object")?;
    object.insert(key.into(), Value::from(count as u64));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    let tmp = home.join(format!("config.json.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    crate::events::emit(
        home,
        None,
        "capacity.changed",
        None,
        serde_json::json!({"budget":format!("max-{kind}"), "previous":previous, "total":count}),
    );
    Ok(previous)
}

fn max_kind_from(object: &Map<String, Value>, key: &str, default: usize) -> Result<usize, String> {
    let Some(raw) = object.get(key) else {
        return Ok(default);
    };
    raw.as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("config.json {key} must be a positive integer"))
}

pub fn config(path: &Path) -> Result<Value, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn max_agents(home: &Path) -> Result<usize, String> {
    max_agents_from(&config(&home.join("config.json"))?)
}

/// Set the budget under a cross-process lock and return the previous value.
pub fn set_max_agents(home: &Path, count: usize) -> Result<usize, String> {
    if count == 0 {
        return Err("max-agents must be at least 1".into());
    }
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let _lock = ConfigLock::acquire(home)?;
    let path = home.join("config.json");
    let mut value = config(&path)?;
    let previous = max_agents_from(&value)?;
    let object = value
        .as_object_mut()
        .ok_or("config.json must contain a JSON object")?;
    object.insert("maxAgents".into(), Value::from(count as u64));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    let tmp = home.join(format!("config.json.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    crate::events::emit(
        home,
        None,
        "capacity.changed",
        None,
        serde_json::json!({"budget":"max-agents", "previous":previous, "total":count}),
    );
    Ok(previous)
}

fn max_agents_from(value: &Value) -> Result<usize, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "config.json must contain a JSON object".to_string())?;
    let Some(raw) = object.get("maxAgents") else {
        return Ok(DEFAULT_MAX_AGENTS);
    };
    raw.as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "config.json maxAgents must be a positive integer".to_string())
}

struct ConfigLock(std::fs::File);

impl ConfigLock {
    fn acquire(home: &Path) -> Result<Self, String> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(home.join("config.json.lock"))
            .map_err(|e| e.to_string())?;
        loop {
            // SAFETY: flock operates on the live file descriptor; the File owns it until Drop.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.to_string());
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the owned lock file descriptor remains valid through Drop.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_runtime_persisted_update() {
        let dir = std::env::temp_dir().join(format!("pi-famulus-capacity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(max_agents(&dir).unwrap(), 8);
        set_max_agents(&dir, 12).unwrap();
        assert_eq!(max_agents(&dir).unwrap(), 12);
        assert_eq!(max_kind(&dir, "build").unwrap(), 12);
        assert_eq!(max_kind(&dir, "test").unwrap(), DEFAULT_MAX_TEST);
        set_max_kind(&dir, "test-suite", 3).unwrap();
        set_max_kind(&dir, "build", 5).unwrap();
        assert_eq!(max_kind(&dir, "test-suite").unwrap(), 3);
        assert_eq!(max_kind(&dir, "build").unwrap(), 5);
        let value = config(&dir.join("config.json")).unwrap();
        assert_eq!(value["maxAgents"], 12);
        assert_eq!(value["maxTestSuite"], 3);
        assert_eq!(value["maxBuild"], 5);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_config_is_refused_and_never_rewritten() {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-capacity-corrupt-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        for corrupt in ["{bad", "[]", "null", "42"] {
            fs::write(&path, corrupt).unwrap();
            assert!(max_agents(&dir).is_err(), "accepted {corrupt}");
            assert!(max_kind(&dir, "test").is_err(), "kind accepted {corrupt}");
            assert!(set_max_agents(&dir, 12).is_err(), "rewrote {corrupt}");
            assert!(set_max_kind(&dir, "test", 12).is_err(), "kind set rewrote {corrupt}");
            assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
