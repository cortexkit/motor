use super::*;

fn job_runner(f: &Fixture) -> Runner {
    Runner::open(job_config(f)).unwrap()
}

fn job_config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.per_job = true;
    config
}

#[test]
fn forked_marker_is_read_at_first_run_before_workspace_binding_and_only_once() {
    for forked in [false, true] {
        let f = Fixture::new();
        let marker = f.scratch.join("motor-workspace-forked");
        let mut config = job_config(&f);
        config.fixture_workspace_forked_marker = Some(marker.clone());
        let runner = Runner::open(config).unwrap();
        assert_eq!(runner.core.inner.lock().unwrap().workspace_forked, None);
        if forked {
            fs::write(&marker, "1").unwrap();
        }
        let (request, _) = f.run("true");
        runner.accept(request.clone()).unwrap();
        {
            let inner = runner.core.inner.lock().unwrap();
            assert_eq!(inner.workspace_forked, Some(forked));
            assert_eq!(inner.per_job_workspace.as_deref(), Some(f.key.as_str()));
            assert_eq!(inner.per_job_id, Some(request.job_id));
        }
        if forked {
            fs::remove_file(&marker).unwrap();
        } else {
            fs::write(&marker, "1").unwrap();
        }
        let (second, _) = f.run("true");
        runner.accept(second).unwrap();
        assert_eq!(
            runner.core.inner.lock().unwrap().workspace_forked,
            Some(forked)
        );
    }
}

// Build-base candidates exist only on Linux, where fixed checkout paths do.
#[cfg(target_os = "linux")]
#[test]
fn per_job_warm_build_guard_blocks_fixed_path_base_fill_queueing() {
    let f = Fixture::new();
    let mut config = job_config(&f);
    config.disable_warm_builds = false;
    config.tenant_write_key = Some(true);
    config.account_subject = Some("acct:00000000000000000000000000".into());
    let runner = Runner::open(config).unwrap();
    assert!(runner.config().tenant_write_key_held());
    assert!(!runner.warm_builds_enabled("nonempty-tip"));
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    let inner = runner.core.inner.lock().unwrap();
    let mut candidate_config = runner.config().clone();
    candidate_config.fixed_path_repositories = vec![f.root.clone()];
    assert!(runner
        .build_base_candidates(&inner, &candidate_config)
        .is_empty());
    assert!(!inner.jobs.values().any(|job| job.base));
    assert!(inner
        .catalog
        .repositories
        .values()
        .all(|repository| repository.desired.is_none()));
}

pub(super) async fn raw_terminal(client: &mut tokio::io::DuplexStream) -> serde_json::Value {
    use tokio::io::AsyncReadExt;
    loop {
        let length = client.read_u32().await.unwrap() as usize;
        assert!(length <= motor_protocol::MAX_FRAME_BYTES);
        let mut bytes = vec![0; length];
        client.read_exact(&mut bytes).await.unwrap();
        let frame: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if frame["type"] == "terminal" {
            return frame;
        }
    }
}

#[tokio::test]
async fn per_job_terminal_stream_carries_accounting_and_only_burst_overage() {
    for burst in [false, true] {
        let f = Fixture::new();
        let runner = job_runner(&f);
        let (request, bytes) = f.run("true");
        f.submit(&runner, &request, &bytes);
        runner.tick().unwrap();
        let expected = terminal(&runner, request.job_id).await;
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&request.job_id).unwrap();
            job.memory_stated_mib = Some(8);
            job.memory_burst_mib = burst.then_some(4);
            job.memory_accounting_pending = true;
            job.process = Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(request.job_id),
            });
            let working = if burst { 10 } else { 6 };
            job.mem_working_peak_mib = serde_json::from_value(working.into()).unwrap();
            let over = crate::working_memory::OverIntegral::default();
            over.sample(Some(10 << 20), 8, 0);
            over.sample(Some(10 << 20), 8, 50);
            // Even a stray measured integral must not escape a no-burst lease.
            job.mem_over_mib_ms = Some(over);
        }
        let unit = runner
            .core
            .backend
            .unit_name(runner.config(), request.job_id);
        let peak = if burst { "11.0M" } else { "7.0M" };
        runner.collect_accounting(request.job_id, format!("Running as unit: {unit}\nFinished with result: success\nCPU time consumed: 95ms\nMemory peak: {peak}\n").as_bytes());
        let (mut client, task) =
            connect(&runner, lease(request.job_id, LeasePurpose::Attach)).await;
        write_frame_async(
            &mut client,
            &Frame::Attach(AttachRequest::new(request.job_id, 0)),
        )
        .await
        .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(10), raw_terminal(&mut client))
            .await
            .unwrap();
        assert_eq!(frame["mem_peak_mib"], if burst { 12 } else { 8 });
        assert_eq!(frame["mem_working_peak_mib"], if burst { 10 } else { 6 });
        assert_eq!(
            frame.get("mem_over_mib_ms").and_then(|v| v.as_u64()),
            burst.then_some(100)
        );
        assert_eq!(frame.get("mem_over_mib_ms").is_some(), burst);
        assert_eq!(
            serde_json::from_value::<Frame>(frame).unwrap(),
            Frame::Terminal(expected)
        );
        write_frame_async(
            &mut client,
            &Frame::TerminalStored {
                jti: request.job_id,
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn per_job_terminal_waits_at_most_two_seconds_for_accounting() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    let expected = terminal(&runner, request.job_id).await;
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&request.job_id).unwrap();
        job.memory_accounting_pending = true;
    }
    let (mut client, task) = connect(&runner, lease(request.job_id, LeasePurpose::Attach)).await;
    write_frame_async(
        &mut client,
        &Frame::Attach(AttachRequest::new(request.job_id, 0)),
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let mut read = tokio::spawn(async move {
        let frame = raw_terminal(&mut client).await;
        (frame, client)
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut read)
            .await
            .is_err(),
        "terminal escaped before accounting settled"
    );
    let (frame, mut client) = tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    eprintln!(
        "pending launcher terminal wait: {} ms",
        started.elapsed().as_millis()
    );
    for name in ["mem_peak_mib", "mem_working_peak_mib", "mem_over_mib_ms"] {
        assert!(frame.get(name).is_none(), "unknown {name} must be absent");
    }
    assert_eq!(
        serde_json::from_value::<Frame>(frame).unwrap(),
        Frame::Terminal(expected)
    );
    assert!(
        !runner.core.inner.lock().unwrap().jobs[&request.job_id].memory_accounting_pending,
        "terminal delivery must also release lifecycle accounting"
    );
    write_frame_async(
        &mut client,
        &Frame::TerminalStored {
            jti: request.job_id,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn host_admitted_run_survives_expiry_while_waiting_for_boot() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (mut run, _) = f.run("true");
    run.weight_hint = Some(2);
    run.timeout = Some(30);
    let mut token = lease(run.job_id, LeasePurpose::Run);
    token.exp = token.exp.saturating_sub(24 * 60 + 60);
    let (mut client, task) = connect(&runner, token).await;
    write_frame_async(&mut client, &Frame::Run(run.clone()))
        .await
        .unwrap();
    assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
    assert!(runner.has_job_record(run.job_id));
    drop(client);
    task.await.unwrap().unwrap();
    assert!(pending(&runner).iter().any(|event| {
        event.jti == Some(run.job_id) && event.event == TenantEventKind::Admitted
    }));
}

#[tokio::test]
async fn host_admitted_expired_lease_still_requires_matching_run() {
    for case in 0..4 {
        let f = Fixture::new();
        let runner = job_runner(&f);
        let (mut run, _) = f.run("true");
        run.weight_hint = Some(2);
        run.timeout = Some(30);
        let id = run.job_id;
        let mut token = lease(id, LeasePurpose::Run);
        token.exp = 1;
        match case {
            0 => run.weight_hint = Some(12),
            1 => run.network = Some(Network::Outbound),
            2 => run.timeout = Some(3600),
            3 => run.job_id = Uuid::now_v7(),
            _ => unreachable!(),
        }
        let (mut client, task) = connect(&runner, token).await;
        write_frame_async(&mut client, &Frame::Run(run.clone()))
            .await
            .unwrap();
        if case != 3 {
            assert!(matches!(
                frame(&mut client).await,
                Some(Frame::Refusal(refusal)) if refusal.reason == RefusalReason::RequestRejected
                    && refusal.job_id == Some(id)
            ));
        }
        assert!(frame(&mut client).await.is_none());
        task.await.unwrap().unwrap();
        assert!(!runner.has_job_record(id));
        assert!(!runner.has_job_record(run.job_id));
        let events = pending(&runner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, TenantEventKind::Refused);
        assert_eq!(
            events[0].cause,
            Some(if case == 3 {
                RefusedCause::Disconnect
            } else {
                RefusedCause::Mismatch
            })
        );
    }
}

#[test]
fn job_vm_accepts_one_execution_and_workspace_even_after_completion() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (first, _) = f.run("true");
    assert!(matches!(
        runner.accept(first.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    assert!(matches!(
        runner.accept(first.clone()).unwrap(),
        Admission::Duplicate
    ));
    runner.cancel(first.job_id).unwrap();
    for same_workspace in [true, false] {
        let mut next = first.clone();
        next.job_id = Uuid::now_v7();
        if !same_workspace {
            next.workspace_key.push_str("-other");
        }
        assert!(matches!(
            runner.accept(next.clone()).unwrap(),
            Admission::Refused(RefusalReason::RequestRejected)
        ));
        let job = runner.core.inner.lock().unwrap().jobs[&next.job_id].clone();
        assert!(job.started_ms.is_none());
        assert!(matches!(
            job.terminal.unwrap().outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected
            }
        ));
    }
    assert!(!runner.warm_builds_enabled("tip"));
}

/// A fill VM's runner starts as a plain tenant runner: the guest agent adds
/// `--per-job` only on job boots. So the fill check must not depend on the
/// per-job flag (requiring it refused every real fill VM), while the
/// write key and a tip remain required, and job VMs still never queue fills.
#[cfg(target_os = "linux")]
#[test]
fn fill_vm_may_fill_only_with_a_write_key_and_tip() {
    let f = Fixture::new();
    for (per_job, key, tip, refusal) in [
        (false, Some(true), "tip", None),
        (true, Some(true), "tip", Some("remote cache is off")),
        // Which write-key reason applies depends on the fixture's cache setup.
        (false, Some(false), "tip", Some("")),
        (false, Some(true), "", Some("no default-branch tip")),
    ] {
        let mut config = f.config();
        config.manual_tick = true;
        config.tenant_vm = true;
        config.per_job = per_job;
        config.fill_vm = !per_job;
        config.tenant_write_key = key;
        let runner = Runner::open(config).unwrap();
        let got = runner.fill_vm_writer_refusal(tip);
        match refusal {
            None => assert_eq!(got, None, "{per_job} {key:?} {tip:?}"),
            Some(expected) => assert!(
                got.as_deref().is_some_and(|got| got.contains(expected)),
                "{per_job} {key:?} {tip:?}: {got:?}"
            ),
        }
        if per_job {
            // Job VMs still never queue fills of their own.
            assert!(!runner.warm_builds_enabled(tip));
        }
        drop(runner);
    }
}

#[test]
fn job_vm_preparation_cannot_select_a_second_workspace() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (first, _) = f.run("true");
    runner
        .claim_prepare_workspace(&first.workspace_key)
        .unwrap();
    runner
        .claim_prepare_workspace(&first.workspace_key)
        .unwrap();
    assert!(runner
        .claim_prepare_workspace("/home/another-workspace")
        .is_err());
    let mut other = first.clone();
    other.workspace_key.push_str("-other");
    assert!(matches!(
        runner.accept(other).unwrap(),
        Admission::Refused(RefusalReason::RequestRejected)
    ));
}

#[test]
fn tenant_mode_still_accepts_multiple_jobs_and_workspaces() {
    let f = Fixture::new();
    let runner = open(&f);
    let (first, _) = f.run("true");
    let mut other = first.clone();
    other.job_id = Uuid::now_v7();
    other.workspace_key.push_str("-other");
    other.cwd = other.workspace_key.clone();
    for request in [first, other] {
        assert!(matches!(
            runner.accept(request).unwrap(),
            Admission::Accepted(_)
        ));
    }
}

/// Firecracker exits on a guest reboot but not on a power-off, which only
/// halts the guest and leaves the VM holding its disk (ovh-2, 10 Nov 2026).
#[test]
fn job_vms_end_by_reboot_because_firecracker_ignores_poweroff() {
    let command = crate::tenant::job_vm_exit_command();
    assert_eq!(command.get_program(), "systemctl");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["reboot"]);
}

#[tokio::test]
async fn poweroff_waits_for_terminal_stored_ack_on_the_host_job_stream() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (request, bytes) = f.run("true");
    runner
        .tenant_events()
        .unwrap()
        .enable_reports(true, true)
        .unwrap();
    assert!(!runner.terminal_stored(request.job_id));
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(!runner.job_poweroff_ready());
    for correct in [false, true] {
        let (mut client, task) =
            connect(&runner, lease(request.job_id, LeasePurpose::Attach)).await;
        write_frame_async(
            &mut client,
            &Frame::Attach(AttachRequest::new(request.job_id, 0)),
        )
        .await
        .unwrap();
        loop {
            if matches!(frame(&mut client).await, Some(Frame::Terminal(_))) {
                break;
            }
        }
        assert!(!runner.job_poweroff_ready());
        assert!(
            !task.is_finished(),
            "terminal delivery alone must not end the drain"
        );
        write_frame_async(
            &mut client,
            &Frame::TerminalStored {
                jti: if correct {
                    request.job_id
                } else {
                    Uuid::now_v7()
                },
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!runner.job_poweroff_ready());
        if correct {
            pending(&runner);
            assert!(runner.job_poweroff_ready());
        }
    }
}

#[tokio::test]
async fn early_terminal_stored_ack_cannot_power_off_a_job_vm() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let id = Uuid::now_v7();
    let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
    write_frame_async(&mut client, &Frame::TerminalStored { jti: id })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!runner.job_poweroff_ready());
}

#[tokio::test]
async fn mirror_drain_skips_legacy_hosts_and_bounds_an_opted_in_host_without_acks() {
    for opted_in in [false, true] {
        let f = Fixture::new();
        let finished = Arc::new(tokio::sync::Notify::new());
        let mut config = job_config(&f);
        config.long_work = Some({
            let finished = finished.clone();
            Arc::new(move |label| {
                if label == "job_finish_complete" {
                    finished.notify_one();
                }
            })
        });
        let runner = Runner::open(config).unwrap();
        let (request, bytes) = f.run("true");
        runner
            .tenant_events()
            .unwrap()
            .enable_reports(true, opted_in)
            .unwrap();
        f.submit(&runner, &request, &bytes);
        runner.tick().unwrap();
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        assert!(runner.terminal_stored(request.job_id));
        // A terminal is published before the finisher's last state-lock holds.
        // Readiness conservatively returns false on contention, so wait for
        // finalization rather than race it while checking the exact deadline.
        tokio::time::timeout(Duration::from_secs(15), finished.notified())
            .await
            .expect("post-terminal finalization did not finish");
        assert_eq!(runner.job_poweroff_ready(), !opted_in);
        if opted_in {
            f.clock.fetch_add(9_999, Ordering::SeqCst);
            assert!(!runner.job_poweroff_ready());
            f.clock.fetch_add(1, Ordering::SeqCst);
            assert!(runner.job_poweroff_ready());
        }
        // Per-job boot clears lifecycle history but retains the mirror journal.
        let state = runner.config().state_dir();
        fs::remove_file(state.join("tenant-events.json")).unwrap();
        let next_boot = crate::tenant::EventLog::open_for_mode(&state, true).unwrap();
        next_boot.enable_reports(true, true).unwrap();
        let mut reports = 0;
        while let Some(event) = next_boot.first() {
            reports += usize::from(event.mirror_refs.is_some());
            next_boot.ack(event.event_seq).unwrap();
        }
        assert_eq!(reports, 2);
        let reopened = crate::tenant::EventLog::open_for_mode(&state, true).unwrap();
        reopened.enable_reports(true, true).unwrap();
        assert!(
            reopened.first().is_none(),
            "acked reports must not be re-minted"
        );
    }
}
