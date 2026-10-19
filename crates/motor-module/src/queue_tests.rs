use super::*;
use crate::transport::Duplex;
use cortexkit_test_support::ScratchDir;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

struct ClockPeer {
    queue_ms: Arc<AtomicU64>,
    withdrawn: Arc<AtomicBool>,
    opens: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
}
impl Connector for ClockPeer {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let primary = self.opens.fetch_add(1, Ordering::SeqCst) == 0;
        let queue = self.queue_ms.clone();
        let withdrawn = self.withdrawn.clone();
        let finished = self.finished.clone();
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                let _ = read_frame_async(&mut peer).await.unwrap();
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        return_paths: false,
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
                if primary {
                    write_frame_async(&mut peer, &Frame::Accepted(Accepted::new(id, 0)))
                        .await
                        .unwrap();
                }
                let mut heartbeat = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    Duration::from_secs(1),
                );
                let mut terminal_sent = false;
                loop {
                    let frame = tokio::select! {
                        frame = read_frame_async(&mut peer) => frame.unwrap(),
                        _ = heartbeat.tick() => {
                            if finished.load(Ordering::SeqCst) && primary && !terminal_sent {
                                let terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0).with_ran(Ran::Remote);
                                let _ = write_frame_async(&mut peer, &Frame::Terminal(terminal)).await;
                                terminal_sent = true;
                                continue;
                            }
                            let ms = queue.load(Ordering::SeqCst);
                            if write_frame_async(&mut peer, &Frame::Heartbeat {
                                queue_wait_ms: (ms != u64::MAX).then_some(ms),
                            }).await.is_err() { return; }
                            continue;
                        }
                    };
                    match frame {
                        Some(Frame::JobStatus { job_id }) => {
                            let ms = queue.load(Ordering::SeqCst);
                            let _ = write_frame_async(
                                &mut peer,
                                &Frame::JobStatusReply(JobStatusReply {
                                    job_id,
                                    state: "queued".into(),
                                    waiting_on: None,
                                    terminal: None,
                                    unit: None,
                                    unit_alive: None,
                                    queue_wait_ms: (ms != u64::MAX).then_some(ms),
                                }),
                            )
                            .await;
                            return;
                        }
                        Some(Frame::Withdraw { .. }) => {
                            withdrawn.store(true, Ordering::SeqCst);
                        }
                        None => return,
                        _ => {}
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn only_observed_permit_wait_can_withdraw_not_elapsed_acceptance() {
    let scratch = ScratchDir::new("motor-queue-clock");
    let queue = Arc::new(AtomicU64::new(u64::MAX));
    let withdrawn = Arc::new(AtomicBool::new(false));
    let motor = Motor::with_connector(
        Config::default(),
        &scratch,
        Arc::new(ClockPeer {
            queue_ms: queue.clone(),
            withdrawn: withdrawn.clone(),
            opens: Arc::new(AtomicU64::new(0)),
            finished: Arc::new(AtomicBool::new(false)),
        }),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 1, None, None, false).unwrap();
    let (tx, _rx) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let mut channel = Channel::open(motor.connector.as_ref(), id).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    let follow = tokio::spawn(async move { motor.follow(id, 0, Some(channel), &tx, wake).await });
    // queue_wait_ms is absent during upload, mirror fetch and workspace
    // creation, and older runners never report it. These states do not
    // authorize queue-budget withdrawal.
    for _ in 0..360 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!withdrawn.load(Ordering::SeqCst));
    }
    queue.store(1_001, Ordering::SeqCst);
    for _ in 0..4 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }
    assert!(withdrawn.load(Ordering::SeqCst));
    follow.abort();
}

async fn advance_and_drain(duration: Duration) {
    tokio::time::advance(duration).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn queued_then_running_uses_only_the_jobs_existing_channel() {
    let scratch = ScratchDir::new("motor-stream-queue");
    let queue = Arc::new(AtomicU64::new(1_000));
    let opens = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let withdrawn = Arc::new(AtomicBool::new(false));
    let motor = Motor::with_connector(
        Config::default(),
        &scratch,
        Arc::new(ClockPeer {
            queue_ms: queue.clone(),
            opens: opens.clone(),
            finished: finished.clone(),
            withdrawn: withdrawn.clone(),
        }),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    let mut channel = Channel::open(motor.connector.as_ref(), id).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    let (tx, mut records) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let follow = tokio::spawn(async move { motor.follow(id, 0, Some(channel), &tx, wake).await });
    for _ in 0..2 {
        advance_and_drain(Duration::from_secs(1)).await;
    }
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "queue timing opened extra channels"
    );
    queue.store(u64::MAX, Ordering::SeqCst);
    // Once the job owns capacity, its heartbeats omit queue_wait_ms.
    // Running longer than the queue budget must neither withdraw the job
    // nor create status channels.
    for _ in 0..1200 {
        advance_and_drain(Duration::from_secs(1)).await;
    }
    assert!(!withdrawn.load(Ordering::SeqCst));
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "running job opened extra channels"
    );
    finished.store(true, Ordering::SeqCst);
    advance_and_drain(Duration::from_secs(1)).await;
    follow.await.unwrap().unwrap();
    assert!(
        matches!(records.recv().await.unwrap().unwrap(), StreamRecord::Terminal(t) if t.ran == Some(Ran::Remote) && t.outcome == Outcome::Exit { code: 0 })
    );
}

#[tokio::test(start_paused = true)]
async fn streamed_elapsed_queue_time_sets_a_local_deadline() {
    let scratch = ScratchDir::new("motor-stream-deadline");
    let withdrawn = Arc::new(AtomicBool::new(false));
    let motor = Motor::with_connector(
        Config::default(),
        &scratch,
        Arc::new(ClockPeer {
            queue_ms: Arc::new(AtomicU64::new(900)),
            withdrawn: withdrawn.clone(),
            opens: Arc::new(AtomicU64::new(0)),
            finished: Arc::new(AtomicBool::new(false)),
        }),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 1, None, None, false).unwrap();
    let mut channel = Channel::open(motor.connector.as_ref(), id).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    let (tx, _records) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let follow = tokio::spawn(async move { motor.follow(id, 0, Some(channel), &tx, wake).await });
    advance_and_drain(Duration::from_secs(1)).await;
    assert!(!withdrawn.load(Ordering::SeqCst));
    advance_and_drain(Duration::from_millis(90)).await;
    assert!(!withdrawn.load(Ordering::SeqCst));
    advance_and_drain(Duration::from_millis(10)).await;
    assert!(withdrawn.load(Ordering::SeqCst));
    follow.abort();
}

struct SpillPeer {
    withdrawn: Arc<AtomicBool>,
    direct: bool,
}
impl Connector for SpillPeer {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let withdrawn = self.withdrawn.clone();
        let direct = self.direct;
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        gate_jobs: true,
                        platforms: vec!["linux".into()],
                        default_branch_tip: false,
                        return_paths: false,
                    },
                )
                .await
                .unwrap();
                while let Ok(Some(frame)) = read_frame_async(&mut peer).await {
                    match frame {
                        Frame::Have { repositories, .. } => write_frame_async(
                            &mut peer,
                            &Frame::HaveReply {
                                repositories,
                                git_dependencies: Some(Vec::new()),
                            },
                        )
                        .await
                        .unwrap(),
                        Frame::Run(run) => {
                            write_frame_async(&mut peer, &Frame::Accepted(Accepted::new(id, 0)))
                                .await
                                .unwrap();
                            if direct {
                                crate::hosted::boat_tests::send_result(&mut peer, id, 0).await;
                                return;
                            }
                            assert_eq!(run.queue_wait_limit_s, 0);
                            write_frame_async(
                                &mut peer,
                                &Frame::Heartbeat {
                                    queue_wait_ms: Some(0),
                                },
                            )
                            .await
                            .unwrap();
                        }
                        Frame::Withdraw { job_id, .. } => {
                            assert_eq!(id, job_id);
                            withdrawn.store(true, Ordering::SeqCst);
                            write_frame_async(
                                &mut peer,
                                &Frame::WithdrawReply {
                                    outcome: WithdrawOutcome::Withdrawn,
                                },
                            )
                            .await
                            .unwrap();
                            write_frame_async(
                                &mut peer,
                                &Frame::Terminal(
                                    TerminalRecord::new(
                                        id,
                                        Outcome::RefusedBeforeStart {
                                            reason: RefusalReason::QueueWaitExceeded,
                                        },
                                        0,
                                        0,
                                        0,
                                    )
                                    .with_ran(Ran::None),
                                ),
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        Frame::Heartbeat { .. } => {}
                        _ => panic!("unexpected build-server frame"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

#[tokio::test]
async fn spill_after_permit_wait_preserves_original_stream_output_and_exit() {
    let boat = crate::hosted::boat_tests::Boat::new(1, true).await;
    let scratch = ScratchDir::new("motor-spill-stream");
    let repo = scratch.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let mut results = Vec::new();
    for direct in [true, false] {
        let withdrawn = Arc::new(AtomicBool::new(false));
        let motor = Motor::with_connector(
            boat.config.clone(),
            &scratch.join(if direct {
                "direct-state"
            } else {
                "spill-state"
            }),
            Arc::new(SpillPeer {
                withdrawn: withdrawn.clone(),
                direct,
            }),
        )
        .unwrap();
        let id = Uuid::now_v7();
        motor
            .storage
            .create(id, 30, None, Some("test".into()), false)
            .unwrap();
        let path = repo.to_str().unwrap();
        let request = RunRequest::new(path, path, path, "exit 23").with_timeout(30);
        let (tx, mut rx) = mpsc::channel(32);
        let (_wake, wake) = watch::channel(false);
        let job = motor.clone();
        let task = tokio::spawn(async move {
            job.run_job(
                id,
                (request, crate::ReturnOptions::default()),
                Vec::new(),
                (30, !direct),
                &tx,
                wake,
            )
            .await
        });
        let mut output = Vec::new();
        let terminal = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match rx.recv().await.unwrap().unwrap() {
                    StreamRecord::Output(record) => output.extend(record.bytes.0),
                    StreamRecord::Terminal(terminal) => break terminal,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(withdrawn.load(Ordering::SeqCst), !direct);
        if !direct {
            let state = motor.hosted_state(id).unwrap();
            assert!(state.spill_wait_ms.is_some());
            assert_eq!(state.placement.unwrap().host, "boat:test");
            assert!(state.finish_done);
        }
        results.push((output, terminal.outcome, terminal.ran));
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1].0, b"same output\n");
    assert_eq!(results[1].1, Outcome::Exit { code: 23 });
    assert_eq!(boat.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(boat.attaches.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(path, _)| path.ends_with("/release")));
}

#[tokio::test]
async fn boat_finish_is_first_for_terminal_refusal_and_unknown_and_never_releases() {
    use crate::hosted::{test_http::Http, HostedJobState};
    use std::sync::Mutex;
    let observed = Arc::new(Mutex::new(None::<Arc<Storage>>));
    let observe = observed.clone();
    let http = Http::new(move |path, _| {
        assert!(
            path.ends_with("/finish"),
            "Boat booking must never be released"
        );
        let id = Uuid::parse_str(path.split('/').nth(3).unwrap()).unwrap();
        let guard = observe.lock().unwrap();
        let job = guard.as_ref().unwrap().job(id).unwrap();
        assert!(job.local_terminal.is_none());
        assert!(job.terminal_observed_at_ms.is_none());
        (
            200,
            serde_json::json!({"job_id":id,"settled_vcpu_ms":500,"released_vcpu_ms":59500}),
        )
    })
    .await;
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let scratch = ScratchDir::new("boat-finish-first");
    let config = Config {
        runner_instance: "test".into(),
        broker_url: Some(http.url.clone()),
        broker_token_source: crate::BrokerTokenSource::Fake,
        ..Config::default()
    };
    let motor = Motor::open(config.clone(), &scratch.join("state")).unwrap();
    *observed.lock().unwrap() = Some(motor.storage.clone());
    for path in ["terminal", "refusal", "unknown"] {
        let id = Uuid::now_v7();
        motor.storage.create(id, 30, None, None, false).unwrap();
        let mut route = HostedJobState::new(&config);
        route.placement = Some(motor_lease::Placement {
            host: "boat:test".into(),
            vm: Some("test".into()),
        });
        route.max_runtime_s = Some(30);
        route.lease_attempted = true;
        if path == "terminal" {
            route.terminal = Some(
                TerminalRecord::new(id, Outcome::Exit { code: 23 }, 0, 0, 0).with_ran(Ran::Remote),
            );
        }
        motor.storage.hosted_route(id, route).unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        if path == "refusal" {
            motor
                .refused(
                    id,
                    RefusalReason::Unreachable,
                    None,
                    Some("hosted_connect_failed".into()),
                    &tx,
                )
                .await
                .unwrap();
        } else {
            motor.finish_hosted(id, false, &tx).await.unwrap();
        }
        assert!(matches!(
            rx.recv().await.unwrap().unwrap(),
            StreamRecord::Terminal(_)
        ));
        assert!(motor.hosted_state(id).unwrap().finish_done);
    }
    assert_eq!(http.requests.lock().unwrap().len(), 3);
    let bodies = http.requests.lock().unwrap();
    assert_eq!(bodies[0].1["outcome"], "exit");
    assert_eq!(bodies[1].1["outcome"], "lost");
    assert_eq!(bodies[2].1["outcome"], "lost");
}
