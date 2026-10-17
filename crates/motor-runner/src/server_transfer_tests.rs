use super::*;
use cortexkit_test_support::ScratchDir;
use sha2::{Digest, Sha256};

fn fixture() -> (ScratchDir, Runner, Run, Vec<u8>) {
    let scratch = ScratchDir::new("motor-transfer-clock");
    let mut config = crate::Config::new(scratch.join("state"), "transfer-clock");
    config.free_space_floor_bytes = 0;
    config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
    let runner = Runner::open(config).unwrap();
    let bytes = vec![42; 21];
    let key = scratch
        .join("remote/worktrees/task")
        .to_string_lossy()
        .into_owned();
    let root = scratch
        .join("remote/source/main")
        .to_string_lossy()
        .into_owned();
    let request = Run {
        gate_job: false,
        network: None,
        platform: None,
        workspace_kind: None,
        job_id: Uuid::now_v7(),
        workspace_key: key.clone(),
        repository_root: root.clone(),
        cwd: key,
        command: "true".into(),
        env: Default::default(),
        weight_hint: None,
        timeout: None,
        queue_wait_limit_s: 1,
        snapshot_commit: "1".repeat(40),
        tree_hash: "2".repeat(40),
        integration_tip: "3".repeat(40),
        siblings: vec![],
        bundles: vec![BundleManifest {
            index: 0,
            repository: root,
            length: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        }],
        git_dependencies: vec![],
        base_commit: None,
    };
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    (scratch, runner, request, bytes)
}

#[tokio::test(start_paused = true)]
async fn upload_stall_bound_ignores_heartbeats_and_empty_chunks() {
    let (_scratch, runner, request, _) = fixture();
    let (tx, mut rx) = mpsc::channel(8);
    let sender = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if tx
                .send(Frame::Heartbeat {
                    queue_wait_ms: None,
                })
                .await
                .is_err()
            {
                return;
            }
            if tx
                .send(Frame::BundleChunk {
                    index: 0,
                    offset: 0,
                    data: BytePayload(vec![]),
                })
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(61),
        upload(&runner, request.job_id, &request.bundles, &mut rx, None),
    )
    .await;
    let (reason, error) = match result {
        Ok(Err(failure)) => failure,
        _ => panic!("expected the transfer stall bound to refuse the job"),
    };
    assert_eq!(reason, RefusalReason::TransferInterrupted);
    assert_eq!(start.elapsed(), Duration::from_secs(60));
    runner.refuse(request.job_id, reason, error).unwrap();
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TransferInterrupted
        }
    ));
    assert!(terminal
        .refusal_detail()
        .unwrap()
        .starts_with("transfer_stalled: "));
    assert_eq!(terminal.retry_after_ms(), None);
    sender.abort();
}

#[tokio::test(start_paused = true)]
async fn upload_rejection_preserves_the_underlying_chunk_error() {
    let (_scratch, runner, request, _) = fixture();
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(Frame::BundleChunk {
        index: 99,
        offset: 0,
        data: BytePayload(vec![1]),
    })
    .await
    .unwrap();
    let (reason, error) =
        match upload(&runner, request.job_id, &request.bundles, &mut rx, None).await {
            Err(failure) => failure,
            Ok(_) => panic!("invalid chunk index must be rejected"),
        };
    assert_eq!(reason, RefusalReason::BundleRejected);
    assert_eq!(error.to_string(), "noncontiguous bundle chunk");
}

#[tokio::test(start_paused = true)]
async fn slow_steady_upload_longer_than_300_seconds_never_stalls() {
    let (_scratch, runner, request, bytes) = fixture();
    let (tx, mut rx) = mpsc::channel(8);
    let sender = tokio::spawn(async move {
        for (offset, byte) in bytes.into_iter().enumerate() {
            tokio::time::sleep(Duration::from_secs(30)).await;
            tx.send(Frame::BundleChunk {
                index: 0,
                offset: offset as u64,
                data: BytePayload(vec![byte]),
            })
            .await
            .unwrap();
        }
    });
    let start = tokio::time::Instant::now();
    let uploaded = upload(&runner, request.job_id, &request.bundles, &mut rx, None)
        .await
        .unwrap();
    assert!(uploaded.complete());
    assert_eq!(start.elapsed(), Duration::from_secs(630));
    assert!(runner
        .job_status(request.job_id)
        .unwrap()
        .terminal
        .is_none());
    sender.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_single_slow_fragmented_chunk_keeps_moving_beyond_300_seconds() {
    use tokio::io::AsyncWriteExt;
    let (_scratch, runner, request, bytes) = fixture();
    let mut encoded = Vec::new();
    write_frame(
        &mut encoded,
        &Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bytes),
        },
    )
    .unwrap();
    let (read, mut write) = tokio::io::duplex(64);
    let (tx, mut rx) = mpsc::channel(8);
    let (activity, mut progress) = watch::channel(Activity::default());
    let reader = tokio::spawn(reader(read, tx, activity, None));
    let sender = tokio::spawn(async move {
        for byte in encoded {
            tokio::time::sleep(Duration::from_secs(30)).await;
            write.write_all(&[byte]).await.unwrap();
        }
    });
    let start = tokio::time::Instant::now();
    let uploaded = upload(
        &runner,
        request.job_id,
        &request.bundles,
        &mut rx,
        Some(&mut progress),
    )
    .await
    .unwrap();
    assert!(uploaded.complete());
    assert!(start.elapsed() > Duration::from_secs(300));
    sender.await.unwrap();
    reader.abort();
}

#[test]
fn fetch_timeout_refusal_is_typed_and_keeps_the_stall_code() {
    let (_scratch, runner, request, _) = fixture();
    let error = io::Error::new(
        io::ErrorKind::TimedOut,
        "transfer_stalled: git fetch made no progress for 60 seconds",
    );
    let reason = transfer_refusal(&error);
    assert_eq!(reason, RefusalReason::TransferInterrupted);
    runner.refuse(request.job_id, reason, &error).unwrap();
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(
        terminal.refusal_detail(),
        Some("transfer_stalled: git fetch made no progress for 60 seconds")
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms(), None);
}

#[test]
fn stalled_withdrawal_keeps_private_intent_and_typed_terminal() {
    let (_scratch, runner, request, _) = fixture();
    assert_eq!(
        runner
            .withdraw_for(request.job_id, Some(RefusalReason::TransferInterrupted))
            .unwrap(),
        motor_protocol::WithdrawOutcome::Started
    );
    assert_eq!(
        runner
            .withdraw_for(
                request.job_id,
                Some(RefusalReason::Unknown("transfer_stalled".into()))
            )
            .unwrap(),
        motor_protocol::WithdrawOutcome::Withdrawn
    );
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TransferInterrupted
        }
    );
    assert_eq!(terminal.refusal_detail(), Some("transfer_stalled"));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.retry_after_ms(), None);
}
