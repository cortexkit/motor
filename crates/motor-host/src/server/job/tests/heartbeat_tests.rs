use super::*;

#[test]
fn periodic_exit_polling_is_bounded_but_explicit_collection_remains_immediate() {
    let (runtime, fake, _) = fixture();
    let now = unix_ms();
    for offset in (0..1000).step_by(100) {
        runtime.tick_jobs(now + offset).unwrap();
    }
    assert_eq!(fake.exit_polls.load(Ordering::SeqCst), 1);
    runtime.collect_job_exits(now + 950).unwrap();
    assert_eq!(fake.exit_polls.load(Ordering::SeqCst), 2);
    runtime.tick_jobs(now + 1000).unwrap();
    assert_eq!(fake.exit_polls.load(Ordering::SeqCst), 2);
    runtime.tick_jobs(now + 1950).unwrap();
    assert_eq!(fake.exit_polls.load(Ordering::SeqCst), 3);
}

#[test]
fn trusted_memavailable_floors_kib_to_mib_and_refuses_missing_or_invalid_samples() {
    assert_eq!(
        host_available_mib("MemTotal: 99999 kB\nMemAvailable: 3071 kB\n").unwrap(),
        2
    );
    for input in [
        "",
        "MemFree: 4000 kB",
        "MemAvailable: -1 kB",
        "MemAvailable: 1024 B",
        "MemAvailable: 1024 kB extra",
        "MemAvailable: 1 kB\nMemAvailable: 2 kB",
    ] {
        assert!(host_available_mib(input).is_err(), "{input}");
    }
}

#[test]
fn required_cpu_settings_validate_against_online_cpus_without_a_64_permit_cap() {
    let (runtime, _, _) = fixture();
    let crate::config::HostSettings::PerJob(mut settings) =
        runtime.host.lock().unwrap().settings.clone()
    else {
        unreachable!()
    };
    settings.vcpu_ceiling = 24;
    settings.permit_budget = 96;
    settings.validate_cpus(24).unwrap();
    for (ceiling, permits, name) in [
        (0, 96, "vcpu_ceiling"),
        (25, 96, "vcpu_ceiling"),
        (24, 23, "permit_budget"),
        (24, 97, "permit_budget"),
    ] {
        settings.vcpu_ceiling = ceiling;
        settings.permit_budget = permits;
        assert!(settings
            .validate_cpus(24)
            .unwrap_err()
            .to_string()
            .contains(name));
    }
}

#[tokio::test]
async fn storage_probe_uses_control_host_memory_and_online_cpus() {
    let (runtime, fake, _) = fixture();
    fake.capacity_available.store(true, Ordering::SeqCst);
    runtime.host.lock().unwrap().enforce_storage();

    runtime.probe_storage();
    {
        let host = runtime.host.lock().unwrap();
        let now = unix_ms();
        let sample = host.fresh_host_resources(now).unwrap();
        assert_eq!(sample.available_mem_mib, 1000);
        assert_eq!((sample.vcpu_ceiling, sample.permit_budget), (2, 4));
        let heartbeat = host.heartbeat(now, host.job_free_mem_mib(now), true);
        assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (4, 950));
    }

    {
        let mut host = runtime.host.lock().unwrap();
        let crate::config::HostSettings::PerJob(settings) = &mut host.settings else {
            unreachable!()
        };
        settings.vcpu_ceiling = 3;
    }
    runtime.probe_storage();
    let host = runtime.host.lock().unwrap();
    let now = unix_ms();
    assert!(host.fresh_host_resources(now).is_none());
    let heartbeat = host.heartbeat(now, host.job_free_mem_mib(now), true);
    assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (0, 0));
}

#[tokio::test]
async fn failed_host_probe_discards_a_previously_fresh_sample() {
    let (runtime, _, _) = fixture();
    {
        let mut host = runtime.host.lock().unwrap();
        assert_eq!(host.job_free_mem_mib(unix_ms()), 950);
        host.enforce_storage();
    }
    // Fake has no capacity file. An earlier injected sample must not survive that failure.
    runtime.probe_storage();
    let host = runtime.host.lock().unwrap();
    let heartbeat = host.heartbeat(unix_ms(), host.job_free_mem_mib(unix_ms()), true);
    assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (0, 0));
}

async fn dax_refusal(runtime: &Arc<Runtime>, fake: &Fake, job: &str) {
    *fake.init_reply.lock().unwrap() = Some(json!({"ok":false,"error":"job_vm_dax_required"}));
    let mut c = claims(job);
    c.workspace_key_hash = Some(motor_lease::base64url(&[job.as_bytes()[job.len() - 1]; 32]));
    let id = claim(runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    next_type(&mut client, "lease_refused").await;
    drop(client);
    task.await.unwrap().unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.per_job.dax_failed);
}

#[tokio::test]
async fn dax_refusal_latches_health_across_restart_until_operator_clear_and_relatches() {
    let (runtime, fake, storage) = fixture();
    let active = claims(JOB2);
    let id = claim(&runtime, &active);
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot(JOB2).unwrap().unwrap();
        host.job_runner_hello(JOB2).unwrap();
        host.delivered(id).unwrap();
        host.job_started(JOB2, unix_ms()).unwrap();
    }
    dax_refusal(&runtime, &fake, JOB).await;
    assert!(
        fake.inputs.lock().unwrap().is_empty(),
        "DAX refusal precedes account frames"
    );
    let settings = runtime.host.lock().unwrap().settings.clone();
    drop(runtime);
    let (journal, snapshot) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
    assert!(snapshot.per_job.dax_failed);
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = settings;
    host.record_host_resources(HostResources {
        at_ms: unix_ms(),
        available_mem_mib: 1000,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    let runtime = Runtime::with_job_resources("host".into(), host, fake.clone(), storage);
    runtime
        .configure_job_resolvers(vec!["1.1.1.1".into()])
        .unwrap();
    {
        let mut host = runtime.host.lock().unwrap();
        let heartbeat = host.heartbeat(unix_ms(), 950, true);
        assert!(!heartbeat.healthy);
        assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (0, 0));
        assert!(matches!(
            host.claim(claims("00000000-0000-7000-8000-000000000003"), unix_ms()),
            Err(crate::state::Error::Refused(
                crate::state::Refusal::HostFull
            ))
        ));
        assert_eq!(host.snapshot.leases[JOB2].state, LeaseState::Started);
        // Unrelated work still receives an ordinary guest settlement while health is latched.
        host.receive_job_terminal(
            JOB2,
            json!({"type":"terminal","outcome":"exit","exit_code":0,"elapsed_ms":1}),
            unix_ms(),
            true,
        )
        .unwrap();
        assert_eq!(host.snapshot.leases[JOB2].state, LeaseState::Ended);
        assert_eq!(
            host.snapshot.billing[JOB2].terminal.as_ref().unwrap()["outcome"],
            "exit"
        );
    }
    let root = cortexkit_test_support::scratch_root();
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let socket = dir.path().join("admin.sock");
    let r = runtime.clone();
    let p = socket.clone();
    let server = tokio::spawn(async move { crate::server::admin_socket(r, &p).await });
    let mut admin = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("{error}"),
            }
        }
    })
    .await
    .unwrap();
    write_frame(&mut admin, &json!({"op":"clear_dax_failure"}))
        .await
        .unwrap();
    assert_eq!(read_frame(&mut admin).await.unwrap()["ok"], true);
    {
        let host = runtime.host.lock().unwrap();
        assert!(!host.snapshot.per_job.dax_failed);
        let heartbeat = host.heartbeat(unix_ms(), 950, true);
        assert!(heartbeat.healthy);
        assert!(heartbeat.free_vcpus > 0);
        assert_eq!(heartbeat.free_mem_mib, 950);
    }
    server.abort();
    dax_refusal(&runtime, &fake, "00000000-0000-7000-8000-000000000003").await;
}

#[test]
fn per_job_omits_retained_tenant_disk_and_guest_reports_cannot_grant_capacity() {
    let (runtime, _, _) = fixture();
    let mut host = runtime.host.lock().unwrap();
    host.tenants.insert(
        "old-vm".into(),
        crate::state::Tenant {
            tenant: "old-tenant".into(),
            sub: claims(JOB).sub,
            vcpu_ceiling: 64,
            max_weight: Some(256),
            guest_ram_mib: 1_000_000,
            idle_suspend_s: 0,
            host_floor_bytes: 0,
        },
    );
    host.guest_ready("old-tenant", 1).unwrap();
    let now = unix_ms();
    host.record_disk("old-tenant", now, 0, 1_000_000, 2_000_000);
    host.record_memory("old-tenant", now, Ok(Some(u64::MAX)));
    host.record_fill("old-tenant", now, 0);
    host.record_warm(
        "old-tenant",
        now,
        vec![motor_protocol::HeldWorkspace {
            job_id: JOB.parse().unwrap(),
            used_ms: now,
        }],
    );
    let heartbeat = host.heartbeat(now, host.job_free_mem_mib(now), true);
    assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (4, 950));
    assert!(heartbeat.warm.is_empty());
    assert!(heartbeat.tenant_disk.is_empty());
    assert!(!serde_json::to_string(&heartbeat)
        .unwrap()
        .contains("tenant_disk"));
}
