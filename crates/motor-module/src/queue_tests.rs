use super::*;
use crate::transport::Duplex;
use cortexkit_test_support::ScratchDir;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

fn queue_budget_config() -> Config {
    Config {
        spill_to_hosted: true,
        ..Config::default()
    }
}

fn queue_budget_request() -> RunRequest {
    let mut request = RunRequest::new(
        "/home/test/repo",
        "/home/test/repo",
        "/home/test/repo",
        "true",
    )
    .with_timeout(3600);
    request.weight_hint = Some(4);
    request
}

fn submitted_queue_budget(config: Config, request: RunRequest, gate: bool) -> u64 {
    let scratch = ScratchDir::new("motor-queue-budget");
    let motor = Motor::open(config, &scratch).unwrap();
    // Submission stores the budget before the spawned job starts running.
    let stream = if gate {
        motor.run_direct(request, None)
    } else {
        motor.run(request)
    }
    .unwrap();
    motor.storage.job(stream.job_id).unwrap().queue_wait_limit_s
}

#[tokio::test]
async fn queue_budget_16c_defaults_to_thirty_minutes() {
    let mut request = queue_budget_request();
    request.weight_hint = Some(16);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, false),
        1800
    );
}

#[tokio::test]
async fn queue_budget_net_defaults_to_thirty_minutes() {
    let mut request = queue_budget_request();
    request.network = Some(Network::Outbound);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, false),
        1800
    );
}

#[tokio::test]
async fn queue_budget_windows_defaults_to_thirty_minutes() {
    let mut request = queue_budget_request();
    request.platform = Some(Platform::Windows);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, false),
        1800
    );
}

#[tokio::test]
async fn queue_budget_disabled_spill_defaults_to_thirty_minutes() {
    let mut config = queue_budget_config();
    config.spill_to_hosted = false;
    assert_eq!(
        submitted_queue_budget(config, queue_budget_request(), false),
        1800
    );
}

#[tokio::test]
async fn queue_budget_invalid_hosted_timeout_defaults_to_thirty_minutes() {
    let mut request = queue_budget_request();
    request.timeout = Some(7201);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, false),
        1800
    );
}

#[tokio::test]
async fn queue_budget_spill_eligible_4c_keeps_five_minutes() {
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), queue_budget_request(), false),
        300
    );
}

#[tokio::test]
async fn queue_budget_spill_eligible_keeps_configured_default() {
    let mut config = queue_budget_config();
    config.queue_wait_limit_s = 90;
    assert_eq!(
        submitted_queue_budget(config, queue_budget_request(), false),
        90
    );
}

#[tokio::test]
async fn queue_budget_explicit_limit_wins_for_spill_and_non_spill_jobs() {
    for weight in [4, 16] {
        for limit in [0, 7, 2400] {
            let mut request = queue_budget_request();
            request.weight_hint = Some(weight);
            request.queue_wait_limit_s = Some(limit);
            request.timeout = Some(600);
            assert_eq!(
                submitted_queue_budget(queue_budget_config(), request, false),
                limit
            );
        }
    }
}

#[tokio::test]
async fn queue_budget_non_spill_default_is_capped_by_ten_minute_timeout() {
    let mut request = queue_budget_request();
    request.weight_hint = Some(16);
    request.timeout = Some(600);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, false),
        600
    );
}

#[tokio::test]
async fn queue_budget_gate_keeps_thirty_minutes_despite_explicit_limit_and_timeout() {
    let mut request = queue_budget_request();
    request.queue_wait_limit_s = Some(7);
    request.timeout = Some(600);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, true),
        1800
    );
}

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
                        return_chunked: false,
                        sibling_paths: false,
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
                        return_chunked: false,
                        sibling_paths: false,
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
                        Frame::Withdraw { job_id, spill, .. } => {
                            assert_eq!(id, job_id);
                            assert!(spill, "hosted spill must be distinguishable from a refusal");
                            withdrawn.store(true, Ordering::SeqCst);
                            write_frame_async(
                                &mut peer,
                                &Frame::WithdrawReply {
                                    outcome: WithdrawOutcome::Withdrawn,
                                },
                            )
                            .await
                            .unwrap();
                            // As the real runner can: once the job is
                            // withdrawn its heartbeats carry no queue wait,
                            // and one may go out before the terminal record,
                            // which marks the withdrawal as a spill.
                            write_frame_async(
                                &mut peer,
                                &Frame::Heartbeat {
                                    queue_wait_ms: None,
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
                                    .with_ran(Ran::None)
                                    .with_refusal_detail(
                                        format!(
                                            "{}waiting_on=permits",
                                            motor_protocol::SPILL_WITHDRAWAL_DETAIL_PREFIX
                                        ),
                                    ),
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
        vec!["init", "-q", "--initial-branch=main"],
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
    for args in [
        vec!["remote", "add", "origin", repo.to_str().unwrap()],
        vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        vec![
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    // The spilled lease must name the build server as the host to avoid.
    let config = Config {
        spill_avoid_host: Some("build-server-1".into()),
        ..boat.config.clone()
    };
    let mut results = Vec::new();
    for direct in [true, false] {
        let withdrawn = Arc::new(AtomicBool::new(false));
        let motor = Motor::with_connector(
            config.clone(),
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
        assert!(
            crate::default_tip::tip(&repo, &motor.storage, scratch.path()).is_some(),
            "the fixture must resolve a real default-branch tip to exercise fill suppression"
        );
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
    let frames = boat.frames.lock().unwrap();
    assert!(frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Hello {
                default_branch_tip, ..
            } => Some(default_branch_tip),
            _ => None,
        })
        .all(|enabled| !enabled));
    let runs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Run(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].integration_tip.is_empty());
    drop(frames);
    assert!(!boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(path, _)| path.ends_with("/release")));
    // One provisioning reply, then the booking: both requests avoid the build
    // server, so the spilled job is never placed back on it.
    let avoided: Vec<_> = boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/leases")
        .map(|(_, body)| body["avoid_host"].clone())
        .collect();
    assert_eq!(avoided, ["build-server-1"; 2]);
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

/// What a fake build server saw, in order.
#[derive(Default)]
pub(crate) struct BuildServerLog {
    /// Run frames as (job ID, queue_wait_limit_s).
    pub runs: Vec<(Uuid, u64)>,
    pub attaches: Vec<Uuid>,
    pub withdraws: Vec<Uuid>,
    /// Jobs that ran to completion, which Attach replays.
    pub completed: std::collections::HashSet<Uuid>,
    pub cancels: Vec<Uuid>,
    pub cancelled: std::collections::HashSet<Uuid>,
    pub cancelled_before_run: std::collections::HashSet<Uuid>,
    admitted: std::collections::HashSet<Uuid>,
}

/// A build server that is full for a job's first submission and runs its
/// resubmission. A Run with a zero queue-wait limit is the spill-watched first
/// submission: it waits for a capacity permit until ck-motor withdraws it. Any
/// other Run starts after a short queue wait and exits 7. Attach serves a
/// completed job and, like the real runner, answers an ID it never admitted
/// with a transfer_interrupted tombstone.
pub(crate) struct ResubmitPeer {
    pub log: Arc<std::sync::Mutex<BuildServerLog>>,
    /// What happens to a Run that is not a spill-watched first submission.
    pub end: ResubmitEnd,
}
/// How the fake build server ends a Run that is not a spill-watched first
/// submission.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ResubmitEnd {
    /// It starts after a short queue wait and exits 7.
    Exit,
    /// It is admitted, then refused when its queue wait runs out, behind a
    /// gate job, with the refusal detail `QUEUE_REFUSAL_DETAIL`.
    QueueRefused,
    /// It is refused at admission as runner_full, with the refusal detail
    /// `ADMISSION_REFUSAL_DETAIL`.
    AdmissionRefused,
    /// It observes this much queue wait before trying to start.
    QueuedFor(u64),
}
pub(crate) const QUEUE_REFUSAL_DETAIL: &str =
    "waiting_on=gate job ahead; blocked_by=job=01a16f1d-0000-7000-8000-000000000000 permits=12";
pub(crate) const ADMISSION_REFUSAL_DETAIL: &str = "runner draining for maintenance";
impl ResubmitPeer {
    pub(crate) fn new() -> Self {
        Self::ending(ResubmitEnd::Exit)
    }
    pub(crate) fn ending(end: ResubmitEnd) -> Self {
        Self {
            log: Arc::default(),
            end,
        }
    }
}
async fn resubmitted_result<W: tokio::io::AsyncWrite + Unpin>(
    peer: &mut W,
    id: Uuid,
    from: u64,
    queue_wait_ms: u64,
) {
    if from == 0 {
        let started = Frame::Started {
            job_id: id,
            seq: 0,
            queue_wait_ms,
            started_at_ms: 1_730_000_000_300,
            path_mapping: None,
        };
        write_frame_async(peer, &started).await.unwrap();
    }
    if from <= 1 {
        let output = Output::new(
            1,
            OutputStream::Stdout,
            BytePayload(b"build server output\n".to_vec()),
        );
        write_frame_async(peer, &Frame::Output(output))
            .await
            .unwrap();
    }
    let terminal = TerminalRecord::new(id, Outcome::Exit { code: 7 }, 2, queue_wait_ms, 0)
        .with_ran(Ran::Remote);
    write_frame_async(peer, &Frame::Terminal(terminal))
        .await
        .unwrap();
}
async fn cancelled_result<W: tokio::io::AsyncWrite + Unpin>(peer: &mut W, id: Uuid) {
    write_frame_async(
        peer,
        &Frame::Terminal(TerminalRecord::new(id, Outcome::Cancelled, 0, 0, 0).with_ran(Ran::None)),
    )
    .await
    .unwrap();
}
impl Connector for ResubmitPeer {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let log = self.log.clone();
        let end = self.end;
        let queue_wait_ms = match end {
            ResubmitEnd::QueuedFor(ms) => ms,
            _ => 300,
        };
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                let Ok(Some(Frame::Hello { .. })) = read_frame_async(&mut peer).await else {
                    return;
                };
                let hello = Frame::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    started: true,
                    gate_jobs: true,
                    platforms: vec!["linux".into()],
                    default_branch_tip: false,
                    return_paths: false,
                    return_chunked: false,
                    sibling_paths: false,
                };
                write_frame_async(&mut peer, &hello).await.unwrap();
                while let Ok(Some(frame)) = read_frame_async(&mut peer).await {
                    match frame {
                        Frame::Have { repositories, .. } => {
                            let reply = Frame::HaveReply {
                                repositories,
                                git_dependencies: Some(Vec::new()),
                            };
                            write_frame_async(&mut peer, &reply).await.unwrap();
                        }
                        Frame::Run(run) => {
                            let id = run.job_id;
                            log.lock().unwrap().runs.push((id, run.queue_wait_limit_s));
                            if run.queue_wait_limit_s != 0 && end == ResubmitEnd::AdmissionRefused {
                                let refusal = Refusal {
                                    job_id: Some(id),
                                    transfer_id: None,
                                    reason: RefusalReason::RunnerFull,
                                    disk_full: None,
                                    retry_after_ms: None,
                                    detail: Some(ADMISSION_REFUSAL_DETAIL.into()),
                                };
                                write_frame_async(&mut peer, &Frame::Refusal(refusal))
                                    .await
                                    .unwrap();
                                return;
                            }
                            let accepted = Frame::Accepted(Accepted::new(id, 0));
                            log.lock().unwrap().admitted.insert(id);
                            write_frame_async(&mut peer, &accepted).await.unwrap();
                            let waiting = Frame::Heartbeat {
                                queue_wait_ms: Some(match end {
                                    ResubmitEnd::QueuedFor(ms) if run.queue_wait_limit_s != 0 => ms,
                                    _ => 0,
                                }),
                            };
                            write_frame_async(&mut peer, &waiting).await.unwrap();
                            if run.queue_wait_limit_s == 0 {
                                continue;
                            }
                            if end == ResubmitEnd::QueueRefused
                                || matches!(end, ResubmitEnd::QueuedFor(ms)
                                    if ms >= run.queue_wait_limit_s.saturating_mul(1000))
                            {
                                let terminal = TerminalRecord::new(
                                    id,
                                    Outcome::RefusedBeforeStart {
                                        reason: RefusalReason::QueueWaitExceeded,
                                    },
                                    0,
                                    0,
                                    0,
                                )
                                .with_ran(Ran::None)
                                .with_refusal_detail(QUEUE_REFUSAL_DETAIL);
                                write_frame_async(&mut peer, &Frame::Terminal(terminal))
                                    .await
                                    .unwrap();
                                return;
                            }
                            // Queue briefly: a job still spill-watched would be
                            // withdrawn here.
                            let until = tokio::time::Instant::now() + Duration::from_millis(300);
                            let mut detached = false;
                            loop {
                                match tokio::time::timeout_at(until, read_frame_async(&mut peer))
                                    .await
                                {
                                    Err(_) => break,
                                    Ok(Ok(Some(Frame::Withdraw { job_id, .. }))) => {
                                        log.lock().unwrap().withdraws.push(job_id)
                                    }
                                    Ok(Ok(Some(_))) => {}
                                    Ok(_) => {
                                        // Detaching does not withdraw an admitted job.
                                        detached = true;
                                        tokio::time::sleep_until(until).await;
                                        break;
                                    }
                                }
                            }
                            log.lock().unwrap().completed.insert(id);
                            if detached {
                                return;
                            }
                            let cancelled = log.lock().unwrap().cancelled.contains(&id);
                            if cancelled {
                                cancelled_result(&mut peer, id).await;
                            } else {
                                resubmitted_result(&mut peer, id, 0, queue_wait_ms).await;
                            }
                            return;
                        }
                        Frame::Withdraw { job_id, .. } => {
                            log.lock().unwrap().withdraws.push(job_id);
                            let reply = Frame::WithdrawReply {
                                outcome: WithdrawOutcome::Withdrawn,
                            };
                            write_frame_async(&mut peer, &reply).await.unwrap();
                            let terminal = TerminalRecord::new(
                                job_id,
                                Outcome::RefusedBeforeStart {
                                    reason: RefusalReason::QueueWaitExceeded,
                                },
                                0,
                                0,
                                0,
                            )
                            .with_ran(Ran::None);
                            write_frame_async(&mut peer, &Frame::Terminal(terminal))
                                .await
                                .unwrap();
                            return;
                        }
                        Frame::Attach(request) => {
                            let id = request.job_id;
                            let admitted = {
                                let mut log = log.lock().unwrap();
                                log.attaches.push(id);
                                log.admitted.contains(&id)
                            };
                            while admitted && !log.lock().unwrap().completed.contains(&id) {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                            let completed = log.lock().unwrap().completed.contains(&id);
                            if completed {
                                let cancelled = log.lock().unwrap().cancelled.contains(&id);
                                if cancelled {
                                    cancelled_result(&mut peer, id).await;
                                } else {
                                    resubmitted_result(
                                        &mut peer,
                                        id,
                                        request.from_seq,
                                        queue_wait_ms,
                                    )
                                    .await;
                                }
                            } else {
                                let tombstone = TerminalRecord::new(
                                    id,
                                    Outcome::RefusedBeforeStart {
                                        reason: RefusalReason::TransferInterrupted,
                                    },
                                    0,
                                    0,
                                    0,
                                )
                                .with_ran(Ran::None);
                                write_frame_async(&mut peer, &Frame::Terminal(tombstone))
                                    .await
                                    .unwrap();
                            }
                            return;
                        }
                        Frame::Cancel(request) => {
                            {
                                let mut log = log.lock().unwrap();
                                log.cancels.push(request.job_id);
                                if !log.admitted.contains(&request.job_id) {
                                    log.cancelled_before_run.insert(request.job_id);
                                }
                                log.cancelled.insert(request.job_id);
                            }
                            write_frame_async(
                                &mut peer,
                                &Frame::CancelReply(CancelReply::new(request.job_id)),
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        Frame::Heartbeat { .. } => {}
                        other => panic!("unexpected build-server frame {other:?}"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

/// A committed repository under the scratch directory.
pub(crate) fn fixture_repo(scratch: &Path) -> PathBuf {
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
    repo
}

#[tokio::test]
async fn direct_hosted_submission_omits_a_resolved_fill_tip() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    let scratch = ScratchDir::new("hosted-fill-tip");
    let repo = fixture_repo(scratch.path());
    let motor = Motor::with_connector(
        boat.config.clone(),
        &scratch.join("state"),
        Arc::new(SpillPeer {
            withdrawn: Arc::new(AtomicBool::new(false)),
            direct: true,
        }),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, 30, None, Some("test".into()), false)
        .unwrap();
    motor
        .storage
        .hosted_route(id, crate::HostedJobState::new(&motor.config))
        .unwrap();
    let path = repo.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "exit 23").with_timeout(30);
    let mut snapshots = Snapshots::build(
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
    // Model a resolved default branch independently of the route: the wire
    // submission must suppress even a nonempty, valid fill tip.
    snapshots.main.tip = Some(snapshots.main.head.clone());
    let (tx, mut rx) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let task = tokio::spawn(async move {
        motor
            .run_snapshot(
                id,
                (request, crate::ReturnOptions::default()),
                Vec::new(),
                30,
                false,
                Arc::new(snapshots),
                &tx,
                wake,
            )
            .await
    });
    let terminal = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let StreamRecord::Terminal(terminal) = rx.recv().await.unwrap().unwrap() {
                break terminal;
            }
        }
    })
    .await
    .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    let frames = boat.frames.lock().unwrap();
    let runs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Run(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].integration_tip.is_empty());
}

/// Runs a spill-eligible job with the given queue-wait limit in seconds and
/// returns every record the caller received, up to and including its terminal.
pub(crate) async fn run_spilled(
    motor: &Motor,
    id: Uuid,
    repo: &Path,
    wait: u64,
    limit: Duration,
) -> Vec<StreamRecord> {
    run_spilled_with_timeout(motor, id, repo, wait, limit, 30).await
}

/// [`run_spilled`] for a job with the given timeout in seconds.
async fn run_spilled_with_timeout(
    motor: &Motor,
    id: Uuid,
    repo: &Path,
    wait: u64,
    limit: Duration,
    timeout: u64,
) -> Vec<StreamRecord> {
    run_job_records(motor, id, repo, wait, limit, timeout, true).await
}

/// Runs a job with the given queue-wait limit and timeout in seconds, spill
/// eligible or not, and returns every record the caller received, up to and
/// including its terminal.
async fn run_job_records(
    motor: &Motor,
    id: Uuid,
    repo: &Path,
    wait: u64,
    limit: Duration,
    timeout: u64,
    spill: bool,
) -> Vec<StreamRecord> {
    motor
        .storage
        .create(id, wait, None, Some("test".into()), false)
        .unwrap();
    let path = repo.to_str().unwrap();
    let mut request = RunRequest::new(path, path, path, "true").with_timeout(timeout);
    request.queue_wait_limit_s = Some(wait);
    let (tx, mut rx) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let job = motor.clone();
    let task = tokio::spawn(async move {
        job.run_job(
            id,
            (request, crate::ReturnOptions::default()),
            Vec::new(),
            (wait, spill),
            &tx,
            wake,
        )
        .await
    });
    let records = tokio::time::timeout(limit, async {
        let mut records = Vec::new();
        loop {
            let record = rx.recv().await.unwrap().unwrap();
            let terminal = matches!(record, StreamRecord::Terminal(_));
            records.push(record);
            if terminal {
                break records;
            }
        }
    })
    .await
    .unwrap();
    task.await.unwrap().unwrap();
    records
}

/// The caller saw one job: a single acceptance, output in increasing
/// sequence, and one terminal under its own job ID with the build server's
/// exit code.
pub(crate) fn assert_one_build_server_job(id: Uuid, records: &[StreamRecord]) {
    let accepted: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            StreamRecord::Accepted(a) => Some(a.job_id),
            _ => None,
        })
        .collect();
    assert_eq!(accepted, vec![id], "{records:?}");
    let seqs: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            StreamRecord::Started(s) => Some(s.seq),
            StreamRecord::Output(o) => Some(o.seq),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, vec![0, 1], "{records:?}");
    let terminals: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            StreamRecord::Terminal(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].job_id, id);
    assert_eq!(terminals[0].outcome, Outcome::Exit { code: 7 });
    assert_eq!(terminals[0].ran, Some(Ran::Remote));
    // A job that ran keeps the build server's terminal as it was: only a
    // refusal names the earlier spill attempt.
    assert_eq!(terminals[0].refusal_detail, None);
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(_))),
        "{records:?}"
    );
}

/// The build server saw the withdrawn first submission and exactly one
/// resubmission under a new ID with the stored queue-wait limit, and the
/// resubmission was never withdrawn again.
pub(crate) fn assert_resubmitted_once(id: Uuid, log: &BuildServerLog, wait: u64) -> Uuid {
    assert_eq!(log.runs.len(), 2, "{:?}", log.runs);
    assert_eq!(log.runs[0], (id, 0));
    let (resubmitted, limit) = log.runs[1];
    assert_ne!(resubmitted, id);
    assert_eq!(
        limit, wait,
        "resubmission uses the stored queue-wait budget"
    );
    assert_eq!(log.withdraws, vec![id], "a resubmission never spills again");
    resubmitted
}

fn spill_config(broker_url: &str) -> Config {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    Config {
        runner_instance: "test".into(),
        broker_token_source: crate::BrokerTokenSource::Fake,
        broker_url: Some(broker_url.into()),
        spill_to_hosted: true,
        spill_after_s: 0,
        ..Config::default()
    }
}

fn lease_requests(requests: &crate::hosted::test_http::Requests) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/leases")
        .count()
}

async fn failed_spill_queue_budget(
    provisioning: bool,
    explicit_budget: Option<u64>,
    timeout: u64,
) -> (u64, Vec<StreamRecord>) {
    let broker = crate::hosted::test_http::Http::new(move |path, _| {
        if path == "/v1/leases" {
            if provisioning {
                (
                    202,
                    serde_json::json!({"status": "provisioning", "retry_after_ms": 5000}),
                )
            } else {
                (503, serde_json::json!({"error": "no_capacity"}))
            }
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-failed-spill-queue-budget");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::ending(ResubmitEnd::QueuedFor(360_000));
    let log = peer.log.clone();
    let mut config = spill_config(&broker.url);
    if provisioning {
        // Exhaust provisioning quickly without changing the request's default-budget status.
        config.queue_wait_limit_s = 1;
    }
    let initial_wait = explicit_budget.unwrap_or(config.queue_wait_limit_s);
    let motor = Motor::with_connector(config, &scratch.join("state"), Arc::new(peer)).unwrap();
    let path = repo.to_str().unwrap();
    let mut request = RunRequest::new(path, path, path, "true").with_timeout(timeout);
    request.queue_wait_limit_s = explicit_budget;
    // Exercise the spill flow directly: Linux scratch paths need not be under
    // the /home or /Users prefixes required for ordinary spill admission.
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, initial_wait, None, Some("test".into()), false)
        .unwrap();
    let (tx, mut rx) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let job = motor.clone();
    let task = tokio::spawn(async move {
        job.run_job(
            id,
            (request, crate::ReturnOptions::default()),
            Vec::new(),
            (initial_wait, true),
            &tx,
            wake,
        )
        .await
    });
    let records = tokio::time::timeout(Duration::from_secs(30), async {
        let mut records = Vec::new();
        loop {
            let record = rx.recv().await.unwrap().unwrap();
            let terminal = matches!(record, StreamRecord::Terminal(_));
            records.push(record);
            if terminal {
                break records;
            }
        }
    })
    .await
    .unwrap();
    task.await.unwrap().unwrap();
    let stored = motor.storage.job(id).unwrap();
    let wait = stored.queue_wait_limit_s;
    let cause = if provisioning {
        "hosted_provisioning"
    } else {
        "hosted_capacity"
    };
    assert_eq!(
        stored.hosted.as_ref().unwrap().hosted_code.as_deref(),
        Some(cause)
    );
    assert_resubmitted_once(id, &log.lock().unwrap(), wait);
    assert!(motor.hosted_state(id).is_none());
    if !provisioning {
        assert_eq!(lease_requests(&broker.requests), 1);
    }

    motor.close().await;
    let reopened = crate::storage::Storage::open(&scratch.join("state")).unwrap();
    let recovered = reopened.job(id).unwrap();
    assert_eq!(recovered.queue_wait_limit_s, wait);
    assert_eq!(recovered.build_server_job_id, stored.build_server_job_id);
    assert_eq!(
        recovered.hosted.unwrap().hosted_code.as_deref(),
        Some(cause)
    );
    (wait, records)
}

#[tokio::test]
async fn failed_spill_default_budget_waits_past_five_minutes_and_starts() {
    for provisioning in [false, true] {
        let (wait, records) = failed_spill_queue_budget(provisioning, None, 3600).await;
        assert!((1790..=1800).contains(&wait), "remaining budget: {wait}");
        let id = match &records[0] {
            StreamRecord::Accepted(accepted) => accepted.job_id,
            other => panic!("expected acceptance, got {other:?}"),
        };
        assert_one_build_server_job(id, &records);
        assert!(records.iter().any(|record| matches!(record, StreamRecord::Started(started) if started.queue_wait_ms == 360_000)));
    }
}

#[tokio::test]
async fn failed_spill_caller_budget_is_not_extended() {
    // An explicit budget equal to the ordinary default must not be mistaken for a default.
    let (wait, records) = failed_spill_queue_budget(false, Some(300), 3600).await;
    assert_eq!(wait, 300);
    let Some(StreamRecord::Terminal(terminal)) = records.last() else {
        panic!("missing terminal: {records:?}");
    };
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    );
    assert_eq!(
        terminal.refusal_detail.as_deref(),
        Some(format!("{QUEUE_REFUSAL_DETAIL}; spill_attempted=hosted_capacity").as_str())
    );
}

#[tokio::test]
async fn failed_spill_default_budget_is_capped_by_timeout() {
    let (wait, records) = failed_spill_queue_budget(false, None, 600).await;
    assert!((590..=600).contains(&wait), "remaining budget: {wait}");
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.outcome == Outcome::Exit { code: 7 })
    );
}

#[tokio::test]
async fn failed_spill_budget_from_original_submission_survives_restart() {
    for cause in ["hosted_capacity", "hosted_provisioning"] {
        for (timeout, explicit, gate, expected) in [
            (3600, None, false, 1680_u64),
            (600, None, false, 480),
            (60, None, false, 0),
            (3600, Some(300), false, 300),
            (3600, Some(2400), false, 2400),
            (600, None, true, 1800),
        ] {
            let scratch = ScratchDir::new("motor-spill-budget-restart");
            let config = spill_config("http://127.0.0.1:1");
            let motor =
                Motor::with_connector(config.clone(), &scratch, Arc::new(ResubmitPeer::new()))
                    .unwrap();
            // Include the initial queue wait and provisioning in the total budget.
            let (seconds, nanos) = Uuid::now_v7().get_timestamp().unwrap().to_unix();
            let id = Uuid::new_v7(uuid::Timestamp::from_unix(
                uuid::NoContext,
                seconds - 120,
                nanos,
            ));
            motor
                .storage
                .create(
                    id,
                    if gate { 1800 } else { explicit.unwrap_or(300) },
                    None,
                    None,
                    gate,
                )
                .unwrap();
            let mut route = crate::HostedJobState::new(&config);
            route.spill_wait_ms = Some(45_000);
            motor.storage.hosted_route(id, route).unwrap();
            assert!(motor
                .return_spill(
                    id,
                    cause,
                    tokio::time::Instant::now(),
                    explicit.is_none().then_some(1800.min(timeout))
                )
                .await
                .unwrap());
            let stored = motor.storage.job(id).unwrap();
            assert!(
                (expected.saturating_sub(2)..=expected).contains(&stored.queue_wait_limit_s),
                "{cause}: expected {expected}, got {}",
                stored.queue_wait_limit_s
            );
            let build_server_id = stored.build_server_job_id.unwrap();
            motor.close().await;

            let reopened =
                Motor::with_connector(config, &scratch, Arc::new(ResubmitPeer::new())).unwrap();
            assert_eq!(
                reopened.storage.job(id).unwrap().queue_wait_limit_s,
                stored.queue_wait_limit_s
            );
            assert_eq!(reopened.build_server_id(id), build_server_id);
            assert!(
                reopened.hosted_state(id).is_none(),
                "a returned spill must not book again"
            );
            assert_eq!(
                reopened.with_spill_attempt(id, None).as_deref(),
                Some(format!("spill_attempted={cause}").as_str())
            );
            reopened.close().await;
        }
    }
}

#[tokio::test]
async fn spilled_no_capacity_resubmits_to_build_server_as_one_stream() {
    let broker = crate::hosted::test_http::Http::new(|path, _| {
        // CLOUD answers 503 no_capacity when the tenant's Boat pool or its
        // hourly start cap is exhausted.
        if path == "/v1/leases" {
            (503, serde_json::json!({"error": "no_capacity"}))
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-spill-no-capacity");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&broker.url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
    assert_one_build_server_job(id, &records);
    assert_resubmitted_once(id, &log.lock().unwrap(), 30);
    // One lease request: no_capacity is not retried for a spill, and the
    // resubmitted job never asks the broker again.
    assert_eq!(lease_requests(&broker.requests), 1);
    assert!(!broker
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(path, _)| path.ends_with("/reattach") || path.ends_with("/finish")));
    assert!(motor.hosted_state(id).is_none());
}

// A provisioning budget that runs out used to refuse the caller; the job had
// already given up its build-server place, so it now goes back there.
#[tokio::test]
async fn spilled_provisioning_budget_resubmits_to_build_server() {
    let broker = crate::hosted::test_http::Http::new(|path, _| {
        if path == "/v1/leases" {
            (
                202,
                serde_json::json!({"status": "provisioning", "retry_after_ms": 5000}),
            )
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-spill-provisioning-budget");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&broker.url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    // A one-second queue-wait limit leaves the lease one second to provision.
    let records = run_spilled(&motor, id, &repo, 1, Duration::from_secs(30)).await;
    assert_one_build_server_job(id, &records);
    assert_resubmitted_once(id, &log.lock().unwrap(), 1);
    assert!(lease_requests(&broker.requests) >= 1);
    assert!(motor.hosted_state(id).is_none());
    let text = captured.text();
    let exhausted: Vec<_> = text
        .lines()
        .filter(|line| line.contains("spill provisioning budget exhausted"))
        .collect();
    assert_eq!(exhausted.len(), 1, "{text}");
    assert!(exhausted[0].contains(&format!("job_id={id}")), "{text}");
    let waited_ms: u64 = exhausted[0]
        .split("waited_ms=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // The one-second budget is counted from submission, while waited_ms is
    // timed from the start of the spill path a moment later, so a full wait
    // can log 999 ms. The bound still proves the lease used its whole budget
    // rather than giving up early.
    assert!(waited_ms >= 900, "{text}");
    assert_eq!(
        text.matches("outcome=returned_to_build_server").count(),
        1,
        "{text}"
    );
    assert!(text.contains("broker_code=hosted_provisioning"), "{text}");
}

#[tokio::test]
async fn spilled_deliberate_refusals_stay_final() {
    for (status, code, reason, detail) in [
        (
            403,
            "account_closed",
            RefusalReason::RequestRejected,
            "hosted_account_closed",
        ),
        (
            409,
            "job_conflict",
            RefusalReason::RequestRejected,
            "lease_mismatch",
        ),
        (
            422,
            "platform_unsupported",
            RefusalReason::PlatformUnsupported,
            "hosted_platform_unsupported",
        ),
    ] {
        let broker = crate::hosted::test_http::Http::new(move |path, _| {
            if path == "/v1/leases" {
                (status, serde_json::json!({"error": code}))
            } else {
                (404, serde_json::json!({"error": "job_unknown"}))
            }
        })
        .await;
        let scratch = ScratchDir::new("motor-spill-final-refusal");
        let repo = fixture_repo(&scratch);
        let peer = ResubmitPeer::new();
        let log = peer.log.clone();
        let motor = Motor::with_connector(
            spill_config(&broker.url),
            &scratch.join("state"),
            Arc::new(peer),
        )
        .unwrap();
        let id = Uuid::now_v7();
        let captured = crate::logging::Captured::default();
        let _log = crate::logging::scoped(captured.clone());
        let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
        let terminal = match records.last().unwrap() {
            StreamRecord::Terminal(terminal) => terminal,
            other => panic!("expected terminal, got {other:?}"),
        };
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart { reason },
            "{code}"
        );
        assert_eq!(terminal.ran, Some(Ran::None));
        assert!(terminal.refusal_detail.as_deref().unwrap().contains(detail));
        assert_eq!(
            log.lock().unwrap().runs,
            [(id, 0)],
            "{code}: no resubmission"
        );
        assert_eq!(lease_requests(&broker.requests), 1);
        assert!(!broker
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path.ends_with("/reattach")));
        let text = captured.text();
        let outcomes: Vec<_> = text
            .lines()
            .filter(|line| line.contains("spill outcome"))
            .collect();
        assert_eq!(outcomes.len(), 1, "{text}");
        assert!(
            outcomes[0].contains("outcome=refused") && outcomes[0].contains(detail),
            "{text}"
        );
        assert!(!text.contains("outcome=returned_to_build_server"), "{text}");
        // Every lease 4xx is logged with the body's non-secret members.
        assert!(
            text.lines()
                .any(|line| line.contains("hosted lease body refused by broker")
                    && line.contains(&format!("http_status={status}"))
                    && line.contains("timeout_s=30")
                    && line.contains("vcpus=2")),
            "{text}"
        );
        assert!(!text.contains("requester_jwk"), "{text}");
    }
}

/// A broker that cannot parse a spilled job's lease body (400 `bad_request`)
/// booked nothing, so the job goes back to the build-server queue the way a
/// spill the broker could not place does. A warning names the job and the
/// body's non-secret members, never the requester key.
#[tokio::test]
async fn spilled_bad_request_returns_to_build_server_and_logs_the_body() {
    for (status, body) in crate::hosted::tests::cloud_bad_request_replies() {
        spilled_bad_request_reply(status, body).await;
    }
}

async fn spilled_bad_request_reply(status: u16, body: String) {
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let expected_field = parsed["field"].as_str().unwrap_or("unknown");
    let broker = crate::hosted::test_http::Http::with_reply_bytes(move |path, _| {
        if path == "/v1/leases" {
            (status, body.as_bytes().to_owned())
        } else {
            (
                404,
                serde_json::to_vec(&serde_json::json!({"error": "job_unknown"})).unwrap(),
            )
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-spill-bad-request");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&broker.url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
    assert_one_build_server_job(id, &records);
    assert_resubmitted_once(id, &log.lock().unwrap(), 30);
    // A 400 is not retried: the same body would be refused again.
    assert_eq!(lease_requests(&broker.requests), 1);
    assert!(motor.hosted_state(id).is_none());
    let x = broker
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|(path, _)| path == "/v1/leases")
        .map(|(_, body)| body["requester_jwk"]["x"].as_str().unwrap().to_owned())
        .unwrap();
    let text = captured.text();
    let refused: Vec<_> = text
        .lines()
        .filter(|line| line.contains("hosted lease body refused by broker"))
        .collect();
    assert_eq!(refused.len(), 1, "{text}");
    for field in [
        "WARN".to_owned(),
        format!("job_id={id}"),
        "route=spill".into(),
        "http_status=400".into(),
        "broker_code=bad_request".into(),
        format!("field={expected_field}"),
        "vcpus=2".into(),
        "timeout_s=30".into(),
        "platform=linux-x64".into(),
        "net=false".into(),
        "allow_spill=true".into(),
        "placement_hint=none".into(),
    ] {
        assert!(refused[0].contains(&field), "{field}: {text}");
    }
    assert!(!text.contains(&x), "requester key logged: {text}");
    assert!(!text.contains("requester_jwk="), "{text}");
    assert!(!text.contains("lease_mismatch"), "{text}");
    let returned: Vec<_> = text
        .lines()
        .filter(|line| line.contains("outcome=returned_to_build_server"))
        .collect();
    assert_eq!(returned.len(), 1, "{text}");
    assert!(
        returned[0].contains("broker_code=hosted_request_invalid"),
        "{text}"
    );
}

/// A spilled job whose timeout no lease body can carry (CLOUD takes 10 to
/// 7200 seconds) is never sent to the broker: it goes straight back to the
/// build-server queue, and a warning names the job, the member and the
/// body's non-secret members.
#[tokio::test]
async fn spilled_job_with_an_unsendable_timeout_returns_without_a_lease_request() {
    let broker = crate::hosted::test_http::Http::new(|path, _| {
        assert_ne!(path, "/v1/leases", "an invalid lease body was sent");
        (404, serde_json::json!({"error": "job_unknown"}))
    })
    .await;
    let scratch = ScratchDir::new("motor-spill-unsendable-timeout");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&broker.url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let id = Uuid::now_v7();
    let records =
        run_spilled_with_timeout(&motor, id, &repo, 30, Duration::from_secs(30), 10_800).await;
    assert_one_build_server_job(id, &records);
    assert_resubmitted_once(id, &log.lock().unwrap(), 30);
    assert!(broker.requests.lock().unwrap().is_empty());
    assert!(motor.hosted_state(id).is_none());
    let text = captured.text();
    let invalid: Vec<_> = text
        .lines()
        .filter(|line| line.contains("hosted lease body invalid; not sent"))
        .collect();
    assert_eq!(invalid.len(), 1, "{text}");
    for field in [
        "WARN".to_owned(),
        format!("job_id={id}"),
        "field=timeout_s".into(),
        "timeout_s=10800".into(),
        "vcpus=2".into(),
        "allow_spill=true".into(),
    ] {
        assert!(invalid[0].contains(&field), "{field}: {text}");
    }
    assert!(!text.contains("requester_jwk"), "{text}");
    assert!(
        text.contains("broker_code=hosted_request_invalid"),
        "{text}"
    );
}

#[tokio::test]
async fn spilled_sandbox_lost_before_run_returns_to_build_server() {
    let broker = crate::hosted::test_http::Http::new(|path, _| {
        if path == "/v1/leases" {
            (409, serde_json::json!({"error": "sandbox_lost"}))
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let scratch = ScratchDir::new("motor-spill-sandbox-lost");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        spill_config(&broker.url),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
    assert_one_build_server_job(id, &records);
    assert_resubmitted_once(id, &log.lock().unwrap(), 30);
    assert!(motor.hosted_state(id).is_none());
}

#[tokio::test]
async fn spilled_lost_run_ack_reattaches_without_resubmitting() {
    use crate::hosted::tests::harness::{Behavior, Harness};
    let hosted = Harness::new(Behavior::DropRunAck).await;
    let scratch = ScratchDir::new("motor-spill-lost-run-ack");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        Config {
            spill_after_s: 0,
            ..hosted.config()
        },
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
    let terminal = match records.last().unwrap() {
        StreamRecord::Terminal(terminal) => terminal,
        other => panic!("expected terminal, got {other:?}"),
    };
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(motor.hosted_state(id).unwrap().run_sent);
    assert_eq!(log.lock().unwrap().runs, [(id, 0)]);
    assert_eq!(hosted.runs.load(Ordering::SeqCst), 1);
    assert!(hosted
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Attach(_))));
    assert!(!hosted
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|path| path.ends_with("/release")));
    let text = captured.text();
    assert_eq!(text.matches("outcome=reattaching").count(), 1, "{text}");
    assert!(!text.contains("outcome=returned_to_build_server"), "{text}");
}

#[tokio::test]
async fn spilled_success_logs_placed_once() {
    use crate::hosted::tests::harness::{Behavior, Harness};
    let hosted = Harness::new(Behavior::CompleteRun).await;
    let scratch = ScratchDir::new("motor-spill-placed");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(
        Config {
            spill_after_s: 0,
            ..hosted.config()
        },
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(terminal)) if terminal.outcome == Outcome::Exit { code: 23 })
    );
    assert_eq!(log.lock().unwrap().runs, [(id, 0)]);
    let text = captured.text();
    let placed: Vec<_> = text
        .lines()
        .filter(|line| line.contains("outcome=placed"))
        .collect();
    assert_eq!(placed.len(), 1, "{text}");
    assert!(placed[0].contains(&format!("job_id={id}")), "{text}");
}

fn boat_paths(boat: &crate::hosted::boat_tests::Boat) -> Vec<String> {
    boat.requests
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _)| path.clone())
        .collect()
}

/// The production sequence of the first spills: CLOUD booked a Boat sandbox,
/// ck-motor rejected the lease's token before connecting, released it (CLOUD
/// answers job_admitted for a booked Boat job), reattached to a sandbox that
/// never received the job and reported outcome unknown. A token stating
/// 512 MiB, below any real Boat size, still fails the client's checks; the job
/// must now settle the booking as never run and finish on the build server.
#[tokio::test]
async fn boat_lease_failing_client_checks_resubmits_instead_of_reattaching() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.mem_mib.store(512, Ordering::SeqCst);
    boat.strict_attach.store(true, Ordering::SeqCst);
    let scratch = ScratchDir::new("motor-boat-claims-resubmit");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(boat.config.clone(), &scratch.join("state"), Arc::new(peer)).unwrap();
    let id = Uuid::now_v7();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(60)).await;
    assert_one_build_server_job(id, &records);
    let resubmitted = assert_resubmitted_once(id, &log.lock().unwrap(), 30);
    // The hosted path explains itself in the log, and never logs a token
    // (compact JWTs start with eyJ) or a key.
    let text = captured.text();
    eprintln!("{text}");
    for line in [
        "hosted lease requested",
        "hosted lease granted",
        "provider=boat",
        "hosted lease claims mismatch",
        "field=mem_mib",
        "actual=512",
        "Boat finish replied",
        "http_status=200",
        "spill not placed; resubmitted to build server",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(text.contains(&format!("build_server_job_id={resubmitted}")));
    assert!(!text.contains("eyJ") && !text.contains("ssh-ed25519"));
    assert_eq!(boat.runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(boat.attaches.load(std::sync::atomic::Ordering::SeqCst), 0);
    let paths = boat_paths(&boat);
    assert_eq!(
        paths.iter().filter(|p| p.as_str() == "/v1/leases").count(),
        1,
        "{paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.ends_with("/reattach") || p.ends_with("/release")),
        "{paths:?}"
    );
    let finishes: Vec<_> = boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path.ends_with("/finish"))
        .map(|(_, body)| body.clone())
        .collect();
    assert_eq!(
        finishes,
        vec![serde_json::json!({"outcome": "lost", "run_ms": 0})]
    );
}

/// The same Boat lease with the sandbox's measured memory (13444 MiB, the
/// production large size) is accepted and the job runs on Boat. The caller
/// sees one acceptance, not a second one from the Boat runner.
#[tokio::test]
async fn boat_lease_stating_sandbox_memory_runs_on_boat() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.mem_mib.store(13444, Ordering::SeqCst);
    boat.strict_attach.store(true, Ordering::SeqCst);
    let scratch = ScratchDir::new("motor-boat-sandbox-memory");
    let repo = fixture_repo(&scratch);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(boat.config.clone(), &scratch.join("state"), Arc::new(peer)).unwrap();
    let id = Uuid::now_v7();
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(60)).await;
    let accepted = records
        .iter()
        .filter(|r| matches!(r, StreamRecord::Accepted(_)))
        .count();
    assert_eq!(accepted, 1, "{records:?}");
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.job_id == id && t.outcome == Outcome::Exit { code: 23 }),
        "{records:?}"
    );
    assert_eq!(log.lock().unwrap().runs, vec![(id, 0)]);
    assert_eq!(boat.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// A spilled job that the broker could not place goes back to the build
/// server; when the build server then refuses it, whether at admission or
/// when its queue wait runs out, the refusal detail keeps the build server's
/// own reason and adds `spill_attempted=` with the broker code that sent the
/// job back. A job that never spilled, refused the same way, keeps the build
/// server's detail exactly.
#[tokio::test]
async fn refusal_after_a_failed_spill_names_the_spill_attempt() {
    for (end, runner_detail) in [
        (ResubmitEnd::QueueRefused, QUEUE_REFUSAL_DETAIL),
        (ResubmitEnd::AdmissionRefused, ADMISSION_REFUSAL_DETAIL),
    ] {
        let broker = crate::hosted::test_http::Http::new(|path, _| {
            if path == "/v1/leases" {
                (503, serde_json::json!({"error": "no_capacity"}))
            } else {
                (404, serde_json::json!({"error": "job_unknown"}))
            }
        })
        .await;
        let scratch = ScratchDir::new("motor-spill-attempted");
        let repo = fixture_repo(&scratch);
        let peer = ResubmitPeer::ending(end);
        let log = peer.log.clone();
        let motor = Motor::with_connector(
            spill_config(&broker.url),
            &scratch.join("state"),
            Arc::new(peer),
        )
        .unwrap();
        let terminal = |records: &[StreamRecord]| match records.last() {
            Some(StreamRecord::Terminal(terminal)) => terminal.clone(),
            other => panic!("{end:?}: expected a terminal, got {other:?}"),
        };

        let id = Uuid::now_v7();
        let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(30)).await;
        assert_resubmitted_once(id, &log.lock().unwrap(), 30);
        let spilled = terminal(&records);
        assert_eq!(spilled.job_id, id);
        assert!(
            matches!(spilled.outcome, Outcome::RefusedBeforeStart { .. }),
            "{end:?}: {spilled:?}"
        );
        assert_eq!(
            spilled.refusal_detail.as_deref(),
            Some(format!("{runner_detail}; spill_attempted=hosted_capacity").as_str()),
            "{end:?}"
        );

        let never_spilled = Uuid::now_v7();
        let records = run_job_records(
            &motor,
            never_spilled,
            &repo,
            30,
            Duration::from_secs(30),
            30,
            false,
        )
        .await;
        let refused = terminal(&records);
        assert!(
            matches!(refused.outcome, Outcome::RefusedBeforeStart { .. }),
            "{end:?}: {refused:?}"
        );
        assert_eq!(
            refused.refusal_detail.as_deref(),
            Some(runner_detail),
            "{end:?}"
        );
    }
}

/// The spill note keeps within the refusal detail size limit: a build-server
/// detail that, with the note appended, would exceed the limit is shortened to
/// leave room for the whole note. The note
/// stands alone when the build server gave no detail, and a job that was not
/// sent back to the build server, routed hosted or never spilled, keeps its
/// detail.
#[tokio::test]
async fn spill_attempt_note_keeps_within_the_refusal_detail_limit() {
    let scratch = ScratchDir::new("motor-spill-attempted-limit");
    let config = spill_config("http://127.0.0.1:1");
    let motor = Motor::with_connector(
        config.clone(),
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    let (resubmitted, hosted, plain) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for id in [resubmitted, hosted, plain] {
        motor
            .storage
            .create(id, 30, None, Some("test".into()), false)
            .unwrap();
    }
    for id in [resubmitted, hosted] {
        let mut route = crate::HostedJobState::new(&config);
        route.spill_wait_ms = Some(45_000);
        route.hosted_code = Some("hosted_provisioning".into());
        motor.storage.hosted_route(id, route).unwrap();
    }
    motor
        .storage
        .resubmitted(resubmitted, Uuid::now_v7(), "hosted_provisioning", None)
        .unwrap();
    let note = "; spill_attempted=hosted_provisioning";

    let long = "é".repeat(REFUSAL_DETAIL_MAX_BYTES);
    let detail = motor
        .with_spill_attempt(resubmitted, Some(long.clone()))
        .unwrap();
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES, "{}", detail.len());
    assert!(detail.ends_with(note), "{detail}");
    assert!(long.starts_with(detail.strip_suffix(note).unwrap()));
    assert_eq!(
        motor.with_spill_attempt(resubmitted, None).as_deref(),
        Some("spill_attempted=hosted_provisioning")
    );
    for id in [hosted, plain] {
        assert_eq!(
            motor.with_spill_attempt(id, Some("waiting_on=permits".into())),
            Some("waiting_on=permits".into())
        );
        assert_eq!(motor.with_spill_attempt(id, None), None);
    }
}

/// A build server whose job waits in its queue, with a heartbeat each second
/// as the runner sends one. The runner's queue clock runs while a job waits
/// for capacity, whether for free permits or behind a queue head that waits
/// for them (a gate job, a cache fill, an older job past its backfill grace),
/// so its heartbeat's queue wait grows; it pauses while the job waits for its
/// own workspace owner or a sibling user of its checkout, so the wait stays
/// put. A withdrawal ends the job with a queue-wait refusal naming `label`,
/// marked as a spill withdrawal when it asked to spill, as the runner does.
struct QueuedPeer {
    label: &'static str,
    clock_runs: bool,
    /// Each withdrawal: whether it asked to spill, and the queue wait the
    /// peer had reported by then.
    withdraws: Arc<std::sync::Mutex<Vec<(bool, u64)>>>,
}
impl Connector for QueuedPeer {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let (label, clock_runs) = (self.label, self.clock_runs);
        let withdraws = self.withdraws.clone();
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                let _ = read_frame_async(&mut peer).await.unwrap();
                let hello = Frame::Hello {
                    return_paths: false,
                    return_chunked: false,
                    sibling_paths: false,
                    protocol_version: PROTOCOL_VERSION,
                    started: true,
                    default_branch_tip: false,
                    gate_jobs: true,
                    platforms: Vec::new(),
                };
                write_frame_async(&mut peer, &hello).await.unwrap();
                write_frame_async(&mut peer, &Frame::Accepted(Accepted::new(id, 0)))
                    .await
                    .unwrap();
                let mut heartbeat = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    Duration::from_secs(1),
                );
                let mut waited_ms = 0u64;
                loop {
                    tokio::select! {
                        _ = heartbeat.tick() => {
                            if clock_runs {
                                waited_ms += 1_000;
                            }
                            let beat = Frame::Heartbeat { queue_wait_ms: Some(waited_ms) };
                            if write_frame_async(&mut peer, &beat).await.is_err() {
                                return;
                            }
                        }
                        frame = read_frame_async(&mut peer) => match frame {
                            Ok(Some(Frame::Withdraw { spill, .. })) => {
                                withdraws.lock().unwrap().push((spill, waited_ms));
                                let reply = Frame::WithdrawReply {
                                    outcome: WithdrawOutcome::Withdrawn,
                                };
                                write_frame_async(&mut peer, &reply).await.unwrap();
                                let prefix = if spill {
                                    motor_protocol::SPILL_WITHDRAWAL_DETAIL_PREFIX
                                } else {
                                    ""
                                };
                                let terminal = TerminalRecord::new(
                                    id,
                                    Outcome::RefusedBeforeStart {
                                        reason: RefusalReason::QueueWaitExceeded,
                                    },
                                    0,
                                    0,
                                    0,
                                )
                                .with_ran(Ran::None)
                                .with_refusal_detail(format!(
                                    "{prefix}waiting_on={label}; requested_permits=2"
                                ));
                                write_frame_async(&mut peer, &Frame::Terminal(terminal))
                                    .await
                                    .unwrap();
                                return;
                            }
                            Ok(Some(_)) => {}
                            _ => return,
                        },
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

/// Follows a spill-watched job with a 300-second queue-wait budget and a
/// 45-second spill wait on a `QueuedPeer` for up to `seconds` seconds.
/// Returns the peer's withdrawals, the second the first one arrived, the
/// stream's result if it ended, and the job's hosted route.
async fn follow_queued(
    label: &'static str,
    clock_runs: bool,
    seconds: u64,
) -> (
    Vec<(bool, u64)>,
    Option<u64>,
    Option<io::Result<bool>>,
    Option<crate::HostedJobState>,
) {
    let scratch = ScratchDir::new("motor-spill-queue-head");
    let withdraws = Arc::new(std::sync::Mutex::new(Vec::new()));
    let config = Config {
        spill_to_hosted: true,
        spill_after_s: 45,
        ..Config::default()
    };
    let motor = Motor::with_connector(
        config,
        &scratch,
        Arc::new(QueuedPeer {
            label,
            clock_runs,
            withdraws: withdraws.clone(),
        }),
    )
    .unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 300, None, None, false).unwrap();
    let mut channel = Channel::open(motor.connector.as_ref(), id).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    let (tx, _records) = mpsc::channel(32);
    let (_wake, wake) = watch::channel(false);
    let follower = motor.clone();
    let follow = tokio::spawn(async move {
        follower
            .follow_inner(id, 0, Some(channel), &tx, wake, Some(45))
            .await
    });
    let mut first_withdraw = None;
    for second in 1..=seconds {
        advance_and_drain(Duration::from_secs(1)).await;
        if first_withdraw.is_none() && !withdraws.lock().unwrap().is_empty() {
            first_withdraw = Some(second);
        }
        if follow.is_finished() {
            break;
        }
    }
    let result = if follow.is_finished() {
        Some(follow.await.unwrap())
    } else {
        follow.abort();
        None
    };
    let withdraws = withdraws.lock().unwrap().clone();
    (withdraws, first_withdraw, result, motor.hosted_state(id))
}

/// A job behind a gate job, a cache fill or an older job that waits for free
/// permits is in a capacity wait, as one waiting for free permits itself is:
/// the runner's heartbeat reports a growing queue wait for it, so once that
/// reaches spill_after_s ck-motor withdraws it asking to spill, and it moves
/// to a hosted runner. ck-motor decides from that heartbeat wait, not from the
/// label; the runner's side, which wait runs the clock, is pinned by the
/// runner test `only_capacity_waits_run_the_queue_clock_a_spill_watches`.
/// This also held before the runner named a gate job in its status reply.
#[tokio::test(start_paused = true)]
async fn capacity_waits_behind_a_queue_head_spill_after_the_spill_wait() {
    for label in [
        "permits",
        "gate job ahead",
        "older job ahead needs more free permits",
        "cache fill ahead needs more free permits",
    ] {
        let (withdraws, first, result, route) = follow_queued(label, true, 120).await;
        // The withdrawal goes out when the heartbeat wait reaches 45 s; the
        // peer may count its own 45th second just before or just after it
        // reads the withdrawal.
        let [(spill, waited_ms)] = withdraws.as_slice() else {
            panic!("{label}: one withdrawal expected: {withdraws:?}")
        };
        assert!(*spill, "{label}: the withdrawal must ask to spill");
        assert!(
            (44_000..=45_000).contains(waited_ms),
            "{label}: {waited_ms}"
        );
        assert!(
            first.is_some_and(|second| (45..=46).contains(&second)),
            "{label}: spilled at the spill wait, not before: {first:?}"
        );
        assert!(
            matches!(result, Some(Ok(true))),
            "{label}: the stream hands the job to a hosted runner: {result:?}"
        );
        let route = route.unwrap_or_else(|| panic!("{label}: no hosted route"));
        assert!(
            route.spill_wait_ms.is_some_and(|ms| ms >= 45_000),
            "{label}"
        );
    }
}

/// A job waiting for its own workspace owner, or for a sibling user of its
/// checkout, needs that job to finish and release the checkout, which a
/// hosted runner cannot do for it. The runner pauses its queue clock, so its
/// heartbeat wait never reaches the spill wait: ck-motor neither withdraws it
/// nor spills it, however long the wait lasts.
#[tokio::test(start_paused = true)]
async fn checkout_waits_never_spill() {
    for label in ["workspace owner", "sibling user of this checkout"] {
        let (withdraws, first, result, route) = follow_queued(label, false, 400).await;
        assert!(withdraws.is_empty(), "{label}: {withdraws:?}");
        assert_eq!(first, None, "{label}");
        assert!(
            result.is_none(),
            "{label}: the job stays queued: {result:?}"
        );
        assert!(route.is_none(), "{label}");
    }
}
