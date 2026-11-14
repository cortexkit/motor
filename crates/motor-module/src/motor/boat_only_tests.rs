use super::repository_routing_tests::{lease_bodies, request};
use super::*;
use crate::hosted::{
    boat_tests::{committed_repo, records},
    tests::harness::{Behavior, Harness, NoSsh},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn boat_only_is_one_job_only_and_rejects_ineligible_jobs_without_submission() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), peer.clone()).unwrap();
    let mut ineligible = Vec::new();
    let mut net = request(&repo);
    net.network = Some(motor_protocol::Network::Outbound);
    ineligible.push((net, "network-free"));
    let mut long = request(&repo);
    long.timeout = Some(crate::hosted::BOAT_MAX_TIMEOUT_S + 1);
    ineligible.push((long, "timeout"));
    let mut untimed = request(&repo);
    untimed.timeout = None;
    ineligible.push((untimed, "timeout"));
    let mut gate = request(&repo);
    gate.queue_wait_limit_s = Some(1);
    ineligible.push((gate, "gate"));
    for (request, reason) in ineligible {
        let error = motor
            .run_direct_boat_only_authorized(request, None, Default::default(), [7; 32])
            .err()
            .unwrap();
        assert!(error.to_string().contains(reason), "{error}");
    }
    assert!(h.lease_bodies.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());

    h.lease_replies.lock().unwrap().push_back((
        403,
        serde_json::json!({"error":"boat_only_not_allowed"}),
        "",
    ));
    let stream = motor
        .run_direct_boat_only_authorized(request(&repo), None, Default::default(), [7; 32])
        .unwrap();
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
        .as_deref()
        .unwrap()
        .contains("boat_only_not_allowed"));
    let state = motor.hosted_state(id).unwrap();
    assert!(state.boat_only && state.hosted_test && state.hosted_no_fallback);
    assert!(!motor.storage.job(id).unwrap().gate_job);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["placement"], "boat");
    assert_eq!(bodies[0]["allow_spill"], true);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);

    let ordinary = motor
        .run_direct_authorized(
            request(&repo),
            None,
            Default::default(),
            false,
            true,
            [8; 32],
        )
        .unwrap();
    let ordinary_id = ordinary.job_id;
    assert_eq!(
        records(ordinary).await.1.outcome,
        Outcome::Exit { code: 23 }
    );
    assert!(!motor.hosted_state(ordinary_id).unwrap().boat_only);
    assert!(motor.storage.job(ordinary_id).unwrap().gate_job);
    assert!(lease_bodies(&h)[1].get("placement").is_none());
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    motor.close().await;

    let repository = Motor::with_connector(
        Config {
            hosted_repositories: vec![repo.to_str().unwrap().into()],
            ..h.config()
        },
        &scratch.0.join("repository-state"),
        peer,
    )
    .unwrap();
    assert!(repository
        .run_direct_boat_only_authorized(request(&repo), None, Default::default(), [9; 32])
        .err()
        .unwrap()
        .to_string()
        .contains("repository-routed"));
    assert_eq!(lease_bodies(&h).len(), 2);
    repository.close().await;
}

#[tokio::test]
async fn broker_boat_only_shape_refusal_names_its_limits_and_never_runs_elsewhere() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), peer.clone()).unwrap();
    for (status, body, detail) in [
        (
            422,
            serde_json::json!({"error":"no_host_fits","max_vcpus":0,"max_mem_mib":0}),
            "broker refused the Boat-only lease; max_vcpus=0, max_mem_mib=0",
        ),
        (
            409,
            serde_json::json!({"error":"job_conflict"}),
            "lease_mismatch",
        ),
        (
            503,
            serde_json::json!({"error":"no_capacity"}),
            "no_capacity",
        ),
    ] {
        h.lease_replies
            .lock()
            .unwrap()
            .push_back((status, body, "0"));
        let mut request = request(&repo);
        // The broker's configured limit may be lower than Motor's spill cap.
        request.timeout = Some(3601);
        let stream = motor
            .run_direct_boat_only_authorized(request, None, Default::default(), [7; 32])
            .unwrap();
        let (_, terminal) = records(stream).await;
        assert!(matches!(
            terminal.outcome,
            Outcome::RefusedBeforeStart { .. }
        ));
        assert!(
            terminal.refusal_detail.as_deref().unwrap().contains(detail),
            "{terminal:?}"
        );
    }
    assert_eq!(lease_bodies(&h).len(), 3);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(h.frames.lock().unwrap().is_empty());
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[test]
#[ignore = "requires a probe linked against the previous release via MOTOR_BOAT_ROLLBACK_PROBE"]
fn rollback_boat_only_journal_and_checkpoint_open_and_refuse_replay() {
    let probe =
        std::env::var_os("MOTOR_BOAT_ROLLBACK_PROBE").expect("previous-release test binary");
    let (scratch, repo) = committed_repo();
    for checkpoint in [false, true] {
        let source = scratch.0.join(format!("source-{checkpoint}"));
        let storage = crate::storage::Storage::open(&source).unwrap();
        let id = Uuid::now_v7();
        storage.create(id, 300, None, None, false).unwrap();
        let mut route = crate::HostedJobState::new(&Config {
            broker_url: Some("http://127.0.0.1:1".into()),
            broker_token_source: crate::BrokerTokenSource::Fake,
            ..Config::default()
        });
        route.boat_only = true;
        route.hosted_test = true;
        route.hosted_no_fallback = true;
        let vector: serde_json::Value = serde_json::from_str(include_str!(
            "../../../motor-lease/contracts/cloud/boat-only-lease.json"
        ))
        .unwrap();
        let mut lease: motor_lease::LeaseRequest =
            serde_json::from_value(vector["granted"]["request"]["body"].clone()).unwrap();
        lease.job_id = id.to_string();
        route.lease_request = Some(lease);
        storage.hosted_route(id, route).unwrap();
        if checkpoint {
            storage.shutdown().unwrap();
        }
        let copy = scratch.0.join(format!("copy-{checkpoint}"));
        fn copy_store(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    copy_store(&entry.path(), &to.join(entry.file_name()));
                } else {
                    std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
                }
            }
        }
        copy_store(&source, &copy);
        if !checkpoint {
            assert!(std::fs::read_dir(&copy).unwrap().any(|entry| {
                let entry = entry.unwrap();
                entry.file_name().to_string_lossy().starts_with("journal-")
                    && std::fs::read_to_string(entry.path())
                        .unwrap()
                        .contains("\"boat_only\":true")
            }));
        } else {
            assert!(std::fs::read_to_string(copy.join("state.json"))
                .unwrap()
                .contains("\"boat_only\":true"));
        }
        let state = serde_json::to_value(storage.job(id).unwrap()).unwrap();
        assert_eq!(state["hosted"]["boat_only"], true);
        assert!(state["hosted"]["lease_request"].get("placement").is_none());
        let output = std::process::Command::new(&probe)
            .args([
                "--exact",
                "motor::boat_only_rollback_probe::replay",
                "--nocapture",
            ])
            .env("MOTOR_BOAT_ROLLBACK_ROOT", &copy)
            .env("MOTOR_BOAT_ROLLBACK_REPO", &repo)
            .env("MOTOR_BOAT_ROLLBACK_ID", id.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "checkpoint={checkpoint}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("1 passed"), "{stdout}");
        println!("previous release opened checkpoint={checkpoint} with boat_only=true; replay refused without Run");
        storage.shutdown().unwrap();
    }
}
