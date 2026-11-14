use super::*;
use crate::hosted::{
    boat_tests::committed_repo,
    tests::harness::{Behavior, Harness, NoSsh},
    Client,
};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn terminal(mut stream: JobStream) -> TerminalRecord {
    let record = tokio::time::timeout(Duration::from_secs(30), stream.records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match record {
        StreamRecord::Terminal(terminal) => terminal,
        other => panic!("unexpected {other:?}"),
    }
}

async fn released_placement_wait_survives_restart(settle_at_shutdown: bool) {
    let h = Harness::new(Behavior::Refuse("host_full")).await;
    h.replace_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error": "no_capacity"}),
        "15",
    ));
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(300);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    tokio::time::timeout(Duration::from_secs(30), async {
        while h.replace_answers.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("submission must be waiting for replacement capacity");
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    if settle_at_shutdown {
        motor.shutdown_hosted().await;
        assert!(motor.hosted_state(id).unwrap().finish_done);
    }
    drop(stream);
    motor.close().await;
    if !settle_at_shutdown {
        assert!(h
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.ends_with("/release")));
    }

    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    motor.resume_finishes().await;
    motor.wait_hosted_cleanup(id).await;
    let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(
        result.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::LostToRestart
        }
    );
    assert_eq!(result.ran, Some(Ran::None));
    assert_eq!(
        motor.job_status(id).await.unwrap().terminal,
        Some(result.clone())
    );
    motor.close().await;

    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    assert_eq!(
        motor.job_status(id).await.unwrap().terminal,
        Some(result.clone())
    );
    assert_eq!(
        terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await,
        result
    );
    let requests = h.requests.lock().unwrap().clone();
    assert_eq!(
        requests.iter().filter(|p| p.ends_with("/release")).count(),
        1
    );
    assert!(requests.iter().all(|p| !p.ends_with("/reattach")));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn placement_wait_shutdown_release_restart_attach_is_never_started() {
    released_placement_wait_survives_restart(true).await;
}

#[tokio::test]
async fn placement_wait_restart_recovery_release_attach_is_never_started() {
    released_placement_wait_survives_restart(false).await;
}

#[tokio::test]
async fn restart_repairs_synthetic_no_run_outcomes_but_preserves_runner_full_refusal() {
    let h = Harness::new(Behavior::Normal).await;
    let refused = Outcome::RefusedBeforeStart {
        reason: RefusalReason::RunnerFull,
    };
    let never_started = Outcome::RefusedBeforeStart {
        reason: RefusalReason::LostToRestart,
    };
    for (stored, expected) in [
        (Outcome::OutcomeUnknown, never_started.clone()),
        (Outcome::HistoryExpired, never_started),
        (refused.clone(), refused),
    ] {
        let scratch = cortexkit_test_support::ScratchDir::new("hosted-stored-synthetic-terminal");
        let storage = Storage::open(&scratch).unwrap();
        let id = Uuid::now_v7();
        storage.create(id, 60, None, None, false).unwrap();
        storage.hosted_route(id, h.route.clone()).unwrap();
        let is_refusal = matches!(stored, Outcome::RefusedBeforeStart { .. });
        let original = TerminalRecord::new(id, stored, 0, 0, 0).with_ran(if is_refusal {
            Ran::None
        } else {
            Ran::Unknown("maybe".into())
        });
        storage
            .hosted_update(id, |s| {
                s.finish_done = true;
                s.terminal = Some(original.clone());
            })
            .unwrap();
        if is_refusal {
            storage.local_terminal(original.clone()).unwrap();
        }
        drop(storage);

        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(h.config(), &scratch, ssh.clone()).unwrap();
        let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
        assert_eq!(result.outcome, expected);
        if is_refusal {
            assert_eq!(result, original);
        }
        if matches!(
            result.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::LostToRestart
            }
        ) {
            assert_eq!(result.ran, Some(Ran::None));
        }
        assert_eq!(motor.job_status(id).await.unwrap().terminal, Some(result));
        assert!(h.requests.lock().unwrap().is_empty());
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
        motor.close().await;
    }
}

async fn observe_release_before_its_reply(attach_first: bool, unknown: bool) {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-release-in-flight");
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch, ssh.clone()).unwrap();
    let id = Uuid::now_v7();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let http = crate::hosted::test_http::Http::with_concurrent_reply({
        let entered = entered.clone();
        let release = release.clone();
        move |path, _| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                if path.ends_with("/release") {
                    entered.notify_one();
                    release.notified().await;
                    if unknown {
                        return (
                            404,
                            serde_json::to_vec(&serde_json::json!({"error": "job_unknown"}))
                                .unwrap(),
                        );
                    }
                    (
                        202,
                        serde_json::to_vec(&serde_json::json!({"job_id": id, "release_at_ms": 0}))
                            .unwrap(),
                    )
                } else {
                    assert_eq!(path, format!("/v1/leases/{id}/reattach"));
                    (
                        if unknown { 404 } else { 409 },
                        serde_json::to_vec(&serde_json::json!({
                            "error": if unknown { "job_unknown" } else { "job_released" }
                        }))
                        .unwrap(),
                    )
                }
            }
        }
    })
    .await;
    motor.storage.create(id, 60, None, None, false).unwrap();
    motor.storage.hosted_route(id, h.route.clone()).unwrap();
    motor
        .storage
        .hosted_update(id, |state| {
            state.broker_url = http.url.clone();
            state.lease_attempted = true;
            state.billing = crate::hosted::BillingStatus::Pending;
        })
        .unwrap();
    motor.recover_hosted(id);
    tokio::time::timeout(Duration::from_secs(30), entered.notified())
        .await
        .unwrap();
    let job = motor.storage.job(id).unwrap();
    assert!(job.local_terminal.is_none());
    assert!(!job.hosted.unwrap().finish_done);

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        if attach_first {
            terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await
        } else {
            motor.job_status(id).await.unwrap().terminal.unwrap()
        }
    })
    .await
    .expect("never-started result must not wait for the release reply");
    assert_eq!(
        result.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::LostToRestart
        }
    );
    assert_eq!(result.ran, Some(Ran::None));
    assert_eq!(motor.job_status(id).await.unwrap().terminal, Some(result));
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        crate::hosted::BillingStatus::Unavailable
    );
    release.notify_one();
    motor.wait_hosted_cleanup(id).await;
    assert!(motor.storage.job(id).unwrap().hosted.unwrap().finish_done);
    assert_eq!(http.requests.lock().unwrap().len(), 2);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn attach_during_release_before_its_reply_is_never_started() {
    observe_release_before_its_reply(true, false).await;
}

#[tokio::test]
async fn status_during_release_before_its_reply_is_never_started() {
    observe_release_before_its_reply(false, false).await;
}

#[tokio::test]
async fn unknown_attach_during_release_before_its_reply_is_never_started() {
    observe_release_before_its_reply(true, true).await;
}

#[tokio::test]
async fn unknown_status_during_release_before_its_reply_is_never_started() {
    observe_release_before_its_reply(false, true).await;
}

async fn broker_ended_reply_during_live_submission_is_not_final(unknown: bool, status_probe: bool) {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let placing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let http = crate::hosted::test_http::Http::with_concurrent_reply({
        let entered = entered.clone();
        let resume = resume.clone();
        let placing = placing.clone();
        let broker = h.route.broker_url.clone();
        move |path, body| {
            let entered = entered.clone();
            let resume = resume.clone();
            let placing = placing.clone();
            let broker = broker.clone();
            async move {
                if path == "/v1/leases" {
                    entered.notify_one();
                    resume.notified().await;
                } else if path.ends_with("/reattach") && placing.load(Ordering::SeqCst) {
                    return (
                        if unknown { 404 } else { 409 },
                        serde_json::to_vec(&serde_json::json!({
                            "error": if unknown { "job_unknown" } else { "job_released" }
                        }))
                        .unwrap(),
                    );
                }
                let response = reqwest::Client::new()
                    .post(format!("{broker}{path}"))
                    .bearer_auth("hosted-test-bearer")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                (
                    response.status().as_u16(),
                    response.bytes().await.unwrap().to_vec(),
                )
            }
        }
    })
    .await;
    let mut config = h.config();
    config.broker_url = Some(http.url.clone());
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let mut stream = motor.run(Harness::request(&repo)).unwrap();
    let id = stream.job_id;
    tokio::time::timeout(Duration::from_secs(30), entered.notified())
        .await
        .unwrap();
    assert!(motor.submissions.lock().unwrap().contains_key(&id));
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    let mut attached = motor.attach(AttachRequest::new(id, 0)).unwrap();
    if status_probe {
        // Remove only the result-owner reservation so status probes the broker.
        // The live submission stays registered and run_sent is unchanged.
        let owner = motor.hosted_results.lock().unwrap().remove(&id).unwrap();
        let status = motor.job_status(id).await;
        motor.hosted_results.lock().unwrap().insert(id, owner);
        let error = status.unwrap_err();
        assert!(error.to_string().contains("Provisioning(1000)"), "{error}");
    } else {
        assert_eq!(
            motor.open_job_channel(id).await.err(),
            Some(if unknown {
                crate::hosted::Error::ReattachUnknown
            } else {
                crate::hosted::Error::Provisioning(1000)
            })
        );
    }
    let job = motor.storage.job(id).unwrap();
    assert!(job.local_terminal.is_none());
    let state = job.hosted.unwrap();
    assert!(state.terminal.is_none());
    assert!(!state.finish_done);
    assert_eq!(
        http.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.ends_with("/reattach"))
            .count(),
        1
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);

    placing.store(false, Ordering::SeqCst);
    resume.notify_one();
    for stream in [&mut stream, &mut attached] {
        let terminal = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let StreamRecord::Terminal(terminal) =
                    stream.records.recv().await.unwrap().unwrap()
                {
                    break terminal;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert_ne!(terminal.ran, Some(Ran::None));
    }
    let job = motor.storage.job(id).unwrap();
    assert!(job.local_terminal.is_none());
    let state = job.hosted.unwrap();
    assert!(state.run_sent);
    assert_eq!(state.terminal.unwrap().outcome, Outcome::Exit { code: 23 });
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    drop(stream);
    drop(attached);
    motor.close().await;
}

#[tokio::test]
async fn unknown_reattach_during_live_submission_does_not_finalize_before_run() {
    broker_ended_reply_during_live_submission_is_not_final(true, false).await;
}

#[tokio::test]
async fn released_reattach_during_live_submission_does_not_finalize_before_run() {
    broker_ended_reply_during_live_submission_is_not_final(false, false).await;
}

#[tokio::test]
async fn released_status_during_live_submission_does_not_finalize_before_run() {
    broker_ended_reply_during_live_submission_is_not_final(false, true).await;
}

async fn unknown_cleanup_is_never_started(stored: Option<Outcome>, boat: bool) {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-unknown-cleanup");
    let id = Uuid::now_v7();
    let http = crate::hosted::test_http::Http::new(move |path, body| {
        if boat {
            assert_eq!(path, format!("/v1/leases/{id}/finish"));
            assert_eq!(body, &serde_json::json!({"outcome": "lost", "run_ms": 0}));
        } else {
            assert_eq!(path, format!("/v1/leases/{id}/release"));
            assert_eq!(body, &serde_json::json!({}));
        }
        (404, serde_json::json!({"error": "job_unknown"}))
    })
    .await;
    let storage = Storage::open(&scratch).unwrap();
    storage.create(id, 60, None, None, false).unwrap();
    storage.hosted_route(id, h.route.clone()).unwrap();
    storage
        .hosted_update(id, |state| {
            state.broker_url = http.url.clone();
            state.lease_attempted = true;
            state.billing = crate::hosted::BillingStatus::Pending;
            if boat {
                state.placement = Some(motor_lease::Placement {
                    host: "boat:unused".into(),
                    vm: None,
                });
                state.pending_finish = Some(crate::hosted::placement::Finish {
                    outcome: "lost".into(),
                    run_ms: 0,
                });
            }
            state.terminal = stored.clone().map(|outcome| {
                TerminalRecord::new(id, outcome, 0, 0, 0).with_ran(Ran::Unknown("maybe".into()))
            });
        })
        .unwrap();
    drop(storage);

    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch, ssh.clone()).unwrap();
    assert!(!motor.hosted_state(id).unwrap().finish_done);
    assert_eq!(motor.storage.unsettled_hosted(), vec![id]);
    if stored.is_some() {
        let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
        assert_eq!(result.ran, Some(Ran::None));
        assert!(!motor.hosted_state(id).unwrap().finish_done);
        assert!(http.requests.lock().unwrap().is_empty());
    }
    motor.resume_finishes().await;
    motor.wait_hosted_cleanup(id).await;
    let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(
        result.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::LostToRestart
        }
    );
    assert_eq!(result.ran, Some(Ran::None));
    let state = motor.hosted_state(id).unwrap();
    assert!(state.finish_done);
    assert_eq!(state.billing, crate::hosted::BillingStatus::Unavailable);
    assert!(state.pending_finish.is_none());
    assert_eq!(state.terminal, Some(result.clone()));
    assert_eq!(
        motor.job_status(id).await.unwrap().terminal,
        Some(result.clone())
    );
    motor.close().await;

    let motor = Motor::with_connector(h.config(), &scratch, ssh.clone()).unwrap();
    motor.resume_finishes().await;
    assert!(motor.storage.unsettled_hosted().is_empty());
    assert_eq!(
        terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await,
        result
    );
    assert_eq!(http.requests.lock().unwrap().len(), 1);
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn unknown_release_or_finish_cleanup_then_attach_is_never_started() {
    for boat in [false, true] {
        unknown_cleanup_is_never_started(None, boat).await;
    }
}

#[tokio::test]
async fn restart_repairs_unsettled_synthetic_no_run_outcomes_then_settles_unknown_booking() {
    for outcome in [Outcome::OutcomeUnknown, Outcome::HistoryExpired] {
        for boat in [false, true] {
            unknown_cleanup_is_never_started(Some(outcome.clone()), boat).await;
        }
    }
}

#[tokio::test]
async fn unknown_booking_with_run_sent_remains_may_have_run() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-unknown-run-sent");
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch, ssh.clone()).unwrap();
    let id = Uuid::now_v7();
    let http = crate::hosted::test_http::Http::new(move |path, _| {
        assert_eq!(path, format!("/v1/leases/{id}/reattach"));
        (404, serde_json::json!({"error": "job_unknown"}))
    })
    .await;
    motor.storage.create(id, 60, None, None, false).unwrap();
    motor.storage.hosted_route(id, h.route.clone()).unwrap();
    motor
        .storage
        .hosted_update(id, |state| {
            state.broker_url = http.url.clone();
            state.run_sent = true;
            state.retry_iat = Some(0);
            state.max_runtime_s = Some(60);
        })
        .unwrap();
    assert_eq!(
        motor.open_job_channel_at(id, u64::MAX).await.err(),
        Some(crate::hosted::Error::ReattachUnknown)
    );
    let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(result.outcome, Outcome::OutcomeUnknown);
    assert_eq!(result.ran, Some(Ran::Unknown("maybe".into())));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn released_booking_with_run_sent_remains_may_have_run() {
    let h = Harness::new(Behavior::CompleteRun).await;
    h.run_gate.send_replace(false);
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    let stream = motor.run(Harness::request(&repo)).unwrap();
    let id = stream.job_id;
    tokio::time::timeout(Duration::from_secs(30), async {
        while h.runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("host must receive Run");
    assert!(motor.hosted_state(id).unwrap().run_sent);
    drop(stream);
    motor.close().await;
    h.requests.lock().unwrap().clear();
    Client::new(h.route.clone()).release(id).await.unwrap();

    let motor = Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(result.outcome, Outcome::OutcomeUnknown);
    assert_eq!(result.ran, Some(Ran::Unknown("maybe".into())));
    // The existing unknown-outcome path also probes cancellation on its own attach.
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.ends_with("/reattach"))
            .count(),
        2
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    motor.close().await;
}
