//! Machine-wide agent-slot budget. CPU tokens and per-provider in-flight
//! limits are deferred: add them when resource-aware scheduling lands.
use serde_json::{Map, Value};
use std::{fs, path::Path};

pub const DEFAULT_MAX_AGENTS: usize = 8;
pub const DEFAULT_MAX_TEST: usize = 2;

pub fn set_max_test(home: &Path, count: usize) -> Result<usize, String> {
    if count == 0 {
        return Err("max-test must be at least 1".into());
    }
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let _lock = ConfigLock::acquire(home)?;
    let path = home.join("config.json");
    let mut value = config(&path)?;
    let previous = max_kind_from(&value, "maxTest", DEFAULT_MAX_TEST)?;
    let object = value
        .as_object_mut()
        .ok_or("config.json must contain a JSON object")?;
    object.insert("maxTest".into(), Value::from(count as u64));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    let tmp = home.join(format!("config.json.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    crate::events::emit(
        home,
        None,
        "capacity.changed",
        None,
        serde_json::json!({"budget":"max-test", "previous":previous, "total":count}),
    );
    Ok(previous)
}

pub fn max_kind(home: &Path, kind: &str) -> Result<usize, String> {
    let value = config(&home.join("config.json"))?;
    let key = match kind {
        "test-suite" => "maxTest",
        "test" => "maxTest",
        _ => return Ok(usize::MAX),
    };
    max_kind_from(&value, key, DEFAULT_MAX_TEST)
}

fn max_kind_from(value: &Value, key: &str, default: usize) -> Result<usize, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .map(Ok)
        .unwrap_or_else(|| {
            if value.get(key).is_none() {
                Ok(default)
            } else {
                Err(format!("config.json {key} must be a positive integer"))
            }
        })
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
        assert_eq!(config(&dir.join("config.json")).unwrap()["maxAgents"], 12);
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
            assert!(set_max_agents(&dir, 12).is_err(), "rewrote {corrupt}");
            assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
