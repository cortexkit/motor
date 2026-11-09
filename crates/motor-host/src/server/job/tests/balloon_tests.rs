use super::*;
use crate::state::settlement::{StopCause, Winner};

const MIB: u64 = 1024 * 1024;
const THIRD: &str = "00000000-0000-7000-8000-000000000003";

fn running(runtime: &Arc<Runtime>, fake: &Fake, job: &str, at: u64, resident_mib: u64) {
    let mut c = claims(job);
    let mut hash = [5; 32];
    hash[0] = job.as_bytes()[job.len() - 1];
    c.workspace_key_hash = Some(motor_lease::base64url(&hash));
    let id = claim(runtime, &c);
    let mut host = runtime.host.lock().unwrap();
    host.begin_job_boot_at(job, at).unwrap().unwrap();
    host.job_runner_hello(job).unwrap();
    host.delivered(id).unwrap();
    host.job_started(job, at).unwrap();
    fake.resident
        .lock()
        .unwrap()
        .insert(job.into(), resident_mib * MIB);
    fake.balloon_supported.store(true, Ordering::Release);
}
fn tick(runtime: &Arc<Runtime>, now: u64, available: u64) {
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: now,
            available_mem_mib: available,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    runtime.tick_balloon(now);
}
fn balloons(fake: &Fake) -> Vec<(String, u64)> {
    fake.balloons.lock().unwrap().clone()
}
fn stopped(runtime: &Arc<Runtime>, job: &str) -> bool {
    runtime.host.lock().unwrap().snapshot.per_job.executions[job].ended
}

#[test]
fn largest_positive_overage_then_later_start_then_job_id() {
    for (residents, starts, expected) in [
        ([130, 150, 140], [100, 100, 200], JOB2),
        ([140, 140, 140], [100, 200, 100], JOB2),
        ([140, 140, 140], [100, 100, 100], THIRD),
    ] {
        let (runtime, fake, _) = fixture();
        for ((job, resident), start) in [JOB, JOB2, THIRD].into_iter().zip(residents).zip(starts) {
            running(&runtime, &fake, job, start, resident);
        }
        tick(&runtime, 1000, 49);
        assert_eq!(
            balloons(&fake),
            vec![(
                expected.into(),
                residents[if expected == JOB2 { 1 } else { 2 }] - 110
            )]
        );
        assert!(![JOB, JOB2, THIRD].iter().any(|job| stopped(&runtime, job)));
    }
}

#[test]
fn file_cache_heavy_vm_reclaims_without_a_stop_and_deflates_after_hysteresis() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 140);
    tick(&runtime, 1000, 49);
    assert_eq!(balloons(&fake), vec![(JOB.into(), 30)]);
    fake.resident.lock().unwrap().insert(JOB.into(), 110 * MIB);
    tick(&runtime, 1050, 49);
    tick(&runtime, 1100, 49);
    assert!(!stopped(&runtime, JOB));
    tick(&runtime, 1200, 60);
    assert_eq!(balloons(&fake), vec![(JOB.into(), 30)]);
    tick(&runtime, 1300, 61);
    assert_eq!(balloons(&fake), vec![(JOB.into(), 30), (JOB.into(), 0)]);
    assert!(!stopped(&runtime, JOB));
}

#[test]
fn working_set_heavy_vm_stops_after_timeout_with_durable_host_pressure() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 190);
    running(&runtime, &fake, JOB2, 200, 150);
    tick(&runtime, 1000, 49);
    // Guest RAM is 200: stated 100 plus the sizing floor 50 must survive.
    assert_eq!(balloons(&fake), vec![(JOB.into(), 50)]);
    tick(&runtime, 1099, 49);
    assert!(!stopped(&runtime, JOB));
    tick(&runtime, 1100, 49);
    let host = runtime.host.lock().unwrap();
    let e = host.snapshot.per_job.executions[JOB]
        .evidence
        .as_ref()
        .unwrap();
    assert_eq!(e.stop.as_ref().unwrap().cause, StopCause::HostPressure);
    assert!(matches!(e.winner, Some(Winner::HostStop)));
    assert_eq!(
        host.snapshot.billing[JOB].terminal.as_ref().unwrap()["oom_kill"]["cause"],
        "host_pressure"
    );
    assert!(
        host.snapshot.per_job.disks[&host.snapshot.per_job.executions[JOB].disk_id].quarantined
    );
    drop(host);
    assert!(stopped(&runtime, JOB));
    assert!(!stopped(&runtime, JOB2));
    tick(&runtime, 1200, 49);
    assert_eq!(balloons(&fake), vec![(JOB.into(), 50), (JOB2.into(), 40)]);
    tick(&runtime, 1300, 49);
    assert!(stopped(&runtime, JOB2));
}

#[test]
fn pressure_clearing_before_deadline_stops_neither_pending_nor_other_vm() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 150);
    running(&runtime, &fake, JOB2, 200, 130);
    tick(&runtime, 1000, 49);
    tick(&runtime, 1099, 50);
    tick(&runtime, 1500, 50);
    assert!(!stopped(&runtime, JOB));
    assert!(!stopped(&runtime, JOB2));
    assert_eq!(balloons(&fake), vec![(JOB.into(), 40)]);
}

#[test]
fn no_positive_overage_never_touches_vms_and_pressure_advertises_zero() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 110);
    running(&runtime, &fake, JOB2, 200, 1);
    tick(&runtime, 1000, 49);
    tick(&runtime, 100_000, 49);
    assert!(balloons(&fake).is_empty());
    assert!(!stopped(&runtime, JOB));
    assert!(!stopped(&runtime, JOB2));
    assert_eq!(runtime.host.lock().unwrap().job_free_mem_mib(100_000), 0);
}

#[test]
fn stale_capacity_cannot_authorize_timeout_stop() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 150);
    tick(&runtime, 1000, 49);
    runtime.tick_balloon(1200);
    assert!(!stopped(&runtime, JOB));
    tick(&runtime, 1201, 50);
    assert!(!stopped(&runtime, JOB));
}

#[test]
fn per_job_lifecycle_tick_runs_the_controller() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 140);
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: 1000,
            available_mem_mib: 49,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    runtime.tick(1000).unwrap();
    assert_eq!(balloons(&fake), vec![(JOB.into(), 30)]);
}

#[test]
fn false_runner_and_balloon_statistics_do_not_affect_targets_order_or_capacity() {
    let mut observed = Vec::new();
    for false_peak in [0, u64::MAX] {
        let (runtime, fake, _) = fixture();
        running(&runtime, &fake, JOB, 100, 130);
        running(&runtime, &fake, JOB2, 200, 140);
        runtime
            .host
            .lock()
            .unwrap()
            .record_host_resources(HostResources {
                at_ms: 900,
                available_mem_mib: 1000,
                vcpu_ceiling: 2,
                permit_budget: 4,
            });
        runtime
            .host
            .lock()
            .unwrap()
            .job_memory_sample(motor_protocol::MemorySample {
                jti: JOB.parse().unwrap(),
                mem_working_peak_mib: Some(false_peak),
                mem_over_mib_ms: Some(Some(false_peak)),
            })
            .unwrap();
        // An unsolicited balloon-statistics-shaped frame is not a control input.
        runtime.observe_job_frame(JOB, &json!({"type":"balloon_statistics", "actual_mib":false_peak, "free_memory":false_peak, "available_memory":false_peak})).unwrap();
        let capacity = runtime.host.lock().unwrap().job_free_mem_mib(900);
        tick(&runtime, 1000, 49);
        observed.push((balloons(&fake), capacity));
    }
    assert_eq!(observed[0], observed[1]);
    assert_eq!(observed[0], (vec![(JOB2.into(), 30)], 950));
}

#[test]
fn repeated_inflation_never_crosses_stated_memory_plus_the_sizing_floor() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 140);
    tick(&runtime, 1000, 49);
    tick(&runtime, 1050, 50);
    tick(&runtime, 1100, 49);
    assert_eq!(balloons(&fake), vec![(JOB.into(), 30), (JOB.into(), 50)]);
    tick(&runtime, 1200, 49);
    assert!(stopped(&runtime, JOB));
}

#[test]
fn no_burst_preserves_the_entire_stated_allocation_and_floor_before_pressure_stop() {
    let (runtime, fake, _) = fixture();
    let mut c = claims(JOB);
    c.mem_burst_mib = None;
    let id = claim(&runtime, &c);
    {
        let mut host = runtime.host.lock().unwrap();
        assert_eq!(host.snapshot.per_job.executions[JOB].guest_ram_mib, 134);
        host.begin_job_boot_at(JOB, 100).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.delivered(id).unwrap();
        host.job_started(JOB, 100).unwrap();
    }
    fake.balloon_supported.store(true, Ordering::Release);
    fake.resident.lock().unwrap().insert(JOB.into(), 130 * MIB);
    tick(&runtime, 1000, 49);
    assert!(balloons(&fake).is_empty());
    assert!(!stopped(&runtime, JOB));
    tick(&runtime, 1100, 49);
    assert!(stopped(&runtime, JOB));
}

#[test]
fn older_helper_disables_controller_without_failing_or_stopping_jobs() {
    let (runtime, fake, _) = fixture();
    running(&runtime, &fake, JOB, 100, 150);
    fake.balloon_supported.store(false, Ordering::Release);
    tick(&runtime, 1000, 49);
    fake.balloon_supported.store(true, Ordering::Release);
    tick(&runtime, 2000, 49);
    assert!(balloons(&fake).is_empty());
    assert!(!stopped(&runtime, JOB));
}
