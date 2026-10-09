//! Machine-wide agent-slot budget.
//! deferred | CPU tokens | impact | CPU-bound agents can saturate the host despite an agent-count cap | trigger | measured CPU-aware scheduling is needed
//! deferred | per-provider in-flight limits | impact | concurrent agents can still burst requests to one provider | trigger | provider-aware dispatch or rate limiting is needed
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

/// Parsed capacity configuration reused across a daemon admission/status
/// decision, avoiding repeated disk reads while scanning queued requests.
#[derive(Clone, Debug)]
pub struct CapacityConfig {
    value: Value,
}

impl CapacityConfig {
    pub fn max_agents(&self) -> Result<usize, String> {
        max_agents_from(&self.value)
    }

    /// Return a kind budget. Unconfigured kinds inherit the machine-wide
    /// limit; explicit test budgets default to two slots.
    pub fn max_kind(&self, kind: &str) -> Result<usize, String> {
        max_kind_from_value(&self.value, kind)
    }
}

pub fn load(home: &Path) -> Result<CapacityConfig, String> {
    Ok(CapacityConfig {
        value: config(&home.join("config.json"))?,
    })
}

/// Return a kind budget. Unconfigured kinds inherit the machine-wide limit;
/// explicit test budgets default to two slots.
pub fn max_kind(home: &Path, kind: &str) -> Result<usize, String> {
    load(home)?.max_kind(kind)
}

fn max_kind_from_value(value: &Value, kind: &str) -> Result<usize, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "config.json must contain a JSON object".to_string())?;
    let Some((key, default)) = kind_setting(kind) else {
        return max_agents_from(value);
    };
    let default = if default == DEFAULT_MAX_AGENTS {
        max_agents_from(value)?
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
    let previous = max_kind_from_value(&value, kind)?;
    let object = value
        .as_object_mut()
        .ok_or("config.json must contain a JSON object")?;
    object.insert(key.into(), Value::from(count as u64));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    let tmp = home.join(format!("config.json.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    if previous != count {
        crate::events::emit(
            home,
            None,
            "capacity.changed",
            None,
            serde_json::json!({"budget":format!("max-{kind}"), "previous":previous, "total":count}),
        );
    }
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
    load(home)?.max_agents()
}

/// Cached parsed budget. The metadata stamp catches atomic replacement, while
/// a content hash also catches fast in-place edits (for example config tools
/// or external editors that preserve length and timestamp granularity).
#[derive(Default)]
pub struct MaxAgentsCache {
    cached: Option<(Option<ConfigStamp>, Result<usize, String>)>,
}

impl MaxAgentsCache {
    pub fn get(&mut self, home: &Path) -> Result<usize, String> {
        let path = home.join("config.json");
        let stamp = config_stamp(&path)?;
        if let Some((cached_stamp, value)) = &self.cached {
            if *cached_stamp == stamp {
                return value.clone();
            }
        }
        let value = max_agents(home);
        self.cached = Some((stamp, value.clone()));
        value
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ConfigStamp {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    content_hash: u64,
}

fn config_stamp(path: &Path) -> Result<Option<ConfigStamp>, String> {
    use std::hash::{Hash, Hasher};
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let metadata = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(Some(identity_stamp(&metadata, hasher.finish())))
}

fn identity_stamp(metadata: &fs::Metadata, content_hash: u64) -> ConfigStamp {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ConfigStamp {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            content_hash,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // `file_index` / `volume_serial_number` need the unstable
        // `windows_by_handle` feature. Content hash plus FILETIME is enough
        // to notice a replacement or in-place edit.
        let write = metadata.last_write_time();
        let created = metadata.creation_time();
        ConfigStamp {
            dev: 0,
            ino: 0,
            len: metadata.len(),
            modified: ((write >> 32) as i64, (write & 0xffff_ffff) as i64),
            changed: ((created >> 32) as i64, (created & 0xffff_ffff) as i64),
            content_hash,
        }
    }
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
    if let Err(error) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    if let Err(error) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    if previous != count {
        crate::events::emit(
            home,
            None,
            "capacity.changed",
            None,
            serde_json::json!({"budget":"max-agents", "previous":previous, "total":count}),
        );
    }
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

struct ConfigLock {
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

impl ConfigLock {
    fn acquire(home: &Path) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(home.join("config.json.lock"))
            .map_err(|e| e.to_string())?;
        let lock: &'static mut fd_lock::RwLock<std::fs::File> =
            Box::leak(Box::new(fd_lock::RwLock::new(file)));
        match lock.write() {
            Ok(guard) => Ok(Self { _guard: guard }),
            Err(error) => Err(error.to_string()),
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
    fn cached_budget_refreshes_after_atomic_update() {
        let dir =
            std::env::temp_dir().join(format!("pi-famulus-capacity-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut cache = MaxAgentsCache::default();
        assert_eq!(cache.get(&dir).unwrap(), 8);
        set_max_agents(&dir, 12).unwrap();
        assert_eq!(cache.get(&dir).unwrap(), 12);
        // Same-length in-place edits must not retain a stale budget.
        fs::write(dir.join("config.json"), r#"{"maxAgents":10}"#).unwrap();
        assert_eq!(cache.get(&dir).unwrap(), 10);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unchanged_budget_does_not_emit_a_capacity_change() {
        let dir =
            std::env::temp_dir().join(format!("pi-famulus-capacity-event-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        set_max_agents(&dir, 12).unwrap();
        set_max_agents(&dir, 12).unwrap();
        let events = fs::read_to_string(crate::events::daemon_events_path(&dir)).unwrap();
        assert_eq!(events.lines().count(), 1, "{events}");
        set_max_agents(&dir, 13).unwrap();
        let events = fs::read_to_string(crate::events::daemon_events_path(&dir)).unwrap();
        assert_eq!(events.lines().count(), 2, "{events}");
        set_max_kind(&dir, "test", DEFAULT_MAX_TEST).unwrap();
        let events = fs::read_to_string(crate::events::daemon_events_path(&dir)).unwrap();
        assert_eq!(events.lines().count(), 2, "same effective kind budget emitted: {events}");
        set_max_kind(&dir, "test", 3).unwrap();
        set_max_kind(&dir, "test", 3).unwrap();
        let events = fs::read_to_string(crate::events::daemon_events_path(&dir)).unwrap();
        assert_eq!(events.lines().count(), 3, "kind change or idempotent update emitted incorrectly: {events}");
        assert!(events.contains(r#""budget":"max-test""#), "{events}");
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
            assert!(
                set_max_kind(&dir, "test", 12).is_err(),
                "kind set rewrote {corrupt}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
