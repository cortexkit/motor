use super::*;

fn close_gate(runner: &Runner) {
    fs::write(
        runner.config().state_dir().join("qualification-required"),
        b"",
    )
    .unwrap();
}

async fn peer(runner: &Runner, qualification: bool) -> tokio::io::DuplexStream {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let runner = runner.clone();
    tokio::spawn(async move {
        crate::server::local_channel(runner, server, std::time::Instant::now(), qualification)
            .await
            .unwrap();
    });
    let hello: Frame = serde_json::from_value(serde_json::json!({
        "type": "hello", "protocol_version": PROTOCOL_VERSION, "started": true, "gate_jobs": true
    }))
    .unwrap();
    write_frame_async(&mut client, &hello).await.unwrap();
    assert!(matches!(
        read_frame_async(&mut client).await.unwrap(),
        Some(Frame::Hello { .. })
    ));
    client
}

async fn next_frame(client: &mut tokio::io::DuplexStream) -> Frame {
    loop {
        match read_frame_async(client).await.unwrap().unwrap() {
            Frame::Heartbeat { .. } => continue,
            frame => return frame,
        }
    }
}

#[tokio::test]
async fn qualification_gate_refuses_fleet_before_admission_even_with_gate_job_claim() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    close_gate(&runner);
    let (mut request, _) = f.run("touch must-not-run");
    request.gate_job = true;
    let id = request.job_id;
    let mut client = peer(&runner, false).await;
    write_frame_async(&mut client, &Frame::Run(request))
        .await
        .unwrap();
    let Frame::Refusal(refusal) = next_frame(&mut client).await else {
        panic!("expected pre-admission refusal")
    };
    assert_eq!(refusal.reason, RefusalReason::RunnerFull);
    assert_eq!(
        refusal.detail.as_deref(),
        Some("runner awaiting qualification")
    );
    let status = runner.job_status(id).unwrap();
    let terminal = status.terminal.unwrap();
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    let health = runner.runner_status().health.unwrap();
    assert_eq!(
        health.admission_blocked.as_deref(),
        Some("runner awaiting qualification")
    );
    assert_eq!(health.counts.accepted, 0);
}

#[tokio::test]
async fn qualification_gate_runs_a_local_qualification_job_while_fleet_is_closed() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    close_gate(&runner);
    let (request, bytes) = f.run("printf qualification-ran");
    let mut client = peer(&runner, true).await;
    write_frame_async(&mut client, &Frame::Run(request.clone()))
        .await
        .unwrap();
    assert!(matches!(next_frame(&mut client).await, Frame::Accepted(_)));
    for (index, data) in bytes.into_iter().enumerate() {
        write_frame_async(
            &mut client,
            &Frame::BundleChunk {
                index: index as u32,
                offset: 0,
                data: BytePayload(data),
            },
        )
        .await
        .unwrap();
    }
    until(|| runner.core.inner.lock().unwrap().jobs[&request.job_id].state == JobState::Queued)
        .await;
    runner.tick().unwrap();
    let terminal = terminal(&runner, request.job_id).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    assert_eq!(job_output(&runner, request.job_id), "qualification-ran");
    assert!(
        runner.qualification_required(),
        "running a qualification job alone must not open admission"
    );
}

#[tokio::test]
async fn qualification_gate_survives_restart_drain_and_ordinary_resume_until_privileged_resume() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config.clone()).unwrap();
    close_gate(&runner);
    drop(runner);
    let runner = reopen(config);
    assert!(runner.qualification_required());
    assert_eq!(runner.drain_if_idle(), (None, 0));
    let mut client = peer(&runner, false).await;
    write_frame_async(&mut client, &Frame::Resume)
        .await
        .unwrap();
    assert!(matches!(
        next_frame(&mut client).await,
        Frame::ResumeReply { resumed: false }
    ));
    assert!(runner.qualification_required());
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::RunnerFull)
    ));
    let mut client = peer(&runner, true).await;
    write_frame_async(&mut client, &Frame::Resume)
        .await
        .unwrap();
    assert!(matches!(
        next_frame(&mut client).await,
        Frame::ResumeReply { resumed: true }
    ));
    assert!(!runner.qualification_required());
    assert_eq!(
        runner.runner_status().health.unwrap().admission_blocked,
        None
    );
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(_)
    ));
}
