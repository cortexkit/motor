use super::super::health as runner_health;
use super::*;

fn recorded_job(
    repository: &str,
    workspace: &str,
    weight: u32,
    started_ms: Option<u64>,
    finished_ms: u64,
    outcome: Outcome,
) -> Job {
    let id = Uuid::now_v7();
    let request = Run {
        return_paths: Vec::new(),
        return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
        job_id: id,
        gate_job: false,
        network: None,
        platform: None,
        workspace_key: workspace.to_owned(),
        workspace_kind: None,
        repository_root: format!("/checkouts/{repository}"),
        cwd: workspace.to_owned(),
        command: "true".into(),
        env: BTreeMap::new(),
        weight_hint: Some(weight),
        timeout: None,
        queue_wait_limit_s: 300,
        snapshot_commit: "a".repeat(40),
        tree_hash: "b".repeat(40),
        integration_tip: "c".repeat(40),
        siblings: Vec::new(),
        bundles: Vec::new(),
        git_dependencies: Vec::new(),
        base_commit: None,
    };
    let mut job = Job::new(id, Some(request), finished_ms, 1, weight);
    job.started_ms = started_ms;
    job.terminal(outcome, finished_ms);
    job
}

#[test]
fn repository_usage_tallies_fake_finished_records_by_repository() {
    let now = 2_000_000;
    let jobs = [
        recorded_job(
            "prefrontal",
            "/workers/head/worktree",
            4,
            Some(now - 5_000),
            now,
            Outcome::Exit { code: 0 },
        ),
        recorded_job(
            "prefrontal",
            "/workers/task/worktree",
            8,
            Some(now - 10_000),
            now - 1_000,
            Outcome::Exit { code: 1 },
        ),
        recorded_job(
            "prefrontal",
            "/workers/queued/worktree",
            2,
            None,
            now - 500,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::QueueWaitExceeded,
            },
        ),
        recorded_job(
            "magic-context",
            "/workers/another/worktree",
            4,
            Some(now - 2_000),
            now,
            Outcome::Exit { code: 0 },
        ),
    ];
    let mut usage = runner_health::RepositoryUsage::from_jobs(jobs.iter(), now);

    assert_eq!(
        usage.snapshot(now),
        BTreeMap::from([
            (
                "magic-context".into(),
                HealthRepositoryUsage {
                    reserved_vcpu_seconds: 8,
                    job_count: 1,
                    jobs_by_vcpu_size: BTreeMap::from([("4".into(), 1)]),
                    queue_wait_refusals: 0,
                },
            ),
            (
                "prefrontal".into(),
                HealthRepositoryUsage {
                    reserved_vcpu_seconds: 92,
                    job_count: 3,
                    jobs_by_vcpu_size: BTreeMap::from([
                        ("2".into(), 1),
                        ("4".into(), 1),
                        ("8".into(), 1)
                    ]),
                    queue_wait_refusals: 1,
                },
            ),
        ])
    );
}

#[test]
fn repository_usage_expires_at_the_exact_24_hour_boundary() {
    let now = 4 * runner_health::REPOSITORY_WINDOW_MS;
    let jobs = [
        recorded_job(
            "expired",
            "/workers/expired",
            4,
            Some(now - runner_health::REPOSITORY_WINDOW_MS),
            now - runner_health::REPOSITORY_WINDOW_MS,
            Outcome::Exit { code: 0 },
        ),
        recorded_job(
            "recent",
            "/workers/recent",
            2,
            Some(now - runner_health::REPOSITORY_WINDOW_MS + 1),
            now - runner_health::REPOSITORY_WINDOW_MS + 1,
            Outcome::Exit { code: 0 },
        ),
    ];
    let mut usage = runner_health::RepositoryUsage::from_jobs(jobs.iter(), now);
    let report = usage.snapshot(now);
    assert!(!report.contains_key("expired"));
    assert_eq!(report["recent"].job_count, 1);

    assert!(usage.snapshot(now + 1).is_empty());
}

#[test]
fn repository_usage_rebuilds_from_retained_records_after_restart() {
    let f = Fixture::new();
    let now = 2_000_000;
    f.clock.store(now, Ordering::SeqCst);
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let job = recorded_job(
        "prefrontal",
        "/workers/head/worktree",
        4,
        Some(now - 3_000),
        now,
        Outcome::Exit { code: 0 },
    );
    let record = runner.record_path(job.id);
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    atomic_json(&record, &job).unwrap();
    drop(runner);

    let runner = Runner::open(config).unwrap();
    let usage = runner.runner_status().health.unwrap().repository_usage;
    assert_eq!(
        usage["prefrontal"],
        HealthRepositoryUsage {
            reserved_vcpu_seconds: 12,
            job_count: 1,
            jobs_by_vcpu_size: BTreeMap::from([("4".into(), 1)]),
            queue_wait_refusals: 0,
        }
    );
}

#[test]
fn health_counts_acceptance_and_each_refusal_reason_once() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let reasons = [
        (RefusalReason::RunnerFull, "runner_full"),
        (RefusalReason::RunnerDraining, "runner_draining"),
        (RefusalReason::RunnerDiskFull, "runner_disk_full"),
        (
            RefusalReason::WorkspaceKeyRejected,
            "workspace_key_rejected",
        ),
        (
            RefusalReason::WorkspaceSetupFailed,
            "workspace_setup_failed",
        ),
        (RefusalReason::BundleRejected, "bundle_rejected"),
        (RefusalReason::TransferInterrupted, "transfer_interrupted"),
        (RefusalReason::TreeHashMismatch, "tree_hash_mismatch"),
        (
            RefusalReason::Unknown("future_refusal".into()),
            "future_refusal",
        ),
        (
            RefusalReason::Unknown("future_refusal: permission denied".into()),
            "future_refusal: permission denied",
        ),
    ];
    let mut expected = BTreeMap::new();
    for (reason, tag) in reasons {
        let (request, _) = f.run("true");
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Accepted(_)
        ));
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Duplicate
        ));
        runner
            .refuse(request.job_id, reason.clone(), "test failure")
            .unwrap();
        runner
            .refuse(request.job_id, reason, "duplicate refusal")
            .unwrap();
        expected.insert(tag.to_owned(), 1);
    }
    let counts = runner.runner_status().health.unwrap().counts;
    assert_eq!(counts.accepted, expected.len() as u64);
    assert_eq!(counts.refusals, expected);
}

#[test]
fn health_counts_admission_refusals_without_acceptance() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_queue_depth = 1;
    let runner = Runner::open(config).unwrap();
    let (first, _) = f.run("true");
    runner.accept(first).unwrap();
    let (second, _) = f.run("true");
    assert!(matches!(
        runner.accept(second).unwrap(),
        Admission::Refused(RefusalReason::RunnerFull)
    ));
    runner.begin_drain(Duration::from_secs(10), false);
    let (third, _) = f.run("true");
    assert!(matches!(
        runner.accept(third).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));
    let counts = runner.runner_status().health.unwrap().counts;
    assert_eq!(counts.accepted, 1);
    assert_eq!(
        counts.refusals,
        BTreeMap::from([("runner_full".into(), 1), ("runner_draining".into(), 1),])
    );
}

#[test]
fn health_minute_buckets_expire_and_restart_resets_counts() {
    let f = Fixture::new();
    let start = 120_000_000;
    f.clock.store(start, Ordering::SeqCst);
    let runner = Runner::open(f.config()).unwrap();
    let (first, _) = f.run("true");
    runner.accept(first.clone()).unwrap();
    runner
        .refuse(first.job_id, RefusalReason::BundleRejected, "bad upload")
        .unwrap();
    f.clock.store(start + 60_000, Ordering::SeqCst);
    let (second, _) = f.run("true");
    runner.accept(second.clone()).unwrap();
    runner
        .refuse(second.job_id, RefusalReason::TransferInterrupted, "EOF")
        .unwrap();
    f.clock.store(start + 60 * 60_000 - 1, Ordering::SeqCst);
    let health = runner.runner_status().health.unwrap();
    assert_eq!(health.since_ms, start);
    assert_eq!(health.window_minutes, 60);
    assert_eq!(health.counts.accepted, 2);
    f.clock.store(start + 60 * 60_000, Ordering::SeqCst);
    let counts = runner.runner_status().health.unwrap().counts;
    assert_eq!(counts.accepted, 1);
    assert_eq!(
        counts.refusals,
        BTreeMap::from([("transfer_interrupted".into(), 1)])
    );
    f.clock.store(start + 61 * 60_000, Ordering::SeqCst);
    assert_eq!(
        runner.runner_status().health.unwrap().counts,
        HealthCounts::default()
    );
    drop(runner);
    let runner = reopen(f.config());
    let health = runner.runner_status().health.unwrap();
    assert_eq!(health.since_ms, start + 61 * 60_000);
    assert_eq!(health.counts, HealthCounts::default());
}

#[test]
fn health_failed_units_is_absent_when_query_fails() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let mut config = f.config();
    config.systemd_command = Some(Arc::new(|args| {
        assert_eq!(
            args,
            &[
                "list-units",
                "--type=service",
                "--state=failed",
                "--no-legend",
                "--plain",
                "--no-pager",
                "motor-job-*"
            ]
        );
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"motor-job-a.service loaded failed failed a\n\nmotor-job-b.service loaded failed failed b\n".to_vec(),
            stderr: vec![],
        })
    }));
    let runner = Runner::open(config).unwrap();
    assert_eq!(
        runner.runner_status().health.unwrap().failed_job_units,
        Some(2)
    );
    drop(runner);
    let mut config = f.config();
    config.systemd_command = Some(Arc::new(|_| {
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: b"motor-job-a.service loaded failed failed a\n".to_vec(),
            stderr: b"systemd unavailable".to_vec(),
        })
    }));
    let runner = reopen(config);
    let health = runner.runner_status().health.unwrap();
    assert_eq!(health.failed_job_units, None);
    assert!(serde_json::to_value(health)
        .unwrap()
        .get("failed_job_units")
        .is_none());
}

#[test]
fn long_eight_vcpu_job_under_one_core_is_persisted_and_reported() {
    use std::os::unix::process::ExitStatusExt;
    for late in [false, true] {
        let f = Fixture::new();
        let launch_id = Uuid::now_v7();
        let mut config = f.config();
        config.systemd_command = Some(Arc::new(move |args| {
            let stdout = match args {
                ["show", "--property=Result", unit] => {
                    assert_eq!(*unit, crate::backend::host_unit_name(launch_id));
                    b"Result=success\n".to_vec()
                }
                ["show", "--property=LoadState", "--property=ActiveState", _] => {
                    b"LoadState=not-found\n".to_vec()
                }
                ["reset-failed", _] | ["list-units", ..] => vec![],
                _ => panic!("unexpected systemd command: {args:?}"),
            };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout,
                stderr: vec![],
            })
        }));
        let runner = Runner::open(config.clone()).unwrap();
        let (mut request, _) = f.run("true");
        request.weight_hint = Some(8);
        runner.accept(request.clone()).unwrap();
        let start = runner.now();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(start);
            job.process = Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(launch_id),
            });
        }
        assert_ne!(request.job_id, launch_id);
        let report = format!("Running as unit: {}\n Finished with result: success\n Service runtime: 10min 1ms\n CPU time consumed: 5min\n", crate::backend::host_unit_name(launch_id));
        if !late {
            runner.collect_accounting(request.job_id, report.as_bytes());
        }
        f.clock.store(start + 600_001, Ordering::SeqCst);
        runner
            .finish(request.job_id, Ok(std::process::ExitStatus::from_raw(0)))
            .unwrap();
        let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
        if late {
            f.clock.store(start + 900_001, Ordering::SeqCst);
            runner.collect_accounting(request.job_id, report.as_bytes());
        }
        assert_eq!(
            runner.job_status(request.job_id).unwrap().terminal.unwrap(),
            terminal
        );
        let disk: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        let usage = disk.reservation_use.unwrap();
        assert_eq!(usage.cpu_time_ns, 300_000_000_000);
        assert_eq!(usage.wall_ms, 600_001);
        assert_eq!(usage.average_use_ppm, 62_499);
        let report = runner.runner_status().health.unwrap();
        assert_eq!(report.repository_usage["main"].reserved_vcpu_seconds, 4_800);
        assert_eq!(report.repository_usage["main"].job_count, 1);
        assert_eq!(
            report.underused_reservations,
            vec![HealthUnderusedReservation {
                job_id: request.job_id,
                workspace_key: f.key.clone(),
                vcpus: 8,
                usage,
            }]
        );
        drop(runner);
        let runner = reopen(config);
        assert_eq!(
            runner
                .runner_status()
                .health
                .unwrap()
                .underused_reservations,
            report.underused_reservations
        );
    }
}

#[test]
fn reservation_report_excludes_short_busy_and_unmeasured_jobs() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (request, _) = f.run("true");
    let mut inner = runner.core.inner.lock().unwrap();
    for (wall_ms, cpu_time_ns) in [
        (600_000, Some(0)),
        (600_001, Some(1_200_002_000_000)),
        (600_001, None),
    ] {
        let mut job = Job::new(Uuid::now_v7(), Some(request.clone()), 0, 0, 8);
        job.started_ms = Some(0);
        if let Some(cpu_time_ns) = cpu_time_ns {
            job.record_reservation_use(cpu_time_ns, wall_ms);
        }
        job.terminal(Outcome::Exit { code: 0 }, wall_ms);
        inner.jobs.insert(job.id, job);
    }
    drop(inner);
    assert!(runner
        .runner_status()
        .health
        .unwrap()
        .underused_reservations
        .is_empty());
    let mut zero_wall = Job::new(Uuid::now_v7(), None, 0, 0, 8);
    zero_wall.started_ms = Some(0);
    zero_wall.record_reservation_use(1, 0);
    assert!(zero_wall.reservation_use.is_none());
}

#[test]
fn blocked_or_invalid_accounting_never_delays_or_changes_terminal() {
    use std::os::unix::process::ExitStatusExt;
    struct BlockedReport {
        ready: std::sync::mpsc::Receiver<()>,
        report: std::io::Cursor<Vec<u8>>,
        released: bool,
    }
    impl Read for BlockedReport {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if !self.released {
                self.ready.recv().map_err(io::Error::other)?;
                self.released = true;
            }
            self.report.read(bytes)
        }
    }
    for valid in [false, true] {
        let f = Fixture::new();
        let mut config = f.config();
        config.systemd_command = Some(Arc::new(|args| {
            let stdout = match args {
                ["show", "--property=Result", _] => b"Result=success\n".to_vec(),
                ["show", "--property=LoadState", "--property=ActiveState", _] => {
                    b"LoadState=not-found\n".to_vec()
                }
                ["reset-failed", _] | ["list-units", ..] => vec![],
                _ => panic!("accounting must not invoke systemctl: {args:?}"),
            };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout,
                stderr: vec![],
            })
        }));
        let runner = Runner::open(config).unwrap();
        let (request, _) = f.run("true");
        runner.accept(request.clone()).unwrap();
        let launch_id = Uuid::now_v7();
        let start = runner.now();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(start);
            job.process = Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(launch_id),
            });
        }
        let report = format!(
            "Running as unit: {}\nFinished with result: success\nCPU time consumed: {}\n",
            crate::backend::host_unit_name(launch_id),
            if valid { "56ms" } else { "unfamiliar" }
        );
        let (release, ready) = std::sync::mpsc::channel();
        let reader = {
            let runner = runner.clone();
            let id = request.job_id;
            std::thread::spawn(move || {
                runner.collect_accounting(
                    id,
                    BlockedReport {
                        ready,
                        report: std::io::Cursor::new(report.into_bytes()),
                        released: false,
                    },
                )
            })
        };
        f.clock.store(start + 1000, Ordering::SeqCst);
        runner
            .finish(
                request.job_id,
                Ok(std::process::ExitStatus::from_raw(7 << 8)),
            )
            .unwrap();
        let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
        assert_eq!(terminal.outcome, Outcome::Exit { code: 7 });
        assert_eq!(terminal.wall_ms, 1000);
        assert!(read_json::<Job>(&runner.record_path(request.job_id))
            .unwrap()
            .reservation_use
            .is_none());
        f.clock.store(start + 60_000, Ordering::SeqCst);
        release.send(()).unwrap();
        reader.join().unwrap();
        assert_eq!(
            runner.job_status(request.job_id).unwrap().terminal.unwrap(),
            terminal
        );
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert_eq!(job.reservation_use.is_some(), valid);
        if let Some(usage) = job.reservation_use {
            assert_eq!(usage.wall_ms, 1000);
            assert_eq!(usage.cpu_time_ns, 56_000_000);
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, sudo and real systemd"]
async fn isolated_forged_job_summary_never_enters_launcher_accounting() {
    let f = Fixture::new();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.cpu_topology = None;
    config.disable_warm_builds = true;
    config.clock = Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    });
    config.accounting_report = Some({
        let reports = reports.clone();
        Arc::new(move |id, report| reports.lock().unwrap().push((id, report.to_vec())))
    });
    let runner = Runner::open(config).unwrap();
    let command = r#"unit=$(awk -F: '$1 == "0" {print $3}' /proc/self/cgroup); unit=${unit##*/}; fake=$(printf 'forged-summary-job-byte\nRunning as unit: %s\nFinished with result: success\nService runtime: 1min\nCPU time consumed: 999999h\n' "$unit"); printf '%s\n' "$fake"; printf '%s\n' "$fake" >&2; printf '%s\n' "$fake" >>"/proc/$PPID/fd/2"; python3 -c 'sum(i*i for i in range(1000000))'"#;
    let (request, bytes) = f.run(command);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    until_with_diagnostics(
        || {
            read_json::<Job>(&runner.record_path(request.job_id))
                .unwrap()
                .reservation_use
                .is_some()
        },
        || launcher_accounting_diagnostics(&runner, request.job_id, &reports),
    )
    .await;
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    let usage = job.reservation_use.unwrap();
    assert!(usage.cpu_time_ns > 0 && usage.cpu_time_ns < 60_000_000_000);
    let reports = reports.lock().unwrap();
    let report = &reports
        .iter()
        .find(|(id, _)| *id == request.job_id)
        .unwrap()
        .1;
    let report = String::from_utf8_lossy(report);
    assert!(report.contains("CPU time consumed:"), "{report}");
    assert!(
        !report.contains("forged-summary-job-byte") && !report.contains("999999h"),
        "a job byte reached trusted accounting: {report}"
    );
    assert!(job_output(&runner, request.job_id).contains("forged-summary-job-byte"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, sudo and real systemd CPU accounting"]
async fn isolated_short_successful_unit_records_cpu_accounting() {
    let f = Fixture::new();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.cpu_topology = None;
    config.disable_warm_builds = true;
    config.accounting_report = Some({
        let reports = reports.clone();
        Arc::new(move |id, report| reports.lock().unwrap().push((id, report.to_vec())))
    });
    config.clock = Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    });
    let runner = Runner::open(config).unwrap();
    let (mut request, bytes) = f.run("python3 -c 'sum(i*i for i in range(1000000))'");
    request.weight_hint = Some(2);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    until_with_diagnostics(
        || {
            read_json::<Job>(&runner.record_path(request.job_id))
                .unwrap()
                .reservation_use
                .is_some()
        },
        || launcher_accounting_diagnostics(&runner, request.job_id, &reports),
    )
    .await;
    let disk: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_ne!(disk.process.unwrap().unit.unwrap(), request.job_id);
    let usage = disk
        .reservation_use
        .expect("short successful unit must supply native launcher accounting without a journal Consumed line");
    assert!(usage.cpu_time_ns > 0);
    assert!(usage.wall_ms > 0 && usage.wall_ms < 600_000);
    assert_eq!(
        usage.average_use_ppm,
        usage.cpu_time_ns / (usage.wall_ms * 2)
    );
}

#[cfg(target_os = "linux")]
fn launcher_accounting_diagnostics(
    runner: &Runner,
    id: Uuid,
    reports: &Mutex<Vec<(Uuid, Vec<u8>)>>,
) -> String {
    let (job, pending_cpu, finalizing) = {
        let inner = runner.core.inner.lock().unwrap();
        (
            inner.jobs.get(&id).cloned(),
            inner.cpu_times.get(&id).copied(),
            inner.finalizing.contains(&id),
        )
    };
    let writer_busy = runner.job_log(id).write.try_lock().is_err();
    let disk = read_json::<Job>(&runner.record_path(id))
        .map(|job| serde_json::to_string_pretty(&job).unwrap());
    let report = reports
        .lock()
        .unwrap()
        .iter()
        .find(|(job, _)| *job == id)
        .map(|(_, report)| report.clone());
    let parsed = report.as_ref().map(|report| {
        let unit = job
            .as_ref()
            .and_then(|job| job.process)
            .and_then(|process| process.unit)
            .map(|unit| runner.core.backend.unit_name(&runner.core.config, unit))
            .unwrap_or_default();
        accounting::read_cpu_time_ns(report.as_slice(), &unit)
    });
    format!("job={id} finalizing={finalizing} writer_busy={writer_busy} pending_cpu_ns={pending_cpu:?}\nmemory={}\ndisk={disk:?}\nlauncher_parse={parsed:?}\nlauncher_report={}\n{}",
        serde_json::to_string_pretty(&job).unwrap(),
        report.map(|report| String::from_utf8_lossy(&report).into_owned()).unwrap_or_else(|| "not yet captured".into()),
        runner_diagnostics(runner))
}
