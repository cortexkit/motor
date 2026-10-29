use super::repository_routing_tests::{control_placement, lease_bodies, request, DirectPeer};
use super::*;
use crate::hosted::{
    boat_tests::{committed_repo, records},
    tests::harness::{Behavior, Harness, NoSsh},
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn config(h: &Harness) -> Config {
    Config {
        hosted_fleet_routing: true,
        network_routing: true,
        ..h.config()
    }
}

#[test]
fn fleet_config_defaults_off_and_roundtrips() {
    let default: Config = serde_json::from_str("{}").unwrap();
    assert!(!default.hosted_fleet_routing);
    assert!(serde_json::to_value(&default)
        .unwrap()
        .get("hosted_fleet_routing")
        .is_none());
    let config: Config = serde_json::from_str(r#"{"hosted_fleet_routing":true}"#).unwrap();
    config.validate().unwrap();
    let restored: Config = serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
    assert!(restored.hosted_fleet_routing);
    assert!(!restored.hosted_runners);
    let mut old_wire = serde_json::to_value(crate::HostedJobState::default()).unwrap();
    assert!(old_wire
        .as_object_mut()
        .unwrap()
        .remove("fleet_routed")
        .is_none());
    assert!(old_wire
        .as_object_mut()
        .unwrap()
        .remove("fleet_no_spill")
        .is_none());
    let old_route: crate::HostedJobState = serde_json::from_value(old_wire).unwrap();
    assert!(!old_route.fleet_routed);
    assert!(!old_route.fleet_no_spill);
}

#[tokio::test]
async fn fleet_linux_default_platform_jobs_allow_hosted_spill() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let mut config = config(&h);
    config.spill_to_hosted = true;
    config.spill_after_s = 0;
    config.hosted_repositories_placement_hint = Some(control_placement());
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    for (platform, timeout) in [(None, 10), (Some(Platform::Linux), 7200)] {
        let mut request = request(&repo);
        request.platform = platform;
        request.timeout = Some(timeout);
        let stream = motor
            .run_classified(
                request,
                (None, None),
                false,
                false,
                false,
                Default::default(),
            )
            .unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        let state = motor.hosted_state(id).unwrap();
        assert!(state.fleet_routed && state.run_sent && state.hosted_no_fallback);
        assert!(!state.repository_routed && !state.fleet_no_spill);
        let restored: crate::HostedJobState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert!(restored.fleet_routed && restored.capacity_routed());
    }
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies
            .iter()
            .map(|b| b["timeout_s"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [10, 7200]
    );
    for body in bodies {
        assert_eq!(body["platform"], "linux-x64");
        assert_eq!(body["allow_spill"], true);
        assert_eq!(body["avoid_host"], serde_json::Value::Null);
        assert_ne!(
            body["placement_hint"],
            serde_json::to_value(control_placement()).unwrap()
        );
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 2);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_workspace_with_a_recorded_placement_leaves_affinity_to_the_broker() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let root = scratch.0.join("state");
    let motor = Motor::with_connector(config(&h), &root, ssh.clone()).unwrap();
    let stream = motor.run(request(&repo)).unwrap();
    let first_id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let placement = motor.hosted_state(first_id).unwrap().placement.unwrap();
    drop(motor);

    let restored = Motor::with_connector(config(&h), &root, ssh.clone()).unwrap();
    assert_eq!(
        restored.hosted_state(first_id).unwrap().placement,
        Some(placement)
    );
    let stream = restored.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(restored.hosted_state(id).unwrap().fleet_routed);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[0]["workspace_key_hash"],
        bodies[1]["workspace_key_hash"]
    );
    assert_eq!(bodies[1]["placement_hint"], serde_json::Value::Null);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_gate_takes_a_host_only_lease_and_sends_the_gate_mark() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    let mut request = request(&repo);
    request.queue_wait_limit_s = Some(1);
    let stream = motor
        .run_direct_authorized(request, None, Default::default(), true, false, [7; 32])
        .unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let route = motor.hosted_state(id).unwrap();
    assert!(route.fleet_routed && route.fleet_no_spill && route.run_sent);
    let job = motor.storage.job(id).unwrap();
    assert!(job.gate_job);
    assert_eq!(job.queue_wait_limit_s, 30 * 60);
    let leases = lease_bodies(&h);
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0]["allow_spill"], false);
    assert!(leases[0].get("gate_job").is_none());
    assert!(h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| { matches!(frame, Frame::Run(run) if run.job_id == id && run.gate_job && run.queue_wait_limit_s == 1800) }));
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_off_gate_uses_the_direct_runner_without_a_lease() {
    for hosted_runners in [false, true] {
        let h = Harness::new(Behavior::CompleteRun).await;
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
        let config = Config {
            hosted_runners,
            hosted_fleet_routing: false,
            ..config(&h)
        };
        let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
        let stream = motor
            .run_direct_authorized(
                request(&repo),
                None,
                Default::default(),
                false,
                false,
                [7; 32],
            )
            .unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 17 });
        assert!(motor.hosted_state(id).is_none());
        assert_eq!(motor.storage.job(id).unwrap().queue_wait_limit_s, 30 * 60);
        assert!(lease_bodies(&h).is_empty());
        assert_eq!(peer.0.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn fleet_gate_no_host_fits_corrects_to_the_direct_priority_lane() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.broker.lock().unwrap().knobs.refusal = Some((422, "no_host_fits".into()));
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    let stream = motor
        .run_direct_authorized(
            request(&repo),
            None,
            Default::default(),
            false,
            false,
            [7; 32],
        )
        .unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 17 });
    assert!(motor.hosted_state(id).is_none());
    let job = motor.storage.job(id).unwrap();
    assert!(job.gate_job);
    assert_eq!(job.routed_direct_reason.as_deref(), Some("no_host_fits"));
    assert_eq!(job.queue_wait_limit_s, 30 * 60);
    assert_eq!(lease_bodies(&h)[0]["allow_spill"], false);
    assert_eq!(peer.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_platform_and_timeout_exclusions_never_request_a_lease() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let mut config = config(&h);
    config.windows_runner_instance = Some("windows-test".into());
    config.windows_runner_state_root = Some("/home/test/windows".into());
    config.macos_runner = Some(crate::MacosRunner {
        host: "macos.example.invalid".into(),
        port: 2222,
        user: "motor".into(),
        host_key: ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .to_openssh()
            .unwrap(),
        instance: "macos-test".into(),
        state_root: "/Users/motor/state".into(),
    });
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    for (platform, timeout) in [
        (Some(Platform::Windows), Some(600)),
        (Some(Platform::Macos), Some(600)),
        (None, None),
        (None, Some(9)),
        (None, Some(21601)),
    ] {
        let mut request = request(&repo);
        request.platform = platform;
        request.timeout = timeout;
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 17 });
        assert!(motor.hosted_state(id).is_none());
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 5);
    assert!(lease_bodies(&h).is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn absent_fleet_key_preserves_global_hosted_validation_refusals() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let wire = serde_json::to_value(h.config()).unwrap();
    assert!(wire.get("hosted_fleet_routing").is_none());
    let config = serde_json::from_value(wire).unwrap();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    for timeout in [None, Some(9), Some(21601)] {
        let mut request = request(&repo);
        request.timeout = timeout;
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert!(matches!(
            terminal.outcome,
            Outcome::RefusedBeforeStart { .. }
        ));
        let route = motor.hosted_state(id).unwrap();
        assert!(!route.fleet_routed && !route.lease_attempted);
    }
    assert!(lease_bodies(&h).is_empty());
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let route = motor.hosted_state(id).unwrap();
    assert!(!route.fleet_routed && !route.repository_routed && route.run_sent);
    assert_eq!(lease_bodies(&h).len(), 1);
    assert_eq!(lease_bodies(&h)[0]["allow_spill"], true);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_repository_route_keeps_precedence_over_the_fleet_route() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let mut config = config(&h);
    config.hosted_repositories = vec![repo.to_str().unwrap().into()];
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    for timeout in [9, 600] {
        let stream = motor.run(request(&repo).with_timeout(timeout)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        let route = motor.hosted_state(id).unwrap();
        assert!(route.repository_routed && !route.fleet_routed);
        if timeout == 9 {
            assert!(matches!(
                terminal.outcome,
                Outcome::RefusedBeforeStart { .. }
            ));
            assert!(!route.lease_attempted);
        } else {
            assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        }
    }
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["allow_spill"], false);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_network_keeps_the_opt_in_before_capture_or_contact() {
    for enabled in [false, true] {
        let h = Harness::new(Behavior::CompleteRun).await;
        let (scratch, repo) = committed_repo();
        if enabled {
            h.lease_replies.lock().unwrap().extend([
                (503, serde_json::json!({"error":"no_capacity"}), "0"),
                (503, serde_json::json!({"error":"no_capacity"}), "1"),
            ]);
        }
        let mut config = config(&h);
        config.network_routing = enabled;
        // Capture would fail if the disabled network path reached snapshotting.
        config.inject_snapshot_failure = !enabled;
        let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
        let mut net_request = request(&repo);
        net_request.network = Some(Network::Outbound);
        let stream = motor.run(net_request).unwrap();
        let id = stream.job_id;
        let route = motor.hosted_state(id).unwrap();
        assert!(route.fleet_no_spill);
        assert!(route.lease_request.is_none());
        let restored: crate::HostedJobState =
            serde_json::from_value(serde_json::to_value(&route).unwrap()).unwrap();
        assert!(restored.fleet_no_spill);
        let (_, terminal) = records(stream).await;
        if enabled {
            assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
            {
                let bodies = h.lease_bodies.lock().unwrap();
                assert_eq!(bodies.len(), 3);
                assert!(bodies.iter().all(|body| body == &bodies[0]));
                let hashes: Vec<_> = bodies
                    .iter()
                    .map(|body| motor_lease::sha256_hex(body))
                    .collect();
                assert!(hashes.iter().all(|hash| hash == &hashes[0]));
                let body: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
                assert_eq!(body["job_id"], id.to_string());
                assert_eq!(body["net"], true);
                assert_eq!(body["allow_spill"], false);
            }
            let route = motor.hosted_state(id).unwrap();
            assert_eq!(route.placement_attempts, 1);
            assert_eq!(route.lease_request.unwrap().allow_spill, Some(false));
            let non_net = motor.run(request(&repo)).unwrap();
            let non_net_id = non_net.job_id;
            let (_, terminal) = records(non_net).await;
            assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
            assert!(!motor.hosted_state(non_net_id).unwrap().fleet_no_spill);
            let bodies = lease_bodies(&h);
            assert_eq!(bodies.len(), 4);
            assert_eq!(bodies[3]["net"], false);
            assert_eq!(bodies[3]["allow_spill"], true);
            assert_eq!(h.runs.load(Ordering::SeqCst), 2);
        } else {
            assert_eq!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::NetworkUnsupported
                }
            );
            assert!(lease_bodies(&h).is_empty());
            assert!(h.frames.lock().unwrap().is_empty());
            assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        }
        assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn fleet_no_host_fits_corrects_only_before_run_and_never_other_refusals() {
    for (code, status, run_sent) in [
        ("no_host_fits", 422, false),
        ("no_host_fits", 422, true),
        ("account_closed", 403, false),
        ("unauthorized", 403, false),
        ("platform_unsupported", 422, false),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        h.broker.lock().unwrap().knobs.refusal = Some((status, code.into()));
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
        let motor =
            Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
        let stream = motor.run(request(&repo)).unwrap();
        let id = stream.job_id;
        if run_sent {
            // Model a persisted route that may already have run: a broker refusal
            // must not cause a second submission to the direct runner.
            motor
                .storage
                .hosted_update(id, |state| state.run_sent = true)
                .unwrap();
        }
        let (_, terminal) = records(stream).await;
        let corrected = code == "no_host_fits" && !run_sent;
        if corrected {
            assert_eq!(terminal.outcome, Outcome::Exit { code: 17 });
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
        } else {
            assert!(
                matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. }),
                "{code}: {terminal:?}"
            );
            assert!(motor.hosted_state(id).is_some());
        }
        assert_eq!(peer.0.load(Ordering::SeqCst), usize::from(corrected));
        assert_eq!(lease_bodies(&h).len(), 1);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn fleet_unknown_boat_booking_is_finished_without_run_or_reattach() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let broker = crate::hosted::test_http::Http::new(|path, _| {
        if path == "/v1/leases" || path.ends_with("/release") {
            (409, serde_json::json!({"error":"job_admitted"}))
        } else {
            assert!(path.ends_with("/finish"));
            let id = path.split('/').nth(3).unwrap();
            (
                200,
                serde_json::json!({"job_id":id,"settled_vcpu_ms":0,"released_vcpu_ms":60000}),
            )
        }
    })
    .await;
    let (scratch, repo) = committed_repo();
    let mut config = config(&h);
    config.broker_url = Some(broker.url.clone());
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    let state = motor.hosted_state(id).unwrap();
    assert!(state.fleet_routed && state.finish_done && !state.run_sent);
    assert!(state.placement.is_none());
    let requests = broker.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        [
            "/v1/leases",
            &format!("/v1/leases/{id}/release"),
            &format!("/v1/leases/{id}/finish")
        ]
    );
    assert_eq!(
        requests[2].1,
        serde_json::json!({"outcome":"lost","run_ms":0})
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_capacity_waits_do_not_exhaust_three_placements() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.host_refusals.lock().unwrap().extend(["host_full"; 4]);
    h.lease_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"no_capacity"}),
        "0",
    ));
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 6);
    assert!(bodies
        .iter()
        .all(|body| body["job_id"] == id.to_string() && body["allow_spill"] == true));
    assert_eq!(motor.hosted_state(id).unwrap().placement_attempts, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(h.tokens.load(Ordering::SeqCst), 5);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_capacity_budget_ends_with_a_retryable_refusal_not_a_lost_job() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"no_capacity"}),
        "30",
    ));
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let config = config(&h);
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    let mut run_request = request(&repo);
    run_request.queue_wait_limit_s = Some(1);
    let stream = motor.run(run_request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms, Some(30000));
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    assert_eq!(lease_bodies(&h).len(), 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rollback_fragment_routes_bounded_and_unbounded_jobs_direct_without_leases() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let rollback: Config = serde_json::from_str(
        r#"{
        "hosted_runners": false,
        "hosted_repositories": [],
        "spill_to_hosted": false,
        "hosted_fleet_routing": false
    }"#,
    )
    .unwrap();
    let config = Config {
        broker_url: h.config().broker_url,
        broker_token_source: crate::BrokerTokenSource::Fake,
        runner_instance: "test".into(),
        ..rollback
    };
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    for timeout in [Some(600), None] {
        let mut request = request(&repo);
        request.timeout = timeout;
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 17 });
        assert!(motor.hosted_state(id).is_none());
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 2);
    assert!(lease_bodies(&h).is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}
