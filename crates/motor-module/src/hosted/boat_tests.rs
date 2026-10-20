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
    pub lost: Arc<AtomicUsize>,
    /// Usable memory the job token states; 0 states the fleet formula.
    pub mem_mib: Arc<AtomicU64>,
    /// Like a real runner, refuse to serve an attach for a job that never
    /// received a Run frame (the connection drops).
    pub strict_attach: Arc<AtomicBool>,
    _http: Http,
    _host: JoinHandle<()>,
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
        let strict_attach = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(Mutex::new(std::collections::HashSet::<Uuid>::new()));
        let run_count = runs.clone();
        let attach_count = attaches.clone();
        let strict = strict_attach.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = host.accept().await {
                let runs = run_count.clone();
                let attaches = attach_count.clone();
                let strict = strict.clone();
                let ran = ran.clone();
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
                    assert!(matches!(
                        read_frame_async(&mut socket).await.unwrap(),
                        Some(Frame::Hello { .. })
                    ));
                    write_frame_async(
                        &mut socket,
                        &Frame::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            started: true,
                            gate_jobs: true,
                            platforms: vec!["linux".into()],
                            default_branch_tip: false,
                            return_paths: false,
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
                            Frame::Run(_) => {
                                runs.fetch_add(1, Ordering::SeqCst);
                                ran.lock().unwrap().insert(id);
                                if drop_run {
                                    return;
                                }
                                write_frame_async(
                                    &mut socket,
                                    &Frame::Accepted(Accepted::new(id, 0)),
                                )
                                .await
                                .unwrap();
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
                let request: motor_lease::LeaseRequest=serde_json::from_value(body.clone()).unwrap();
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
            (200,lease)
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
            lost,
            mem_mib,
            strict_attach,
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
        RequesterJwk::from_key(&key.verifying_key()),
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
            "timeout_s",
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
