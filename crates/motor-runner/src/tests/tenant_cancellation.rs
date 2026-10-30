use super::*;
use std::os::unix::process::ExitStatusExt;
use std::time::Instant;

struct UnitFixture {
    runner: Runner,
    fixture: Fixture,
    id: Uuid,
    fail: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    stops: Arc<AtomicU64>,
}

impl UnitFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        let fail = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let stops = Arc::new(AtomicU64::new(0));
        let mut config = fixture.config();
        config.manual_tick = true;
        config.tenant_vm = true;
        let (failing, live, calls) = (fail.clone(), alive.clone(), stops.clone());
        // Substitute only systemctl, retaining the real host backend and tenant
        // frame handler so an ignored stop error would produce a false ack.
        config.systemd_command = Some(Arc::new(move |args| {
            let mut output = std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            match args[0] {
                "show" => {
                    output.stdout = if live.load(Ordering::SeqCst) {
                        b"LoadState=loaded\nActiveState=active\nSubState=running\n".to_vec()
                    } else {
                        b"LoadState=loaded\nActiveState=inactive\nSubState=dead\n".to_vec()
                    };
                }
                "stop" => {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if failing.load(Ordering::SeqCst) {
                        output.status = std::process::ExitStatus::from_raw(1 << 8);
                        output.stderr = b"control bus unavailable".to_vec();
                    }
                }
                "kill" | "reset-failed" => {}
                _ => panic!("unexpected systemctl: {args:?}"),
            }
            Ok(output)
        }));
        let runner = Runner::open(config).unwrap();
        let (mut run, _) = fixture.run("true");
        run.weight_hint = Some(2);
        run.timeout = Some(30);
        run.platform = None;
        run.network = None;
        let id = run.job_id;
        assert!(matches!(
            runner.accept(run).unwrap(),
            Admission::Accepted(_)
        ));
        runner.tenant_admitted(id).unwrap();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&id).unwrap();
            job.state = JobState::Running;
            job.started_ms = Some(runner.now());
            job.started = Some(Started::new(0, 0, runner.now()));
            job.next_seq = 1;
            job.process = Some(platform::ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(id),
            });
            runner.save(job).unwrap();
        }
        Self {
            runner,
            fixture,
            id,
            fail,
            alive,
            stops,
        }
    }

    async fn attach(&self) -> (DuplexStream, tokio::task::JoinHandle<io::Result<()>>) {
        let (mut client, task) = connect(&self.runner, lease(self.id, LeasePurpose::Attach)).await;
        motor_protocol::write_frame_async(
            &mut client,
            &Frame::Attach(AttachRequest::new(self.id, 0)),
        )
        .await
        .unwrap();
        assert!(
            matches!(frame(&mut client).await, Some(Frame::Started { job_id, .. }) if job_id == self.id)
        );
        (client, task)
    }
}

impl Drop for UnitFixture {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.runner
            .enforce_kills(Instant::now() + Duration::from_secs(3600))
            .unwrap();
    }
}

async fn cancel_frame(client: &mut DuplexStream, id: Uuid) {
    motor_protocol::write_frame_async(client, &Frame::Cancel(CancelRequest::new(id)))
        .await
        .unwrap();
}

async fn refused_stop(client: &mut DuplexStream, id: Uuid) {
    let reply = frame(client).await;
    assert!(
        matches!(&reply, Some(Frame::Refusal(refusal))
        if refusal.job_id == Some(id)
            && refusal.reason == RefusalReason::RequestRejected
            && refusal.detail.as_deref().is_some_and(|s| s.contains("control bus unavailable"))),
        "a failed stop must be named, not acknowledged: {reply:?}"
    );
}

async fn terminal_frame(client: &mut DuplexStream) -> TerminalRecord {
    loop {
        match frame(client).await {
            Some(Frame::Output(_)) => {}
            Some(Frame::Terminal(record)) => return record,
            other => panic!("expected output followed by terminal, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn tenant_cancel_stop_failure_is_refused_and_reattached_stream_survives_retry() {
    let f = UnitFixture::new();
    let (mut client, task) = f.attach().await;
    cancel_frame(&mut client, f.id).await;
    refused_stop(&mut client, f.id).await;
    let saved: Job = read_json(&f.runner.record_path(f.id)).unwrap();
    assert!(matches!(saved.kill, Some((Killed::Cancel, _))));
    motor_protocol::write_frame_async(&mut client, &Frame::JobStatus { job_id: f.id })
        .await
        .unwrap();
    assert!(
        matches!(frame(&mut client).await, Some(Frame::JobStatusReply(status))
        if status.state == "running" && status.unit_alive == Some(true))
    );

    f.fail.store(false, Ordering::SeqCst);
    cancel_frame(&mut client, f.id).await;
    assert!(
        matches!(frame(&mut client).await, Some(Frame::CancelReply(reply)) if reply.job_id == f.id)
    );
    assert_eq!(
        f.stops.load(Ordering::SeqCst),
        2,
        "a repeated cancel reissues the stop"
    );
    let (mut fresh, fresh_task) = connect(&f.runner, lease(f.id, LeasePurpose::Attach)).await;
    cancel_frame(&mut fresh, f.id).await;
    assert!(
        matches!(frame(&mut fresh).await, Some(Frame::CancelReply(reply)) if reply.job_id == f.id)
    );
    assert_eq!(
        f.stops.load(Ordering::SeqCst),
        3,
        "a new connection also reissues the stop"
    );
    drop(fresh);
    fresh_task.await.unwrap().unwrap();

    f.alive.store(false, Ordering::SeqCst);
    f.runner.enforce_kills(Instant::now()).unwrap();
    let record = terminal_frame(&mut client).await;
    assert_eq!(record.job_id, f.id);
    assert_eq!(record.killed, Some(Killed::Cancel));
    task.await.unwrap().unwrap();
    assert!(pending(&f.runner)
        .iter()
        .any(|event| event.event == TenantEventKind::Terminal));
}

#[tokio::test]
async fn tenant_failed_stops_retry_in_seconds_without_consuming_kill_budget() {
    let f = UnitFixture::new();
    // A fresh cancel connection and an attached stream use the same job intent.
    let (mut fresh, fresh_task) = connect(&f.runner, lease(f.id, LeasePurpose::Attach)).await;
    cancel_frame(&mut fresh, f.id).await;
    refused_stop(&mut fresh, f.id).await;
    drop(fresh);
    fresh_task.await.unwrap().unwrap();
    let (mut client, task) = f.attach().await;
    f.fixture.clock.fetch_add(2000, Ordering::SeqCst);
    let instant = Instant::now();
    for attempt in 0..6 {
        let due = instant + Duration::from_secs(2 * attempt);
        f.runner.enforce_kills(due).unwrap();
        assert_eq!(f.stops.load(Ordering::SeqCst), 2 + attempt);
        assert_eq!(
            f.runner.core.inner.lock().unwrap().kill_retries[&f.id],
            (0, due + Duration::from_secs(2)),
            "failed stops must not consume the kill budget"
        );
        f.runner
            .enforce_kills(due + Duration::from_millis(1999))
            .unwrap();
        assert_eq!(
            f.stops.load(Ordering::SeqCst),
            2 + attempt,
            "no busy retry loop"
        );
    }
    f.fail.store(false, Ordering::SeqCst);
    f.runner
        .enforce_kills(instant + Duration::from_secs(12))
        .unwrap();
    assert_eq!(f.stops.load(Ordering::SeqCst), 8);
    assert_eq!(f.runner.core.inner.lock().unwrap().kill_retries[&f.id].0, 1);
    f.alive.store(false, Ordering::SeqCst);
    f.runner
        .enforce_kills(instant + Duration::from_secs(13))
        .unwrap();
    assert_eq!(
        terminal_frame(&mut client).await.killed,
        Some(Killed::Cancel)
    );
    task.await.unwrap().unwrap();
}

#[cfg(target_os = "linux")]
async fn queued_run(
    f: &Fixture,
    runner: &Runner,
    command: &str,
) -> (Uuid, DuplexStream, tokio::task::JoinHandle<io::Result<()>>) {
    let (mut run, bundles) = f.run(command);
    run.weight_hint = Some(2);
    run.timeout = Some(30);
    run.queue_wait_limit_s = 0;
    run.platform = None;
    run.network = None;
    let id = run.job_id;
    let (mut client, task) = connect(runner, lease(id, LeasePurpose::Run)).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Run(run))
        .await
        .unwrap();
    assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
    for (index, bytes) in bundles.into_iter().enumerate() {
        motor_protocol::write_frame_async(
            &mut client,
            &Frame::BundleChunk {
                index: index as u32,
                offset: 0,
                data: BytePayload(bytes),
            },
        )
        .await
        .unwrap();
    }
    until(|| runner.job_status(id).unwrap().state == "queued").await;
    (id, client, task)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn tenant_reattached_cancel_stops_the_original_execution_and_streams_terminal() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    let (id, mut original, original_task) =
        queued_run(&f, &runner, "while :; do sleep 1; done").await;
    runner.tick().unwrap();
    assert!(
        matches!(frame(&mut original).await, Some(Frame::Started { job_id, .. }) if job_id == id)
    );
    drop(original);
    original_task.await.unwrap().unwrap();
    let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Attach)).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Attach(AttachRequest::new(id, 0)))
        .await
        .unwrap();
    assert!(
        matches!(frame(&mut client).await, Some(Frame::Started { job_id, .. }) if job_id == id)
    );
    cancel_frame(&mut client, id).await;
    assert!(
        matches!(frame(&mut client).await, Some(Frame::CancelReply(reply)) if reply.job_id == id)
    );
    assert!(
        matches!(frame(&mut client).await, Some(Frame::Terminal(record))
        if record.job_id == id && record.killed == Some(Killed::Cancel)
            && matches!(record.outcome, Outcome::Signal { .. }))
    );
    task.await.unwrap().unwrap();
    let saved: Job = read_json(&runner.record_path(id)).unwrap();
    let identity = saved.process.unwrap();
    assert!(
        !platform::identity(identity.pid).is_some_and(|p| p.start == identity.start),
        "the original execution must have exited, not merely recorded a cancel"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn tenant_cancel_on_another_connection_wins_queued_and_starting_races() {
    for starting in [false, true] {
        let f = Fixture::new();
        let hold = Hold::new("materialize");
        let release = Release(hold.clone());
        let mut config = f.config();
        config.manual_tick = true;
        config.tenant_vm = true;
        config.disable_warm_builds = true;
        config.long_work = Some(hold.hook());
        let runner = Runner::open(config).unwrap();
        let (id, mut original, original_task) =
            queued_run(&f, &runner, "touch must-not-start").await;
        if starting {
            hold.arm();
            runner.tick().unwrap();
            reached(&hold).await;
            assert!(runner.core.inner.lock().unwrap().starting.contains(&id));
        }
        let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Attach)).await;
        cancel_frame(&mut client, id).await;
        assert!(
            matches!(frame(&mut client).await, Some(Frame::CancelReply(reply)) if reply.job_id == id)
        );
        drop(client);
        task.await.unwrap().unwrap();
        drop(release);
        runner.tick().unwrap();
        assert!(
            matches!(frame(&mut original).await, Some(Frame::Terminal(record))
            if record.outcome == Outcome::Cancelled && record.ran == Some(Ran::None))
        );
        original_task.await.unwrap().unwrap();
        until(|| !runner.core.inner.lock().unwrap().starting.contains(&id)).await;
        assert!(!Path::new(&f.key).join("must-not-start").exists());
        let saved: Job = read_json(&runner.record_path(id)).unwrap();
        assert_eq!(saved.state, JobState::Tombstone);
        assert_eq!(saved.terminal.unwrap().outcome, Outcome::Cancelled);
    }
}
