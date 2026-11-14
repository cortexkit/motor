use super::*;

const MIB: u64 = 1024 * 1024;

fn hold_host(percent: u8) -> Host {
    let mut host = memory_host(1000);
    let motor_host::config::HostSettings::PerJob(settings) = &mut host.settings else {
        unreachable!()
    };
    settings.unused_memory_hold_percent = percent;
    settings.per_vm_overhead_mib = 10;
    host
}

fn running(host: &mut Host, claims: Claims) {
    let job = claims.jti.clone();
    host.claim(claims, AT).unwrap();
    host.begin_job_boot_at(&job, AT).unwrap().unwrap();
    host.job_runner_hello(&job).unwrap();
}

fn refresh(host: &mut Host, now: u64, available: u64) {
    host.record_host_resources(HostResources {
        at_ms: now,
        available_mem_mib: available,
        job_vm_headroom_mib: u64::MAX,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
}

#[test]
fn quarter_unused_stated_memory_reduces_admission() {
    let mut host = hold_host(25);
    assert_eq!(host.job_free_mem_mib(AT), 950);
    running(&mut host, job(JOB));
    host.record_job_resident(JOB, AT, Ok(0));
    // ceil(25% of (100 stated + 10 overhead)) = 28 MiB, beyond the host reserve.
    assert_eq!(host.job_free_mem_mib(AT), 922);
}

#[test]
fn holds_cover_zero_full_and_overstated_residency_and_percentage_extremes() {
    for (percent, resident, expected_free) in [
        (25, 0, 922),
        (25, 110, 950),
        (25, 111, 950),
        (25, 10, 925),
        (0, 0, 950),
        (0, 111, 950),
        (100, 0, 840),
        (100, 10, 850),
    ] {
        let mut host = hold_host(percent);
        running(&mut host, job(JOB));
        host.record_job_resident(JOB, AT, Ok(resident * MIB));
        assert_eq!(host.job_free_mem_mib(AT), expected_free);
    }
}

#[test]
fn mixed_vms_hold_only_their_unused_stated_memory_not_burst() {
    let mut host = hold_host(25);
    let mut first = workspace_job(1);
    first.mem_burst_mib = Some(50);
    running(&mut host, first.clone());
    let mut second = workspace_job(2);
    second.mem_mib = 200;
    running(&mut host, second.clone());
    host.record_job_resident(&first.jti, AT, Ok(10 * MIB));
    host.record_job_resident(&second.jti, AT, Ok(50 * MIB));
    assert_eq!(host.job_free_mem_mib(AT), 885); // Holds are 25 and 40 MiB.
    host.record_job_resident(&first.jti, AT, Ok(110 * MIB));
    assert_eq!(host.job_free_mem_mib(AT), 910); // Burst headroom is not held.
}

#[test]
fn missing_and_older_than_thirty_seconds_residents_fail_safe() {
    let mut host = hold_host(25);
    running(&mut host, job(JOB));
    assert_eq!(host.job_free_mem_mib(AT), 922);
    host.record_job_resident(JOB, AT, Ok(110 * MIB));
    refresh(&mut host, AT + 30_000, 1000);
    assert_eq!(host.job_free_mem_mib(AT + 30_000), 950);
    refresh(&mut host, AT + 30_001, 1000);
    assert_eq!(host.job_free_mem_mib(AT + 30_001), 922);

    let settings = host.settings.clone();
    let mut restarted = Host::new(
        BTreeMap::new(),
        host.snapshot.clone(),
        Box::<MemoryJournal>::default(),
    );
    restarted.settings = settings;
    refresh(&mut restarted, AT, 1000);
    assert_eq!(restarted.job_free_mem_mib(AT), 922);
}

#[test]
fn booting_and_ended_jobs_do_not_get_a_running_hold() {
    let mut host = hold_host(25);
    host.claim(job(JOB), AT).unwrap();
    assert!(host.resident_samples_due(AT).is_empty());
    assert_eq!(host.job_free_mem_mib(AT), 950);
    host.begin_job_boot_at(JOB, AT).unwrap().unwrap();
    assert!(host.resident_samples_due(AT).is_empty());
    assert_eq!(host.job_free_mem_mib(AT), 830);
    host.job_runner_hello(JOB).unwrap();
    host.record_job_resident(JOB, AT, Ok(0));
    assert_eq!(host.job_free_mem_mib(AT), 922);
    host.job_stopped(JOB, false).unwrap();
    assert!(host.resident_samples_due(AT).is_empty());
    assert_eq!(host.job_free_mem_mib(AT), 950);
}

#[test]
fn fill_after_hello_gets_the_same_hold_instead_of_a_boot_debit() {
    let mut host = hold_host(25);
    refresh(&mut host, AT, 100_000);
    let fill = host
        .reserve_fill(&job(JOB).sub, "repo", 24_577, AT)
        .unwrap()
        .unwrap();
    let id = &fill.fill.fill_id;
    assert!(host.resident_samples_due(AT).is_empty());
    assert_eq!(host.job_free_mem_mib(AT), 75_353);
    host.fill_runner_hello(id).unwrap();
    assert_eq!(host.resident_samples_due(AT), vec![id.clone()]);
    assert_eq!(host.job_free_mem_mib(AT), 93_803);
    host.record_job_resident(id, AT, Ok(24_587 * MIB));
    assert_eq!(host.job_free_mem_mib(AT), 99_950);
}

#[test]
fn attempts_are_paced_and_failures_preserve_samples_and_warn_only_once() {
    let mut host = hold_host(25);
    running(&mut host, job(JOB));
    assert_eq!(host.resident_samples_due(AT), vec![JOB]);
    host.record_job_resident(JOB, AT, Ok(10 * MIB));
    assert!(host.resident_samples_due(AT + 4999).is_empty());
    let logs = crate::test_logs::capture(|| {
        for now in [AT + 5000, AT + 10_000] {
            assert_eq!(host.resident_samples_due(now), vec![JOB]);
            host.record_job_resident(JOB, now, Err(anyhow::anyhow!("sample failed")));
            refresh(&mut host, now, 1000);
            assert_eq!(host.job_free_mem_mib(now), 925);
            assert!(host.resident_samples_due(now + 1).is_empty());
        }
    });
    assert_eq!(
        logs.iter()
            .filter(|line| line.contains("job_resident_memory_unavailable"))
            .count(),
        1
    );
    refresh(&mut host, AT + 30_001, 1000);
    assert_eq!(host.job_free_mem_mib(AT + 30_001), 922);
}

#[test]
fn advertised_memory_logs_hold_and_stale_sample_count() {
    let mut host = hold_host(25);
    running(&mut host, job(JOB));
    let logs = crate::test_logs::capture(|| host.log_job_memory(AT, host.job_free_mem_mib(AT)));
    let line = logs
        .iter()
        .find(|line| line.contains("job_memory_advertised"))
        .unwrap();
    assert!(line.contains("free_mem_mib=922 "));
    assert!(line.contains("held_back_mib=28 "));
    assert!(line.contains("resident_samples_stale=1 "));
    refresh(&mut host, AT + 60_000, 1000);
    host.record_job_resident(JOB, AT + 60_000, Ok(10 * MIB));
    let logs = crate::test_logs::capture(|| {
        host.log_job_memory(AT + 60_000, host.job_free_mem_mib(AT + 60_000));
    });
    let line = logs
        .iter()
        .find(|line| line.contains("job_memory_advertised"))
        .unwrap();
    assert!(line.contains("held_back_mib=25 "));
    assert!(line.contains("resident_samples_stale=0 "));
}

#[test]
fn admission_saturates_at_zero_even_with_maximum_memory_inputs() {
    let mut host = hold_host(100);
    running(&mut host, job(JOB));
    host.snapshot
        .per_job
        .executions
        .get_mut(JOB)
        .unwrap()
        .claims
        .mem_mib = u64::MAX;
    if let motor_host::config::HostSettings::PerJob(settings) = &mut host.settings {
        settings.per_vm_overhead_mib = u64::MAX;
    }
    refresh(&mut host, AT, u64::MAX);
    host.record_job_resident(JOB, AT, Ok(u64::MAX));
    assert_eq!(host.job_free_mem_mib(AT), 0);
    if let motor_host::config::HostSettings::PerJob(settings) = &mut host.settings {
        settings.unused_memory_hold_percent = 0;
    }
    assert_eq!(host.job_free_mem_mib(AT), u64::MAX - 50);
    refresh(&mut host, AT, 0);
    assert_eq!(host.job_free_mem_mib(AT), 0);
}
