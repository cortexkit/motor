use super::*;

#[test]
fn disk_wait_outlives_token_expiry_before_booting() {
    let (runtime, _, _) = fixture();
    let mut owner = claims(JOB);
    owner.max_s = 3600;
    let id = claim(&runtime, &owner);
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot(JOB).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.delivered(id).unwrap();
        host.job_started(JOB, unix_ms()).unwrap();
    }
    let waiting = claims(JOB2);
    claim(&runtime, &waiting);
    let launch = Arc::new(Launch::default());
    *launch.prestart_limit_ms.lock().unwrap() = Some(2_000);
    runtime
        .job_launches
        .lock()
        .unwrap()
        .insert(JOB2.into(), launch);

    let after_expiry = waiting.exp * 1000 + 1;
    runtime.tick_jobs(after_expiry).unwrap();
    let mut host = runtime.host.lock().unwrap();
    assert!(!host.snapshot.per_job.executions[JOB2].ended);
    assert!(!host.snapshot.per_job.executions[JOB2].boot_started);
    assert!(host
        .heartbeat(after_expiry, 0, true)
        .running
        .iter()
        .any(|job| job.job_id == JOB2));
    host.job_stopped_at(JOB, false, after_expiry).unwrap();
    host.record_host_resources(HostResources {
        at_ms: after_expiry,
        available_mem_mib: 1000,
        job_vm_headroom_mib: u64::MAX,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    let boot = host.begin_job_boot_at(JOB2, after_expiry).unwrap().unwrap();
    assert_eq!(boot.boot_started_at_ms, Some(after_expiry));
}

#[tokio::test]
async fn memory_wait_before_boot_ends_at_prestart_deadline() {
    let (runtime, fake, storage) = fixture();
    claim(&runtime, &claims(JOB2));
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB2)
        .unwrap()
        .unwrap();
    let mut c = claims(JOB);
    c.iat -= 200;
    let id = claim(&runtime, &c);
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: unix_ms(),
            available_mem_mib: 169,
            job_vm_headroom_mib: u64::MAX,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    let mut request = run(JOB);
    request["queue_wait_limit_s"] = json!(2);
    write_frame(&mut client, &request).await.unwrap();
    assert_eq!(
        next_type(&mut client, "accepted").await["queue_position"],
        1
    );
    assert_eq!(
        next_type(&mut client, "heartbeat").await["queue_wait_ms"],
        0
    );
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].boot_started);
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(runtime.job_launches.lock().unwrap()[JOB]
        .boot_ready_at_ms
        .lock()
        .unwrap()
        .is_none());
    let disk_released_at = unix_ms();
    runtime.stop_job(JOB2, false, false).unwrap();
    let ready_at = loop {
        let ready = *runtime.job_launches.lock().unwrap()[JOB]
            .boot_ready_at_ms
            .lock()
            .unwrap();
        if let Some(at) = ready {
            break at;
        }
        tokio::task::yield_now().await;
    };
    assert!(ready_at >= disk_released_at);
    assert!(ready_at <= unix_ms());
    let deadline = ready_at + 2_000 + crate::billing::BACKSTOP_CANCEL_GRACE_MS;

    runtime.tick_jobs(deadline - 1).unwrap();
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
    runtime.tick_jobs(deadline).unwrap();
    {
        let host = runtime.host.lock().unwrap();
        assert!(host.snapshot.per_job.executions[JOB].ended);
        let frames = host.host_settled_frames(JOB, 0).unwrap();
        let terminal = frames
            .iter()
            .find(|frame| frame["type"] == "terminal")
            .unwrap();
        assert_eq!(terminal["ran"], "none");
        assert_eq!(terminal["outcome"]["type"], "refused_before_start");
    }
    assert!(!storage.held.lock().unwrap().contains(JOB));
    assert!(!storage.spool(JOB).exists());
    assert!(!fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("start:")));
    let terminal = loop {
        let frame = raw(&mut client).await.unwrap();
        if frame.value["type"] != "heartbeat" {
            break frame;
        }
    };
    assert_eq!(terminal.value["type"], "terminal");
    assert_eq!(terminal.value["ran"], "none");
    assert_eq!(
        terminal.value["outcome"],
        json!({
            "type":"refused_before_start", "reason":"internal_failure"
        })
    );
    let stored = runtime
        .job_stream(JOB)
        .unwrap()
        .with_store(|store| Ok(store.cursor_frame(0, 0)?.unwrap().1))
        .unwrap();
    assert_eq!(terminal.bytes, stored);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ended_lease_during_preparation_replays_its_stored_terminal() {
    let (runtime, _, storage) = fixture();
    storage.pause_prepare.store(true, Ordering::Release);
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    storage.prepare_entered.notified().await;
    next_type(&mut client, "heartbeat").await;
    runtime.stop_job(JOB, true, false).unwrap();
    {
        let host = runtime.host.lock().unwrap();
        assert_eq!(host.snapshot.leases[JOB].state, LeaseState::Ended);
        assert!(!host.snapshot.per_job.executions[JOB].ended);
    }
    *storage.prepare_gate.0.lock().unwrap() = true;
    storage.prepare_gate.1.notify_all();
    let terminal = next_type(&mut client, "terminal").await;
    assert_eq!(terminal["ran"], "none");
    assert_eq!(terminal["outcome"]["type"], "refused_before_start");
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn prestart_deadline_uses_validated_run_queue_budget() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    let mut request = run(JOB);
    request["queue_wait_limit_s"] = json!(2);
    write_frame(&mut client, &request).await.unwrap();
    next_type(&mut client, "accepted").await;
    next_type(&mut client, "output").await;
    let boot = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .boot_started_at_ms
        .unwrap();
    let deadline = boot + 2_000 + crate::billing::BACKSTOP_CANCEL_GRACE_MS;
    runtime.tick_jobs(deadline - 1).unwrap();
    assert!(!fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
    runtime.tick_jobs(deadline).unwrap();
    assert!(fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
    loop {
        let frame = read_frame(&mut client).await.unwrap();
        if frame["type"] == "terminal" {
            assert_eq!(frame["outcome"]["type"], "outcome_unknown");
            break;
        }
        assert!(matches!(
            frame["type"].as_str(),
            Some("output" | "heartbeat")
        ));
    }
    drop(client);
    task.await.unwrap().unwrap();
}

#[test]
fn hello_without_start_is_stopped_at_prestart_bound_and_releases_resources() {
    for delivered in [false, true] {
        let (runtime, fake, storage) = fixture();
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        let boot = unix_ms();
        {
            let mut host = runtime.host.lock().unwrap();
            host.begin_job_boot_at(JOB, boot).unwrap().unwrap();
            host.job_runner_hello(JOB).unwrap();
            if delivered {
                host.delivered(id).unwrap();
            }
        }
        std::fs::write(storage.spool(JOB), b"unfinished upload").unwrap();
        let deadline = boot + c.max_s * 1000 + crate::billing::BACKSTOP_CANCEL_GRACE_MS;
        runtime.tick_jobs(deadline - 1).unwrap();
        assert!(!fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
        runtime.tick_jobs(deadline).unwrap();
        assert!(fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
        let host = runtime.host.lock().unwrap();
        assert!(host.snapshot.per_job.executions[JOB].ended);
        assert!(host
            .snapshot
            .per_job
            .disks
            .values()
            .all(|d| d.attached_job.is_none()));
        let billing = &host.snapshot.billing[JOB];
        assert!(!billing.started_live);
        let usage = billing.usage.as_ref().unwrap();
        assert_eq!(usage.started_at_ms, usage.ended_at_ms);
        let frames = host.host_settled_frames(JOB, 0).unwrap();
        let terminal = frames.iter().find(|f| f["type"] == "terminal").unwrap();
        assert_eq!(
            terminal["outcome"]["type"],
            if delivered {
                "outcome_unknown"
            } else {
                "refused_before_start"
            }
        );
        drop(host);
        assert!(!storage.spool(JOB).exists());
        assert!(!storage.held.lock().unwrap().contains(JOB));
        assert_eq!(storage.releases.lock().unwrap()[JOB], 1);
    }
}

#[test]
fn started_deadline_uses_observed_start_not_boot_or_queue_budget() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let boot = unix_ms();
    let started = boot + 20_000;
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot_at(JOB, boot).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.delivered(id).unwrap();
        host.job_started(JOB, started).unwrap();
    }
    let deadline = started + c.max_s * 1000 + crate::billing::BACKSTOP_CANCEL_GRACE_MS;
    runtime.tick_jobs(deadline - 1).unwrap();
    assert!(!fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
    runtime.tick_jobs(deadline).unwrap();
    assert!(fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
}

#[test]
fn restarted_prestart_without_boot_timestamp_falls_back_to_claim_issue_time() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot(JOB).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.snapshot
            .per_job
            .executions
            .get_mut(JOB)
            .unwrap()
            .boot_started_at_ms = None;
    }
    let deadline = (c.iat + c.max_s) * 1000 + crate::billing::BACKSTOP_CANCEL_GRACE_MS;
    runtime.tick_jobs(deadline - 1).unwrap();
    assert!(!fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
    runtime.tick_jobs(deadline).unwrap();
    assert!(fake.calls.lock().unwrap().contains(&format!("stop:{JOB}")));
}
