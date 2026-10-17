use super::*;
use motor_protocol::{LeasePurpose, RefusedCause, TenantEventKind, VerifiedLease};
use tokio::io::DuplexStream;

fn open(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    Runner::open(config).unwrap()
}
fn lease(id: Uuid, purpose: LeasePurpose) -> VerifiedLease {
    VerifiedLease {
        jti: id,
        purpose,
        sub: "acct:test".into(),
        vcpus: 2,
        mem_mib: 16384,
        platform: "linux".into(),
        net: false,
        max_s: 30,
        exp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60,
    }
}
fn hello() -> Frame {
    Frame::Hello {
        protocol_version: motor_protocol::PROTOCOL_VERSION,
        started: true,
        default_branch_tip: false,
        gate_jobs: false,
        platforms: vec![],
    }
}
async fn frame(socket: &mut DuplexStream) -> Option<Frame> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match motor_protocol::read_frame_async(socket).await.unwrap() {
                Some(Frame::Heartbeat { .. }) => {}
                other => return other,
            }
        }
    })
    .await
    .expect("tenant response timed out")
}
async fn connect(
    runner: &Runner,
    lease: VerifiedLease,
) -> (DuplexStream, tokio::task::JoinHandle<io::Result<()>>) {
    let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
    let runner = runner.clone();
    let task = tokio::spawn(crate::server::tenant_channel(runner, server));
    motor_protocol::write_frame_async(&mut client, &Frame::VerifiedLease(lease))
        .await
        .unwrap();
    motor_protocol::write_frame_async(&mut client, &hello())
        .await
        .unwrap();
    assert!(matches!(
        frame(&mut client).await,
        Some(Frame::Hello { .. })
    ));
    (client, task)
}
fn pending(runner: &Runner) -> Vec<motor_protocol::TenantEvent> {
    let log = runner.tenant_events().unwrap();
    let mut events = vec![];
    while let Some(event) = log.first() {
        log.ack(event.event_seq).unwrap();
        events.push(event);
    }
    events
}

#[tokio::test]
async fn tenant_first_frame_is_strict_and_local_ssh_still_uses_hello() {
    let f = Fixture::new();
    let runner = open(&f);
    for first in [
        serde_json::json!({"type":"future_frame"}),
        serde_json::to_value(hello()).unwrap(),
    ] {
        let (mut client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(crate::server::tenant_channel(runner.clone(), server));
        let bytes = serde_json::to_vec(&first).unwrap();
        use tokio::io::AsyncWriteExt;
        client
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(&bytes).await.unwrap();
        assert!(frame(&mut client).await.is_none());
        task.await.unwrap().unwrap();
    }
    assert!(pending(&runner).is_empty());
    let listener = tokio::net::UnixListener::bind(runner.config().socket_path()).unwrap();
    let task = tokio::spawn(crate::serve(runner.clone(), listener));
    let mut socket = tokio::net::UnixStream::connect(runner.config().socket_path())
        .await
        .unwrap();
    motor_protocol::write_frame_async(&mut socket, &hello())
        .await
        .unwrap();
    loop {
        if matches!(
            motor_protocol::read_frame_async(&mut socket).await.unwrap(),
            Some(Frame::Hello { .. })
        ) {
            break;
        }
    }
    task.abort();
}

#[tokio::test]
async fn tenant_connection_without_event_log_returns_error_instead_of_panicking() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (client, task) = connect(&runner, lease(Uuid::now_v7(), LeasePurpose::Run)).await;
    drop(client);
    let error = task
        .await
        .expect("connection task must not panic")
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("tenant event log is not configured"));
}

#[tokio::test]
async fn tenant_mismatch_refuses_without_admission() {
    let f = Fixture::new();
    let runner = open(&f);
    for case in 0..8 {
        let (mut run, _) = f.run("true");
        run.weight_hint = Some(2);
        run.timeout = Some(30);
        run.platform = None;
        run.network = None;
        let mut token = lease(run.job_id, LeasePurpose::Run);
        match case {
            0 => run.weight_hint = None,
            1 => run.weight_hint = Some(1),
            2 => run.network = Some(Network::Outbound),
            3 => token.net = true,
            4 => run.platform = Some(Platform::Windows),
            5 => run.timeout = None,
            6 => run.timeout = Some(31),
            7 => token.platform = "windows".into(),
            _ => unreachable!(),
        }
        let (mut client, task) = connect(&runner, token).await;
        motor_protocol::write_frame_async(&mut client, &Frame::Run(run.clone()))
            .await
            .unwrap();
        assert!(
            matches!(frame(&mut client).await, Some(Frame::Refusal(r)) if r.reason == RefusalReason::RequestRejected)
        );
        assert!(frame(&mut client).await.is_none());
        task.await.unwrap().unwrap();
        assert!(!runner.has_job_record(run.job_id));
        let events = pending(&runner);
        assert_eq!(events.len(), 1);
        assert_eq!(
            (events[0].event, events[0].jti, events[0].cause),
            (
                TenantEventKind::Refused,
                Some(run.job_id),
                Some(RefusedCause::Mismatch)
            )
        );
    }
}

#[tokio::test]
async fn tenant_operations_fail_closed_without_job_changes() {
    let f = Fixture::new();
    let runner = open(&f);
    let (run, _) = f.run("true");
    let id = run.job_id;
    let other = Uuid::now_v7();
    let forbidden = vec![
        Frame::Status,
        Frame::Resume,
        Frame::Drain {
            deadline_s: 1,
            exit_after: false,
        },
        Frame::JobStatus { job_id: other },
        Frame::Withdraw {
            job_id: other,
            reason: None,
        },
        Frame::Cancel(CancelRequest::new(other)),
        Frame::Run(run.clone()),
        Frame::Drop(DropRequest::new(f.key.clone())),
        Frame::Prepare(Prepare {
            transfer_id: other,
            workspace_key: run.workspace_key.clone(),
            workspace_kind: None,
            repository_root: run.repository_root.clone(),
            base_commit: run.snapshot_commit.clone(),
            integration_tip: run.integration_tip.clone(),
            siblings: vec![],
            bundles: vec![],
            git_dependencies: vec![],
        }),
        Frame::Have {
            repositories: vec![],
            git_dependencies: vec![],
        },
    ];
    for operation in forbidden {
        let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Attach)).await;
        motor_protocol::write_frame_async(&mut client, &operation)
            .await
            .unwrap();
        assert!(frame(&mut client).await.is_none());
        task.await.unwrap().unwrap();
        assert!(!runner.has_job_record(id));
        assert!(
            pending(&runner).is_empty(),
            "attach leases never reserve or settle"
        );
    }
    let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Status)
        .await
        .unwrap();
    assert!(frame(&mut client).await.is_none());
    task.await.unwrap().unwrap();
    assert_eq!(pending(&runner)[0].cause, Some(RefusedCause::Disconnect));
}

#[tokio::test]
async fn tenant_attach_keeps_serving_after_connect_expiry_and_allows_repeated_queries() {
    let f = Fixture::new();
    let runner = open(&f);
    let (run, _) = f.run("true");
    let id = run.job_id;
    assert!(matches!(
        runner.accept(run).unwrap(),
        Admission::Accepted(_)
    ));
    let mut token = lease(id, LeasePurpose::Attach);
    token.exp = 1;
    let (mut client, task) = connect(&runner, token).await;
    for _ in 0..2 {
        motor_protocol::write_frame_async(&mut client, &Frame::JobStatus { job_id: id })
            .await
            .unwrap();
        assert!(
            matches!(frame(&mut client).await, Some(Frame::JobStatusReply(status)) if status.job_id == id)
        );
    }
    drop(client);
    task.await.unwrap().unwrap();
    assert!(pending(&runner).is_empty());
}

#[tokio::test]
async fn tenant_rejection_disconnect_and_expiry_are_distinct() {
    let f = Fixture::new();
    let runner = open(&f);
    let (mut run, _) = f.run("true");
    run.weight_hint = Some(2);
    run.timeout = Some(30);
    run.platform = None;
    run.network = None;
    let (client, task) = connect(&runner, lease(run.job_id, LeasePurpose::Run)).await;
    drop(client);
    task.await.unwrap().unwrap();
    assert_eq!(pending(&runner)[0].cause, Some(RefusedCause::Disconnect));
    let mut expired = lease(run.job_id, LeasePurpose::Run);
    expired.exp = 1;
    let (mut client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(crate::server::tenant_channel(runner.clone(), server));
    motor_protocol::write_frame_async(&mut client, &Frame::VerifiedLease(expired))
        .await
        .unwrap();
    assert!(frame(&mut client).await.is_none());
    task.await.unwrap().unwrap();
    assert_eq!(pending(&runner)[0].cause, Some(RefusedCause::Expired));
    assert!(!runner.has_job_record(run.job_id));
    runner.begin_drain(Duration::from_secs(30), false);
    let (mut client, task) = connect(&runner, lease(run.job_id, LeasePurpose::Run)).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Run(run))
        .await
        .unwrap();
    assert!(
        matches!(frame(&mut client).await, Some(Frame::Refusal(r)) if r.reason == RefusalReason::RunnerDraining)
    );
    assert!(frame(&mut client).await.is_none());
    task.await.unwrap().unwrap();
    assert_eq!(pending(&runner)[0].cause, Some(RefusedCause::Rejected));
}

#[tokio::test]
async fn tenant_accepted_is_admitted_and_withdraw_is_terminal() {
    let f = Fixture::new();
    let runner = open(&f);
    let (mut run, bytes) = f.run("true");
    run.weight_hint = Some(2);
    run.queue_wait_limit_s = 0;
    run.timeout = Some(30);
    run.platform = None;
    run.network = None;
    let id = run.job_id;
    let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Run(run))
        .await
        .unwrap();
    assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
    for (index, bytes) in bytes.into_iter().enumerate() {
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
    tokio::time::timeout(Duration::from_secs(10), async {
        while runner.job_status(id).unwrap().state != "queued" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events = pending(&runner);
    assert_eq!(
        events.iter().map(|e| e.event).collect::<Vec<_>>(),
        [TenantEventKind::Admitted]
    );
    motor_protocol::write_frame_async(
        &mut client,
        &Frame::Withdraw {
            job_id: id,
            reason: None,
        },
    )
    .await
    .unwrap();
    loop {
        match frame(&mut client).await {
            Some(Frame::Output(_)) => {}
            Some(Frame::WithdrawReply {
                outcome: motor_protocol::WithdrawOutcome::Withdrawn,
            }) => break,
            other => panic!("expected withdrawal acknowledgement, got {other:?}"),
        }
    }
    loop {
        match frame(&mut client).await {
            Some(Frame::Output(_)) => {}
            Some(Frame::Terminal(record)) => {
                assert!(matches!(record.outcome, Outcome::RefusedBeforeStart { .. }));
                break;
            }
            other => panic!("expected withdrawal terminal, got {other:?}"),
        }
    }
    assert_eq!(
        pending(&runner).iter().map(|e| e.event).collect::<Vec<_>>(),
        [TenantEventKind::Terminal]
    );
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn tenant_event_channel_replays_unacked_frames_in_order() {
    let f = Fixture::new();
    let runner = open(&f);
    let id = Uuid::now_v7();
    let log = runner.tenant_events().unwrap();
    log.refused(id, RefusedCause::Disconnect).unwrap();
    log.admit(id).unwrap();
    let first = log.first().unwrap();
    for _ in 0..2 {
        let (mut client, server) = tokio::io::duplex(4096);
        let r = runner.clone();
        let task = tokio::spawn(async move { crate::tenant::event_session(&r, server).await });
        assert_eq!(frame(&mut client).await, Some(Frame::Event(first.clone())));
        drop(client);
        assert!(task.await.unwrap().is_err());
        assert_eq!(log.first().unwrap(), first);
    }
    let (mut client, server) = tokio::io::duplex(4096);
    let r = runner.clone();
    let task = tokio::spawn(async move { crate::tenant::event_session(&r, server).await });
    assert_eq!(frame(&mut client).await, Some(Frame::Event(first.clone())));
    motor_protocol::write_frame_async(
        &mut client,
        &Frame::Ack {
            event_seq: first.event_seq,
        },
    )
    .await
    .unwrap();
    let Some(Frame::Event(second)) = frame(&mut client).await else {
        panic!("expected next event")
    };
    assert_eq!(
        (second.event_seq, second.event),
        (first.event_seq + 1, TenantEventKind::Admitted)
    );
    motor_protocol::write_frame_async(
        &mut client,
        &Frame::Ack {
            event_seq: second.event_seq + 1,
        },
    )
    .await
    .unwrap();
    assert!(frame(&mut client).await.is_none());
    assert!(task.await.unwrap().is_err());
    assert_eq!(log.first().unwrap(), second);
}

#[tokio::test]
async fn tenant_retained_records_cannot_be_run_again_in_any_state() {
    let f = Fixture::new();
    let runner = open(&f);
    for state in [
        JobState::AwaitingBundles,
        JobState::Running,
        JobState::Terminal,
    ] {
        let (mut run, _) = f.run("true");
        run.weight_hint = Some(2);
        run.timeout = Some(30);
        let id = run.job_id;
        assert!(matches!(
            runner.accept(run.clone()).unwrap(),
            Admission::Accepted(_)
        ));
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let job = inner.jobs.get_mut(&id).unwrap();
            if state == JobState::Terminal {
                job.terminal(Outcome::Cancelled, runner.now());
            }
            job.state = state;
            runner.save(job).unwrap();
        }
        let before = fs::read(runner.record_path(id)).unwrap();
        let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
        motor_protocol::write_frame_async(&mut client, &Frame::Run(run))
            .await
            .unwrap();
        assert!(frame(&mut client).await.is_none());
        task.await.unwrap().unwrap();
        assert_eq!(fs::read(runner.record_path(id)).unwrap(), before);
        let events = pending(&runner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cause, Some(RefusedCause::Disconnect));
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .jobs
            .get_mut(&id)
            .unwrap()
            .terminal(Outcome::Cancelled, runner.now());
    }
}

#[tokio::test]
async fn tenant_second_run_after_started_detaches_but_original_finishes() {
    let f = Fixture::new();
    let runner = open(&f);
    let (mut run, bytes) = f.run("sleep 1; printf original");
    run.weight_hint = Some(2);
    run.timeout = Some(30);
    run.platform = None;
    run.network = None;
    let id = run.job_id;
    let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Run(run.clone()))
        .await
        .unwrap();
    assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
    for (index, bytes) in bytes.into_iter().enumerate() {
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
    tokio::time::timeout(Duration::from_secs(10), async {
        while runner.job_status(id).unwrap().state != "queued" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runner.tick().unwrap();
    loop {
        if matches!(frame(&mut client).await, Some(Frame::Started { .. })) {
            break;
        }
    }
    motor_protocol::write_frame_async(&mut client, &Frame::Run(run))
        .await
        .unwrap();
    while frame(&mut client).await.is_some() {}
    task.await.unwrap().unwrap();
    let terminal = terminal(&runner, id).await;
    assert!(matches!(terminal.outcome, Outcome::Exit { code: 0 }));
    let events = pending(&runner);
    assert_eq!(
        events.iter().map(|e| e.event).collect::<Vec<_>>(),
        [
            TenantEventKind::Admitted,
            TenantEventKind::Started,
            TenantEventKind::Terminal
        ]
    );
}

#[tokio::test]
async fn tenant_failed_accepted_write_is_not_admitted_and_can_reconnect() {
    use std::{
        pin::Pin,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll, Waker},
    };
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    #[derive(Default)]
    struct Gate {
        blocked: AtomicBool,
        release: AtomicBool,
        waker: Mutex<Option<Waker>>,
    }
    struct BrokenAccepted {
        socket: DuplexStream,
        gate: Arc<Gate>,
    }
    impl AsyncRead for BrokenAccepted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.socket).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for BrokenAccepted {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if buf
                .windows(b"\"type\":\"accepted\"".len())
                .any(|window| window == b"\"type\":\"accepted\"")
            {
                *self.gate.waker.lock().unwrap() = Some(cx.waker().clone());
                self.gate.blocked.store(true, Ordering::SeqCst);
                if self.gate.release.load(Ordering::SeqCst) {
                    return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                }
                return Poll::Pending;
            }
            Pin::new(&mut self.socket).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.socket).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.socket).poll_shutdown(cx)
        }
    }
    let f = Fixture::new();
    let runner = open(&f);
    let (mut run, _) = f.run("true");
    run.weight_hint = Some(2);
    run.timeout = Some(30);
    run.network = None;
    run.platform = None;
    let id = run.job_id;
    let gate = Arc::new(Gate::default());
    let (mut client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(crate::server::tenant_channel(
        runner.clone(),
        BrokenAccepted {
            socket: server,
            gate: gate.clone(),
        },
    ));
    motor_protocol::write_frame_async(
        &mut client,
        &Frame::VerifiedLease(lease(id, LeasePurpose::Run)),
    )
    .await
    .unwrap();
    motor_protocol::write_frame_async(&mut client, &hello())
        .await
        .unwrap();
    assert!(matches!(
        frame(&mut client).await,
        Some(Frame::Hello { .. })
    ));
    motor_protocol::write_frame_async(&mut client, &Frame::Run(run))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !gate.blocked.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        pending(&runner).is_empty(),
        "accepted is still blocked on the actual writer"
    );
    gate.release.store(true, Ordering::SeqCst);
    gate.waker.lock().unwrap().take().unwrap().wake();
    task.await.unwrap().unwrap();
    assert!(!runner.has_job_record(id));
    assert_eq!(
        pending(&runner)
            .iter()
            .map(|e| (e.event, e.cause))
            .collect::<Vec<_>>(),
        [(TenantEventKind::Refused, Some(RefusedCause::Disconnect))]
    );
    let (client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
    drop(client);
    task.await.unwrap().unwrap();
    assert_eq!(pending(&runner)[0].cause, Some(RefusedCause::Disconnect));
}
