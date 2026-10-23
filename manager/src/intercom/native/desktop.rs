use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

#[derive(Clone)]
pub(super) struct Client {
    tx: mpsc::Sender<Command>,
    catalog: Arc<Mutex<HashMap<String, Value>>>,
    connected: Arc<AtomicBool>,
}
struct Command {
    thread: String,
    body: String,
    mid: String,
    observe: bool,
    fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    reply: oneshot::Sender<Result<(String, String), String>>,
}
// Desktop rows are only true while this connection is attached; a detached
// catalog would keep advertising threads and their alias children.
#[cfg(unix)]
struct Attached {
    connected: Arc<AtomicBool>,
    catalog: Arc<Mutex<HashMap<String, Value>>>,
}
#[cfg(unix)]
impl Drop for Attached {
    fn drop(&mut self) {
        self.catalog.lock().unwrap().clear();
        self.connected.store(false, Ordering::Release);
    }
}
impl Client {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
    pub async fn idle_observation(&self, thread: &str) -> Result<(String, String), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command {
                thread: thread.into(),
                body: String::new(),
                mid: String::new(),
                observe: true,
                fence: None,
                reply: tx,
            })
            .await
            .map_err(|_| "Desktop IPC not connected".to_string())?;
        rx.await
            .map_err(|_| "Desktop idle observation connection closed".to_string())?
    }
    pub fn list(&self) -> Vec<Value> {
        self.catalog.lock().unwrap().values().cloned().collect()
    }
    pub async fn send(
        &self,
        thread: &str,
        body: String,
        mid: &str,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command {
                thread: thread.into(),
                body,
                mid: mid.into(),
                observe: false,
                fence,
                reply: tx,
            })
            .await
            .map_err(|_| "Desktop IPC not connected".to_string())?;
        rx.await
            .map_err(|_| "Desktop send outcome unknown: connection closed".to_string())?
            .map(|(status, _)| status)
    }
}
#[cfg(unix)]
struct Connection {
    writer: tokio::net::unix::OwnedWriteHalf,
    broken: bool,
    frames: mpsc::Receiver<Result<Value, String>>,
    _reader: Reader,
    source: String,
    generation: String,
    owners: HashMap<String, String>,
    states: HashMap<String, (u64, Value)>,
    /// Correlation state is per-thread and bounded: timestamps age entries
    /// out and a size cap evicts the oldest, expiring unresolved waits with
    /// an explicit unknown outcome.
    completed: HashMap<(String, String, String), u64>,
    waiting: HashMap<(String, String, String), Vec<(String, u64)>>,
    following: HashMap<String, tokio::time::Instant>,
    owner: Weak<super::super::Runtime>,
    catalog: Arc<Mutex<HashMap<String, Value>>>,
}
#[cfg(unix)]
const CORRELATION_TTL_MS: u64 = 600_000;
#[cfg(unix)]
const CORRELATION_LIMIT: usize = 1024;
#[cfg(unix)]
struct Reader(tokio::task::JoinHandle<()>);
#[cfg(unix)]
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(super) async fn start(
    rt: &Shared,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        let (tx, mut rx) = mpsc::channel::<Command>(64);
        let catalog = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(false));
        let client = Client {
            tx,
            catalog: catalog.clone(),
            connected: connected.clone(),
        };
        *rt.native.desktop.lock().unwrap() = Some(client);
        let owner = Arc::downgrade(rt);
        let attached = connected;
        let mut stop = stop;
        let task = tokio::spawn(async move {
            loop {
                let path = std::env::var_os("CODEX_DESKTOP_IPC_SOCK")
                    .or_else(|| std::env::var_os("CODEX_IPC_SOCK"))
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        std::env::var_os("CODEX_HOME")
                            .map(PathBuf::from)
                            .unwrap_or_else(|| super::super::config::home().join(".codex"))
                            .join("ipc/ipc.sock")
                    });
                let connected = connect(&path, owner.clone(), catalog.clone()).await;
                if let Ok(mut connection) = connected {
                    attached.store(true, Ordering::Release);
                    let _attached = Attached {
                        connected: attached.clone(),
                        catalog: catalog.clone(),
                    };
                    let mut cleanup = tokio::time::interval(Duration::from_secs(30));
                    while !connection.broken {
                        tokio::select! {
                            _=stop.changed()=>return,
                            command=rx.recv()=>{let Some(mut command)=command else{return};let result=if command.observe{tokio::select!{_ = command.reply.closed()=>continue,result=connection.idle_observation(&command.thread)=>result}}else{connection.send(&mut command).await.map(|state|(state,String::new()))};let _=command.reply.send(result);},
                            frame=connection.frames.recv()=>{match frame{Some(Ok(value))=>{if connection.broadcast(&value).await.is_err(){break;}},_=>break}},
                            _=cleanup.tick()=>{
                                let expired_waits = connection.prune_correlation(crate::proto::now_ms());
                                for (mid, thread) in expired_waits {
                                    if let Some(runtime) = connection.owner.upgrade() {
                                        runtime.local_receipt(&mid, &format!("codex/{thread}"), "unknown").await;
                                    }
                                }
                                let expired=connection.following.iter().filter(|(_,deadline)|**deadline<=tokio::time::Instant::now()).map(|(thread,_)|thread.clone()).collect::<Vec<_>>();for thread in expired{connection.unfollow(&thread).await;}
                            }
                        }
                    }
                    drop(connection);
                }
                tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_secs(3))=>{},command=rx.recv()=>{if let Some(command)=command{let _=command.reply.send(Err("Desktop IPC not connected".into()));}else{return;}}}
            }
        });
        Some(task)
    }
    #[cfg(not(unix))]
    {
        let _ = (rt, stop);
        None
    }
}
#[cfg(unix)]
async fn connect(
    path: &std::path::Path,
    owner: Weak<super::super::Runtime>,
    catalog: Arc<Mutex<HashMap<String, Value>>>,
) -> Result<Connection, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = super::socket_metadata(path)?;
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    let parent = std::fs::metadata(path.parent().unwrap()).map_err(|e| e.to_string())?;
    if parent.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::getuid() } {
        return Err("Desktop IPC directory is not private".into());
    }
    let stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(path),
    )
    .await
    .map_err(|_| "Desktop IPC connection timed out".to_string())?
    .map_err(|e| e.to_string())?;
    let after = super::socket_metadata(path)?;
    {
        use std::os::unix::fs::MetadataExt;
        if (after.dev(), after.ino()) != identity {
            return Err("Desktop IPC socket replaced during connect".into());
        }
    }
    if stream.peer_cred().map_err(|e| e.to_string())?.uid() != unsafe { libc::getuid() } {
        return Err("Desktop IPC peer UID mismatch".into());
    }
    let mut connection = attach(stream, owner, catalog);
    let response = connection
        .request(
            "initialize",
            json!({"clientType":"agent-intercom"}),
            "",
            0,
            None,
        )
        .await?;
    let source = str_field(&response["result"], "clientId");
    if source.is_empty() {
        return Err("Desktop initialize: missing client id".into());
    }
    connection.source = source.into();
    Ok(connection)
}
#[cfg(unix)]
fn attach(
    stream: tokio::net::UnixStream,
    owner: Weak<super::super::Runtime>,
    catalog: Arc<Mutex<HashMap<String, Value>>>,
) -> Connection {
    let (mut reader, writer) = stream.into_split();
    let (tx, frames) = mpsc::channel(256);
    let reader = Reader(tokio::spawn(async move {
        loop {
            let result = async {
                let size = reader.read_u32_le().await.map_err(|e| e.to_string())? as usize;
                if size == 0 || size > 64 * 1024 * 1024 {
                    return Err("invalid Desktop IPC frame".into());
                }
                let mut bytes = vec![0; size];
                reader
                    .read_exact(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
            }
            .await;
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    }));
    Connection {
        writer,
        broken: false,
        frames,
        _reader: reader,
        source: "initializing-client".into(),
        generation: uuid::Uuid::new_v4().to_string(),
        owners: HashMap::new(),
        states: HashMap::new(),
        completed: HashMap::new(),
        waiting: HashMap::new(),
        following: HashMap::new(),
        owner,
        catalog,
    }
}
#[cfg(unix)]
impl Connection {
    // A frame interrupted by a timeout, an I/O error or a cancelled caller
    // leaves the length-prefixed stream unframed; the connection is then unusable.
    async fn write(&mut self, value: Value) -> Result<(), String> {
        if self.broken {
            return Err("Desktop IPC stream is unframed after an interrupted write".into());
        }
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        self.broken = true;
        tokio::time::timeout(Duration::from_secs(5), async {
            self.writer.write_u32_le(bytes.len() as u32).await?;
            self.writer.write_all(&bytes).await
        })
        .await
        .map_err(|_| "Desktop write outcome unknown: timeout".to_string())?
        .map_err(|e| format!("Desktop write outcome unknown: {e}"))?;
        self.broken = false;
        Ok(())
    }
    async fn next(&mut self) -> Result<Value, String> {
        self.frames
            .recv()
            .await
            .ok_or("Desktop IPC connection closed")?
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        target: &str,
        version: u64,
        fence: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<Value, String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.write(json!({"type":"request","requestId":id,"sourceClientId":self.source,"targetClientId":target,"version":version,"method":method,"params":params,"timeoutMs":10000})).await?;
        drop(fence);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let value = tokio::time::timeout_at(deadline, self.next())
                .await
                .map_err(|_| format!("Desktop {method}: timeout"))??;
            if str_field(&value, "type") == "response" && value["requestId"] == id {
                if str_field(&value, "resultType") == "error" {
                    return Err(format!("Desktop {method}: {}", value["error"]));
                }
                if !str_field(&value, "method").is_empty() && value["method"] != method {
                    return Err("Desktop response method mismatch".into());
                }
                return Ok(value);
            }
            self.broadcast(&value).await?;
        }
    }
    async fn follow(&mut self, thread: &str, owner: &str, following: bool) -> Result<(), String> {
        self.write(json!({"type":"broadcast","method":"thread-stream-following-changed","sourceClientId":self.source,"targetClientIds":[owner],"version":1,"params":{"conversationId":thread,"hostId":"local","following":following}})).await?;
        if following {
            self.following.insert(
                thread.into(),
                tokio::time::Instant::now() + Duration::from_secs(300),
            );
        }
        Ok(())
    }
    async fn unfollow(&mut self, thread: &str) {
        if self.following.remove(thread).is_some() {
            if let Some(owner) = self.owners.get(thread).cloned() {
                let _ = self.follow(thread, &owner, false).await;
            }
        }
        // Unfollowed threads stop streaming, so their pending waits can never
        // resolve: settle them with an explicit unknown outcome and retire the
        // heavyweight per-thread state. The catalog row and owner stay so the
        // thread remains listed and addressable; the next send or observation
        // re-discovers fresh state.
        let unresolved = self.retire_thread_observation(thread);
        for mid in unresolved {
            if let Some(runtime) = self.owner.upgrade() {
                runtime
                    .local_receipt(&mid, &format!("codex/{thread}"), "unknown")
                    .await;
            }
        }
    }
    /// Retire the heavyweight per-thread observation state (the conversation
    /// state JSON and correlation entries) and return the mids of waits that
    /// can now never resolve, so the caller settles them unknown.
    fn retire_thread_observation(&mut self, thread: &str) -> Vec<String> {
        self.states.remove(thread);
        self.completed.retain(|(t, _, _), _| t != thread);
        let mut unresolved = Vec::new();
        self.waiting.retain(|(t, _, _), mids| {
            if t == thread {
                unresolved.extend(mids.iter().map(|(mid, _)| mid.clone()));
                false
            } else {
                true
            }
        });
        unresolved
    }
    /// Drop every cached fact about a thread, including its catalog row and
    /// owner: used when the thread is gone (discovery failure, owner change).
    fn retire_thread_state(&mut self, thread: &str) -> Vec<String> {
        let unresolved = self.retire_thread_observation(thread);
        self.owners.remove(thread);
        self.catalog.lock().unwrap().remove(thread);
        unresolved
    }
    /// Ages correlation state out and enforces its size bound. Returns
    /// `(mid, thread)` for waits that expired or were evicted unresolved so
    /// the caller settles them with an explicit unknown outcome.
    fn prune_correlation(&mut self, now: u64) -> Vec<(String, String)> {
        self.completed
            .retain(|_, at| now.saturating_sub(*at) <= CORRELATION_TTL_MS);
        let mut expired = Vec::new();
        self.waiting.retain(|key, mids| {
            mids.retain(|(mid, at)| {
                if now.saturating_sub(*at) <= CORRELATION_TTL_MS {
                    true
                } else {
                    expired.push((mid.clone(), key.0.clone()));
                    false
                }
            });
            !mids.is_empty()
        });
        while self.completed.len() > CORRELATION_LIMIT {
            let Some(oldest) = self
                .completed
                .iter()
                .min_by_key(|(_, at)| *at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.completed.remove(&oldest);
        }
        let waiting_total: usize = self.waiting.values().map(Vec::len).sum();
        if waiting_total > CORRELATION_LIMIT {
            let mut flat: Vec<(u64, (String, String, String), String)> = self
                .waiting
                .iter()
                .flat_map(|(key, mids)| {
                    mids.iter()
                        .map(move |(mid, at)| (*at, key.clone(), mid.clone()))
                })
                .collect();
            flat.sort_unstable();
            for (_, key, mid) in flat.into_iter().take(waiting_total - CORRELATION_LIMIT) {
                if let Some(mids) = self.waiting.get_mut(&key) {
                    if let Some(index) = mids.iter().position(|(m, _)| m == &mid) {
                        mids.swap_remove(index);
                    }
                }
                expired.push((mid, key.0));
            }
            self.waiting.retain(|_, mids| !mids.is_empty());
        }
        expired
    }
    /// Returns whether the event replaced the stored state of its thread.
    async fn broadcast(&mut self, event: &Value) -> Result<bool, String> {
        if str_field(event, "type") == "client-discovery-request" {
            self.write(json!({"type":"client-discovery-response","requestId":event["requestId"],"response":{"canHandle":false}})).await?;
            return Ok(false);
        }
        if str_field(event, "method") != "thread-stream-state-changed" || event["version"] != 11 {
            return Ok(false);
        }
        if event["targetClientIds"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty() && !ids.iter().any(|id| id == &self.source))
        {
            return Ok(false);
        }
        let params = &event["params"];
        let thread = str_field(params, "conversationId");
        let owner = str_field(event, "sourceClientId");
        if thread.is_empty()
            || str_field(params, "hostId") != "local"
            || self.owners.get(thread).is_none_or(|id| id != owner)
        {
            return Ok(false);
        }
        let change = &params["change"];
        let revision = change["revision"].as_u64().unwrap_or(0);
        let previous = self.states.get(thread).map(|(r, _)| *r).unwrap_or(0);
        let state = match str_field(change, "type") {
            "snapshot" if revision >= previous && change["conversationState"].is_object() => {
                change["conversationState"].clone()
            }
            "patches" if revision > previous => {
                if previous == 0 || change["baseRevision"].as_u64() != Some(previous) {
                    self.follow(thread, owner, true).await?;
                    return Ok(false);
                }
                let mut state = self.states[thread].1.clone();
                for patch in change["patches"].as_array().into_iter().flatten() {
                    if apply_patch(&mut state, patch).is_err() {
                        return Ok(false);
                    }
                }
                state
            }
            _ => return Ok(false),
        };
        let status = str_field(&state["threadRuntimeStatus"], "type");
        let row = json!({"id":format!("codex/{thread}"),"name":state["title"].as_str().unwrap_or(""),"cwd":state["cwd"].as_str().unwrap_or(""),"harness":"codex","status":status,"notify_idle":true,"_desktop":true});
        self.catalog.lock().unwrap().insert(thread.into(), row);
        if str_field(&state["turnHistory"], "kind") == "canonical" {
            for turn in state["turnHistory"]["history"]["entitiesByKey"]
                .as_object()
                .into_iter()
                .flat_map(|o| o.values())
            {
                if str_field(turn, "status") == "completed" && !str_field(turn, "turnId").is_empty()
                {
                    let key = (
                        thread.to_string(),
                        owner.to_string(),
                        str_field(turn, "turnId").to_string(),
                    );
                    self.completed.insert(key.clone(), crate::proto::now_ms());
                    if let Some(messages) = self.waiting.remove(&key) {
                        if let Some(runtime) = self.owner.upgrade() {
                            for (mid, _) in messages {
                                runtime
                                    .local_receipt(&mid, &format!("codex/{thread}"), "read")
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        self.states.insert(thread.into(), (revision, state));
        Ok(true)
    }
    async fn fresh_state(&mut self, thread: &String) -> Result<(String, Value), String> {
        let discovered = match self
            .request(
                "thread-owner-discovery",
                json!({"hostId":"local","conversationId":thread}),
                "",
                1,
                None,
            )
            .await
        {
            Ok(discovered) => discovered,
            Err(error) => {
                // Discovery failed: the cached owner and state may be stale,
                // so retire them instead of keeping the thread half-known.
                // Their pending sends can no longer be correlated, but their
                // outcomes must remain explicitly unknown rather than stuck.
                let unresolved = self.retire_thread_state(thread);
                for mid in unresolved {
                    if let Some(runtime) = self.owner.upgrade() {
                        runtime
                            .local_receipt(&mid, &format!("codex/{thread}"), "unknown")
                            .await;
                    }
                }
                return Err(error);
            }
        };
        let owner = str_field(&discovered, "handledByClientId").to_string();
        if owner.is_empty() {
            let unresolved = self.retire_thread_state(thread);
            for mid in unresolved {
                if let Some(runtime) = self.owner.upgrade() {
                    runtime
                        .local_receipt(&mid, &format!("codex/{thread}"), "unknown")
                        .await;
                }
            }
            return Err("Desktop owner discovery returned no owner".into());
        }
        if self
            .owners
            .get(thread)
            .is_some_and(|previous| previous != &owner)
        {
            // The previous owner can never complete turns it owned; resolve
            // their unresolved waits with an explicit unknown outcome.
            let unresolved = self.retire_thread_state(thread);
            for mid in unresolved {
                if let Some(runtime) = self.owner.upgrade() {
                    runtime
                        .local_receipt(&mid, &format!("codex/{thread}"), "unknown")
                        .await;
                }
            }
        }
        self.owners.insert(thread.clone(), owner.clone());
        let previous = self.states.get(thread).map(|(r, _)| *r).unwrap_or(0);
        self.follow(thread, &owner, true).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let state = loop {
            let event = tokio::time::timeout_at(deadline, self.next())
                .await
                .map_err(|_| "Desktop fresh owner state timeout".to_string())??;
            let fresh = str_field(&event["params"], "conversationId") == thread
                && str_field(&event["params"]["change"], "type") == "snapshot"
                && event["params"]["change"]["revision"]
                    .as_u64()
                    .is_some_and(|r| r >= previous)
                && str_field(&event, "sourceClientId") == owner;
            if self.broadcast(&event).await? && fresh {
                break self.states[thread].1.clone();
            }
        };
        Ok((owner, state))
    }
    async fn idle_observation(&mut self, thread: &String) -> Result<(String, String), String> {
        let (owner, state) = self.fresh_state(thread).await?;
        let status = str_field(&state["threadRuntimeStatus"], "type").to_ascii_lowercase();
        let flags = state["threadRuntimeStatus"]["activeFlags"]
            .as_array()
            .filter(|flags| !flags.is_empty());
        let result = if let Some(flags) = flags {
            if flags.iter().any(|flag| {
                flag.as_str().is_some_and(|flag| {
                    ["waitingonapproval", "waitingonuserinput"]
                        .contains(&flag.to_ascii_lowercase().as_str())
                })
            }) {
                "waiting"
            } else {
                "active"
            }
        } else if status == "idle"
            && state["turnHistory"]["history"]["entitiesByKey"]
                .as_object()
                .is_some_and(|turns| {
                    turns.values().any(|turn| {
                        matches!(
                            str_field(turn, "status").to_ascii_lowercase().as_str(),
                            "active" | "inprogress" | "running" | "busy"
                        )
                    })
                })
        {
            "active"
        } else {
            match status.as_str() {
                "idle" => "idle",
                "waiting" | "waitingonapproval" | "waitingonuserinput" => "waiting",
                "offline" | "notloaded" => "notLoaded",
                "active" | "inprogress" | "running" | "busy" => "active",
                _ => {
                    return Err(format!(
                        "Desktop host state {status:?} is not an idle observation"
                    ));
                }
            }
        };
        if self.owners.get(thread) != Some(&owner) {
            return Err("Desktop owner changed during observation".into());
        }
        Ok((
            result.into(),
            format!("desktop:{}:{owner}", self.generation),
        ))
    }
    async fn send(&mut self, command: &mut Command) -> Result<String, String> {
        let thread = &command.thread;
        let (owner, state) = self.fresh_state(thread).await?;
        let status = str_field(&state["threadRuntimeStatus"], "type").to_string();
        let input = json!([{"type":"text","text":command.body,"text_elements":[]}]);
        let busy = matches!(
            status.as_str(),
            "active" | "inProgress" | "running" | "busy"
        );
        let (method, version, params) = if busy {
            (
                "thread-follower-steer-turn",
                1,
                json!({"conversationId":thread,"clientUserMessageId":uuid::Uuid::new_v4().to_string(),"input":input,"attachments":[],"additionalContext":[],"serviceTier":null,"toolOutput":null,"restoreMessage":{"id":uuid::Uuid::new_v4().to_string(),"text":command.body,"cwd":"/","createdAt":crate::proto::now_ms(),"context":{"prompt":command.body,"turnTrigger":"user","addedFiles":[],"fileAttachments":[],"ideContext":null,"imageAttachments":[],"workspaceRoots":["/"]},"responsesapiClientMetadata":{"workspace_kind":"project"}}}),
            )
        } else {
            (
                "thread-follower-start-turn",
                2,
                json!({"conversationId":thread,"turnStart":{"request":{"threadId":thread,"input":input,"responsesapiClientMetadata":{"workspace_kind":"project"},"serviceTier":null},"context":{"inheritThreadSettings":true}}}),
            )
        };
        let response = self
            .request(method, params, &owner, version, command.fence.take())
            .await
            .map_err(|e| format!("Desktop send outcome unknown: {e}"))?;
        let result = &response["result"];
        let turn = [
            &result["id"],
            &result["turnId"],
            &result["turn"]["id"],
            &result["result"]["id"],
            &result["result"]["turnId"],
            &result["result"]["turn"]["id"],
        ]
        .into_iter()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        .ok_or("Desktop send outcome unknown: missing turn ID")?
        .to_string();
        let key = (thread.clone(), owner, turn);
        if self.completed.contains_key(&key) {
            self.unfollow(thread).await;
            return Ok("read".into());
        }
        self.waiting
            .entry(key)
            .or_default()
            .push((command.mid.clone(), crate::proto::now_ms()));
        Ok("delivered".into())
    }
}
fn apply_patch(root: &mut Value, patch: &Value) -> Result<(), String> {
    let path = patch["path"].as_array().ok_or("invalid Immer path")?;
    let mut node = root;
    if path.is_empty() {
        match str_field(patch, "op") {
            "add" | "replace" => {
                *node = patch["value"].clone();
                return Ok(());
            }
            _ => return Err("invalid Immer root operation".into()),
        }
    }
    for segment in &path[..path.len() - 1] {
        node = if let Some(key) = segment.as_str() {
            node.get_mut(key)
        } else {
            segment.as_u64().and_then(|i| node.get_mut(i as usize))
        }
        .ok_or("Immer path missing")?;
    }
    let last = path.last().unwrap();
    let op = str_field(patch, "op");
    if let Some(object) = node.as_object_mut() {
        let key = last.as_str().ok_or("invalid Immer object key")?;
        match op {
            "remove" => {
                if object.remove(key).is_none() {
                    return Err("Immer key missing".into());
                }
            }
            "replace" if !object.contains_key(key) => return Err("Immer key missing".into()),
            "add" | "replace" => {
                object.insert(key.into(), patch["value"].clone());
            }
            _ => return Err("unsupported Immer operation".into()),
        }
    } else if let Some(array) = node.as_array_mut() {
        let index = last
            .as_u64()
            .map(|i| i as usize)
            .or_else(|| last.as_str().and_then(|s| s.parse().ok()))
            .ok_or("invalid Immer array index")?;
        match op {
            "add" if index <= array.len() => array.insert(index, patch["value"].clone()),
            "replace" if index < array.len() => array[index] = patch["value"].clone(),
            "remove" if index < array.len() => {
                array.remove(index);
            }
            _ => return Err("invalid Immer array operation".into()),
        }
    } else {
        return Err("invalid Immer container".into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn connection() -> (Connection, tokio::net::UnixStream) {
        let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
        let mut connection = attach(ours, Weak::new(), Arc::new(Mutex::new(HashMap::new())));
        connection.source = "follower".into();
        (connection, peer)
    }
    async fn read_frame(peer: &mut tokio::net::UnixStream) -> Value {
        let size = peer.read_u32_le().await.unwrap() as usize;
        let mut bytes = vec![0; size];
        peer.read_exact(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    async fn write_frame(peer: &mut tokio::net::UnixStream, value: Value) {
        let bytes = serde_json::to_vec(&value).unwrap();
        peer.write_u32_le(bytes.len() as u32).await.unwrap();
        peer.write_all(&bytes).await.unwrap();
    }
    fn snapshot(version: u64, revision: u64, title: &str) -> Value {
        json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner","version":version,
            "params":{"conversationId":"t","hostId":"local","change":{"type":"snapshot","revision":revision,
            "conversationState":{"title":title,"threadRuntimeStatus":{"type":"idle"}}}}})
    }

    #[tokio::test]
    async fn fresh_state_never_returns_the_cached_state_for_an_unapplied_snapshot() {
        let (mut connection, mut peer) = connection();
        connection
            .states
            .insert("t".into(), (5, json!({"title":"cached"})));
        let host = tokio::spawn(async move {
            let discovery = read_frame(&mut peer).await;
            write_frame(&mut peer, json!({"type":"response","requestId":discovery["requestId"],
                "resultType":"success","method":"thread-owner-discovery","handledByClientId":"owner"})).await;
            assert_eq!(read_frame(&mut peer).await["params"]["following"], true);
            write_frame(&mut peer, snapshot(10, 5, "unsupported version")).await;
            write_frame(&mut peer, snapshot(11, 6, "fresh")).await;
            peer
        });
        let (owner, state) = connection.fresh_state(&"t".to_string()).await.unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(state["title"], "fresh");
        drop(host.await.unwrap());
    }

    #[tokio::test]
    async fn an_interrupted_frame_write_retires_the_stream() {
        let (mut connection, mut peer) = connection();
        let huge = json!({"pad":"x".repeat(16 * 1024 * 1024)});
        let error = connection.write(huge).await.unwrap_err();
        assert!(error.contains("timeout"), "{error}");
        let drain = tokio::spawn(async move {
            let mut sink = vec![0; 1 << 20];
            while peer.read(&mut sink).await.is_ok_and(|n| n > 0) {}
        });
        assert!(
            connection.write(json!({"type":"request"})).await.is_err(),
            "a frame was appended after a partial frame"
        );
        assert!(connection.broken);
        drop(connection);
        drain.await.unwrap();
    }

    #[test]
    fn detaching_clears_the_desktop_catalog() {
        let catalog = Arc::new(Mutex::new(HashMap::new()));
        catalog
            .lock()
            .unwrap()
            .insert("t".to_string(), json!({"id":"codex/t","_desktop":true}));
        let connected = Arc::new(AtomicBool::new(true));
        let (tx, _rx) = mpsc::channel(1);
        let client = Client {
            tx,
            catalog: catalog.clone(),
            connected: connected.clone(),
        };
        drop(Attached { connected, catalog });
        assert!(!client.connected());
        assert!(
            client.list().is_empty(),
            "stale Desktop rows survived detach"
        );
    }

    #[tokio::test]
    async fn unfollow_retires_heavy_state_and_resolves_waits_but_keeps_the_row() {
        let (mut connection, mut peer) = connection();
        connection.owners.insert("t".into(), "owner".into());
        connection
            .states
            .insert("t".into(), (1, json!({"title":"cached"})));
        connection.completed.insert(
            ("t".into(), "owner".into(), "turn".into()),
            crate::proto::now_ms(),
        );
        connection.waiting.insert(
            ("t".into(), "owner".into(), "pending".into()),
            vec![("mid".into(), crate::proto::now_ms())],
        );
        connection.following.insert(
            "t".into(),
            tokio::time::Instant::now() + Duration::from_secs(300),
        );
        connection
            .catalog
            .lock()
            .unwrap()
            .insert("t".into(), json!({"id":"codex/t"}));
        connection.unfollow("t").await;
        // The heavyweight state and the never-resolvable wait are retired;
        // the catalog row and owner stay so the thread remains addressable.
        assert!(connection.states.is_empty());
        assert!(connection.completed.is_empty());
        assert!(connection.waiting.is_empty());
        assert_eq!(connection.owners["t"], "owner");
        assert!(connection.catalog.lock().unwrap().contains_key("t"));
        let frame = read_frame(&mut peer).await;
        assert_eq!(frame["method"], "thread-stream-following-changed");
        assert_eq!(frame["params"]["following"], false);
    }

    #[tokio::test]
    async fn late_events_while_unfollowed_refresh_the_row_without_resurrecting_correlation() {
        let (mut connection, mut peer) = connection();
        connection.owners.insert("t".into(), "owner".into());
        connection
            .states
            .insert("t".into(), (1, json!({"title":"cached"})));
        connection.following.insert(
            "t".into(),
            tokio::time::Instant::now() + Duration::from_secs(300),
        );
        connection.unfollow("t").await;
        let drained = read_frame(&mut peer).await;
        assert_eq!(drained["params"]["following"], false);
        // A late event for the still-known thread refreshes the advertised
        // state but never resurrects retired correlation entries.
        let event = json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner","version":11,
            "params":{"conversationId":"t","hostId":"local","change":{"type":"snapshot","revision":2,
            "conversationState":{"title":"refreshed","threadRuntimeStatus":{"type":"idle"},
            "turnHistory":{"kind":"canonical","history":{"entitiesByKey":{"turn-key":{"turnId":"turn-A","status":"completed"}}}}}}}});
        assert!(connection.broadcast(&event).await.unwrap());
        assert_eq!(connection.states["t"].1["title"], "refreshed");
        assert!(connection.catalog.lock().unwrap().contains_key("t"));
        assert!(
            connection
                .completed
                .contains_key(&("t".into(), "owner".into(), "turn-A".into())),
            "new completions still correlate"
        );
    }

    #[tokio::test]
    async fn discovery_failure_retires_cached_thread_state() {
        let (mut connection, mut peer) = connection();
        connection.owners.insert("t".into(), "owner-A".into());
        connection
            .states
            .insert("t".into(), (3, json!({"title":"cached"})));
        connection
            .catalog
            .lock()
            .unwrap()
            .insert("t".into(), json!({"id":"codex/t"}));
        let host = tokio::spawn(async move {
            let discovery = read_frame(&mut peer).await;
            write_frame(&mut peer, json!({"type":"response","requestId":discovery["requestId"],
                "resultType":"error","method":"thread-owner-discovery","error":"fixture owner unavailable"})).await;
            peer
        });
        let error = connection.fresh_state(&"t".to_string()).await.unwrap_err();
        assert!(error.contains("owner unavailable"), "{error}");
        assert!(connection.owners.is_empty());
        assert!(connection.states.is_empty());
        assert!(connection.catalog.lock().unwrap().is_empty());
        drop(host.await.unwrap());
    }

    #[tokio::test]
    async fn owner_change_resolves_previous_waits_and_retires_state() {
        let (mut connection, mut peer) = connection();
        connection.owners.insert("t".into(), "owner-A".into());
        connection
            .states
            .insert("t".into(), (3, json!({"title":"cached"})));
        connection.waiting.insert(
            ("t".into(), "owner-A".into(), "turn-1".into()),
            vec![("mid-1".into(), crate::proto::now_ms())],
        );
        let host = tokio::spawn(async move {
            let discovery = read_frame(&mut peer).await;
            write_frame(&mut peer, json!({"type":"response","requestId":discovery["requestId"],
                "resultType":"success","method":"thread-owner-discovery","handledByClientId":"owner-B"})).await;
            assert_eq!(read_frame(&mut peer).await["params"]["following"], true);
            write_frame(&mut peer, json!({"type":"broadcast","method":"thread-stream-state-changed","sourceClientId":"owner-B","version":11,
                "params":{"conversationId":"t","hostId":"local","change":{"type":"snapshot","revision":4,
                "conversationState":{"title":"fresh","threadRuntimeStatus":{"type":"idle"}}}}})).await;
            peer
        });
        let (owner, state) = connection.fresh_state(&"t".to_string()).await.unwrap();
        assert_eq!(owner, "owner-B");
        assert_eq!(state["title"], "fresh");
        // The previous owner's unresolved wait was settled unknown and every
        // stale fact about the thread is gone.
        assert!(connection.waiting.is_empty());
        assert_eq!(connection.owners["t"], "owner-B");
        drop(host.await.unwrap());
    }

    #[tokio::test]
    async fn correlation_state_expires_and_bounds_with_unknown_outcome() {
        let (mut connection, _peer) = connection();
        let now = crate::proto::now_ms();
        let key = |thread: &str| (thread.to_string(), "owner".to_string(), "turn".to_string());
        connection
            .completed
            .insert(key("stale-completed"), now - CORRELATION_TTL_MS - 1);
        connection.waiting.insert(
            key("expired-wait"),
            vec![
                ("fresh-mid".into(), now),
                ("stale-mid".into(), now - CORRELATION_TTL_MS - 1),
            ],
        );
        let expired = connection.prune_correlation(now);
        assert_eq!(expired, vec![("stale-mid".into(), "expired-wait".into())]);
        assert!(connection.completed.is_empty());
        assert_eq!(connection.waiting[&key("expired-wait")].len(), 1);
        // Completed early-turn markers evict the oldest entry, never the
        // freshest marker that a subsequent send still needs to observe.
        for index in 0..=CORRELATION_LIMIT as u64 {
            connection.completed.insert(
                ("completed".into(), format!("owner-{index}"), "turn".into()),
                now + index,
            );
        }
        connection.prune_correlation(now);
        assert_eq!(connection.completed.len(), CORRELATION_LIMIT);
        assert!(!connection.completed.contains_key(&(
            "completed".into(),
            "owner-0".into(),
            "turn".into()
        )));
        assert!(connection.completed.contains_key(&(
            "completed".into(),
            format!("owner-{CORRELATION_LIMIT}"),
            "turn".into()
        )));
        connection.completed.clear();
        // Size bound: the oldest waiting mids beyond the cap are evicted and
        // reported for an explicit unknown outcome.
        connection.waiting.clear();
        connection.waiting.insert(
            key("bounded"),
            (0..CORRELATION_LIMIT as u64 + 5)
                .map(|index| (format!("m{index}"), now + index))
                .collect(),
        );
        let evicted = connection.prune_correlation(now);
        assert_eq!(evicted.len(), 5, "{evicted:?}");
        assert!(evicted
            .iter()
            .all(|(mid, thread)| { thread == "bounded" && mid.starts_with('m') }));
        assert_eq!(connection.waiting[&key("bounded")].len(), CORRELATION_LIMIT);
    }
}
