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

#[test]
fn queue_expiry_uses_last_definite_answer_and_explains_mixed_attempts() {
    for (answers, expected, mixed) in [
        (
            vec![Error::HostFull, Error::ReplacePending(15_000)],
            Error::NoCapacity(15_000),
            false,
        ),
        (
            vec![
                Error::Connect,
                Error::NoCapacity(1000),
                Error::ReplacePending(15_000),
            ],
            Error::NoCapacity(15_000),
            true,
        ),
        (
            vec![Error::NoCapacity(1000), Error::Connect],
            Error::Connect,
            true,
        ),
        (
            vec![Error::NoCapacity(1000), Error::Broker],
            Error::Broker,
            true,
        ),
        (
            vec![Error::ReplacePending(15_000)],
            Error::ReplacePending(15_000),
            false,
        ),
    ] {
        let mut wait = WaitAnswers::default();
        for answer in answers {
            wait.observe(answer);
        }
        assert_eq!(wait.expire(Error::ReplacePending(15_000)), expected);
        let detail = wait.detail(expected, expected.detail());
        assert!(
            detail.contains(&format!("final_cause={}", expected.code())),
            "{detail}"
        );
        assert_eq!(detail.contains("mixed_answers=true"), mixed, "{detail}");
        if mixed {
            assert!(detail.contains("using last definite answer"), "{detail}");
        }
    }
}

#[tokio::test]
async fn capacity_refusal_replaces_without_returning_to_the_refusing_host() {
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let first = Harness::with_host(Behavior::Refuse("capacity"), "first-host").await;
    let second = Harness::with_host(Behavior::CompleteRun, "second-host").await;
    first
        .replacement_hosts
        .lock()
        .unwrap()
        .push_back(second.broker.clone());
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        first.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(3);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert_eq!(replace_count(&first), 1);
    assert_eq!(first.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(first.runs.load(Ordering::SeqCst), 0);
    assert_eq!(second.runs.load(Ordering::SeqCst), 1);
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.placement.unwrap().host, "second-host");
    assert!(state.replacement_hosts.contains("first-host"));
    assert_eq!(first.lease_bodies.lock().unwrap().len(), 1);
    let logs = logs.text();
    let replacements: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("hosted capacity refusal; requesting replacement"))
        .collect();
    assert_eq!(replacements.len(), 1, "{logs}");
    assert!(replacements[0].contains(&id.to_string()), "{logs}");
    assert!(replacements[0].contains("first-host"), "{logs}");
}

#[tokio::test]
async fn capacity_replace_pending_queue_expiry_is_runner_full_and_never_ran() {
    let h = Harness::new(Behavior::Refuse("host_full")).await;
    h.replace_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"replace_pending"}),
        "15",
    ));
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(1);
    let (_, terminal) = records(motor.run(request).unwrap()).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(motor_protocol::Ran::None));
    assert_eq!(terminal.retry_after_ms, Some(15_000));
    let detail = terminal.refusal_detail.unwrap();
    assert!(detail.starts_with("hosted_capacity"), "{detail}");
    assert!(detail.contains("final_cause=hosted_capacity"), "{detail}");
    assert!(
        detail.contains("last_response=hosted_replace_pending"),
        "{detail}"
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn real_connect_failure_at_queue_expiry_remains_unreachable() {
    let h = Harness::new(Behavior::ConnectFailure).await;
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(1);
    let (_, terminal) = records(motor.run(request).unwrap()).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable
        }
    );
    assert_eq!(terminal.ran, Some(motor_protocol::Ran::None));
    let detail = terminal.refusal_detail.unwrap();
    assert!(detail.starts_with("hosted_connect_failed"), "{detail}");
    assert_eq!(replace_count(&h), 0);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replace_pending_retries_same_route_after_retry_after_without_blacklisting_host() {
    let first = Harness::with_host(Behavior::Refuse("host_full"), "first-host").await;
    let second = Harness::with_host(Behavior::CompleteRun, "second-host").await;
    first.replace_replies.lock().unwrap().extend([
        (503, serde_json::json!({"error":"replace_pending"}), "1"),
        (503, serde_json::json!({"error":"replace_pending"}), "1"),
    ]);
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
    let client = Client::new(route);
    let request = Harness::request(&scratch.0);
    let start = tokio::time::Instant::now();
    let run = client.run(id, &request, &storage);
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => panic!("run finished before the fence wait: {:?}", result.err()),
        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
    }
    assert_eq!(replace_count(&first), 1);
    assert!(storage
        .job(id)
        .unwrap()
        .hosted
        .unwrap()
        .replacement_hosts
        .is_empty());
    let (_, lease) = run.await.unwrap();
    assert_eq!(lease.placement.host, "second-host");
    assert_eq!(replace_count(&first), 3);
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert_eq!(first.lease_bodies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pending_replace_deadline_keeps_host_eligible_and_carries_retry_after() {
    let h = Harness::with_host(Behavior::Refuse("host_full"), "first-host").await;
    h.replace_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"replace_pending"}),
        "15",
    ));
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let route = HostedJobState {
        fleet_routed: true,
        spill_wait_ms: Some(29_000),
        ..h.route.clone()
    };
    let storage = stored(&scratch, id, route.clone());
    let error = Client::new(route)
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .err()
        .unwrap();
    assert_eq!(error, Error::NoCapacity(15_000));
    assert_eq!(error.retry_after_ms(), Some(15_000));
    assert_eq!(replace_count(&h), 1);
    assert!(storage
        .job(id)
        .unwrap()
        .hosted
        .unwrap()
        .replacement_hosts
        .is_empty());
}

#[tokio::test]
async fn lease_rerequest_pending_retries_identical_bytes_and_preserves_stored_body_on_restart() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let request = Harness::request(&scratch.0);
    let route = HostedJobState {
        fleet_routed: true,
        ..h.route.clone()
    };
    let mut storage = stored(&scratch, id, route);
    let mut stored_body = None;
    for _ in 0..2 {
        drop(storage);
        storage = Arc::new(Storage::open(&scratch.0.join("state")).unwrap());
        h.lease_replies.lock().unwrap().push_back((
            503,
            serde_json::json!({"error":"replace_pending"}),
            "1",
        ));
        let route = storage.job(id).unwrap().hosted.unwrap();
        let start = tokio::time::Instant::now();
        drop(
            Client::new(route)
                .run(id, &request, &storage)
                .await
                .unwrap(),
        );
        assert!(start.elapsed() >= Duration::from_secs(1));
        let body = storage
            .job(id)
            .unwrap()
            .hosted
            .unwrap()
            .lease_request
            .unwrap();
        if let Some(original) = &stored_body {
            assert_eq!(&body, original);
        } else {
            stored_body = Some(body);
        }
    }
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 4);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[2], bodies[3]);
    assert_ne!(bodies[0], bodies[2], "a restart rotates the requester key");
    let original = canonical_lease_bytes(stored_body.as_ref().unwrap());
    for body in bodies.iter() {
        let sent: LeaseRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(canonical_lease_bytes(&sent), original);
    }
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
        let config = Config { ..first.config() };
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
    let config = Config { ..h.config() };
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
