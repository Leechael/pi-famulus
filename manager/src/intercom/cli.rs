use super::config::{Config, Identity};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::PathBuf;

#[derive(usage::Args)]
pub struct Options {
    #[usage(long, global)]
    pub config: Option<PathBuf>,
    #[usage(long, global)]
    pub sock: Option<PathBuf>,
    #[usage(long, global)]
    pub state_dir: Option<PathBuf>,
    #[usage(long, global)]
    pub server: Option<String>,
    #[usage(long, global)]
    pub token: Option<String>,
    #[usage(long, global)]
    pub name: Option<String>,
    #[usage(long, global)]
    pub mcp_listen: Option<String>,
    #[usage(long, global)]
    pub mcp_token: Option<String>,
    #[usage(long, global)]
    pub remote_messages: Option<String>,
    #[usage(long, global)]
    pub local_only: bool,
    #[usage(long, global)]
    pub insecure: bool,
    #[usage(long, global)]
    pub save_token: bool,
}

impl Options {
    /// No command-line overrides: the configuration file and environment only.
    pub fn from_environment() -> Self {
        Self {
            config: None,
            sock: None,
            state_dir: None,
            server: None,
            token: None,
            name: None,
            mcp_listen: None,
            mcp_token: None,
            remote_messages: None,
            local_only: false,
            insecure: false,
            save_token: false,
        }
    }
}

#[derive(usage::Subcommands)]
pub enum Command {
    /// Read command help or the offline topics: exit-codes, herdr, idle.
    Help {
        path: Vec<String>,
    },
    /// Legacy launch agents run `intercom --config C serve`; it now means
    /// `pi-famulus serve` with C as the bridge configuration.
    #[usage(hide)]
    Serve,
    /// Reachable local and remote agent sessions.
    Agents {
        query: Option<String>,
        #[usage(long)]
        json: bool,
        #[usage(long)]
        offline: bool,
        #[usage(long)]
        harness: Option<String>,
        #[usage(long)]
        machine: Option<String>,
        #[usage(long)]
        cwd: Option<String>,
    },
    /// Send text, or read it from stdin when omitted or '-'.
    Send {
        to: String,
        message: Vec<String>,
        #[usage(long = "as")]
        origin: Option<String>,
        #[usage(long)]
        reply_to: Option<String>,
    },
    Status {
        #[usage(long)]
        json: bool,
        #[usage(long)]
        machine: Option<String>,
    },
    NotifyIdle {
        to: String,
        #[usage(long = "from")]
        from: Option<String>,
    },
    IdleStatus {
        subscription_id: String,
    },
    Enroll {
        #[usage(long)]
        new_identity: bool,
    },
    Device {
        #[usage(long)]
        json: bool,
    },
    Session {
        #[usage(subcommand)]
        action: SessionCommand,
    },
    Workspace {
        #[usage(subcommand)]
        action: WorkspaceCommand,
    },
    Listen {
        #[usage(long)]
        endpoint: String,
        #[usage(long)]
        forward_to: String,
        #[usage(long)]
        secret: Option<String>,
    },
    Inbox {
        message_id: Option<String>,
        #[usage(long = "as")]
        origin: String,
        #[usage(long, default = "20")]
        limit: u32,
        #[usage(long)]
        cursor: Option<String>,
    },
    Events {
        #[usage(long, default = "0")]
        since: u64,
        #[usage(long)]
        follow: bool,
    },
    Message {
        id: String,
    },
    Codex {
        #[usage(subcommand)]
        action: CodexCommand,
    },
    Service {
        #[usage(subcommand)]
        action: ServiceCommand,
    },
}
#[derive(usage::Subcommands)]
pub enum SessionCommand {
    Registrations {
        #[usage(long)]
        all: bool,
        #[usage(long)]
        json: bool,
    },
    Unpublish {
        ids: Vec<String>,
    },
    Restore {
        ids: Vec<String>,
    },
    #[usage(alias = "ls")]
    List {
        query: Option<String>,
        #[usage(long)]
        json: bool,
        #[usage(long)]
        agent: Option<String>,
        #[usage(long)]
        machine: Option<String>,
    },
    Add {
        #[usage(long)]
        name: String,
        #[usage(long)]
        cwd: String,
        #[usage(long)]
        agent: String,
        #[usage(long)]
        workspace: Option<String>,
        #[usage(long)]
        machine: Option<String>,
        args: Vec<String>,
    },
    Rm {
        id: String,
    },
    Prompt {
        id: String,
        text: Vec<String>,
        #[usage(long)]
        wait: bool,
        #[usage(long)]
        until: Option<String>,
        #[usage(long)]
        timeout: Option<String>,
    },
    Keys {
        id: String,
        keys: Vec<String>,
    },
    Read {
        id: String,
        #[usage(long)]
        lines: Option<u32>,
        #[usage(long)]
        source: Option<String>,
    },
    Wait {
        id: String,
        #[usage(long)]
        until: String,
        #[usage(long)]
        timeout: Option<String>,
    },
}
#[derive(usage::Subcommands)]
pub enum WorkspaceCommand {
    #[usage(alias = "ls")]
    List {
        #[usage(long)]
        json: bool,
        #[usage(long)]
        machine: Option<String>,
    },
    Add {
        #[usage(long)]
        name: String,
        #[usage(long)]
        cwd: String,
        #[usage(long)]
        agent: Option<String>,
        #[usage(long)]
        machine: Option<String>,
        args: Vec<String>,
    },
    Rm {
        id: String,
        #[usage(long)]
        machine: Option<String>,
    },
}
#[derive(usage::Subcommands)]
pub enum CodexCommand {
    Hook,
    Setup {
        #[usage(long)]
        force: bool,
    },
}
#[derive(usage::Subcommands)]
pub enum ServiceCommand {
    Print,
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
    Logs {
        #[usage(long)]
        follow: bool,
    },
}

pub async fn run(home: PathBuf, options: Options, action: Command) -> Result<(), String> {
    if let Command::Help { path } = &action {
        crate::out::bytes(help_text(path)?.as_bytes());
        return Ok(());
    }
    if matches!(action, Command::Serve) {
        if let Some(path) = &options.config {
            // The manager `serve` spawns inherits this as its configuration.
            std::env::set_var("AGENT_INTERCOM_CONFIG", path);
        }
        return crate::client::cmd_serve(&home).await;
    }
    let cfg = Config::load(&options)?;
    let (as_json, collection) = match &action {
        Command::Status { json, .. } => (*json, "report"),
        Command::Session {
            action: SessionCommand::Registrations { json, .. },
        } => (*json, "sessions"),
        Command::Session {
            action: SessionCommand::List { json, .. },
        } => (*json, "sessions"),
        Command::Workspace {
            action: WorkspaceCommand::List { json, .. },
        } => (*json, "workspaces"),
        _ => (false, ""),
    };
    let pane_filter = match &action {
        Command::Session {
            action:
                SessionCommand::List {
                    query,
                    agent,
                    machine,
                    ..
                },
        } => Some((query.clone(), agent.clone(), machine.clone())),
        _ => None,
    };
    let req = match action {
        Command::Agents {
            query,
            json: as_json,
            offline,
            harness,
            machine,
            cwd,
        } => {
            let r = list_sessions(&cfg, true).await?;
            let rows: Vec<Value> = r["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| {
                    !s["all"].as_bool().unwrap_or(false)
                        && (offline
                            || !matches!(s["status"].as_str(), Some("offline" | "notLoaded")))
                        && session_matches(
                            s,
                            query.as_deref(),
                            harness.as_deref(),
                            machine.as_deref(),
                            cwd.as_deref(),
                        )
                })
                .cloned()
                .collect();
            if as_json {
                print_json(&json!(rows));
            } else {
                for s in rows {
                    crate::out::line(&format!(
                        "{}\t{}\t{}\t{}",
                        text(&s, "id"),
                        text(&s, "name"),
                        text(&s, "status"),
                        text(&s, "cwd")
                    ));
                }
            }
            return Ok(());
        }
        Command::Send {
            to,
            message,
            origin,
            reply_to,
        } => {
            let origin = origin.filter(|value| !value.is_empty());
            let reply_to = reply_to.filter(|value| !value.is_empty());
            let body = if message.is_empty() || message == ["-"] {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .map_err(|e| e.to_string())?;
                s
            } else {
                message.join(" ")
            };
            if let Some(origin) = &origin {
                if !cfg.local_only
                    && !cfg
                        .endpoints
                        .iter()
                        .any(|ep| ep.name == origin.trim_start_matches("webhook/"))
                {
                    if reply_to.is_some() {
                        return Err("--as and --reply-to cannot be combined".into());
                    }
                    return super::relay::endpoint_send(&cfg, &to, &body, origin).await;
                }
            }
            let reply = reply_to.or_else(|| {
                if origin.is_some() {
                    return None;
                }
                codex_reply_to(std::env::var("CODEX_THREAD_ID").ok())
            });
            let to = resolve_target(&cfg, &to).await?;
            json!({"op":"send","to":to,"body":body,"as":origin,"reply_to":reply})
        }
        Command::Status { json: _, machine } => json!({"op":"status","machine":machine}),
        Command::NotifyIdle { to, from } => {
            let from = idle_watcher(from, std::env::var("CODEX_THREAD_ID").ok())?;
            json!({"op":"subscribe_idle","to":to,"as":from})
        }
        Command::IdleStatus { subscription_id } => {
            json!({"op":"idle_status","subscription_id":subscription_id})
        }
        Command::Device { json: _json } => {
            let identity = Identity::load(&cfg.state_dir)?;
            let mut v = serde_json::to_value(&identity).map_err(|e| e.to_string())?;
            v["credential"] = json!(if identity.credential.is_empty() {
                "unenrolled"
            } else {
                "enrolled"
            });
            crate::out::line(&serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?);
            return Ok(());
        }
        Command::Enroll { new_identity } => {
            super::relay::enroll(&cfg, new_identity).await?;
            return Ok(());
        }
        Command::Session { action } => match action {
            SessionCommand::Registrations { all, .. } => {
                json!({"op":"session_registrations","all":all})
            }
            SessionCommand::Unpublish { ids } => json!({"op":"session_unpublish","args":ids}),
            SessionCommand::Restore { ids } => json!({"op":"session_restore","args":ids}),
            SessionCommand::List { machine, .. } => {
                json!({"op":"list","all":true,"machine":machine})
            }
            SessionCommand::Add {
                name,
                cwd,
                agent,
                workspace,
                machine,
                args,
            } => {
                json!({"op":"session_create","name":name,"cwd":cwd,"agent":agent,"workspace":workspace,"machine":machine,"args":args})
            }
            SessionCommand::Rm { id } => json!({"op":"session_close","to":id}),
            SessionCommand::Prompt {
                id,
                text,
                wait,
                until,
                timeout,
            } => {
                json!({"op":"agent_prompt","to":id,"body":text.join(" "),"wait":wait || until.is_some(),"until":until,"timeout_ms":timeout_ms(timeout)?})
            }
            SessionCommand::Keys { id, keys } => json!({"op":"agent_keys","to":id,"keys":keys}),
            SessionCommand::Read { id, lines, source } => {
                json!({"op":"agent_read","to":id,"lines":lines,"source":source})
            }
            SessionCommand::Wait { id, until, timeout } => {
                json!({"op":"agent_wait","to":id,"until":until,"timeout_ms":timeout_ms(timeout)?})
            }
        },
        Command::Workspace { action } => match action {
            WorkspaceCommand::List { machine, .. } => {
                json!({"op":"workspace_list","machine":machine})
            }
            WorkspaceCommand::Add {
                name,
                cwd,
                agent,
                machine,
                args,
            } => {
                json!({"op":"workspace_create","name":name,"cwd":cwd,"agent":agent,"machine":machine,"args":args})
            }
            WorkspaceCommand::Rm { id, machine } => {
                json!({"op":"workspace_close","to":id,"machine":machine})
            }
        },
        Command::Listen {
            endpoint,
            forward_to,
            secret,
        } => {
            return super::relay::listen(cfg, endpoint, forward_to, secret.unwrap_or_default())
                .await
        }
        Command::Inbox {
            message_id,
            origin,
            limit,
            cursor,
        } => return super::relay::inbox(&cfg, message_id, origin, limit, cursor).await,
        Command::Events { since, follow } => {
            return super::http::inspect_events(&cfg, since, follow).await
        }
        Command::Message { id } => return super::http::inspect_message(&cfg, &id).await,
        Command::Codex { action } => return super::native::codex_helper(action).await,
        Command::Service { action } => {
            return super::service::run(&home, &cfg, &options, action).await
        }
        Command::Help { .. } | Command::Serve => unreachable!(),
    };
    let op = text(&req, "op").to_string();
    let r = match super::localrpc::call_raw(&cfg, req.clone()).await {
        Ok(reply) => {
            if !text(&reply, "error").is_empty() {
                if op == "send"
                    && text(&reply, "status") == "unknown"
                    && !text(&reply, "message_id").is_empty()
                {
                    crate::out::line(&format!(
                        "{} {}",
                        text(&reply, "message_id"),
                        text(&reply, "status")
                    ));
                }
                return Err(text(&reply, "error").into());
            }
            reply
        }
        Err(error) if op == "status" && text(&req, "machine").is_empty() && no_daemon(&error) => {
            let mut report = super::tool_status_report().await;
            let name = if cfg.name.is_empty() {
                std::env::var("USER").unwrap_or_else(|_| "user".into())
            } else {
                cfg.name.clone()
            };
            report["machine"] = json!(format!("{name}@{}", super::config::hostname()));
            report["daemon"] = json!("not running");
            report["socket"] = json!(cfg.sock);
            report["mcp_listen"] = json!(cfg.mcp_listen);
            json!({"report":report})
        }
        Err(error) if op == "send" => {
            direct_fallback(&cfg, &error)?;
            if !text(&req, "reply_to").is_empty() {
                return Err("--reply-to requires the local client daemon".into());
            }
            if cfg.remote_messages == "refuse" {
                return Err("cross-machine operation refused: remote_messages=refuse".into());
            }
            super::relay::call_once(
                &cfg,
                text(&req, "as"),
                json!({"type":"send","to":req["to"],"body":req["body"]}),
            )
            .await?;
            crate::out::line("ok");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if op == "send" {
        if !text(&r, "message_id").is_empty() || !text(&r, "status").is_empty() {
            crate::out::line(&format!(
                "{} {}",
                text(&r, "message_id"),
                text(&r, "status")
            ));
        } else {
            crate::out::line("ok");
        }
        if text(&req, "as").is_empty() && text(&req, "reply_to").is_empty() {
            eprintln!("note: no reply identity (--reply-to <session-id> or --as <endpoint>); the recipient cannot reply to this message");
        }
        return Ok(());
    }
    if matches!(op.as_str(), "subscribe_idle" | "idle_status") {
        let mut line = format!("{} {}", text(&r, "subscription_id"), text(&r, "status"));
        if !text(&r, "delivery_mode").is_empty() {
            line.push(' ');
            line.push_str(text(&r, "delivery_mode"));
        }
        crate::out::line(&line);
        return Ok(());
    }
    if matches!(op.as_str(), "session_create" | "workspace_create") {
        let key = if op == "session_create" {
            "session"
        } else {
            "workspace"
        };
        if let Some(id) = r[key]["id"].as_str() {
            crate::out::line(id);
            return Ok(());
        }
    }
    if matches!(
        op.as_str(),
        "session_close"
            | "workspace_close"
            | "agent_prompt"
            | "agent_keys"
            | "agent_wait"
            | "session_unpublish"
            | "session_restore"
    ) {
        crate::out::line("ok");
        return Ok(());
    }
    let mut v = if collection.is_empty() {
        r
    } else {
        r[collection].clone()
    };
    if let (Some((query, agent, machine)), Some(rows)) = (pane_filter, v.as_array_mut()) {
        rows.retain(|row| {
            !text(row, "herdr_pane_id").is_empty()
                && session_matches(
                    row,
                    query.as_deref(),
                    agent.as_deref(),
                    machine.as_deref(),
                    None,
                )
        });
    }
    if collection == "report" && !as_json {
        for tool in ["claude", "pi", "codex", "herdr"] {
            if text(&v, tool).is_empty() {
                v[tool] = json!("not installed");
            }
        }
    }
    if let Some(s) = v.get("text").and_then(Value::as_str) {
        crate::out::bytes(s.as_bytes());
    } else if as_json {
        print_json(&v);
    } else if let Some(rows) = v.as_array() {
        for row in rows {
            crate::out::line(&format!(
                "{}\t{}\t{}",
                text(row, "id"),
                text(row, "name"),
                text(row, "status")
            ));
        }
    } else if let Some(fields) = v.as_object() {
        for (key, value) in fields {
            crate::out::line(&format!(
                "{key}\t{}",
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            ));
        }
    }
    Ok(())
}
fn no_daemon(error: &str) -> bool {
    error.starts_with("client daemon is not running (")
}

fn direct_fallback(cfg: &Config, error: &str) -> Result<(), String> {
    if !no_daemon(error) || cfg.local_only {
        return Err(error.into());
    }
    if cfg.token.is_empty() {
        return Err(format!("{error}\nor set a relay token (config token, --token, or AGENT_INTERCOM_TOKEN) to reach the relay without a daemon"));
    }
    Ok(())
}

async fn list_sessions(cfg: &Config, all: bool) -> Result<Value, String> {
    match super::localrpc::call_raw(cfg, json!({"op":"list","all":all})).await {
        Ok(reply) => {
            if !text(&reply, "error").is_empty() {
                return Err(text(&reply, "error").into());
            }
            Ok(reply)
        }
        Err(error) => {
            direct_fallback(cfg, &error)?;
            super::relay::call_once(cfg, "", json!({"type":"list"})).await
        }
    }
}

async fn resolve_target(cfg: &Config, target: &str) -> Result<String, String> {
    let reply = match list_sessions(cfg, false).await {
        Ok(reply) => reply,
        Err(_) => return Ok(target.into()),
    };
    let rows: Vec<&Value> = reply["sessions"].as_array().into_iter().flatten().collect();
    if rows.iter().any(|row| text(row, "id") == target) {
        return Ok(target.into());
    }
    let hits: Vec<&str> = rows
        .iter()
        .filter(|row| text(row, "name") == target)
        .map(|row| text(row, "id"))
        .collect();
    match hits.as_slice() {
        [] => Ok(target.into()),
        [id] => Ok((*id).into()),
        _ => Err(format!("ambiguous target {target:?}: {}", hits.join(", "))),
    }
}

fn idle_watcher(explicit: Option<String>, codex: Option<String>) -> Result<String, String> {
    let from = explicit
        .or_else(|| codex_reply_to(codex))
        .unwrap_or_default();
    let from = from.trim();
    if from.is_empty() {
        Err("--from is required: pass your intercom session id (or set CODEX_THREAD_ID)".into())
    } else {
        Ok(from.into())
    }
}

fn session_matches(
    row: &Value,
    query: Option<&str>,
    harness: Option<&str>,
    machine: Option<&str>,
    cwd: Option<&str>,
) -> bool {
    query.is_none_or(|query| {
        query.is_empty()
            || ["id", "name", "machine_id", "machine_name", "cwd"]
                .iter()
                .any(|key| {
                    text(row, key)
                        .to_lowercase()
                        .contains(&query.to_lowercase())
                })
    }) && harness.is_none_or(|harness| {
        harness.is_empty() || text(row, "harness").eq_ignore_ascii_case(harness)
    }) && machine.is_none_or(|machine| {
        machine.is_empty()
            || text(row, "machine_id") == machine
            || text(row, "machine_name") == machine
            || text(row, "machine_id").starts_with(&format!("{machine}@"))
    }) && cwd.is_none_or(|cwd| text(row, "cwd").starts_with(cwd))
}

fn codex_reply_to(id: Option<String>) -> Option<String> {
    let id = id?;
    let id = id.trim();
    if id.is_empty() {
        None
    } else if id.starts_with("codex/") {
        Some(id.into())
    } else {
        Some(format!("codex/{id}"))
    }
}

fn timeout_ms(value: Option<String>) -> Result<i64, String> {
    let Some(value) = value else { return Ok(0) };
    let invalid = || format!("invalid duration {value:?}");
    let (negative, mut rest) = if let Some(rest) = value.strip_prefix('-') {
        (true, rest)
    } else {
        (false, value.strip_prefix('+').unwrap_or(&value))
    };
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err(invalid());
    }
    let limit = if negative {
        1_u128 << 63
    } else {
        i64::MAX as u128
    };
    let mut nanos = 0_u128;
    while !rest.is_empty() {
        let integer_end = rest.bytes().take_while(u8::is_ascii_digit).count();
        let whole = if integer_end == 0 {
            0
        } else {
            rest[..integer_end].parse::<u64>().map_err(|_| invalid())?
        };
        rest = &rest[integer_end..];
        let mut fraction = 0_u64;
        let mut scale = 1_f64;
        let mut fraction_digits = 0;
        if let Some(after_dot) = rest.strip_prefix('.') {
            fraction_digits = after_dot.bytes().take_while(u8::is_ascii_digit).count();
            let mut precision_lost = false;
            for digit in after_dot[..fraction_digits].bytes() {
                if precision_lost {
                    continue;
                }
                if fraction > (i64::MAX as u64) / 10 {
                    precision_lost = true;
                    continue;
                }
                let next = fraction * 10 + u64::from(digit - b'0');
                if next > 1_u64 << 63 {
                    precision_lost = true;
                    continue;
                }
                fraction = next;
                scale *= 10.0;
            }
            rest = &after_dot[fraction_digits..];
        }
        if integer_end == 0 && fraction_digits == 0 {
            return Err(invalid());
        }
        let (unit, multiplier) = [
            ("ns", 1_u64),
            ("us", 1_000),
            ("µs", 1_000),
            ("μs", 1_000),
            ("ms", 1_000_000),
            ("s", 1_000_000_000),
            ("m", 60_000_000_000),
            ("h", 3_600_000_000_000),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(*unit))
        .ok_or_else(invalid)?;
        let fractional_nanos = (fraction as f64 * (multiplier as f64 / scale)) as u128;
        nanos += u128::from(whole) * u128::from(multiplier) + fractional_nanos;
        if nanos > limit {
            return Err(invalid());
        }
        rest = &rest[unit.len()..];
    }
    let millis = (nanos / 1_000_000) as i64;
    Ok(if negative { -millis } else { millis })
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
pub fn print_json(v: &Value) {
    crate::out::line(&v.to_string());
}

fn help_text(path: &[String]) -> Result<String, String> {
    if let Some(topic) = path.first() {
        let text = match topic.to_ascii_lowercase().as_str() {
            "exit-codes" => Some(HELP_EXIT_CODES),
            "herdr" => Some(HELP_HERDR),
            "idle" => Some(HELP_IDLE),
            _ => None,
        };
        if let Some(text) = text {
            return Ok(text.replace("agent-intercom-client", "pi-famulus intercom"));
        }
    }
    let mut args = vec![OsStr::new("intercom")];
    args.extend(path.iter().map(|arg| OsStr::new(arg.as_str())));
    args.push(OsStr::new("--help"));
    match crate::cli::Cli::parse_from(&args) {
        Err(usage::Error::Help { cmd, long }) => {
            let mut text = crate::cli::Cli::render_help(cmd, long)
                .ok_or_else(|| "could not render intercom command help".to_string())?;
            if path.is_empty() {
                text.push_str("\nHelp topics: exit-codes, herdr, idle\n");
            }
            Ok(text)
        }
        _ => Err(format!(
            "unknown help topic {:?}; topics: exit-codes, herdr, idle",
            path.first().map(String::as_str).unwrap_or_default()
        )),
    }
}

const HELP_EXIT_CODES: &str = r#"# Exit Codes

0 means success and 1 means an operation failed. The Pi Famulus argument parser
uses exit 2 for usage errors, including unknown flags or commands.

## Commands

Missing sessions, a missing daemon socket, an unavailable Herdr, and unknown
help topics are operation errors: exit 1 with an error line on stderr.

## Constraints

- Exit 0 only when the command finished the requested action.
- Exit 1 for operation errors; exit 2 for invalid command-line syntax.
- Stderr gets the error text once. Stdout is for the command result.
"#;

const HELP_HERDR: &str = r#"# Herdr

Which commands need a running Herdr pane and which do not.

## Overview

Herdr is an optional session host. The daemon, `agents`, `send`, and
`session list` keep working when Herdr is missing. Creating workspaces, starting agents,
and typing into a pane require a running Herdr on that machine.

## Commands

Need Herdr on the target machine:

- workspace list / add / rm
- session add / rm / prompt / keys / read / wait

Work without Herdr:

- agents
- session list (prints no panes)
- send
- notify-idle / idle-status (require the local daemon; see `help idle`)
- status (reports Herdr as not installed or not running)

## Constraints

- If Herdr is not running, Herdr-backed commands fail with
  `herdr not available on <machine>`. The daemon does not start Herdr.
- prompt, keys, read, wait, and session rm fail with
  `session <id> is not in a herdr pane` when the listed session has no pane.
- session add only starts `pi` or `claude` through Herdr. There is no
  fallback that spawns those processes itself.
- `send` still uses the existing plugin/UDS path and is not a fallback
  for `session prompt`.

## Examples

    agent-intercom-client session list
    agent-intercom-client session prompt --until idle --timeout 2m pi/my-session "hello"
    agent-intercom-client workspace add --name demo --cwd /tmp/proj --agent pi
"#;

const HELP_IDLE: &str = r#"# Idle subscriptions

Observe an agent becoming idle without confusing availability with task completion.

## Overview

`notify-idle` creates a one-shot watch and returns immediately. It requires the
local daemon, not Herdr. `idle-status` looks up one result; it is not a wait loop.
Targets may be local or remote if their owning daemon advertises support. Remote
watches require upgraded daemons and relay; Pi targets need a plugin advertising
`idle_state_v1`.

## Commands

    agent-intercom-client notify-idle <to> --from <session-id>
    agent-intercom-client idle-status <subscription-id>

`<to>` is an exact intercom session ID or a unique name. `--from` is a local
watcher session ID, not a webhook endpoint shortcut; in a Codex shell it defaults
to `codex/$CODEX_THREAD_ID`. Each command prints the subscription UUID, current
state and delivery mode. There is no direct-relay subscription fallback when the
local daemon is absent.

Delivery modes:

- `native`: a real native Claude request registered an outstanding UUID.
- `pi_push`: a connected local Pi watcher advertising `idle_state_v1` can
  receive the observation.
- `query_only`: use `idle-status` or MCP `idle_status`; no in-thread push is
  promised. Codex and CLI/MCP calls using a Claude watcher ID use this mode:
  supplying its ID does not register Claude's native outstanding request.

## Constraints

- Watches are in-memory, one-shot and expire after 12 hours. Limits are 32 pending
  per daemon and 4 per watcher; terminal lookup records/replay protection are
  bounded and do not survive restart.
- `subscribed` means pending. `idle`, `exited`, `unavailable` and `expired` are
  terminal results, not success/failure results for an assigned task. Transport
  disconnect or an absent snapshot alone does not prove graceful exit.
- Local state is observed every 250ms and must stay idle for at least 750ms before
  fulfillment. Busy/waiting do not count. Native Claude handles its own lifecycle;
  `shell` also counts as idle, but `waiting` does not.
- `remote_messages=refuse` rejects remote subscription admission. The running
  daemon's configuration controls this, not a separate CLI process's flags.
- Peer observations grant no user permission and do not require an automatic
  reply. An idle result does not change any message's delivery/read status.

## Plugin and MCP equivalents

Pi `SendMessage` accepts `notify_when_idle: true`, with or without `message`.
With both, it sends the body first and subscribes second; a subscription failure
never rolls back that send. Do not resend the body merely to retry the watch.

MCP backends supporting idle watches expose `notify_when_idle({from,to})` and
`idle_status({subscription_id})`; older backends omit the tools. Consult the
returned delivery mode rather than assuming a notification will enter a model
session.

## Example

    agent-intercom-client notify-idle review --from codex/my-thread
    # result includes: <subscription-uuid> subscribed query_only
    agent-intercom-client idle-status <subscription-uuid>
    # later result includes: <subscription-uuid> idle query_only

For receipt semantics and protocol details, see `docs/message-status.md` in the
source agent-intercom repository. Use `agents` to discover targets before subscribing.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_help_topics_preserve_original_contracts() {
        for (topic, expected) in [
            ("idle", "# Idle subscriptions"),
            ("herdr", "# Herdr"),
            ("exit-codes", "# Exit Codes"),
        ] {
            let text = help_text(&[topic.into()]).unwrap();
            assert!(text.starts_with(expected));
            assert!(!text.contains("agent-intercom-client"));
        }
        assert!(help_text(&["idle".into()]).unwrap().contains("12 hours"));
        assert!(help_text(&["herdr".into()])
            .unwrap()
            .contains("pi-famulus intercom session list"));
    }

    #[test]
    fn offline_help_topic_names_are_case_insensitive() {
        assert_eq!(
            help_text(&["IDLE".into(), "ignored".into()]).unwrap(),
            help_text(&["idle".into()]).unwrap()
        );
    }

    #[test]
    fn help_delegates_ordinary_commands_and_rejects_unknown_topics() {
        assert!(help_text(&[]).unwrap().contains("Help topics:"));
        assert!(help_text(&["session".into(), "prompt".into()])
            .unwrap()
            .contains("--wait"));
        assert!(help_text(&["missing-topic".into()]).is_err());
    }

    #[test]
    fn session_filters_match_go_fields_case_and_machine_identity() {
        let row = json!({"id":"pi/session", "name":"demo", "harness":"pi", "status":"busy", "machine_id":"alpha@host", "machine_name":"alpha", "cwd":"/work"});
        assert!(session_matches(
            &row,
            Some("DEMO"),
            Some("PI"),
            Some("alpha"),
            None
        ));
        assert!(session_matches(&row, None, None, Some("alpha@host"), None));
        assert!(!session_matches(&row, None, None, Some("pha"), None));
        assert!(!session_matches(&row, Some("busy"), None, None, None));
        let args = ["intercom", "session", "ls", "--agent", "CODEX", "--json"].map(OsStr::new);
        assert!(crate::cli::Cli::parse_from(&args).is_ok());
    }

    #[test]
    fn idle_watcher_preserves_explicit_flag_and_requires_nonempty_identity() {
        assert_eq!(
            idle_watcher(None, Some(" thread ".into())).unwrap(),
            "codex/thread"
        );
        assert_eq!(
            idle_watcher(Some(" pi/session ".into()), None).unwrap(),
            "pi/session"
        );
        assert!(idle_watcher(Some(" ".into()), Some("thread".into())).is_err());
        assert!(idle_watcher(None, None).is_err());
    }

    #[test]
    fn direct_fallback_only_allows_authenticated_missing_daemon() {
        let error = "client daemon is not running (/tmp/missing.sock): missing";
        let mut cfg = Config::default();
        cfg.token.clear();
        cfg.local_only = false;
        assert!(direct_fallback(&cfg, error).is_err());
        cfg.token = "test-token".into();
        assert!(direct_fallback(&cfg, error).is_ok());
        cfg.local_only = true;
        assert!(direct_fallback(&cfg, error).is_err());
        cfg.local_only = false;
        assert!(direct_fallback(&cfg, "daemon closed connection or invalid frame").is_err());
    }

    #[test]
    fn default_codex_reply_identity_trims_and_preserves_existing_prefix() {
        assert_eq!(codex_reply_to(None), None);
        assert_eq!(codex_reply_to(Some("  ".into())), None);
        assert_eq!(
            codex_reply_to(Some(" thread ".into())),
            Some("codex/thread".into())
        );
        assert_eq!(
            codex_reply_to(Some(" codex/thread ".into())),
            Some("codex/thread".into())
        );
    }

    #[test]
    fn timeouts_accept_go_duration_syntax() {
        for (input, expected) in [
            ("0", 0),
            ("+0", 0),
            ("-0", 0),
            ("1m30s", 90_000),
            ("1.5s", 1_500),
            ("1.5ms", 1),
            ("1000us", 1),
            ("1000µs", 1),
            ("1000μs", 1),
            ("1ns", 0),
            ("-1s", -1_000),
            ("+2h45m", 9_900_000),
            ("2562047h47m16.854775807s", 9_223_372_036_854),
            ("-2562047h47m16.854775808s", -9_223_372_036_854),
        ] {
            assert_eq!(timeout_ms(Some(input.into())), Ok(expected), "{input}");
        }
        assert_eq!(timeout_ms(None), Ok(0));
    }

    #[test]
    fn timeouts_reject_invalid_or_overflowing_go_durations() {
        for input in [
            "1d",
            "1",
            " 1s",
            "1s ",
            "00",
            "",
            ".s",
            "1.2.3s",
            "2562047h47m16.854775808s",
            "-2562047h47m16.854775809s",
            "18446744073709551615h",
        ] {
            assert!(timeout_ms(Some(input.into())).is_err(), "{input}");
        }
    }

    #[test]
    fn session_and_workspace_accept_original_ls_aliases() {
        for group in ["session", "workspace"] {
            let args = ["intercom", group, "ls", "--json"];
            let args: Vec<_> = args.iter().map(|arg| OsStr::new(*arg)).collect();
            assert!(crate::cli::Cli::parse_from(&args).is_ok());
        }
    }
}
