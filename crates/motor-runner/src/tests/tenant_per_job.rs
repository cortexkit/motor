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
        (true, Some(true), "tip", None),
        // Which write-key reason applies depends on the fixture's cache setup.
        (false, Some(false), "tip", Some("")),
        (false, Some(true), "", Some("no default-branch tip")),
    ] {
        let mut config = f.config();
        config.manual_tick = true;
        config.tenant_vm = true;
        config.per_job = per_job;
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
