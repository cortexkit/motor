use super::*;
use crate::{
    hosted::tests::harness::{Behavior, Harness, NoSsh},
    logging,
};
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::AtomicUsize;

fn assert_cause(log: &str, id: Uuid, step: &str, cause: &str) {
    assert!(
        log.lines().any(|line| {
            (line.contains(" INFO ") || line.contains(" WARN "))
                && line.contains(&format!("job_id={id}"))
                && line.contains(&format!("step={step}"))
                && line.contains(&format!("cause={cause}"))
        }),
        "missing job/step/cause diagnostic: {log}"
    );
}

#[tokio::test]
async fn post_connect_refusals_and_admission_failure_log_the_cause() {
    for fault in ["capabilities", "bundles", "have", "admission"] {
        let behavior = match fault {
            "have" => Behavior::DropHave,
            "admission" => Behavior::DropRunAck,
            _ => Behavior::Normal,
        };
        let h = Harness::new(behavior).await;
        let scratch = ScratchDir::new("hosted-post-connect-log");
        let repo = queue_tests::fixture_repo(scratch.path());
        let motor = Motor::with_test_fixture_direct_route(
            h.config(),
            &scratch.join("state"),
            Arc::new(NoSsh(AtomicUsize::new(0))),
        )
        .unwrap();
        let id = Uuid::now_v7();
        motor
            .storage
            .create(id, 30, None, Some("test".into()), false)
            .unwrap();
        motor.storage.hosted_route(id, h.route.clone()).unwrap();
        let path = repo.to_str().unwrap();
        let request = RunRequest::new(path, path, path, "true").with_timeout(30);
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
        if fault == "bundles" {
            std::fs::remove_dir_all(&snapshots.spool.0).unwrap();
        }
        let mut returns = crate::ReturnOptions::default();
        if fault == "capabilities" {
            returns
                .return_paths
                .push(repo.join("result").to_str().unwrap().into());
        }
        let (tx, _rx) = mpsc::channel(32);
        let (_wake, wake) = watch::channel(false);
        let captured = logging::Captured::default();
        let _logs = logging::scoped(captured.clone());
        tokio::time::timeout(
            Duration::from_secs(30),
            motor.run_snapshot(
                id,
                (request, returns),
                Vec::new(),
                30,
                false,
                Arc::new(snapshots),
                &tx,
                wake,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let log = captured.text();
        assert!(log.contains("hosted connect succeeded"), "{fault}: {log}");
        let (step, cause) = match fault {
            "capabilities" => ("refused_before_start", "return_paths_unsupported"),
            // The missing spool is found when the transfer clears its earlier
            // bundles, before git runs.
            "bundles" => ("transfer_bundles", "No such file or directory"),
            "have" => (
                "have_exchange",
                "peer closed connection without sending TLS close_notify",
            ),
            "admission" => (
                "admission",
                "peer closed connection without sending TLS close_notify",
            ),
            _ => unreachable!(),
        };
        assert_cause(&log, id, step, cause);
        if fault != "admission" {
            assert_eq!(h.runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
}

fn hosted_motor(scratch: &Path) -> (Motor, Uuid) {
    let motor = Motor::with_test_fixture_direct_route(
        Config::default(),
        &scratch.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 30, None, None, false).unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::default())
        .unwrap();
    (motor, id)
}

async fn connected_channel() -> (Channel, tokio::io::DuplexStream) {
    let (client, mut peer) = tokio::io::duplex(65536);
    let (channel, peer) = tokio::join!(Channel::from_hosted(Box::new(client), false), async move {
        assert!(matches!(
            read_frame_async(&mut peer).await.unwrap(),
            Some(Frame::Hello { .. })
        ));
        write_frame_async(
            &mut peer,
            &Frame::Hello {
                return_chunked_max_bytes: None,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                return_paths: false,
                return_chunked: false,
                sibling_paths: false,
                default_branch_tip: false,
                gate_jobs: false,
                platforms: Vec::new(),
            },
        )
        .await
        .unwrap();
        peer
    });
    (channel.unwrap(), peer)
}

#[tokio::test(start_paused = true)]
async fn post_connect_timeout_preserves_and_logs_the_transport_cause() {
    let scratch = ScratchDir::new("hosted-timeout-log");
    let (motor, id) = hosted_motor(scratch.path());
    let (mut channel, _peer) = connected_channel().await;
    let captured = logging::Captured::default();
    let _logs = logging::scoped(captured.clone());
    let error = motor
        .have(
            &mut channel,
            Vec::new(),
            Vec::new(),
            id,
            UnreachableStep::HaveExchange,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_cause(&captured.text(), id, "have_exchange", "read inactivity");
}

#[tokio::test]
async fn post_connect_detach_cancel_and_invalid_terminal_log_the_cause() {
    let scratch = ScratchDir::new("hosted-detach-log");
    let (motor, id) = hosted_motor(scratch.path());
    let (channel, _peer) = connected_channel().await;
    let (tx, rx) = mpsc::channel(32);
    drop(rx);
    let (_wake, wake) = watch::channel(false);
    let captured = logging::Captured::default();
    let _logs = logging::scoped(captured.clone());
    assert!(motor
        .follow_inner(id, 0, Some(channel), &tx, wake, None)
        .await
        .is_err());
    assert_cause(&captured.text(), id, "follow", "caller detached");
    let (stream, _tx, _wake) = motor.stream(id);
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    assert_cause(
        &captured.text(),
        id,
        "cancel",
        "caller requested cancellation",
    );
    {
        let captured = logging::Captured::default();
        let _logs = logging::scoped(captured.clone());
        stream.cancel().unwrap();
        assert_cause(
            &captured.text(),
            id,
            "cancel",
            "caller requested cancellation",
        );
    }
    motor.storage.cancel(id, false).unwrap();
    let (channel, mut peer) = connected_channel().await;
    write_frame_async(
        &mut peer,
        &Frame::Terminal(TerminalRecord::new(
            Uuid::now_v7(),
            Outcome::Exit { code: 0 },
            0,
            0,
            0,
        )),
    )
    .await
    .unwrap();
    let (tx, _rx) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    assert!(motor
        .follow_inner(id, 0, Some(channel), &tx, wake, None)
        .await
        .is_err());
    assert_cause(&captured.text(), id, "terminal", "terminal job ID mismatch");
}

#[tokio::test]
async fn hosted_hello_version_mismatch_logs_its_cause_without_claiming_connect_success() {
    let h = Harness::new(Behavior::WrongHello).await;
    let scratch = ScratchDir::new("hosted-hello-mismatch-log");
    let (motor, id) = hosted_motor(scratch.path());
    motor.storage.hosted_route(id, h.route.clone()).unwrap();
    let captured = logging::Captured::default();
    let _logs = logging::scoped(captured.clone());
    assert_eq!(
        crate::hosted::Client::new(h.route.clone())
            .run(id, &Harness::request(scratch.path()), &motor.storage)
            .await
            .err(),
        Some(crate::hosted::Error::Connect)
    );
    let log = captured.text();
    assert_cause(&log, id, "runner_hello", "runner_version_mismatch");
    assert!(!log.contains("hosted connect succeeded"), "{log}");
}
