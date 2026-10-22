use super::*;
use crate::hosted::{ssh::tests::Sshd, test_http::Http};
use base64::Engine;
use motor_lease::{Claims, Confirmation, Purpose};
use motor_protocol::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Mutex,
};
use tokio::{net::TcpListener, task::JoinHandle};

pub(crate) struct Boat {
    pub config: Config,
    pub requests: test_http::Requests,
    pub runs: Arc<AtomicUsize>,
    pub attaches: Arc<AtomicUsize>,
    pub frames: Arc<Mutex<Vec<Frame>>>,
    pub lost: Arc<AtomicUsize>,
    /// Usable memory the job token states; 0 states the fleet formula.
    pub mem_mib: Arc<AtomicU64>,
    /// Like a real runner, refuse to serve an attach for a job that never
    /// received a Run frame (the connection drops).
    pub strict_attach: Arc<AtomicBool>,
    pub hang_run: Arc<AtomicBool>,
    pub run_refusal: Arc<Mutex<Option<RunRefusal>>>,
    _http: Http,
    _host: JoinHandle<()>,
}
#[derive(Clone)]
pub(crate) struct RunRefusal {
    pub reason: RefusalReason,
    pub detail: String,
    pub after_accept: bool,
}
impl Drop for Boat {
    fn drop(&mut self) {
        self._host.abort();
    }
}
impl Boat {
    pub(crate) async fn new(provisioning: usize, drop_run: bool) -> Self {
        std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
        let initial = Arc::new(Sshd::new().await);
        let resumed = Arc::new(Sshd::new().await);
        let host = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = host.local_addr().unwrap().port();
        let runs = Arc::new(AtomicUsize::new(0));
        let attaches = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let captured = frames.clone();
        let strict_attach = Arc::new(AtomicBool::new(false));
        let hang_run = Arc::new(AtomicBool::new(false));
        let hang = hang_run.clone();
        let run_refusal = Arc::new(Mutex::new(None::<RunRefusal>));
        let refusal = run_refusal.clone();
        let ran = Arc::new(Mutex::new(std::collections::HashSet::<Uuid>::new()));
        let run_count = runs.clone();
        let attach_count = attaches.clone();
        let strict = strict_attach.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = host.accept().await {
                let runs = run_count.clone();
                let attaches = attach_count.clone();
                let strict = strict.clone();
                let hang = hang.clone();
                let refusal = refusal.clone();
                let ran = ran.clone();
                let frames = captured.clone();
                tokio::spawn(async move {
                    let Ok(motor_lease::Prelude::LeaseHello {
                        token,
                        requester_jwk,
                        ..
                    }) = tls::read(&mut socket).await
                    else {
                        return;
                    };
                    let claims: Claims = serde_json::from_slice(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(token.split('.').nth(1).unwrap())
                            .unwrap(),
                    )
                    .unwrap();
                    let challenge = motor_lease::Challenge {
                        nonce: "b".repeat(64),
                        host_id: claims.placement.host.clone(),
                        ts: now_s(),
                    };
                    tls::write(
                        &mut socket,
                        &motor_lease::Prelude::Challenge(challenge.clone()),
                    )
                    .await
                    .unwrap();
                    let motor_lease::Prelude::ChallengeResponse { sig } =
                        tls::read(&mut socket).await.unwrap()
                    else {
                        panic!("challenge response");
                    };
                    motor_lease::Nonces::default()
                        .verify(&challenge, &claims, &requester_jwk, &sig, now_s())
                        .unwrap();
                    tls::write(&mut socket, &motor_lease::Prelude::LeaseOk {})
                        .await
                        .unwrap();
                    let hello = read_frame_async(&mut socket).await.unwrap().unwrap();
                    assert!(matches!(hello, Frame::Hello { .. }));
                    frames.lock().unwrap().push(hello);
                    write_frame_async(
                        &mut socket,
                        &Frame::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            started: true,
                            gate_jobs: true,
                            platforms: vec!["linux".into()],
                            default_branch_tip: false,
                            return_paths: false,
                            return_chunked: false,
                            sibling_paths: false,
                        },
                    )
                    .await
                    .unwrap();
                    let id = Uuid::parse_str(&claims.jti).unwrap();
                    while let Ok(Some(frame)) = read_frame_async(&mut socket).await {
                        match frame {
                            Frame::Heartbeat { .. } => {}
                            Frame::Have { repositories, .. } => {
                                // Claim all candidate commits so no bundle upload is needed.
                                write_frame_async(
                                    &mut socket,
                                    &Frame::HaveReply {
                                        repositories,
                                        git_dependencies: Some(Vec::new()),
                                    },
                                )
                                .await
                                .unwrap();
                            }
                            Frame::Run(request) => {
                                frames.lock().unwrap().push(Frame::Run(request));
                                runs.fetch_add(1, Ordering::SeqCst);
                                ran.lock().unwrap().insert(id);
                                if drop_run {
                                    return;
                                }
                                let refused = refusal.lock().unwrap().clone();
                                if let Some(refused) = refused {
                                    let reply = if refused.after_accept {
                                        write_frame_async(
                                            &mut socket,
                                            &Frame::Accepted(Accepted::new(id, 0)),
                                        )
                                        .await
                                        .unwrap();
                                        Frame::Terminal(
                                            TerminalRecord::new(
                                                id,
                                                Outcome::RefusedBeforeStart {
                                                    reason: refused.reason,
                                                },
                                                0,
                                                0,
                                                0,
                                            )
                                            .with_refusal_detail(refused.detail),
                                        )
                                    } else {
                                        let refusal = Refusal {
                                            job_id: Some(id),
                                            transfer_id: None,
                                            reason: refused.reason,
                                            disk_full: None,
                                            retry_after_ms: None,
                                            detail: Some(refused.detail),
                                        };
                                        Frame::Refusal(refusal)
                                    };
                                    write_frame_async(&mut socket, &reply).await.unwrap();
                                    return;
                                }
                                write_frame_async(
                                    &mut socket,
                                    &Frame::Accepted(Accepted::new(id, 0)),
                                )
                                .await
                                .unwrap();
                                if hang.load(Ordering::SeqCst) {
                                    continue;
                                }
                                send_result(&mut socket, id, 0).await;
                                return;
                            }
                            Frame::Attach(request) => {
                                assert_eq!(claims.purpose, Purpose::Attach);
                                attaches.fetch_add(1, Ordering::SeqCst);
                                if strict.load(Ordering::SeqCst)
                                    && !ran.lock().unwrap().contains(&id)
                                {
                                    return;
                                }
                                send_result(&mut socket, id, request.from_seq).await;
                                return;
                            }
                            Frame::Cancel(request) => {
                                frames.lock().unwrap().push(Frame::Cancel(request));
                                return;
                            }
                            _ => panic!("unexpected Boat frame"),
                        }
                    }
                });
            }
        });
        let requests_seen = Arc::new(AtomicUsize::new(0));
        let job = Arc::new(Mutex::new(None::<motor_lease::LeaseRequest>));
        let lost = Arc::new(AtomicUsize::new(0));
        let lost_reply = lost.clone();
        let mem_mib = Arc::new(AtomicU64::new(0));
        let token_mem = mem_mib.clone();
        let http=Http::new(move |path,body| {
            if path.ends_with("/reattach") && lost_reply.load(Ordering::SeqCst) != 0 { return (409, serde_json::json!({"error":"sandbox_lost"})); }
            if path.ends_with("/finish") {return (200,serde_json::json!({"job_id":path.split('/').nth(3).unwrap(),"settled_vcpu_ms":250,"released_vcpu_ms":59750}));}
            // CLOUD admits a Boat job at booking, so release can never end it.
            if path.ends_with("/release") { return (409, serde_json::json!({"error":"job_admitted"})); }
            if requests_seen.fetch_add(1, Ordering::SeqCst) < provisioning {
                return (202, serde_json::json!({"status":"provisioning","retry_after_ms":10}));
            }
            let (id,jwk,purpose,sshd)=if path=="/v1/leases" {
                let mut ordinary = body.clone();
                if let Some(shadow) = ordinary.as_object_mut().unwrap().remove("shadow") { assert_eq!(shadow, true); }
                let request: motor_lease::LeaseRequest=serde_json::from_value(ordinary).unwrap();
                let mut saved=job.lock().unwrap();
                if let Some(saved)=saved.as_ref() {assert_eq!(saved.job_id,request.job_id);}
                *saved=Some(request.clone());
                (request.job_id,request.requester_jwk,Purpose::Run,&initial)
            } else {
                assert!(path.ends_with("/reattach"));
                (path.split('/').nth(3).unwrap().to_owned(),serde_json::from_value::<motor_lease::ReattachRequest>(body.clone()).unwrap().requester_jwk,Purpose::Attach,&resumed)
            };
            sshd.authorize_public(&jwk,port);
            let request=job.lock().unwrap().clone().unwrap(); let now=now_s();
            let mem_mib = match token_mem.load(Ordering::SeqCst) { 0 => motor_lease::memory_max_mib(request.vcpus), mib => mib };
            let claims=Claims {iss:motor_lease::ISSUER.into(),aud:"boat:test".into(),sub:"acct:test".into(),jti:id.clone(),iat:now,exp:now+120,vcpus:request.vcpus,mem_mib,platform:request.platform,net:request.net,placement:motor_lease::Placement {host:"boat:test".into(),vm:Some("test".into())},max_s:request.timeout_s,purpose,cnf:Confirmation{jkt:jwk.thumbprint().unwrap()}};
            let lease=serde_json::json!({"provider":"boat","ssh":{"endpoint":sshd.endpoint,"host_key":sshd.host_key,"user":sshd.user},"runner_port":port,"lease_id":id,"job_id":id,"placement":claims.placement,"job_token":motor_lease::sign_token(&claims,"test",&SigningKey::from_bytes(&[7;32])).unwrap(),"token_expires_at_ms":claims.exp*1000,"max_s":claims.max_s,"reserved_vcpu_ms":60000});
            (201,lease)
        }).await;
        let config = Config {
            runner_instance: "test".into(),
            broker_token_source: BrokerTokenSource::Fake,
            broker_url: Some(http.url.clone()),
            spill_to_hosted: true,
            spill_after_s: 0,
            ..Config::default()
        };
        Self {
            config,
            requests: http.requests.clone(),
            runs,
            attaches,
            frames,
            lost,
            mem_mib,
            strict_attach,
            hang_run,
            run_refusal,
            _http: http,
            _host: task,
        }
    }
}

pub(crate) async fn send_result<W: tokio::io::AsyncWrite + Unpin>(
    socket: &mut W,
    id: Uuid,
    from: u64,
) {
    if from == 0 {
        write_frame_async(
            socket,
            &Frame::Output(Output::new(
                0,
                OutputStream::Stdout,
                BytePayload(b"same output\n".to_vec()),
            )),
        )
        .await
        .unwrap();
    }
    write_frame_async(
        socket,
        &Frame::Terminal(
            TerminalRecord::new(id, Outcome::Exit { code: 23 }, 0, 125, 0).with_ran(Ran::Remote),
        ),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn boat_provisioning_retry_and_reattach_refresh_endpoint_and_pin() {
    let boat = Boat::new(2, true).await;
    let scratch = cortexkit_test_support::ScratchDir::new("boat-client");
    let storage = Arc::new(Storage::open(&scratch.join("state")).unwrap());
    let id = Uuid::now_v7();
    storage.create(id, 300, None, None, false).unwrap();
    let route = HostedJobState::new(&boat.config);
    storage.hosted_route(id, route.clone()).unwrap();
    let path = scratch.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "true").with_timeout(30);
    let (mut channel, _) = Client::new(route)
        .run(id, &request, &storage)
        .await
        .unwrap();
    let initial = storage.job(id).unwrap().hosted.unwrap().endpoint;
    // A dropped run connection is reattached by job ID, never resubmitted.
    drop(channel);
    let route = storage.job(id).unwrap().hosted.unwrap();
    channel = Client::new(route).attach(id, &storage).await.unwrap();
    assert_ne!(storage.job(id).unwrap().hosted.unwrap().endpoint, initial);
    channel
        .send(Frame::Attach(AttachRequest::new(id, 0)))
        .await
        .unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        crate::transport::Incoming::Frame(Frame::Output(_))
    ));
    assert!(
        matches!(channel.next().await.unwrap(),crate::transport::Incoming::Frame(Frame::Terminal(t)) if t.outcome==Outcome::Exit{code:23})
    );
    assert_eq!(
        boat.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/leases")
            .count(),
        3
    );
}

#[tokio::test]
async fn boat_loss_409_stops_reattach_and_never_resubmits_or_releases() {
    let boat = Boat::new(0, true).await;
    let scratch = cortexkit_test_support::ScratchDir::new("boat-loss");
    let storage = Arc::new(Storage::open(&scratch.join("state")).unwrap());
    let id = Uuid::now_v7();
    storage.create(id, 300, None, None, false).unwrap();
    let route = HostedJobState::new(&boat.config);
    storage.hosted_route(id, route.clone()).unwrap();
    let path = scratch.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "true").with_timeout(30);
    let (channel, _) = Client::new(route)
        .run(id, &request, &storage)
        .await
        .unwrap();
    drop(channel);
    drop(storage);
    boat.lost.store(1, Ordering::SeqCst);
    let motor = crate::Motor::open(boat.config.clone(), &scratch.join("state")).unwrap();
    let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
    let record = tokio::time::timeout(Duration::from_secs(10), stream.records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(record, StreamRecord::Terminal(ref t) if t.outcome == Outcome::OutcomeUnknown && t.ran == Some(Ran::Unknown("maybe".into())) && t.refusal_detail.as_deref() == Some("hosted_sandbox_lost: test"))
    );
    let requests = boat.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(path, _)| path.ends_with("/reattach"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(path, _)| path == "/v1/leases")
            .count(),
        1
    );
    assert!(!requests.iter().any(|(path, _)| path.ends_with("/release")));
    assert_eq!(
        requests.last().unwrap().1,
        serde_json::json!({"outcome":"lost","run_ms":0})
    );
    assert_eq!(boat.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cloud_boat_vectors_match_client_request_lease_and_finish_types() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-lease.json"
    ))
    .unwrap();
    let decode_hex = |text: &str| -> [u8; 32] {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u8::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    };
    let key = SigningKey::from_bytes(&decode_hex(
        vector["requester"]["test_only_seed_hex"].as_str().unwrap(),
    ));
    let booked_request = &vector["booked"]["request"]["body"];
    let id = Uuid::parse_str(booked_request["job_id"].as_str().unwrap()).unwrap();
    let request = RunRequest::new(
        "/home/test/workspace",
        "/home/test/workspace/repo",
        "/home/test/workspace",
        "true",
    )
    .with_platform(Platform::Linux)
    .with_weight_hint(booked_request["vcpus"].as_u64().unwrap() as u32)
    .with_timeout(booked_request["timeout_s"].as_u64().unwrap());
    let lease_request = lease_body(
        id,
        &request,
        (
            booked_request["workspace_key_hash"]
                .as_str()
                .unwrap()
                .into(),
            booked_request["repo_hash"].as_str().unwrap().into(),
        ),
        serde_json::from_value(booked_request["placement_hint"].clone()).unwrap(),
        serde_json::from_value(booked_request["avoid_host"].clone()).unwrap(),
        RequesterJwk::from_key(&key.verifying_key()),
        // Every lease ck-motor sends that can reach Boat (spill and shadow)
        // allows spilling.
        Some(true),
    )
    .unwrap();
    let built = serde_json::to_value(&lease_request).unwrap();
    for (scenario, expected) in [
        ("provisioning", &vector["provisioning"]["request"]["body"]),
        ("booked", booked_request),
    ] {
        for field in [
            "job_id",
            "vcpus",
            "platform",
            "workspace_key_hash",
            "repo_hash",
            "placement_hint",
            "avoid_host",
            "timeout_s",
            "allow_spill",
        ] {
            assert_eq!(
                built[field], expected[field],
                "{scenario} LeaseRequest.{field}"
            );
        }
        // CLOUD's net=true example is illustrative; Motor spills only network-free jobs.
        assert_eq!(built["net"], false, "{scenario} LeaseRequest.net");
        assert_eq!(
            built["requester_jwk"]["kty"],
            expected["requester_jwk"]["kty"]
        );
        assert_eq!(
            built["requester_jwk"]["crv"],
            expected["requester_jwk"]["crv"]
        );
        assert_eq!(
            built["requester_jwk"]["x"], expected["requester_jwk"]["x"],
            "{scenario} LeaseRequest.requester_jwk.x"
        );
    }
    assert_eq!(
        built["requester_jwk"]["x"], vector["requester"]["x"],
        "LeaseRequest.requester_jwk.x"
    );

    let booked_reply = &vector["booked"]["reply"];
    assert_eq!(booked_reply["status"], 201);
    let lease: placement::Lease = serde_json::from_value(booked_reply["body"].clone()).unwrap();
    assert_eq!(booked_reply["body"]["provider"], "boat");
    assert_eq!(
        lease.lease_id,
        booked_reply["body"]["lease_id"].as_str().unwrap()
    );
    assert_eq!(
        lease.job_id,
        booked_reply["body"]["job_id"].as_str().unwrap()
    );
    assert_eq!(lease.placement.host, "boat:bx_22222222");
    assert_eq!(lease.placement.vm.as_deref(), Some("bx_22222222"));
    let ssh = lease.ssh.as_ref().unwrap();
    assert_eq!(ssh.endpoint, "198.51.100.1:22000");
    assert_eq!(
        ssh.host_key,
        booked_reply["body"]["ssh"]["host_key"].as_str().unwrap()
    );
    assert_eq!(ssh.user, "user");
    assert_eq!(lease.runner_port, Some(9443));
    assert_eq!(lease.job_token, booked_reply["body"]["job_token"]);
    assert_eq!(
        lease.token_expires_at_ms,
        booked_reply["body"]["token_expires_at_ms"]
            .as_u64()
            .unwrap()
    );
    assert_eq!(lease.max_s, booked_reply["body"]["max_s"].as_u64().unwrap());
    assert_eq!(
        lease.reserved_vcpu_ms,
        booked_reply["body"]["reserved_vcpu_ms"].as_u64().unwrap()
    );

    let reattach_body = serde_json::to_value(reattach_body(&key)).unwrap();
    let reattach_request = &vector["sandbox_lost"]["reattach"]["request"];
    assert_eq!(reattach_body["requester_jwk"]["kty"], "OKP");
    assert_eq!(reattach_body["requester_jwk"]["crv"], "Ed25519");
    assert_eq!(
        reattach_body["requester_jwk"]["x"],
        reattach_request["body"]["requester_jwk"]["x"]
    );

    let finish = &vector["finish"];
    let finish_id = Uuid::parse_str(
        finish["request"]["pathname"]
            .as_str()
            .unwrap()
            .split('/')
            .nth(3)
            .unwrap(),
    )
    .unwrap();
    let mut terminal = TerminalRecord::new(finish_id, Outcome::Exit { code: 23 }, 0, 0, 0);
    terminal.wall_ms = finish["request"]["body"]["run_ms"].as_u64().unwrap();
    let finish_body = placement::Finish::from_terminal(&terminal, 100);
    let finish_value = serde_json::to_value(&finish_body).unwrap();
    assert_eq!(
        finish_value["outcome"],
        finish["request"]["body"]["outcome"]
    );
    assert_eq!(finish_value["run_ms"], finish["request"]["body"]["run_ms"]);
    assert_eq!(finish["request"]["method"], "POST");
    assert_eq!(finish["reply"]["status"], 200);
    let finish_reply: placement::FinishReply =
        serde_json::from_value(finish["reply"]["body"].clone()).unwrap();
    assert_eq!(finish_reply.job_id, finish["reply"]["body"]["job_id"]);
    assert_eq!(
        finish_reply.settled_vcpu_ms,
        finish["reply"]["body"]["settled_vcpu_ms"]
    );
    assert_eq!(
        finish_reply.released_vcpu_ms,
        finish["reply"]["body"]["released_vcpu_ms"]
    );

    let finish_reply_body = finish["reply"]["body"].clone();
    let finish_http = Http::new(move |_path, _body| (200, finish_reply_body.clone())).await;
    let config = Config {
        runner_instance: "boat-vector-test".into(),
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(finish_http.url.clone()),
        spill_to_hosted: true,
        ..Config::default()
    };
    let scratch = cortexkit_test_support::ScratchDir::new("boat-vector-finish");
    let storage = Arc::new(Storage::open(&scratch.join("state")).unwrap());
    storage
        .hosted_route(finish_id, HostedJobState::new(&config))
        .unwrap();
    storage
        .hosted_update(finish_id, |state| state.lease(&lease))
        .unwrap();
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    assert!(!Client::new(HostedJobState::new(&config))
        .finish(finish_id, Some(&terminal), &storage)
        .await
        .unwrap());
    let sent = finish_http.requests.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, finish["request"]["pathname"].as_str().unwrap());
    assert_eq!(sent[0].1["outcome"], finish["request"]["body"]["outcome"]);
    assert_eq!(sent[0].1["run_ms"], finish["request"]["body"]["run_ms"]);
    let settled = storage.job(finish_id).unwrap().hosted.unwrap();
    assert!(settled.finish_done);
    assert_eq!(settled.billed_vcpu_ms, Some(180000));
}

#[tokio::test]
async fn cloud_boat_sandbox_lost_vector_maps_to_client_error() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-lease.json"
    ))
    .unwrap();
    let decode_hex = |text: &str| -> [u8; 32] {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u8::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    };
    let key = SigningKey::from_bytes(&decode_hex(
        vector["requester"]["test_only_seed_hex"].as_str().unwrap(),
    ));
    let request = &vector["sandbox_lost"]["reattach"]["request"];
    let reply_body = vector["sandbox_lost"]["reattach"]["reply"]["body"].clone();
    let http = Http::new(move |_path, _body| (409, reply_body.clone())).await;
    let config = Config {
        runner_instance: "boat-vector-loss-test".into(),
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        spill_to_hosted: true,
        ..Config::default()
    };
    let result = Client::new(HostedJobState::new(&config))
        .post_authorized::<_, serde_json::Value>(
            request["pathname"].as_str().unwrap(),
            &reattach_body(&key),
            "hosted-test-bearer".into(),
        )
        .await;
    assert!(matches!(result, Err(Error::SandboxLost)));
    let sent = http.requests.lock().unwrap();
    let (path, body) = sent.last().unwrap();
    assert_eq!(path, request["pathname"].as_str().unwrap());
    assert_eq!(
        body["requester_jwk"]["x"],
        request["body"]["requester_jwk"]["x"]
    );
    let lost_reply = &vector["sandbox_lost"]["reattach"]["reply"];
    assert_eq!(lost_reply["status"], 409);
    assert_eq!(lost_reply["body"]["error"], "sandbox_lost");
}

#[tokio::test]
async fn cloud_boat_provisioning_response_retries_then_parses_booked_lease() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-lease.json"
    ))
    .unwrap();
    let provisioning = &vector["provisioning"]["reply"];
    let booked = &vector["booked"]["reply"];
    let calls = Arc::new(AtomicUsize::new(0));
    let reply_calls = calls.clone();
    let provisioning_status = provisioning["status"].as_u64().unwrap() as u16;
    let provisioning_body = provisioning["body"].clone();
    let booked_status = booked["status"].as_u64().unwrap() as u16;
    let booked_body = booked["body"].clone();
    let http = Http::new(move |_path, _body| {
        if reply_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            (provisioning_status, provisioning_body.clone())
        } else {
            (booked_status, booked_body.clone())
        }
    })
    .await;
    let config = Config {
        runner_instance: "boat-vector-provisioning-test".into(),
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        spill_to_hosted: true,
        ..Config::default()
    };
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let lease: placement::Lease = Client::new(HostedJobState::new(&config))
        .post_ready_authorized(
            vector["booked"]["request"]["pathname"].as_str().unwrap(),
            &vector["booked"]["request"]["body"],
            "hosted-test-bearer".into(),
            tokio::time::Instant::now() + Duration::from_secs(30),
            Error::Provisioning(MIN_PROVISIONING_RETRY_MS),
        )
        .await
        .unwrap();
    assert_eq!(lease.job_id, booked["body"]["job_id"].as_str().unwrap());
    assert_eq!(lease.placement.host, "boat:bx_22222222");
    assert_eq!(lease.runner_port, Some(9443));
    let sent = http.requests.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(sent[0].0, "/v1/leases");
    assert_eq!(sent[1].0, "/v1/leases");
    assert_eq!(sent[0].1, vector["booked"]["request"]["body"]);
    assert_eq!(sent[1].1, vector["booked"]["request"]["body"]);
    assert_eq!(sent[0].1["job_id"], sent[1].1["job_id"]);
}

#[tokio::test]
async fn cloud_boat_unknown_success_shape_is_a_typed_broker_error() {
    let http = Http::new(|_path, _body| (200, serde_json::json!({"status":"unexpected"}))).await;
    let config = Config {
        runner_instance: "boat-invalid-success-test".into(),
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        spill_to_hosted: true,
        ..Config::default()
    };
    let result = Client::new(HostedJobState::new(&config))
        .post_authorized::<_, placement::Lease>(
            "/v1/leases",
            &serde_json::json!({}),
            "token".into(),
        )
        .await;
    assert!(matches!(result, Err(Error::Broker)));
}

#[tokio::test]
async fn cloud_boat_provisioning_budget_exhaustion_is_retryable_and_bounded() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-lease.json"
    ))
    .unwrap();
    let reply = vector["provisioning"]["reply"]["body"].clone();
    let http = Http::new(move |_path, _body| (202, reply.clone())).await;
    let config = Config {
        runner_instance: "boat-vector-budget-test".into(),
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        spill_to_hosted: true,
        ..Config::default()
    };
    let route = HostedJobState {
        spill_wait_ms: Some(9_500),
        ..HostedJobState::default()
    };
    let job = crate::storage::Job {
        queue_wait_limit_s: 10,
        hosted: Some(route),
        ..crate::storage::Job::default()
    };
    let budget_ms = spill_wait_budget_ms(&job);
    assert_eq!(budget_ms, 500);
    let started = tokio::time::Instant::now();
    let result = Client::new(HostedJobState::new(&config))
        .post_ready_authorized::<_, placement::Lease>(
            "/v1/leases",
            &vector["provisioning"]["request"]["body"],
            "hosted-test-bearer".into(),
            started + Duration::from_millis(budget_ms),
            Error::Provisioning(MIN_PROVISIONING_RETRY_MS),
        )
        .await;
    let elapsed = started.elapsed();
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("provisioning unexpectedly produced a lease"),
    };
    assert!(matches!(error, Error::Provisioning(5_000)));
    assert_eq!(error.code(), "hosted_provisioning");
    assert_eq!(error.reason(), RefusalReason::Unreachable);
    assert_eq!(error.retry_after_ms(), Some(5_000));
    assert!(elapsed >= Duration::from_millis(budget_ms));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

#[test]
fn cloud_boat_provisioning_retry_delay_is_clamped() {
    assert_eq!(bounded_provisioning_retry_ms(0), 1_000);
    assert_eq!(bounded_provisioning_retry_ms(5_000), 5_000);
    assert_eq!(bounded_provisioning_retry_ms(u64::MAX), 30_000);
}

/// CLOUD's recorded Boat lease states the sandbox size's measured usable
/// memory, not the fleet formula, and ck-motor must accept it. A first-party
/// host's token must still state the formula exactly.
#[test]
fn cloud_boat_vector_token_memory_is_accepted_for_boat_only() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-lease.json"
    ))
    .unwrap();
    let body: motor_lease::LeaseRequest =
        serde_json::from_value(vector["booked"]["request"]["body"].clone()).unwrap();
    let token = vector["booked"]["reply"]["body"]["job_token"]
        .as_str()
        .unwrap();
    let claims: Claims = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims.mem_mib, vector["boat_sizes"][0]["mem_mib"]);
    assert_ne!(claims.mem_mib, motor_lease::memory_max_mib(body.vcpus));
    assert_eq!(resource_mismatch(&claims, &body, true), None);
    assert_eq!(
        resource_mismatch(&claims, &body, false).map(|(field, ..)| field),
        Some("mem_mib")
    );
    // Production's large size: 8 vCPUs with 13444 MiB usable.
    let mut large = claims.clone();
    let mut large_body = body.clone();
    large.vcpus = 8;
    large_body.vcpus = 8;
    large.mem_mib = 13444;
    assert_eq!(resource_mismatch(&large, &large_body, true), None);
    // Below 1 GiB per leased vCPU, or above the runner's cap, is refused.
    for mem_mib in [8191, motor_lease::memory_max_mib(8) + 1, 0] {
        large.mem_mib = mem_mib;
        assert_eq!(
            resource_mismatch(&large, &large_body, true),
            Some((
                "mem_mib",
                format!("8192..={}", motor_lease::memory_max_mib(8)),
                mem_mib.to_string()
            ))
        );
    }
    // The other resources stay exact for Boat too.
    let mut wrong = claims.clone();
    wrong.vcpus = 4;
    assert_eq!(
        resource_mismatch(&wrong, &body, true).map(|(field, ..)| field),
        Some("vcpus")
    );
    let mut wrong = claims.clone();
    wrong.net = !body.net;
    assert_eq!(
        resource_mismatch(&wrong, &body, true).map(|(field, ..)| field),
        Some("net")
    );
    let mut wrong = claims.clone();
    wrong.platform = "linux-arm64".into();
    assert_eq!(
        resource_mismatch(&wrong, &body, true).map(|(field, ..)| field),
        Some("platform")
    );
    let mut wrong = claims;
    wrong.max_s = body.timeout_s + 1;
    assert_eq!(
        resource_mismatch(&wrong, &body, true).map(|(field, ..)| field),
        Some("max_s")
    );
}

/// The `avoid_host` value of every lease request the broker saw, in order.
fn avoided(requests: &test_http::Requests) -> Vec<serde_json::Value> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/leases")
        .map(|(_, body)| body["avoid_host"].clone())
        .collect()
}

#[tokio::test]
async fn spilled_lease_avoids_the_build_server_and_every_re_request_repeats_it() {
    let scratch = cortexkit_test_support::ScratchDir::new("boat-avoid-host");
    let path = scratch.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "true").with_timeout(30);
    // An ordinary hosted route avoids no host, even when the spill key is set.
    let ordinary = Boat::new(0, true).await;
    let config = Config {
        spill_avoid_host: Some("build-server-1".into()),
        ..ordinary.config.clone()
    };
    let storage = Arc::new(Storage::open(&scratch.join("ordinary")).unwrap());
    let id = Uuid::now_v7();
    storage.create(id, 300, None, None, false).unwrap();
    let route = HostedJobState::new(&config);
    storage.hosted_route(id, route.clone()).unwrap();
    Client::new(route)
        .run(id, &request, &storage)
        .await
        .unwrap();
    assert_eq!(avoided(&ordinary.requests), [serde_json::Value::Null]);

    // A spilled route carries the build server's host id. The broker answers
    // provisioning twice, so the first run sends three lease requests.
    let boat = Boat::new(2, true).await;
    let config = Config {
        spill_avoid_host: Some("build-server-1".into()),
        ..boat.config.clone()
    };
    let storage = Arc::new(Storage::open(&scratch.join("spilled")).unwrap());
    let id = Uuid::now_v7();
    storage.create(id, 300, None, None, false).unwrap();
    let mut route = HostedJobState::new(&config);
    route.spill_wait_ms = Some(45_000);
    route.avoid_host = config.spill_avoid_host.clone();
    storage.hosted_route(id, route.clone()).unwrap();
    let (channel, _) = Client::new(route)
        .run(id, &request, &storage)
        .await
        .unwrap();
    drop(channel);
    assert_eq!(avoided(&boat.requests), ["build-server-1"; 3]);
    // After a restart the job's stored lease body is sent again, unchanged,
    // whatever the configuration says now.
    drop(storage);
    let storage = Arc::new(Storage::open(&scratch.join("spilled")).unwrap());
    let route = storage.job(id).unwrap().hosted.unwrap();
    Client::new(route)
        .run(id, &request, &storage)
        .await
        .unwrap();
    assert_eq!(avoided(&boat.requests), ["build-server-1"; 4]);
}

#[test]
fn a_lease_body_stored_before_avoid_host_existed_loads_as_avoiding_none() {
    let mut body = serde_json::to_value(
        lease_body(
            Uuid::now_v7(),
            &RunRequest::new("/home/a", "/home/a", "/home/a", "true").with_timeout(30),
            (
                motor_lease::base64url(&[1; 32]),
                motor_lease::base64url(&[2; 32]),
            ),
            None,
            Some("build-server-1".into()),
            RequesterJwk::from_key(&SigningKey::from_bytes(&[3; 32]).verifying_key()),
            Some(true),
        )
        .unwrap(),
    )
    .unwrap();
    let state = |body: &serde_json::Value| {
        serde_json::from_value::<HostedJobState>(serde_json::json!({
            "hosted_no_fallback": false, "hosted_code": null, "placement": null,
            "endpoint": null, "endpoint_spki_sha256": null, "max_runtime_s": null,
            "billed_vcpu_ms": null, "billing": "pending", "terminal": null,
            "broker_url": "https://runners.example", "token_source": "fake",
            "lease_request": body,
        }))
        .unwrap()
        .lease_request
        .unwrap()
    };
    assert_eq!(state(&body).avoid_host.as_deref(), Some("build-server-1"));
    body.as_object_mut().unwrap().remove("avoid_host");
    assert_eq!(state(&body).avoid_host, None);
}

/// A git checkout with one commit below this crate's target directory. Hosted
/// workspace paths must be under /Users or /home, so the tests using it need a
/// checkout in one of those roots, which test scratch directories are not.
pub(crate) fn committed_repo() -> (tests::harness::Scratch, std::path::PathBuf) {
    let scratch = tests::harness::Scratch(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("hosted-fixture-{}", Uuid::now_v7())),
    );
    let repo = scratch.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(allowed_path(&std::fs::canonicalize(&repo).unwrap()));
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    (scratch, repo)
}

/// Every output byte of `stream`, and its terminal.
pub(crate) async fn records(mut stream: crate::JobStream) -> (Vec<u8>, TerminalRecord) {
    let mut output = Vec::new();
    let terminal = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match stream.records.recv().await.unwrap().unwrap() {
                StreamRecord::Output(record) => output.extend(record.bytes.0),
                StreamRecord::Terminal(terminal) => break terminal,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    (output, terminal)
}

/// A broker that has no capacity for any lease.
async fn no_capacity_broker() -> Http {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    Http::new(|path, _| {
        if path == "/v1/leases" {
            (503, serde_json::json!({"error":"no_capacity"}))
        } else {
            (404, serde_json::json!({"error":"not_found"}))
        }
    })
    .await
}

fn broker_config(http: &Http) -> Config {
    Config {
        runner_instance: "test".into(),
        broker_url: Some(http.url.clone()),
        broker_token_source: BrokerTokenSource::Fake,
        ..Config::default()
    }
}

#[tokio::test]
async fn direct_hosted_route_is_only_for_its_job_and_its_refusal_never_falls_back() {
    let http = no_capacity_broker().await;
    let scratch = cortexkit_test_support::ScratchDir::new("direct-hosted-route");
    let config = Config {
        spill_to_hosted: true,
        spill_avoid_host: Some("build-server-1".into()),
        ..broker_config(&http)
    };
    assert!(!config.hosted_runners);
    let ssh = Arc::new(tests::harness::NoSsh(AtomicUsize::new(0)));
    let motor = crate::Motor::with_connector(config, &scratch.join("state"), ssh.clone()).unwrap();
    // A path outside the hosted roots refuses before any lease or snapshot.
    let request =
        RunRequest::new("/tmp/missing", "/tmp/missing", "/tmp/missing", "true").with_timeout(30);
    let stream = motor
        .run_direct_hosted(request.clone(), None, crate::ReturnOptions::default())
        .unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    let state = motor.hosted_state(id).unwrap();
    assert!(state.hosted_test && state.hosted_no_fallback);
    assert_eq!(state.spill_wait_ms, None);
    assert_eq!(state.avoid_host, None);
    assert_eq!(
        state.hosted_code.as_deref(),
        Some("hosted_path_outside_roots")
    );
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    // The next job is not hosted: hosted routing stayed off.
    let stream = motor.run_direct(request, None).unwrap();
    let id = stream.job_id;
    records(stream).await;
    assert!(motor.hosted_state(id).is_none());
    assert!(http.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hosted_test_lease_is_not_retried_for_capacity_and_keeps_the_broker_code() {
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-test-capacity");
    let path = scratch.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "true").with_timeout(30);
    for hosted_test in [true, false] {
        let http = no_capacity_broker().await;
        let config = broker_config(&http);
        let storage = Arc::new(Storage::open(&scratch.join(format!("{hosted_test}"))).unwrap());
        let id = Uuid::now_v7();
        storage.create(id, 300, None, None, true).unwrap();
        let mut route = HostedJobState::new(&config);
        route.hosted_test = hosted_test;
        storage.hosted_route(id, route.clone()).unwrap();
        let client = Client::new(route);
        assert_eq!(
            client.run(id, &request, &storage).await.err(),
            Some(Error::Capacity)
        );
        assert_eq!(client.broker_code().as_deref(), Some("no_capacity"));
        // A hosted test asks once; an ordinary hosted job tries three times.
        let avoided = avoided(&http.requests);
        assert_eq!(avoided.len(), if hosted_test { 1 } else { 3 });
        assert!(avoided.iter().all(serde_json::Value::is_null));
    }
    assert_eq!(
        Error::Capacity.detail_with_broker_code(Some("no_capacity")),
        "hosted_capacity: broker no_capacity"
    );
    assert_eq!(
        Error::Capacity.detail_with_broker_code(None),
        "hosted_capacity"
    );
}

#[tokio::test]
#[ignore = "requires checkout under /Users or /home"]
async fn direct_hosted_job_runs_on_a_lease_avoiding_no_host_with_hosted_routing_off() {
    use tests::harness::{Behavior, Harness};
    // The runner drops the connection after the Run frame, so the result
    // arrives by reattach, as in the hosted snapshot test.
    let h = Harness::new(Behavior::DropRunAck).await;
    h.broker.lock().unwrap().knobs.max_s = Some(30);
    let (scratch, repo) = committed_repo();
    // `test-host` is the fake broker's only host. Configuring it as the host
    // a spill avoids shows this job avoids no host: a lease avoiding it would
    // have no capacity.
    let config = Config {
        hosted_runners: false,
        spill_to_hosted: true,
        spill_avoid_host: Some("test-host".into()),
        ..h.config()
    };
    let ssh = Arc::new(tests::harness::NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let stream = motor
        .run_direct_hosted(
            Harness::request(&repo).with_timeout(120),
            None,
            crate::ReturnOptions::default(),
        )
        .unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 }, "{terminal:?}");
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.as_str() == "/v1/leases")
            .count(),
        1
    );
    let state = motor.hosted_state(id).unwrap();
    assert!(state.hosted_test);
    assert_eq!(state.lease_request.unwrap().avoid_host, None);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[ignore = "requires checkout under /Users or /home"]
async fn direct_hosted_lease_refusal_is_typed_at_once_and_never_falls_back() {
    let http = no_capacity_broker().await;
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(tests::harness::NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(broker_config(&http), &scratch.0.join("state"), ssh.clone())
            .unwrap();
    let path = repo.to_str().unwrap();
    let stream = motor
        .run_direct_hosted(
            RunRequest::new(path, path, path, "true").with_timeout(30),
            None,
            crate::ReturnOptions::default(),
        )
        .unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(
        terminal.refusal_detail.as_deref(),
        Some("hosted_capacity: broker no_capacity")
    );
    // One lease request: no capacity retries, and no build-server fallback.
    assert_eq!(avoided(&http.requests), [serde_json::Value::Null]);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    let state = motor.hosted_state(id).unwrap();
    assert!(state.hosted_no_fallback);
    assert_eq!(state.hosted_code.as_deref(), Some("hosted_capacity"));
}
