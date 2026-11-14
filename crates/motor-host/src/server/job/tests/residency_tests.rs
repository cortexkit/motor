use super::*;

#[test]
fn host_tick_samples_residency_without_pressure_or_the_host_lock() {
    let (runtime, fake, _) = fixture();
    let now = unix_ms();
    claim(&runtime, &claims(JOB));
    {
        let mut host = runtime.host.lock().unwrap();
        boot_at(&mut host, JOB, now);
        host.job_runner_hello(JOB).unwrap();
        if let crate::config::HostSettings::PerJob(settings) = &mut host.settings {
            settings.unused_memory_hold_percent = 25;
        }
        assert_eq!(host.job_free_mem_mib(now), 922);
    }
    *fake.resident_host.lock().unwrap() = Some(Arc::downgrade(&runtime.host));
    fake.balloon_supported.store(true, Ordering::Release);
    fake.resident
        .lock()
        .unwrap()
        .insert(JOB.into(), 10 * 1024 * 1024);
    runtime.tick(now).unwrap();
    assert_eq!(*fake.resident_reads.lock().unwrap(), vec![JOB]);
    assert_eq!(runtime.host.lock().unwrap().job_free_mem_mib(now), 925);
    runtime.sample_job_residents(now + 4999);
    assert_eq!(*fake.resident_reads.lock().unwrap(), vec![JOB]);
    fake.resident
        .lock()
        .unwrap()
        .insert(JOB.into(), 110 * 1024 * 1024);
    runtime.sample_job_residents(now + 5000);
    assert_eq!(*fake.resident_reads.lock().unwrap(), vec![JOB, JOB]);
    assert_eq!(runtime.host.lock().unwrap().job_free_mem_mib(now), 950);
}
