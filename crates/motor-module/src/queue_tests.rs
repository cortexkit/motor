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
