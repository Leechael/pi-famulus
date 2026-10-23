use super::{str_field, Config, Push, Session, Shared};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

#[cfg(unix)]
pub async fn bind(path: &Path) -> Result<(tokio::net::UnixListener, (u64, u64)), String> {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
    let parent = path.parent().ok_or("socket parent required")?;
    if let Some(ancestor) = parent.parent().filter(|p| !p.as_os_str().is_empty()) {
        super::config::private_dir(ancestor)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    let m = std::fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
    if !m.is_dir() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o022 != 0 {
        return Err(format!(
            "socket directory {} must be a directory owned by this user and not group/world writable",
            parent.display()
        ));
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() || m.uid() != unsafe { libc::getuid() } {
            return Err("refusing to replace unowned or non-socket path".into());
        }
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err("intercom socket already in use".into());
        }
        let now = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if (m.dev(), m.ino()) != (now.dev(), now.ino()) {
            return Err("socket changed during bind".into());
        }
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    let listener = bind_private(path)?;
    let m = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    Ok((listener, (m.dev(), m.ino())))
}
/// Bind a Unix socket that is never connectable by another user: it is bound
/// inside a fresh owner-only directory, made 0600 there, then renamed into
/// place. A process-wide umask cannot be narrowed safely in a threaded runtime.
#[cfg(unix)]
pub fn bind_private(path: &Path) -> Result<tokio::net::UnixListener, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let parent = path.parent().ok_or("socket parent required")?;
    let staging = parent.join(format!(".{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|e| e.to_string())?;
    let staged = staging.join("s");
    let result = (|| {
        let listener = std::os::unix::net::UnixListener::bind(&staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        std::fs::rename(&staged, path)?;
        tokio::net::UnixListener::from_std(listener)
    })();
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_dir(&staging);
    result.map_err(|e| e.to_string())
}
#[cfg(not(unix))]
pub async fn bind(_path: &Path) -> Result<(tokio::net::TcpListener, (u64, u64)), String> {
    Err("intercom native adapters support Linux/macOS only".into())
}

#[cfg(unix)]
pub async fn serve(rt: Shared, listener: tokio::net::UnixListener) {
    let mut stop = rt.closing.subscribe();
    loop {
        tokio::select! {biased;_=stop.changed()=>break,accepted=listener.accept()=>match accepted {Ok((stream,_))=>{if *stop.borrow(){break;}let owner=rt.clone();let task=tokio::spawn(async move{connection(owner,stream).await;});let mut tasks=rt.rpc_tasks.lock().unwrap();tasks.retain(|task|!task.is_finished());tasks.push(task);},Err(e)=>{eprintln!("intercom accept: {e}");tokio::time::sleep(Duration::from_millis(50)).await;}}}
    }
}
#[cfg(not(unix))]
pub async fn serve(_rt: Shared, _listener: tokio::net::TcpListener) {}

#[cfg(unix)]
async fn connection(rt: Shared, stream: tokio::net::UnixStream) {
    if *rt.closing.borrow() {
        return;
    }
    let conn = {
        let mut s = rt.state.lock().unwrap();
        s.next_conn += 1;
        s.next_conn
    };
    let (read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Push>(128);
    let mut writer_stop = rt.stop.subscribe();
    let writer = tokio::spawn(async move {
        loop {
            let p = tokio::select! {_=writer_stop.changed()=>break,p=rx.recv()=>match p{Some(p)=>p,None=>break}};
            let mut data = p.value.to_string().into_bytes();
            data.push(b'\n');
            let r = tokio::select! {biased;_=writer_stop.changed()=>break,result=tokio::time::timeout(Duration::from_secs(5),write.write_all(&data))=>result.map_err(|_|"write timeout".to_string()).and_then(|value|value.map_err(|e|e.to_string()))};
            let failed = r.is_err();
            drop(p.fence);
            if let Some(reply) = p.written {
                let _ = reply.send(r);
            }
            if failed {
                break;
            }
        }
    });
    let (disconnect, mut disconnected) = watch::channel(false);
    let mut reader = BufReader::new(read);
    let mut bound = String::new();
    let mut stop = rt.stop.subscribe();
    let mut closing = rt.closing.subscribe();
    loop {
        if *disconnected.borrow() {
            break;
        }
        if *closing.borrow() {
            if !*stop.borrow() {
                tokio::select! { _ = stop.changed() => {}, _ = disconnected.changed() => {} }
            }
            break;
        }
        let mut bytes = Vec::new();
        let mut limited = (&mut reader).take(1024 * 1024 + 1);
        let result = tokio::select! {biased;_=stop.changed()=>break,_=disconnected.changed()=>break,_=closing.changed()=>continue,r=limited.read_until(b'\n',&mut bytes)=>r};
        match result {
            Ok(0) | Err(_) => break,
            Ok(_) if bytes.len() > 1024 * 1024 => break,
            _ => {}
        }
        let req = match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) if v.is_object() => v,
            _ => {
                let _ = tx
                    .send(Push {
                        value: json!({"error":"invalid JSON request"}),
                        written: None,
                        fence: None,
                    })
                    .await;
                break;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let result = tokio::select! {biased;_=stop.changed()=>break,_=disconnected.changed()=>break,_=closing.changed()=>continue,result=handle(&rt,conn,&tx,&disconnect,&mut bound,&req)=>result};
        let mut response = match result {
            Ok(mut v) => {
                if str_field(&v, "error").is_empty() {
                    v["ok"] = json!(true);
                }
                v
            }
            Err(e) => json!({"error":e}),
        };
        if !id.is_null() {
            response["id"] = id;
        }
        let sent = tokio::select! {biased;_=stop.changed()=>break,_=disconnected.changed()=>break,_=closing.changed()=>continue,result=tx.send(Push{value:response,written:None,fence:None})=>result};
        if sent.is_err() {
            break;
        }
    }
    let removed = {
        let mut state = rt.state.lock().unwrap();
        if state
            .sessions
            .get(&bound)
            .is_some_and(|session| session.owner == conn)
        {
            state.sessions.remove(&bound).map(|session| session.epoch)
        } else {
            None
        }
    };
    if let Some(epoch) = removed {
        rt.cancel_idle_watcher(&bound, epoch).await;
        rt.native.source_unpublished(&bound, conn, epoch).await;
    }
    drop(tx);
    writer.abort();
    let _ = writer.await;
}

async fn handle(
    rt: &Shared,
    conn: u64,
    tx: &mpsc::Sender<Push>,
    disconnect: &watch::Sender<bool>,
    bound: &mut String,
    req: &Value,
) -> Result<Value, String> {
    let op = str_field(req, "op");
    if !bound.is_empty()
        && ["send", "receipt", "subscribe_idle", "inbound"].contains(&op)
        && rt
            .state
            .lock()
            .unwrap()
            .sessions
            .get(bound)
            .is_none_or(|v| v.owner != conn)
    {
        return Err("session ownership ended".into());
    }
    match op {
        "register" => {
            let mut v = req["session"].clone();
            let id = str_field(&v, "id");
            if id.is_empty() {
                return Err("session.id required".into());
            }
            let id = if id.starts_with("pi/") {
                id.to_string()
            } else {
                format!("pi/{id}")
            };
            v["id"] = json!(id);
            v["harness"] = json!("pi");
            if str_field(&v, "status").is_empty() {
                v["status"] = json!("idle");
            }
            let caps: HashSet<String> = v["caps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            v["notify_idle"] = json!(caps.contains("idle_state_v1"));
            if v.get("name").is_none() {
                v["name"] = json!("");
            }
            if v.get("cwd").is_none() {
                v["cwd"] = json!("");
            }
            v.as_object_mut().unwrap().remove("caps");
            let mut cancelled = Vec::new();
            let mut absences = Vec::new();
            {
                let mut s = rt.state.lock().unwrap();
                if s.unpublished.get(&id).is_some_and(|(_, blocked)| *blocked) {
                    return Err("session is unpublished; restore it with the local CLI".into());
                }
                if !bound.is_empty()
                    && *bound != id
                    && s.sessions
                        .get(bound)
                        .is_some_and(|session| session.owner == conn)
                {
                    if let Some(old) = s.sessions.remove(bound) {
                        cancelled.push((bound.clone(), old.epoch));
                        absences.push((bound.clone(), old.owner, old.epoch));
                    }
                    s.exited.insert(bound.clone());
                }
                let previous = s
                    .sessions
                    .get(&id)
                    .filter(|session| session.owner == conn)
                    .map(|session| (session.epoch, session.idle_since));
                let (epoch, idle_since) = if let Some((epoch, idle)) = previous {
                    (
                        epoch,
                        if str_field(&v, "status") == "idle" {
                            idle.or(Some(rt.clock.now_ms()))
                        } else {
                            None
                        },
                    )
                } else {
                    if let Some(old) = s.sessions.get(&id) {
                        cancelled.push((id.clone(), old.epoch));
                    }
                    s.next_epoch += 1;
                    (
                        s.next_epoch,
                        if str_field(&v, "status") == "idle" {
                            Some(rt.clock.now_ms())
                        } else {
                            None
                        },
                    )
                };
                s.unpublished.remove(&id);
                s.exited.remove(&id);
                s.sessions.insert(
                    id.clone(),
                    Session {
                        value: v,
                        owner: conn,
                        epoch,
                        tx: tx.clone(),
                        disconnect: Some(disconnect.clone()),
                        caps,
                        idle_since,
                    },
                );
            }
            *bound = id.clone();
            for (watcher, epoch) in cancelled {
                rt.cancel_idle_watcher(&watcher, epoch).await;
            }
            for (id, owner, epoch) in absences {
                rt.native.source_unpublished(&id, owner, epoch).await;
            }
            Ok(json!({"session_id":id}))
        }
        "unregister" => {
            let removed = {
                let mut state = rt.state.lock().unwrap();
                if state
                    .sessions
                    .get(bound)
                    .is_some_and(|session| session.owner == conn)
                {
                    let removed = state.sessions.remove(bound);
                    state.exited.insert(bound.clone());
                    removed.map(|session| (bound.clone(), session.epoch))
                } else {
                    None
                }
            };
            bound.clear();
            if let Some((watcher, epoch)) = removed {
                rt.cancel_idle_watcher(&watcher, epoch).await;
                rt.native.source_unpublished(&watcher, conn, epoch).await;
            }
            Ok(json!({}))
        }
        "list" => {
            let mut rows = rt.directory().await;
            if req["all"] != true {
                rows.retain(|r| r["all"] != true);
            }
            Ok(json!({"sessions":rows}))
        }
        "send" => {
            let mut reply = str_field(req, "reply_to").to_string();
            if bound.is_empty() && reply.is_empty() && !str_field(req, "as").is_empty() {
                let origin = str_field(req, "as");
                reply = if origin.starts_with("webhook/") {
                    origin.into()
                } else {
                    format!("webhook/{origin}")
                };
            }
            let result = rt
                .send(
                    bound,
                    str_field(req, "to"),
                    str_field(req, "body"),
                    &reply,
                    str_field(req, "from_mode"),
                )
                .await?;
            Ok(result)
        }
        "inbound" => {
            if !req["session"].is_object() {
                return Ok(json!({}));
            }
            let raw = str_field(&req["session"], "id");
            let id = if raw.starts_with("pi/") {
                raw.to_string()
            } else {
                format!("pi/{raw}")
            };
            if rt.state.lock().unwrap().unpublished.contains_key(&id) {
                return Err("session is unpublished".into());
            }
            let mid = uuid::Uuid::new_v4().to_string();
            let result=super::relay::call(rt,json!({"type":"inbound","message_id":mid,"session_id":id,"from":id,"body":req["body"].as_str().unwrap_or(""),"reply_to":req["reply_to"].as_str().unwrap_or("")})).await;
            match result {
                Ok(response)
                    if response["type"] == "inbound"
                        && response["message_id"] == mid
                        && matches!(str_field(&response, "status"), "accepted" | "queued") =>
                {
                    rt.record_status(&mid, str_field(&response, "status"), true);
                }
                Ok(_) => rt.record(
                    "error",
                    &mid,
                    &id,
                    "protocol error: invalid inbound acknowledgement",
                ),
                Err(error) => {
                    rt.record_status(
                        &mid,
                        if error.contains("not connected") {
                            "unavailable"
                        } else {
                            "unknown"
                        },
                        true,
                    );
                    rt.record("error", &mid, &id, &error);
                }
            }
            Ok(json!({}))
        }
        "receipt" => {
            if bound.is_empty() {
                return Err("receipt requires a registered session".into());
            }
            let status = str_field(req, "status");
            if !["read", "dropped"].contains(&status) || str_field(req, "message_id").is_empty() {
                return Err("message_id required; status must be read or dropped".into());
            }
            rt.local_receipt(str_field(req, "message_id"), bound, status)
                .await;
            Ok(json!({}))
        }
        "subscribe_idle" => {
            rt.subscribe(
                if bound.is_empty() {
                    str_field(req, "as")
                } else {
                    bound
                },
                str_field(req, "to"),
            )
            .await
        }
        "idle_status" => {
            let mut s = rt.state.lock().unwrap();
            s.subscriptions.retain(|_, w| {
                w.status == "subscribed"
                    || rt.clock.now_ms().saturating_sub(w.updated_at) <= 12 * 3600 * 1000
            });
            let w = s
                .subscriptions
                .get(str_field(req, "subscription_id"))
                .ok_or("unknown subscription id")?;
            if !bound.is_empty() && *bound != w.watcher {
                return Err("subscription belongs to another session".into());
            }
            Ok(
                json!({"subscription_id":req["subscription_id"],"status":w.status,"from":w.target,"delivery_mode":w.mode}),
            )
        }
        "session_registrations" | "session_unpublish" | "session_restore" => {
            if !bound.is_empty() {
                return Err(
                    "session publication operations require an unregistered local connection"
                        .into(),
                );
            }
            if !str_field(req, "machine").is_empty() {
                return Err("session publication operations are local-only".into());
            }
            let absences = {
                let mut absences = Vec::new();
                let mut s = rt.state.lock().unwrap();
                if op == "session_registrations" {
                    let mut rows: Vec<_> = s.sessions.values().map(|v| v.value.clone()).collect();
                    if req["all"] == true {
                        for (v, blocked) in s.unpublished.values() {
                            let mut v = v.clone();
                            v["status"] = json!(if *blocked { "unpublished" } else { "restoring" });
                            rows.push(v);
                        }
                    }
                    rows.sort_by(|a, b| str_field(a, "id").cmp(str_field(b, "id")));
                    return Ok(json!({"sessions":rows}));
                }
                let ids = req["args"]
                    .as_array()
                    .ok_or("args must contain 1..128 exact pi/ session IDs")?;
                if ids.is_empty() || ids.len() > 128 {
                    return Err("args must contain 1..128 exact pi/ session IDs".into());
                }
                let mut unique = HashSet::new();
                for id in ids {
                    let id = id
                        .as_str()
                        .ok_or("invalid local Pi session ID; use an exact full pi/ ID")?;
                    if !id.starts_with("pi/")
                        || id.len() <= 3
                        || id.chars().any(|c| c.is_whitespace() || c.is_control())
                    {
                        return Err("invalid local Pi session ID; use an exact full pi/ ID".into());
                    }
                    if !s.sessions.contains_key(id) && !s.unpublished.contains_key(id) {
                        return Err(format!("unknown local Pi registration {id:?}"));
                    }
                    unique.insert(id.to_string());
                }
                let additional = unique
                    .iter()
                    .filter(|id| !s.unpublished.contains_key(*id))
                    .count();
                if op == "session_unpublish" && s.unpublished.len() + additional > 1024 {
                    return Err("unpublished registration capacity reached".into());
                }
                for id in unique {
                    if op == "session_restore" {
                        if let Some((_, blocked)) = s.unpublished.get_mut(&id) {
                            *blocked = false;
                        }
                        s.exited.remove(&id);
                    } else {
                        if let Some(v) = s.sessions.remove(&id) {
                            absences.push((id.clone(), v.owner, v.epoch, v.disconnect));
                            s.unpublished.insert(id.clone(), (v.value, true));
                        } else if let Some((_, blocked)) = s.unpublished.get_mut(&id) {
                            *blocked = true;
                        }
                        s.exited.remove(&id);
                    }
                }
                absences
            };
            for (id, owner, epoch, disconnect) in absences {
                rt.cancel_idle_watcher(&id, epoch).await;
                rt.native.source_unpublished(&id, owner, epoch).await;
                if let Some(disconnect) = disconnect {
                    let _ = disconnect.send(true);
                }
            }
            Ok(json!({}))
        }
        "message_status" => {
            let mut s = rt.state.lock().unwrap();
            s.prune_deliveries(rt.clock.now_ms());
            if !s.history.contains(str_field(req, "message_id")) {
                return Ok(json!({"message_id":req["message_id"],"status":"unknown"}));
            }
            let d = s
                .messages
                .get(str_field(req, "message_id"))
                .ok_or("unknown message id")?;
            Ok(json!({"message_id":req["message_id"],"status":d.status,"updated_at":d.updated_at}))
        }
        "session_name" => {
            let row = rt.resolve(str_field(req, "to")).await?;
            let id = str_field(&row, "id");
            let name = str_field(req, "name");
            if name.is_empty() {
                return Err("name required".into());
            }
            rt.native.rename(&row, name).await?;
            rt.state
                .lock()
                .unwrap()
                .aliases
                .insert(id.into(), name.into());
            Ok(json!({}))
        }
        "status" => {
            let machine = str_field(req, "machine");
            if !machine.is_empty() && machine != rt.machine {
                super::relay::call(rt, json!({"type":"status","machine_id":machine})).await
            } else {
                Ok(json!({"report":rt.status_report().await}))
            }
        }
        "workspace_list" | "workspace_create" | "workspace_close" | "session_create"
        | "session_close" | "agent_prompt" | "agent_keys" | "agent_read" | "agent_wait" => {
            super::native::herdr(rt, req).await
        }
        _ => Err(format!("unknown op {op:?}")),
    }
}

#[cfg(unix)]
pub async fn call(cfg: &Config, req: Value) -> Result<Value, String> {
    let value = call_raw(cfg, req).await?;
    if !str_field(&value, "error").is_empty() {
        return Err(str_field(&value, "error").into());
    }
    Ok(value)
}
#[cfg(unix)]
pub async fn call_raw(cfg: &Config, mut req: Value) -> Result<Value, String> {
    let stream = tokio::net::UnixStream::connect(&cfg.sock)
        .await
        .map_err(|e| format!("client daemon is not running ({}): {e}", cfg.sock.display()))?;
    let (read, mut write) = stream.into_split();
    let id = uuid::Uuid::new_v4().to_string();
    req["id"] = json!(id);
    let mut data = req.to_string();
    data.push('\n');
    write
        .write_all(data.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(read);
    loop {
        let mut bytes = Vec::new();
        let n = tokio::time::timeout(
            Duration::from_secs(65),
            (&mut reader)
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut bytes),
        )
        .await
        .map_err(|_| "daemon request timed out".to_string())?
        .map_err(|e| e.to_string())?;
        if n == 0 || n > 1024 * 1024 {
            return Err("daemon closed connection or invalid frame".into());
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if v["id"] == id {
            return Ok(v);
        }
    }
}
#[cfg(not(unix))]
pub async fn call(_cfg: &Config, _req: Value) -> Result<Value, String> {
    Err("intercom native adapters support Linux/macOS only".into())
}
#[cfg(not(unix))]
pub async fn call_raw(_cfg: &Config, _req: Value) -> Result<Value, String> {
    Err("intercom native adapters support Linux/macOS only".into())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn bind_creates_missing_socket_directories_private() {
        let root = std::path::PathBuf::from(format!(
            "/tmp/ib-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        let path = root.join("nested/run/client.sock");
        let (_listener, _) = super::bind(&path).await.unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn bind_refuses_a_writable_existing_directory_without_chmodding_it() {
        let root = std::path::PathBuf::from(format!(
            "/tmp/ib-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = super::bind(&root.join("client.sock")).await.unwrap_err();
        assert!(error.contains("writable"), "{error}");
        assert_eq!(
            mode(&root),
            0o777,
            "bind chmodded a directory it did not create"
        );
        assert!(!root.join("client.sock").exists());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_listener, _) = super::bind(&root.join("client.sock")).await.unwrap();
        assert_eq!(mode(&root), 0o755);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn bound_socket_is_never_visible_with_a_permissive_mode() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = std::path::PathBuf::from(format!(
            "/tmp/ib-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.join("client.sock");
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let watcher = {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                let mut seen = std::collections::BTreeSet::new();
                while !done.load(Ordering::Relaxed) {
                    if let Ok(m) = std::fs::symlink_metadata(&path) {
                        seen.insert(m.permissions().mode() & 0o777);
                    }
                }
                seen
            })
        };
        for attempt in 0..300 {
            let (listener, _) = super::bind(&path)
                .await
                .unwrap_or_else(|error| panic!("{attempt}: {error}"));
            drop(listener);
            // A sibling test's fork can briefly hold the listener open, so
            // do not rely on bind's stale-socket probe here.
            std::fs::remove_file(&path).unwrap();
        }
        done.store(true, Ordering::Relaxed);
        let seen = watcher.join().unwrap();
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            vec![0o600],
            "the socket was reachable before it was made owner-only"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
