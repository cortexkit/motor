use super::*;
use cortexkit_test_support::ScratchDir;
use tokio::sync::oneshot;

struct Upload {
    _scratch: ScratchDir,
    motor: Motor,
    snapshots: Snapshots,
    bundles: Vec<BundleManifest>,
    channel: Channel,
    id: Uuid,
    capped: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    host: tokio::task::JoinHandle<()>,
}

async fn upload(heartbeats: bool, budget: u64) -> Upload {
    upload_with_checkpoints(heartbeats, budget, None).await
}

const CHECKPOINTS: [u64; 2] = [1024 * 1024, 2 * 1024 * 1024];

async fn upload_with_checkpoints(
    heartbeats: bool,
    budget: u64,
    checkpoints: Option<(mpsc::Sender<u64>, mpsc::Receiver<()>)>,
) -> Upload {
    let scratch = ScratchDir::new("upload-peer-activity");
    let repo = super::queue_tests::fixture_repo(scratch.path());
    let motor = Motor::open(Config::default(), &scratch.join("state")).unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, budget, None, None, false).unwrap();
    let path = repo.to_str().unwrap();
    let snapshots = Snapshots::build(
        &motor.config,
        path,
        path,
        &[],
        scratch.path(),
        id,
        None,
        &motor.storage,
    )
    .unwrap();
    // Use the production upload loop and 64 MiB cap, not a large-frame writer
    // shortcut. Sparse zero bytes keep the fixture independent of Git compression.
    let length = motor_host::jobs::SPOOL_LIMIT + 65536;
    std::fs::create_dir_all(&snapshots.spool.0).unwrap();
    std::fs::File::create(snapshots.spool.0.join("bundle-0"))
        .unwrap()
        .set_len(length)
        .unwrap();
    let bundle: BundleManifest = serde_json::from_value(serde_json::json!({
        "index": 0, "repository": path, "length": length, "sha256": "0".repeat(64),
    }))
    .unwrap();
    let run: Run = serde_json::from_value(serde_json::json!({
        "job_id": id, "workspace_key": path, "repository_root": path, "cwd": path,
        "command": "true", "env": {}, "queue_wait_limit_s": budget,
        "snapshot_commit": "0".repeat(40), "tree_hash": "0".repeat(40),
        "integration_tip": "0".repeat(40), "siblings": [], "bundles": [bundle],
    }))
    .unwrap();
    let (client, mut peer) = tokio::io::duplex(4096);
    let (capped_tx, capped) = oneshot::channel();
    let (release, mut released) = oneshot::channel();
    let host = tokio::spawn(async move {
        let hello = read_frame_async(&mut peer).await.unwrap().unwrap();
        write_frame_async(&mut peer, &hello).await.unwrap();
        assert!(matches!(
            read_frame_async(&mut peer).await.unwrap().unwrap(),
            Frame::Run(_)
        ));
        write_frame_async(&mut peer, &Frame::Accepted(Accepted::new(id, 0)))
            .await
            .unwrap();
        let mut bytes = Vec::new();
        let mut spooled = 0;
        let mut offset = 0;
        let mut checkpoints = checkpoints;
        while offset < length {
            let frame = read_frame_async(&mut peer).await.unwrap().unwrap();
            bytes.clear();
            write_frame(&mut bytes, &frame).unwrap();
            spooled += bytes.len() as u64;
            match frame {
                Frame::BundleChunk {
                    index,
                    offset: at,
                    data,
                } => {
                    assert_eq!(index, 0);
                    assert_eq!(at, offset);
                    assert!(data.0.iter().all(|b| *b == 0));
                    offset += data.0.len() as u64;
                    if let Some((arrived, resume)) = &mut checkpoints {
                        if CHECKPOINTS.contains(&offset) {
                            arrived.send(offset).await.unwrap();
                            resume.recv().await.unwrap();
                        }
                    }
                }
                Frame::Heartbeat { .. } => {}
                other => panic!("unexpected upload frame: {other:?}"),
            }
            if spooled + motor_protocol::MAX_FRAME_BYTES as u64 + 4 > motor_host::jobs::SPOOL_LIMIT
            {
                break;
            }
        }
        assert!(offset < length, "upload must exceed the host spool cap");
        capped_tx.send(()).unwrap();
        if checkpoints.is_some() {
            let status = serde_json::from_value(serde_json::json!({
                "state": "queued", "position": 3, "waiting_on": "checkout disk",
            }))
            .unwrap();
            write_frame_async(
                &mut peer,
                &Frame::Heartbeat {
                    queue_wait_ms: Some(0),
                    queue_status: Some(status),
                },
            )
            .await
            .unwrap();
        }
        let mut sequence = 0;
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        loop {
            tokio::select! {
                _ = &mut released => break,
                _ = heartbeat.tick(), if heartbeats => {
                    let status = serde_json::from_value(serde_json::json!({
                        "state": "queued", "waiting_on": if sequence % 2 == 0 { "checkout disk" } else { "memory" },
                    })).unwrap();
                    write_frame_async(&mut peer, &Frame::Heartbeat {
                        queue_wait_ms: Some(sequence), queue_status: Some(status),
                    }).await.unwrap();
                    sequence += 1;
                }
            }
        }
        while offset < length {
            match read_frame_async(&mut peer).await.unwrap().unwrap() {
                Frame::BundleChunk {
                    offset: at, data, ..
                } => {
                    assert_eq!(at, offset);
                    assert!(data.0.iter().all(|b| *b == 0));
                    offset += data.0.len() as u64;
                }
                Frame::Heartbeat { .. } => {}
                other => panic!("unexpected upload frame: {other:?}"),
            }
        }
        assert_eq!(offset, length);
        write_frame_async(
            &mut peer,
            &Frame::Terminal(TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0)),
        )
        .await
        .unwrap();
    });
    let mut channel = Channel::from_hosted(Box::new(client), false).await.unwrap();
    channel.send(Frame::Run(run)).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    Upload {
        _scratch: scratch,
        motor,
        snapshots,
        bundles: vec![bundle],
        channel,
        id,
        capped,
        release,
        host,
    }
}

async fn tick() {
    tokio::time::advance(Duration::from_millis(1001)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn flowing_bundle_reports_growing_upload_bytes_then_queue_wait() {
    let (arrived, mut checkpoint) = mpsc::channel(1);
    let (resume, resumes) = mpsc::channel(1);
    let Upload {
        _scratch,
        motor,
        snapshots,
        bundles,
        mut channel,
        id,
        capped,
        release,
        host,
    } = upload_with_checkpoints(false, 180, Some((arrived, resumes))).await;
    let reporter = progress::Reporter::new(true, 1).with_job_id(id);
    let _submission = motor.observe_health_submission(id, Platform::Linux, reporter.clone());
    let (records_tx, records_rx) = mpsc::channel(8);
    let (caller_tx, mut caller) = mpsc::channel(8);
    let reporting = tokio::spawn(async move {
        reporter
            .forward(std::future::pending(), records_rx, &caller_tx)
            .await
    });
    records_tx
        .send(Ok(StreamRecord::Accepted(Accepted::new(id, 0))))
        .await
        .unwrap();
    assert!(matches!(
        caller.recv().await,
        Some(Ok(StreamRecord::Accepted(_)))
    ));
    let uploading = tokio::spawn({
        let motor = motor.clone();
        async move {
            motor
                .upload(&mut channel, &snapshots, &bundles, Some(id))
                .await
                .unwrap();
            channel
        }
    });
    for (count, (delay_s, detail)) in CHECKPOINTS
        .into_iter()
        .zip([(2, "uploading 1 of 65 MB"), (10, "uploading 2 of 65 MB")])
    {
        assert_eq!(checkpoint.recv().await, Some(count));
        tokio::time::pause();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(delay_s)).await;
        let update = tokio::time::timeout(Duration::from_millis(100), caller.recv())
            .await
            .expect("the live bundle upload must report its byte count");
        assert!(matches!(update, Some(Ok(StreamRecord::Progress(p)))
            if p.phase() == &ProgressPhase::Upload && p.detail() == Some(detail)));
        tokio::time::resume();
        resume.send(()).await.unwrap();
    }
    capped.await.unwrap();
    release.send(()).unwrap();
    let mut channel = uploading.await.unwrap();
    while let Incoming::Frame(Frame::Heartbeat { queue_status, .. }) =
        channel.next_job().await.unwrap()
    {
        if let Some(status) = queue_status {
            motor.report_queue_status(id, &status);
        }
    }
    let update = tokio::time::timeout(Duration::from_secs(5), caller.recv())
        .await
        .expect("queue status must be visible again after upload");
    assert!(matches!(update, Some(Ok(StreamRecord::Progress(p)))
        if p.phase() == &ProgressPhase::QueueWait
            && p.waiting_on() == Some(&ProgressWaitingOn::CheckoutDisk)));
    host.await.unwrap();
    reporting.abort();
    let _ = reporting.await;
}

#[tokio::test]
async fn capped_upload_survives_its_queue_budget_with_live_host_progress() {
    const BEATS: usize = 100;
    let Upload {
        _scratch,
        motor,
        snapshots,
        bundles,
        mut channel,
        id,
        capped,
        release,
        host,
    } = upload(true, 60).await;
    let reporter = progress::Reporter::new(true, 1).with_job_id(id);
    let _submission = motor.observe_health_submission(id, Platform::Linux, reporter.clone());
    let (records_tx, records_rx) = mpsc::channel(8);
    let (caller_tx, mut caller) = mpsc::channel(256);
    let reporting = tokio::spawn(async move {
        reporter
            .forward(std::future::pending(), records_rx, &caller_tx)
            .await
    });
    records_tx
        .send(Ok(StreamRecord::Accepted(Accepted::new(id, 0))))
        .await
        .unwrap();
    let uploading = tokio::spawn(async move {
        let result = motor
            .upload(&mut channel, &snapshots, &bundles, Some(id))
            .await;
        (result, channel)
    });
    capped.await.unwrap();
    tokio::time::pause();
    // More than 16 heartbeats also exceeds Channel's bounded inbox. They must
    // reach the progress reporter while the upload is blocked, not just later.
    for _ in 0..BEATS {
        tick().await;
        assert!(
            !uploading.is_finished(),
            "live host lost its backpressured upload"
        );
    }
    // The last heartbeats can still be crossing the host fixture's socket and
    // the reporter task when the clock stops; keep yielding (without advancing
    // time or releasing the upload) until they arrive, so a loaded machine
    // cannot turn scheduling delay into a false "delayed until after upload".
    // The upload reads its bundle with tokio::fs, on blocking-pool OS threads,
    // and reads heartbeats only while it is inside send_upload. So a count of
    // yields is no time bound. Bound the wait by wall-clock time.
    let mut waits = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        while let Ok(Ok(record)) = caller.try_recv() {
            if let StreamRecord::Progress(progress) = record {
                if progress.phase() == &ProgressPhase::QueueWait {
                    waits.push(progress.waiting_on().cloned());
                }
            }
        }
        if waits.len() >= BEATS {
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !uploading.is_finished(),
        "progress must arrive while the upload is still held"
    );
    // Queue waits are display progress, which the reporter coalesces into one
    // slot when it falls behind, so a loaded machine can skip a pair of
    // alternating waits. More waits than the 16-frame inbox can hold proves the
    // heartbeats were read during the blocked upload rather than buffered until
    // after it; the ordered heartbeat frames are checked one by one below.
    assert!(
        waits.len() > 16 && waits.len() <= BEATS,
        "queue progress was delayed until after upload: {} of {BEATS} waits",
        waits.len()
    );
    assert!(waits.iter().all(|wait| matches!(
        wait,
        Some(ProgressWaitingOn::CheckoutDisk | ProgressWaitingOn::Memory)
    )));
    for pair in waits.windows(2) {
        assert_ne!(pair[0], pair[1], "only a changed wait is reported");
    }
    tokio::time::resume();
    release.send(()).unwrap();
    let (result, mut channel) = uploading.await.unwrap();
    result.expect("host heartbeats must keep the capped upload alive past its queue budget");
    let mut sequence = 0;
    loop {
        match channel.next_job().await.unwrap() {
            Incoming::Frame(Frame::Heartbeat {
                queue_wait_ms: Some(at),
                ..
            }) => {
                assert_eq!(at, sequence, "upload reordered or dropped a heartbeat");
                sequence += 1;
            }
            Incoming::Frame(Frame::Terminal(terminal)) => {
                assert_eq!(terminal.job_id, id);
                assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
                break;
            }
            other => panic!("unexpected job frame: {other:?}"),
        }
    }
    assert_eq!(sequence, BEATS as u64);
    host.await.unwrap();
    reporting.abort();
    let _ = reporting.await;
}

#[tokio::test]
async fn capped_upload_to_silent_host_still_times_out() {
    let Upload {
        _scratch,
        motor,
        snapshots,
        bundles,
        mut channel,
        id,
        capped,
        release,
        host,
    } = upload(false, 180).await;
    let uploading = tokio::spawn(async move {
        motor
            .upload(&mut channel, &snapshots, &bundles, Some(id))
            .await
    });
    capped.await.unwrap();
    tokio::time::pause();
    for _ in 0..61 {
        tick().await;
    }
    let error = uploading
        .await
        .unwrap()
        .expect_err("a silent host must time out");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(
        matches!(
            error.to_string().as_str(),
            "write inactivity" | "flush inactivity"
        ),
        "{error}"
    );
    host.abort();
    let _ = host.await;
    drop(release);
}
