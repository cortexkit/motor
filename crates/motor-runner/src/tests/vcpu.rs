use super::*;
use std::os::unix::process::ExitStatusExt;

#[cfg(target_os = "linux")]
fn assert_oom_detail_uses_configured_limits(runner: &Runner, job: &Job, detail: &str) {
    let floor_gib = runner.config().memory_floor_gib;
    let limit = crate::backend::memory_limit_gib(job.weight, floor_gib);
    assert!(
        detail.contains(&format!("memory limit of {limit} GiB")),
        "{detail}"
    );

    let more = (job.weight.max(4) * 2).min(runner.core.cpus.capacity());
    if more > job.weight {
        let more_limit = crate::backend::memory_limit_gib(more, floor_gib);
        assert!(
            detail.contains(&format!(
                "for example linux,{more}c, to raise the limit to {more_limit} GiB"
            )),
            "{detail}"
        );
    } else {
        assert!(
            detail.contains(&format!("maximum of {more} vCPUs ({limit} GiB)")),
            "{detail}"
        );
    }
}

#[test]
fn oom_detail_keeps_the_production_sixteen_and_twenty_eight_gib_wording() {
    let config = Config::new("unused", "main");
    assert_eq!(
        crate::engine::oom_memory_limit_detail(&config, 2, 8),
        "the job's memory limit of 16 GiB was reached; the kernel killed a process and the job was stopped. Request more vCPUs, for example linux,8c, to raise the limit to 28 GiB."
    );
}

#[test]
fn claims_are_exact_and_unsized_jobs_always_claim_two() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    for command in [
        "cargo test -J32",
        "cargo nextest run --test-threads=64",
        "cargo build -j 128",
        "cargo check",
        "true",
    ] {
        for hint in [None, Some(1), Some(4)] {
            let (mut request, _) = f.run(command);
            request.weight_hint = hint;
            for key in [
                "CARGO_BUILD_JOBS",
                "RUST_TEST_THREADS",
                "NEXTEST_TEST_THREADS",
            ] {
                request.env.insert(key.into(), "256".into());
            }
            assert!(matches!(
                runner.accept(request.clone()).unwrap(),
                Admission::Accepted(_)
            ));
            assert_eq!(
                runner.core.inner.lock().unwrap().jobs[&request.job_id].weight,
                hint.unwrap_or(2)
            );
        }
    }
}

#[test]
fn oversized_and_zero_claims_are_refused_before_start_with_limit_detail() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    for claim in [0, 31, u32::MAX] {
        let (mut request, _) = f.run("touch must-not-start");
        request.weight_hint = Some(claim);
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Refused(RefusalReason::RunnerFull)
        ));
        let frames = runner.replay(request.job_id, 0).unwrap();
        assert!(
            matches!(&frames[0], Frame::Terminal(t) if t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::RunnerFull })
        );
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        let terminal = job.terminal.unwrap();
        assert_eq!(terminal.ran, Some(Ran::None));
        let detail = terminal.refusal_detail().unwrap();
        assert!(
            detail.contains("1..=30") && detail.contains(&format!("vcpu request {claim}")),
            "{detail}"
        );
        assert!(job.cpus.is_empty());
        assert!(job.process.is_none() && job.started_ms.is_none() && job.setup_ms.is_none());
    }
    assert_eq!(runner.status().queue_depth, 0);
    assert!(!Path::new(&f.key).join("must-not-start").exists());
}

#[test]
fn explicit_sandbox_reservation_admits_one_through_eight_vcpus() {
    let f = Fixture::new();
    let mut config = f.config();
    assert!(config.reserve_daemon_core);
    config.reserve_daemon_core = false;
    config.cpu_topology = Some(CpuTopology::new((0..8).map(|cpu| vec![cpu]).collect()).unwrap());
    let runner = Runner::open(config).unwrap();
    assert_eq!(runner.core.cpus.capacity(), 8);
    for claim in 1..=8 {
        assert!(runner.cpu_refusal(claim).is_none());
    }
    assert!(runner.cpu_refusal(9).unwrap().1.contains("1..=8"));
}

#[tokio::test]
async fn internal_warm_build_shrinks_to_a_small_host() {
    let f = Fixture::new();
    let mut config = f.config();
    // One core, two threads (a 2-vCPU CI runner): a fixed 4-vCPU warm build
    // would be refused forever, so it claims the whole allocatable set instead.
    config.cpu_topology = Some(CpuTopology::new(vec![vec![0, 1]]).unwrap());
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    let inner = runner.core.inner.lock().unwrap();
    let warm = inner.jobs.values().find(|job| job.base).unwrap();
    assert_eq!(warm.weight, 2);
    assert!(
        !matches!(
            warm.terminal.as_ref().unwrap().outcome,
            Outcome::RefusedBeforeStart { .. }
        ),
        "the warm build must run on a small host"
    );
}

#[tokio::test]
async fn pinned_job_waits_until_its_exact_claim_fits_max_weight() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 6;
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    let release = f.scratch.join("release-vcpu-holder");
    let (mut first, bytes) = job_at(
        &f,
        "vcpu-holder",
        &format!(
            "while test ! -e '{}'; do sleep .02; done",
            release.display()
        ),
    );
    first.weight_hint = Some(4);
    f.submit(&runner, &first, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|job| job.job_id == first.job_id)
    })
    .await;
    let (mut waiting, bytes) = f.run("true");
    waiting.weight_hint = Some(4);
    f.submit(&runner, &waiting, &bytes);
    runner.tick().unwrap();
    settle().await;
    assert!(!begun(&runner, waiting.job_id));
    assert!(runner.core.inner.lock().unwrap().jobs[&waiting.job_id]
        .cpus
        .is_empty());
    assert_eq!(runner.status().running_jobs[0].weight, 4);
    fs::write(release, "").unwrap();
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        terminal(&runner, waiting.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&waiting.job_id]
            .cpus
            .len(),
        4
    );
}

#[test]
fn cpu_reservations_release_on_terminal() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    for path in ["exit", "lost", "cancel", "refusal", "restart"] {
        let (request, _) = f.run("true");
        let mut job = Job::new(request.job_id, Some(request.clone()), runner.now(), 0, 2);
        job.state = JobState::Running;
        job.cpus = vec![1, 17];
        runner.core.inner.lock().unwrap().jobs.insert(job.id, job);
        let place = || {
            let inner = runner.core.inner.lock().unwrap();
            runner
                .core
                .cpus
                .place(2, inner.jobs.values().map(Job::claimed_cpus))
        };
        assert_eq!(place(), [2, 18]);
        match path {
            "exit" => runner
                .finish(request.job_id, Ok(std::process::ExitStatus::from_raw(0)))
                .unwrap(),
            "lost" => runner
                .finish(request.job_id, Err(io::Error::other("unit lost")))
                .unwrap(),
            "cancel" | "refusal" => {
                runner
                    .core
                    .inner
                    .lock()
                    .unwrap()
                    .jobs
                    .get_mut(&request.job_id)
                    .unwrap()
                    .state = JobState::Queued;
                if path == "cancel" {
                    runner.cancel(request.job_id).unwrap();
                } else {
                    runner
                        .refuse(
                            request.job_id,
                            RefusalReason::WorkspaceSetupFailed,
                            "setup failed",
                        )
                        .unwrap();
                }
            }
            "restart" => {
                let mut inner = runner.core.inner.lock().unwrap();
                refuse_before_start(
                    &runner.core.health,
                    inner.jobs.get_mut(&request.job_id).unwrap(),
                    RefusalReason::LostToRestart,
                    "restart before launch",
                    Some("restart before launch".into()),
                    runner.now(),
                );
                runner.save(&inner.jobs[&request.job_id]).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            place(),
            [1, 17],
            "{path} must release its recorded reservation"
        );
        assert_eq!(
            runner.core.inner.lock().unwrap().jobs[&request.job_id].cpus,
            [1, 17]
        );
    }
}

#[test]
fn recorded_live_cpu_sets_reregister_after_restart() {
    let f = Fixture::new();
    let config = f.config();
    let (request, _) = f.run("true");
    let mut job = Job::new(request.job_id, Some(request), 0, 0, 3);
    job.state = JobState::Running;
    job.cpus = vec![1, 2, 18];
    let record = config
        .state_dir()
        .join("jobs")
        .join(job.id.to_string())
        .join("record.json");
    atomic_json(&record, &job).unwrap();
    let recovered: Job = read_json(&record).unwrap();
    let cpus = config.cpu_topology.as_ref().unwrap();
    assert_eq!(
        cpus.place(3, std::iter::once(recovered.claimed_cpus())),
        [3, 17, 19]
    );
    // Older unpinned running records have no set to re-register.
    let mut old = serde_json::to_value(&job).unwrap();
    old.as_object_mut().unwrap().remove("cpus");
    let old: Job = serde_json::from_value(old).unwrap();
    assert!(old.claimed_cpus().is_empty());
}

#[test]
fn windows_avoidance_options_require_a_valid_pair() {
    let f = Fixture::new();
    for (cpus, lock) in [
        (Some("12-15"), None),
        (None, Some("/windows/slot.lock")),
        (Some(""), Some("/windows/slot.lock")),
        (Some("32"), Some("/windows/slot.lock")),
        (Some("12-15"), Some("relative/slot.lock")),
    ] {
        let mut config = f.config();
        config.windows_cpus = cpus.map(String::from);
        config.windows_slot_lock = lock.map(PathBuf::from);
        assert!(Runner::open(config).is_err(), "{cpus:?} {lock:?}");
    }
    let mut config = f.config();
    config.windows_cpus = Some("12-15".into());
    config.windows_slot_lock = Some("/windows/slot.lock".into());
    assert_eq!(Runner::open(config).unwrap().core.cpus.capacity(), 30);
}

#[tokio::test]
async fn windows_start_does_not_move_existing_job_or_exempt_cache_fills() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 4;
    let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cpus = config
        .cpu_topology
        .take()
        .unwrap()
        .avoid_windows(&[1], PathBuf::from("/windows/slot.lock"))
        .unwrap();
    let observed = active.clone();
    cpus.set_windows_probe(Arc::new(move || Ok(observed.load(Ordering::SeqCst))));
    config.cpu_topology = Some(cpus);
    let runner = Runner::open(config).unwrap();
    let release = f.scratch.join("release-before-windows");
    let (first, bytes) = f.run(&format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    ));
    f.submit(&runner, &first, &bytes);
    until(|| {
        runner
            .status()
            .running_jobs
            .iter()
            .any(|job| job.job_id == first.job_id)
    })
    .await;
    let original = runner.core.inner.lock().unwrap().jobs[&first.job_id]
        .cpus
        .clone();
    assert_eq!(original, [1, 17]);

    active.store(true, Ordering::SeqCst);
    let (second, bytes) = job_at(&f, "after-windows", "true");
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        terminal(&runner, second.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&first.job_id].cpus,
        original
    );
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&second.job_id].cpus,
        [2, 18]
    );
    fs::write(release, "").unwrap();
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    let inner = runner.core.inner.lock().unwrap();
    let fills: Vec<_> = inner.jobs.values().filter(|job| job.base).collect();
    assert!(!fills.is_empty());
    for fill in fills {
        assert_eq!(fill.cpus.len(), 4);
        assert!(fill.cpus.iter().all(|cpu| ![0, 1, 16, 17].contains(cpu)));
    }
    let recorded: Job = read_json(&runner.record_path(first.job_id)).unwrap();
    assert_eq!(recorded.cpus, original);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, sudo and systemd cpuset support"]
async fn isolated_four_vcpus_enforce_cpu_set_and_memory_max() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.cpu_topology = None;
    let runner = Runner::open(config).unwrap();
    let (mut request, bytes) = f.run(r#"nproc; awk '/Cpus_allowed_list/ {print $2}' /proc/self/status; group=$(awk -F: '$1 == "0" {print $3}' /proc/self/cgroup); cat "/sys/fs/cgroup${group}/memory.max""#);
    request.weight_hint = Some(4);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let output = job_output(&runner, request.job_id);
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), 3, "{output}");
    assert_eq!(lines[0], "4");
    let record: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(record.weight, 4);
    assert_eq!(record.cpus.len(), 4);
    assert_eq!(crate::cpu::parse_list(lines[1]).unwrap(), record.cpus);
    assert_eq!(lines[2], "17179869184");
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, sudo and systemd cgroup OOM support"]
async fn isolated_oom_stop_records_detail_and_health_counter() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.cpu_topology = None;
    config.memory_floor_gib = Some(1);
    let runner = Runner::open(config).unwrap();
    let (mut request, bytes) = f.run("python3 -c 'b = bytearray(5 * 2**30)'");
    request.weight_hint = Some(1);
    f.submit(&runner, &request, &bytes);

    let terminal = terminal(&runner, request.job_id).await;
    // A surviving wrapper records the command's exit. If OOM stops the wrapper
    // too, systemd retains its actual TERM/KILL signal instead of a shell exit.
    assert!(
        matches!(terminal.outcome, Outcome::Exit { code } if code != 0)
            || matches!(terminal.outcome, Outcome::Signal { signal: 9 | 15 }),
        "OOM stop must preserve a nonzero exit or the wrapper's stop signal: {:?}",
        terminal.outcome
    );
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    let detail = job
        .memory_limit_detail
        .as_ref()
        .expect("recorded OOM explanation");
    assert_oom_detail_uses_configured_limits(&runner, &job, detail);
    assert!(
        job_output(&runner, request.job_id).contains(detail),
        "the caller must receive the detail on stderr"
    );
    assert_eq!(runner.runner_status().health.unwrap().counts.oom_stops, 1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, sudo and systemd cgroup OOM support"]
async fn isolated_oom_group_kill_preserves_signal_and_memory_detail() {
    let f = Fixture::new();
    let (mut request, bytes) = f.run("python3 -c 'b = bytearray(5 * 2**30)'");
    request.weight_hint = Some(1);
    let observed = Arc::new(Mutex::new(None));
    let armed = Arc::new(AtomicBool::new(true));
    let setup = Arc::new(Mutex::new(None));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.cpu_topology = None;
    config.memory_floor_gib = Some(1);
    let record = config
        .state_dir()
        .join("jobs")
        .join(request.job_id.to_string())
        .join("record.json");
    let exit_file = config
        .state_dir()
        .join("sandboxes")
        .join(request.job_id.to_string())
        .join("output/exit");
    config.before_gate = Some({
        let setup = setup.clone();
        let record = record.clone();
        Arc::new(move || {
            if !armed.swap(false, Ordering::SeqCst) {
                return;
            }
            // Kill the exit-file writer with the memory-hungry child so the
            // monitor must recover the actual signal from the retained unit.
            let mut unit = None;
            let result = (|| -> io::Result<()> {
                let job: Job = read_json(&record)?;
                let launch = job
                    .process
                    .and_then(|process| process.unit)
                    .ok_or_else(|| io::Error::other("started job has no unit identity"))?;
                unit = Some(crate::backend::host_unit_name(launch));
                let unit = unit.as_ref().unwrap();
                let output = Command::new("/usr/bin/sudo")
                    .args([
                        "-n",
                        "/usr/bin/systemctl",
                        "show",
                        "--property=ControlGroup",
                        "--value",
                        unit,
                    ])
                    .output()?;
                let group = String::from_utf8_lossy(&output.stdout);
                let group = group.trim();
                if !output.status.success() || !group.starts_with('/') || group == "/" {
                    return Err(io::Error::other(format!(
                        "cannot find job cgroup for {unit}: {output:?}"
                    )));
                }
                let cgroup = format!("/sys/fs/cgroup{group}");
                let output = Command::new("/usr/bin/sudo")
                    .args([
                        "-n",
                        "/bin/sh",
                        "-c",
                        "printf '1\\n' > \"$1/memory.oom.group\"",
                        "oom-group",
                        &cgroup,
                    ])
                    .output()?;
                if !output.status.success() {
                    return Err(io::Error::other(format!(
                        "cannot enable cgroup OOM group kill for {unit}: {output:?}"
                    )));
                }

                Ok(())
            })();
            if result.is_err() {
                // This hook runs under the runner lock. Report setup failure on the
                // test thread rather than poisoning that lock with a panic.
                if let Some(unit) = &unit {
                    let _ = Command::new("/usr/bin/sudo")
                        .args(["-n", "/usr/bin/systemctl", "stop", unit])
                        .output();
                    let _ = Command::new("/usr/bin/sudo")
                        .args(["-n", "/usr/bin/systemctl", "reset-failed", unit])
                        .output();
                }
            }
            *setup.lock().unwrap() = Some(result.map_err(|error| error.to_string()));
        })
    });
    config.systemd_command = Some({
        let observed = observed.clone();
        Arc::new(move |args| {
            let output = Command::new("/usr/bin/sudo")
                .args(["-n", "/usr/bin/systemctl"])
                .args(args)
                .output()?;
            if args.contains(&"--property=Result") {
                let job: Job = read_json(&record)?;
                let launch = job
                    .process
                    .and_then(|process| process.unit)
                    .ok_or_else(|| io::Error::other("started job has no unit identity"))?;
                let unit = crate::backend::host_unit_name(launch);
                if args.last() != Some(&unit.as_str()) {
                    return Ok(output);
                }
                let status = Command::new("/usr/bin/sudo")
                    .args([
                        "-n",
                        "/usr/bin/systemctl",
                        "show",
                        "--property=Result",
                        "--property=ExecMainCode",
                        "--property=ExecMainStatus",
                        &unit,
                    ])
                    .output()?;
                assert!(status.status.success(), "{status:?}");
                let exit = fs::read_to_string(&exit_file).map_err(|error| error.to_string());
                let modified = fs::metadata(&exit_file).and_then(|meta| meta.modified());
                let status = String::from_utf8_lossy(&status.stdout).into_owned();
                println!(
                    "OOM_FINISH_OBSERVATION: unit={unit}\n{status}exit={exit:?} mtime={modified:?}"
                );
                *observed.lock().unwrap() = Some((status, exit));
            }
            Ok(output)
        })
    });
    let runner = Runner::open(config).unwrap();
    f.submit(&runner, &request, &bytes);
    wait_for(|| setup.lock().unwrap().is_some()).await;
    assert_eq!(
        setup.lock().unwrap().as_ref().unwrap(),
        &Ok(()),
        "OOM group-kill setup failed"
    );

    let terminal = terminal(&runner, request.job_id).await;
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    let (status, exit) = observed.lock().unwrap().clone().unwrap();
    println!(
        "OOM_TERMINAL_OBSERVATION: outcome={:?} monitoring_error={:?} memory_limit_detail={:?}",
        terminal.outcome, job.monitoring_error, job.memory_limit_detail
    );
    assert!(
        status.contains("Result=oom-kill"),
        "the failed unit must still be loaded when finish inspects it: {status}"
    );
    assert_eq!(
        terminal.outcome,
        Outcome::Signal { signal: 9 },
        "OOM must preserve the unit's SIGKILL; monitoring_error={:?}; unit_status={status}; exit={exit:?}",
        job.monitoring_error
    );
    let detail = job
        .memory_limit_detail
        .as_ref()
        .expect("recorded OOM explanation");
    assert_oom_detail_uses_configured_limits(&runner, &job, detail);
    assert!(job_output(&runner, request.job_id).contains(detail));
    assert_eq!(runner.runner_status().health.unwrap().counts.oom_stops, 1);
}

#[tokio::test]
async fn oversized_vcpu_refusal_frame_carries_the_recorded_detail() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (socket, server) = listener(&f, &runner);
    let (mut request, _) = f.run("true");
    request.weight_hint = Some(40);
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(request.clone()))
        .await
        .unwrap();

    let Frame::Refusal(refusal) = read_next(&mut stream).await else {
        panic!("oversized vCPU request must be refused before upload")
    };
    assert_eq!(refusal.reason, RefusalReason::RunnerFull);
    let detail = refusal.detail.expect("runner refusal detail");
    assert!(detail.contains("vcpu request 40"), "{detail}");
    assert!(detail.contains("1..=30"), "{detail}");
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(
        job.terminal.unwrap().refusal_detail(),
        Some(detail.as_str()),
        "wire detail must match the persisted terminal detail"
    );
    server.abort();
}

#[test]
fn refusal_details_redact_runner_paths_and_environment_values() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let state_path = runner
        .config()
        .state_dir()
        .join("mirrors/private-repository");
    let detail = runner.safe_refusal_detail(
        format!(
            "could not read {}; /srv/build/sandboxes/job/checkout; token=secret-value",
            state_path.display()
        ),
        ["secret-value".to_owned()],
    );
    assert!(detail.contains("runner state"), "{detail}");
    assert!(!detail.contains(state_path.to_str().unwrap()), "{detail}");
    assert!(!detail.contains("/srv/build"), "{detail}");
    assert!(!detail.contains("secret-value"), "{detail}");
    // Short values like CARGO_INCREMENTAL=0 aren't secrets; redacting them
    // would rewrite every digit in a detail such as the vCPU range.
    let short = runner.safe_refusal_detail(
        "vcpu request 40 is outside the supported range 1..=30",
        ["0".to_owned(), "1".to_owned(), "true".to_owned()],
    );
    assert_eq!(
        short,
        "vcpu request 40 is outside the supported range 1..=30"
    );
    let long = runner.safe_refusal_detail("x".repeat(REFUSAL_DETAIL_MAX_BYTES + 1), []);
    assert_eq!(long.len(), REFUSAL_DETAIL_MAX_BYTES);
}

#[test]
fn durable_exit_falls_back_to_retained_status_when_missing_or_invalid() {
    let f = Fixture::new();
    let calls = Arc::new(AtomicU64::new(0));
    let main_code = Arc::new(AtomicU64::new(1));
    let main_status = Arc::new(AtomicU64::new(42));
    let mut config = f.config();
    config.systemd_command = Some({
        let (calls, main_code, main_status) =
            (calls.clone(), main_code.clone(), main_status.clone());
        Arc::new(move |args| {
            let text = match args {
                ["show", "--property=ExecMainCode", "--property=ExecMainStatus", _] => {
                    calls.fetch_add(1, Ordering::SeqCst);
                    format!(
                        "ExecMainCode={}\nExecMainStatus={}\n",
                        main_code.load(Ordering::SeqCst),
                        main_status.load(Ordering::SeqCst)
                    )
                }
                ["list-units", ..] => String::new(),
                _ => panic!("unexpected systemd command: {args:?}"),
            };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: text.into_bytes(),
                stderr: Vec::new(),
            })
        })
    });
    let runner = Runner::open(config).unwrap();
    let process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(Uuid::now_v7()),
    });
    let exit = f.scratch.join("durable-exit");
    for (code, status, expected) in [
        (1, 42, Outcome::Exit { code: 42 }),
        (2, 9, Outcome::Signal { signal: 9 }),
        (3, 11, Outcome::Signal { signal: 11 }),
    ] {
        main_code.store(code, Ordering::SeqCst);
        main_status.store(status, Ordering::SeqCst);
        for contents in [None, Some(""), Some("bad\n"), Some("256\n")] {
            let _ = fs::remove_file(&exit);
            if let Some(contents) = contents {
                fs::write(&exit, contents).unwrap();
            }
            let before = calls.load(Ordering::SeqCst);
            let status = runner.durable_exit_status(process, &exit).unwrap();
            assert_eq!(backend::ExecutionOutcome::Unix(status).wire(), expected);
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        }
    }
    fs::write(&exit, "55\n").unwrap();
    let before = calls.load(Ordering::SeqCst);
    assert_eq!(
        runner.durable_exit_status(process, &exit).unwrap().code(),
        Some(55)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "a complete exit file remains authoritative"
    );

    fs::write(&exit, "").unwrap();
    main_code.store(0, Ordering::SeqCst);
    main_status.store(0, Ordering::SeqCst);
    let error = runner
        .durable_exit_status(process, &exit)
        .unwrap_err()
        .to_string();
    assert!(error.contains("durable exit unavailable"), "{error}");
    assert!(error.contains("systemd exit status unavailable"), "{error}");
    assert!(error.contains("no valid retained exit status"), "{error}");
}

#[test]
fn oom_without_any_exit_status_records_an_explicit_unknown_detail() {
    for contents in [None, Some("")] {
        let f = Fixture::new();
        let mut config = f.config();
        config.systemd_command = Some(Arc::new(|args| {
            let text = match args {
                ["show", "--property=LoadState", "--property=ActiveState", _] => {
                    "LoadState=loaded\nActiveState=failed\nSubState=failed\n"
                }
                ["show", "--property=ExecMainCode", "--property=ExecMainStatus", _] => {
                    "ExecMainCode=0\nExecMainStatus=0\n"
                }
                ["show", "--property=Result", _] => "Result=oom-kill\n",
                ["reset-failed", _] | ["list-units", ..] => "",
                _ => panic!("unexpected systemd command: {args:?}"),
            };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: text.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }));
        let runner = Runner::open(config).unwrap();
        let (request, _) = f.run("true");
        runner.accept(request.clone()).unwrap();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(runner.now());
            job.durable_output = true;
            job.process = Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(Uuid::now_v7()),
            });
            runner.save(job).unwrap();
        }
        let output = runner
            .config()
            .state_dir()
            .join("sandboxes")
            .join(request.job_id.to_string())
            .join("output");
        fs::create_dir_all(&output).unwrap();
        for name in ["stdout", "stderr"] {
            fs::write(output.join(name), "").unwrap();
        }
        if let Some(contents) = contents {
            fs::write(output.join("exit"), contents).unwrap();
        }
        runner.follow_unit(request.job_id).unwrap();
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert_eq!(job.terminal.unwrap().outcome, Outcome::OutcomeUnknown);
        let detail = job.memory_limit_detail.unwrap();
        assert!(detail.contains("memory limit of 16 GiB"), "{detail}");
        assert!(detail.contains("exit status could not be read"), "{detail}");
        assert!(
            detail.contains("systemd exit status unavailable"),
            "{detail}"
        );
        assert!(job_output(&runner, request.job_id).contains("systemd exit status unavailable"));
        assert_eq!(runner.runner_status().health.unwrap().counts.oom_stops, 1);
    }
}

/// A Boat job's PID namespace init that fails exits 226 before the wrapper
/// reads the gate, so the command never ran. That must reach the caller as a
/// retry-safe refusal carrying the init's reason, never as "exit 226". The
/// same status with an exit file, or without the init's line, is the job's
/// own exit and stays one.
#[cfg(target_os = "linux")]
#[test]
fn boat_pid_namespace_setup_failure_is_refused_before_start_not_reported_as_exit() {
    use crate::isolation::pid_init;
    let line = format!(
        "{}mount the namespace's proc: Operation not permitted (os error 1)",
        pid_init::SETUP_FAILURE_PREFIX
    );
    for (case, exit_file, setup_error, boat_init) in [
        ("setup failure", "", line.as_str(), true),
        ("job exited 226", "226\n", line.as_str(), true),
        ("no init line", "", "", true),
        ("unit without the init", "", line.as_str(), false),
    ] {
        let f = Fixture::new();
        let mut config = f.config();
        config.boat_profile = true;
        config.boat_memory_max_bytes = Some(2 << 30);
        config.private_pids = !boat_init;
        config.systemd_command = Some(Arc::new(|args| {
            let text = match args {
                ["show", "--property=LoadState", "--property=ActiveState", _] => {
                    "LoadState=loaded\nActiveState=failed\nSubState=failed\n"
                }
                ["show", "--property=ExecMainCode", "--property=ExecMainStatus", _] => {
                    "ExecMainCode=1\nExecMainStatus=226\n"
                }
                ["show", "--property=Result", _] => "Result=exit-code\n",
                ["reset-failed", _] | ["list-units", ..] => "",
                _ => panic!("unexpected systemd command: {args:?}"),
            };
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: text.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }));
        let runner = Runner::open(config).unwrap();
        let (request, _) = f.run("true");
        runner.accept(request.clone()).unwrap();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(runner.now());
            job.started = Some(Started::new(0, 0, runner.now()));
            job.next_seq = 1;
            job.durable_output = true;
            job.process = Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(Uuid::now_v7()),
            });
            runner.save(job).unwrap();
        }
        let output = runner
            .config()
            .state_dir()
            .join("sandboxes")
            .join(request.job_id.to_string())
            .join("output");
        fs::create_dir_all(&output).unwrap();
        for name in ["stdout", "stderr"] {
            fs::write(output.join(name), "").unwrap();
        }
        fs::write(output.join("exit"), exit_file).unwrap();
        fs::write(output.join(pid_init::SETUP_ERROR_FILE), setup_error).unwrap();
        runner.follow_unit(request.job_id).unwrap();
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        let terminal = job.terminal.clone().unwrap();
        if case == "setup failure" {
            assert_eq!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::WorkspaceSetupFailed
                },
                "{case}"
            );
            assert_eq!(terminal.ran, Some(Ran::None), "{case}");
            assert_eq!(terminal.refusal_detail(), Some(line.as_str()), "{case}");
            assert_eq!(job.state, JobState::Tombstone, "{case}");
            assert!(job.started.is_none() && job.started_ms.is_none(), "{case}");
        } else {
            assert_eq!(terminal.outcome, Outcome::Exit { code: 226 }, "{case}");
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, sudo and systemd isolation"]
async fn isolated_ordinary_exits_preserve_exact_codes_without_status_fallback() {
    for (command, code) in [("true", 0), ("exit 7", 7)] {
        let f = Fixture::new();
        let fallback_reads = Arc::new(AtomicU64::new(0));
        let mut config = f.config();
        config.isolate_jobs = true;
        config.cpu_topology = None;
        config.systemd_command = Some({
            let fallback_reads = fallback_reads.clone();
            Arc::new(move |args| {
                if args.contains(&"--property=ExecMainCode") {
                    fallback_reads.fetch_add(1, Ordering::SeqCst);
                }
                Command::new("/usr/bin/sudo")
                    .args(["-n", "/usr/bin/systemctl"])
                    .args(args)
                    .output()
            })
        });
        let runner = Runner::open(config).unwrap();
        let (request, bytes) = f.run(command);
        f.submit(&runner, &request, &bytes);
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code }
        );
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert!(job.durable_output);
        assert_eq!(job.monitoring_error, None);
        assert_eq!(job.memory_limit_detail, None);
        // Real mount permissions must allow the wrapper's fixed-inode write.
        // A fallback would hide a broken write even if it recovered the same code.
        assert_eq!(fallback_reads.load(Ordering::SeqCst), 0);
    }
}
