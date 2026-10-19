use super::*;
use crate::hosted::{ssh::tests::Sshd, test_http::Http};
use base64::Engine;
use motor_lease::{Claims, Confirmation, Purpose};
use motor_protocol::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use tokio::{net::TcpListener, task::JoinHandle};

pub(crate) struct Boat {
    pub config: Config,
    pub requests: test_http::Requests,
    pub runs: Arc<AtomicUsize>,
    pub attaches: Arc<AtomicUsize>,
    pub lost: Arc<AtomicUsize>,
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
        let run_count = runs.clone();
        let attach_count = attaches.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = host.accept().await {
                let runs = run_count.clone();
                let attaches = attach_count.clone();
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
        let http=Http::new(move |path,body| {
            if path.ends_with("/reattach") && lost_reply.load(Ordering::SeqCst) != 0 { return (409, serde_json::json!({"error":"sandbox_lost"})); }
            if path.ends_with("/finish") {return (200,serde_json::json!({"job_id":path.split('/').nth(3).unwrap(),"settled_vcpu_ms":250,"released_vcpu_ms":59750}));}
            if requests_seen.fetch_add(1,Ordering::SeqCst)<provisioning {return (503,serde_json::json!({"error":"provisioning","retry_after_ms":10}));}
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
            let claims=Claims {iss:motor_lease::ISSUER.into(),aud:"boat:test".into(),sub:"acct:test".into(),jti:id.clone(),iat:now,exp:now+120,vcpus:request.vcpus,mem_mib:motor_lease::memory_max_mib(request.vcpus),platform:request.platform,net:request.net,placement:motor_lease::Placement {host:"boat:test".into(),vm:Some("test".into())},max_s:request.timeout_s,purpose,cnf:Confirmation{jkt:jwk.thumbprint().unwrap()}};
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
