use super::*;

#[tokio::test]
async fn tls_url_answered_by_cleartext_http_fails_without_sending_the_token_in_clear() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut tls, _) = listener.accept().await.unwrap();
            let mut record = [0u8; 5];
            tls.read_exact(&mut record).await.unwrap();
            assert_eq!(record[0], 22, "a wss:// dial must only ever start with TLS");
            let mut hello = vec![0; u16::from_be_bytes([record[3], record[4]]) as usize];
            tls.read_exact(&mut hello).await.unwrap();
            tls.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            tls.shutdown().await.unwrap();
        }
        tokio::time::timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err()
    });
    for scheme in ["wss", "https"] {
        let config = Config {
            server: format!("{scheme}://{address}"),
            token: "fixture-secret".into(),
            ..Config::default()
        };
        let error = tokio::time::timeout(Duration::from_secs(3), dial(&config))
            .await
            .unwrap()
            .err()
            .unwrap_or_else(|| panic!("{scheme}:// downgraded to cleartext"));
        assert!(error.contains("cleartext"), "{error}");
    }
    assert!(
        server.await.unwrap(),
        "a cleartext retry carried the bearer token"
    );
}

fn transport_runtime(cfg: Config) -> Shared {
    transport_runtime_on(cfg, crate::clock::Clock::default())
}
fn transport_runtime_on(cfg: Config, clock: crate::clock::Clock) -> Shared {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{Arc, Mutex},
    };
    Arc::new(Runtime {
        cfg,
        clock,
        identity: Identity {
            device_id: uuid::Uuid::new_v4().to_string(),
            ..Identity::default()
        },
        machine: "transport-fixture".into(),
        stop: tokio::sync::watch::channel(false).0,
        closing: tokio::sync::watch::channel(false).0,
        rpc_tasks: Mutex::new(Vec::new()),
        native: super::super::native::Native::new(),
        legacy_gate: tokio::sync::Mutex::new(()),
        tool_status: tokio::sync::OnceCell::new(),
        mcp_url: Mutex::new(String::new()),
        state: Mutex::new(super::super::State {
            sessions: HashMap::new(),
            unpublished: HashMap::new(),
            exited: HashSet::new(),
            messages: HashMap::new(),
            history: HashSet::new(),
            pending_local: HashSet::new(),
            pending_remote: HashSet::new(),
            incoming: Default::default(),
            subscriptions: HashMap::new(),
            idle_stable: HashMap::new(),
            next_epoch: 0,
            next_conn: 0,
            events: VecDeque::new(),
            seq: 0,
            remote: Vec::new(),
            relay_tx: None,
            control_outbox: VecDeque::new(),
            relay_message_ids: false,
            relay_generation: 0,
            legacy_active: None,
            pending: HashMap::new(),
            aliases: HashMap::new(),
            mcp_status: String::new(),
            native_status: String::new(),
        }),
    })
}

#[cfg(unix)]
#[tokio::test]
async fn stalled_real_relay_writer_fails_after_original_ten_second_write_deadline() {
    use std::os::fd::AsRawFd;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, hold) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let registration = socket.next().await.unwrap().unwrap();
        assert!(
            matches!(registration,Message::Text(ref text) if serde_json::from_str::<Value>(text).unwrap()["type"]=="register")
        );
        let size: libc::c_int = 1024;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.get_ref().as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        socket
            .send(Message::Text(
                json!({"type":"welcome","machine_id":"transport-fixture","send_message_id_v1":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let _ = hold.await;
        drop(socket);
    });
    let rt = transport_runtime(Config {
        server: format!("http://{address}"),
        token: "fixture".into(),
        ..Config::default()
    });
    let active = rt.clone();
    let mut task = tokio::spawn(async move {
        session(
            active.clone(),
            &mut tokio::task::JoinSet::new(),
            &delivery_lanes(&active),
            &mut None,
        )
        .await
    });
    let writer = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(writer) = rt.state.lock().unwrap().relay_tx.clone() {
                break writer;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        rt.state.lock().unwrap().relay_message_ids,
        "the fixture welcome must negotiate send_message_id_v1 like the real relay"
    );
    let first = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let mut fence = Some(first.clone().lock_owned().await);
    // The fenced frame is small so it flushes regardless of the platform's
    // socket buffer sizing; the large frames that follow then stall.
    for size in std::iter::once(1_000).chain(std::iter::repeat_n(700_000, 16)) {
        writer.send(super::super::Push{value:json!({"type":"send","id":uuid::Uuid::new_v4().to_string(),"message_id":uuid::Uuid::new_v4().to_string(),"to":"pi/target","body":"x".repeat(size)}),written:None,fence:fence.take()}).await.unwrap();
    }
    drop(tokio::time::timeout(Duration::from_secs(2),first.lock()).await.expect("the physical write fence must prove at least one legal frame flushed before stalling"));
    let stalled = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(20), &mut task).await;
    let waited = stalled.elapsed();
    if outcome.is_err() {
        task.abort();
        let _ = task.await;
    }
    let _ = release.send(());
    server.await.unwrap();
    assert!(
        matches!(&outcome,Ok(Ok(Err(error))) if error.to_string().contains("write timed out")),
        "the physical writer must close its session after Go WriteWait=10s, got {outcome:?}"
    );
    assert!(
        waited >= Duration::from_secs(9),
        "the writer closed after {waited:?}, before the 10s write deadline"
    );
}

#[tokio::test]
async fn relay_tcp_endpoint_preserves_go_default_nodelay() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    });
    let config = Config {
        server: format!("http://{address}"),
        token: "fixture".into(),
        ..Config::default()
    };
    let socket = dial(&config).await.unwrap();
    let _server = server.await.unwrap();
    let MaybeTlsStream::Plain(stream) = socket.get_ref() else {
        panic!("plain fixture unexpectedly negotiated TLS")
    };
    assert!(
        stream.nodelay().unwrap(),
        "Go net.TCPConn defaults to TCP_NODELAY; snapshot + send must not wait for delayed ACKs"
    );
}

#[tokio::test]
async fn relay_send_rejects_a_body_whose_encoded_frame_exceeds_the_relay_frame_limit() {
    let rt = transport_runtime(Config::default());
    let (writer, mut wire) = mpsc::channel(8);
    {
        let mut state = rt.state.lock().unwrap();
        state.relay_tx = Some(writer);
        state.relay_message_ids = true;
    }
    // Each quote doubles when JSON-escaped, so the raw body fits but the frame does not.
    let body = "\"".repeat(MAX_FRAME / 2 + 1);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        send(
            &rt,
            "pi/origin",
            "pi/remote",
            &body,
            "",
            "oversized-id",
            None,
        ),
    )
    .await
    .expect("oversized relay send waited for a reply instead of failing");
    assert!(result.unwrap_err().contains("too large"));
    assert!(
        wire.try_recv().is_err(),
        "oversized send reached the relay writer"
    );
}

#[tokio::test]
async fn mcp_rejects_bodies_larger_than_one_relay_frame_at_ingress() {
    let rt = transport_runtime(Config {
        mcp_listen: "127.0.0.1:0".into(),
        ..Config::default()
    });
    let server = super::super::http::start(rt.clone()).await.unwrap();
    let url = rt.mcp_url.lock().unwrap().clone();
    let response = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(vec![b' '; MAX_FRAME + 1])
        .send()
        .await
        .unwrap();
    server.abort();
    assert_eq!(response.status(), 413);
}

type Peer = WebSocketStream<tokio::net::TcpStream>;
async fn welcomed_peer(listener: &tokio::net::TcpListener) -> Peer {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    assert_eq!(peer_frame(&mut socket, |_| true).await["type"], "register");
    socket
        .send(Message::Text(
            json!({"type":"welcome","machine_id":"transport-fixture","send_message_id_v1":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket
}
async fn peer_frame(socket: &mut Peer, wanted: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let frame: Value = serde_json::from_str(&text).unwrap();
                if wanted(&frame) {
                    return frame;
                }
            }
        }
    })
    .await
    .expect("relay peer never received the expected frame")
}
async fn peer_send(socket: &mut Peer, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn remote_messages_refuse_rejects_every_relay_initiated_operation_and_delivery() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rt = transport_runtime(Config {
        server: format!("http://{}", listener.local_addr().unwrap()),
        token: "fixture".into(),
        remote_messages: "refuse".into(),
        ..Config::default()
    });
    let active = rt.clone();
    let task = tokio::spawn(async move {
        session(
            active.clone(),
            &mut tokio::task::JoinSet::new(),
            &delivery_lanes(&active),
            &mut None,
        )
        .await
    });
    let mut peer = welcomed_peer(&listener).await;
    for op in [
        "status",
        "workspace_list",
        "workspace_create",
        "workspace_close",
        "session_create",
        "session_close",
        "agent_prompt",
        "agent_keys",
        "agent_read",
        "agent_wait",
    ] {
        let id = format!("call-{op}");
        peer_send(&mut peer, json!({"type":op,"id":id,"session_id":"pi/local","workspace_id":"w1","keys":["enter"],"body":"typed","name":"n","cwd":"/","agent":"pi"})).await;
        let reply = peer_frame(&mut peer, |frame| frame["id"] == id).await;
        assert_eq!(reply["type"], "error", "{op}: {reply}");
        assert!(
            str_field(&reply, "message").contains("remote_messages=refuse"),
            "{op} ran under remote_messages=refuse: {reply}"
        );
    }
    peer_send(&mut peer, json!({"type":"deliver","session_id":"pi/local","from":"pi/remote","message_id":"refused-id","body":"typed"})).await;
    let receipt = peer_frame(&mut peer, |frame| frame["type"] == "receipt").await;
    assert_eq!(receipt["message_id"], "refused-id");
    assert_eq!(receipt["status"], "refused");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn delivery_lanes_keep_per_target_order_and_cap_concurrency() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    let log = Arc::new(Mutex::new(Vec::<(String, &'static str)>::new()));
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (release, gate) = tokio::sync::watch::channel(false);
    let lanes = {
        let (log, running, peak) = (log.clone(), running.clone(), peak.clone());
        Lanes::new(
            3,
            4,
            16,
            Arc::new(move |item: String| {
                let (log, running, peak, mut gate) =
                    (log.clone(), running.clone(), peak.clone(), gate.clone());
                Box::pin(async move {
                    peak.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    log.lock().unwrap().push((item.clone(), "start"));
                    let _ = gate.wait_for(|open| *open).await;
                    tokio::task::yield_now().await;
                    log.lock().unwrap().push((item, "end"));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
            }),
        )
    };
    for n in 0..4 {
        lanes.push("pi/a", format!("a{n}")).unwrap();
    }
    assert_eq!(
        lanes.push("pi/a", "a-overflow".into()),
        Err("a-overflow".into()),
        "a full per-target lane must reject instead of growing"
    );
    for key in 0..8 {
        lanes
            .push(&format!("pi/k{key}"), format!("k{key}"))
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(running.load(Ordering::SeqCst), 3);
    release.send_replace(true);
    tokio::time::timeout(Duration::from_secs(3), async {
        while log.lock().unwrap().len() < 24 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        peak.load(Ordering::SeqCst) <= 3,
        "deliveries exceeded the worker cap"
    );
    let lane_a: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|(item, _)| item.starts_with('a'))
        .cloned()
        .collect();
    let expected: Vec<_> = (0..4)
        .flat_map(|n| [(format!("a{n}"), "start"), (format!("a{n}"), "end")])
        .collect();
    assert_eq!(
        lane_a, expected,
        "one target's deliveries overlapped or reordered"
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !lanes.inner.queues.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle lanes must release their table entry");
    lanes.push("pi/a", "a-after".into()).unwrap();
}

#[tokio::test]
async fn idle_control_backlog_never_rejects_replies_and_keeps_their_order() {
    let (queue, mut receiver) = ordered_queue();
    for n in 0..IDLE_CONTROL_QUEUE {
        queue.control(json!({"n":n})).unwrap();
    }
    assert!(queue.control(json!({"n":"overflow"})).is_err());
    let (reply, answered) = oneshot::channel();
    queue.reply(json!({"id":"ack"}), reply);
    for n in 0..IDLE_CONTROL_QUEUE {
        match receiver.recv().await.unwrap() {
            OrderedIncoming::Control(frame) => assert_eq!(frame["n"], n),
            OrderedIncoming::Reply(..) => panic!("reply overtook an earlier idle control"),
        }
    }
    let next = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("reply was rejected by the control backlog");
    let OrderedIncoming::Reply(frame, reply) = next.unwrap() else {
        panic!("reply lost behind the control backlog")
    };
    reply.send(frame).unwrap();
    assert_eq!(answered.await.unwrap()["id"], "ack");
    queue.control(json!({"n":"drained"})).unwrap();
}

#[cfg(feature = "test-clock")]
async fn reconnect_wait(clock: &crate::clock::Clock) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(timer) = clock
            .manual()
            .unwrap()
            .status()
            .pending
            .into_iter()
            .find(|timer| timer.label == "intercom-relay-reconnect" && timer.due_in_ms > 0)
        {
            return timer.due_in_ms;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reconnect timer never armed"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[cfg(feature = "test-clock")]
#[tokio::test]
async fn relay_that_welcomes_then_drops_keeps_backing_off_until_a_session_stays_healthy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let clock = crate::clock::Clock::manual_for_test();
    let rt = transport_runtime_on(
        Config {
            server: format!("http://{}", listener.local_addr().unwrap()),
            token: "fixture".into(),
            ..Config::default()
        },
        clock.clone(),
    );
    let task = tokio::spawn(run(rt.clone()));
    let mut waits = Vec::new();
    for _ in 0..3 {
        let mut peer = welcomed_peer(&listener).await;
        peer.close(None).await.unwrap();
        let wait = reconnect_wait(&clock).await;
        waits.push(wait);
        clock.manual().unwrap().advance(wait);
    }
    assert!(
        waits[2] >= 3000,
        "welcome-then-drop reset the backoff into a hot reconnect loop: {waits:?}"
    );
    let mut peer = welcomed_peer(&listener).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while rt.state.lock().unwrap().relay_tx.is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    clock
        .manual()
        .unwrap()
        .advance(HEALTHY_SESSION.as_millis() as u64);
    peer.close(None).await.unwrap();
    let wait = reconnect_wait(&clock).await;
    assert!(
        (750..=1250).contains(&wait),
        "a session that stayed healthy must reset the backoff: {wait}"
    );
    rt.stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}
