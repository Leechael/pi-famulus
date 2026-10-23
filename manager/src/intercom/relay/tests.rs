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
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{Arc, Mutex},
    };
    Arc::new(Runtime {
        cfg,
        clock: crate::clock::Clock::default(),
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
    let mut task =
        tokio::spawn(
            async move { session(active, &mut tokio::task::JoinSet::new(), &mut false).await },
        );
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
    let outcome = tokio::time::timeout(Duration::from_secs(12), &mut task).await;
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
