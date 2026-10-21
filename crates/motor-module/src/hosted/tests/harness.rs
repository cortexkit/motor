use super::super::*;
use crate::{Connector, Duplex};
use motor_lease::{
    fake::{BrokerRequest, FakeBroker, FakeConfig},
    *,
};
use motor_protocol::*;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use x509_cert::der::{Decode, Encode};

pub struct Scratch(pub std::path::PathBuf);
impl Scratch {
    pub fn new() -> Self {
        let path =
            cortexkit_test_support::scratch_root().join(format!("motor-hosted-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct NoSsh(pub AtomicUsize);
impl Connector for NoSsh {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(io::Error::other("SSH must not open")) })
    }
}

#[derive(Clone, Copy)]
pub enum Behavior {
    Normal,
    CompleteRun,
    ConnectFailure,
    DropHave,
    Refuse(&'static str),
    RefuseHello(&'static str),
    DropTerminal,
    WrongHost,
    DropRunAck,
    WrongHello,
    ReturnCapFailure,
    /// The run connection carries `Started` and the first output record; the
    /// job then ends on the host, which reports its usage to the broker and
    /// drops the connection before the rest of the output and the terminal
    /// reach ck-motor. An attach replays everything from its cursor.
    SettleThenDrop,
}

/// The host key the harness registers with its fake broker, so the fake host
/// can sign its own usage report.
pub const HOST_MACHINE_SEED: [u8; 32] = [11; 32];
/// Exit code and output of the `SettleThenDrop` job.
pub const SETTLED_EXIT_CODE: i32 = 41;
pub const SETTLED_OUTPUT: [&[u8]; 2] = [b"before the drop\n", b"after the drop\n"];

type LeaseReplies = Arc<Mutex<VecDeque<(u16, serde_json::Value, &'static str)>>>;

pub struct Harness {
    pub route: HostedJobState,
    pub broker: Arc<Mutex<FakeBroker>>,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub lease_bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    pub lease_replies: LeaseReplies,
    pub tokens: Arc<AtomicUsize>,
    pub iats: Arc<Mutex<Vec<u64>>>,
    pub runs: Arc<AtomicUsize>,
    pub frames: Arc<Mutex<Vec<Frame>>>,
    pub finished: Arc<std::sync::atomic::AtomicBool>,
    pub drop_lease: Arc<std::sync::atomic::AtomicBool>,
    pub release_admitted: Arc<std::sync::atomic::AtomicBool>,
    /// Refusal reasons the host sends, one per run connection and in order,
    /// before it falls back to its behavior.
    pub host_refusals: Arc<Mutex<VecDeque<&'static str>>>,
    pub tasks: Vec<JoinHandle<()>>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Harness {
    pub async fn new(behavior: Behavior) -> Self {
        // A deliberately self-signed certificate for an unrelated DNS name.
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["not-the-host.invalid".into()]).unwrap();
        let cert_der = cert.der().clone();
        let parsed = x509_cert::Certificate::from_der(cert_der.as_ref()).unwrap();
        let pin = motor_lease::base64url(&Sha256::digest(
            parsed
                .tbs_certificate
                .subject_public_key_info
                .to_der()
                .unwrap(),
        ));
        let provider = rustls::crypto::ring::default_provider();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert_der],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let host = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = if matches!(behavior, Behavior::ConnectFailure) {
            "127.0.0.1:1".to_string()
        } else {
            host.local_addr().unwrap().to_string()
        };
        let signing = SigningKey::from_bytes(&[7; 32]);
        let mut keys = KeySet::default();
        keys.refresh(Ok(Jwks {
            keys: vec![JwksKey::from_key("test", &signing.verifying_key())],
        }))
        .unwrap();
        let keys = Arc::new(keys);
        let broker = Arc::new(Mutex::new(
            FakeBroker::new(
                "test",
                FakeConfig {
                    worker_bearer: "hosted-test-bearer".into(),
                    subject: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    placement: Placement {
                        host: "test-host".into(),
                        vm: Some("test-vm".into()),
                    },
                    endpoint,
                    endpoint_spki_sha256: pin,
                    kid: "test".into(),
                    signing_key: signing,
                    announced_keys: Vec::new(),
                },
            )
            .unwrap(),
        ));
        std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
        broker.lock().unwrap().register_host(
            "test-host".into(),
            SigningKey::from_bytes(&HOST_MACHINE_SEED).verifying_key(),
        );
        let tokens = Arc::new(AtomicUsize::new(0));
        let iats = Arc::new(Mutex::new(Vec::new()));
        let runs = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let host_refusals = Arc::new(Mutex::new(VecDeque::<&'static str>::new()));
        let host_task = tokio::spawn({
            let broker = broker.clone();
            let host_refusals = host_refusals.clone();
            let tokens = tokens.clone();
            let iats = iats.clone();
            let runs = runs.clone();
            let frames = frames.clone();
            async move {
                while let Ok((socket, _)) = host.accept().await {
                    let acceptor = acceptor.clone();
                    let keys = keys.clone();
                    let tokens = tokens.clone();
                    let iats = iats.clone();
                    let runs = runs.clone();
                    let frames = frames.clone();
                    let host_refusals = host_refusals.clone();
                    let broker = broker.clone();
                    tokio::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(socket).await else {
                            return;
                        };
                        let Ok(Prelude::LeaseHello {
                            token,
                            requester_jwk,
                            ..
                        }) = super::super::tls::read(&mut tls).await
                        else {
                            return;
                        };
                        tokens.fetch_add(1, Ordering::SeqCst);
                        let purpose = if serde_json::from_slice::<Claims>(
                            &base64::Engine::decode(
                                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                                token.split('.').nth(1).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap()
                        .purpose
                            == Purpose::Run
                        {
                            Purpose::Run
                        } else {
                            Purpose::Attach
                        };
                        let claims =
                            verify_token(&token, &keys, "test-host", None, purpose, now_s())
                                .unwrap();
                        iats.lock().unwrap().push(claims.iat);
                        let challenge = Challenge {
                            nonce: "a".repeat(64),
                            host_id: if matches!(behavior, Behavior::WrongHost) {
                                "wrong-host"
                            } else {
                                "test-host"
                            }
                            .into(),
                            ts: now_s(),
                        };
                        super::super::tls::write(&mut tls, &Prelude::Challenge(challenge.clone()))
                            .await
                            .unwrap();
                        let Ok(Prelude::ChallengeResponse { sig }) =
                            super::super::tls::read(&mut tls).await
                        else {
                            return;
                        };
                        Nonces::default()
                            .verify(&challenge, &claims, &requester_jwk, &sig, now_s())
                            .unwrap();
                        let queued = if purpose == Purpose::Run {
                            host_refusals.lock().unwrap().pop_front()
                        } else {
                            None
                        };
                        if let Some(reason) = queued.or(match behavior {
                            Behavior::Refuse(reason) => Some(reason),
                            _ => None,
                        }) {
                            if purpose == Purpose::Run {
                                super::super::tls::write(
                                    &mut tls,
                                    &Prelude::LeaseRefused {
                                        reason: reason.into(),
                                    },
                                )
                                .await
                                .unwrap();
                                return;
                            }
                        }
                        super::super::tls::write(&mut tls, &Prelude::LeaseOk {})
                            .await
                            .unwrap();
                        let Ok(Some(Frame::Hello { .. })) =
                            motor_protocol::read_frame_async(&mut tls).await
                        else {
                            return;
                        };
                        if let Behavior::RefuseHello(reason) = behavior {
                            super::super::tls::write(
                                &mut tls,
                                &Prelude::LeaseRefused {
                                    reason: reason.into(),
                                },
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        motor_protocol::write_frame_async(
                            &mut tls,
                            &Frame::Hello {
                                return_paths: false,
                                protocol_version: if matches!(behavior, Behavior::WrongHello) {
                                    PROTOCOL_VERSION + 1
                                } else {
                                    PROTOCOL_VERSION
                                },
                                started: true,
                                default_branch_tip: true,
                                gate_jobs: false,
                                platforms: vec!["linux".into()],
                            },
                        )
                        .await
                        .unwrap();
                        while let Ok(Some(frame)) = motor_protocol::read_frame_async(&mut tls).await
                        {
                            frames.lock().unwrap().push(frame.clone());
                            let id = Uuid::parse_str(&claims.jti).unwrap();
                            match frame {
                                Frame::Heartbeat { .. } => continue,
                                Frame::Have { repositories, .. } => {
                                    if matches!(behavior, Behavior::DropHave) {
                                        return;
                                    }
                                    motor_protocol::write_frame_async(
                                        &mut tls,
                                        &Frame::HaveReply {
                                            repositories: repositories
                                                .into_iter()
                                                .map(|mut r| {
                                                    if !matches!(behavior, Behavior::CompleteRun) {
                                                        r.commits.clear();
                                                    }
                                                    r
                                                })
                                                .collect(),
                                            git_dependencies: Some(Vec::new()),
                                        },
                                    )
                                    .await
                                    .unwrap();
                                }
                                Frame::Run(run) => {
                                    runs.fetch_add(1, Ordering::SeqCst);
                                    if matches!(behavior, Behavior::SettleThenDrop) {
                                        settle_then_drop(&mut tls, &broker, &claims, run.network)
                                            .await;
                                        return;
                                    }
                                    if matches!(behavior, Behavior::DropRunAck) {
                                        return;
                                    }
                                    if matches!(behavior, Behavior::CompleteRun) {
                                        let mut accepted = Accepted::new(id, 0);
                                        accepted.network = run.network;
                                        for frame in [
                                            Frame::Accepted(accepted),
                                            Frame::Terminal(TerminalRecord::new(
                                                id,
                                                Outcome::Exit { code: 23 },
                                                0,
                                                0,
                                                0,
                                            )),
                                            Frame::HostedTerminal(HostedTerminal {
                                                job_id: id,
                                                placement: "first_party/test-host".into(),
                                                billed_vcpu_ms: 6001,
                                                max_runtime_s: claims.max_s,
                                            }),
                                        ] {
                                            motor_protocol::write_frame_async(&mut tls, &frame)
                                                .await
                                                .unwrap();
                                        }
                                        return;
                                    }
                                }
                                Frame::Cancel(_) => {
                                    motor_protocol::write_frame_async(
                                        &mut tls,
                                        &Frame::CancelReply(CancelReply::new(id)),
                                    )
                                    .await
                                    .unwrap();
                                    return;
                                }
                                Frame::Attach(attach) => {
                                    if matches!(behavior, Behavior::SettleThenDrop) {
                                        replay_settled(&mut tls, &claims, attach.from_seq).await;
                                        return;
                                    }
                                    if matches!(behavior, Behavior::ReturnCapFailure) {
                                        motor_protocol::write_frame_async(
                                            &mut tls,
                                            &Frame::ReturnResult(ReturnResult {
                                                job_id: id,
                                                seq: 0,
                                                returned_files: None,
                                                return_paths_failed: Some(ReturnPathsFailed {
                                                    measured_bytes: 2,
                                                    cap_bytes: 1,
                                                    cause: ReturnFailureCause::OverCap,
                                                }),
                                            }),
                                        )
                                        .await
                                        .unwrap();
                                    }
                                    let mut terminal = TerminalRecord::new(
                                        id,
                                        Outcome::Exit { code: 23 },
                                        0,
                                        0,
                                        0,
                                    );
                                    if matches!(behavior, Behavior::ReturnCapFailure) {
                                        terminal.ran = Some(Ran::Remote);
                                    }
                                    motor_protocol::write_frame_async(
                                        &mut tls,
                                        &Frame::Terminal(terminal),
                                    )
                                    .await
                                    .unwrap();
                                    if matches!(behavior, Behavior::DropTerminal) {
                                        return;
                                    }
                                    motor_protocol::write_frame_async(
                                        &mut tls,
                                        &Frame::HostedTerminal(HostedTerminal {
                                            job_id: id,
                                            placement: "first_party/test-host".into(),
                                            billed_vcpu_ms: 6001,
                                            max_runtime_s: claims.max_s,
                                        }),
                                    )
                                    .await
                                    .unwrap();
                                    return;
                                }
                                _ => return,
                            }
                        }
                    });
                }
            }
        });
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let broker_url = format!("http://{}", http.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let lease_bodies = Arc::new(Mutex::new(Vec::new()));
        let lease_replies = Arc::new(Mutex::new(
            VecDeque::<(u16, serde_json::Value, &'static str)>::new(),
        ));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drop_lease = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_admitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let http_task = tokio::spawn({
            let broker = broker.clone();
            let requests = requests.clone();
            let lease_bodies = lease_bodies.clone();
            let lease_replies = lease_replies.clone();
            let finished = finished.clone();
            let drop_lease = drop_lease.clone();
            let release_admitted = release_admitted.clone();
            async move {
                while let Ok((mut socket, _)) = http.accept().await {
                    let mut bytes = Vec::new();
                    loop {
                        let mut byte = [0];
                        if socket.read_exact(&mut byte).await.is_err() {
                            break;
                        }
                        bytes.push(byte[0]);
                        if bytes.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&bytes);
                    let path = text
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_string();
                    let length: usize = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    let bearer = text
                        .lines()
                        .find_map(|l| l.strip_prefix("authorization: Bearer "))
                        .map(str::trim)
                        .map(str::to_string);
                    let mut body = vec![0; length];
                    socket.read_exact(&mut body).await.unwrap();
                    requests.lock().unwrap().push(path.clone());
                    let stub = if path == "/v1/leases" {
                        lease_bodies.lock().unwrap().push(body.clone());
                        lease_replies.lock().unwrap().pop_front()
                    } else {
                        None
                    };
                    let mut retry_after = "";
                    let reply = if let Some((status, body, delay)) = stub {
                        retry_after = delay;
                        motor_lease::fake::BrokerReply { status, body }
                    } else if release_admitted.load(Ordering::SeqCst) && path.ends_with("/release")
                    {
                        motor_lease::fake::BrokerReply::error(409, "job_admitted")
                    } else if finished.load(Ordering::SeqCst) && path.ends_with("/reattach") {
                        motor_lease::fake::BrokerReply::error(409, "job_finished")
                    } else {
                        broker.lock().unwrap().handle(
                            BrokerRequest {
                                content_type: Some("application/json"),
                                method: "POST",
                                path: &path,
                                bearer: bearer.as_deref(),
                                host_headers: None,
                                body: &body,
                            },
                            now_s() * 1000,
                        )
                    };
                    if path == "/v1/leases" && drop_lease.swap(false, Ordering::SeqCst) {
                        continue;
                    }
                    let body = serde_json::to_vec(&reply.body).unwrap();
                    let head = format!("HTTP/1.1 {} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: {retry_after}\r\nConnection: close\r\n\r\n", reply.status, body.len());
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            }
        });
        Self {
            route: HostedJobState {
                broker_url,
                token_source: BrokerTokenSource::Fake,
                ..HostedJobState::default()
            },
            broker,
            requests,
            lease_bodies,
            lease_replies,
            tokens,
            iats,
            runs,
            frames,
            finished,
            drop_lease,
            release_admitted,
            host_refusals,
            tasks: vec![host_task, http_task],
        }
    }
    pub fn config(&self) -> Config {
        Config {
            hosted_runners: true,
            runner_instance: "test".into(),
            broker_url: Some(self.route.broker_url.clone()),
            broker_token_source: BrokerTokenSource::Fake,
            ..Config::default()
        }
    }
    pub fn request(path: &Path) -> RunRequest {
        RunRequest::new(
            path.to_str().unwrap(),
            path.to_str().unwrap(),
            path.to_str().unwrap(),
            "true",
        )
        .with_timeout(30)
        .with_weight_hint(2)
    }
}

/// Milliseconds of runtime the `SettleThenDrop` host reports per vCPU; the
/// broker settles `vcpus * SETTLED_RUNTIME_MS` and the replayed
/// `HostedTerminal` bills the same amount.
pub const SETTLED_RUNTIME_MS: u64 = 3000;

/// The `SettleThenDrop` job's sequenced records: `Started`, then one output
/// record per `SETTLED_OUTPUT` entry.
fn settled_records(id: Uuid) -> Vec<(u64, Frame)> {
    let mut records = vec![(
        0,
        Frame::Started {
            job_id: id,
            seq: 0,
            queue_wait_ms: 0,
            started_at_ms: 1_730_000_000_000,
            path_mapping: None,
        },
    )];
    for (seq, bytes) in (1..).zip(SETTLED_OUTPUT) {
        records.push((
            seq,
            Frame::Output(Output::new(
                seq,
                OutputStream::Stdout,
                BytePayload(bytes.to_vec()),
            )),
        ));
    }
    records
}

/// The run connection of `Behavior::SettleThenDrop`: ck-motor receives the
/// acknowledgement, `Started` and the first output record. The job then ends
/// on the host, which reports its usage to the broker (settling the job there)
/// and drops the connection before the last output record and the terminal
/// are delivered.
async fn settle_then_drop<W: tokio::io::AsyncWrite + Unpin>(
    socket: &mut W,
    broker: &Mutex<FakeBroker>,
    claims: &Claims,
    network: Option<Network>,
) {
    let id = Uuid::parse_str(&claims.jti).unwrap();
    let mut accepted = Accepted::new(id, 0);
    accepted.network = network;
    motor_protocol::write_frame_async(socket, &Frame::Accepted(accepted))
        .await
        .unwrap();
    for (_, frame) in settled_records(id).into_iter().take(2) {
        motor_protocol::write_frame_async(socket, &frame)
            .await
            .unwrap();
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let usage = Usage {
        job_id: claims.jti.clone(),
        vcpus: claims.vcpus,
        started_at_ms: now_ms - SETTLED_RUNTIME_MS,
        ended_at_ms: now_ms,
        mem_peak_mib: None,
        egress_bytes: None,
        outcome: UsageOutcome::Exit,
    };
    let body = serde_json::to_vec(&usage).unwrap();
    let path = "/v1/hosts/test-host/usage";
    let headers = HostHeaders::sign(
        "test-host",
        "POST",
        path,
        &body,
        now_ms,
        now_ms * 1000,
        &SigningKey::from_bytes(&HOST_MACHINE_SEED),
    )
    .unwrap();
    let reply = broker.lock().unwrap().handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path,
            bearer: None,
            host_headers: Some(&headers),
            body: &body,
        },
        now_ms,
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.body["settled_vcpu_ms"],
        u64::from(claims.vcpus) * SETTLED_RUNTIME_MS
    );
}

/// An attach to the ended `SettleThenDrop` job: the host replays its retained
/// records from the caller's cursor, then the terminal and the hosted
/// settlement it already reported. It reports no usage again.
async fn replay_settled<W: tokio::io::AsyncWrite + Unpin>(
    socket: &mut W,
    claims: &Claims,
    from_seq: u64,
) {
    let id = Uuid::parse_str(&claims.jti).unwrap();
    for (seq, frame) in settled_records(id) {
        if seq >= from_seq {
            motor_protocol::write_frame_async(socket, &frame)
                .await
                .unwrap();
        }
    }
    for frame in [
        Frame::Terminal(
            TerminalRecord::new(
                id,
                Outcome::Exit {
                    code: SETTLED_EXIT_CODE,
                },
                SETTLED_RUNTIME_MS,
                0,
                0,
            )
            .with_ran(Ran::Remote),
        ),
        Frame::HostedTerminal(HostedTerminal {
            job_id: id,
            placement: "first_party/test-host".into(),
            billed_vcpu_ms: u64::from(claims.vcpus) * SETTLED_RUNTIME_MS,
            max_runtime_s: claims.max_s,
        }),
    ] {
        motor_protocol::write_frame_async(socket, &frame)
            .await
            .unwrap();
    }
}
