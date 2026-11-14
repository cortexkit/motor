use super::*;

#[test]
fn sampler_refreshes_residency_without_pressure_or_the_host_lock() {
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
    runtime.sample_job_residents(now);
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

#[tokio::test]
async fn blocked_resident_sampler_does_not_delay_host_tick() {
    let (runtime, fake, _) = fixture();
    let now = unix_ms();
    claim(&runtime, &claims(JOB));
    {
        let mut host = runtime.host.lock().unwrap();
        boot_at(&mut host, JOB, now);
        host.job_runner_hello(JOB).unwrap();
    }
    *fake.resident_host.lock().unwrap() = Some(Arc::downgrade(&runtime.host));
    *fake.hang.lock().unwrap() = "job-memory";
    let sampling = runtime.clone();
    let sampler = std::thread::spawn(move || sampling.sample_job_residents(now));
    let parked = tokio::time::timeout(Duration::from_secs(5), fake.entered.notified()).await;
    let (done, completion) = std::sync::mpsc::channel();
    let ticking = runtime.clone();
    let tick = std::thread::spawn(move || {
        // Move past the sample interval so an inline sampler would enter the helper too.
        done.send(ticking.tick(now + 5000)).unwrap();
    });
    let completed = completion.recv_timeout(Duration::from_secs(5));
    // Release both threads before asserting, including when the tick timed out.
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    sampler.join().unwrap();
    tick.join().unwrap();
    parked.expect("the sampler must enter the parked helper");
    completed
        .expect("tick must complete while the resident helper is parked")
        .unwrap();
}
