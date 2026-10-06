//! Machine-wide agent-slot budget. CPU tokens and per-provider in-flight
//! limits are deferred: add them when resource-aware scheduling lands.
use serde_json::{Map, Value};
use std::{fs, path::Path};

pub const DEFAULT_MAX_AGENTS: usize = 8;

pub fn config(path: &Path) -> Result<Value, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn max_agents(home: &Path) -> usize {
    config(&home.join("config.json")).ok()
        .and_then(|v| v.get("maxAgents").and_then(Value::as_u64))
        .and_then(|n| usize::try_from(n).ok()).filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_AGENTS)
}

pub fn set_max_agents(home: &Path, count: usize) -> Result<(), String> {
    if count == 0 { return Err("max-agents must be at least 1".into()); }
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let path = home.join("config.json");
    let mut value = config(&path)?;
    let object = value.as_object_mut().ok_or("config.json must contain a JSON object")?;
    object.insert("maxAgents".into(), Value::from(count as u64));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    let tmp = home.join("config.json.tmp");
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_runtime_persisted_update() {
        let dir = std::env::temp_dir().join(format!("pi-famulus-capacity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(max_agents(&dir), 8);
        set_max_agents(&dir, 12).unwrap();
        assert_eq!(max_agents(&dir), 12);
        assert_eq!(config(&dir.join("config.json")).unwrap()["maxAgents"], 12);
        fs::remove_dir_all(dir).unwrap();
    }
}
