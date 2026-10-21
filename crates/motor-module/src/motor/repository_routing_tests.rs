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

/// Broker request paths that release, finish or reattach to a hosted lease.
fn lease_settlement_requests(h: &Harness) -> Vec<String> {
    h.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|path| {
            path.ends_with("/release") || path.ends_with("/finish") || path.ends_with("/reattach")
        })
        .cloned()
        .collect()
}

/// A routed job asking for more vCPUs than any hosted host holds gets one
/// lease request answered `no_host_fits`, then runs on the build server with
/// its real exit code. Nothing was booked, so nothing is released or finished.
/// A job that fits keeps the hosted route.
#[tokio::test]
async fn stage1_no_host_fits_runs_on_the_build_server_once_and_fitting_jobs_stay_hosted() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.max_vcpus = Some(8);
    let (scratch, repo) = committed_repo();
    let peer = super::queue_tests::ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), Arc::new(peer)).unwrap();

    let mut large = request(&repo);
    large.weight_hint = Some(16);
    let stream = motor.run(large).unwrap();
    let id = stream.job_id;
    let (output, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
    assert_eq!(output, b"build server output\n");
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1, "no_host_fits is never retried");
    assert_eq!(bodies[0]["vcpus"], 16);
    assert_eq!(bodies[0]["allow_spill"], false);
    assert_eq!(*h.requests.lock().unwrap(), vec!["/v1/leases".to_owned()]);
    assert!(lease_settlement_requests(&h).is_empty());
    {
        let log = log.lock().unwrap();
        // The lease answer arrives within a second, so none of the default
        // 300-second queue-wait limit is taken off.
        assert_eq!(log.runs, vec![(id, 300)]);
        assert!(log.withdraws.is_empty());
    }
    assert!(motor.hosted_state(id).is_none());
    let job = motor.storage.job(id).unwrap();
    assert!(job.hosted.is_none());
    assert_eq!(job.routed_direct_reason.as_deref(), Some("no_host_fits"));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);

    let mut fitting = request(&repo);
    fitting.weight_hint = Some(8);
    let stream = motor.run(fitting).unwrap();
    let fitting_id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let state = motor.hosted_state(fitting_id).unwrap();
    assert!(state.repository_routed && state.run_sent);
    assert_eq!(lease_bodies(&h)[1]["vcpus"], 8);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        log.lock().unwrap().runs.len(),
        1,
        "the fitting job never reached the build server"
    );

    let health = motor.hosted_health().unwrap();
    assert_eq!(health["hosted_no_host_fits_1h"], 1);
    assert_eq!(health["hosted_routed_jobs_1h"], 1);
}

/// The decision to run on the build server is stored before the Run frame is
/// sent. After a restart, with the repository still listed, attaching to the
/// job reads its output from the build server and never asks the broker again.
#[tokio::test]
async fn stage1_no_host_fits_decision_survives_restart_and_attaches_to_the_build_server() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.max_vcpus = Some(8);
    let (scratch, repo) = committed_repo();
    let peer = super::queue_tests::ResubmitPeer::new();
    let log = peer.log.clone();
    let id = {
        let motor =
            Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), Arc::new(peer))
                .unwrap();
        let mut large = request(&repo);
        large.weight_hint = Some(16);
        let stream = motor.run(large).unwrap();
        let id = stream.job_id;
        assert_eq!(records(stream).await.1.outcome, Outcome::Exit { code: 7 });
        id
    };
    // The job's task drops its handle on the store just after delivering the
    // terminal record; wait for the store's lock to be released before reopening.
    let motor = {
        let mut attempts = 0;
        loop {
            match Motor::with_connector(
                config(&h, &repo),
                &scratch.0.join("state"),
                Arc::new(super::queue_tests::ResubmitPeer { log: log.clone() }),
            ) {
                Ok(motor) => break motor,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && attempts < 100 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("reopen: {error}"),
            }
        }
    };
    assert!(motor.hosted_state(id).is_none());
    assert_eq!(
        motor
            .storage
            .job(id)
            .unwrap()
            .routed_direct_reason
            .as_deref(),
        Some("no_host_fits")
    );
    let (_, terminal) = records(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
    let log = log.lock().unwrap();
    assert_eq!(log.attaches, vec![id]);
    assert_eq!(log.runs.len(), 1, "a restart never submits again");
    assert_eq!(*h.requests.lock().unwrap(), vec!["/v1/leases".to_owned()]);
    assert_eq!(motor.hosted_health().unwrap()["hosted_no_host_fits_1h"], 1);
}

/// For a spilled job `no_host_fits` is an ordinary refusal before the Run
/// frame: the job goes back to the build server after one lease request, and
/// it does not count as a repository-routed decision.
#[tokio::test]
async fn spill_no_host_fits_resubmits_to_the_build_server_without_retrying() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.max_vcpus = Some(1);
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
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
    super::queue_tests::assert_resubmitted_once(id, &log.lock().unwrap(), 300);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1, "spill does not retry no_host_fits");
    assert_eq!(bodies[0]["allow_spill"], true);
    assert_eq!(
        motor
            .storage
            .job(id)
            .unwrap()
            .hosted
            .unwrap()
            .hosted_code
            .as_deref(),
        Some("hosted_no_host_fits")
    );
    assert_eq!(motor.hosted_health().unwrap()["hosted_no_host_fits_1h"], 0);
}

/// Time spent waiting on the broker before `no_host_fits` is taken off the
/// queue-wait limit the build server receives, once: here a one-second
/// `no_capacity` wait leaves 299 of the default 300 seconds.
#[tokio::test]
async fn stage1_no_host_fits_after_a_capacity_wait_sends_the_remaining_queue_wait() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.max_vcpus = Some(8);
    h.lease_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"no_capacity"}),
        "1",
    ));
    let (scratch, repo) = committed_repo();
    let peer = super::queue_tests::ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), Arc::new(peer)).unwrap();
    let mut large = request(&repo);
    large.weight_hint = Some(16);
    let stream = motor.run(large).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
    assert_eq!(lease_bodies(&h).len(), 2);
    let limit = motor.storage.job(id).unwrap().queue_wait_limit_s;
    assert!((297..300).contains(&limit), "{limit}");
    assert_eq!(log.lock().unwrap().runs, vec![(id, limit)]);
}
