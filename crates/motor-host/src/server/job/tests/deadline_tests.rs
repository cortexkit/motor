use super::*;

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
