use super::*;
use crate::storage::legacy_fixtures::{seed_direct_route, DirectRoute};
use crate::transport::Duplex;
use cortexkit_test_support::ScratchDir;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

fn spill_config(broker_url: &str) -> Config {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    Config {
        runner_instance: "test".into(),
        broker_token_source: crate::BrokerTokenSource::Fake,
        broker_url: Some(broker_url.into()),
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

fn queue_budget_config() -> Config {
    Config {
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
async fn ordinary_two_vcpu_linux_job_defaults_to_thirty_minutes() {
    let mut request = queue_budget_request();
    request.weight_hint = Some(2);
    assert_eq!(
        submitted_queue_budget(queue_budget_config(), request, false),
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
                        return_chunked_max_bytes: None,
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
    let motor = Motor::with_test_fixture_direct_route(
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
    let motor = Motor::with_test_fixture_direct_route(
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
    let motor = Motor::with_test_fixture_direct_route(
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
        let queue_wait_ms = 300;
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
                    return_chunked_max_bytes: None,
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
                                queue_wait_ms: Some(0),
                            };
                            write_frame_async(&mut peer, &waiting).await.unwrap();
                            if run.queue_wait_limit_s == 0 {
                                continue;
                            }
                            if end == ResubmitEnd::QueueRefused {
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

/// Submits one direct hosted run, through `config`'s broker, of a repository
/// whose default branch resolved, and returns that tip and the one Run frame
/// the hosted runner recorded in `frames`.
async fn direct_hosted_run_with_a_resolved_tip(
    config: Config,
    frames: &std::sync::Mutex<Vec<Frame>>,
) -> (String, motor_protocol::Run) {
    let scratch = ScratchDir::new("hosted-fill-tip");
    let repo = fixture_repo(scratch.path());
    let motor = Motor::with_test_fixture_direct_route(
        config,
        &scratch.join("state"),
        Arc::new(crate::hosted::tests::harness::NoSsh(
            std::sync::atomic::AtomicUsize::new(0),
        )),
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
    // Model a resolved default branch independently of the route.
    snapshots.main.tip = Some(snapshots.main.head.clone());
    let tip = snapshots.main.head.clone();
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
    let frames = frames.lock().unwrap();
    let runs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Run(request) => Some(request.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1);
    (tip, runs.into_iter().next().unwrap())
}

/// Boat only reads the cache, so a Boat submission suppresses even a
/// nonempty, valid fill tip.
#[tokio::test]
async fn direct_boat_submission_omits_a_resolved_fill_tip() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    let (tip, run) = direct_hosted_run_with_a_resolved_tip(boat.config.clone(), &boat.frames).await;
    assert!(!tip.is_empty());
    assert!(run.integration_tip.is_empty());
}

/// A registered host's tenant runner fills its own cache, so a submission
/// placed there carries the resolved tip, after a Hello that offered tips.
#[tokio::test]
async fn direct_first_party_submission_sends_the_resolved_fill_tip() {
    use crate::hosted::tests::harness::{Behavior, Harness};
    let host = Harness::new(Behavior::CompleteRun).await;
    let (tip, run) = direct_hosted_run_with_a_resolved_tip(host.config(), &host.frames).await;
    assert!(!tip.is_empty());
    assert_eq!(run.integration_tip, tip);
    let hellos = host.client_hellos.lock().unwrap();
    assert!(!hellos.is_empty());
    assert!(hellos.iter().all(|hello| matches!(
        hello,
        Frame::Hello {
            default_branch_tip: true,
            ..
        }
    )));
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
    let mut route = crate::HostedJobState::new(&motor.config);
    route.spill_wait_ms = Some(0);
    motor.storage.hosted_route(id, route).unwrap();
    let path = repo.to_str().unwrap();
    let mut request = RunRequest::new(path, path, path, "true").with_timeout(timeout);
    request.queue_wait_limit_s = Some(wait);
    let (tx, mut rx) = mpsc::channel(32);
    // A persisted spill already delivered its original acceptance before moving to hosted capacity.
    tx.send(Ok(StreamRecord::Accepted(Accepted::new(id, 0))))
        .await
        .unwrap();
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

#[tokio::test]
async fn hosted_lease_log_names_caller_and_only_repository_folder() {
    use crate::hosted::tests::harness::{Behavior, Harness};
    for caller in ["direct", "reserved:aft"] {
        for keyed in [false, true] {
            if caller == "direct" && keyed {
                continue;
            }
            let hosted = Harness::new(Behavior::CompleteRun).await;
            let (scratch, repo) = crate::hosted::boat_tests::committed_repo();
            let motor = Motor::with_connector(
                Config { ..hosted.config() },
                &scratch.0.join("state"),
                Arc::new(ResubmitPeer::new()),
            )
            .unwrap();
            let captured = crate::logging::Captured::default();
            let _log = crate::logging::scoped(captured.clone());
            let root = repo.to_str().unwrap();
            let mut request = RunRequest::new(root, root, root, "exit 23").with_timeout(30);
            let stream = if caller == "direct" {
                motor
                    .run_direct_hosted(request, None, crate::ReturnOptions::default())
                    .unwrap()
            } else {
                if keyed {
                    request.submission_key = Some("source-log-key-0001".into());
                }
                motor.submit_agent(request, None, caller).await.unwrap()
            };
            let id = stream.job_id;
            let mut records = stream.records;
            while let Some(record) = records.recv().await {
                if let StreamRecord::Terminal(terminal) = record.unwrap() {
                    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
                    break;
                }
            }
            let text = captured.text();
            let requested: Vec<_> = text
                .lines()
                .filter(|line| line.contains("hosted lease requested"))
                .collect();
            assert_eq!(requested.len(), 1, "{text}");
            let line = requested[0];
            for field in [
                format!("job_id={id}"),
                format!("caller={caller}"),
                "repo=repo".into(),
                "vcpus=".into(),
                "platform=".into(),
            ] {
                assert!(line.contains(&field), "missing {field}: {line}");
            }
            assert!(
                !line.contains(root) && !line.contains("source-log-key") && !line.contains("eyJ"),
                "{line}"
            );
        }
    }
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
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_test_fixture_direct_route(
        boat.config.clone(),
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
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
    assert!(log.lock().unwrap().runs.is_empty());
    assert_eq!(boat.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    let motor = Motor::with_test_fixture_direct_route(
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
    seed_direct_route(
        &motor.storage,
        resubmitted,
        DirectRoute::SpillReturn {
            runner_job_id: Uuid::now_v7(),
            hosted_code: "hosted_provisioning",
        },
    )
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
                    return_chunked_max_bytes: None,
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
        ..Config::default()
    };
    let motor = Motor::with_test_fixture_direct_route(
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
        let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
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
        assert!(
            log.lock().unwrap().runs.is_empty(),
            "{code}: no direct submission"
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

#[tokio::test]
async fn spilled_lost_run_ack_reattaches_without_resubmitting() {
    use crate::hosted::tests::harness::{Behavior, Harness};
    let hosted = Harness::new(Behavior::DropRunAck).await;
    let scratch = ScratchDir::new("motor-spill-lost-run-ack");
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(hosted.config(), &scratch.join("state"), Arc::new(peer)).unwrap();
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
    assert!(log.lock().unwrap().runs.is_empty());
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
    assert!(!text.contains("outcome=returned_to_build_server"), "{text}");
}

#[tokio::test]
async fn boat_lease_failing_client_checks_refuses_without_direct_fallback() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.mem_mib.store(512, Ordering::SeqCst);
    boat.strict_attach.store(true, Ordering::SeqCst);
    let scratch = ScratchDir::new("motor-boat-claims-resubmit");
    let (_repo_scratch, repo) = crate::hosted::boat_tests::committed_repo();
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor =
        Motor::with_connector(boat.config.clone(), &scratch.join("state"), Arc::new(peer)).unwrap();
    let id = Uuid::now_v7();
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let records = run_spilled(&motor, id, &repo, 30, Duration::from_secs(60)).await;
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(terminal)) if terminal.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::RunnerAuthenticationFailed } && terminal.refusal_detail.as_deref() == Some("hosted_auth")),
        "{records:?}"
    );
    assert!(log.lock().unwrap().runs.is_empty());
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
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(!text.contains("resubmitted to build server"));
    assert!(!text.contains("eyJ") && !text.contains("ssh-ed25519"));
    assert_eq!(boat.runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(boat.attaches.load(std::sync::atomic::Ordering::SeqCst), 0);
    let paths: Vec<_> = boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
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
