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
#[cfg(unix)]
struct Attached(Arc<AtomicBool>);
#[cfg(unix)]
impl Drop for Attached {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
    frames: mpsc::Receiver<Result<Value, String>>,
    _reader: Reader,
    source: String,
    generation: String,
    owners: HashMap<String, String>,
    states: HashMap<String, (u64, Value)>,
    completed: HashSet<(String, String, String)>,
    waiting: HashMap<(String, String, String), Vec<String>>,
    following: HashMap<String, tokio::time::Instant>,
    owner: Weak<super::super::Runtime>,
    catalog: Arc<Mutex<HashMap<String, Value>>>,
}
#[cfg(unix)]
struct Reader(tokio::task::JoinHandle<()>);
#[cfg(unix)]
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(super) async fn start(rt: &Shared) {
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
        let mut stop = rt.stop.subscribe();
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
                    let _attached = Attached(attached.clone());
                    let mut cleanup = tokio::time::interval(Duration::from_secs(30));
                    loop {
                        tokio::select! {
                            _=stop.changed()=>return,
                            command=rx.recv()=>{let Some(mut command)=command else{return};let result=if command.observe{tokio::select!{_ = command.reply.closed()=>continue,result=connection.idle_observation(&command.thread)=>result}}else{connection.send(&mut command).await.map(|state|(state,String::new()))};let _=command.reply.send(result);},
                            frame=connection.frames.recv()=>{match frame{Some(Ok(value))=>{if connection.broadcast(&value).await.is_err(){break;}},_=>break}},
                            _=cleanup.tick()=>{let expired=connection.following.iter().filter(|(thread,deadline)|**deadline<=tokio::time::Instant::now()&&!connection.waiting.keys().any(|(t,_,_)|t==*thread)).map(|(thread,_)|thread.clone()).collect::<Vec<_>>();for thread in expired{connection.unfollow(&thread).await;}}
                        }
                    }
                    drop(connection);
                }
                tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(Duration::from_secs(3))=>{},command=rx.recv()=>{if let Some(command)=command{let _=command.reply.send(Err("Desktop IPC not connected".into()));}else{return;}}}
            }
        });
        rt.native.tasks.lock().unwrap().push(task);
    }
    #[cfg(not(unix))]
    {
        let _ = rt;
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
    let mut connection = Connection {
        writer,
        frames,
        _reader: reader,
        source: "initializing-client".into(),
        generation: uuid::Uuid::new_v4().to_string(),
        owners: HashMap::new(),
        states: HashMap::new(),
        completed: HashSet::new(),
        waiting: HashMap::new(),
        following: HashMap::new(),
        owner,
        catalog,
    };
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
impl Connection {
    async fn write(&mut self, value: Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        tokio::time::timeout(Duration::from_secs(5), async {
            self.writer.write_u32_le(bytes.len() as u32).await?;
            self.writer.write_all(&bytes).await
        })
        .await
        .map_err(|_| "Desktop write outcome unknown: timeout".to_string())?
        .map_err(|e| format!("Desktop write outcome unknown: {e}"))
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
    }
    async fn broadcast(&mut self, event: &Value) -> Result<(), String> {
        if str_field(event, "type") == "client-discovery-request" {
            return self.write(json!({"type":"client-discovery-response","requestId":event["requestId"],"response":{"canHandle":false}})).await;
        }
        if str_field(event, "method") != "thread-stream-state-changed" || event["version"] != 11 {
            return Ok(());
        }
        if event["targetClientIds"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty() && !ids.iter().any(|id| id == &self.source))
        {
            return Ok(());
        }
        let params = &event["params"];
        let thread = str_field(params, "conversationId");
        let owner = str_field(event, "sourceClientId");
        if thread.is_empty()
            || str_field(params, "hostId") != "local"
            || self.owners.get(thread).is_none_or(|id| id != owner)
        {
            return Ok(());
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
                    return Ok(());
                }
                let mut state = self.states[thread].1.clone();
                for patch in change["patches"].as_array().into_iter().flatten() {
                    if apply_patch(&mut state, patch).is_err() {
                        return Ok(());
                    }
                }
                state
            }
            _ => return Ok(()),
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
                    self.completed.insert(key.clone());
                    if let Some(messages) = self.waiting.remove(&key) {
                        if let Some(runtime) = self.owner.upgrade() {
                            for mid in messages {
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
        Ok(())
    }
    async fn fresh_state(&mut self, thread: &String) -> Result<(String, Value), String> {
        let discovered = self
            .request(
                "thread-owner-discovery",
                json!({"hostId":"local","conversationId":thread}),
                "",
                1,
                None,
            )
            .await?;
        let owner = str_field(&discovered, "handledByClientId").to_string();
        if owner.is_empty() {
            return Err("Desktop owner discovery returned no owner".into());
        }
        if self
            .owners
            .get(thread)
            .is_some_and(|previous| previous != &owner)
        {
            self.states.remove(thread);
            self.waiting.retain(|(t, _, _), _| t != thread);
            self.completed.retain(|(t, _, _)| t != thread);
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
            self.broadcast(&event).await?;
            if fresh {
                if let Some((_, state)) = self.states.get(thread) {
                    break state.clone();
                }
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
                    ))
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
        if self.completed.contains(&key) {
            self.unfollow(thread).await;
            return Ok("read".into());
        }
        self.waiting
            .entry(key)
            .or_default()
            .push(command.mid.clone());
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
