use super::*;

#[test]
fn pre_vm_queue_diagnostics_do_not_name_a_foreign_account_holder() {
    let (runtime, _, _) = fixture();
    let first = claims(JOB);
    claim(&runtime, &first);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    let mut other = claims(JOB2);
    other.sub.push_str("-other");
    claim(&runtime, &other);
    {
        let mut host = runtime.host.lock().unwrap();
        let disk = host.snapshot.per_job.executions[JOB2].disk_id.clone();
        // Even a stale or inconsistent attachment must not disclose another account.
        host.snapshot
            .per_job
            .disks
            .get_mut(&disk)
            .unwrap()
            .attached_job = Some(JOB.into());
    }
    let status = queue_status(&runtime, &other).unwrap();
    assert_eq!(status.waiting_on.as_deref(), Some("checkout disk"));
    assert!(status.blocker.is_none());
    other.sub = first.sub;
    assert!(queue_status(&runtime, &other).is_none());
}

#[test]
fn adoption_polling_is_bounded_per_vm_and_exit_reports_do_not_wait_for_it() {
    let (runtime, fake, _) = fixture();
    let now = unix_ms();
    for (job, key) in [(JOB, 5), (JOB2, 6)] {
        let mut c = claims(job);
        c.workspace_key_hash = Some(motor_lease::base64url(&[key; 32]));
        claim(&runtime, &c);
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot_at(job, now).unwrap().unwrap();
        host.job_runner_hello(job).unwrap();
    }
    let count = |job: &str| {
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == format!("adopt:{job}"))
            .count()
    };
    for offset in (0..2000).step_by(10) {
        runtime.tick_jobs(now + offset).unwrap();
    }
    assert_eq!((count(JOB), count(JOB2)), (1, 1));
    runtime.tick_jobs(now + 2000).unwrap();
    assert_eq!((count(JOB), count(JOB2)), (2, 2));

    fake.exits
        .lock()
        .unwrap()
        .push(motor_vm_helper::job::ExitReport {
            job_id: JOB.into(),
            oom_kill: 0,
            egress_bytes: None,
        });
    runtime.tick_jobs(now + 3000).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
    assert_eq!((count(JOB), count(JOB2)), (2, 2));
    runtime.tick_jobs(now + 4000).unwrap();
    assert_eq!((count(JOB), count(JOB2)), (2, 3));
    assert!(!runtime.job_adoptions.lock().unwrap().contains_key(JOB));
    // A backwards clock must not suppress probing indefinitely.
    runtime.tick_jobs(now + 3500).unwrap();
    assert_eq!(count(JOB2), 4);
}

#[test]
fn adoption_failures_are_paced_without_delaying_stop_deadlines() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let now = unix_ms();
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot_at(JOB, now).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.delivered(id).unwrap();
        host.job_started(JOB, now).unwrap();
    }
    fake.fail_adopt.store(true, Ordering::SeqCst);
    assert!(runtime.tick_jobs(now).is_err());
    for offset in (10..2000).step_by(10) {
        runtime.tick_jobs(now + offset).unwrap();
    }
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("adopt:"))
            .count(),
        1
    );
    fake.fail_adopt.store(false, Ordering::SeqCst);
    // Poll just before the absolute execution deadline, then cross it within
    // the cache interval. The stop must not wait for another liveness probe.
    let deadline = now + c.max_s * 1000 + crate::billing::BACKSTOP_CANCEL_GRACE_MS;
    runtime.tick_jobs(deadline - 1).unwrap();
    runtime.tick_jobs(deadline).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|c| c.starts_with("adopt:")).count(), 2);
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == format!("stop:{JOB}"))
            .count(),
        1
    );
}

#[test]
fn memory_advertisement_logs_initial_gib_changes_and_one_minute_refresh() {
    let (runtime, _, _) = fixture();
    let mut host = runtime.host.lock().unwrap();
    let now = unix_ms();
    let sample = host.fresh_host_resources(now).unwrap();
    let logs = crate::test_logs::capture(|| {
        for (offset, free) in [
            (0, 4000),
            (1, 5023),
            (2, 5024),
            (3, 4000),
            (60_002, 4000),
            (60_003, 4000),
        ] {
            host.record_host_resources(HostResources {
                at_ms: now + offset,
                ..sample
            });
            host.heartbeat(now + offset, free, true);
        }
        host.snapshot.per_job.dax_failed = true;
        host.heartbeat(now + 60_004, 4000, true);
    });
    let logs: Vec<_> = logs
        .iter()
        .filter(|s| s.contains("message=job_memory_advertised "))
        .collect();
    assert_eq!(logs.len(), 5, "{logs:?}");
    for (line, free) in logs.iter().zip([4000, 5024, 4000, 4000, 0]) {
        assert!(line.starts_with("INFO "), "{line}");
        assert!(line.contains(&format!("free_mem_mib={free} ")), "{line}");
    }
}

#[test]
fn lifecycle_comparison_heartbeats_do_not_log_their_placeholder_memory() {
    // The broker loop builds a heartbeat with free memory 0 on every tick only
    // to compare lifecycle state, then the real one. Only the real figure may
    // reach the log, and the one-minute refresh must still suppress repeats.
    let (runtime, _, _) = fixture();
    let host = runtime.host.lock().unwrap();
    let now = unix_ms();
    let logs = crate::test_logs::capture(|| {
        for offset in 0..8 {
            host.heartbeat_unlogged(now + offset, 0, true);
            host.heartbeat(now + offset, 4000, true);
        }
    });
    let logs: Vec<_> = logs
        .iter()
        .filter(|s| s.contains("message=job_memory_advertised "))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(logs[0].contains("free_mem_mib=4000 "), "{}", logs[0]);
}

#[test]
fn memory_advertisement_logs_the_actual_accounting_inputs_and_unreserved_disk_waiters() {
    let (runtime, _, _) = fixture();
    let now = unix_ms();
    {
        let mut host = runtime.host.lock().unwrap();
        host.record_host_resources(HostResources {
            at_ms: now,
            available_mem_mib: 100_000,
            job_vm_headroom_mib: 80_000,
            vcpu_ceiling: 2,
            permit_budget: 100,
        });
        host.reserve_fill(&claims(JOB).sub, &"a".repeat(64), 1000, now)
            .unwrap()
            .unwrap();
    }
    for (job, key, boot, hello) in [
        (JOB, 5, true, true),
        (JOB2, 6, true, false),
        ("00000000-0000-7000-8000-000000000003", 5, false, false),
    ] {
        let mut c = claims(job);
        c.workspace_key_hash = Some(motor_lease::base64url(&[key; 32]));
        claim(&runtime, &c);
        if boot {
            let mut host = runtime.host.lock().unwrap();
            host.begin_job_boot_at(job, now).unwrap().unwrap();
            if hello {
                host.job_runner_hello(job).unwrap();
            }
        }
    }
    let mut host = runtime.host.lock().unwrap();
    let disk = host.snapshot.per_job.executions[JOB2].disk_id.clone();
    host.snapshot.per_job.disks.get_mut(&disk).unwrap().recovery =
        Some(crate::state::jobs::recovery::Recovery {
            id: "00000000-0000-7000-8000-000000000004".into(),
            memory_debit_mib: 600,
            guest_ram_mib: 700,
            started_at_ms: None,
            phase: crate::state::jobs::recovery::Phase::Running,
            retry_after_ms: None,
        });
    host.job_vm_file_mib = Some(512);
    let fill_debit = host
        .snapshot
        .per_job
        .fills
        .values()
        .next()
        .unwrap()
        .memory_debit_mib;
    let logs = crate::test_logs::capture(|| {
        let free = host.job_free_mem_mib(now);
        assert_eq!(free, 80_000 - 50 - 120 - 600 - fill_debit);
        host.heartbeat(now, free, true);
    });
    let line = logs
        .iter()
        .find(|s| s.contains("message=job_memory_advertised "))
        .unwrap();
    for field in [
        "available_mem_mib=100000 ",
        "job_vm_headroom_mib=80000 ",
        "job_vm_file_mib=512 ",
        "host_reserve_mib=50 ",
        "booting_memory_debit_mib=120 ",
        "running_memory_debit_mib=120 ",
        "running_subtracted_mib=0 ",
        "recovery_memory_debit_mib=600 ",
        "disk_waiters=1 ",
    ] {
        assert!(line.contains(field), "missing {field}: {line}");
    }
    assert!(
        line.contains(&format!("fill_memory_debit_mib={fill_debit} ")),
        "{line}"
    );
}

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

fn slice_files(storage: &Storage, max: &str, current: &str, stat: &str) -> std::path::PathBuf {
    let slice = storage.root.path().join("slice");
    std::fs::create_dir_all(&slice).unwrap();
    for (name, content) in [
        ("memory.max", max),
        ("memory.current", current),
        ("memory.stat", stat),
    ] {
        std::fs::write(slice.join(name), content).unwrap();
    }
    slice
}

#[test]
fn slice_file_pages_are_reclaimable_but_anon_and_kernel_are_not() {
    let (runtime, fake, storage) = fixture();
    let slice = slice_files(
        &storage,
        "1048576000\n",
        "1038090240\n",
        "anon 786432000\nfile 209715200\nkernel 41943040\nfile_mapped 104857600\n",
    );
    // 1000 MiB limit - (990 MiB charged - 200 MiB file) = 210 MiB.
    let headroom = job_vm_headroom_mib(&slice).unwrap();
    assert_eq!(headroom, 210);
    assert_eq!(job_vm_memory(&slice).unwrap().file_mib, Some(200));
    *fake.slice_headroom_mib.lock().unwrap() = Some(headroom);
    fake.capacity_available.store(true, Ordering::SeqCst);
    runtime.host.lock().unwrap().enforce_storage();
    runtime.probe_storage_at(1000);
    let host = runtime.host.lock().unwrap();
    assert_eq!(host.job_free_mem_mib(1000), 160);
}

#[test]
fn invalid_or_missing_slice_files_never_yield_headroom() {
    let (_, _, storage) = fixture();
    for (max, current, stat) in [
        ("max", "0", "file 0"),
        ("bad", "0", "file 0"),
        ("1000", "bad", "file 0"),
        ("1000", "0", "anon 0"),
        ("1000", "0", "file bad"),
        ("1000", "0", "file 0\nfile 0"),
        ("1000", "0", "file 0 extra"),
    ] {
        let slice = slice_files(&storage, max, current, stat);
        assert!(
            job_vm_headroom_mib(&slice).is_err(),
            "{max}, {current}, {stat}"
        );
    }
    for name in ["memory.max", "memory.current", "memory.stat"] {
        let slice = slice_files(&storage, "1000", "0", "file 0");
        std::fs::remove_file(slice.join(name)).unwrap();
        assert!(job_vm_headroom_mib(&slice).is_err(), "{name}");
    }
}

#[test]
fn slice_headroom_saturates_drift_and_floors_bytes_to_mib() {
    let (_, _, storage) = fixture();
    for (current, file, expected) in [(0, 100, 1), (2097153, 0, 0), (1, 0, 0)] {
        let slice = slice_files(
            &storage,
            "1048576",
            &current.to_string(),
            &format!("file {file}"),
        );
        assert_eq!(job_vm_headroom_mib(&slice).unwrap(), expected);
    }
}

#[test]
fn unreadable_slice_file_withdraws_a_previously_fresh_capacity_sample() {
    let (runtime, fake, _) = fixture();
    fake.capacity_available.store(true, Ordering::SeqCst);
    runtime.host.lock().unwrap().enforce_storage();
    runtime.probe_storage_at(1000);
    assert_eq!(runtime.host.lock().unwrap().job_free_mem_mib(1000), 950);

    // Model a read error through Control, including PermissionDenied even when
    // the test runner is privileged enough to bypass a fixture's mode bits.
    *fake.slice_headroom_mib.lock().unwrap() = None;
    runtime.probe_storage_at(1001);
    let host = runtime.host.lock().unwrap();
    assert!(host.fresh_host_resources(1001).is_none());
    let heartbeat = host.heartbeat(1001, host.job_free_mem_mib(1001), true);
    assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (0, 0));
}

#[test]
fn missing_slice_logs_path_and_remedy_only_when_failure_changes() {
    let (runtime, _, storage) = fixture();
    let slice = storage.root.path().join("missing-motorvm.slice");
    let detail = format!("{:#}", job_vm_memory(&slice).unwrap_err());
    let mut host = runtime.host.lock().unwrap();
    let sample = host.fresh_host_resources(unix_ms()).unwrap();
    let logs = crate::test_logs::capture(|| {
        // An earlier heartbeat must not hide the more specific probe failure.
        assert!(host.fresh_host_resources(1000).is_none());
        for now in 1000..1003 {
            host.host_resources_unavailable(&detail);
            let heartbeat = host.heartbeat(now, host.job_free_mem_mib(now), true);
            assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (0, 0));
        }
        host.host_resources_unavailable("different probe failure");
        host.host_resources_unavailable("different probe failure");
        host.record_host_resources(HostResources {
            at_ms: 1004,
            ..sample
        });
        assert!(host.job_free_mem_mib(1004) > 0);
        host.host_resources_unavailable(&detail);
        host.host_resources_unavailable(&detail);
    });
    let failures: Vec<_> = logs
        .iter()
        .filter(|line| line.contains("message=host_resources_unavailable "))
        .collect();
    assert_eq!(failures.len(), 3, "{logs:?}");
    for line in [failures[0], failures[2]] {
        assert!(line.contains(slice.to_str().unwrap()), "{line}");
        assert!(line.contains("memory.max"), "{line}");
        assert!(
            line.contains("sudo systemctl restart motor-vm-helper.service"),
            "{line}"
        );
    }
}

#[test]
fn host_sample_freshness_bounds_both_past_age_and_future_skew() {
    let (runtime, _, _) = fixture();
    let mut host = runtime.host.lock().unwrap();
    let mut sample = host.fresh_host_resources(unix_ms()).unwrap();
    sample.at_ms = 50_000;
    host.record_host_resources(sample);
    let window = crate::state::MEMORY_FRESH_MS;
    for now in [49_500, 49_000, 50_000 - window, 50_000, 50_000 + window] {
        assert!(host.fresh_host_resources(now).is_some(), "{now}");
    }
    for now in [50_000 - window - 1, 50_001 + window] {
        assert!(host.fresh_host_resources(now).is_none(), "{now}");
    }
    host.host_resources_unavailable("test missing sample");
    assert!(host.fresh_host_resources(50_000).is_none());
}

async fn dax_refusal(runtime: &Arc<Runtime>, fake: &Fake, job: &str) {
    *fake.init_reply.lock().unwrap() = Some(json!({"ok":false,"error":"job_vm_dax_required"}));
    let mut c = claims(job);
    c.workspace_key_hash = Some(motor_lease::base64url(&[job.as_bytes()[job.len() - 1]; 32]));
    let id = claim(runtime, &c);
    existing_workspace(runtime, job);
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
        job_vm_headroom_mib: u64::MAX,
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
