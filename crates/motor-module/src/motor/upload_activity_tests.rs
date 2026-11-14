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
async fn capped_upload_keeps_live_progress_and_frame_order_then_completes() {
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
    } = upload(true, 180).await;
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
    for _ in 0..65 {
        tick().await;
        assert!(
            !uploading.is_finished(),
            "live host lost its backpressured upload"
        );
    }
    let mut waits = Vec::new();
    while let Ok(Ok(record)) = caller.try_recv() {
        if let StreamRecord::Progress(progress) = record {
            if progress.phase() == &ProgressPhase::QueueWait {
                waits.push(progress.waiting_on().cloned());
            }
        }
    }
    assert_eq!(
        waits.len(),
        65,
        "queue progress was delayed until after upload"
    );
    for (i, wait) in waits.into_iter().enumerate() {
        assert_eq!(
            wait,
            Some(if i % 2 == 0 {
                ProgressWaitingOn::CheckoutDisk
            } else {
                ProgressWaitingOn::Memory
            })
        );
    }
    tokio::time::resume();
    release.send(()).unwrap();
    let (result, mut channel) = uploading.await.unwrap();
    result.expect("heartbeats must keep the capped upload alive past 60 seconds");
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
    assert_eq!(sequence, 65);
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
