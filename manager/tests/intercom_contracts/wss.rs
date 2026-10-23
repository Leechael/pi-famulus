use super::*;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

struct WssRelay {
    address: SocketAddr,
    requests: mpsc::Receiver<Value>,
    handshakes: mpsc::Receiver<bool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    roots: PathBuf,
}

fn openssl(root: &std::path::Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("OpenSSL fixture tool is required");
    assert!(
        output.status.success(),
        "OpenSSL certificate fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl WssRelay {
    fn new(fixture: &Fixture) -> Self {
        let root = fixture.root.join("tls");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("root.cnf"),"[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\nCN=Intercom fixture root\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n").unwrap();
        fs::write(root.join("leaf.cnf"),"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n").unwrap();
        openssl(
            &root,
            &[
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "root.key", "-out",
                "root.pem", "-days", "1", "-config", "root.cnf",
            ],
        );
        openssl(
            &root,
            &[
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "leaf.key",
                "-out",
                "leaf.csr",
                "-subj",
                "/CN=localhost",
            ],
        );
        openssl(
            &root,
            &[
                "x509",
                "-req",
                "-in",
                "leaf.csr",
                "-CA",
                "root.pem",
                "-CAkey",
                "root.key",
                "-CAcreateserial",
                "-out",
                "leaf.pem",
                "-days",
                "1",
                "-extfile",
                "leaf.cnf",
            ],
        );
        openssl(
            &root,
            &[
                "x509", "-in", "leaf.pem", "-outform", "DER", "-out", "leaf.der",
            ],
        );
        openssl(
            &root,
            &[
                "pkcs8", "-topk8", "-nocrypt", "-in", "leaf.key", "-outform", "DER", "-out",
                "leaf.pk8",
            ],
        );
        let certificate =
            rustls::pki_types::CertificateDer::from(fs::read(root.join("leaf.der")).unwrap());
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(fs::read(root.join("leaf.pk8")).unwrap());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key.into())
        .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (tx, requests) = mpsc::channel();
        let (events, handshakes) = mpsc::channel();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    // BSD-derived systems inherit O_NONBLOCK from the listener.
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let tls = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(config.clone()).unwrap(),
                    stream,
                );
                let mut ws = match tokio_tungstenite::tungstenite::accept(tls) {
                    Ok(ws) => {
                        let _ = events.send(true);
                        ws
                    }
                    Err(_) => {
                        let _ = events.send(false);
                        continue;
                    }
                };
                while !stopped.load(Ordering::Relaxed) {
                    let frame = match ws.read() {
                        Ok(frame) => frame,
                        Err(tokio_tungstenite::tungstenite::Error::Io(e))
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(_) => break,
                    };
                    if !frame.is_text() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<Value>(frame.to_text().unwrap())
                    else {
                        continue;
                    };
                    let _ = tx.send(request.clone());
                    let response = match request["type"].as_str().unwrap_or_default() {
                        "register" => {
                            json!({"type":"welcome","machine_id":request["device_id"],"send_message_id_v1":true})
                        }
                        "list" => {
                            json!({"type":"list","id":request["id"],"sessions":[{"id":"pi/wss-remote","harness":"pi","name":"TLS relay peer","status":"busy","machine_id":"fixture-remote"}]})
                        }
                        "ping" => json!({"type":"pong"}),
                        _ => continue,
                    };
                    if ws
                        .send(tokio_tungstenite::tungstenite::Message::text(
                            response.to_string(),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            address,
            requests,
            handshakes,
            stop,
            worker: Some(worker),
            roots: root.join("root.pem"),
        }
    }

    fn start(
        &self,
        fixture: &mut Fixture,
        insecure: bool,
        toml_insecure: bool,
        trusted_root: bool,
    ) {
        let config = fs::read_to_string(fixture.root.join("client.toml"))
            .unwrap()
            .replace("local_only = true", "local_only = false");
        fs::write(
            fixture.root.join("client.toml"),
            format!(
                "{config}\nserver='wss://{}/ws'\ntoken='fixture-token'\ntls_insecure={}\n",
                self.address,
                insecure || toml_insecure
            ),
        )
        .unwrap();
        let mut env = Vec::new();
        let (roots, absent) = (
            self.roots.to_string_lossy().into_owned(),
            fixture.root.join("absent-cert-dir").to_string_lossy().into_owned(),
        );
        if trusted_root {
            env.push(("SSL_CERT_FILE", roots.as_str()));
            env.push(("SSL_CERT_DIR", absent.as_str()));
        }
        fixture.spawn_serve(&[], &env);
        fixture.wait_supervised();
    }

    fn registered(&self, fixture: &Fixture) {
        assert!(
            self.handshakes
                .recv_timeout(TIMEOUT)
                .expect("TLS13/WebSocket handshake"),
            "TLS13 connection was rejected: {}",
            fixture.log()
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let request = self
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("WSS registration before deadline");
            if request["type"] == "register" {
                break;
            }
        }
        fixture.wait_session("pi/wss-remote");
    }
}
impl Drop for WssRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
fn wss_default_rejects_fixture_ca_without_sending_registration() {
    let mut fixture = Fixture::new();
    let relay = WssRelay::new(&fixture);
    relay.start(&mut fixture, false, false, false);
    assert!(
        !relay
            .handshakes
            .recv_timeout(TIMEOUT)
            .expect("untrusted TLS handshake outcome"),
        "default TLS configuration trusted a private fixture CA"
    );
    assert!(
        relay
            .requests
            .recv_timeout(Duration::from_millis(200))
            .is_err(),
        "credentials escaped to untrusted relay"
    );
}

#[test]
fn wss_insecure_cli_explicitly_allows_fixture_ca_and_registers() {
    let mut fixture = Fixture::new();
    let relay = WssRelay::new(&fixture);
    relay.start(&mut fixture, true, false, false);
    relay.registered(&fixture);
}

#[test]
fn wss_tls_insecure_toml_explicitly_allows_fixture_ca_and_registers() {
    let mut fixture = Fixture::new();
    let relay = WssRelay::new(&fixture);
    relay.start(&mut fixture, false, true, false);
    relay.registered(&fixture);
}

#[test]
fn wss_explicit_trusted_roots_registers_without_disabling_verification() {
    let mut fixture = Fixture::new();
    let relay = WssRelay::new(&fixture);
    relay.start(&mut fixture, false, false, true);
    relay.registered(&fixture);
}
