use super::*;
use crate::hosted::{
    boat_tests::{committed_repo, records},
    tests::harness::{Behavior, Harness, NoSsh},
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn config(h: &Harness, repo: &Path) -> Config {
    Config {
        hosted_runners: false,
        hosted_repositories: vec![repo.to_str().unwrap().into()],
        spill_to_hosted: true,
        spill_after_s: 0,
        spill_avoid_host: Some("test-host".into()),
        network_routing: true,
        ..h.config()
    }
}

fn request(workspace: &Path) -> RunRequest {
    let args = crate::cli::RunArgs {
        cwd: Some(workspace.into()),
        command: vec!["true".into()],
        timeout: 30,
        ..crate::cli::RunArgs::default()
    };
    crate::cli::build_request(&args).unwrap()
}

fn lease_bodies(h: &Harness) -> Vec<serde_json::Value> {
    h.lease_bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect()
}

#[test]
fn hosted_repositories_roundtrips_and_default_is_omitted() {
    let default: Config = serde_json::from_str("{}").unwrap();
    assert!(default.hosted_repositories.is_empty());
    assert!(serde_json::to_value(&default)
        .unwrap()
        .get("hosted_repositories")
        .is_none());
    let config: Config = serde_json::from_value(serde_json::json!({
        "hosted_repositories": ["/home/operator/project"]
    }))
    .unwrap();
    config.validate().unwrap();
    let wire = serde_json::to_value(&config).unwrap();
    assert_eq!(
        wire["hosted_repositories"],
        serde_json::json!(["/home/operator/project"])
    );
    let restored: Config = serde_json::from_value(wire).unwrap();
    assert_eq!(restored.hosted_repositories, config.hosted_repositories);
    assert!(!restored.hosted_runners);
}

#[tokio::test]
async fn listed_main_and_linked_agent_and_direct_jobs_use_hosted_without_avoiding_a_host() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let linked = scratch.0.join("worker");
    snapshot::git(
        &repo,
        &["worktree", "add", "-qb", "worker", linked.to_str().unwrap()],
    )
    .unwrap();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone()).unwrap();
    for workspace in [&repo, &linked] {
        for direct in [false, true] {
            for net in [false, true] {
                let mut request = request(workspace);
                assert_eq!(request.repository_root, repo.to_str().unwrap());
                if net {
                    request.network = Some(Network::Outbound);
                }
                let stream = if direct {
                    motor.run_direct(request, None)
                } else {
                    motor.run(request)
                }
                .unwrap();
                let id = stream.job_id;
                let (_, terminal) = records(stream).await;
                assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
                let state = motor.hosted_state(id).unwrap();
                assert!(state.repository_routed && state.run_sent);
                assert_eq!(state.spill_wait_ms, None);
                assert_eq!(state.avoid_host, None);
                assert_eq!(state.billed_vcpu_ms, Some(6001));
            }
        }
    }
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 8);
    for body in &bodies {
        assert_eq!(body["avoid_host"], serde_json::Value::Null);
        assert_eq!(body["allow_spill"], false);
    }
    assert_eq!(bodies.iter().filter(|b| b["net"] == true).count(), 4);
    assert_eq!(h.runs.load(Ordering::SeqCst), 8);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(motor.hosted_health().unwrap()["hosted_routed_jobs_1h"], 8);
}

#[tokio::test]
async fn unlisted_and_windows_jobs_keep_the_direct_path() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let mut config = config(&h, &repo);
    config.spill_to_hosted = false;
    config.windows_runner_instance = Some("windows-test".into());
    config.windows_runner_state_root = Some("/home/test/windows".into());
    let ssh = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let (_unlisted_scratch, unlisted) = committed_repo();
    for request in [
        request(&unlisted),
        request(&repo).with_platform(Platform::Windows),
    ] {
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 17 });
        assert!(motor.hosted_state(id).is_none());
    }
    assert_eq!(ssh.0.load(Ordering::SeqCst), 2);
    assert!(h.lease_bodies.lock().unwrap().is_empty());
    assert_eq!(motor.hosted_health().unwrap()["hosted_routed_jobs_1h"], 0);
}

struct DirectPeer(AtomicUsize);
type Connection<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + 'a>,
>;
impl Connector for DirectPeer {
    fn connect_to<'a>(
        &'a self,
        id: Uuid,
        _instance: &'a str,
        _state_root: Option<&'a str>,
    ) -> Connection<'a> {
        self.connect(id)
    }
    fn connect(
        &self,
        id: Uuid,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + '_>,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: true,
                        platforms: vec!["linux".into(), "windows".into()],
                        return_paths: true,
                    },
                )
                .await
                .unwrap();
                while let Some(frame) = read_frame_async(&mut peer).await.unwrap() {
                    match frame {
                        Frame::Heartbeat { .. } => {}
                        Frame::Have { repositories, .. } => {
                            write_frame_async(
                                &mut peer,
                                &Frame::HaveReply {
                                    repositories,
                                    git_dependencies: Some(vec![]),
                                },
                            )
                            .await
                            .unwrap();
                        }
                        Frame::Run(run) => {
                            let mut accepted = Accepted::new(id, 0);
                            accepted.platform = run.platform;
                            accepted.network = run.network;
                            write_frame_async(&mut peer, &Frame::Accepted(accepted))
                                .await
                                .unwrap();
                            write_frame_async(
                                &mut peer,
                                &Frame::Terminal(TerminalRecord::new(
                                    id,
                                    Outcome::Exit { code: 17 },
                                    0,
                                    0,
                                    0,
                                )),
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        other => panic!("unexpected direct frame: {other:?}"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn crate::Duplex>)
        })
    }
}

#[tokio::test]
async fn stage1_pre_run_failures_are_typed_and_never_submit_directly() {
    for behavior in [Behavior::Refuse("internal_failure"), Behavior::DropHave] {
        let h = Harness::new(behavior).await;
        h.release_admitted.store(true, Ordering::SeqCst);
        let (scratch, repo) = committed_repo();
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone())
            .unwrap();
        let stream = motor.run(request(&repo)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        let reason = if matches!(behavior, Behavior::DropHave) {
            RefusalReason::Unreachable
        } else {
            RefusalReason::RequestRejected
        };
        assert_eq!(terminal.outcome, Outcome::RefusedBeforeStart { reason });
        assert!(terminal
            .refusal_detail
            .unwrap()
            .contains("hosted stage-1 routing (hosted_repositories)"));
        let state = motor.hosted_state(id).unwrap();
        assert!(!state.run_sent);
        assert!(state.hosted_no_fallback);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stage1_validation_and_lease_refusals_name_the_route_and_never_fall_back() {
    for refusal in [
        None,
        Some((403, "unauthorized")),
        Some((409, "sandbox_lost")),
        Some((409, "job_admitted")),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        if let Some((status, code)) = refusal {
            h.broker.lock().unwrap().knobs.refusal = Some((status, code.into()));
        }
        let (scratch, repo) = committed_repo();
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone())
            .unwrap();
        let mut request = request(&repo);
        if refusal.is_none() {
            request.timeout = None;
        }
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        let reason = if refusal == Some((403, "unauthorized")) {
            RefusalReason::RunnerAuthenticationFailed
        } else {
            RefusalReason::RequestRejected
        };
        assert_eq!(terminal.outcome, Outcome::RefusedBeforeStart { reason });
        assert!(terminal
            .refusal_detail
            .unwrap()
            .contains("hosted stage-1 routing (hosted_repositories)"));
        assert!(!motor.hosted_state(id).unwrap().run_sent);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
        assert!(!h
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with("/reattach")));
    }
}

#[tokio::test]
async fn stage1_lost_run_ack_reattaches_without_direct_fallback() {
    let h = Harness::new(Behavior::DropRunAck).await;
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone()).unwrap();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(motor.hosted_state(id).unwrap().run_sent);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert!(h
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|path| path.ends_with("/reattach")));
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stage1_no_capacity_retries_the_identical_body_then_runs_on_the_host() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().extend([
        (503, serde_json::json!({"error":"no_capacity"}), "0"),
        (503, serde_json::json!({"error":"no_capacity"}), "1"),
    ]);
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone()).unwrap();
    let started = tokio::time::Instant::now();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "Retry-After is clamped to at least one second"
    );
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(
        bodies.iter().all(|body| body == &bodies[0]),
        "Retries must preserve every field, including job_id and requester key"
    );
    let hashes: Vec<_> = bodies
        .iter()
        .map(|body| motor_lease::sha256_hex(body))
        .collect();
    assert!(hashes.iter().all(|hash| hash == &hashes[0]));
    let body: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(body["job_id"], id.to_string());
    assert_eq!(body["allow_spill"], false);
    assert_eq!(motor.hosted_state(id).unwrap().placement_attempts, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stage1_no_capacity_and_provisioning_share_one_bounded_queue_deadline() {
    for provisioning in [false, true] {
        let h = Harness::new(Behavior::CompleteRun).await;
        h.lease_replies.lock().unwrap().push_back((
            503,
            serde_json::json!({"error":"no_capacity"}),
            "0",
        ));
        if provisioning {
            h.lease_replies.lock().unwrap().push_back((
                202,
                serde_json::json!({"status":"provisioning", "retry_after_ms":60000}),
                "",
            ));
        } else {
            h.lease_replies.lock().unwrap().push_back((
                503,
                serde_json::json!({"error":"no_capacity"}),
                "9999999999999999999",
            ));
        }
        let (scratch, repo) = committed_repo();
        let config = Config {
            queue_wait_limit_s: 2,
            ..config(&h, &repo)
        };
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
        let started = tokio::time::Instant::now();
        let stream = motor.run(request(&repo)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "Neither retry can restart the queue-wait budget"
        );
        let reason = if provisioning {
            RefusalReason::Unreachable
        } else {
            RefusalReason::RunnerFull
        };
        assert_eq!(terminal.outcome, Outcome::RefusedBeforeStart { reason });
        assert_eq!(terminal.retry_after_ms, Some(30000));
        assert!(terminal
            .refusal_detail
            .unwrap()
            .contains("hosted stage-1 routing (hosted_repositories)"));
        assert!(!motor.hosted_state(id).unwrap().run_sent);
        assert_eq!(h.lease_bodies.lock().unwrap().len(), 2);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn unlisted_spill_eligible_job_still_withdraws_and_sends_allow_spill_true() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.refusal = Some((503, "no_capacity".into()));
    let (scratch, repo) = committed_repo();
    let (_listed_scratch, listed) = committed_repo();
    let peer = super::queue_tests::ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        config(&h, &listed),
        &scratch.0.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert!(matches!(terminal.outcome, Outcome::Exit { .. }));
    super::queue_tests::assert_resubmitted_once(id, &log.lock().unwrap(), 300);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1, "Spill does not retry no_capacity");
    assert_eq!(bodies[0]["allow_spill"], true);
    assert_eq!(bodies[0]["avoid_host"], "test-host");
    assert_eq!(motor.hosted_health().unwrap()["hosted_routed_jobs_1h"], 0);
}

#[tokio::test]
async fn stage1_cloud_no_spill_reply_honors_retry_after_and_preserves_the_stored_request() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/no-spill-lease.json"
    ))
    .unwrap();
    let delay = vector["reply"]["headers"]["Retry-After"].as_str().unwrap();
    assert_eq!(delay, "5");
    assert_eq!(
        serde_json::to_string(&vector["reply"]["body"]).unwrap(),
        vector["reply"]["body_bytes"]
    );
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().extend([
        (
            vector["reply"]["status"].as_u64().unwrap() as u16,
            vector["reply"]["body"].clone(),
            "5",
        ),
        (
            202,
            serde_json::json!({"status":"provisioning", "retry_after_ms":1000}),
            "",
        ),
    ]);
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone()).unwrap();
    let start = tokio::time::Instant::now();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(
        start.elapsed() >= Duration::from_secs(delay.parse::<u64>().unwrap() + 1),
        "The 503 Retry-After and subsequent provisioning delay must both be honored"
    );
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(bodies.iter().all(|body| body == &bodies[0]));
    let sent: motor_lease::LeaseRequest = serde_json::from_slice(&bodies[0]).unwrap();
    let saved = motor.hosted_state(id).unwrap().lease_request.unwrap();
    assert_eq!(saved.allow_spill, Some(false));
    assert_eq!(saved.avoid_host, None);
    assert_eq!(
        crate::hosted::tests::canonical_lease_bytes(&saved),
        crate::hosted::tests::canonical_lease_bytes(&sent)
    );
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}
