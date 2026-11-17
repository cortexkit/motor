//! Fleet-routed jobs waiting for hosted capacity, and the sizes each route
//! serves. The broker double selects the host only from the tenant's last
//! heartbeat (see `hosted::tests::capacity`), so these tests decide exactly
//! when the broker's view goes stale.
use crate::hosted_salt::tests::motor as test_motor;

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

/// Checks that placement waits send no lease requests before their deadline.
/// Old-booking releases run independently and may proceed during the wait.
async fn no_request_until(h: &Harness, ms: u64) {
    let placement_count = || {
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.as_str() == "/v1/leases" || path.ends_with("/replace"))
            .count()
    };
    let before = placement_count();
    tokio::time::advance(Duration::from_millis(ms - 1)).await;
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(
        placement_count(),
        before,
        "asked again before the wait ended"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
}

/// Waits until ck-motor has parsed `count` broker no_capacity replies and fixed
/// when it asks again.
async fn until_retry_wait(waits: &crate::logging::Captured, count: usize) {
    until("the broker retry wait", || {
        waits.text().matches("hosted broker retry wait").count() == count
    })
    .await;
}

/// Broker capacity waits before or after a host refusal share one queue budget.
/// The host refusal adds a two-second re-book wait, while fresh-lease broker
/// refusals still honor their exact two- and three-second Retry-After values.
#[tokio::test(start_paused = true)]
async fn fleet_capacity_wait_takes_host_full_and_no_capacity_in_either_order() {
    let _clock = HeldClock::new();
    for host_full_first in [true, false] {
        let waits = crate::logging::Captured::default();
        let _logs = crate::logging::scoped(waits.clone());
        let h = Harness::new(Behavior::CompleteRun).await;
        let next = Harness::with_host(Behavior::CompleteRun, "fresh-host").await;
        let mut model = TenantCapacity::new(64, TENANT_1_STOPPED_FREE_MEM_MIB);
        model.retry_after = "3";
        *tenant(&h) = Some(model);
        let set_full = |full: bool| {
            with_tenant(&h, |t| {
                t.max_weight = if full { 0 } else { 64 };
                t.heartbeat();
            })
        };
        h.host_refusals.lock().unwrap().push_back("host_full");
        h.lease_hosts
            .lock()
            .unwrap()
            .extend([h.broker.clone(), next.broker.clone()]);
        set_full(!host_full_first);
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = test_motor(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
        let started = tokio::time::Instant::now();
        let stream = motor.run(request(&repo)).unwrap();
        let id = stream.job_id;
        let job = tokio::spawn(records(stream));
        if !host_full_first {
            until_retry_wait(&waits, 1).await;
            set_full(false);
            no_request_until(&h, 3000).await;
        }
        until("persisted capacity re-book", || {
            motor.hosted_state(id).unwrap().runner_id(id) != id
        })
        .await;
        let fresh = motor.hosted_state(id).unwrap().runner_id(id);
        h.lease_replies.lock().unwrap().push_back((
            503,
            serde_json::json!({"error":"no_capacity"}),
            "2",
        ));
        if host_full_first {
            h.lease_replies.lock().unwrap().push_back((
                503,
                serde_json::json!({"error":"no_capacity"}),
                "3",
            ));
        }
        no_request_until(&h, 2000).await;
        until_retry_wait(&waits, if host_full_first { 1 } else { 2 }).await;
        no_request_until(&h, 2000).await;
        if host_full_first {
            until_retry_wait(&waits, 2).await;
            no_request_until(&h, 3000).await;
        }
        let terminal = terminal_without_more_time(job).await;
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert_eq!(terminal.job_id, id);
        assert_eq!(started.elapsed(), Duration::from_secs(7));
        let answers: Vec<_> = tenant(&h)
            .as_ref()
            .unwrap()
            .leases
            .iter()
            .map(|(job, answer, _)| (job.clone(), answer.clone()))
            .collect();
        let expected = if host_full_first {
            vec![
                (id.to_string(), LeaseAnswer::Selected),
                (fresh.to_string(), LeaseAnswer::Selected),
            ]
        } else {
            vec![
                (id.to_string(), LeaseAnswer::NoCapacity),
                (id.to_string(), LeaseAnswer::Selected),
                (fresh.to_string(), LeaseAnswer::Selected),
            ]
        };
        assert_eq!(answers, expected, "host_full_first={host_full_first}");
        let answers = h.lease_answers.lock().unwrap().clone();
        assert_eq!(
            answers
                .iter()
                .map(|(_, status, _)| *status)
                .collect::<Vec<_>>(),
            if host_full_first {
                vec![201, 503, 503, 201]
            } else {
                vec![503, 201, 503, 201]
            }
        );
        let bodies = lease_bodies(&h);
        assert_eq!(bodies.len(), 4);
        let mut shape = bodies[0].clone();
        for body in &bodies[1..] {
            shape["job_id"] = body["job_id"].clone();
            assert_eq!(&shape, body);
        }
        let state = motor.hosted_state(id).unwrap();
        assert!(state.fleet_routed && state.run_sent);
        assert_eq!(state.placement_attempts, 1);
        assert_eq!(state.hosted_code, None);
        assert_eq!(state.placement.unwrap().host, "fresh-host");
        assert_eq!(h.tokens.load(Ordering::SeqCst), 1);
        assert_eq!(next.tokens.load(Ordering::SeqCst), 1);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(next.runs.load(Ordering::SeqCst), 1);
        motor.wait_hosted_cleanup(id).await;
        assert_eq!(
            lease_settlement_requests(&h),
            vec![format!("/v1/leases/{id}/release")]
        );
        assert!(!h
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with("/replace")));
        assert!(!waits.text().contains("hosted job stopped"));
        assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    }
}

/// A stale heartbeat grants eight jobs to a host that can admit only four.
/// The four refused callers re-book, wait for fresh-lease capacity, and run on
/// the second host. Each caller runs once, and each refused booking releases once.
#[tokio::test]
async fn fleet_burst_over_one_heartbeat_waits_through_host_full_and_no_capacity() {
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    let next = Harness::with_host(Behavior::CompleteRun, "fresh-host").await;
    let mut model = TenantCapacity::new(8, TENANT_1_STOPPED_FREE_MEM_MIB);
    model.heartbeat();
    *tenant(&h) = Some(model);
    h.run_gate.send_replace(false);
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = test_motor(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
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
    let surplus: Vec<Uuid> = with_tenant(&h, |t| {
        t.heartbeat();
        t.admissions
            .iter()
            .filter(|(_, answer)| *answer == HostAnswer::HostFull)
            .map(|(job, _)| Uuid::parse_str(job).unwrap())
            .collect()
    });
    until("fresh lease no_capacity for every refused caller", || {
        let answers = h.lease_answers.lock().unwrap();
        surplus.iter().all(|id| {
            let fresh = motor.hosted_state(*id).unwrap().runner_id(*id);
            fresh != *id
                && answers
                    .iter()
                    .any(|(job, status, _)| *job == fresh.to_string() && *status == 503)
        })
    })
    .await;
    until("the four admitted jobs sent Run", || {
        h.runs.load(Ordering::SeqCst) == 4
    })
    .await;
    assert_eq!(h.runs.load(Ordering::SeqCst), 4, "refused jobs never ran");
    assert!(jobs.iter().all(|job| !job.is_finished()));
    h.run_gate.send_replace(true);
    until("the admitted jobs ended", || {
        tenant(&h).as_ref().unwrap().running() == 0
    })
    .await;
    let mut fresh_capacity = TenantCapacity::new(8, TENANT_1_STOPPED_FREE_MEM_MIB);
    fresh_capacity.heartbeat();
    *tenant(&next) = Some(fresh_capacity);
    h.lease_hosts
        .lock()
        .unwrap()
        .extend(std::iter::repeat_n(next.broker.clone(), surplus.len()));
    with_tenant(&h, TenantCapacity::heartbeat);
    for job in jobs {
        let (_, terminal) = job.await.unwrap();
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    }
    for id in &ids {
        motor.wait_hosted_cleanup(*id).await;
    }
    let tenant = tenant(&h);
    let tenant = tenant.as_ref().unwrap();
    let fresh_tenant = self::tenant(&next);
    let fresh_tenant = fresh_tenant.as_ref().unwrap();
    assert_eq!(tenant.count(&HostAnswer::HostFull), 4);
    assert_eq!(tenant.count(&HostAnswer::Admitted), 4);
    assert_eq!(fresh_tenant.count(&HostAnswer::Admitted), 4);
    for id in &surplus {
        let fresh = motor.hosted_state(*id).unwrap().runner_id(*id);
        assert_ne!(fresh, *id);
        let answers: Vec<_> = tenant
            .admissions
            .iter()
            .chain(fresh_tenant.admissions.iter())
            .filter(|(job, _)| *job == id.to_string() || *job == fresh.to_string())
            .map(|(_, answer)| answer.clone())
            .collect();
        assert_eq!(answers, [HostAnswer::HostFull, HostAnswer::Admitted]);
    }
    let answers = h.lease_answers.lock().unwrap();
    for (index, (job, status, at)) in answers.iter().enumerate() {
        if *status == 503 {
            let next_answer = answers[index + 1..]
                .iter()
                .find(|(next_job, _, _)| next_job == job)
                .expect("a job refused no_capacity asked again");
            assert!(
                next_answer.2.duration_since(*at) >= Duration::from_secs(1),
                "Retry-After was not honored"
            );
        }
    }
    let frames = h.frames.lock().unwrap();
    let fresh_frames = next.frames.lock().unwrap();
    let bodies = lease_bodies(&h);
    for id in &ids {
        let state = motor.hosted_state(*id).unwrap();
        let fresh = state.runner_id(*id);
        let runs = frames.iter().chain(fresh_frames.iter()).filter(|frame| matches!(frame, Frame::Run(run) if run.job_id == *id || run.job_id == fresh)).count();
        assert_eq!(runs, 1, "{id} ran once across all its bookings");
        assert!(state.fleet_routed && state.run_sent);
        assert_eq!(state.placement_attempts, 1);
        assert_eq!(state.hosted_code, None);
        assert_eq!(
            state.placement.unwrap().host,
            if surplus.contains(id) {
                "fresh-host"
            } else {
                "test-host"
            }
        );
        assert_eq!(
            bodies
                .iter()
                .filter(|body| body["job_id"] == id.to_string())
                .count(),
            1,
            "each caller's original booking was sent once"
        );
        if surplus.contains(id) {
            assert!(
                !frames
                    .iter()
                    .any(|frame| matches!(frame, Frame::Run(run) if run.job_id == *id)),
                "the refused booking never received Run"
            );
            assert!(
                bodies
                    .iter()
                    .filter(|body| body["job_id"] == fresh.to_string())
                    .count()
                    >= 2,
                "the fresh booking saw both no_capacity and its eventual grant"
            );
        }
    }
    assert_eq!(bodies.len(), answers.len());
    assert_eq!(
        bodies
            .iter()
            .map(|body| body["job_id"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        12
    );
    assert_eq!(h.tokens.load(Ordering::SeqCst), 8);
    assert_eq!(next.tokens.load(Ordering::SeqCst), 4);
    assert_eq!(h.runs.load(Ordering::SeqCst), 4);
    assert_eq!(next.runs.load(Ordering::SeqCst), 4);
    let mut settlements = lease_settlement_requests(&h);
    settlements.sort();
    let mut expected: Vec<_> = surplus
        .iter()
        .map(|id| format!("/v1/leases/{id}/release"))
        .collect();
    expected.sort();
    assert_eq!(settlements, expected);
    assert!(!h
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|path| path.ends_with("/replace")));
    assert!(!logs.text().contains("hosted job stopped"));
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

/// A stale grant is refused before Run, then a fresh lease waits for capacity.
/// The two-second re-book delay and eight one-second broker waits must end at
/// the original ten-second deadline, without exhausting placement attempts.
#[tokio::test(start_paused = true)]
async fn fleet_without_capacity_refuses_at_the_queue_budget_not_after_three_placements() {
    let _clock = HeldClock::new();
    let waits = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(waits.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    let mut model = TenantCapacity::new(8, TENANT_1_STOPPED_FREE_MEM_MIB);
    model.heartbeat();
    model.max_weight = 0;
    *tenant(&h) = Some(model);
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = test_motor(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
    let started = tokio::time::Instant::now();
    let mut run_request = request(&repo);
    run_request.queue_wait_limit_s = Some(10);
    let stream = motor.run(run_request).unwrap();
    let id = stream.job_id;
    let job = tokio::spawn(records(stream));
    until("persisted capacity re-book", || {
        motor.hosted_state(id).unwrap().runner_id(id) != id
    })
    .await;
    let fresh = motor.hosted_state(id).unwrap().runner_id(id);
    with_tenant(&h, TenantCapacity::heartbeat);
    no_request_until(&h, 2000).await;
    for waits_seen in 1..=8 {
        until_retry_wait(&waits, waits_seen).await;
        assert_eq!(motor.hosted_state(id).unwrap().placement_attempts, 1);
        if waits_seen < 8 {
            no_request_until(&h, 1000).await;
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
        .map(|(job, answer, _)| (job.clone(), answer.clone()))
        .collect();
    let expected = std::iter::once((id.to_string(), LeaseAnswer::Selected))
        .chain(std::iter::repeat_n(
            (fresh.to_string(), LeaseAnswer::NoCapacity),
            8,
        ))
        .collect::<Vec<_>>();
    assert_eq!(answers, expected);
    let bodies = lease_bodies(&h);
    assert_eq!(bodies.len(), 9);
    assert!(bodies[1..].iter().all(|body| body == &bodies[1]));
    assert_ne!(bodies[0]["job_id"], bodies[1]["job_id"]);
    assert_eq!(
        h.tokens.load(Ordering::SeqCst),
        1,
        "only the refused booking received a token"
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(!motor.hosted_state(id).unwrap().run_sent);
    until("queue-expiry booking settlement", || {
        motor.hosted_state(id).unwrap().finish_done
    })
    .await;
    let settlements = lease_settlement_requests(&h);
    assert_eq!(
        settlements
            .iter()
            .filter(|path| **path == format!("/v1/leases/{id}/release"))
            .count(),
        1
    );
    assert!(
        settlements
            .iter()
            .any(|path| *path == format!("/v1/leases/{fresh}/release")),
        "the ungranted fresh attempt must also be cleaned up"
    );
    assert!(
        settlements.iter().all(|path| path.ends_with("/release")),
        "no never-ran finish is needed for first-party bookings"
    );
    assert!(!h
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|path| path.ends_with("/replace")));
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
}

/// Requests that cannot fit are refused by the broker, never tried on another Linux endpoint.
#[tokio::test]
async fn invalid_fleet_timeouts_and_no_host_fits_are_typed_without_a_direct_probe() {
    for (claim, timeout, reply, expected, leases) in [
        (22, 600, None, "hosted_no_host_fits: the largest job a server can hold now is 21 vCPUs and 98 GiB memory", 1),
        (22, 600, Some(serde_json::json!({"error":"no_host_fits", "max_vcpus":21})), "hosted_no_host_fits: the largest job a server can hold now is 21 vCPUs", 1),
        (22, 600, Some(serde_json::json!({"error":"no_host_fits", "max_mem_mib":100353})), "hosted_no_host_fits: the largest job a server can hold now is 98 GiB memory", 1),
        (22, 600, Some(serde_json::json!({"error":"no_host_fits", "max_vcpus":"secret", "max_mem_mib":100353})), "hosted_no_host_fits: the largest job a server can hold now is 98 GiB memory", 1),
        (22, 600, Some(serde_json::json!({"error":"no_host_fits", "max_vcpus":21, "max_mem_mib":-1})), "hosted_no_host_fits: the largest job a server can hold now is 21 vCPUs", 1),
        (22, 600, Some(serde_json::json!({"error":"no_host_fits"})), "hosted_no_host_fits", 1),
        (17, 21601, None, "", 0),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        {
            let mut broker = h.broker.lock().unwrap();
            broker.knobs.max_vcpus = Some(21);
            broker.knobs.max_mem_mib = Some(100_352);
        }
        if let Some(reply) = reply {
            h.lease_replies.lock().unwrap().push_back((422, reply, ""));
        }
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor =
            test_motor(config(&h), &scratch.0.join("state"), peer.clone()).unwrap();
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
            assert_eq!(detail, expected);
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
    let motor = test_motor(config, &scratch.0.join("state"), peer.clone()).unwrap();
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
            until_retry_wait(&waits, 1).await;
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
