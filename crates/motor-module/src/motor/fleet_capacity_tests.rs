//! Fleet-routed jobs waiting for hosted capacity, and the sizes each route
//! serves. The broker double selects the host only from the tenant's last
//! heartbeat (see `hosted::tests::capacity`), so these tests decide exactly
//! when the broker's view goes stale.

use super::repository_routing_tests::{
    lease_bodies, lease_settlement_requests, request, terminal_without_more_time, until, HeldClock,
};
use super::*;
use crate::hosted::{
    boat_tests::{committed_repo, records},
    tests::{
        capacity::{HostAnswer, LeaseAnswer, TenantCapacity},
        harness::{Behavior, Harness, NoSsh},
    },
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn config(h: &Harness) -> Config {
    Config { ..h.config() }
}

/// Tenant 1's advertisement while stopped, in MiB: its 78 GiB of guest RAM
/// minus capacity.json's 2 GiB `tenant_memory_reserve_bytes`.
const TENANT_1_STOPPED_FREE_MEM_MIB: u64 = ((78u64 << 30) >> 20) - (2_147_483_648 >> 20);

fn tenant(h: &Harness) -> std::sync::MutexGuard<'_, Option<TenantCapacity>> {
    h.capacity.lock().unwrap()
}

fn with_tenant<T>(h: &Harness, f: impl FnOnce(&mut TenantCapacity) -> T) -> T {
    f(tenant(h).as_mut().unwrap())
}

/// Waits until the host has judged `tokens` run tokens and refused the last
/// as full: ck-motor gives that placement attempt back just before its
/// two-second wait. Then waits for the wall clock to pass every token's issue
/// second, since the next request otherwise waits for a later issue time in
/// wall-clock seconds, which the held clock never reaches.
async fn until_host_full_wait(h: &Harness, motor: &Motor, id: Uuid, tokens: usize) {
    until("the host-full wait", || {
        h.tokens.load(Ordering::SeqCst) == tokens
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

/// Moves the held clock to one millisecond before `ms` more, shows that no
/// new lease request was sent in that time, then moves the last millisecond.
async fn no_request_until(h: &Harness, ms: u64) {
    let before = h.lease_bodies.lock().unwrap().len();
    tokio::time::advance(Duration::from_millis(ms - 1)).await;
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(
        h.lease_bodies.lock().unwrap().len(),
        before,
        "asked again before the wait ended"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
}

/// Waits until ck-motor has parsed one broker no_capacity reply and fixed
/// when it asks again.
async fn until_retry_wait(waits: &crate::logging::Captured) {
    until("the broker retry wait", || {
        waits.text().matches("hosted broker retry wait").count() == 1
    })
    .await;
}

/// One fleet job meets `hosted_host_full` (from a stale heartbeat) and broker
/// 503 `no_capacity` (from a heartbeat showing the tenant full), in each
/// order. Both are the same capacity wait inside one queue budget: the host
/// refusal waits two seconds, the broker refusal waits its Retry-After of
/// three seconds and not a millisecond less, and neither uses up a placement
/// attempt. The job then runs once, and nothing is released or recorded as
/// an error.
#[tokio::test(start_paused = true)]
async fn fleet_capacity_wait_takes_host_full_and_no_capacity_in_either_order() {
    let _clock = HeldClock::new();
    for host_full_first in [true, false] {
        let waits = crate::logging::Captured::default();
        let _logs = crate::logging::scoped(waits.clone());
        let h = Harness::new(Behavior::CompleteRun).await;
        let mut model = TenantCapacity::new(64, TENANT_1_STOPPED_FREE_MEM_MIB);
        model.retry_after = "3";
        *tenant(&h) = Some(model);
        let set_full = |full: bool| {
            with_tenant(&h, |t| {
                t.max_weight = if full { 0 } else { 64 };
                t.heartbeat();
            })
        };
        // The broker grants from a heartbeat with room, and the host refuses
        // the token: the queued refusal stands in for a burst that filled it.
        h.host_refusals.lock().unwrap().push_back("host_full");
        set_full(!host_full_first);
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor =
            Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
        let started = tokio::time::Instant::now();
        let stream = motor.run(request(&repo)).unwrap();
        let id = stream.job_id;
        let job = tokio::spawn(records(stream));
        if host_full_first {
            until_host_full_wait(&h, &motor, id, 1).await;
            set_full(true);
            no_request_until(&h, 2000).await;
            until_retry_wait(&waits).await;
            set_full(false);
            no_request_until(&h, 3000).await;
        } else {
            until_retry_wait(&waits).await;
            set_full(false);
            no_request_until(&h, 3000).await;
            until_host_full_wait(&h, &motor, id, 1).await;
            no_request_until(&h, 2000).await;
        }
        let terminal = terminal_without_more_time(job).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        let answers: Vec<_> = tenant(&h)
            .as_ref()
            .unwrap()
            .leases
            .iter()
            .map(|(job, answer, _)| {
                assert_eq!(job, &id.to_string());
                answer.clone()
            })
            .collect();
        let expected = if host_full_first {
            [
                LeaseAnswer::Selected,
                LeaseAnswer::NoCapacity,
                LeaseAnswer::Selected,
            ]
        } else {
            [
                LeaseAnswer::NoCapacity,
                LeaseAnswer::Selected,
                LeaseAnswer::Selected,
            ]
        };
        assert_eq!(answers, expected, "host_full_first={host_full_first}");
        let state = motor.hosted_state(id).unwrap();
        assert!(state.fleet_routed && state.run_sent);
        assert_eq!(state.placement_attempts, 1);
        assert_eq!(state.hosted_code, None);
        assert_eq!(h.tokens.load(Ordering::SeqCst), 2);
        assert_eq!(h.runs.load(Ordering::SeqCst), 1);
        assert!(lease_settlement_requests(&h).is_empty());
        assert!(!waits.text().contains("hosted job stopped"));
        assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    }
}

/// Eight 2-vCPU fleet jobs arrive within one heartbeat interval at a tenant
/// with 8 permits. The broker grants all eight from the last heartbeat; the
/// host admits four and refuses the other four `hosted_host_full` before any
/// Run. The next heartbeat shows the tenant full, so their next requests get
/// 503 `no_capacity`, each honouring its one-second Retry-After. When the
/// first four end and a heartbeat shows room again, the other four are
/// admitted. Every job runs exactly once; none is released, finished as
/// lost, recorded with an error code, or reported as a stopped hosted job.
#[tokio::test]
async fn fleet_burst_over_one_heartbeat_waits_through_host_full_and_no_capacity() {
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    let mut model = TenantCapacity::new(8, TENANT_1_STOPPED_FREE_MEM_MIB);
    model.heartbeat();
    *tenant(&h) = Some(model);
    h.run_gate.send_replace(false);
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    let mut ids = Vec::new();
    let mut jobs = Vec::new();
    for _ in 0..8 {
        let mut request = request(&repo);
        request.weight_hint = Some(2);
        let stream = motor.run(request).unwrap();
        ids.push(stream.job_id);
        jobs.push(tokio::spawn(records(stream)));
    }
    until("four admitted and four refused", || {
        let tenant = tenant(&h);
        let tenant = tenant.as_ref().unwrap();
        tenant.running() == 4 && tenant.count(&HostAnswer::HostFull) == 4
    })
    .await;
    let surplus: Vec<String> = with_tenant(&h, |t| {
        t.heartbeat();
        t.admissions
            .iter()
            .filter(|(_, answer)| *answer == HostAnswer::HostFull)
            .map(|(job, _)| job.clone())
            .collect()
    });
    until("no_capacity for every refused job", || {
        let tenant = tenant(&h);
        surplus.iter().all(|job| {
            tenant
                .as_ref()
                .unwrap()
                .leases
                .iter()
                .any(|(j, answer, _)| j == job && *answer == LeaseAnswer::NoCapacity)
        })
    })
    .await;
    assert_eq!(h.runs.load(Ordering::SeqCst), 4, "refused jobs never ran");
    assert!(jobs.iter().all(|job| !job.is_finished()));
    h.run_gate.send_replace(true);
    until("the admitted jobs ended", || {
        tenant(&h).as_ref().unwrap().running() == 0
    })
    .await;
    with_tenant(&h, TenantCapacity::heartbeat);
    for job in jobs {
        let (_, terminal) = job.await.unwrap();
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    }

    let tenant = tenant(&h);
    let tenant = tenant.as_ref().unwrap();
    assert!(tenant.count(&HostAnswer::HostFull) >= 4);
    assert_eq!(tenant.count(&HostAnswer::Admitted), 8);
    for job in &surplus {
        let answers: Vec<_> = tenant
            .admissions
            .iter()
            .filter(|(j, _)| j == job)
            .map(|(_, answer)| answer.clone())
            .collect();
        assert_eq!(answers.first(), Some(&HostAnswer::HostFull));
        assert_eq!(answers.last(), Some(&HostAnswer::Admitted));
    }
    for (index, (job, answer, at)) in tenant.leases.iter().enumerate() {
        if *answer == LeaseAnswer::NoCapacity {
            let next = tenant.leases[index + 1..]
                .iter()
                .find(|(j, _, _)| j == job)
                .expect("a job refused no_capacity asked again");
            assert!(
                next.2.duration_since(*at) >= Duration::from_secs(1),
                "Retry-After was not honoured"
            );
        }
    }
    let frames = h.frames.lock().unwrap();
    for id in &ids {
        let runs = frames
            .iter()
            .filter(|frame| matches!(frame, Frame::Run(run) if run.job_id == *id))
            .count();
        assert_eq!(runs, 1, "{id} ran once");
        let state = motor.hosted_state(*id).unwrap();
        assert!(state.fleet_routed && state.run_sent);
        assert_eq!(state.placement_attempts, 1);
        assert_eq!(state.hosted_code, None);
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 8);
    let settlement_requests = lease_settlement_requests(&h);
    assert!(
        settlement_requests.is_empty(),
        "unexpected broker requests: {settlement_requests:?}"
    );
    assert_eq!(
        motor.hosted_health().unwrap()["last_lease_error"],
        serde_json::Value::Null
    );
    assert!(!logs.text().contains("hosted job stopped"));
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

/// With no capacity anywhere, a fleet job keeps waiting past three
/// placements: the host refuses four stale grants as full, then a heartbeat
/// shows the tenant full and the broker answers 503 `no_capacity`. The job
/// ends exactly when its ten-second queue budget does, with the typed,
/// retryable refusal (runner full, retry after the broker's one second), no
/// Run anywhere and only the unused reservation released.
#[tokio::test(start_paused = true)]
async fn fleet_without_capacity_refuses_at_the_queue_budget_not_after_three_placements() {
    let _clock = HeldClock::new();
    let waits = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(waits.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    let mut model = TenantCapacity::new(8, TENANT_1_STOPPED_FREE_MEM_MIB);
    model.heartbeat();
    // Other callers' jobs take every permit after that heartbeat.
    model.max_weight = 0;
    *tenant(&h) = Some(model);
    let (scratch, repo) = committed_repo();
    let config = config(&h);
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    let started = tokio::time::Instant::now();
    let mut run_request = request(&repo);
    run_request.queue_wait_limit_s = Some(10);
    let stream = motor.run(run_request).unwrap();
    let id = stream.job_id;
    let job = tokio::spawn(records(stream));
    for tokens in 1..=4 {
        until_host_full_wait(&h, &motor, id, tokens).await;
        if tokens == 4 {
            with_tenant(&h, TenantCapacity::heartbeat);
        }
        tokio::time::advance(Duration::from_secs(2)).await;
    }
    for waits_seen in 1..=2 {
        until("the broker retry wait", || {
            waits.text().matches("hosted broker retry wait").count() == waits_seen
        })
        .await;
        if waits_seen == 1 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
    }
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)))
        .await
        .unwrap();
    assert!(!job.is_finished(), "refused before the queue budget ended");
    tokio::time::advance(Duration::from_millis(1)).await;
    let terminal = terminal_without_more_time(job).await;
    assert_eq!(started.elapsed(), Duration::from_secs(10));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms, Some(1000));
    let detail = terminal.refusal_detail.unwrap();
    assert!(detail.starts_with("hosted_capacity"), "{detail}");
    let answers: Vec<_> = tenant(&h)
        .as_ref()
        .unwrap()
        .leases
        .iter()
        .map(|(_, answer, _)| answer.clone())
        .collect();
    assert_eq!(
        answers,
        [
            LeaseAnswer::Selected,
            LeaseAnswer::Selected,
            LeaseAnswer::Selected,
            LeaseAnswer::Selected,
            LeaseAnswer::NoCapacity,
            LeaseAnswer::NoCapacity,
        ]
    );
    assert_eq!(h.tokens.load(Ordering::SeqCst), 4, "four placements");
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    assert_eq!(
        lease_settlement_requests(&h),
        vec![format!("/v1/leases/{id}/release")]
    );
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

/// Requests that cannot fit are refused by the broker, never tried on another Linux endpoint.
#[tokio::test]
async fn invalid_fleet_timeouts_and_no_host_fits_are_typed_without_a_direct_probe() {
    for (claim, timeout, stub_without_max, leases) in [
        (22, 600, false, 1),
        (22, 600, true, 1),
        (17, 21601, false, 0),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        h.broker.lock().unwrap().knobs.max_vcpus = Some(21);
        if stub_without_max {
            h.lease_replies.lock().unwrap().push_back((
                422,
                serde_json::json!({"error":"no_host_fits"}),
                "",
            ));
        }
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor =
            Motor::with_connector(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
        let mut request = request(&repo);
        request.weight_hint = Some(claim);
        request.timeout = Some(timeout);
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected
            }
        );
        assert_eq!(terminal.ran, Some(Ran::None));
        let detail = terminal.refusal_detail.unwrap();
        if leases == 1 {
            assert_eq!(detail, "hosted_no_host_fits");
        } else {
            assert!(detail.contains("timeout_s"), "{detail}");
            assert!(detail.contains("10..=21600"), "{detail}");
        }
        assert!(motor
            .storage
            .job(id)
            .unwrap()
            .routed_direct_reason
            .is_none());
        assert!(motor.hosted_state(id).is_some());
        assert_eq!(peer.0.load(Ordering::SeqCst), 0);
        let bodies = lease_bodies(&h);
        assert_eq!(bodies.len(), leases);
        if leases == 1 {
            assert_eq!(bodies[0]["vcpus"], 22);
        }
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            lease_settlement_requests(&h),
            if leases == 1 {
                vec![format!("/v1/leases/{id}/release")]
            } else {
                Vec::new()
            }
        );
    }
}

/// The runbook's 21-vCPU canary against the 78 GiB guest. Stopped, tenant 1
/// advertises 79,872 - 2,048 = 77,824 MiB, and the broker's per-job rule
/// needs `memory_max_mib(21)` = 75,264 MiB, so 21 is the largest placeable
/// size. The canary, sent through a temporary `hosted_repositories` entry with
/// `allow_spill: false`, is placed on the tenant cold, and again warm at an
/// advertisement of exactly 75,264 MiB. At one MiB less the broker answers
/// `no_capacity`, and the canary fails without any Run when its queue budget
/// ends: the warm canary's failure outcome.
#[tokio::test(start_paused = true)]
async fn twenty_one_vcpu_canary_qualifies_against_the_78_gib_guest_advertisement() {
    let _clock = HeldClock::new();
    let waits = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(waits.clone());
    assert_eq!(TENANT_1_STOPPED_FREE_MEM_MIB, 77_824);
    assert_eq!(
        (1..=64u32)
            .filter(|n| motor_lease::memory_max_mib(*n) <= TENANT_1_STOPPED_FREE_MEM_MIB)
            .max(),
        Some(21)
    );
    let h = Harness::new(Behavior::CompleteRun).await;
    // The broker's per-host vCPU ceiling for the build server's tenant host.
    h.broker.lock().unwrap().knobs.max_vcpus = Some(21);
    let mut model = TenantCapacity::new(64, TENANT_1_STOPPED_FREE_MEM_MIB);
    model.heartbeat();
    assert!(model.qualifies(21) && !model.qualifies(22));
    *tenant(&h) = Some(model);
    let (scratch, repo) = committed_repo();
    let mut config = config(&h);
    config.hosted_repositories = vec![repo.to_str().unwrap().into()];
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    for (free_mem_mib, placed) in [
        (TENANT_1_STOPPED_FREE_MEM_MIB, true),
        (75_264, true),
        (75_263, false),
    ] {
        with_tenant(&h, |t| {
            t.free_mem_mib = free_mem_mib;
            t.heartbeat();
        });
        let mut canary = request(&repo);
        canary.weight_hint = Some(21);
        canary.timeout = Some(600);
        canary.queue_wait_limit_s = Some(1);
        let stream = motor.run(canary).unwrap();
        let id = stream.job_id;
        let job = tokio::spawn(records(stream));
        if !placed {
            // Spend the budget only after the broker has actually answered.
            // A busy host must not expire this canary before its lease request.
            until_retry_wait(&waits).await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let terminal = terminal_without_more_time(job).await;
        let state = motor.hosted_state(id).unwrap();
        assert!(state.repository_routed);
        assert!(motor
            .storage
            .job(id)
            .unwrap()
            .routed_direct_reason
            .is_none());
        let body = lease_bodies(&h).pop().unwrap();
        assert_eq!(body["job_id"], id.to_string());
        assert_eq!(body["vcpus"], 21);
        assert_eq!(body["allow_spill"], false);
        let tenant = tenant(&h);
        let tenant = tenant.as_ref().unwrap();
        let answers: Vec<_> = tenant
            .leases
            .iter()
            .filter(|(job, _, _)| *job == id.to_string())
            .map(|(_, answer, _)| answer.clone())
            .collect();
        if placed {
            assert_eq!(
                terminal.outcome,
                Outcome::Exit { code: 23 },
                "{free_mem_mib}"
            );
            assert!(state.run_sent);
            assert_eq!(answers, [LeaseAnswer::Selected]);
            assert_eq!(
                tenant.admissions.last(),
                Some(&(id.to_string(), HostAnswer::Admitted))
            );
        } else {
            assert_eq!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::RunnerFull
                }
            );
            assert!(!state.run_sent);
            assert!(answers.iter().all(|a| *a == LeaseAnswer::NoCapacity));
        }
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 2);
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}
