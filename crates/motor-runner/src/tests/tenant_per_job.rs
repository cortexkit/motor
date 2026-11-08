use super::*;

fn job_runner(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.per_job = true;
    Runner::open(config).unwrap()
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
        let runner = job_runner(&f);
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
