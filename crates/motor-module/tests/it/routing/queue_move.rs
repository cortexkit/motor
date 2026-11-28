use super::*;
use motor_protocol::{QueueStatus, WithdrawOutcome, QUEUE_MOVE_REASON};
use std::collections::HashMap;
use tokio::sync::{watch, Semaphore};

#[derive(Clone)]
enum Event {
    Queued(u64),
    Done(Box<TerminalRecord>),
}

#[derive(Clone, Copy)]
enum MoveReply {
    Withdrawn,
    Started,
    Lost,
}

pub(super) struct MoveQueue {
    pub(super) capable: bool,
    reply: MoveReply,
    jobs: Mutex<HashMap<Uuid, watch::Sender<Event>>>,
    attached: Semaphore,
    pub(super) probes: Semaphore,
    withdrew: Semaphore,
}

impl MoveQueue {
    fn new(reply: MoveReply, capable: bool) -> Self {
        Self {
            capable,
            reply,
            jobs: Mutex::default(),
            attached: Semaphore::new(0),
            probes: Semaphore::new(0),
            withdrew: Semaphore::new(0),
        }
    }

    pub(super) fn admit(&self, id: Uuid) {
        let (state, _) = watch::channel(Event::Queued(1_000));
        assert!(self.jobs.lock().unwrap().insert(id, state).is_none());
    }

    pub(super) fn depth(&self) -> u32 {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .filter(|state| matches!(*state.borrow(), Event::Queued(_)))
            .count() as u32
    }

    fn heartbeat(&self, id: Uuid, elapsed: u64) {
        self.jobs.lock().unwrap()[&id].send_replace(Event::Queued(elapsed));
    }

    fn finish(&self, terminal: TerminalRecord, dispatches: &Mutex<DispatchLog>) {
        dispatches.lock().unwrap().finished(terminal.job_id);
        let id = terminal.job_id;
        self.jobs.lock().unwrap()[&id].send_replace(Event::Done(Box::new(terminal)));
    }

    fn start_and_finish(&self, id: Uuid, dispatches: &Mutex<DispatchLog>) {
        self.finish(
            TerminalRecord::new(id, Outcome::Exit { code: 0 }, 1, 0, 0).with_ran(Ran::Remote),
            dispatches,
        );
    }

    pub(super) fn withdraw(
        &self,
        id: Uuid,
        reason: Option<RefusalReason>,
        dispatches: &Mutex<DispatchLog>,
    ) -> Option<WithdrawOutcome> {
        let move_reason = RefusalReason::Unknown(QUEUE_MOVE_REASON.into());
        let moving = reason.as_ref() == Some(&move_reason);
        if moving {
            assert!(self.capable);
            assert_eq!(self.depth(), 1, "a move needs an unambiguous queue");
        } else {
            assert_eq!(reason, None);
        }
        let reply = if moving && matches!(self.reply, MoveReply::Started) {
            self.start_and_finish(id, dispatches);
            Some(WithdrawOutcome::Started)
        } else {
            let reason = if moving {
                move_reason
            } else {
                RefusalReason::QueueWaitExceeded
            };
            self.finish(
                TerminalRecord::new(id, Outcome::RefusedBeforeStart { reason }, 0, 0, 0)
                    .with_ran(Ran::None),
                dispatches,
            );
            if moving && matches!(self.reply, MoveReply::Lost) {
                None
            } else {
                Some(WithdrawOutcome::Withdrawn)
            }
        };
        self.withdrew.add_permits(1);
        reply
    }

    pub(super) async fn follow(
        &self,
        id: Uuid,
        peer: &mut tokio::io::DuplexStream,
        dispatches: &Mutex<DispatchLog>,
    ) {
        let mut state = self.jobs.lock().unwrap()[&id].subscribe();
        self.attached.add_permits(1);
        let (mut input, mut output) = tokio::io::split(peer);
        let send = async {
            loop {
                let event = state.borrow_and_update().clone();
                match event {
                    Event::Queued(elapsed) => {
                        if write_frame_async(
                            &mut output,
                            &Frame::Heartbeat {
                                queue_wait_ms: Some(elapsed),
                                queue_status: Some(QueueStatus {
                                    state: "queued".into(),
                                    waiting_on: Some("permits".into()),
                                    blocker: None,
                                }),
                            },
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                    Event::Done(terminal) => {
                        if terminal.ran == Some(Ran::Remote) {
                            let _ = write_frame_async(
                                &mut output,
                                &Frame::Started {
                                    job_id: id,
                                    seq: 0,
                                    queue_wait_ms: 1_000,
                                    started_at_ms: 1,
                                    path_mapping: None,
                                },
                            )
                            .await;
                        }
                        let _ = write_frame_async(&mut output, &Frame::Terminal(*terminal)).await;
                        return;
                    }
                }
                if state.changed().await.is_err() {
                    return;
                }
            }
        };
        let receive = async {
            loop {
                match read_frame_async(&mut input).await {
                    Ok(Some(Frame::Heartbeat { .. })) => {}
                    Ok(Some(Frame::Withdraw {
                        job_id,
                        reason,
                        spill: false,
                    })) => {
                        assert_eq!(job_id, id);
                        assert_eq!(
                            self.withdraw(id, reason, dispatches),
                            Some(WithdrawOutcome::Withdrawn)
                        );
                    }
                    Ok(None) | Err(_) => return,
                    other => panic!("unexpected frame on queued job stream: {other:?}"),
                }
            }
        };
        tokio::select! { _ = send => {}, _ = receive => {} }
    }
}

fn setup(
    f: &Fixture,
    reply: MoveReply,
    capable: bool,
) -> (Arc<RoutingPeer>, Arc<MoveQueue>, Config, Motor) {
    let mut peer = RoutingPeer::new(f);
    let queue = Arc::new(MoveQueue::new(reply, capable));
    Arc::get_mut(&mut peer).unwrap().move_queue = Some(queue.clone());
    peer.ally_behavior.store(5, Ordering::SeqCst);
    let config = ordered(f.config.clone());
    let motor = peer.motor(config.clone());
    (peer, queue, config, motor)
}

async fn wait(semaphore: &Semaphore, event: &str) {
    tokio::time::timeout(Duration::from_secs(5), semaphore.acquire())
        .await
        .unwrap_or_else(|_| panic!("no {event}"))
        .unwrap()
        .forget();
}

async fn submit(f: &Fixture, motor: &Motor, queue: &MoveQueue, budget: u64) -> crate::JobStream {
    let mut request = f
        .request("true")
        .with_platform(Platform::Windows)
        .with_progress(true);
    request.queue_wait_limit_s = Some(budget);
    let stream = motor.run(request).unwrap();
    wait(&queue.attached, "queued attachment").await;
    stream
}

async fn collect_done(stream: crate::JobStream) -> Vec<StreamRecord> {
    tokio::time::timeout(Duration::from_secs(10), collect(stream))
        .await
        .expect("job did not finish")
}

fn assert_refused(records: &[StreamRecord], id: Uuid, reason: RefusalReason) {
    assert!(matches!(records.last(), Some(StreamRecord::Terminal(t))
        if t.job_id == id && t.ran == Some(Ran::None)
        && t.outcome == (Outcome::RefusedBeforeStart { reason })));
}

async fn refresh_until<T>(queue: &MoveQueue, ids: &[Uuid], until: impl Future<Output = T>) -> T {
    let start = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tokio::pin!(until);
    loop {
        tokio::select! {
            biased;
            done = &mut until => return done,
            _ = tick.tick() => {
                for &id in ids {
                    if let Some(state) = queue.jobs.lock().unwrap().get(&id) {
                        state.send_if_modified(|current| {
                            if let Event::Queued(elapsed) = current {
                                *elapsed = 1_000 + start.elapsed().as_millis() as u64;
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn sole_queued_windows_job_moves_and_keeps_its_caller_id() {
    let f = Fixture::new(Cut::None, 28).await;
    let (peer, queue, _, motor) = setup(&f, MoveReply::Withdrawn, true);
    let stream = submit(&f, &motor, &queue, 30).await;
    let id = stream.job_id;
    peer.ally_behavior.store(18, Ordering::SeqCst);
    let records = refresh_until(&queue, &[id], collect_done(stream)).await;
    assert!(matches!(records.last(), Some(StreamRecord::Terminal(t))
        if t.job_id == id && t.outcome == Outcome::Exit { code: 0 }));
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, StreamRecord::Accepted(a) if a.job_id == id))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, StreamRecord::Progress(p)
        if p.detail() == Some("queued job moved to ally")))
            .count(),
        1
    );
    let dispatches = peer.dispatches.lock().unwrap();
    assert!(
        matches!(dispatches.runs.as_slice(), [(caller, original, last), (same, next, first)]
        if *caller == id && same == caller && *original == id && next != original
        && last == "windows" && first == "ally-windows")
    );
    let new_id = dispatches.runs[1].1;
    assert!(peer
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Run(r)
        if r.job_id == new_id && (1..=30).contains(&r.queue_wait_limit_s))));
}

#[tokio::test]
async fn windows_move_racing_start_leaves_job_in_place_and_releases_permit() {
    let f = Fixture::new(Cut::None, 28).await;
    let (peer, queue, _, motor) = setup(&f, MoveReply::Started, true);
    let stream = submit(&f, &motor, &queue, 30).await;
    let id = stream.job_id;
    peer.ally_behavior.store(18, Ordering::SeqCst);
    let records = refresh_until(&queue, &[id], collect_done(stream)).await;
    assert!(matches!(records.last(), Some(StreamRecord::Terminal(t))
        if t.job_id == id && t.ran == Some(Ran::Remote)));
    assert!(records
        .iter()
        .all(|r| !matches!(r, StreamRecord::Progress(p)
        if p.detail() == Some("queued job moved to ally"))));
    let next = motor
        .run(f.request("true").with_platform(Platform::Windows))
        .unwrap();
    let next_id = next.job_id;
    collect_done(next).await;
    assert_eq!(
        peer.dispatches.lock().unwrap().runs,
        vec![
            (id, id, "windows".into()),
            (next_id, next_id, "ally-windows".into()),
        ]
    );
}

#[tokio::test]
async fn neither_windows_job_moves_until_only_one_is_pending() {
    let f = Fixture::new(Cut::None, 28).await;
    let (peer, queue, _, motor) = setup(&f, MoveReply::Withdrawn, true);
    let first = submit(&f, &motor, &queue, 30).await;
    let second = submit(&f, &motor, &queue, 30).await;
    let (first_id, second_id) = (first.job_id, second.job_id);
    peer.ally_behavior.store(18, Ordering::SeqCst);
    refresh_until(&queue, &[first_id, second_id], async {
        wait(&queue.probes, "first multi-job status probe").await;
        wait(&queue.probes, "second multi-job status probe").await;
    })
    .await;
    assert_eq!(queue.withdrew.available_permits(), 0);
    assert_eq!(peer.dispatches.lock().unwrap().runs.len(), 2);
    queue.start_and_finish(first_id, &peer.dispatches);
    collect_done(first).await;
    refresh_until(&queue, &[second_id], collect_done(second)).await;
    let dispatches = peer.dispatches.lock().unwrap();
    assert!(
        matches!(dispatches.runs.as_slice(), [_, _, (caller, _, instance)]
        if *caller == second_id && instance == "ally-windows")
    );
}

#[tokio::test]
async fn restart_without_move_reply_reattaches_to_withdrawn_job() {
    let f = Fixture::new(Cut::None, 28).await;
    let (peer, queue, config, motor) = setup(&f, MoveReply::Lost, true);
    let stream = submit(&f, &motor, &queue, 30).await;
    let id = stream.job_id;
    peer.ally_behavior.store(18, Ordering::SeqCst);
    refresh_until(
        &queue,
        &[id],
        wait(&queue.withdrew, "unacknowledged withdrawal"),
    )
    .await;
    let stored = crate::storage::durable_value(&peer.module_state).unwrap();
    assert_eq!(
        stored["jobs"][id.to_string()]["windows_runner"]["name"],
        "build-server"
    );
    drop(stream);
    motor.close().await;
    let motor = peer.motor(config);
    let records = collect_done(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_refused(
        &records,
        id,
        RefusalReason::Unknown(QUEUE_MOVE_REASON.into()),
    );
    assert_eq!(
        peer.dispatches.lock().unwrap().runs,
        vec![(id, id, "windows".into())]
    );
}

#[tokio::test]
async fn windows_queue_deadline_withdrawal_remains_queue_wait_exceeded() {
    let f = Fixture::new(Cut::None, 28).await;
    let (peer, queue, _, motor) = setup(&f, MoveReply::Withdrawn, true);
    let stream = submit(&f, &motor, &queue, 2).await;
    let id = stream.job_id;
    queue.heartbeat(id, 2_000);
    let records = collect_done(stream).await;
    assert_refused(&records, id, RefusalReason::QueueWaitExceeded);
    assert_eq!(queue.withdrew.available_permits(), 1);
    assert_eq!(peer.dispatches.lock().unwrap().runs.len(), 1);
}

#[tokio::test]
async fn legacy_windows_queue_owner_never_receives_a_move_request() {
    let f = Fixture::new(Cut::None, 28).await;
    let (peer, queue, _, motor) = setup(&f, MoveReply::Withdrawn, false);
    let stream = submit(&f, &motor, &queue, 2).await;
    let id = stream.job_id;
    peer.ally_behavior.store(18, Ordering::SeqCst);
    let records = collect_done(stream).await;
    assert_refused(&records, id, RefusalReason::QueueWaitExceeded);
    assert_eq!(queue.withdrew.available_permits(), 1);
    assert_eq!(peer.dispatches.lock().unwrap().runs.len(), 1);
}
