use super::*;
use crate::{
    hosted::boat_tests::{committed_repo, records},
    Motor,
};

fn replace_count(h: &Harness) -> usize {
    h.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.ends_with("/replace"))
        .count()
}

#[tokio::test]
async fn host_full_replaces_onto_second_or_third_host_with_one_job_and_reservation() {
    for third_host in [false, true] {
        let first = Harness::with_host(Behavior::Refuse("host_full"), "first-host").await;
        let second = Harness::with_host(
            if third_host {
                Behavior::Refuse("host_full")
            } else {
                Behavior::CompleteRun
            },
            "second-host",
        )
        .await;
        let third = Harness::with_host(Behavior::CompleteRun, "third-host").await;
        first
            .replacement_hosts
            .lock()
            .unwrap()
            .push_back(second.broker.clone());
        if third_host {
            first
                .replacement_hosts
                .lock()
                .unwrap()
                .push_back(third.broker.clone());
        }
        let (scratch, repo) = committed_repo();
        let config = Config {
            hosted_fleet_routing: true,
            ..first.config()
        };
        let motor = Motor::with_connector(
            config,
            &scratch.0.join("state"),
            Arc::new(NoSsh(AtomicUsize::new(0))),
        )
        .unwrap();
        let stream = motor.run(Harness::request(&repo)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert_eq!(replace_count(&first), if third_host { 2 } else { 1 });
        assert_eq!(first.lease_bodies.lock().unwrap().len(), 1);
        assert_eq!(first.runs.load(Ordering::SeqCst), 0);
        assert_eq!(second.runs.load(Ordering::SeqCst), usize::from(!third_host));
        assert_eq!(third.runs.load(Ordering::SeqCst), usize::from(third_host));
        let state = motor.hosted_state(id).unwrap();
        assert_eq!(
            state.placement.unwrap().host,
            if third_host {
                "third-host"
            } else {
                "second-host"
            }
        );
        assert_eq!(
            state.replacement_hosts.len(),
            if third_host { 2 } else { 1 }
        );
        let body: LeaseRequest =
            serde_json::from_slice(&first.lease_bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body.job_id, id.to_string());
    }
}

#[tokio::test]
async fn replace_refusals_wait_on_the_same_job_and_try_each_host_only_once() {
    for (status, code) in [
        (404, "job_unknown"),
        (409, "job_released"),
        (409, "job_finished"),
        (409, "job_admitted"),
        (503, "no_capacity"),
    ] {
        let h = Harness::new(Behavior::Normal).await;
        h.host_refusals
            .lock()
            .unwrap()
            .extend(["host_full", "host_full"]);
        h.replace_replies.lock().unwrap().push_back((
            status,
            serde_json::json!({"error":code}),
            "5",
        ));
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        let route = HostedJobState {
            fleet_routed: true,
            ..h.route.clone()
        };
        let storage = stored(&scratch, id, route.clone());
        let start = tokio::time::Instant::now();
        let (_, lease) = Client::new(route)
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .unwrap();
        assert_eq!(lease.job_id, id.to_string());
        assert!(start.elapsed() >= Duration::from_millis(HOST_FULL_RETRY_MS * 2));
        assert_eq!(replace_count(&h), 1, "{code}");
        let bodies = h.lease_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3, "{code}");
        for bytes in bodies.iter() {
            let body: LeaseRequest = serde_json::from_slice(bytes).unwrap();
            assert_eq!(body.job_id, id.to_string());
        }
        assert!(storage
            .job(id)
            .unwrap()
            .hosted
            .unwrap()
            .replacement_hosts
            .contains("test-host"));
    }
}

#[tokio::test]
async fn second_host_refusal_attempts_replace_once_then_uses_capacity_wait() {
    let first = Harness::with_host(Behavior::Refuse("host_full"), "first-host").await;
    let second = Harness::with_host(Behavior::Normal, "second-host").await;
    second
        .host_refusals
        .lock()
        .unwrap()
        .extend(["host_full", "host_full"]);
    first
        .replacement_hosts
        .lock()
        .unwrap()
        .push_back(second.broker.clone());
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let route = HostedJobState {
        fleet_routed: true,
        ..first.route.clone()
    };
    let storage = stored(&scratch, id, route.clone());
    let (_, lease) = Client::new(route)
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    assert_eq!(lease.placement.host, "second-host");
    assert_eq!(replace_count(&first), 2);
    assert_eq!(first.lease_bodies.lock().unwrap().len(), 3);
    assert_eq!(second.tokens.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn binding_repository_hint_never_attempts_replace() {
    let h = Harness::new(Behavior::Normal).await;
    h.host_refusals.lock().unwrap().push_back("host_full");
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let route = HostedJobState {
        repository_routed: true,
        repository_placement_hint: Some(Placement {
            host: "test-host".into(),
            vm: Some("test-vm".into()),
        }),
        ..h.route.clone()
    };
    let storage = stored(&scratch, id, route.clone());
    Client::new(route)
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    assert_eq!(replace_count(&h), 0);
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn long_hosted_leases_disable_boat_and_queue_on_no_capacity() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let config = Config {
        hosted_fleet_routing: true,
        ..h.config()
    };
    let motor = Motor::with_connector(
        config,
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    for timeout in [7201, 21600] {
        h.lease_replies.lock().unwrap().push_back((
            503,
            serde_json::json!({"error":"no_capacity"}),
            "1",
        ));
        let stream = motor
            .run(Harness::request(&repo).with_timeout(timeout))
            .unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert!(motor.hosted_state(id).unwrap().fleet_routed);
    }
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 4);
    for bytes in bodies.iter() {
        let body: LeaseRequest = serde_json::from_slice(bytes).unwrap();
        assert_eq!(body.allow_spill, Some(false));
        assert!([7201, 21600].contains(&body.timeout_s));
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 2);
}
