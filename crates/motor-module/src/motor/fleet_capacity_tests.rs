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
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

fn config(h: &Harness) -> Config {
    Config {
        hosted_fleet_routing: true,
        ..h.config()
    }
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
    assert!(lease_settlement_requests(&h).is_empty());
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

/// Job ids and claimed vCPUs of the Run frames a build-server double received.
type RunLog = Arc<Mutex<Vec<(Uuid, Option<u32>)>>>;

/// A build server that serves Linux claims of up to 16 vCPUs and refuses a
/// larger one before start with `runner_full`, naming its limit, as it does
/// when Linux jobs are confined to a CPU list under `--max-weight 16`.
#[derive(Default)]
struct SixteenVcpuRunner {
    /// Each Run frame received: job id and claimed vCPUs.
    runs: RunLog,
    /// Replaces the refusal detail, to exercise the detail size limit.
    detail: Mutex<Option<String>>,
}
type Connection<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + 'a>,
>;
impl Connector for SixteenVcpuRunner {
    fn connect_to<'a>(
        &'a self,
        id: Uuid,
        _instance: &'a str,
        _state_root: Option<&'a str>,
    ) -> Connection<'a> {
        self.connect(id)
    }
    fn connect(&self, id: Uuid) -> Connection<'_> {
        let detail = self.detail.lock().unwrap().clone();
        let runs = self.runs.clone();
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: true,
                        platforms: vec!["linux".into()],
                        return_paths: true,
                        return_chunked: false,
                        sibling_paths: false,
                    },
                )
                .await
                .unwrap();
                while let Some(frame) = read_frame_async(&mut peer).await.unwrap() {
                    match frame {
                        Frame::Heartbeat { .. } => {}
                        Frame::Have { repositories, .. } => {
                            write_frame_async(
                                &mut peer,
                                &Frame::HaveReply {
                                    repositories,
                                    git_dependencies: Some(vec![]),
                                },
                            )
                            .await
                            .unwrap();
                        }
                        Frame::Run(run) => {
                            let claim = run.weight_hint.unwrap_or(2);
                            assert!(claim > 16, "only oversized claims reach this runner");
                            runs.lock().unwrap().push((id, run.weight_hint));
                            let refusal = Refusal {
                                job_id: Some(id),
                                transfer_id: None,
                                reason: RefusalReason::RunnerFull,
                                disk_full: None,
                                retry_after_ms: None,
                                detail: Some(detail.unwrap_or_else(|| {
                                    format!("vcpu request {claim} is outside the supported range 1..=16 (Linux job claim limit)")
                                })),
                            };
                            write_frame_async(&mut peer, &Frame::Refusal(refusal))
                                .await
                                .unwrap();
                            return;
                        }
                        other => panic!("unexpected direct frame: {other:?}"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn crate::Duplex>)
        })
    }
}

/// A 22-vCPU fleet job gets the broker's 422 `no_host_fits`, is corrected to
/// the build server before any Run, and is refused there before start with
/// `runner_full`. The refusal names both limits: the build server's 16 and
/// `hosted max_vcpus 21` from the broker's answer, or `unknown` when the
/// answer named no number. A 17-vCPU job over the lease time limit goes
/// direct without a lease request, so its refusal names only the build
/// server's limit. None of them waits or is submitted twice.
#[tokio::test]
async fn oversized_fleet_jobs_are_refused_before_start_naming_both_routes_limits() {
    let detail_max = motor_protocol::REFUSAL_DETAIL_MAX_BYTES;
    let long = "é".repeat(detail_max);
    for (claim, timeout, stub_without_max, long_detail, leases, suffix) in [
        (22, 600, false, false, 1, Some("; hosted max_vcpus 21")),
        (22, 600, true, false, 1, Some("; hosted max_vcpus unknown")),
        (17, 7201, false, false, 0, None),
        (22, 600, false, true, 1, Some("; hosted max_vcpus 21")),
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
        let peer = Arc::new(SixteenVcpuRunner::default());
        if long_detail {
            *peer.detail.lock().unwrap() = Some(long.clone());
        }
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
                reason: RefusalReason::RunnerFull
            }
        );
        assert_eq!(terminal.ran, Some(Ran::None));
        let detail = terminal.refusal_detail.unwrap();
        match suffix {
            Some(suffix) => {
                assert!(detail.ends_with(suffix), "{detail}");
                assert!(detail.len() <= detail_max);
                if long_detail {
                    assert!(detail.starts_with("éé"), "{detail}");
                } else {
                    assert!(detail.contains("1..=16"), "{detail}");
                }
                let job = motor.storage.job(id).unwrap();
                assert_eq!(job.routed_direct_reason.as_deref(), Some("no_host_fits"));
            }
            None => {
                assert_eq!(
                    detail,
                    "vcpu request 17 is outside the supported range 1..=16 (Linux job claim limit)"
                );
                assert!(motor
                    .storage
                    .job(id)
                    .unwrap()
                    .routed_direct_reason
                    .is_none());
            }
        }
        assert!(motor.hosted_state(id).is_none());
        assert_eq!(*peer.runs.lock().unwrap(), vec![(id, Some(claim))]);
        let bodies = lease_bodies(&h);
        assert_eq!(bodies.len(), leases);
        if leases == 1 {
            assert_eq!(bodies[0]["vcpus"], 22);
        }
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert!(lease_settlement_requests(&h).is_empty());
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
