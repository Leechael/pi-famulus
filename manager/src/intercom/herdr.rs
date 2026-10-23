use super::{config, str_field, Runtime};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return path.into();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".config"))
        .join("herdr");
    match std::env::var("HERDR_SESSION") {
        Ok(name) if !name.is_empty() && name != "default" => {
            base.join("sessions").join(name).join("herdr.sock")
        }
        _ => base.join("herdr.sock"),
    }
}
pub async fn call(method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
    #[cfg(unix)]
    {
        let mut stream = tokio::net::UnixStream::connect(socket_path())
            .await
            .map_err(|e| format!("herdr not available: {e}"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut line = json!({"id":id,"method":method,"params":params}).to_string();
        line.push('\n');
        stream
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream).take((1 << 20) + 1);
        let mut line = String::new();
        tokio::time::timeout(timeout, reader.read_line(&mut line))
            .await
            .map_err(|_| "Herdr timed out".to_string())?
            .map_err(|e| e.to_string())?;
        if line.len() > (1 << 20) {
            return Err("Herdr response exceeds frame limit".into());
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(format!(
                "herdr {}: {}",
                str_field(&v["error"], "code"),
                str_field(&v["error"], "message")
            ));
        }
        if v["id"] != id || v.get("result").is_none() {
            return Err("invalid Herdr response".into());
        }
        Ok(v["result"].clone())
    }
    #[cfg(not(unix))]
    {
        let _ = (method, params, timeout);
        Err("Herdr native IPC supports Linux/macOS only".into())
    }
}
use tokio::io::AsyncReadExt;
async fn quick(method: &str, params: Value) -> Result<Value, String> {
    call(method, params, Duration::from_secs(8)).await
}
async fn panes() -> Result<Vec<Value>, String> {
    let result = quick("pane.list", json!({})).await?;
    Ok(result["panes"].as_array().cloned().unwrap_or_default())
}
fn native_ids(pane: &Value) -> Vec<String> {
    let s = &pane["agent_session"];
    let mut value = str_field(s, "value").to_string();
    if value.is_empty() {
        return vec![];
    }
    if str_field(s, "kind") == "path" {
        value = PathBuf::from(&value)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if str_field(s, "agent") == "codex" {
            value = value
                .get(value.len().saturating_sub(36)..)
                .unwrap_or("")
                .to_string();
            if uuid::Uuid::parse_str(&value).is_err() {
                return vec![];
            }
        } else if let Some((_, suffix)) = value.rsplit_once('_') {
            value = suffix.to_string();
        }
    }
    match str_field(s, "agent") {
        "pi" => vec![format!("pi/pi-{value}"), format!("pi/{value}")],
        "claude" => vec![format!("claude/{value}")],
        "codex" => vec![format!("codex/{}", value.trim_start_matches("codex/"))],
        _ => vec![],
    }
}
pub async fn overlay(rt: &Runtime, rows: &mut Vec<Value>) {
    let Ok(panes) = panes().await else { return };
    let local = |row: &Value| {
        let machine = str_field(row, "machine_id");
        machine.is_empty() || machine == rt.machine
    };
    let mut used = HashSet::new();
    let mut bound_panes = HashSet::new();
    let mut claims = HashMap::<String, Vec<usize>>::new();
    for (i, row) in rows.iter().enumerate().filter(|(_, row)| local(row)) {
        let pane = str_field(row, "herdr_pane_id");
        if !pane.is_empty() {
            claims.entry(pane.into()).or_default().push(i);
        }
    }
    for pane in &panes {
        let pid = str_field(pane, "pane_id");
        let candidates = if let Some(indices) = claims.get(pid).filter(|v| v.len() == 1) {
            indices.clone()
        } else {
            let ids = native_ids(pane);
            rows.iter()
                .enumerate()
                .filter(|(i, r)| {
                    local(r) && !used.contains(i) && ids.iter().any(|id| id == str_field(r, "id"))
                })
                .map(|(i, _)| i)
                .collect()
        };
        if candidates.len() == 1 {
            let index = candidates[0];
            if used.insert(index) {
                bound_panes.insert(pid.to_string());
                rows[index]["herdr_pane_id"] = pane["pane_id"].clone();
                rows[index]["herdr_workspace_id"] = pane["workspace_id"].clone();
                rows[index]["herdr_status"] = pane["agent_status"].clone();
            }
        }
    }
    for pane in &panes {
        if str_field(pane, "agent") != "claude" || !pane["agent_session"].is_null() {
            continue;
        }
        let candidates = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                local(r)
                    && !used.contains(i)
                    && str_field(r, "harness") == "claude"
                    && r["pid"].as_u64().unwrap_or(0) > 0
            })
            .map(|(i, r)| (i, r["pid"].as_u64().unwrap()))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            break;
        }
        if let Ok(result) = quick("pane.process_info", json!({"target":pane["pane_id"]})).await {
            for process in result["process_info"]["foreground_processes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                for (index, pid) in &candidates {
                    if process["pid"].as_u64() == Some(*pid) && used.insert(*index) {
                        bound_panes.insert(str_field(pane, "pane_id").to_string());
                        rows[*index]["herdr_pane_id"] = pane["pane_id"].clone();
                        rows[*index]["herdr_workspace_id"] = pane["workspace_id"].clone();
                        rows[*index]["herdr_status"] = pane["agent_status"].clone();
                    }
                }
            }
        }
    }
    for row in rows.iter_mut().filter(|row| local(row)) {
        let pane_id = str_field(row, "herdr_pane_id");
        if str_field(row, "harness") != "pi" || pane_id.is_empty() {
            continue;
        }
        let Some(pane) = panes
            .iter()
            .find(|pane| str_field(pane, "pane_id") == pane_id)
        else {
            row["status"] = json!("offline");
            continue;
        };
        let current = native_ids(pane);
        if current.is_empty() {
            continue;
        }
        if current.iter().any(|id| id == str_field(row, "id")) {
            if str_field(row, "herdr_status").is_empty() {
                row["herdr_status"] = pane["agent_status"].clone();
                row["herdr_workspace_id"] = pane["workspace_id"].clone();
            }
        } else {
            row["status"] = json!("offline");
        }
    }
    let mut seen = rows
        .iter()
        .filter(|row| local(row))
        .map(|row| str_field(row, "id").to_string())
        .collect::<HashSet<_>>();
    for pane in &panes {
        let agent = str_field(pane, "agent");
        let pane_id = str_field(pane, "pane_id");
        if agent.is_empty() || bound_panes.contains(pane_id) {
            continue;
        }
        let cwd = if str_field(pane, "cwd").is_empty() {
            str_field(pane, "foreground_cwd")
        } else {
            str_field(pane, "cwd")
        };
        if agent == "codex" {
            let Some(id) = native_ids(pane)
                .into_iter()
                .find(|id| id.starts_with("codex/"))
            else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            rows.push(json!({
                "id":id,"harness":"codex","status":"offline","cwd":cwd,
                "machine_id":rt.machine,"machine_name":rt.cfg.name,
                "herdr_pane_id":pane_id,"herdr_workspace_id":pane["workspace_id"],
                "herdr_status":pane["agent_status"]
            }));
            continue;
        }
        let name = if str_field(pane, "label").is_empty() {
            agent
        } else {
            str_field(pane, "label")
        };
        rows.push(json!({
            "id":pane_id,"name":name,"harness":agent,"cwd":cwd,
            "machine_id":rt.machine,"machine_name":rt.cfg.name,
            "herdr_pane_id":pane_id,"herdr_workspace_id":pane["workspace_id"],
            "herdr_status":pane["agent_status"],"all":true
        }));
    }
}
async fn require(rt: &Runtime) -> Result<(), String> {
    quick("ping", json!({}))
        .await
        .map(|_| ())
        .map_err(|_| format!("herdr not available on {}", rt.machine))
}
async fn pane_for(rt: &Runtime, token: &str) -> Result<String, String> {
    match rt.resolve(token).await {
        Ok(row) if !str_field(&row, "herdr_pane_id").is_empty() => {
            Ok(str_field(&row, "herdr_pane_id").into())
        }
        Ok(row) => Err(format!(
            "session {} is not in a herdr pane",
            str_field(&row, "id")
        )),
        Err(error) => {
            if (token.contains(":p") || token.contains(":t"))
                && panes()
                    .await?
                    .iter()
                    .any(|p| str_field(p, "pane_id") == token)
            {
                return Ok(token.into());
            }
            Err(error)
        }
    }
}
async fn start_agent(rt: &Runtime, req: &Value, pane: &str) -> Result<String, String> {
    quick("agent.start",json!({"name":req["name"],"kind":req["agent"],"pane_id":pane,"args":req["args"].as_array().cloned().unwrap_or_default()})).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        if let Some(row) = rt
            .directory()
            .await
            .iter()
            .find(|r| str_field(r, "herdr_pane_id") == pane)
        {
            return Ok(str_field(row, "id").into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(pane.into())
}
async fn wait_exited(pane: &str, seconds: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        match quick("pane.process_info", json!({"target":pane})).await {
            Ok(r)
                if r["process_info"]["foreground_processes"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()) => {}
            _ => return true,
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
pub async fn dispatch(rt: &Runtime, req: &Value) -> Result<Value, String> {
    let op = str_field(req, "op");
    let machine = str_field(req, "machine");
    let target = str_field(req, "to");
    let resolved = if !target.is_empty() && !["workspace_close"].contains(&op) {
        rt.resolve(target).await.ok()
    } else {
        None
    };
    let remote = if !machine.is_empty() {
        machine
    } else {
        resolved
            .as_ref()
            .map(|r| str_field(r, "machine_id"))
            .unwrap_or("")
    };
    if !remote.is_empty() && remote != rt.machine {
        if rt.cfg.remote_messages == "refuse" {
            return Err("cross-machine operation refused: remote_messages=refuse".into());
        }
        let mut forwarded = req.clone();
        forwarded["type"] = json!(op);
        forwarded["machine_id"] = json!(remote);
        forwarded["session_id"] = json!(target);
        forwarded["workspace_id"] = if op == "workspace_close" {
            json!(target)
        } else {
            req["workspace"].clone()
        };
        return super::relay::call(rt, forwarded).await;
    }
    require(rt).await?;
    match op {
        "workspace_list" => {
            let listed = quick("workspace.list", json!({})).await?;
            let panes = panes().await.unwrap_or_default();
            let workspaces = listed["workspaces"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|w| {
                    let id = str_field(w, "workspace_id");
                    let cwd = panes
                        .iter()
                        .find(|p| {
                            str_field(p, "workspace_id") == id && !str_field(p, "cwd").is_empty()
                        })
                        .map(|p| str_field(p, "cwd"))
                        .unwrap_or("");
                    json!({"id":id,"name":w["label"],"cwd":cwd})
                })
                .collect::<Vec<_>>();
            Ok(json!({"workspaces":workspaces}))
        }
        "workspace_create" | "session_create" => {
            let name = str_field(req, "name");
            let cwd = str_field(req, "cwd");
            let agent = str_field(req, "agent");
            if name.is_empty() || cwd.is_empty() {
                return Err("workspace/session add requires --name and --cwd".into());
            }
            if !(["pi", "claude"].contains(&agent) || op == "workspace_create" && agent.is_empty())
            {
                return Err("--agent must be pi or claude".into());
            }
            let workspace = str_field(req, "workspace").to_string();
            let workspace = if op == "session_create" && workspace.is_empty() {
                panes()
                    .await?
                    .iter()
                    .find(|p| str_field(p, "cwd") == cwd || str_field(p, "foreground_cwd") == cwd)
                    .map(|p| str_field(p, "workspace_id").to_string())
                    .unwrap_or_default()
            } else {
                workspace
            };
            let result = if workspace.is_empty() {
                quick("workspace.create", json!({"cwd":cwd,"label":name})).await?
            } else {
                quick(
                    "tab.create",
                    json!({"workspace_id":workspace,"cwd":cwd,"label":name}),
                )
                .await?
            };
            let pane = str_field(&result["root_pane"], "pane_id");
            if pane.is_empty() {
                return Err("herdr did not return a pane".into());
            }
            let session = if agent.is_empty() {
                String::new()
            } else {
                start_agent(rt, req, pane).await?
            };
            let mut out = json!({"pane_id":pane,"session_id":session});
            if op == "workspace_create" {
                let wid = if str_field(&result["root_pane"], "workspace_id").is_empty() {
                    str_field(&result["workspace"], "workspace_id")
                } else {
                    str_field(&result["root_pane"], "workspace_id")
                };
                out["workspaces"] = json!([{ "id":wid,"name":name,"cwd":cwd }]);
            }
            Ok(out)
        }
        "workspace_close" => {
            let id = if target.is_empty() {
                str_field(req, "workspace")
            } else {
                target
            };
            if id.is_empty() {
                return Err("workspace id required".into());
            }
            quick("workspace.close", json!({"workspace_id":id})).await?;
            Ok(json!({}))
        }
        "session_close" => {
            let pane = pane_for(rt, target).await?;
            let _ = quick("agent.send_keys", json!({"target":pane,"keys":["ctrl+c"]})).await;
            if !wait_exited(&pane, 10).await {
                let _ = quick("agent.send_keys", json!({"target":pane,"keys":["ctrl+d"]})).await;
                wait_exited(&pane, 5).await;
            }
            quick("pane.close", json!({"pane_id":pane})).await?;
            Ok(json!({}))
        }
        "agent_prompt" | "agent_keys" | "agent_read" | "agent_wait" => {
            let pane = pane_for(rt, target).await?;
            let timeout = req["timeout_ms"].as_u64().unwrap_or(0);
            let duration = Duration::from_millis(timeout.saturating_add(5000).max(60000));
            let (method, params) = match op {
                "agent_prompt" => {
                    let mut p = json!({"target":pane,"text":req["body"]});
                    if req["wait"].as_bool() == Some(true) {
                        let until = if str_field(req, "until").is_empty() {
                            "idle"
                        } else {
                            str_field(req, "until")
                        };
                        p["wait"] = json!({"until":[until]});
                        if timeout > 0 {
                            p["wait"]["timeout_ms"] = json!(timeout);
                        }
                    }
                    ("agent.prompt", p)
                }
                "agent_keys" => {
                    if req["keys"].as_array().is_none_or(|a| a.is_empty()) {
                        return Err("session keys requires at least one key".into());
                    }
                    ("agent.send_keys", json!({"target":pane,"keys":req["keys"]}))
                }
                "agent_read" => {
                    let source = if str_field(req, "source").is_empty() {
                        "recent"
                    } else {
                        str_field(req, "source")
                    };
                    let mut p =
                        json!({"target":pane,"source":source,"format":"text","strip_ansi":true});
                    if req["lines"].as_i64().unwrap_or(0) > 0 {
                        p["lines"] = req["lines"].clone();
                    }
                    ("agent.read", p)
                }
                _ => {
                    let until = str_field(req, "until");
                    if until.is_empty() {
                        return Err("session wait requires --until idle|blocked".into());
                    }
                    let mut p = json!({"target":pane,"until":[until]});
                    if timeout > 0 {
                        p["timeout_ms"] = json!(timeout);
                    }
                    ("agent.wait", p)
                }
            };
            let result = call(method, params, duration).await?;
            Ok(if op == "agent_read" {
                json!({"text":result["read"]["text"]})
            } else {
                json!({})
            })
        }
        _ => Err(format!("unknown Herdr operation {op}")),
    }
}
