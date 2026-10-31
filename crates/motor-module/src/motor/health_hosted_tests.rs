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
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
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
            motor
                .storage
                .resubmitted(id, Uuid::now_v7(), "hosted_connect_failed", None)
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
async fn hosted_unknown_before_deadline_backs_off_without_retiring_even_while_provisioning() {
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
        let http =
            crate::hosted::test_http::Http::new(|_, _| (404, json!({"error": "job_unknown"})))
                .await;
        motor
            .storage
            .hosted_update(id, |s| s.broker_url = http.url.clone())
            .unwrap();
        let _submission = (!run_sent).then(|| motor.submission(id));
        let state = motor.hosted_state(id).unwrap();
        let now = crate::hosted::now_s() * 1000;
        let deadline = state.reattach_deadline_s().map_or_else(
            || deadline(id, &motor.storage.job(id).unwrap()),
            |d| d * 1000,
        );
        assert!(now + 210_000 < deadline);
        for (elapsed, attempts) in [
            (0, 1),
            (15_000, 1),
            (29_999, 1),
            (30_000, 2),
            (89_999, 2),
            (90_000, 3),
            (209_999, 3),
            (210_000, 4),
        ] {
            motor.machine_jobs_health_at(now + elapsed).await;
            assert_eq!(
                http.requests.lock().unwrap().len(),
                if run_sent { attempts } else { 0 },
                "at {elapsed}ms, run_sent={run_sent}"
            );
            let job = motor.storage.job(id).unwrap();
            assert!(job.local_terminal.is_none());
            let current = job.hosted.unwrap();
            assert!(current.terminal.is_none());
            assert!(!current.finish_done);
            assert_eq!(current.retry_iat, state.retry_iat);
            assert_eq!(current.run_sent, run_sent);
        }
        assert!(h.requests.lock().unwrap().is_empty());
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
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
    assert_eq!(http.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn hosted_auth_health_probes_back_off_until_original_deadline_then_retire() {
    let h = Harness::new(Behavior::RefuseAttach("auth")).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-auth");
    let (motor, id) = detached_job(&h, &scratch).await;
    let state = motor.hosted_state(id).unwrap();
    let issued = state.retry_iat.unwrap();
    let now = issued * 1000;
    let deadline = (issued + 30 + HOST_LOST_GRACE_S) * 1000;
    assert_eq!(state.reattach_deadline_s().unwrap() * 1000, deadline);
    // A legacy generic health deadline must not replace the original Run limit.
    motor.storage.health_deadline(id, None).unwrap();
    for (elapsed, attempts) in [
        (0, 1),
        (15_000, 1),
        (29_999, 1),
        (30_000, 2),
        (45_000, 2),
        (89_999, 2),
        (90_000, 3),
        (120_000, 3),
        (209_999, 3),
        (210_000, 4),
        (450_000, 5),
    ] {
        motor.machine_jobs_health_at(now + elapsed).await;
        assert_eq!(h.requests.lock().unwrap().len(), attempts, "at {elapsed}ms");
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
        assert_eq!(motor.hosted_state(id).unwrap().retry_iat, Some(issued));
    }
    motor.machine_jobs_health_at(deadline - 1).await;
    assert_eq!(h.requests.lock().unwrap().len(), 5);
    motor.machine_jobs_health_at(deadline).await;
    let terminal = motor.storage.job(id).unwrap().local_terminal.unwrap();
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
    assert_eq!(terminal.ran, Some(Ran::Unknown("maybe".into())));
    assert!(terminal.refusal_detail.unwrap().contains(
        "host refused reattach with auth past the job's deadline; no result reached ck-motor"
    ));
    assert_eq!(h.requests.lock().unwrap().len(), 6);
    motor
        .machine_jobs_health_at(deadline + STALE_PROBE_INTERVAL_MS)
        .await;
    assert_eq!(h.requests.lock().unwrap().len(), 6);
    assert!(h.frames.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
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
    assert_eq!(h.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn broker_auth_failures_back_off_to_fifteen_minutes_without_retiring_the_job() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-broker-auth");
    let (motor, id) = detached_job(&h, &scratch).await;
    let http = crate::hosted::test_http::Http::new(|_, _| (401, json!({"error": "auth"}))).await;
    motor
        .storage
        .hosted_update(id, |s| {
            s.broker_url = http.url.clone();
            s.retry_iat = Some(0);
        })
        .unwrap();
    let now = crate::hosted::now_s() * 1000;
    for elapsed in [0, 30_000, 90_000, 210_000, 450_000, 930_000, 1_830_000] {
        motor.machine_jobs_health_at(now + elapsed).await;
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
    }
    assert_eq!(http.requests.lock().unwrap().len(), 7);
    motor
        .machine_jobs_health_at(now + 1_830_000 + STALE_PROBE_INTERVAL_MS - 1)
        .await;
    assert_eq!(http.requests.lock().unwrap().len(), 7);
    motor
        .machine_jobs_health_at(now + 1_830_000 + STALE_PROBE_INTERVAL_MS)
        .await;
    assert_eq!(http.requests.lock().unwrap().len(), 8);
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hosted_running_health_probe_keeps_the_job_even_past_deadline() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = cortexkit_test_support::ScratchDir::new("health-hosted-running");
    let (motor, id) = detached_job(&h, &scratch).await;
    motor
        .storage
        .hosted_update(id, |s| s.retry_iat = Some(0))
        .unwrap();
    let now = crate::hosted::now_s() * 1000;
    for elapsed in [0, 15_000] {
        let health = motor.machine_jobs_health_at(now + elapsed).await;
        assert_eq!(health["linux"]["running"], 1);
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
        let state = motor.hosted_state(id).unwrap();
        assert!(state.terminal.is_none());
        assert!(!state.finish_done);
    }
    assert_eq!(h.requests.lock().unwrap().len(), 2);
    assert_eq!(
        h.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| matches!(f, Frame::JobStatus { .. }))
            .count(),
        2
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}
