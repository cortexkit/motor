use super::repository_routing_tests::{control_placement, lease_bodies, request, DirectPeer};
use super::*;
use crate::hosted::{
    boat_tests::{committed_repo, records},
    tests::harness::{Behavior, Harness, NoSsh},
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn config(h: &Harness) -> Config {
    Config {
        network_routing: true,
        ..h.config()
    }
}

#[tokio::test]
async fn an_early_refusal_is_local_before_its_first_stream_record() {
    let scratch = cortexkit_test_support::ScratchDir::new("local-refusal-followups");
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let mut motor = Motor::open(Config::default(), &scratch).unwrap();
    motor.set_connector(peer.clone());
    let stream = motor
        .refuse_before_start(
            RefusalReason::RequestRejected,
            Some("invalid_request".into()),
        )
        .unwrap();
    let id = stream.job_id;
    let status = motor.job_status(id).await.unwrap();
    assert_eq!(
        status.terminal.as_ref().unwrap().refusal_detail.as_deref(),
        Some("invalid_request")
    );
    assert_eq!(
        records(motor.attach(AttachRequest::new(id, 0)).unwrap())
            .await
            .1,
        status.terminal.unwrap()
    );
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    assert_eq!(
        records(stream).await.1.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn fleet_linux_default_platform_jobs_allow_hosted_spill() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let mut config = config(&h);
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
        assert!(state.placement_wait_ms.is_some());
        let subc_client_rs::HandlerOutcome::Response(bytes) =
            crate::module::status_reply(&motor, &serde_json::json!({"job_id": id}))
                .await
                .unwrap()
        else {
            panic!("expected status response");
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["placement"], "first_party/test-host");
        assert_eq!(value["placement_wait_ms"].as_u64(), state.placement_wait_ms);
        assert_eq!(value["runner_queue_wait_ms"], terminal.queue_wait_ms);
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

// These missing checkout paths check routing before snapshotting; executing /home checkouts needs a tenant VM test.
#[tokio::test]
async fn home_and_users_requests_select_hosted_routes_without_direct_contact() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let scratch = crate::hosted::tests::harness::Scratch::new();
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    for prefix in ["/Users", "/home"] {
        for mixed in [false, true] {
            for entry in ["run", "agent", "gate", "hosted"] {
                let root = format!("{prefix}/missing-{}/repo", Uuid::now_v7());
                let mut request = RunRequest::new(&root, &root, &root, "true")
                    .with_timeout(60)
                    .with_siblings(vec![format!("{prefix}/u/lib")]);
                if mixed {
                    request.siblings = vec!["/Users/u/lib".into(), "/home/u/lib".into()];
                }
                crate::hosted::validate(&request).unwrap();
                let stream = match entry {
                    "run" => motor.run(request),
                    "agent" => motor.run_agent(request, None, Default::default()),
                    "gate" => motor.run_direct_authorized(
                        request,
                        None,
                        Default::default(),
                        true,
                        false,
                        [7; 32],
                    ),
                    _ => motor.run_direct_hosted(request, None, Default::default()),
                }
                .unwrap();
                let id = stream.job_id;
                let (_, terminal) = records(stream).await;
                assert!(matches!(
                    terminal.outcome,
                    Outcome::RefusedBeforeStart { .. }
                ));
                let route = motor.hosted_state(id).unwrap();
                assert!(route.hosted_no_fallback);
                assert_eq!(route.hosted_test, entry == "hosted");
                assert_eq!(route.fleet_routed, entry != "hosted");
                assert_eq!(route.fleet_no_spill, entry == "gate");
                assert!(!route.repository_routed && !route.lease_attempted);
                assert!(motor.storage.job(id).unwrap().runner_instance.is_none());
            }
        }
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
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
async fn fleet_non_linux_platforms_never_request_a_lease() {
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
    assert_eq!(peer.0.load(Ordering::SeqCst), 2);
    assert!(lease_bodies(&h).is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fleet_invalid_linux_requests_are_refused_without_direct_fallback() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    let mut cases = Vec::new();
    for timeout in [None, Some(0), Some(9), Some(21601)] {
        let mut request = request(&repo);
        request.timeout = timeout;
        cases.push((request, "timeout_s", "10..=21600"));
    }
    for vcpus in [0, 65] {
        let mut request = request(&repo);
        request.weight_hint = Some(vcpus);
        cases.push((request, "vcpus", "1..=64"));
    }
    for path in ["/tmp/werni", "/home2/werni"] {
        for field in ["workspace_key", "repository_root", "cwd", "sibling"] {
            let mut request = request(&repo);
            match field {
                "workspace_key" => request.workspace_key = path.into(),
                "repository_root" => request.repository_root = path.into(),
                "cwd" => request.cwd = path.into(),
                _ => request.siblings.push(path.into()),
            }
            cases.push((request, field, path));
        }
    }
    for (request, field, remedy) in cases {
        for (platform, gate) in [(None, false), (Some(Platform::Linux), true)] {
            let mut request = request.clone();
            request.platform = platform;
            let (_, terminal) = records(
                motor
                    .run_classified(
                        request,
                        (None, None),
                        gate,
                        false,
                        false,
                        Default::default(),
                    )
                    .unwrap(),
            )
            .await;
            assert_eq!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::RequestRejected
                },
                "{field}"
            );
            assert_eq!(terminal.ran, Some(Ran::None));
            let detail = terminal.refusal_detail.unwrap();
            assert!(detail.contains(field), "{detail}");
            assert!(detail.contains(remedy), "{detail}");
            if remedy.starts_with('/') {
                assert!(detail.contains("under /Users or /home"), "{detail}");
            }
        }
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
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
        assert!(route.fleet_routed && !route.lease_attempted);
    }
    assert!(lease_bodies(&h).is_empty());
    let stream = motor.run(request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let route = motor.hosted_state(id).unwrap();
    assert!(route.fleet_routed && !route.repository_routed && route.run_sent);
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
async fn linux_exec_refusals_and_followups_never_reach_a_direct_runner() {
    for (code, status, reason, detail_code) in [
        (
            "no_host_fits",
            422,
            RefusalReason::RequestRejected,
            "hosted_no_host_fits",
        ),
        (
            "account_closed",
            403,
            RefusalReason::RequestRejected,
            "hosted_account_closed",
        ),
        (
            "unauthorized",
            403,
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_auth",
        ),
        (
            "platform_unsupported",
            422,
            RefusalReason::PlatformUnsupported,
            "hosted_platform_unsupported",
        ),
    ] {
        for gate in [false, true] {
            let h = Harness::new(Behavior::CompleteRun).await;
            h.broker.lock().unwrap().knobs.refusal = Some((status, code.into()));
            let (scratch, repo) = committed_repo();
            let peer = Arc::new(DirectPeer(AtomicUsize::new(0)));
            let root = scratch.0.join("state");
            let config = config(&h);
            let mut motor = Motor::open(config.clone(), &root).unwrap();
            motor.set_connector(peer.clone());
            let mut request = request(&repo);
            request.platform = Some(Platform::Linux);
            let stream = if gate {
                motor.run_direct_authorized(
                    request,
                    None,
                    Default::default(),
                    false,
                    false,
                    [7; 32],
                )
            } else {
                motor.run(request)
            }
            .unwrap();
            let id = stream.job_id;
            let (_, terminal) = records(stream).await;
            assert_eq!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: reason.clone()
                },
                "{code}: {terminal:?}"
            );
            assert!(
                terminal
                    .refusal_detail
                    .as_deref()
                    .unwrap()
                    .contains(detail_code),
                "{code}: {terminal:?}"
            );
            assert!(motor.hosted_state(id).is_some());
            assert!(motor.storage.job(id).unwrap().runner_instance.is_none());
            assert!(motor
                .storage
                .job(id)
                .unwrap()
                .routed_direct_reason
                .is_none());
            motor.close().await;

            let mut motor = Motor::open(config, &root).unwrap();
            motor.set_connector(peer.clone());
            assert_eq!(
                records(motor.attach(AttachRequest::new(id, 0)).unwrap())
                    .await
                    .1,
                terminal
            );
            assert_eq!(motor.job_status(id).await.unwrap().terminal, Some(terminal));
            motor.cancel(CancelRequest::new(id)).await.unwrap();
            assert_eq!(motor.status().await.rustc_version, "hosted");
            let missing = Uuid::now_v7();
            assert!(motor.attach(AttachRequest::new(missing, 0)).is_err());
            assert!(motor.job_status(missing).await.is_err());
            assert!(motor.cancel(CancelRequest::new(missing)).await.is_err());
            assert_eq!(peer.0.load(Ordering::SeqCst), 0);
            assert_eq!(lease_bodies(&h).len(), 1);
            assert_eq!(h.runs.load(Ordering::SeqCst), 0);
            motor.close().await;
        }
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
