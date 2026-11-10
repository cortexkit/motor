use super::*;
use crate::hosted::{
    tests::harness::{Behavior, Harness, NoSsh},
    BillingStatus, Client, HOST_LOST_GRACE_S,
};
use motor_protocol::{AttachRequest, Outcome, Ran, StreamRecord};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn detached_job(h: &Harness, root: &std::path::Path) -> (Motor, Uuid) {
    let motor =
        Motor::with_connector(h.config(), root, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    motor.storage.hosted_route(id, h.route.clone()).unwrap();
    let (channel, lease) = Client::new(h.route.clone())
        .run(id, &Harness::request(root), &motor.storage)
        .await
        .unwrap();
    drop(channel);
    motor
        .storage
        .hosted_update(id, |s| {
            s.lease(&lease);
            s.run_sent = true;
        })
        .unwrap();
    h.requests.lock().unwrap().clear();
    (motor, id)
}

#[tokio::test]
async fn detached_never_sent_hosted_job_is_refused_once_without_reattach_or_resubmit() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-never-sent");
    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    motor.storage.hosted_route(id, h.route.clone()).unwrap();
    let now = crate::hosted::now_s() * 1000;
    let health = motor.machine_jobs_health_at(now).await;
    let job = motor.storage.job(id).unwrap();
    let terminal = job
        .local_terminal
        .expect("ownerless job must end before any reattach");
    // A returning caller must read this as safe to resubmit, not as a request
    // to avoid, since nothing ran.
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: motor_protocol::RefusalReason::LostToRestart
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.wall_ms, 0);
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("never started:"));
    assert_eq!(job.terminal_observed_at_ms, Some(now));
    let state = job.hosted.unwrap();
    assert_eq!(state.terminal, Some(terminal.clone()));
    assert!(state.hosted_no_fallback);
    assert!(state.finish_done);
    assert_eq!(health["linux"]["unknown"], 0);
    assert!(!motor.health_probes.lock().unwrap().contains_key(&id));
    for elapsed in [15_000, STALE_PROBE_INTERVAL_MS, LEGACY_JOB_HORIZON_MS] {
        motor.machine_jobs_health_at(now + elapsed).await;
    }
    assert_eq!(
        motor.job_status(id).await.unwrap().terminal,
        Some(terminal.clone())
    );
    let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
    assert_eq!(
        stream.records.recv().await.unwrap().unwrap(),
        StreamRecord::Terminal(terminal.clone())
    );
    drop(stream);
    drop(motor);
    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    motor.resume_finishes().await;
    motor
        .machine_jobs_health_at(now + 2 * STALE_PROBE_INTERVAL_MS)
        .await;
    let job = motor.storage.job(id).unwrap();
    assert_eq!(job.local_terminal, Some(terminal));
    assert_eq!(job.terminal_observed_at_ms, Some(now));
    assert!(motor.storage.unsettled_hosted().is_empty());
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn detached_never_sent_booking_is_settled_once_without_reattach_or_resubmit() {
    let h = Harness::new(Behavior::Normal).await;
    for booking in ["first_party", "boat", "lost_grant", "not_granted"] {
        let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-unused-booking");
        let motor =
            Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0))))
                .unwrap();
        let id = Uuid::now_v7();
        motor.storage.create(id, 60, None, None, false).unwrap();
        motor.storage.hosted_route(id, h.route.clone()).unwrap();
        let http = crate::hosted::test_http::Http::new(move |path, body| {
            if path.ends_with("/release") {
                assert_eq!(body, &json!({}));
                match booking {
                    "first_party" => (202, json!({"job_id": id, "release_at_ms": 0})),
                    "lost_grant" => (409, json!({"error": "job_admitted"})),
                    "not_granted" => (404, json!({"error": "job_unknown"})),
                    _ => panic!("Boat booking must finish without release"),
                }
            } else if path.ends_with("/finish") {
                assert!(matches!(booking, "boat" | "lost_grant"));
                assert_eq!(body, &json!({"outcome": "lost", "run_ms": 0}));
                (
                    200,
                    json!({"job_id": id, "settled_vcpu_ms": 0, "released_vcpu_ms": 120_000}),
                )
            } else {
                (404, json!({"error": "job_unknown"}))
            }
        })
        .await;
        motor
            .storage
            .hosted_update(id, |state| {
                state.broker_url = http.url.clone();
                state.lease_attempted = true;
                state.retry_iat = Some(crate::hosted::now_s());
                state.billing = BillingStatus::Pending;
                if matches!(booking, "first_party" | "boat") {
                    state.placement = Some(motor_lease::Placement {
                        host: if booking == "boat" {
                            "boat:unused"
                        } else {
                            "unused"
                        }
                        .into(),
                        vm: None,
                    });
                }
            })
            .unwrap();
        let now = crate::hosted::now_s() * 1000;
        motor.machine_jobs_health_at(now).await;
        let terminal = motor
            .storage
            .job(id)
            .unwrap()
            .local_terminal
            .expect("unused booking must be refused");
        assert!(matches!(
            terminal.outcome,
            Outcome::RefusedBeforeStart { .. }
        ));
        assert_eq!(terminal.ran, Some(Ran::None));
        assert_eq!(
            motor.job_status(id).await.unwrap().terminal,
            Some(terminal.clone())
        );
        let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
        assert_eq!(
            stream.records.recv().await.unwrap().unwrap(),
            StreamRecord::Terminal(terminal.clone())
        );
        drop(stream);
        motor.wait_hosted_cleanup(id).await;
        let state = motor.hosted_state(id).unwrap();
        assert_eq!(state.terminal, Some(terminal.clone()));
        assert!(state.finish_done);
        assert!(state.pending_finish.is_none());
        if matches!(booking, "boat" | "lost_grant") {
            assert_eq!(state.billing, BillingStatus::Available);
            assert_eq!(state.billed_vcpu_ms, Some(0));
        }
        motor
            .machine_jobs_health_at(now + STALE_PROBE_INTERVAL_MS)
            .await;
        drop(motor);
        let motor =
            Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0))))
                .unwrap();
        motor.resume_finishes().await;
        motor
            .machine_jobs_health_at(now + LEGACY_JOB_HORIZON_MS)
            .await;
        assert_eq!(
            motor.storage.job(id).unwrap().local_terminal,
            Some(terminal)
        );
        assert_eq!(
            motor.storage.job(id).unwrap().terminal_observed_at_ms,
            Some(now)
        );
        assert!(motor.storage.unsettled_hosted().is_empty());
        let paths: Vec<_> = http
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        let expected = match booking {
            "boat" => vec![format!("/v1/leases/{id}/finish")],
            "lost_grant" => vec![
                format!("/v1/leases/{id}/release"),
                format!("/v1/leases/{id}/finish"),
            ],
            _ => vec![format!("/v1/leases/{id}/release")],
        };
        assert_eq!(paths, expected, "{booking}");
    }
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn never_sent_hosted_recovery_after_restart_owns_the_booking_until_settlement() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-recovering");
    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    let id = Uuid::now_v7();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let http = crate::hosted::test_http::Http::with_async_reply({
        let entered = entered.clone();
        let release = release.clone();
        move |path, _| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                assert_eq!(path, format!("/v1/leases/{id}/release"));
                entered.notify_one();
                release.notified().await;
                (
                    202,
                    serde_json::to_vec(&json!({"job_id": id, "release_at_ms": 0})).unwrap(),
                )
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
            state.retry_iat = Some(0);
        })
        .unwrap();
    drop(motor);
    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    motor.resume_finishes().await;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let before = motor.storage.job(id).unwrap();
    let now = crate::hosted::now_s() * 1000;
    motor.machine_jobs_health_at(now).await;
    let after = motor.storage.job(id).unwrap();
    assert!(after.local_terminal.is_none());
    assert_eq!(after.hosted, before.hosted);
    assert_eq!(http.requests.lock().unwrap().len(), 1);
    release.notify_one();
    motor.wait_hosted_cleanup(id).await;
    assert!(motor.hosted_state(id).unwrap().finish_done);
    motor.machine_jobs_health_at(now + 15_000).await;
    let terminal = motor.storage.job(id).unwrap().local_terminal.unwrap();
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn never_sent_hosted_job_with_a_local_owner_is_not_retired_or_probed() {
    let h = Harness::new(Behavior::Normal).await;
    for owner in [
        "stream",
        "placement",
        "progress",
        "submission",
        "pending_cancel",
        "cancel_delivery",
        "archived_spill",
    ] {
        let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-owned");
        let motor =
            Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0))))
                .unwrap();
        let id = Uuid::now_v7();
        motor.storage.create(id, 60, None, None, false).unwrap();
        motor.storage.hosted_route(id, h.route.clone()).unwrap();
        let stream = (owner == "stream").then(|| motor.stream(id));
        let submission = (owner == "submission").then(|| motor.submission(id));
        let progress = matches!(owner, "placement" | "progress").then(|| {
            let reporter = progress::Reporter::new(false, 2);
            if owner == "placement" {
                reporter.phase(ProgressPhase::PlacementWait);
            }
            motor.observe_health_submission(id, Platform::Linux, reporter)
        });
        if owner == "pending_cancel" {
            motor.storage.cancel(id, true).unwrap();
        }
        if owner == "archived_spill" {
            crate::storage::legacy_fixtures::seed_direct_route(
                &motor.storage,
                id,
                crate::storage::legacy_fixtures::DirectRoute::SpillReturn {
                    runner_job_id: Uuid::now_v7(),
                    hosted_code: "hosted_connect_failed",
                },
            )
            .unwrap();
            assert!(!motor
                .retire_unstarted_hosted(id, crate::hosted::now_s() * 1000)
                .unwrap());
        }
        let cancel = (owner == "cancel_delivery").then(|| motor.deliver_cancel(id));
        let before = motor.storage.job(id).unwrap();
        let now = crate::hosted::now_s() * 1000 + LEGACY_JOB_HORIZON_MS;
        let health = motor.machine_jobs_health_at(now).await;
        let after = motor.storage.job(id).unwrap();
        assert!(after.local_terminal.is_none(), "{owner}");
        assert_eq!(after.hosted, before.hosted, "{owner}");
        assert_eq!(after.pending_cancel, before.pending_cancel, "{owner}");
        if owner == "placement" {
            assert_eq!(health["linux"]["waiting_for_capacity"], 1);
        }
        assert!(h.requests.lock().unwrap().is_empty(), "{owner}");
        drop((stream, submission, progress, cancel));
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hosted_finished_health_probe_retires_once_across_restart_without_reattach_or_resubmit() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-finished");
    let (motor, id) = detached_job(&h, &scratch).await;
    h.finished.store(true, Ordering::SeqCst);
    let now = crate::hosted::now_s() * 1000;
    motor.machine_jobs_health_at(now).await;
    let job = motor.storage.job(id).unwrap();
    let terminal = job.local_terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert_eq!(terminal.ran, Some(Ran::Unknown("maybe".into())));
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("broker reports the job finished; no result reached ck-motor"));
    assert_eq!(job.terminal_observed_at_ms, Some(now));
    let state = job.hosted.unwrap();
    assert_eq!(state.terminal, Some(terminal.clone()));
    assert!(state.finish_done);
    assert!(state.hosted_no_fallback);
    assert_eq!(state.billing, BillingStatus::Unavailable);
    for elapsed in [15_000, 60_000, STALE_PROBE_INTERVAL_MS] {
        motor.machine_jobs_health_at(now + elapsed).await;
    }
    assert_eq!(
        motor.job_status(id).await.unwrap().terminal,
        Some(terminal.clone())
    );
    let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
    assert_eq!(
        stream.records.recv().await.unwrap().unwrap(),
        StreamRecord::Terminal(terminal.clone())
    );
    drop(stream);
    drop(motor);

    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    motor.resume_finishes().await;
    motor
        .machine_jobs_health_at(now + 2 * STALE_PROBE_INTERVAL_MS)
        .await;
    let job = motor.storage.job(id).unwrap();
    assert_eq!(job.local_terminal, Some(terminal));
    assert_eq!(job.terminal_observed_at_ms, Some(now));
    assert!(motor.storage.unsettled_hosted().is_empty());
    assert_eq!(
        *h.requests.lock().unwrap(),
        vec![format!("/v1/leases/{id}/reattach")]
    );
    assert!(h.frames.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hosted_unknown_past_deadline_retires_once_across_restart_without_reattach_or_resubmit() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-unknown");
    let (motor, id) = detached_job(&h, &scratch).await;
    let http =
        crate::hosted::test_http::Http::new(|_, _| (404, json!({"error": "job_unknown"}))).await;
    motor
        .storage
        .hosted_update(id, |s| {
            s.broker_url = http.url.clone();
            s.placement.as_mut().unwrap().host = "boat:unknown".into();
            s.billing = BillingStatus::Pending;
        })
        .unwrap();
    let now = motor
        .hosted_state(id)
        .unwrap()
        .reattach_deadline_s()
        .unwrap()
        * 1000
        + 1;
    motor.machine_jobs_health_at(now).await;
    let job = motor.storage.job(id).unwrap();
    let terminal = job.local_terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert_eq!(terminal.ran, Some(Ran::Unknown("maybe".into())));
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("broker no longer knows the job past its deadline; no result reached ck-motor"));
    assert_eq!(job.terminal_observed_at_ms, Some(now));
    let state = job.hosted.unwrap();
    assert_eq!(state.terminal, Some(terminal.clone()));
    assert!(state.finish_done);
    assert!(state.pending_finish.is_none());
    assert!(state.hosted_no_fallback);
    assert_eq!(state.billing, BillingStatus::Unavailable);
    for elapsed in [15_000, 60_000, STALE_PROBE_INTERVAL_MS] {
        motor.machine_jobs_health_at(now + elapsed).await;
    }
    assert_eq!(
        motor.job_status(id).await.unwrap().terminal,
        Some(terminal.clone())
    );
    let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
    assert_eq!(
        stream.records.recv().await.unwrap().unwrap(),
        StreamRecord::Terminal(terminal.clone())
    );
    drop(stream);
    drop(motor);

    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    motor.resume_finishes().await;
    motor
        .machine_jobs_health_at(now + 2 * STALE_PROBE_INTERVAL_MS)
        .await;
    let job = motor.storage.job(id).unwrap();
    assert_eq!(job.local_terminal, Some(terminal));
    assert_eq!(job.terminal_observed_at_ms, Some(now));
    assert!(motor.storage.unsettled_hosted().is_empty());
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, format!("/v1/leases/{id}/reattach"));
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hosted_unknown_before_deadline_shares_recovery_and_never_probes_provisioning() {
    let h = Harness::new(Behavior::Normal).await;
    for run_sent in [false, true] {
        let scratch =
            cortexkit_test_support::ScratchDir::new("health-hosted-unknown-before-deadline");
        let (motor, id) = if run_sent {
            detached_job(&h, &scratch).await
        } else {
            let motor =
                Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0))))
                    .unwrap();
            let id = Uuid::now_v7();
            motor.storage.create(id, 60, None, None, false).unwrap();
            motor.storage.hosted_route(id, h.route.clone()).unwrap();
            (motor, id)
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let http = crate::hosted::test_http::Http::with_async_reply({
            let entered = entered.clone();
            let release = release.clone();
            move |_, _| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    (
                        404,
                        serde_json::to_vec(&json!({"error": "job_unknown"})).unwrap(),
                    )
                }
            }
        })
        .await;
        motor
            .storage
            .hosted_update(id, |s| s.broker_url = http.url.clone())
            .unwrap();
        let _submission = (!run_sent).then(|| motor.submission(id));
        let state = motor.hosted_state(id).unwrap();
        let now = crate::hosted::now_s() * 1000;
        let limit = state.reattach_deadline_s().map_or_else(
            || deadline(id, &motor.storage.job(id).unwrap()),
            |d| d * 1000,
        );
        assert!(now + 210_000 < limit);
        let first = if run_sent {
            let motor = motor.clone();
            let first = tokio::spawn(async move { motor.machine_jobs_health_at(now).await });
            entered.notified().await;
            Some(first)
        } else {
            None
        };
        for elapsed in [0, 15_000, 29_999, 30_000, 89_999, 90_000, 209_999, 210_000] {
            motor.machine_jobs_health_at(now + elapsed).await;
            assert_eq!(http.requests.lock().unwrap().len(), usize::from(run_sent));
            let job = motor.storage.job(id).unwrap();
            assert!(job.local_terminal.is_none());
            let current = job.hosted.unwrap();
            assert!(current.terminal.is_none());
            assert!(!current.finish_done);
            assert_eq!(current.retry_iat, state.retry_iat);
            assert_eq!(current.run_sent, run_sent);
        }
        if let Some(first) = first {
            release.notify_one();
            first.await.unwrap();
        }
        assert!(h.requests.lock().unwrap().is_empty());
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        drop(_submission);
        motor.close().await;
    }
}

#[tokio::test]
async fn legacy_hosted_unknown_uses_the_conservative_health_deadline() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-legacy-unknown");
    let (motor, id) = detached_job(&h, &scratch).await;
    let http =
        crate::hosted::test_http::Http::new(|_, _| (404, json!({"error": "job_unknown"}))).await;
    motor
        .storage
        .hosted_update(id, |s| {
            s.broker_url = http.url.clone();
            s.retry_iat = None;
        })
        .unwrap();
    let now = crate::hosted::now_s() * 1000;
    motor.machine_jobs_health_at(now).await;
    assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    motor
        .machine_jobs_health_at(now + LEGACY_JOB_HORIZON_MS + 1000)
        .await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while motor.storage.job(id).unwrap().local_terminal.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("legacy unknown recovery must retire at its conservative deadline");
    assert_eq!(
        motor
            .storage
            .job(id)
            .unwrap()
            .local_terminal
            .unwrap()
            .outcome,
        Outcome::OutcomeUnknown
    );
    assert!(motor.hosted_state(id).unwrap().finish_done);
    let attempts = http.requests.lock().unwrap().len();
    motor
        .machine_jobs_health_at(now + LEGACY_JOB_HORIZON_MS + 2000)
        .await;
    assert_eq!(http.requests.lock().unwrap().len(), attempts);
    motor.close().await;
}

#[tokio::test]
async fn hosted_auth_health_and_status_share_one_recovery_until_original_deadline() {
    let h = Harness::new(Behavior::RefuseAttach("auth")).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-auth");
    let (motor, id) = detached_job(&h, &scratch).await;
    let state = motor.hosted_state(id).unwrap();
    let issued = state.retry_iat.unwrap();
    let now = issued * 1000;
    let deadline = (issued + 30 + HOST_LOST_GRACE_S) * 1000;
    assert_eq!(state.reattach_deadline_s().unwrap() * 1000, deadline);
    motor.storage.health_deadline(id, None).unwrap();
    let first = tokio::spawn({
        let motor = motor.clone();
        async move { motor.machine_jobs_health_at(now).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for elapsed in [15_000, 30_000, 90_000] {
        motor.machine_jobs_health_at(now + elapsed).await;
        motor.job_status(id).await.unwrap();
        assert_eq!(
            h.requests.lock().unwrap().len(),
            1,
            "health/status must add no request to the owner's held attempt"
        );
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    }
    h.run_gate.send_replace(true);
    first.await.unwrap();
    motor.machine_jobs_health_at(deadline).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while motor.storage.job(id).unwrap().local_terminal.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle auth recovery must retire at the original deadline");
    let terminal = motor.storage.job(id).unwrap().local_terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert_eq!(terminal.ran, Some(Ran::Unknown("maybe".into())));
    assert!(terminal
        .refusal_detail
        .unwrap()
        .contains("auth past the job's deadline"));
    assert_eq!(motor.hosted_state(id).unwrap().retry_iat, Some(issued));
    let count = h.requests.lock().unwrap().len();
    motor
        .machine_jobs_health_at(deadline + STALE_PROBE_INTERVAL_MS)
        .await;
    assert_eq!(h.requests.lock().unwrap().len(), count);
    assert!(h.frames.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn legacy_hosted_auth_uses_the_conservative_health_deadline() {
    let h = Harness::new(Behavior::RefuseAttach("auth")).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-legacy-auth");
    let (motor, id) = detached_job(&h, &scratch).await;
    motor
        .storage
        .hosted_update(id, |s| s.retry_iat = None)
        .unwrap();
    let now = crate::hosted::now_s() * 1000;
    motor.machine_jobs_health_at(now).await;
    assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    motor
        .machine_jobs_health_at(now + LEGACY_JOB_HORIZON_MS + 1000)
        .await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while motor.storage.job(id).unwrap().local_terminal.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("legacy idle auth recovery must observe its conservative deadline");
    assert_eq!(
        motor
            .storage
            .job(id)
            .unwrap()
            .local_terminal
            .unwrap()
            .outcome,
        Outcome::OutcomeUnknown
    );
    let attempts = h.requests.lock().unwrap().len();
    motor
        .machine_jobs_health_at(now + LEGACY_JOB_HORIZON_MS + 2000)
        .await;
    assert_eq!(h.requests.lock().unwrap().len(), attempts);
    motor.close().await;
}

#[tokio::test]
async fn broker_auth_failure_is_shared_by_health_and_status_without_false_retirement() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-broker-auth");
    let (motor, id) = detached_job(&h, &scratch).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let http = crate::hosted::test_http::Http::with_async_reply({
        let entered = entered.clone();
        let release = release.clone();
        move |_, _| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                (401, serde_json::to_vec(&json!({"error": "auth"})).unwrap())
            }
        }
    })
    .await;
    motor
        .storage
        .hosted_update(id, |s| {
            s.broker_url = http.url.clone();
            s.retry_iat = Some(0);
        })
        .unwrap();
    let now = crate::hosted::now_s() * 1000;
    let first = tokio::spawn({
        let motor = motor.clone();
        async move { motor.machine_jobs_health_at(now).await }
    });
    entered.notified().await;
    for elapsed in [0, 30_000, 90_000, 210_000, 450_000, 930_000, 1_830_000] {
        motor.machine_jobs_health_at(now + elapsed).await;
        motor.job_status(id).await.unwrap();
        assert_eq!(
            http.requests.lock().unwrap().len(),
            1,
            "only the elected owner's attempt may open"
        );
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    }
    release.notify_one();
    first.await.unwrap();
    entered.notified().await;
    assert_eq!(http.requests.lock().unwrap().len(), 2);
    assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    motor.close().await;
}

#[tokio::test]
async fn hosted_running_owner_keeps_the_job_even_past_deadline_without_rival_probes() {
    let h = Harness::new(Behavior::QuietAttach).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-running");
    let (motor, id) = detached_job(&h, &scratch).await;
    motor
        .storage
        .hosted_update(id, |s| s.retry_iat = Some(0))
        .unwrap();
    let now = crate::hosted::now_s() * 1000;
    let health = motor.machine_jobs_health_at(now).await;
    assert_eq!(health["linux"]["running"], 1);
    owner_attach_count(&h, 1).await;
    let health = motor.machine_jobs_health_at(now + 15_000).await;
    assert_eq!(health["linux"]["running"], 1);
    assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    assert!(motor.hosted_state(id).unwrap().terminal.is_none());
    assert!(!motor.hosted_state(id).unwrap().finish_done);
    assert_eq!(h.requests.lock().unwrap().len(), 2);
    assert_eq!(
        h.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| matches!(f, Frame::JobStatus { .. }))
            .count(),
        1
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    h.run_gate.send_replace(true);
    owner_settled(&motor, id).await;
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.unwrap().outcome,
        Outcome::Exit { code: 23 }
    );
    motor.close().await;
}

async fn result_record(stream: &mut crate::JobStream) -> StreamRecord {
    tokio::time::timeout(Duration::from_secs(15), stream.records.recv())
        .await
        .expect("owner must deliver a record")
        .expect("stream closed")
        .unwrap()
}

async fn owner_settled(motor: &Motor, id: Uuid) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while motor.hosted_state(id).unwrap().billing == BillingStatus::Pending {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner must retain settlement without a caller");
}

async fn owner_attach_count(h: &Harness, count: usize) {
    let mut lifecycle = h.attach_lifecycle.subscribe();
    tokio::time::timeout(
        Duration::from_secs(15),
        lifecycle.wait_for(|s| s.2 >= count),
    )
    .await
    .expect("owner must reach gated host replay")
    .unwrap();
}

#[tokio::test]
async fn health_ended_hosted_reply_retains_terminal_and_recovers_settlement() {
    let h = Harness::new(Behavior::StatusTerminal).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("health-ended-hosted-owner");
    let (motor, id) = detached_job(&h, &scratch).await;
    motor
        .machine_jobs_health_at(crate::hosted::now_s() * 1000)
        .await;
    let terminal = motor
        .hosted_state(id)
        .unwrap()
        .terminal
        .expect("a hosted terminal in a successful health reply must be durable");
    assert_eq!(terminal.job_id, id);
    assert_eq!(terminal.outcome, Outcome::Exit { code: 41 });
    assert_eq!(terminal.ran, Some(Ran::Remote));
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        BillingStatus::Pending
    );
    owner_attach_count(&h, 1).await;
    h.run_gate.send_replace(true);
    owner_settled(&motor, id).await;
    let requests = h.requests.lock().unwrap().len();
    for elapsed in [15_000, 30_000, 3_600_000] {
        motor
            .machine_jobs_health_at(crate::hosted::now_s() * 1000 + elapsed)
            .await;
    }
    assert_eq!(h.requests.lock().unwrap().len(), requests);
    assert_eq!(h.attach_lifecycle.borrow().2, 1);
    assert_eq!(motor.hosted_state(id).unwrap().terminal, Some(terminal));
    assert_eq!(motor.hosted_state(id).unwrap().billed_vcpu_ms, Some(6001));
    assert!(!h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Cancel(_))));
    motor.close().await;
}

#[tokio::test]
async fn caller_joins_internal_hosted_follower_while_health_and_status_probe() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-local-join");
    let (motor, id) = detached_job(&h, &scratch).await;
    motor.resume_finishes().await;
    owner_attach_count(&h, 1).await;
    assert_eq!(owner_replay_cursors(&h), vec![u64::MAX]);
    let mut caller = motor.attach(AttachRequest::new(id, 7)).unwrap();
    let (health, status) = tokio::join!(
        motor.machine_jobs_health_at(crate::hosted::now_s() * 1000),
        motor.job_status(id),
    );
    assert!(status.unwrap().terminal.is_none());
    assert!(health["linux"]["unknown"].as_u64().is_some());
    owner_outputs(&motor, id, &mut caller, 7).await;
    assert_eq!(owner_replay_cursors(&h), vec![u64::MAX, 7]);
    h.run_gate.send_replace(true);
    let StreamRecord::Terminal(terminal) = result_record(&mut caller).await else {
        panic!("terminal missing")
    };
    assert_eq!(terminal.outcome, Outcome::Exit { code: 41 });
    assert_eq!(motor.hosted_state(id).unwrap().billed_vcpu_ms, Some(6001));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(!h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::JobStatus { .. } | Frame::Cancel(_))));
    drop(caller);
    motor.close().await;
}

#[tokio::test]
async fn health_auth_past_deadline_cannot_retire_live_hosted_result_owner() {
    let h = Harness::new(Behavior::QuietAttach).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-auth-fence");
    let (motor, id) = detached_job(&h, &scratch).await;
    let mut caller = motor.attach(AttachRequest::new(id, 0)).unwrap();
    owner_attach_count(&h, 1).await;
    let expired = motor
        .hosted_state(id)
        .unwrap()
        .reattach_deadline_s()
        .unwrap()
        * 1000
        + 1;
    motor.machine_jobs_health_at(expired).await;
    assert!(
        motor.storage.job(id).unwrap().local_terminal.is_none(),
        "a live owner must fence auth retirement"
    );
    assert!(motor.hosted_state(id).unwrap().terminal.is_none());
    h.run_gate.send_replace(true);
    assert!(matches!(
        result_record(&mut caller).await,
        StreamRecord::Output(_)
    ));
    let StreamRecord::Terminal(terminal) = result_record(&mut caller).await else {
        panic!("terminal missing")
    };
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    // The caller already consumed seq 0; only the terminal summary is pending.
    let mut later = motor.attach(AttachRequest::new(id, 1)).unwrap();
    assert_eq!(
        result_record(&mut later).await,
        StreamRecord::Terminal(terminal)
    );
    assert_eq!(h.attach_lifecycle.borrow().2, 1);
    assert!(!h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Cancel(_))));
    drop((caller, later));
    motor.close().await;
}

fn owner_replay_cursors(h: &Harness) -> Vec<u64> {
    h.frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            Frame::Attach(request) => Some(request.from_seq),
            _ => None,
        })
        .collect()
}

async fn owner_outputs(motor: &Motor, id: Uuid, stream: &mut crate::JobStream, from: u64) {
    use crate::hosted::tests::harness::{OWNER_REPLAY_BYTES, OWNER_REPLAY_RECORDS};
    for expected in from..OWNER_REPLAY_RECORDS {
        let record = tokio::time::timeout(Duration::from_secs(15), stream.records.recv())
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "stream from {from} missing output {expected}: {error}; {}",
                    motor.hosted_result_snapshot_for_test(id)
                )
            })
            .expect("owner stream must remain open")
            .expect("owner must deliver a valid record");
        let StreamRecord::Output(output) = record else {
            panic!("output {expected} missing")
        };
        assert_eq!(output.seq, expected);
        assert_eq!(output.bytes.0, vec![expected as u8; OWNER_REPLAY_BYTES]);
        assert_eq!(output.truncated_before_seq, None);
    }
}

#[tokio::test]
async fn hosted_late_subscriber_rewinds_single_host_connection() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-late-rewind");
    let (motor, id) = detached_job(&h, &scratch).await;
    let mut first = motor.attach(AttachRequest::new(id, 0)).unwrap();
    owner_outputs(&motor, id, &mut first, 0).await;
    let mut late = motor.attach(AttachRequest::new(id, 11)).unwrap();
    owner_outputs(&motor, id, &mut late, 11).await;
    assert_eq!(owner_replay_cursors(&h), vec![0, 11]);
    assert_eq!(
        h.attach_lifecycle.borrow().0 - h.attach_lifecycle.borrow().1,
        1
    );
    h.run_gate.send_replace(true);
    assert!(matches!(
        result_record(&mut first).await,
        StreamRecord::Terminal(_)
    ));
    assert!(matches!(
        result_record(&mut late).await,
        StreamRecord::Terminal(_)
    ));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    drop((first, late));
    motor.close().await;
}

#[tokio::test]
async fn hosted_concurrent_rewinds_fold_to_oldest_waiting_cursor() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-fold-rewind");
    let (motor, id) = detached_job(&h, &scratch).await;
    let mut first = motor.attach(AttachRequest::new(id, 0)).unwrap();
    owner_outputs(&motor, id, &mut first, 0).await;
    let mut newer = motor.attach(AttachRequest::new(id, 17)).unwrap();
    let mut older = motor.attach(AttachRequest::new(id, 3)).unwrap();
    tokio::join!(
        owner_outputs(&motor, id, &mut newer, 17),
        owner_outputs(&motor, id, &mut older, 3)
    );
    assert_eq!(owner_replay_cursors(&h), vec![0, 3]);
    h.run_gate.send_replace(true);
    for (name, stream) in [
        ("first", &mut first),
        ("newer", &mut newer),
        ("older", &mut older),
    ] {
        let record = tokio::time::timeout(Duration::from_secs(15), stream.records.recv())
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{name} terminal missing: {error}; {}",
                    motor.hosted_result_snapshot_for_test(id)
                )
            })
            .expect("owner stream must remain open")
            .expect("owner must deliver a valid record");
        assert!(matches!(record, StreamRecord::Terminal(_)));
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    drop((first, newer, older));
    motor.close().await;
}

async fn assert_rewind_auth_outcome(refusals: usize, expected: Outcome) {
    let h = Harness::new(Behavior::OwnerReplay).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-expired-rewind-auth");
    let (motor, id) = detached_job(&h, &scratch).await;
    let mut first = motor.attach(AttachRequest::new(id, 0)).unwrap();
    owner_outputs(&motor, id, &mut first, 0).await;
    let tokens = h.tokens.load(Ordering::SeqCst);
    motor
        .storage
        .hosted_update(id, |state| state.retry_iat = Some(0))
        .unwrap();
    h.attach_auth_refusals.store(refusals, Ordering::SeqCst);
    let mut late = motor.attach(AttachRequest::new(id, 11)).unwrap();
    let recovered = matches!(expected, Outcome::Exit { code: 41 });
    if recovered {
        owner_outputs(&motor, id, &mut late, 11).await;
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
        h.run_gate.send_replace(true);
    }
    let StreamRecord::Terminal(terminal) = result_record(&mut late).await else {
        panic!("terminal missing")
    };
    assert_eq!(terminal.outcome, expected);
    assert_eq!(
        result_record(&mut first).await,
        StreamRecord::Terminal(terminal.clone())
    );
    assert_eq!(h.tokens.load(Ordering::SeqCst) - tokens, 3);
    assert_eq!(h.attach_auth_refusals.load(Ordering::SeqCst), 0);
    assert_eq!(
        owner_replay_cursors(&h),
        if recovered { vec![0, 11] } else { vec![0] }
    );
    if recovered {
        assert_eq!(motor.hosted_state(id).unwrap().billed_vcpu_ms, Some(6001));
    } else {
        assert!(terminal
            .refusal_detail
            .as_deref()
            .unwrap()
            .contains("auth past the job's deadline"));
        assert_eq!(
            motor.storage.job(id).unwrap().local_terminal,
            Some(terminal)
        );
        assert_eq!(
            motor.hosted_state(id).unwrap().billing,
            BillingStatus::Unavailable
        );
    }
    assert!(!h
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Cancel(_) | Frame::Run(_))));
    drop((first, late));
    motor.close().await;
}

#[tokio::test]
async fn hosted_rewind_past_deadline_retries_two_auth_refusals_and_delivers_result() {
    assert_rewind_auth_outcome(2, Outcome::Exit { code: 41 }).await;
}

#[tokio::test]
async fn hosted_rewind_past_deadline_retires_after_third_auth_refusal() {
    assert_rewind_auth_outcome(3, Outcome::OutcomeUnknown).await;
}

#[tokio::test]
async fn hosted_restart_collects_terminal_without_replaying_until_subscriber_arrives() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    h.run_gate.send_replace(false);
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-restart-no-replay");
    let (motor, id) = detached_job(&h, &scratch).await;
    motor.resume_finishes().await;
    owner_attach_count(&h, 1).await;
    assert_eq!(owner_replay_cursors(&h), vec![u64::MAX]);
    motor.close().await;
    let mut lifecycle = h.attach_lifecycle.subscribe();
    tokio::time::timeout(Duration::from_secs(15), lifecycle.wait_for(|s| s.1 == 1))
        .await
        .unwrap()
        .unwrap();
    let motor =
        Motor::with_connector(h.config(), &scratch, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    motor.resume_finishes().await;
    owner_attach_count(&h, 2).await;
    assert_eq!(owner_replay_cursors(&h), vec![u64::MAX, u64::MAX]);
    let mut caller = motor.attach(AttachRequest::new(id, 9)).unwrap();
    owner_outputs(&motor, id, &mut caller, 9).await;
    assert_eq!(owner_replay_cursors(&h), vec![u64::MAX, u64::MAX, 9]);
    h.run_gate.send_replace(true);
    assert!(matches!(
        result_record(&mut caller).await,
        StreamRecord::Terminal(_)
    ));
    drop(caller);
    motor.close().await;
}

impl Motor {
    pub(crate) fn expire_hosted_result_for_test(&self, id: Uuid) {
        self.storage
            .hosted_update(id, |state| state.retry_iat = Some(0))
            .unwrap();
    }
}

async fn completed_detached_owner(h: &Harness, root: &std::path::Path) -> (Motor, Uuid) {
    let (motor, id) = detached_job(h, root).await;
    // The result connection outlives its caller and receives the entire stream
    // without any subscribers. Wait for retirement, not just terminal storage,
    // so the following attach cannot accidentally use the old in-memory ring.
    motor.ensure_hosted_result_at(id, 1);
    let mut stopped = motor.hosted_result_stopped_for_test(id);
    tokio::time::timeout(
        Duration::from_secs(15),
        stopped.wait_for(|stopped| *stopped),
    )
    .await
    .expect("detached completed owner must retire")
    .unwrap();
    assert!(motor.storage.job(id).unwrap().hosted_complete());
    assert_eq!(motor.hosted_state(id).unwrap().replay_end_seq, Some(64));
    (motor, id)
}

async fn assert_completed_replay(motor: &Motor, id: Uuid) {
    let mut caller = motor.attach(AttachRequest::new(id, 1)).unwrap();
    let has_owner = motor.hosted_results.lock().unwrap().contains_key(&id);
    let stopped = has_owner.then(|| motor.hosted_result_stopped_for_test(id));
    owner_outputs(motor, id, &mut caller, 1).await;
    assert!(matches!(result_record(&mut caller).await,
        StreamRecord::Terminal(terminal) if terminal.outcome == Outcome::Exit { code: 41 }));
    assert!(caller.records.recv().await.is_none());
    tokio::time::timeout(
        Duration::from_secs(15),
        stopped.unwrap().wait_for(|stopped| *stopped),
    )
    .await
    .unwrap()
    .unwrap();
}

async fn assert_expired_replay(motor: &Motor, id: Uuid) {
    let mut caller = motor.attach(AttachRequest::new(id, 1)).unwrap();
    assert!(matches!(result_record(&mut caller).await,
        StreamRecord::Terminal(terminal) if terminal.outcome == Outcome::HistoryExpired));
    assert!(caller.records.recv().await.is_none());
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.unwrap().outcome,
        Outcome::Exit { code: 41 }
    );
}

#[tokio::test]
async fn hosted_finished_late_attach_replays_after_owner_retirement() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    let root = cortexkit_test_support::ScratchDir::new("hosted-finished-late-attach");
    let (motor, id) = completed_detached_owner(&h, &root).await;
    assert_completed_replay(&motor, id).await;
    assert_eq!(owner_replay_cursors(&h), vec![1, 1]);
    let mut boundary = motor.attach(AttachRequest::new(id, 63)).unwrap();
    owner_outputs(&motor, id, &mut boundary, 63).await;
    assert!(matches!(
        result_record(&mut boundary).await,
        StreamRecord::Terminal(_)
    ));
    drop(boundary);
    let mut summary = motor.attach(AttachRequest::new(id, 64)).unwrap();
    assert!(matches!(
        result_record(&mut summary).await,
        StreamRecord::Terminal(_)
    ));
    assert_eq!(owner_replay_cursors(&h), vec![1, 1, 63]);
    drop(summary);
    motor.close().await;
}

#[tokio::test]
async fn hosted_finished_late_attach_reports_expired_host_history() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    let root = cortexkit_test_support::ScratchDir::new("hosted-finished-expired");
    let (motor, id) = completed_detached_owner(&h, &root).await;
    h.attach_history_expired.store(true, Ordering::SeqCst);
    assert_expired_replay(&motor, id).await;
    assert_eq!(owner_replay_cursors(&h), vec![1, 1]);
    motor.close().await;
}

#[tokio::test]
async fn hosted_finished_late_attach_does_not_use_billing_delivery_deadline() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    let root = cortexkit_test_support::ScratchDir::new("hosted-finished-slow-replay");
    let (motor, id) = completed_detached_owner(&h, &root).await;
    h.run_gate.send_replace(false);
    let mut caller = motor.attach(AttachRequest::new(id, 1)).unwrap();
    owner_outputs(&motor, id, &mut caller, 1).await;
    // Billing normally gets five seconds after a terminal. A replay's output
    // and terminal may take longer even though billing already completed.
    assert!(
        tokio::time::timeout(Duration::from_secs(6), caller.records.recv())
            .await
            .is_err()
    );
    h.run_gate.send_replace(true);
    assert!(matches!(result_record(&mut caller).await,
        StreamRecord::Terminal(terminal) if terminal.outcome == Outcome::Exit { code: 41 }));
    drop(caller);
    motor.close().await;
}

#[tokio::test]
async fn hosted_finished_late_attach_reports_retired_boat_history() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    let root = cortexkit_test_support::ScratchDir::new("hosted-finished-boat");
    let (motor, id) = completed_detached_owner(&h, &root).await;
    motor
        .storage
        .hosted_update(id, |state| {
            state.placement.as_mut().unwrap().host = "boat:retired".into();
            state.finish_done = true;
        })
        .unwrap();
    assert_expired_replay(&motor, id).await;
    assert_eq!(owner_replay_cursors(&h), vec![1]);
    motor.close().await;
}

#[tokio::test]
async fn hosted_finished_late_attach_reports_refused_host_replay() {
    let h = Harness::new(Behavior::OwnerReplay).await;
    let root = cortexkit_test_support::ScratchDir::new("hosted-finished-refused");
    let (motor, id) = completed_detached_owner(&h, &root).await;
    h.attach_auth_refusals.store(1, Ordering::SeqCst);
    assert_expired_replay(&motor, id).await;
    assert_eq!(h.attach_auth_refusals.load(Ordering::SeqCst), 0);
    assert_eq!(owner_replay_cursors(&h), vec![1]);
    motor.close().await;
}

#[tokio::test]
async fn hosted_finished_late_attach_replays_after_journal_recovery() {
    for expired in [false, true] {
        let h = Harness::new(Behavior::OwnerReplay).await;
        let root = cortexkit_test_support::ScratchDir::new("hosted-finished-journal-source");
        let recovered = cortexkit_test_support::ScratchDir::new("hosted-finished-journal-recovery");
        let (motor, id) = completed_detached_owner(&h, &root).await;
        // Copy the durable files before orderly shutdown checkpoints the journal.
        for entry in std::fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), recovered.join(entry.file_name())).unwrap();
            }
        }
        motor.close().await;
        let motor =
            Motor::with_connector(h.config(), &recovered, Arc::new(NoSsh(AtomicUsize::new(0))))
                .unwrap();
        assert_eq!(motor.hosted_state(id).unwrap().replay_end_seq, Some(64));
        h.attach_history_expired.store(expired, Ordering::SeqCst);
        if expired {
            assert_expired_replay(&motor, id).await;
        } else {
            assert_completed_replay(&motor, id).await;
        }
        assert_eq!(owner_replay_cursors(&h), vec![1, 1]);
        motor.close().await;
    }
}

#[tokio::test]
async fn hosted_finished_old_store_without_replay_end_seq_still_requests_replay() {
    for expired in [false, true] {
        let h = Harness::new(Behavior::OwnerReplay).await;
        let root = cortexkit_test_support::ScratchDir::new("hosted-finished-old-source");
        let recovered = cortexkit_test_support::ScratchDir::new("hosted-finished-old-store");
        let (motor, id) = completed_detached_owner(&h, &root).await;
        let mut job = serde_json::to_value(motor.storage.job(id).unwrap()).unwrap();
        job["hosted"]
            .as_object_mut()
            .unwrap()
            .remove("replay_end_seq");
        std::fs::write(
            recovered.join("state.json"),
            serde_json::to_vec(&serde_json::json!({
                "jobs": {id.to_string(): job}, "snapshots": {},
            }))
            .unwrap(),
        )
        .unwrap();
        motor.close().await;
        let motor =
            Motor::with_connector(h.config(), &recovered, Arc::new(NoSsh(AtomicUsize::new(0))))
                .unwrap();
        let state = motor.hosted_state(id).unwrap();
        assert_eq!(state.replay_end_seq, None);
        assert!(serde_json::to_value(state)
            .unwrap()
            .get("replay_end_seq")
            .is_none());
        h.attach_history_expired.store(expired, Ordering::SeqCst);
        if expired {
            assert_expired_replay(&motor, id).await;
        } else {
            assert_completed_replay(&motor, id).await;
        }
        assert_eq!(owner_replay_cursors(&h), vec![1, 1]);
        motor.close().await;
    }
}
