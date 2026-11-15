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

fn release_count(h: &Harness, id: &str) -> usize {
    h.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|p| **p == format!("/v1/leases/{id}/release"))
        .count()
}

async fn wait_rebook(motor: &Motor, id: Uuid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if motor.hosted_state(id).unwrap().runner_id(id) != id {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn two_hosts_refuse_then_third_booking_runs_with_original_caller_id() {
    let first = Harness::with_host(Behavior::CompleteRun, "first-host").await;
    first.host_refusals.lock().unwrap().push_back("host_full");
    let second = Harness::with_host(Behavior::Refuse("capacity"), "second-host").await;
    first.lease_hosts.lock().unwrap().extend([
        first.broker.clone(),
        second.broker.clone(),
        first.broker.clone(),
    ]);
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        first.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(12);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.job_id, id);
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    motor.wait_hosted_cleanup(id).await;
    assert_eq!(replace_count(&first), 0);
    assert_eq!(first.runs.load(Ordering::SeqCst), 1);
    assert_eq!(second.runs.load(Ordering::SeqCst), 0);
    let bodies: Vec<LeaseRequest> = first
        .lease_bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[0].job_id, id.to_string());
    assert_ne!(bodies[0].job_id, bodies[1].job_id);
    assert_ne!(bodies[1].job_id, bodies[2].job_id);
    let mut shape = bodies[0].clone();
    for body in &bodies[1..] {
        shape.job_id = body.job_id.clone();
        assert_eq!(&shape, body, "only the booking ID may change");
    }
    for body in &bodies[..2] {
        assert_eq!(release_count(&first, &body.job_id), 1);
    }
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.runner_id(id).to_string(), bodies[2].job_id);
    assert!(state.pending_releases.is_empty());
    assert!(
        state.replacement_hosts.is_empty(),
        "retired hosts belong only to their old lease ID"
    );
}

#[tokio::test]
async fn rebooks_keep_original_queue_deadline_and_capacity_reason() {
    let h = Harness::new(Behavior::Refuse("host_full")).await;
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(5);
    let started = tokio::time::Instant::now();
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (_, terminal) = tokio::time::timeout(Duration::from_secs(12), records(stream))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "missing queue terminal: {:?}; {}",
                motor.hosted_state(id),
                h.timeline.snapshot()
            )
        });
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(terminal
        .refusal_detail
        .unwrap()
        .contains("hosted_host_full"));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "re-books must not renew the queue budget"
    );
    tokio::time::timeout(Duration::from_secs(5), motor.wait_hosted_cleanup(id))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "cleanup did not stop: {:?}; {}",
                motor.hosted_state(id),
                h.timeline.snapshot()
            )
        });
    let bodies = h.lease_bodies.lock().unwrap();
    assert!(bodies.len() >= 2, "the deadline must span multiple IDs");
    for bytes in bodies.iter() {
        let body: LeaseRequest = serde_json::from_slice(bytes).unwrap();
        assert_eq!(release_count(&h, &body.job_id), 1);
    }
    assert_eq!(replace_count(&h), 0);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replacement_validation_keeps_reservation_shape_and_excludes_retired_hosts() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let storage = stored(&scratch, id, h.route.clone());
    let (_, previous) = Client::new(h.route.clone())
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    let mut next = previous.clone();
    next.response.placement.host = "second-host".into();
    let retired = ["retired-host".to_owned()].into();
    assert_eq!(validate_replacement(&previous, &next, &retired), Ok(()));
    for field in [
        "lease_id",
        "job_id",
        "max_s",
        "reserved_vcpu_ms",
        "same_host",
        "retired_host",
        "boat",
    ] {
        let mut invalid = next.clone();
        match field {
            "lease_id" => invalid.response.lease_id = Uuid::now_v7().to_string(),
            "job_id" => invalid.response.job_id = Uuid::now_v7().to_string(),
            "max_s" => invalid.response.max_s += 1,
            "reserved_vcpu_ms" => invalid.response.reserved_vcpu_ms += 1,
            "same_host" => invalid.response.placement.host = previous.placement.host.clone(),
            "retired_host" => invalid.response.placement.host = "retired-host".into(),
            "boat" => invalid.response.placement.host = "boat:unexpected".into(),
            _ => unreachable!(),
        }
        assert_eq!(
            validate_replacement(&previous, &invalid, &retired),
            Err(Error::Auth),
            "{field}"
        );
    }
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
async fn capacity_refusal_rebooks_and_logs_both_ids_and_host() {
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let first = Harness::with_host(Behavior::Refuse("capacity"), "first-host").await;
    let second = Harness::with_host(Behavior::CompleteRun, "second-host").await;
    first
        .lease_hosts
        .lock()
        .unwrap()
        .extend([first.broker.clone(), second.broker.clone()]);
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        first.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let stream = motor.run(Harness::request(&repo)).unwrap();
    let id = stream.job_id;
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert_eq!(first.runs.load(Ordering::SeqCst), 0);
    assert_eq!(second.runs.load(Ordering::SeqCst), 1);
    motor.wait_hosted_cleanup(id).await;
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.placement.as_ref().unwrap().host, "second-host");
    let text = logs.text();
    let lines: Vec<_> = text
        .lines()
        .filter(|l| l.contains("hosted capacity refusal; re-booking"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    for value in [
        id.to_string(),
        state.runner_id(id).to_string(),
        "first-host".into(),
    ] {
        assert!(lines[0].contains(&value), "{text}");
    }
    assert_eq!(replace_count(&first), 0);
}

#[tokio::test]
async fn lost_pre_run_upload_capacity_refusal_rebooks_without_sending_run_to_old_host() {
    for reason in ["capacity", "host_full"] {
        let h = Harness::new(Behavior::DropHaveThenRefuse(reason)).await;
        let ready = Harness::with_host(Behavior::CompleteRun, "ready-host").await;
        h.lease_hosts
            .lock()
            .unwrap()
            .extend([h.broker.clone(), ready.broker.clone()]);
        let (scratch, repo) = committed_repo();
        let motor = Motor::with_connector(
            h.config(),
            &scratch.0.join("state"),
            Arc::new(NoSsh(AtomicUsize::new(0))),
        )
        .unwrap();
        let stream = motor.run(Harness::request(&repo)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert_eq!(terminal.job_id, id);
        assert!(h
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Frame::Have { .. })));
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(ready.runs.load(Ordering::SeqCst), 1);
        assert_eq!(replace_count(&h), 0);
        motor.wait_hosted_cleanup(id).await;
        assert_eq!(release_count(&h, &id.to_string()), 1);
        let bodies: Vec<LeaseRequest> = h
            .lease_bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(bodies.len(), 3);
        assert_eq!(
            bodies[0].job_id, bodies[1].job_id,
            "lost upload retries its existing booking"
        );
        assert_ne!(
            bodies[1].job_id, bodies[2].job_id,
            "the capacity refusal requires a fresh booking"
        );
    }
}

#[tokio::test]
async fn lost_run_ack_capacity_refusal_stays_unknown_and_never_replaces() {
    let h = Harness::new(Behavior::DropRunAckRefuseAttach("capacity")).await;
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let stream = motor.run(Harness::request(&repo)).unwrap();
    let id = stream.job_id;
    tokio::time::timeout(Duration::from_secs(10), async {
        while h.tokens.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    h.finished.store(true, Ordering::SeqCst);
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert_eq!(terminal.ran, Some(Ran::Unknown("maybe".into())));
    assert!(motor.hosted_state(id).unwrap().run_sent);
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
    assert_eq!(replace_count(&h), 0);
    assert!(h
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.ends_with("/reattach")));
}

#[tokio::test]
async fn cancel_during_rebook_wait_releases_only_the_refused_booking() {
    let h = Harness::new(Behavior::Refuse("host_full")).await;
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let stream = motor.run(Harness::request(&repo)).unwrap();
    let id = stream.job_id;
    wait_rebook(&motor, id).await;
    let fresh = motor.hosted_state(id).unwrap().runner_id(id);
    stream.cancel().unwrap();
    let (_, terminal) = records(stream).await;
    assert_eq!(terminal.outcome, Outcome::Cancelled);
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.job_id, id);
    motor.wait_hosted_cleanup(id).await;
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
    assert_eq!(release_count(&h, &id.to_string()), 1);
    assert_eq!(release_count(&h, &fresh.to_string()), 0);
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
async fn restart_after_persisted_rebook_releases_old_id_once_without_placing() {
    let h = Harness::new(Behavior::Refuse("host_full")).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    {
        let route = HostedJobState {
            fleet_routed: true,
            ..h.route.clone()
        };
        let storage = stored(&scratch, id, route.clone());
        let client = Client::new(route);
        let request = Harness::request(&scratch.0);
        let run = client.run(id, &request, &storage);
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("placement ended before restart: {:?}", result.err()),
            _ = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if storage.job(id).unwrap().hosted.unwrap().runner_id(id) != id { break; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }) => {}
        }
        let state = storage.job(id).unwrap().hosted.unwrap();
        assert_eq!(state.pending_releases, vec![id]);
        assert!(!state.lease_attempted);
        assert!(!state.run_sent);
    }
    assert_eq!(release_count(&h, &id.to_string()), 0);
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    motor.machine_jobs_health().await;
    motor.wait_hosted_cleanup(id).await;
    let state = motor.hosted_state(id).unwrap();
    let fresh = state.runner_id(id);
    assert!(state.pending_releases.is_empty());
    assert!(state.finish_done);
    assert_eq!(release_count(&h, &id.to_string()), 1);
    assert_eq!(release_count(&h, &fresh.to_string()), 0);
    let terminal = motor.job_status(id).await.unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::LostToRestart
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    motor.machine_jobs_health().await;
    motor.wait_hosted_cleanup(id).await;
    assert_eq!(release_count(&h, &id.to_string()), 1);
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
    assert_eq!(replace_count(&h), 0);
}

#[tokio::test]
async fn pending_release_already_gone_answers_are_final_and_failures_retry() {
    for (status, code) in [
        (404, "job_unknown"),
        (409, "job_released"),
        (409, "job_finished"),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        h.host_refusals.lock().unwrap().push_back("host_full");
        h.release_replies.lock().unwrap().extend([
            (503, serde_json::json!({"error":"broker_unavailable"}), ""),
            (status, serde_json::json!({"error":code}), ""),
        ]);
        let (scratch, repo) = committed_repo();
        let motor = Motor::with_connector(
            h.config(),
            &scratch.0.join("state"),
            Arc::new(NoSsh(AtomicUsize::new(0))),
        )
        .unwrap();
        let stream = motor.run(Harness::request(&repo)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        motor.wait_hosted_cleanup(id).await;
        assert!(
            motor.hosted_state(id).unwrap().pending_releases.is_empty(),
            "{code}"
        );
        assert_eq!(release_count(&h, &id.to_string()), 2, "{code}");
    }
}

#[tokio::test]
async fn rebook_keeps_retired_booking_durable_until_release_recovery() {
    let h = Harness::new(Behavior::Normal).await;
    h.host_refusals.lock().unwrap().push_back("host_full");
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let route = HostedJobState {
        fleet_routed: true,
        ..h.route.clone()
    };
    let storage = stored(&scratch, id, route.clone());
    Client::new(route)
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    let state = storage.job(id).unwrap().hosted.unwrap();
    assert_ne!(state.runner_id(id), id);
    assert_eq!(state.pending_releases, vec![id]);
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_ne!(bodies[0], bodies[1]);
    assert_eq!(replace_count(&h), 0);
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
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies[0], bodies[1], "pinned jobs wait on the same booking");
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

#[tokio::test]
async fn fresh_booking_can_spill_to_boat_and_finish_while_old_release_is_hung() {
    let first = Harness::new(Behavior::Refuse("host_full")).await;
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    let release_gate = Arc::new(tokio::sync::Notify::new());
    let proxy = crate::hosted::test_http::Http::with_concurrent_reply({
        let broker = first.broker.clone();
        let boat_url = boat.config.broker_url.clone().unwrap();
        let first_request = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let release_gate = release_gate.clone();
        move |path, body| {
            let broker = broker.clone();
            let boat_url = boat_url.clone();
            let first_request = first_request.clone();
            let release_gate = release_gate.clone();
            async move {
                if path.ends_with("/release") {
                    release_gate.notified().await;
                }
                if path.ends_with("/release")
                    || path == "/v1/leases" && first_request.swap(false, Ordering::SeqCst)
                {
                    let bytes = serde_json::to_vec(&body).unwrap();
                    let reply = broker.lock().unwrap().handle(
                        motor_lease::fake::BrokerRequest {
                            content_type: Some("application/json"),
                            method: "POST",
                            path: &path,
                            bearer: Some("hosted-test-bearer"),
                            host_headers: None,
                            body: &bytes,
                        },
                        now_s() * 1000,
                    );
                    return (reply.status, serde_json::to_vec(&reply.body).unwrap());
                }
                let reply = reqwest::Client::new()
                    .post(format!("{boat_url}{path}"))
                    .bearer_auth("hosted-test-bearer")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                (
                    reply.status().as_u16(),
                    reply.bytes().await.unwrap().to_vec(),
                )
            }
        }
    })
    .await;
    let (scratch, repo) = committed_repo();
    let config = Config {
        broker_url: Some(proxy.url.clone()),
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
    let (_, terminal) = tokio::time::timeout(Duration::from_secs(12), records(stream))
        .await
        .expect("old release must not delay terminal or active finish");
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert_eq!(terminal.job_id, id);
    let state = motor.hosted_state(id).unwrap();
    assert!(state.placement.as_ref().unwrap().host.starts_with("boat:"));
    assert_eq!(state.pending_releases, vec![id]);
    assert!(
        state.finish_done,
        "the Boat finish must not wait on old release"
    );
    assert_eq!(first.runs.load(Ordering::SeqCst), 0);
    assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
    let requests = proxy.requests.lock().unwrap().clone();
    let bodies: Vec<_> = requests
        .iter()
        .filter(|(p, _)| p == "/v1/leases")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_ne!(bodies[0]["job_id"], bodies[1]["job_id"]);
    assert_eq!(bodies[0]["allow_spill"], true);
    assert_eq!(bodies[0]["allow_spill"], bodies[1]["allow_spill"]);
    assert!(requests
        .iter()
        .any(|(p, _)| p == &format!("/v1/leases/{id}/release")));
    assert!(!requests.iter().any(|(p, _)| p.ends_with("/replace")));
    release_gate.notify_one();
    motor.wait_hosted_cleanup(id).await;
    assert!(motor.hosted_state(id).unwrap().pending_releases.is_empty());
    assert_eq!(
        proxy
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.ends_with("/release"))
            .count(),
        1
    );
}

#[tokio::test]
async fn restart_after_old_release_retires_unsent_fresh_request_without_releasing_it() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let fresh;
    {
        let storage = stored(&scratch, id, h.route.clone());
        let client = Client::new(h.route.clone());
        let (_, lease) = client
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .unwrap();
        let mut body = storage
            .job(id)
            .unwrap()
            .hosted
            .unwrap()
            .lease_request
            .unwrap();
        client
            .rebook(id, &mut body, &lease, &storage, false)
            .unwrap();
        client.release_booking(id).await.unwrap();
        storage
            .hosted_update(id, |s| s.pending_releases.clear())
            .unwrap();
        fresh = Uuid::parse_str(&body.job_id).unwrap();
    }
    let motor = Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    motor.machine_jobs_health().await;
    motor.wait_hosted_cleanup(id).await;
    let terminal = motor.job_status(id).await.unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::LostToRestart
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(release_count(&h, &id.to_string()), 1);
    assert_eq!(release_count(&h, &fresh.to_string()), 0);
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
}

#[test]
fn pending_releases_defaults_are_omitted_and_nonempty_ids_roundtrip() {
    let mut state = HostedJobState::default();
    let value = serde_json::to_value(&state).unwrap();
    assert!(value.get("pending_releases").is_none());
    assert!(serde_json::from_value::<HostedJobState>(value)
        .unwrap()
        .pending_releases
        .is_empty());
    let id = Uuid::now_v7();
    state.pending_releases.push(id);
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value["pending_releases"], serde_json::json!([id]));
    assert_eq!(
        serde_json::from_value::<HostedJobState>(value)
            .unwrap()
            .pending_releases,
        vec![id]
    );
}
