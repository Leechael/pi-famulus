use super::{str_field, Config, Push, Shared};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

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
        // link, unlike rename, fails if anything appeared at the path since
        // the caller cleared it, instead of silently replacing it.
        std::fs::hard_link(&staged, path)?;
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
    let (read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Push>(128);
    let mut port = super::port::Port::new(rt.clone(), tx.clone(), super::port::PortOrigin::Client);
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
    let mut disconnected = port.disconnected();
    let mut reader = BufReader::new(read);
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
        let result = tokio::select! {biased;_=stop.changed()=>break,_=disconnected.changed()=>break,_=closing.changed()=>continue,result=port.handle(&req)=>result};
        let mut response = match result {
            Ok(mut v) => {
                if super::str_field(&v, "error").is_empty() {
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
    port.close().await;
    drop(tx);
    writer.abort();
    let _ = writer.await;
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
    async fn private_bind_never_replaces_a_path_that_appeared_meanwhile() {
        let root = std::path::PathBuf::from(format!(
            "/tmp/ib-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("client.sock");
        std::fs::write(&path, b"user data").unwrap();
        assert!(super::bind_private(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"user data");
        let leftovers = std::fs::read_dir(&root).unwrap().count();
        assert_eq!(leftovers, 1, "staging left behind");
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
            assert_eq!(
                std::fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            drop(listener);
            // A sibling test's fork can briefly hold the listener open, so
            // do not rely on bind's stale-socket probe here.
            std::fs::remove_file(&path).unwrap();
        }
        done.store(true, Ordering::Relaxed);
        // The watcher may never catch the path; it must never see it open.
        let seen = watcher.join().unwrap();
        assert!(
            seen.iter().all(|mode| *mode == 0o600),
            "the socket was reachable before it was made owner-only: {seen:?}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
