use super::*;

#[test]
fn per_job_block_skips_rollback_tenant_binding() {
    struct ControlWithRollback(Arc<Fake>);
    impl Control for ControlWithRollback {
        fn adopt(&self, _: &str) -> Result<bool> {
            anyhow::bail!("tenant VM unavailable in per-job mode")
        }
        fn adopt_job(&self, job: &str) -> Result<bool> {
            self.0.adopt_job(job)
        }
        fn job_admin(&self, job: &str, request: &Value) -> Result<Value> {
            self.0.job_admin(job, request)
        }
        fn admin(&self, tenant: &str, request: &Value) -> Result<Value> {
            self.0
                .admin_requests
                .lock()
                .unwrap()
                .push((tenant.into(), request.clone()));
            Ok(json!({"ok":true,"blocked":request["blocked"]}))
        }
        fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
            unreachable!()
        }
        fn quota(&self, _: &str, _: u32) -> Result<()> {
            unreachable!()
        }
        fn relay(&self, _: &str, _: u32) -> Result<UnixStream> {
            unreachable!()
        }
        fn suspend(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn available_bytes(&self) -> Result<u64> {
            unreachable!()
        }
        fn host_available_mib(&self) -> Result<u64> {
            unreachable!()
        }
        fn online_cpus(&self) -> Result<u32> {
            unreachable!()
        }
    }

    let (mut runtime, fake, _storage) = fixture();
    Arc::get_mut(&mut runtime).unwrap().control = Arc::new(ControlWithRollback(fake.clone()));
    let c = claims(JOB);
    claim(&runtime, &c);
    {
        let mut host = runtime.host.lock().unwrap();
        host.tenants.insert(
            "tenant-1".into(),
            crate::state::Tenant {
                tenant: "tenant-1".into(),
                sub: c.sub.clone(),
                vcpu_ceiling: 1,
                max_weight: None,
                guest_ram_mib: 100,
                idle_suspend_s: 60,
                host_floor_bytes: 0,
            },
        );
        host.snapshot
            .per_job
            .executions
            .get_mut(JOB)
            .unwrap()
            .boot_started = true;
    }
    for blocked in [true, false] {
        let request = json!({"op":"set_remote_cache_write_block","blocked":blocked});
        assert_eq!(
            runtime.host_admin(&request).unwrap()["not_acknowledged"],
            json!([])
        );
        assert_eq!(
            fake.admin_requests.lock().unwrap().pop(),
            Some((JOB.into(), request))
        );
        assert!(fake.admin_requests.lock().unwrap().is_empty());
    }
}

#[test]
fn block_reaches_each_running_job_vm_and_reports_unknown_runner_controls() {
    let (runtime, fake, _storage) = fixture();
    for job in [JOB, JOB2] {
        let mut c = claims(job);
        c.workspace_key_hash = Some(motor_lease::base64url(if job == JOB {
            &[5; 32]
        } else {
            &[6; 32]
        }));
        claim(&runtime, &c);
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .per_job
            .executions
            .get_mut(job)
            .unwrap()
            .boot_started = true;
    }
    *fake.write_block_reply.lock().unwrap() = Some(json!({"ok":false,"error":"unknown_frame"}));
    assert_eq!(
        runtime
            .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
            .unwrap()["not_acknowledged"],
        json!([JOB, JOB2])
    );
    assert_eq!(fake.admin_requests.lock().unwrap().len(), 2);
    assert!(
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .remote_cache_write_blocked
    );
    *fake.write_block_reply.lock().unwrap() = None;
    assert_eq!(
        runtime
            .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":false}))
            .unwrap()["not_acknowledged"],
        json!([])
    );
    assert_eq!(fake.admin_requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn boot_while_blocked_resends_deny_before_job_lease() {
    let (runtime, fake, storage) = fixture();
    runtime
        .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
        .unwrap();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    existing_workspace(&runtime, JOB);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    wait_for_runner_hello(&runtime, JOB).await;
    assert_eq!(
        fake.admin_requests.lock().unwrap().as_slice(),
        [(
            JOB.into(),
            json!({"op":"set_remote_cache_write_block","blocked":true})
        )]
    );
    assert_eq!(fake.inputs.lock().unwrap().len(), 1);
    write_frame(&mut client, &run(JOB)).await.unwrap();
    next_type(&mut client, "accepted").await;
    next_type(&mut client, "output").await;
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
    drop(storage);
}

#[tokio::test]
async fn block_set_during_boot_retries_failed_resync_and_stops_unacknowledged_vm() {
    for failures in [2, 4] {
        let (runtime, fake, _storage) = fixture();
        hold_boot(&runtime, &fake, "runner");
        struct ReleaseGate(Arc<Fake>);
        impl Drop for ReleaseGate {
            fn drop(&mut self) {
                *self.0.gate.0.lock().unwrap() = true;
                self.0.gate.1.notify_all();
            }
        }
        let _release = ReleaseGate(fake.clone());
        let c = claims(JOB);
        let id = claim(&runtime, &c);
        let (mut client, task) = start(runtime.clone(), c, id).await;
        queued(&mut client, JOB).await;
        tokio::time::timeout(Duration::from_secs(2), fake.entered.notified())
            .await
            .unwrap();
        assert!(
            !fake.job_starts.lock().unwrap()[0].cache_write_blocked,
            "block must arrive after Start"
        );
        *fake.write_block_errors.lock().unwrap() = failures;
        let blocked = runtime
            .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
            .unwrap();
        assert_eq!(blocked["not_acknowledged"], json!([JOB]));
        *fake.gate.0.lock().unwrap() = true;
        fake.gate.1.notify_all();
        if failures == 2 {
            next_type(&mut client, "output").await;
            assert_eq!(
                fake.admin_requests.lock().unwrap().len(),
                3,
                "broadcast plus two resync attempts"
            );
            assert!(!fake.inputs.lock().unwrap().is_empty());
        } else {
            next_type(&mut client, "lease_refused").await;
            assert_eq!(
                fake.admin_requests.lock().unwrap().len(),
                4,
                "broadcast plus three bounded resync attempts"
            );
            assert!(
                fake.inputs.lock().unwrap().is_empty(),
                "unacknowledged VM received a lease"
            );
            assert!(fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call == &format!("stop:{JOB}")));
            assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
        }
        drop(client);
        task.await.unwrap().unwrap();
        runtime.stop_job(JOB, true, false).unwrap();
    }
}

#[tokio::test]
async fn undelivered_live_refusal_owns_reply_when_replay_publishes_before_boot_error() {
    let (runtime, fake, _storage) = fixture();
    hold_boot(&runtime, &fake, "runner");
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    tokio::time::timeout(Duration::from_secs(2), fake.entered.notified())
        .await
        .unwrap();
    let gate = Arc::new(RelayResultGate::default());
    *runtime.job_launches.lock().unwrap()[JOB]
        .relay_result_gate
        .lock()
        .unwrap() = Some(gate.clone());
    *fake.write_block_errors.lock().unwrap() = 4;
    let blocked = runtime
        .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
        .unwrap();
    assert_eq!(blocked["not_acknowledged"], json!([JOB]));
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();

    // The replay terminal is committed, but boot cannot return its error yet.
    // Wake the session to force its ended-job replay path.
    let stream = runtime.job_stream(JOB).unwrap();
    let stored = stream
        .with_store(|store| {
            assert!(store.ended());
            crate::replay::decode(&store.frame(0)?.unwrap())
        })
        .unwrap();
    assert_eq!(stored["type"], "terminal");
    assert_eq!(stored["ran"], "none");
    assert_eq!(stored["outcome"]["reason"], "internal_failure");
    assert_eq!(
        stored["refusal_detail"],
        "execution ended before Run delivery"
    );
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    assert!(fake.inputs.lock().unwrap().is_empty());
    write_frame(&mut client, &json!({"type":"heartbeat"}))
        .await
        .unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        next_type(&mut client, "lease_refused"),
    )
    .await
    .unwrap();
    assert_eq!(
        reply,
        json!({"type":"lease_refused","reason":"internal_failure"})
    );
    task.await.unwrap().unwrap();
    assert!(
        read_frame(&mut client).await.is_err(),
        "one live reply only"
    );
    runtime.host.lock().unwrap().close(id).unwrap();

    let mut attach = claims(JOB);
    attach.purpose = Purpose::Attach;
    let attach_id = runtime
        .host
        .lock()
        .unwrap()
        .claim(attach.clone(), unix_ms())
        .unwrap()
        .0;
    let (mut client, task) = start(runtime.clone(), attach, attach_id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    write_frame(
        &mut client,
        &json!({"type":"attach","job_id":JOB,"from_seq":0}),
    )
    .await
    .unwrap();
    assert_eq!(next_type(&mut client, "terminal").await, stored);
    task.await.unwrap().unwrap();
}
