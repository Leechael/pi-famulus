//! Live conversation for one hosted subagent inside `pi-famulus top`.
//!
//! One daemon connection owns both directions: `get_entries` fills the
//! transcript, later records append to it, and typed lines go out as
//! `agent_input` with `source: "user"`.

use crate::client::{self, HelloMode};
use crate::inspect::TranscriptTurn;
use crate::proto::{self, read_frame, write_frame, Request, RequestKind, PROTO_VERSION};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum LiveCmd {
    /// `prompt`, `prompt_steer`, `prompt_follow`, or `abort` (clear the queue, then abort).
    Send { op: String, text: String },
    Stop,
}

#[derive(Debug)]
pub enum LiveMsg {
    Turns(Vec<TranscriptTurn>),
    /// Append to the in-progress assistant line.
    Delta(String),
    /// Replace the in-progress assistant line with the finished text.
    Assistant(String),
    User(String),
    State {
        activity: String,
        residency: String,
        cache: String,
    },
    Meta { model: String, context: String },
    /// Text pulled out of the steer / follow-up queues by Esc Esc.
    Restored(String),
    Error(String),
    /// The watch connection ended. The composer must stop trusting the live transcript.
    Disconnected(String),
}

pub async fn run(home: PathBuf, child_id: String, tx: mpsc::UnboundedSender<LiveMsg>, mut cmds: mpsc::UnboundedReceiver<LiveCmd>) {
    if let Err(e) = session(home, &child_id, &tx, &mut cmds).await {
        let _ = tx.send(LiveMsg::Disconnected(e));
    }
}

async fn session(
    home: PathBuf,
    child_id: &str,
    tx: &mpsc::UnboundedSender<LiveMsg>,
    cmds: &mut mpsc::UnboundedReceiver<LiveCmd>,
) -> Result<(), String> {
    let mut conn = tokio::time::timeout(Duration::from_secs(5), client::connect_existing(&home, &HelloMode::Cli))
        .await
        .map_err(|_| "pi-famulus did not answer".to_string())?
        .map_err(|_| "pi-famulus is not running".to_string())?;
    let _: serde_json::Value = conn
        .roundtrip(RequestKind::AgentWatch { child_id: child_id.to_string() })
        .await?;
    let (mut rd, mut wr) = conn.into_halves();
    let (frame_tx, mut frames) = mpsc::unbounded_channel();
    let _reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            match read_frame(&mut rd).await {
                Ok(Some(bytes)) => {
                    if frame_tx.send(Ok(bytes)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = frame_tx.send(Err("manager closed the connection".to_string()));
                    break;
                }
                Err(e) => {
                    let _ = frame_tx.send(Err(format!("recv: {e}")));
                    break;
                }
            }
        }
    }));
    let mut bootstrap = vec!["get_session_stats", "get_state", "get_entries"];
    let mut waiting: Option<(String, String)> = None;
    let mut abort_after_clear = false;
    let mut queued: VecDeque<LiveCmd> = VecDeque::new();
    loop {
        if waiting.is_none() {
            if abort_after_clear {
                abort_after_clear = false;
                waiting = Some(send_input(&mut wr, child_id, "abort", "").await?);
                continue;
            }
            if let Some(op) = bootstrap.pop() {
                waiting = Some(send_input(&mut wr, child_id, op, "").await?);
                continue;
            }
            if let Some(cmd) = queued.pop_front() {
                match cmd {
                    LiveCmd::Stop => return Ok(()),
                    LiveCmd::Send { op, text } if op == "abort" => {
                        abort_after_clear = true;
                        waiting = Some(send_input(&mut wr, child_id, "clear_queue", "").await?);
                        let _ = text;
                    }
                    LiveCmd::Send { op, text } => {
                        waiting = Some(send_input(&mut wr, child_id, &op, &text).await?);
                    }
                }
                continue;
            }
        }
        tokio::select! {
            cmd = cmds.recv() => {
                match cmd {
                    None | Some(LiveCmd::Stop) => return Ok(()),
                    Some(cmd) if waiting.is_some() => queued.push_back(cmd),
                    Some(LiveCmd::Send { op, text }) if op == "abort" => {
                        waiting = Some(send_input(&mut wr, child_id, "clear_queue", "").await?);
                        abort_after_clear = true;
                        let _ = text;
                    }
                    Some(LiveCmd::Send { op, text }) => {
                        waiting = Some(send_input(&mut wr, child_id, &op, &text).await?);
                    }
                }
            }
            frame = frames.recv() => {
                let bytes = frame.ok_or_else(|| "manager closed the connection".to_string())??;
                let value: Value = serde_json::from_slice(&bytes).map_err(|e| format!("bad JSON: {e}"))?;
                if let Some((id, op)) = waiting.clone() {
                    if value.get("id").and_then(|v| v.as_str()) == Some(id.as_str()) {
                        waiting = None;
                        if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                            let _ = tx.send(LiveMsg::Error(value["error"]["message"].as_str().unwrap_or("request failed").to_string()));
                        } else {
                            apply_response(tx, &op, &value);
                        }
                        continue;
                    }
                }
                apply_event(tx, &value);
            }
        }
    }
}

async fn send_input(
    wr: &mut tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>,
    child_id: &str,
    op: &str,
    text: &str,
) -> Result<(String, String), String> {
    let id = proto::new_request_id();
    let req = Request {
        v: Some(PROTO_VERSION),
        id: id.clone(),
        kind: RequestKind::AgentInput {
            child_id: child_id.to_string(),
            op: op.to_string(),
            text: text.to_string(),
            source: "user".into(),
            since: None,
        },
    };
    write_frame(wr, &proto::encode(&req)).await.map_err(|e| format!("send: {e}"))?;
    Ok((id, op.to_string()))
}

fn apply_response(tx: &mpsc::UnboundedSender<LiveMsg>, op: &str, value: &Value) {
    match op {
        "get_entries" => {
            let _ = tx.send(LiveMsg::Turns(turns_from_entries(&value["data"]["entries"])));
        }
        "get_state" => {
            let model = value["data"]["model"].as_object().map(|m| {
                format!(
                    "{}/{}",
                    m.get("provider").and_then(|v| v.as_str()).unwrap_or("?"),
                    m.get("id").and_then(|v| v.as_str()).unwrap_or("?")
                )
            });
            if let Some(model) = model {
                let _ = tx.send(LiveMsg::Meta { model, context: String::new() });
            }
            let activity = if value["data"]["isStreaming"].as_bool() == Some(true) { "running" } else { "idle" };
            let _ = tx.send(LiveMsg::State { activity: activity.into(), residency: String::new(), cache: String::new() });
        }
        "get_session_stats" => {
            let pct = value["data"]["contextUsage"]["percent"].as_f64().or_else(|| {
                value["data"]["contextUsage"]["percent"].as_u64().map(|n| n as f64)
            });
            let context = pct.map(|p| format!("ctx {p:.0}%")).unwrap_or_default();
            let _ = tx.send(LiveMsg::Meta { model: String::new(), context });
        }
        "clear_queue" => {
            let steering = texts(&value["data"]["steering"]);
            let follow = texts(&value["data"]["followUp"]);
            let mut parts = steering;
            parts.extend(follow);
            let _ = tx.send(LiveMsg::Restored(parts.join("\n")));
        }
        _ => {}
    }
}

fn apply_event(tx: &mpsc::UnboundedSender<LiveMsg>, value: &Value) {
    match value.get("event").and_then(|v| v.as_str()) {
        Some("agent_state") => {
            let mut cache = cache_label(value.get("cache_expires_at").and_then(|v| v.as_u64()));
            if let Some(ms) = value.get("cache_lifetime_ms").and_then(|v| v.as_u64()) {
                cache = format!("{cache} · {}s", ms / 1000);
            }
            if let Some(version) = value.get("pi_version").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                cache = format!("{cache} · pi {version}");
            }
            let _ = tx.send(LiveMsg::State {
                activity: value.get("activity").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                residency: value.get("residency").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                cache,
            });
        }
        Some("agent_rpc") => apply_record(tx, &value["record"]),
        Some("agent_ui") => {
            let _ = tx.send(LiveMsg::Error(format!("dialog: {}", value["request"])));
        }
        _ => {}
    }
}

fn apply_record(tx: &mpsc::UnboundedSender<LiveMsg>, record: &Value) {
    let kind = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "agent_start" => {
            let _ = tx.send(LiveMsg::State { activity: "running".into(), residency: String::new(), cache: String::new() });
        }
        "agent_settled" => {
            let _ = tx.send(LiveMsg::State { activity: "idle".into(), residency: String::new(), cache: String::new() });
        }
        "message_update" => {
            let event = record.get("assistantMessageEvent");
            let event_type = event.and_then(|e| e.get("type")).and_then(|t| t.as_str());
            if matches!(event_type, Some(kind) if kind != "text_delta") {
                return;
            }
            let delta = event
                .and_then(|e| e.get("delta"))
                .and_then(|d| d.as_str())
                .or_else(|| record.get("delta").and_then(|d| d.as_str()));
            if let Some(delta) = delta {
                if !delta.is_empty() {
                    let _ = tx.send(LiveMsg::Delta(delta.to_string()));
                }
            }
        }
        "message_end" => {
            if let Some(text) = message_text(&record["message"]) {
                match record["message"]["role"].as_str() {
                    Some("assistant") => { let _ = tx.send(LiveMsg::Assistant(text)); }
                    Some("user") => { let _ = tx.send(LiveMsg::User(text)); }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

pub fn turns_from_entries(entries: &Value) -> Vec<TranscriptTurn> {
    let Some(list) = entries.as_array() else { return Vec::new() };
    let mut turns = Vec::new();
    for entry in list {
        let message = entry.get("message").unwrap_or(entry);
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let Some(text) = message_text(message) else { continue };
        if text.is_empty() {
            continue;
        }
        let turn_role = match role {
            "user" => "user",
            "assistant" => "assistant",
            "toolResult" | "tool" => "tool",
            _ => continue,
        };
        turns.push(TranscriptTurn {
            role: turn_role.into(),
            tool: message.get("toolName").or_else(|| message.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string()),
            args: None,
            text,
            ts: None,
            error: message.get("isError").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }
    turns
}

fn message_text(message: &Value) -> Option<String> {
    if let Some(text) = message.get("text").and_then(|t| t.as_str()) {
        return Some(text.to_string());
    }
    let content = message.get("content")?.as_array()?;
    let text: String = content.iter().filter_map(|c| c.get("text").and_then(|t| t.as_str())).collect();
    Some(text)
}

fn texts(value: &Value) -> Vec<String> {
    value.as_array().map(|items| items.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn cache_label(expires_at: Option<u64>) -> String {
    let Some(at) = expires_at else { return "cache ?".into() };
    let now = proto::now_ms();
    if at <= now {
        return "cache expired".into();
    }
    let secs = (at - now) / 1000;
    if secs >= 60 {
        format!("cache {}m", secs / 60)
    } else {
        format!("cache {secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entries_become_user_and_assistant_turns() {
        let entries = json!([
            {"type":"message","message":{"role":"user","content":[{"type":"text","text":"hi"}]}},
            {"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"pong"}]}},
            {"type":"message","message":{"role":"toolResult","toolName":"bash","content":[{"type":"text","text":"ok"}]}}
        ]);
        let turns = turns_from_entries(&entries);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "hi");
        assert_eq!(turns[1].text, "pong");
        assert_eq!(turns[2].role, "tool");
        assert_eq!(turns[2].tool.as_deref(), Some("bash"));
    }

    #[test]
    fn cache_label_counts_down_and_expires() {
        assert_eq!(cache_label(None), "cache ?");
        assert_eq!(cache_label(Some(0)), "cache expired");
        let soon = proto::now_ms() + 90_000;
        assert_eq!(cache_label(Some(soon)), "cache 1m");
    }
}
