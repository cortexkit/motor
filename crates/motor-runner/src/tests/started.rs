use super::*;

#[tokio::test]
async fn started_is_recorded_only_after_setup_with_capacity_wait() {
    let f = Fixture::new();
    let (runner, hold, _release) = held_runner(&f);
    warm(&f, &runner).await;
    hold.arm();
    let (request, bytes) = f.run("printf started-output");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    // Hold dispatch, then advance only the runner clock to measure permit wait.
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .dropping
        .insert(f.key.clone());
    let spool = f.spool(&runner, &request, &bytes);
    runner.fetch_run(&request, &spool.directory, true).unwrap();
    f.clock.fetch_add(25, Ordering::SeqCst);
    runner.core.inner.lock().unwrap().dropping.remove(&f.key);
    runner.tick().unwrap();
    reached(&hold).await;
    assert!(
        runner.replay(request.job_id, 0).unwrap().is_empty(),
        "setup is not command start"
    );
    f.clock.fetch_add(500_000, Ordering::SeqCst);
    let command_start = f.clock.load(Ordering::SeqCst);
    hold.release();
    let result = terminal(&runner, request.job_id).await;
    assert_eq!(result.outcome, Outcome::Exit { code: 0 });
    assert_eq!(result.queue_wait_ms, 25);
    let wait = runner
        .runner_status()
        .health
        .unwrap()
        .queue_wait_1h
        .unwrap();
    // The warm build contributes a zero-wait sample alongside this job.
    assert_eq!(wait.p50_ms, 0);
    assert_eq!(wait.p90_ms, 25);
    assert_eq!(wait.refusals, 0);
    let frames = runner.replay(request.job_id, 0).unwrap();
    assert_eq!(
        frames.first(),
        Some(&Frame::Started {
            job_id: request.job_id,
            seq: 0,
            queue_wait_ms: 25,
            started_at_ms: command_start,
            path_mapping: None,
        })
    );
    assert!(matches!(&frames[1], Frame::Output(o) if o.seq == 1 && o.bytes.0 == b"started-output"));
    assert!(matches!(frames.last(), Some(Frame::Terminal(_))));
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&request.job_id].started_ms,
        Some(command_start)
    );
}

#[tokio::test]
async fn started_replay_is_inclusive_durable_and_not_repeated() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let (request, bytes) = f.run("printf replay-output");
    f.submit(&runner, &request, &bytes);
    terminal(&runner, request.job_id).await;
    base_done(&runner).await;
    let before = runner.replay(request.job_id, 0).unwrap();
    assert!(matches!(
        before.first(),
        Some(Frame::Started { seq: 0, .. })
    ));
    assert_eq!(
        before
            .iter()
            .filter(|f| matches!(f, Frame::Started { .. }))
            .count(),
        1
    );
    assert!(matches!(&before[1], Frame::Output(o) if o.seq == 1));
    let after = runner.replay(request.job_id, 1).unwrap();
    assert_eq!(after, before[1..]);
    drop(runner);
    let runner = reopen(config);
    assert_eq!(runner.replay(request.job_id, 0).unwrap(), before);
    assert_eq!(runner.replay(request.job_id, 1).unwrap(), after);
}

#[tokio::test]
async fn live_started_frame_is_sent_once_before_output() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let (socket, server) = listener(&f, &runner);
    let (request, bytes) =
        f.run("while test ! -f release-output; do sleep 0.02; done; printf live-output");
    let mut stream = connect(&socket).await;
    write_frame_async(&mut stream, &Frame::Run(request.clone()))
        .await
        .unwrap();
    assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .dropping
        .insert(f.key.clone());
    chunks(&mut stream, &bytes).await;
    until(|| {
        runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .queued_ms
            .is_some()
    })
    .await;
    f.clock.fetch_add(25, Ordering::SeqCst);
    runner.core.inner.lock().unwrap().dropping.remove(&f.key);
    let started = read_next(&mut stream).await;
    assert!(
        matches!(started, Frame::Started { job_id, seq: 0, queue_wait_ms: 25, .. } if job_id == request.job_id)
    );
    // Allow several replay polls while the command has no output to advance past.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), read_next(&mut stream))
            .await
            .is_err(),
        "start progress must not repeat on a live stream"
    );
    fs::write(Path::new(&f.key).join("release-output"), "go").unwrap();
    let mut output = Vec::new();
    let mut seq = 1;
    loop {
        match read_next(&mut stream).await {
            Frame::Output(o) => {
                assert_eq!(o.seq, seq);
                seq += 1;
                output.extend(o.bytes.0);
            }
            Frame::Terminal(t) => {
                assert_eq!(t.outcome, Outcome::Exit { code: 0 });
                assert_eq!(t.queue_wait_ms, 25);
                break;
            }
            other => panic!("unexpected frame after Started: {other:?}"),
        }
    }
    assert_eq!(output, b"live-output");
    server.abort();
}

#[tokio::test]
async fn refused_before_start_has_no_started_record() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("echo must-not-run > marker");
    request.tree_hash = "0".repeat(40);
    f.submit(&runner, &request, &bytes);
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TreeHashMismatch
        }
    );
    assert_eq!(t.ran, Some(Ran::None));
    assert_eq!(
        runner.replay(request.job_id, 0).unwrap(),
        vec![Frame::Terminal(t)]
    );
    assert!(!Path::new(&f.key).join("marker").exists());
}

#[test]
fn older_job_records_do_not_synthesize_started() {
    let id = Uuid::now_v7();
    let mut raw = serde_json::to_value(Job::new(id, None, 100, 0, 0)).unwrap();
    raw.as_object_mut().unwrap().remove("started");
    raw["started_ms"] = serde_json::json!(125);
    let job: Job = serde_json::from_value(raw).unwrap();
    assert!(job.started.is_none());
    assert_eq!(job.started_ms, Some(125));
}

#[test]
fn legacy_replay_projects_cursors_and_real_truncation_but_not_terminal() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&id).unwrap();
        job.started = Some(Started::new(0, 0, 125));
        job.next_seq = 1;
    }
    for bytes in [b"a", b"b", b"c"] {
        runner.output(id, OutputStream::Stdout, bytes).unwrap();
    }
    let native = runner.replay(id, 0).unwrap();
    let legacy = runner.replay_legacy(id, 0).unwrap();
    assert_eq!(legacy.len(), 4);
    for (seq, frame) in legacy[..3].iter().enumerate() {
        assert!(
            matches!(frame, Frame::Output(o) if o.seq == seq as u64 && o.truncated_before_seq.is_none())
        );
    }
    assert_eq!(
        legacy.last(),
        native.last(),
        "Terminal has no sequence fields"
    );
    assert_eq!(runner.replay_legacy(id, 1).unwrap(), legacy[1..]);
    assert_eq!(runner.replay_legacy(id, 3).unwrap(), legacy[3..]);
    assert_eq!(
        runner.replay_legacy(id, u64::MAX).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    // Removing the first retained output simulates eviction, not a missing
    // Started record. The legacy replay must still show this gap in the sequence
    // numbers the legacy caller sees, rather than hiding it when they are shifted.
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .chunks
        .pop_front();
    assert!(
        matches!(&runner.replay_legacy(id, 0).unwrap()[0], Frame::Output(o) if o.seq == 1 && o.truncated_before_seq == Some(1))
    );
    assert!(
        matches!(&runner.replay_legacy(id, 1).unwrap()[0], Frame::Output(o) if o.seq == 1 && o.truncated_before_seq.is_none())
    );
}

#[test]
fn legacy_replay_is_identity_for_refused_and_pre_started_records() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    assert_eq!(
        runner.replay_legacy(id, 0).unwrap(),
        runner.replay(id, 0).unwrap()
    );
    runner
        .output(id, OutputStream::Stdout, b"old-output")
        .unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .started_ms = Some(125);
    for from in [0, 1, u64::MAX] {
        assert_eq!(
            runner.replay_legacy(id, from).unwrap(),
            runner.replay(id, from).unwrap()
        );
    }
    assert!(matches!(&runner.replay_legacy(id, 0).unwrap()[0], Frame::Output(o) if o.seq == 0));
}

#[test]
fn legacy_replay_rejects_started_that_is_not_first() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    runner.cancel(id).unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .started = Some(Started::new(1, 0, 125));
    assert_eq!(
        runner.replay_legacy(id, 0).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .get_mut(&id)
        .unwrap()
        .started = Some(Started::new(0, 0, 125));
    runner
        .output(id, OutputStream::Stdout, b"invalid-seq-zero")
        .unwrap();
    assert_eq!(
        runner.replay_legacy(id, 0).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

struct FaultBackend {
    stage: &'static str,
    launched: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
}
struct FaultExecution {
    inner: Box<dyn backend::Execution>,
    stage: &'static str,
    reaped: Arc<AtomicBool>,
}
impl backend::Execution for FaultExecution {
    fn release(&mut self) -> io::Result<()> {
        self.inner.release()?;
        if self.stage == "release" {
            Err(io::Error::other("injected release failure"))
        } else {
            Ok(())
        }
    }
    fn output(&self) -> io::Result<backend::Output> {
        if self.stage == "output" {
            Err(io::Error::other("injected output failure"))
        } else {
            self.inner.output()
        }
    }
    fn wait(&mut self) -> io::Result<backend::ExecutionOutcome> {
        self.inner.wait()
    }
    fn abort(&self) -> io::Result<()> {
        self.inner.abort()
    }
    fn reap(&self) {
        self.inner.reap();
        self.reaped.store(true, Ordering::SeqCst);
    }
}
impl Backend for FaultBackend {
    fn materialize(&self, c: &Config, t: &Trees, s: &Setup) -> io::Result<Prepared> {
        backend::HostBackend.materialize(c, t, s)
    }
    fn launch(
        &self,
        c: &Config,
        t: &Trees,
        s: &Setup,
        p: &Prepared,
        w: u32,
    ) -> io::Result<backend::Launch> {
        let mut launch = backend::HostBackend.launch(c, t, s, p, w)?;
        launch.execution = Box::new(FaultExecution {
            inner: launch.execution,
            stage: self.stage,
            reaped: self.reaped.clone(),
        });
        self.launched.store(true, Ordering::SeqCst);
        Ok(launch)
    }
    fn cancel(&self, c: &Config, j: &Job, hard: bool) -> io::Result<()> {
        backend::HostBackend.cancel(c, j, hard)
    }
    fn unit_alive(
        &self,
        c: &Config,
        p: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        backend::HostBackend.unit_alive(c, p)
    }
    fn collect_changes(&self, t: &Trees, j: &Job) -> io::Result<Option<Vec<String>>> {
        if self.stage == "collect" {
            Err(io::Error::other("injected change collection failure"))
        } else {
            backend::HostBackend.collect_changes(t, j)
        }
    }
    fn reconcile(&self, c: &Config, jobs: &[Job]) -> io::Result<Vec<Uuid>> {
        backend::HostBackend.reconcile(c, jobs)
    }
}

async fn launch_fault(stage: &'static str) {
    launch_fault_inner(stage, false).await;
}

async fn launch_fault_inner(stage: &'static str, isolated: bool) {
    let f = Fixture::new();
    let launched = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicBool::new(false));
    let hold = Hold::new("launch-cleanup");
    let _release = Release(hold.clone());
    hold.arm();
    let mut config = f.config();
    // Dispatch explicitly so a scheduler tick cannot race the cleanup lock probe.
    config.manual_tick = true;
    config.disable_warm_builds = true;
    config.isolate_jobs = isolated;
    config.long_work = Some(hold.hook());
    if stage == "save" {
        config.before_job_write = Some({
            let launched = launched.clone();
            Arc::new(move |_| {
                if launched.swap(false, Ordering::SeqCst) {
                    Err(io::Error::from_raw_os_error(libc::ENOSPC))
                } else {
                    Ok(())
                }
            })
        });
    }
    let runner = Runner::open_with_backend(
        config,
        Box::new(FaultBackend {
            stage,
            launched,
            reaped: reaped.clone(),
        }),
    )
    .unwrap();
    let (request, bytes) = f.run("sleep 60");
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    reached(&hold).await;
    {
        let inner = runner
            .core
            .inner
            .try_lock()
            .expect("cleanup must not hold the global state lock");
        assert!(Runner::occupied(&inner, &f.key));
        assert!(inner.finalizing.contains(&request.job_id));
        assert!(!reaped.load(Ordering::SeqCst));
    }
    hold.release();
    let result = terminal(&runner, request.job_id).await;
    assert!(reaped.load(Ordering::SeqCst));
    if cfg!(target_os = "linux") && stage != "release" {
        assert_eq!(
            result.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::WorkspaceSetupFailed
            }
        );
        assert_eq!(
            result.refusal_detail(),
            Some(if stage == "save" {
                "execution gate was not opened: start_not_persisted"
            } else {
                "execution gate was not opened: output_unavailable"
            })
        );
        assert_eq!(result.retry_after_ms(), None);
        assert_eq!(result.ran, Some(Ran::None));
        assert_execution_ended_wire(&result, true);
        assert_eq!(
            runner.replay(request.job_id, 0).unwrap(),
            vec![Frame::Terminal(result)]
        );
    } else {
        assert_eq!(result.outcome, Outcome::OutcomeUnknown);
        assert_execution_ended_wire(&result, false);
    }
    let inner = runner.core.inner.lock().unwrap();
    assert!(!Runner::occupied(&inner, &f.key));
    assert!(!inner.starting.contains(&request.job_id));
    assert!(!inner.finalizing.contains(&request.job_id));
}

#[tokio::test]
async fn started_write_failure_stops_and_reaps_unreleased_execution() {
    launch_fault("save").await;
}
#[tokio::test]
async fn output_handle_failure_stops_and_reaps_unreleased_execution() {
    launch_fault("output").await;
}
#[tokio::test]
async fn partial_release_failure_stops_and_reaps_without_no_start_claim() {
    launch_fault("release").await;
}

#[tokio::test]
async fn change_collection_failure_still_persists_terminal_and_releases_owner() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let runner = Runner::open_with_backend(
        config,
        Box::new(FaultBackend {
            stage: "collect",
            launched: Arc::new(AtomicBool::new(false)),
            reaped: Arc::new(AtomicBool::new(false)),
        }),
    )
    .unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    let result = terminal(&runner, request.job_id).await;
    assert_eq!(result.outcome, Outcome::Exit { code: 0 });
    assert_eq!(result.workspace_changes, Some(vec![]));
    let saved: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(saved.terminal, Some(result));
    let inner = runner.core.inner.lock().unwrap();
    assert!(!Runner::occupied(&inner, &f.key));
    assert!(!inner.finalizing.contains(&request.job_id));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-job account, ACL tools and systemd private PID support"]
async fn isolated_post_launch_faults_stop_services_before_releasing_ownership() {
    for stage in ["save", "release"] {
        launch_fault_inner(stage, true).await;
    }
}
