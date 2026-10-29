//! Late grants use the real broker protocol and an in-process runner transport.
//! Gates hold the grant, Have reply and admission independently of wall time.
use super::queue_tests::{fixture_repo, ResubmitPeer};
use super::*;
use crate::hosted::test_http::Http;
use cortexkit_test_support::ScratchDir;
use motor_lease::{Claims, Confirmation, Purpose};
use motor_protocol::{read_frame_async, write_frame_async, Accepted, CancelReply, Refusal};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    BeforeConnect,
    BeforeRun,
    Accepted,
    Refused,
    Lost,
    Running,
}

#[derive(Default)]
struct Gates {
    lease_requested: Notify,
    grant: Notify,
    have: Notify,
    have_reply: Notify,
    run: Notify,
    admission: Notify,
    accepted: Notify,
    cancel_ready: Notify,
}

struct Runner {
    branch: Branch,
    first_party: bool,
    gates: Arc<Gates>,
    connects: AtomicUsize,
    runs: Arc<AtomicUsize>,
    attaches: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
    tombstone: Arc<std::sync::Mutex<Option<TerminalRecord>>>,
}

impl Connector for Runner {
    fn connect(
        &self,
        id: Uuid,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = io::Result<Box<dyn crate::transport::Duplex>>>
                + Send
                + '_,
        >,
    > {
        let attempt = self.connects.fetch_add(1, Ordering::SeqCst);
        let branch = self.branch;
        let first_party = self.first_party;
        let gates = self.gates.clone();
        let runs = self.runs.clone();
        let attaches = self.attaches.clone();
        let cancels = self.cancels.clone();
        let tombstone = self.tombstone.clone();
        Box::pin(async move {
            if attempt > 0 && matches!(branch, Branch::BeforeRun | Branch::Lost) {
                return Err(io::Error::other("runner connection lost"));
            }
            let (client, mut server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut server).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut server,
                    &Frame::Hello {
                        protocol_version: motor_protocol::PROTOCOL_VERSION,
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
                while let Ok(Some(frame)) = read_frame_async(&mut server).await {
                    match frame {
                        Frame::Heartbeat { .. } => {}
                        Frame::Have { repositories, .. } => {
                            if branch == Branch::BeforeRun {
                                gates.have.notify_one();
                                gates.have_reply.notified().await;
                            }
                            write_frame_async(
                                &mut server,
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
                            gates.run.notify_one();
                            if branch == Branch::BeforeConnect {
                                // The real runner retains an unknown-job cancel
                                // as a tombstone and closes a duplicate Run.
                                assert!(tombstone.lock().unwrap().is_some());
                                return;
                            }
                            gates.admission.notified().await;
                            if branch == Branch::Lost {
                                return;
                            }
                            if branch == Branch::Refused {
                                write_frame_async(
                                    &mut server,
                                    &Frame::Refusal(Refusal {
                                        job_id: Some(id),
                                        transfer_id: None,
                                        reason: RefusalReason::RequestRejected,
                                        disk_full: None,
                                        retry_after_ms: None,
                                        detail: Some("runner refused Run".into()),
                                    }),
                                )
                                .await
                                .unwrap();
                                return;
                            }
                            write_frame_async(&mut server, &Frame::Accepted(Accepted::new(id, 0)))
                                .await
                                .unwrap();
                            gates.accepted.notify_one();
                            gates.cancel_ready.notified().await;
                            write_frame_async(
                                &mut server,
                                &Frame::Heartbeat {
                                    queue_wait_ms: None,
                                },
                            )
                            .await
                            .unwrap();
                        }
                        Frame::Cancel(_) => {
                            cancels.fetch_add(1, Ordering::SeqCst);
                            let mut terminal = TerminalRecord::new(
                                id,
                                Outcome::Cancelled,
                                0,
                                if branch == Branch::Accepted { 7_000 } else { 0 },
                                0,
                            )
                            .with_ran(if branch == Branch::Accepted {
                                Ran::Remote
                            } else {
                                Ran::None
                            });
                            terminal.wall_ms = if branch == Branch::Accepted { 7_000 } else { 0 };
                            *tombstone.lock().unwrap() = Some(terminal.clone());
                            write_frame_async(
                                &mut server,
                                &Frame::CancelReply(CancelReply::new(id)),
                            )
                            .await
                            .unwrap();
                            if branch == Branch::Accepted {
                                write_frame_async(&mut server, &Frame::Terminal(terminal))
                                    .await
                                    .unwrap();
                            }
                            return;
                        }
                        Frame::Attach(_) => {
                            attaches.fetch_add(1, Ordering::SeqCst);
                            let terminal = if branch == Branch::Running {
                                assert_eq!(runs.load(Ordering::SeqCst), 1);
                                let mut terminal =
                                    TerminalRecord::new(id, Outcome::Exit { code: 23 }, 0, 125, 0)
                                        .with_ran(Ran::Remote);
                                terminal.wall_ms = 125;
                                terminal
                            } else {
                                tombstone.lock().unwrap().clone().expect("cancel tombstone")
                            };
                            write_frame_async(&mut server, &Frame::Terminal(terminal))
                                .await
                                .unwrap();
                            if first_party {
                                write_frame_async(
                                    &mut server,
                                    &Frame::HostedTerminal(motor_protocol::HostedTerminal {
                                        job_id: id,
                                        placement: "first_party/host:test".into(),
                                        billed_vcpu_ms: 250,
                                        max_runtime_s: 7200,
                                    }),
                                )
                                .await
                                .unwrap();
                            }
                            return;
                        }
                        other => panic!("unexpected runner frame: {other:?}"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn crate::transport::Duplex>)
        })
    }
}

struct Harness {
    broker: Http,
    gates: Arc<Gates>,
    runner: Arc<Runner>,
}

impl Harness {
    async fn new(branch: Branch) -> Self {
        Self::with_host(branch, "boat:test").await
    }

    async fn with_host(branch: Branch, host: &'static str) -> Self {
        std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
        let gates = Arc::new(Gates::default());
        let held = gates.clone();
        let request = Arc::new(std::sync::Mutex::new(None::<motor_lease::LeaseRequest>));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let broker = Http::with_async_reply(move |path, body| {
            let gates = held.clone();
            let saved = request.clone();
            let finished = finished.clone();
            async move {
                let response = if path.ends_with("/finish") {
                    finished.store(true, Ordering::SeqCst);
                    (200, serde_json::json!({"job_id": path.split('/').nth(3).unwrap(),
                        "settled_vcpu_ms": 14_000, "released_vcpu_ms": 46_000}))
                } else if path.ends_with("/release") {
                    (409, serde_json::json!({"error": "job_admitted"}))
                } else if path.ends_with("/reattach") && finished.load(Ordering::SeqCst) {
                    (409, serde_json::json!({"error": "job_finished"}))
                } else {
                    let (request, jwk, purpose) = if path == "/v1/leases" {
                        let request: motor_lease::LeaseRequest = serde_json::from_value(body).unwrap();
                        *saved.lock().unwrap() = Some(request.clone());
                        gates.lease_requested.notify_one();
                        gates.grant.notified().await;
                        let jwk = request.requester_jwk.clone();
                        (request, jwk, Purpose::Run)
                    } else {
                        assert!(path.ends_with("/reattach"));
                        let request = saved.lock().unwrap().clone().unwrap();
                        let jwk = serde_json::from_value::<motor_lease::ReattachRequest>(body).unwrap().requester_jwk;
                        (request, jwk, Purpose::Attach)
                    };
                    let now = crate::hosted::now_s();
                    let claims = Claims {
                        workspace_key_hash: None,
                        iss: motor_lease::ISSUER.into(), aud: host.into(), sub: "acct:test".into(),
                        jti: request.job_id.clone(), iat: now, exp: now + 120,
                        vcpus: request.vcpus, mem_mib: motor_lease::memory_max_mib(request.vcpus),
                        platform: request.platform, net: request.net,
                        placement: motor_lease::Placement { host: host.into(), vm: Some("test".into()) },
                        max_s: request.timeout_s, purpose,
                        cnf: Confirmation { jkt: jwk.thumbprint().unwrap() },
                    };
                    let mut lease = serde_json::json!({"provider": "boat", "ssh": {
                        "endpoint": "127.0.0.1:1", "host_key": ssh_key::PublicKey::new(
                            ssh_key::public::KeyData::Ed25519(ssh_key::public::Ed25519PublicKey([7; 32])),
                            "in-process-runner").to_openssh().unwrap(), "user": "test"},
                        "runner_port": 1, "lease_id": request.job_id, "job_id": request.job_id,
                        "placement": claims.placement,
                        "job_token": motor_lease::sign_token(&claims, "test", &ed25519_dalek::SigningKey::from_bytes(&[7; 32])).unwrap(),
                        "token_expires_at_ms": claims.exp * 1000, "max_s": claims.max_s,
                        "reserved_vcpu_ms": 60_000});
                    if !host.starts_with("boat:") {
                        let fields = lease.as_object_mut().unwrap();
                        fields.remove("provider");
                        fields.remove("ssh");
                        fields.remove("runner_port");
                        fields.insert("endpoint".into(), "127.0.0.1:1".into());
                        fields.insert("endpoint_spki_sha256".into(), motor_lease::base64url(&[7; 32]).into());
                    }
                    (201, lease)
                };
                (response.0, serde_json::to_vec(&response.1).unwrap())
            }
        }).await;
        let runner = Arc::new(Runner {
            branch,
            first_party: !host.starts_with("boat:"),
            gates: gates.clone(),
            connects: AtomicUsize::new(0),
            runs: Arc::default(),
            attaches: Arc::default(),
            cancels: Arc::default(),
            tombstone: Arc::default(),
        });
        Self {
            broker,
            gates,
            runner,
        }
    }
}

struct Job {
    motor: Motor,
    stream: JobStream,
    owner: tokio::task::JoinHandle<io::Result<()>>,
    _scratch: ScratchDir,
}

impl Job {
    async fn running(h: &Harness) -> Self {
        let job = Self::start(h).await;
        h.gates.grant.notify_one();
        h.gates.run.notified().await;
        h.gates.admission.notify_one();
        h.gates.accepted.notified().await;
        job
    }

    async fn start(h: &Harness) -> Self {
        let scratch = ScratchDir::new("late-boat-grant");
        let repo = fixture_repo(&scratch);
        let config = Config {
            runner_instance: "test".into(),
            broker_token_source: crate::BrokerTokenSource::Fake,
            broker_url: Some(h.broker.url.clone()),
            spill_to_hosted: true,
            spill_after_s: 0,
            ..Config::default()
        };
        let mut motor = Motor::with_connector(
            config,
            &scratch.join("state"),
            Arc::new(ResubmitPeer::new()),
        )
        .unwrap();
        motor.hosted_connector = Some(h.runner.clone());
        let id = Uuid::now_v7();
        motor
            .storage
            .create(id, 300, None, Some("test".into()), false)
            .unwrap();
        let path = repo.to_str().unwrap();
        let request = RunRequest::new(path, path, path, "sleep 7200").with_timeout(7200);
        let (stream, tx, wake) = motor.stream(id);
        let job = motor.clone();
        let submission = motor.submission(id);
        let owner = tokio::spawn(async move {
            let _submission = submission;
            job.run_job(
                id,
                (request, crate::ReturnOptions::default()),
                Vec::new(),
                (300, true),
                &tx,
                wake,
            )
            .await
        });
        h.gates.lease_requested.notified().await;
        Self {
            motor,
            stream,
            owner,
            _scratch: scratch,
        }
    }
}

async fn detach_running(h: &Harness, job: Job) -> (Motor, ScratchDir, Uuid) {
    let Job {
        motor,
        stream,
        owner,
        _scratch: scratch,
    } = job;
    let id = stream.job_id;
    drop(stream);
    h.gates.cancel_ready.notify_one();
    assert!(owner.await.unwrap().is_err(), "the caller detached");
    let state = motor.hosted_state(id).unwrap();
    assert!(state.run_sent);
    assert!(state.terminal.is_none());
    assert!(!motor.pending(id));
    (motor, scratch, id)
}

fn assert_still_booked(h: &Harness, motor: &Motor, id: Uuid) {
    let state = motor.hosted_state(id).unwrap();
    assert!(state.run_sent);
    assert!(state.terminal.is_none());
    assert!(!state.finish_done);
    assert!(state.pending_finish.is_none());
    assert!(!motor.pending(id));
    assert_eq!(h.runner.cancels.load(Ordering::SeqCst), 0);
    assert!(h
        .broker
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(p, _)| { !p.ends_with("/finish") && !p.ends_with("/release") }));
}

fn reopen(h: &Harness, motor: Motor, scratch: &ScratchDir) -> Motor {
    let config = motor.config.clone();
    motor.storage.shutdown().unwrap();
    drop(motor);
    let mut motor = Motor::with_connector(
        config,
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    motor.hosted_connector = Some(h.runner.clone());
    motor
}

async fn terminal(mut stream: JobStream) -> TerminalRecord {
    let mut terminals = Vec::new();
    while let Some(record) = stream.records.recv().await {
        if let StreamRecord::Terminal(terminal) = record.unwrap() {
            terminals.push(terminal);
        }
    }
    assert_eq!(terminals.len(), 1, "one caller-facing terminal");
    terminals.pop().unwrap()
}

fn assert_settled(h: &Harness, motor: &Motor, id: Uuid, outcome: &str, run_ms: u64) {
    let requests = h.broker.requests.lock().unwrap();
    let finishes: Vec<_> = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/finish"))
        .collect();
    assert_eq!(
        finishes.len(),
        1,
        "each granted booking needs exactly one finish: {requests:?}"
    );
    assert_eq!(finishes[0].0, format!("/v1/leases/{id}/finish"));
    assert_eq!(
        finishes[0].1,
        serde_json::json!({"outcome": outcome, "run_ms": run_ms})
    );
    assert!(
        requests.iter().all(|(path, _)| !path.ends_with("/release")),
        "known Boat bookings cannot be released"
    );
    let state = motor.storage.job(id).unwrap().hosted.unwrap();
    assert!(state.finish_done);
    assert!(state.pending_finish.is_none());
}

async fn shutdown_restart_attach(host: &'static str) {
    let h = Harness::with_host(Branch::Running, host).await;
    let job = Job::running(&h).await;
    let (motor, scratch, id) = detach_running(&h, job).await;
    motor.shutdown_hosted().await;
    assert_still_booked(&h, &motor, id);

    let motor = reopen(&h, motor, &scratch);
    motor.resume_finishes().await;
    assert_still_booked(&h, &motor, id);
    let t = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 23 });
    assert_eq!(t.ran, Some(Ran::Remote));
    assert_eq!(h.runner.runs.load(Ordering::SeqCst), 1);
    assert_eq!(h.runner.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(h.runner.cancels.load(Ordering::SeqCst), 0);
    {
        let requests = h.broker.requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|(p, _)| p == "/v1/leases").count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|(p, _)| p.ends_with("/reattach"))
                .count(),
            1
        );
        assert!(requests.iter().all(|(p, _)| !p.ends_with("/release")));
    }
    motor.shutdown_hosted().await;
    if host.starts_with("boat:") {
        assert_settled(&h, &motor, id, "exit", 125);
    } else {
        assert!(h
            .broker
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| !p.ends_with("/finish")));
    }
}

#[tokio::test]
async fn running_boat_shutdown_restart_reattaches_and_streams_terminal() {
    shutdown_restart_attach("boat:test").await;
}

#[tokio::test]
async fn running_first_party_shutdown_restart_reattaches_and_streams_terminal() {
    shutdown_restart_attach("host:test").await;
}

#[tokio::test]
async fn boat_deadline_passed_during_downtime_settles_without_attach() {
    let h = Harness::new(Branch::Running).await;
    let job = Job::running(&h).await;
    let (motor, scratch, id) = detach_running(&h, job).await;
    motor.shutdown_hosted().await;
    assert_still_booked(&h, &motor, id);
    // Age only the original Run token: reattach tokens must never renew the
    // host's runtime bound, and no caller is needed to recover an expired job.
    motor
        .storage
        .hosted_update(id, |s| s.retry_iat = Some(0))
        .unwrap();
    let motor = reopen(&h, motor, &scratch);
    motor.resume_finishes().await;
    motor.wait_hosted_cleanup(id).await;
    assert_settled(&h, &motor, id, "lost", 0);
    assert_eq!(h.runner.attaches.load(Ordering::SeqCst), 0);
    assert_eq!(h.runner.cancels.load(Ordering::SeqCst), 0);
    assert_eq!(h.runner.runs.load(Ordering::SeqCst), 1);
    motor.shutdown_hosted().await;
}

#[tokio::test]
async fn detached_boat_deadline_is_watched_after_restart_without_attach() {
    let h = Harness::new(Branch::Running).await;
    let job = Job::running(&h).await;
    let (motor, scratch, id) = detach_running(&h, job).await;
    motor.shutdown_hosted().await;
    let motor = reopen(&h, motor, &scratch);
    motor.resume_finishes().await;
    assert!(!motor.cleanup_hosted_once(id, false).await.unwrap());
    assert_still_booked(&h, &motor, id);
    motor
        .storage
        .hosted_update(id, |s| s.retry_iat = Some(0))
        .unwrap();
    motor.wait_hosted_cleanup(id).await;
    assert_settled(&h, &motor, id, "lost", 0);
    assert_eq!(h.runner.attaches.load(Ordering::SeqCst), 0);
    assert_eq!(h.runner.cancels.load(Ordering::SeqCst), 0);
    motor.shutdown_hosted().await;
}

#[tokio::test]
async fn unused_boat_booking_is_settled_at_shutdown() {
    let h = Harness::new(Branch::BeforeRun).await;
    let job = Job::start(&h).await;
    let id = job.stream.job_id;
    h.gates.grant.notify_one();
    h.gates.have.notified().await;
    assert!(!job.motor.hosted_state(id).unwrap().run_sent);
    job.motor.shutdown_hosted().await;
    assert_settled(&h, &job.motor, id, "lost", 0);
    assert_eq!(h.runner.runs.load(Ordering::SeqCst), 0);
    assert_eq!(h.runner.cancels.load(Ordering::SeqCst), 0);
    // The submission is held before Run to isolate unused-booking shutdown.
    job.owner.abort();
    assert!(job.owner.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn acknowledged_cancel_recovery_settles_before_host_deadline() {
    let h = Harness::new(Branch::Running).await;
    let job = Job::running(&h).await;
    let (motor, scratch, id) = detach_running(&h, job).await;
    motor.shutdown_hosted().await;
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    let motor = reopen(&h, motor, &scratch);
    motor.resume_finishes().await;
    motor.wait_hosted_cleanup(id).await;
    assert_settled(&h, &motor, id, "lost", 0);
    assert_eq!(h.runner.cancels.load(Ordering::SeqCst), 1);
    motor.shutdown_hosted().await;
}

#[tokio::test]
async fn grant_after_cancel_before_connect_finishes_cancelled_once() {
    let h = Harness::new(Branch::BeforeConnect).await;
    let job = Job::start(&h).await;
    let id = job.stream.job_id;
    job.motor.cancel(CancelRequest::new(id)).await.unwrap();
    h.gates.grant.notify_one();
    job.owner.await.unwrap().unwrap();
    assert_eq!(terminal(job.stream).await.outcome, Outcome::Cancelled);
    job.motor.shutdown_hosted().await;
    assert_eq!(
        h.runner.runs.load(Ordering::SeqCst),
        1,
        "only the tombstoned submission, never a resubmission"
    );
    assert_settled(&h, &job.motor, id, "cancelled", 0);
}

#[tokio::test]
async fn cancel_after_connect_before_run_failure_finishes_lost_once() {
    let h = Harness::new(Branch::BeforeRun).await;
    let job = Job::start(&h).await;
    let id = job.stream.job_id;
    h.gates.grant.notify_one();
    h.gates.have.notified().await;
    job.motor.cancel(CancelRequest::new(id)).await.unwrap();
    h.gates.have_reply.notify_one();
    assert!(
        job.owner.await.unwrap().is_err(),
        "the pre-Run cancel connection fails"
    );
    job.motor.wait_hosted_cleanup(id).await;
    assert_eq!(h.runner.runs.load(Ordering::SeqCst), 0);
    assert!(!job.motor.storage.job(id).unwrap().hosted.unwrap().run_sent);
    assert_settled(&h, &job.motor, id, "lost", 0);
    job.motor.shutdown_hosted().await;
    assert_settled(&h, &job.motor, id, "lost", 0);
}

#[tokio::test]
async fn cancel_after_run_accepted_finishes_cancelled_once() {
    let h = Harness::new(Branch::Accepted).await;
    let job = Job::start(&h).await;
    let id = job.stream.job_id;
    h.gates.grant.notify_one();
    h.gates.run.notified().await;
    h.gates.admission.notify_one();
    h.gates.accepted.notified().await;
    job.motor.cancel(CancelRequest::new(id)).await.unwrap();
    h.gates.cancel_ready.notify_one();
    job.owner.await.unwrap().unwrap();
    assert_eq!(terminal(job.stream).await.outcome, Outcome::Cancelled);
    job.motor.shutdown_hosted().await;
    assert_settled(&h, &job.motor, id, "cancelled", 7_000);
}

#[tokio::test]
async fn runner_refusal_after_cancel_finishes_lost_once() {
    let h = Harness::new(Branch::Refused).await;
    let job = Job::start(&h).await;
    let id = job.stream.job_id;
    h.gates.grant.notify_one();
    h.gates.run.notified().await;
    job.motor.cancel(CancelRequest::new(id)).await.unwrap();
    h.gates.admission.notify_one();
    job.owner.await.unwrap().unwrap();
    assert!(matches!(
        terminal(job.stream).await.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    job.motor.shutdown_hosted().await;
    assert_settled(&h, &job.motor, id, "lost", 0);
}

#[tokio::test]
async fn connection_lost_after_run_and_cancel_finishes_lost_at_host_deadline() {
    for shutdown in [false, true] {
        let h = Harness::new(Branch::Lost).await;
        let job = Job::start(&h).await;
        let id = job.stream.job_id;
        h.gates.grant.notify_one();
        h.gates.run.notified().await;
        job.motor.cancel(CancelRequest::new(id)).await.unwrap();
        // Detaching must not own cleanup. The Run may have started, so neither
        // resubmit nor settlement is safe while the host can still be running it.
        drop(job.stream);
        h.gates.admission.notify_one();
        assert!(job.owner.await.unwrap().is_err());
        assert!(!job.motor.cleanup_hosted_once(id, false).await.unwrap());
        assert!(h
            .broker
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _)| !p.ends_with("/finish")));
        if shutdown {
            job.motor.shutdown_hosted().await;
            assert!(!job.motor.hosted_state(id).unwrap().finish_done);
        }
        job.motor
            .storage
            .hosted_update(id, |s| s.retry_iat = Some(0))
            .unwrap();
        assert!(job.motor.cleanup_hosted_once(id, false).await.unwrap());
        job.motor.shutdown_hosted().await;
        assert_eq!(h.runner.runs.load(Ordering::SeqCst), 1);
        assert_eq!(job.motor.storage.job(id).unwrap().build_server_job_id, None);
        assert_settled(&h, &job.motor, id, "lost", 0);
    }
}

#[tokio::test]
#[ignore = "run scripts/mixed-version.py with the real rollback client"]
async fn rollback_222d833_preserves_recovery_and_uncancelled_detach() {
    let helper = std::env::var_os("MOTOR_ROLLBACK_CLIENT")
        .expect("run python3 scripts/mixed-version.py for the real 222d833 client");
    for case in [
        "never_run",
        "cancel_pending",
        "cancel_acknowledged",
        "detached",
    ] {
        let h = Harness::new(Branch::Lost).await;
        let scratch = ScratchDir::new("rollback-boat-recovery");
        let root = scratch.join("state");
        let config = Config {
            runner_instance: "test".into(),
            broker_token_source: crate::BrokerTokenSource::Fake,
            broker_url: Some(h.broker.url.clone()),
            ..Config::default()
        };
        let storage = Storage::open(&root).unwrap();
        let id = Uuid::now_v7();
        storage.create(id, 300, None, None, false).unwrap();
        let mut route = crate::HostedJobState::new(&config);
        route.placement = Some(motor_lease::Placement {
            host: "boat:test".into(),
            vm: Some("test".into()),
        });
        route.max_runtime_s = Some(7200);
        route.retry_iat = Some(if case == "detached" {
            crate::hosted::now_s()
        } else {
            0
        });
        route.run_sent = case != "never_run";
        storage.hosted_route(id, route).unwrap();
        if case == "cancel_pending" || case == "cancel_acknowledged" {
            storage.cancel(id, true).unwrap();
        }
        if case == "cancel_acknowledged" {
            storage.cancel_acknowledged(id).unwrap();
        }
        storage.shutdown().unwrap();
        drop(storage);
        let expected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("state.json")).unwrap()).unwrap();
        let config_path = scratch.join("config.json");
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let old_root = root.clone();
        let helper = helper.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(helper)
                .env_remove("MOTOR_FAKE_BROKER_TOKEN")
                .args([
                    "127.0.0.1:1",
                    config_path.to_str().unwrap(),
                    old_root.to_str().unwrap(),
                    "store",
                    &id.to_string(),
                ])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rewritten: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            rewritten.get("_journal").is_none(),
            "the real old writer must rewrite the store"
        );
        let mut actual_job = rewritten["jobs"][id.to_string()].clone();
        let mut expected_job = expected["jobs"][id.to_string()].clone();
        actual_job
            .as_object_mut()
            .unwrap()
            .remove("retain_until_ms");
        expected_job
            .as_object_mut()
            .unwrap()
            .remove("retain_until_ms");
        assert_eq!(
            actual_job, expected_job,
            "old reader/writer must retain every recovery transition: {case}"
        );
        assert!(actual_job["hosted"].get("cleanup_pending").is_none());
        assert!(actual_job["hosted"].get("host_deadline_s").is_none());

        let mut motor =
            Motor::with_connector(config, &root, Arc::new(ResubmitPeer::new())).unwrap();
        motor.hosted_connector = Some(h.runner.clone());
        motor.resume_finishes().await;
        if case == "detached" {
            assert!(!motor.cleanup_hosted_once(id, false).await.unwrap());
            motor.shutdown_hosted().await;
            assert!(
                h.broker.requests.lock().unwrap().is_empty(),
                "rollback must not turn a detach into cancellation"
            );
            assert!(!motor.hosted_state(id).unwrap().finish_done);
        } else {
            motor.wait_hosted_cleanup(id).await;
            assert_settled(&h, &motor, id, "lost", 0);
            assert!(!motor.pending(id));
        }
        assert_eq!(h.runner.runs.load(Ordering::SeqCst), 0);
    }
}
