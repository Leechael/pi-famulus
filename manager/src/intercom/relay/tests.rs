use super::*;

#[tokio::test]
async fn dial_auto_retries_only_actual_cleartext_http_and_caches_the_plain_server() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tls, _) = listener.accept().await.unwrap();
        let mut record = [0u8; 5];
        tls.read_exact(&mut record).await.unwrap();
        assert_eq!(
            record[0], 22,
            "first attempt must really be TLS, not a fixture redirect"
        );
        let mut hello = vec![0; u16::from_be_bytes([record[3], record[4]]) as usize];
        tls.read_exact(&mut hello).await.unwrap();
        tls.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        tls.shutdown().await.unwrap();
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut prefix = [0; 4];
            assert_eq!(stream.peek(&mut prefix).await.unwrap(), 4);
            assert_eq!(
                &prefix, b"GET ",
                "cached server must skip another TLS attempt"
            );
            sockets.push(
                tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     response| {
                        assert_eq!(request.uri().path(), "/v1/relay");
                        assert_eq!(request.uri().query(), None);
                        assert_eq!(request.headers()["authorization"], "Bearer fixture");
                        Ok(response)
                    },
                )
                .await
                .unwrap(),
            );
        }
        sockets
    });
    let config = Config {
        server: format!("https://{address}/discard?discard=yes#discard"),
        token: "fixture".into(),
        ..Config::default()
    };
    let first = tokio::time::timeout(Duration::from_secs(3), dial(&config))
        .await
        .unwrap();
    let first = match first {
        Ok(socket) => socket,
        Err(error) => {
            server.abort();
            let _ = server.await;
            panic!(
                "real cleartext HTTP TLS record header must invoke Go DialAuto fallback: {error}"
            );
        }
    };
    let second = tokio::time::timeout(Duration::from_secs(3), dial(&config))
        .await
        .unwrap()
        .unwrap();
    let sockets = server.await.unwrap();
    drop((first, second, sockets));
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
                json!({"type":"welcome","features":["send_message_id_v1"]})
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
    let first = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let mut fence = Some(first.clone().lock_owned().await);
    for _ in 0..16 {
        writer.send(super::super::Push{value:json!({"type":"send","id":uuid::Uuid::new_v4().to_string(),"message_id":uuid::Uuid::new_v4().to_string(),"to":"pi/target","body":"x".repeat(700_000)}),written:None,fence:fence.take()}).await.unwrap();
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
