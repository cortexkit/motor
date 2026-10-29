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

pub(super) fn control_placement() -> motor_lease::Placement {
    motor_lease::Placement {
        host: "control-host".into(),
        vm: Some("t1".into()),
    }
}

pub(super) fn request(workspace: &Path) -> RunRequest {
    let args = crate::cli::RunArgs {
        cwd: Some(workspace.into()),
        command: vec!["true".into()],
        timeout: 30,
        ..crate::cli::RunArgs::default()
    };
    crate::cli::build_request(&args).unwrap()
}

pub(super) fn lease_bodies(h: &Harness) -> Vec<serde_json::Value> {
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

#[test]
fn repository_placement_hint_config_roundtrips_absent_null_and_present() {
    for input in ["{}", r#"{"hosted_repositories_placement_hint":null}"#] {
        let config: Config = serde_json::from_str(input).unwrap();
        config.validate().unwrap();
        assert_eq!(config.hosted_repositories_placement_hint, None);
        let wire = serde_json::to_value(config).unwrap();
        assert!(wire.get("hosted_repositories_placement_hint").is_none());
        let restored: Config = serde_json::from_value(wire).unwrap();
        assert_eq!(restored.hosted_repositories_placement_hint, None);
    }
    let config: Config = serde_json::from_value(serde_json::json!({
        "hosted_repositories": ["/home/operator/project"],
        "hosted_repositories_placement_hint": {"host":"control-host","vm":"t1"}
    }))
    .unwrap();
    config.validate().unwrap();
    let wire = serde_json::to_value(config).unwrap();
    assert_eq!(
        wire["hosted_repositories_placement_hint"],
        serde_json::json!({"host":"control-host","vm":"t1"})
    );
    let restored: Config = serde_json::from_value(wire).unwrap();
    assert_eq!(
        restored.hosted_repositories_placement_hint,
        Some(control_placement())
    );
}

#[test]
fn repository_placement_hint_rejects_invalid_names_and_null_vm() {
    for hint in [
        serde_json::json!({"host":"","vm":"t1"}),
        serde_json::json!({"host":"control-host","vm":null}),
        serde_json::json!({"host":"control-host","vm":""}),
        serde_json::json!({"host":"control host","vm":"t1"}),
        serde_json::json!({"host":"control-host","vm":"a/b"}),
    ] {
        let config: Config = serde_json::from_value(serde_json::json!({
            "hosted_repositories_placement_hint": hint
        }))
        .unwrap();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("hosted_repositories_placement_hint"));
    }
}

#[tokio::test]
async fn repository_routed_lease_carries_the_configured_hint_without_caching_it_for_other_routes() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let mut config = config(&h, &repo);
    config.hosted_repositories_placement_hint = Some(control_placement());
    let root = scratch.0.join("state");
    let motor = Motor::with_connector(config, &root, ssh.clone()).unwrap();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["placement_hint"],
        serde_json::json!({"host":"control-host","vm":"t1"})
    );
    assert_eq!(bodies[0]["allow_spill"], false);
    assert_eq!(bodies[0]["avoid_host"], serde_json::Value::Null);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    drop(motor);

    let restored = Motor::with_connector(
        Config {
            hosted_fleet_routing: true,
            ..h.config()
        },
        &root,
        ssh.clone(),
    )
    .unwrap();
    let state = restored.hosted_state(id).unwrap();
    assert_eq!(state.repository_placement_hint, Some(control_placement()));
    assert_eq!(
        state.lease_request.unwrap().placement_hint,
        Some(control_placement())
    );
    let stream = restored.run(request(&repo)).unwrap();
    let fleet_id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(restored.hosted_state(fleet_id).unwrap().fleet_routed);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["placement_hint"], serde_json::Value::Null);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
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

pub(super) struct DirectPeer(pub(super) AtomicUsize);
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
                        platforms: vec!["linux".into(), "windows".into(), "macos".into()],
                        return_paths: true,
                        return_chunked: false,
                        sibling_paths: false,
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

/// A routed job has no build server to go back to, so a lease body the
/// broker cannot take is a typed refusal: one ck-motor finds invalid is never
/// sent and names the member, and one the broker refuses with 400
/// `bad_request` says so. Neither calls it a lease mismatch.
#[tokio::test]
async fn stage1_invalid_lease_bodies_are_typed_refusals_not_lease_mismatch() {
    for (timeout, broker_refusal, detail) in [
        (21_601, false, "hosted_request_invalid: timeout_s"),
        (5, false, "hosted_request_invalid: timeout_s"),
        (30, true, "hosted_request_invalid"),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        if broker_refusal {
            h.broker.lock().unwrap().knobs.refusal = Some((400, "bad_request".into()));
        }
        let (scratch, repo) = committed_repo();
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone())
            .unwrap();
        let request = request(&repo).with_timeout(timeout);
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected
            }
        );
        let refusal = terminal.refusal_detail.unwrap();
        assert!(
            refusal.contains("hosted stage-1 routing (hosted_repositories)"),
            "{refusal}"
        );
        assert!(refusal.contains(detail), "{refusal}");
        assert!(!refusal.contains("lease_mismatch"), "{refusal}");
        let state = motor.hosted_state(id).unwrap();
        assert_eq!(state.hosted_code.as_deref(), Some("hosted_request_invalid"));
        assert!(state.hosted_no_fallback && !state.run_sent);
        assert_eq!(lease_bodies(&h).len(), usize::from(broker_refusal));
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}

async fn assert_stage1_bad_request(status: u16, body: String, expected_field: Option<&str>) {
    let broker = crate::hosted::test_http::Http::with_reply_bytes(move |path, _| {
        if path == "/v1/leases" {
            (status, body.as_bytes().to_owned())
        } else {
            (
                404,
                serde_json::to_vec(&serde_json::json!({"error": "job_unknown"})).unwrap(),
            )
        }
    })
    .await;
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let mut cfg = config(&h, &repo);
    cfg.broker_url = Some(broker.url.clone());
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(cfg, &scratch.0.join("state"), ssh.clone()).unwrap();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    let detail = expected_field.map_or_else(
        || "hosted_request_invalid".to_owned(),
        |field| format!("hosted_request_invalid: {field}"),
    );
    assert_eq!(
        terminal.refusal_detail.as_deref(),
        Some(format!("hosted stage-1 routing (hosted_repositories): {detail}").as_str())
    );
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.hosted_code.as_deref(), Some("hosted_request_invalid"));
    assert!(state.hosted_no_fallback && !state.run_sent);
    assert_eq!(
        broker
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/leases")
            .count(),
        1
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    let text = captured.text();
    let warnings: Vec<_> = text
        .lines()
        .filter(|line| line.contains("hosted lease body refused by broker"))
        .collect();
    assert_eq!(warnings.len(), 1, "{text}");
    for member in [
        "WARN".to_owned(),
        format!("job_id={id}"),
        "route=repository_routed".into(),
        "http_status=400".into(),
        "broker_code=bad_request".into(),
        format!("field={}", expected_field.unwrap_or("unknown")),
    ] {
        assert!(warnings[0].contains(&member), "{member}: {text}");
    }
}

#[tokio::test]
async fn stage1_cloud_bad_request_fields_reach_warning_and_refusal() {
    let replies = crate::hosted::tests::cloud_bad_request_replies();
    assert_eq!(replies.len(), 9);
    for (status, body) in replies {
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_stage1_bad_request(status, body, parsed["field"].as_str()).await;
    }
}

#[tokio::test]
async fn stage1_bad_request_omits_oversized_fields() {
    let (status, body) = crate::hosted::tests::cloud_bad_request_replies().remove(0);
    let mut parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    parsed["field"] = "f".repeat(65).into();
    assert_stage1_bad_request(status, parsed.to_string(), None).await;
}

#[tokio::test]
async fn stage1_bad_request_omits_nonprintable_fields() {
    let (status, body) = crate::hosted::tests::cloud_bad_request_replies().remove(0);
    for field in [
        "timeout_s\n",
        "timeout_s\r",
        "timeout_s\t",
        "timeout_s\0",
        "timeout_s\u{7f}",
        "timeout_é",
    ] {
        let mut parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        parsed["field"] = field.into();
        assert_stage1_bad_request(status, parsed.to_string(), None).await;
    }
}

#[tokio::test]
async fn stage1_bad_request_accepts_64_printable_ascii_characters() {
    let (status, body) = crate::hosted::tests::cloud_bad_request_replies().remove(0);
    let mut parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let field = format!("{} ~", "f".repeat(62));
    parsed["field"] = field.clone().into();
    assert_stage1_bad_request(status, parsed.to_string(), Some(&field)).await;
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
async fn hinted_no_capacity_retries_the_identical_body_then_runs_without_fallback() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().extend([
        (503, serde_json::json!({"error":"no_capacity"}), "0"),
        (503, serde_json::json!({"error":"no_capacity"}), "1"),
    ]);
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let mut config = config(&h, &repo);
    config.hosted_repositories_placement_hint = Some(control_placement());
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let started = tokio::time::Instant::now();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(started.elapsed() >= Duration::from_secs(2));
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(bodies.iter().all(|body| body == &bodies[0]));
    let body: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(body["job_id"], id.to_string());
    assert_eq!(
        body["placement_hint"],
        serde_json::json!({"host":"control-host","vm":"t1"})
    );
    assert_eq!(body["allow_spill"], false);
    assert_eq!(motor.hosted_state(id).unwrap().placement_attempts, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hinted_no_capacity_exhausts_the_queue_budget_without_fallback() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies
        .lock()
        .unwrap()
        .extend((0..10).map(|_| (503, serde_json::json!({"error":"no_capacity"}), "1")));
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let mut config = config(&h, &repo);
    config.hosted_repositories_placement_hint = Some(control_placement());
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let mut request = request(&repo);
    request.queue_wait_limit_s = Some(3);
    let started = tokio::time::Instant::now();
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = tokio::time::timeout(Duration::from_secs(10), records(stream))
        .await
        .expect("capacity waits must end within the queue budget");
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert!(terminal.refusal_detail.unwrap().contains("hosted_capacity"));
    assert!(terminal.retry_after_ms.is_some());
    let bodies = h.lease_bodies.lock().unwrap();
    assert!(
        bodies.len() >= 2,
        "capacity must be retried before refusing"
    );
    assert!(bodies.iter().all(|body| body == &bodies[0]));
    let body: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(body["allow_spill"], false);
    assert_eq!(
        body["placement_hint"],
        serde_json::json!({"host":"control-host","vm":"t1"})
    );
    assert!(motor
        .storage
        .job(id)
        .unwrap()
        .routed_direct_reason
        .is_none());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        lease_settlement_requests(&h),
        vec![format!("/v1/leases/{id}/release")]
    );
}

#[tokio::test]
async fn hinted_no_host_fits_refuses_without_build_server_correction() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.max_vcpus = Some(8);
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let mut config = config(&h, &repo);
    config.hosted_repositories_placement_hint = Some(control_placement());
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let mut request = request(&repo);
    request.weight_hint = Some(16);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    assert!(terminal
        .refusal_detail
        .unwrap()
        .contains("hosted_no_host_fits"));
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["allow_spill"], false);
    assert_eq!(
        bodies[0]["placement_hint"],
        serde_json::json!({"host":"control-host","vm":"t1"})
    );
    let job = motor.storage.job(id).unwrap();
    assert!(job.routed_direct_reason.is_none());
    assert!(job.hosted.unwrap().repository_routed);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        lease_settlement_requests(&h),
        vec![format!("/v1/leases/{id}/release")]
    );
}

#[tokio::test(start_paused = true)]
async fn stage1_no_capacity_and_provisioning_share_one_bounded_queue_deadline() {
    let _clock = HeldClock::new();
    for provisioning in [false, true] {
        let waits = crate::logging::Captured::default();
        let _logs = crate::logging::scoped(waits.clone());
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
        let config = config(&h, &repo);
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
        let started = tokio::time::Instant::now();
        let mut run_request = request(&repo);
        run_request.queue_wait_limit_s = Some(2);
        let stream = motor.run(run_request).unwrap();
        let id = stream.job_id;
        let job = tokio::spawn(records(stream));
        for replies in 1..=2 {
            // A recorded lease request does not mean its reply has been parsed.
            // Hold time still until the client has fixed the next retry's wake time.
            until("the broker retry wait", || {
                waits.text().matches("hosted broker retry wait").count() == replies
            })
            .await;
            assert_eq!(h.lease_bodies.lock().unwrap().len(), replies);
            assert!(!job.is_finished(), "the job must wait out its queue budget");
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let terminal = terminal_without_more_time(job).await;
        assert_eq!(started.elapsed(), Duration::from_secs(2));
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
    let mut config = config(&h, &listed);
    config.hosted_repositories_placement_hint = Some(control_placement());
    let motor = Motor::with_connector(config, &scratch.0.join("state"), Arc::new(peer)).unwrap();
    let mut request = request(&repo);
    request.queue_wait_limit_s = Some(300);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert!(matches!(terminal.outcome, Outcome::Exit { .. }));
    super::queue_tests::assert_resubmitted_once(id, &log.lock().unwrap(), 300);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1, "Spill does not retry no_capacity");
    assert_eq!(bodies[0]["allow_spill"], true);
    assert_eq!(bodies[0]["avoid_host"], "test-host");
    assert_eq!(bodies[0]["placement_hint"], serde_json::Value::Null);
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
pub(super) fn lease_settlement_requests(h: &Harness) -> Vec<String> {
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
#[tokio::test(start_paused = true)]
async fn stage1_no_host_fits_runs_on_the_build_server_once_and_fitting_jobs_stay_hosted() {
    let clock = HeldClock::new();
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
    let job = tokio::spawn(records(stream));
    until("the build-server Run", || {
        !log.lock().unwrap().runs.is_empty()
    })
    .await;
    // Freeze the budget through routing, then let the direct peer's simulated
    // queue timer run so it can send Started and Terminal.
    drop(clock);
    let (output, terminal) = job.await.unwrap();
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
        // Routing used none of the request's 30-second queue-wait budget.
        assert_eq!(log.runs, vec![(id, 30)]);
        assert!(log.withdraws.is_empty());
    }
    assert!(motor.hosted_state(id).is_none());
    let job = motor.storage.job(id).unwrap();
    assert!(job.hosted.is_none());
    assert_eq!(job.routed_direct_reason.as_deref(), Some("no_host_fits"));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);

    let _hosted_clock = HeldClock::new();
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
        motor.close().await;
        id
    };
    let motor = Motor::with_connector(
        config(&h, &repo),
        &scratch.0.join("state"),
        Arc::new(super::queue_tests::ResubmitPeer {
            log: log.clone(),
            end: super::queue_tests::ResubmitEnd::Exit,
        }),
    )
    .unwrap();
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
    large.queue_wait_limit_s = Some(300);
    let stream = motor.run(large).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
    assert_eq!(lease_bodies(&h).len(), 2);
    let limit = motor.storage.job(id).unwrap().queue_wait_limit_s;
    assert!((297..300).contains(&limit), "{limit}");
    assert_eq!(log.lock().unwrap().runs, vec![(id, limit)]);
}

/// CLOUD's pinned `no_host_fits` reply, byte for byte and without
/// Retry-After, goes through ck-motor's real broker response parser: the
/// routed 16-vCPU job asks once and then runs on the build server.
#[tokio::test(start_paused = true)]
async fn stage1_cloud_no_host_fits_reply_routes_the_job_to_the_build_server() {
    // Loopback I/O and filesystem work must not spend the queue budget whose
    // exact remaining value is asserted below.
    let clock = HeldClock::new();
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/no-host-fits-lease.json"
    ))
    .unwrap();
    let reply = &vector["reply"];
    assert_eq!(reply["headers"]["Retry-After"], serde_json::Value::Null);
    // The harness writes the stubbed body with serde_json, so these are the
    // exact bytes CLOUD recorded.
    assert_eq!(
        serde_json::to_string(&reply["body"]).unwrap(),
        reply["body_bytes"]
    );
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().push_back((
        reply["status"].as_u64().unwrap() as u16,
        reply["body"].clone(),
        "",
    ));
    let (scratch, repo) = committed_repo();
    let peer = super::queue_tests::ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), Arc::new(peer)).unwrap();
    let mut large = request(&repo);
    large.weight_hint = Some(vector["request"]["body"]["vcpus"].as_u64().unwrap() as u32);
    let stream = motor.run(large).unwrap();
    let id = stream.job_id;
    let job = tokio::spawn(records(stream));
    until("the build-server Run", || {
        !log.lock().unwrap().runs.is_empty()
    })
    .await;
    // The Run now contains the frozen queue budget. Allow the direct peer's
    // simulated queue timer to advance before awaiting its terminal record.
    drop(clock);
    let (_, terminal) = job.await.unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["vcpus"], vector["request"]["body"]["vcpus"]);
    assert_eq!(
        bodies[0]["allow_spill"],
        vector["request"]["body"]["allow_spill"]
    );
    assert_eq!(*h.requests.lock().unwrap(), vec!["/v1/leases".to_owned()]);
    assert_eq!(log.lock().unwrap().runs, vec![(id, 30)]);
    assert_eq!(
        motor
            .storage
            .job(id)
            .unwrap()
            .routed_direct_reason
            .as_deref(),
        Some("no_host_fits")
    );
    assert_eq!(motor.hosted_health().unwrap()["hosted_no_host_fits_1h"], 1);
}

/// Keeps tokio's paused clock still until dropped, so only
/// `tokio::time::advance` moves it. A paused clock otherwise jumps to the next
/// timer whenever the runtime is idle, including while it waits for the
/// harness's real loopback sockets, which would fire queue-wait deadlines in
/// the middle of a lease request. Tokio makes no such jump while a blocking
/// task runs, so this holds one open.
pub(super) struct HeldClock {
    _release: std::sync::mpsc::Sender<()>,
}
impl HeldClock {
    pub(super) fn new() -> Self {
        let (release, held) = std::sync::mpsc::channel::<()>();
        tokio::task::spawn_blocking(move || {
            let _ = held.recv();
        });
        Self { _release: release }
    }
}

/// Waits in real time, without moving the held clock, until `done` holds.
pub(super) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(5)))
            .await
            .unwrap();
    }
}

/// Waits until job `id` has had `leases` lease requests and its host refused
/// the last granted one as full: ck-motor gives a placement attempt back only
/// for that refusal, just before it starts its two-second wait. Then waits
/// for the wall clock to pass the issue second of every run token so far, so
/// the next request needs no wait for a later token issue time; the run
/// token's issue time is in wall-clock seconds, which the held clock does not
/// move.
async fn until_host_full_wait(h: &Harness, motor: &Motor, id: Uuid, leases: usize) {
    until("the host-full wait", || {
        h.lease_bodies.lock().unwrap().len() == leases
            && h.tokens.load(Ordering::SeqCst) == leases
            && motor
                .hosted_state(id)
                .is_some_and(|state| state.placement_attempts == 0)
    })
    .await;
    let issued = h.iats.lock().unwrap().iter().copied().max().unwrap();
    until("a later token issue second", || {
        crate::hosted::now_s() > issued
    })
    .await;
}

/// The terminal record of the job whose records `job` collects, awaited in
/// real time: the job must end without the held clock moving any further.
pub(super) async fn terminal_without_more_time(
    job: tokio::task::JoinHandle<(Vec<u8>, TerminalRecord)>,
) -> TerminalRecord {
    until("the terminal record", || job.is_finished()).await;
    job.await.unwrap().1
}

/// The broker grants a routed job's lease from a stale view of the host, and
/// the host refuses it as full (or out of capacity) twice before admitting
/// it. The job waits and asks again with the same job ID and lease body,
/// then runs once on the host; nothing is released and nothing reaches the
/// build server. The clock is held, so each wait ends only when the test
/// moves time on by two seconds.
#[tokio::test(start_paused = true)]
async fn stage1_host_full_waits_and_asks_again_until_the_host_admits_the_job() {
    let _clock = HeldClock::new();
    for refusals in [["host_full", "host_full"], ["host_full", "capacity"]] {
        let h = Harness::new(Behavior::CompleteRun).await;
        h.host_refusals.lock().unwrap().extend(refusals);
        let (scratch, repo) = committed_repo();
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config(&h, &repo), &scratch.0.join("state"), ssh.clone())
            .unwrap();
        let started = tokio::time::Instant::now();
        let stream = motor.run(request(&repo)).unwrap();
        let id = stream.job_id;
        let job = tokio::spawn(records(stream));
        for leases in [1, 2] {
            until_host_full_wait(&h, &motor, id, leases).await;
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        let terminal = terminal_without_more_time(job).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 }, "{refusals:?}");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(4),
            "each host refusal waits two seconds before asking again"
        );
        assert_eq!(h.runs.load(Ordering::SeqCst), 1);
        assert_eq!(h.tokens.load(Ordering::SeqCst), 3);
        assert!(h.host_refusals.lock().unwrap().is_empty());
        let bodies: Vec<motor_lease::LeaseRequest> = h
            .lease_bodies
            .lock()
            .unwrap()
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect();
        assert_eq!(bodies.len(), 3);
        for body in &bodies {
            assert_eq!(body.job_id, id.to_string());
            assert_eq!(
                crate::hosted::tests::canonical_lease_bytes(body),
                crate::hosted::tests::canonical_lease_bytes(&bodies[0])
            );
        }
        assert!(lease_settlement_requests(&h).is_empty());
        let state = motor.hosted_state(id).unwrap();
        assert!(state.repository_routed && state.run_sent);
        assert_eq!(state.placement_attempts, 1);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}

/// A host that stays full uses up the routed job's queue-wait budget and no
/// more, then gives the same typed, retryable refusal as broker no_capacity.
/// The unused reservation is released; nothing reaches the build server.
/// With a three-second budget and the clock held, the job asks exactly
/// twice: at the start and after one two-second wait. A second wait would
/// end past the deadline, so the job waits out the last second instead.
#[tokio::test(start_paused = true)]
async fn stage1_host_full_until_the_queue_budget_ends_refuses_like_no_capacity() {
    let _clock = HeldClock::new();
    let h = Harness::new(Behavior::Refuse("host_full")).await;
    let (scratch, repo) = committed_repo();
    let config = config(&h, &repo);
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let started = tokio::time::Instant::now();
    let mut run_request = request(&repo);
    run_request.queue_wait_limit_s = Some(3);
    let stream = motor.run(run_request).unwrap();
    let id = stream.job_id;
    let job = tokio::spawn(records(stream));
    until_host_full_wait(&h, &motor, id, 1).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    until("the second host refusal", || {
        h.tokens.load(Ordering::SeqCst) == 2
    })
    .await;
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)))
        .await
        .unwrap();
    assert!(
        !job.is_finished(),
        "the job is refused when its queue-wait budget ends, not before"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    let terminal = terminal_without_more_time(job).await;
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.retry_after_ms, Some(2000));
    let detail = terminal.refusal_detail.unwrap();
    assert!(
        detail.contains("hosted stage-1 routing (hosted_repositories): hosted_capacity"),
        "{detail}"
    );
    let bodies: Vec<motor_lease::LeaseRequest> = h
        .lease_bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1].job_id, id.to_string());
    assert_eq!(
        crate::hosted::tests::canonical_lease_bytes(&bodies[1]),
        crate::hosted::tests::canonical_lease_bytes(&bodies[0])
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    assert_eq!(
        lease_settlement_requests(&h),
        vec![format!("/v1/leases/{id}/release")]
    );
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

/// The queue-wait budget can end while a lease request is in flight. Once
/// the host has refused the job as full, the job is waiting for that host,
/// so cutting off the request that asks again still gives the host-full
/// refusal (runner full, retry after 2 s), not the transport failure the
/// cut-off causes, and the unused reservation is released. A broker that
/// has not answered the job's first request when the budget ends is
/// unreachable.
#[tokio::test(start_paused = true)]
async fn stage1_queue_budget_ending_during_a_lease_request_refuses_for_what_the_job_waited_on() {
    let _clock = HeldClock::new();
    for after_host_full in [true, false] {
        let h = Harness::new(Behavior::Refuse("host_full")).await;
        let leases = if after_host_full { 2 } else { 1 };
        h.unanswered_lease.store(leases, Ordering::SeqCst);
        let (scratch, repo) = committed_repo();
        let config = config(&h, &repo);
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
        let started = tokio::time::Instant::now();
        let mut run_request = request(&repo);
        run_request.queue_wait_limit_s = Some(3);
        let stream = motor.run(run_request).unwrap();
        let id = stream.job_id;
        let job = tokio::spawn(records(stream));
        if after_host_full {
            until_host_full_wait(&h, &motor, id, 1).await;
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        until("the unanswered lease request", || {
            h.lease_bodies.lock().unwrap().len() == leases
        })
        .await;
        tokio::time::advance(started + Duration::from_secs(3) - tokio::time::Instant::now()).await;
        let terminal = terminal_without_more_time(job).await;
        assert_eq!(started.elapsed(), Duration::from_secs(3));
        let (reason, code, retry_after_ms) = if after_host_full {
            (RefusalReason::RunnerFull, "hosted_capacity", 2000)
        } else {
            (RefusalReason::Unreachable, "hosted_provisioning", 1000)
        };
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart { reason },
            "after_host_full={after_host_full}"
        );
        assert_eq!(terminal.retry_after_ms, Some(retry_after_ms));
        let detail = terminal.refusal_detail.unwrap();
        assert!(
            detail.contains(&format!(
                "hosted stage-1 routing (hosted_repositories): {code}"
            )),
            "{detail}"
        );
        assert_eq!(h.tokens.load(Ordering::SeqCst), leases - 1);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert!(!motor.hosted_state(id).unwrap().run_sent);
        if after_host_full {
            assert_eq!(
                lease_settlement_requests(&h),
                vec![format!("/v1/leases/{id}/release")]
            );
        }
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}
