use super::*;
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

const RING_BYTES: usize = 1024 * 1024;
const RING_RECORDS: usize = 1024;

pub(super) struct Owner {
    state: Mutex<State>,
    changed: watch::Sender<()>,
    pub(super) wake: watch::Sender<bool>,
    stopped: watch::Sender<bool>,
}

struct State {
    ring: VecDeque<(u64, usize, StreamRecord)>,
    bytes: usize,
    index: u64,
    floor: u64,
    cursor: u64,
    rewind: Option<u64>,
    subscribers: HashMap<u64, u64>,
    next_subscriber: u64,
    subscriber_errors: HashMap<u64, String>,
    boat_drain: Option<(tokio::time::Instant, Vec<u64>)>,
    accepted: Option<Accepted>,
    terminal: Option<TerminalRecord>,
    reply: Option<JobStatusReply>,
    error: Option<String>,
    finished: bool,
    started: bool,
    observed_now_ms: u64,
    live: bool,
}

fn sequence(record: &StreamRecord) -> Option<u64> {
    match record {
        StreamRecord::Started(record) => Some(record.seq),
        StreamRecord::Output(record) => Some(record.seq),
        StreamRecord::ReturnChunk(record) => Some(record.seq),
        StreamRecord::ReturnManifest(record) => Some(record.seq),
        _ => None,
    }
}

impl Owner {
    fn new(cursor: u64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                ring: VecDeque::new(),
                bytes: 0,
                index: 0,
                floor: cursor,
                cursor,
                rewind: None,
                subscribers: HashMap::new(),
                next_subscriber: 0,
                subscriber_errors: HashMap::new(),
                boat_drain: None,
                accepted: None,
                terminal: None,
                reply: None,
                error: None,
                finished: false,
                started: false,
                observed_now_ms: 0,
                live: true,
            }),
            changed: watch::channel(()).0,
            wake: watch::channel(false).0,
            stopped: watch::channel(false).0,
        })
    }

    fn publish(&self, record: io::Result<StreamRecord>) {
        let mut state = self.state.lock().unwrap();
        match record {
            Ok(StreamRecord::Accepted(accepted)) => state.accepted = Some(accepted),
            Ok(StreamRecord::Terminal(terminal)) => state.terminal = Some(terminal),
            Ok(record) => {
                state.started |= matches!(record, StreamRecord::Started(_));
                let bytes = serde_json::to_vec(&record).map_or(RING_BYTES + 1, |s| s.len());
                if let Some(seq) = sequence(&record) {
                    state.cursor = state.cursor.max(seq.saturating_add(1));
                }
                let index = state.index;
                state.index += 1;
                state.bytes += bytes;
                state.ring.push_back((index, bytes, record));
                while state.bytes > RING_BYTES || state.ring.len() > RING_RECORDS {
                    let (_, bytes, record) = state.ring.pop_front().unwrap();
                    state.bytes -= bytes;
                    if let Some(seq) = sequence(&record) {
                        state.floor = state.floor.max(seq.saturating_add(1));
                    }
                }
            }
            Err(error) => state.error = Some(error.to_string()),
        }
        drop(state);
        self.changed.send_replace(());
    }

    pub(super) fn take_rewind(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let cursor = state.rewind.take()?;
        state.ring.clear();
        state.bytes = 0;
        state.floor = cursor;
        state.cursor = cursor;
        state.finished = false;
        state.terminal = None;
        state.error = None;
        Some(cursor)
    }

    pub(super) fn now_ms(&self) -> u64 {
        self.state
            .lock()
            .unwrap()
            .observed_now_ms
            .max(crate::hosted::now_s().saturating_mul(1000))
    }

    pub(super) fn live(&self, live: bool) {
        self.state.lock().unwrap().live = live;
    }
}

struct Subscriber(Arc<Owner>, u64);
impl Drop for Subscriber {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().subscribers.remove(&self.1);
        self.0.changed.send_replace(());
    }
}

impl Motor {
    pub(super) fn reserve_hosted_result(&self, id: Uuid, cursor: u64) -> Arc<Owner> {
        let mut owners = self.hosted_results.lock().unwrap();
        owners
            .entry(id)
            .or_insert_with(|| Owner::new(cursor))
            .clone()
    }

    pub(super) fn subscribe_hosted_result(
        &self,
        owner: Arc<Owner>,
        mut cursor: u64,
        original: bool,
        tx: mpsc::Sender<io::Result<StreamRecord>>,
    ) {
        let subscriber = {
            let mut state = owner.state.lock().unwrap();
            let subscriber = state.next_subscriber;
            state.next_subscriber += 1;
            state.subscribers.insert(subscriber, cursor);
            subscriber
        };
        tokio::spawn(async move {
            let _subscriber = Subscriber(owner.clone(), subscriber);
            let mut changed = owner.changed.subscribe();
            let mut index = 0;
            let mut accepted = !original;
            loop {
                let (records, terminal, error, request) = {
                    let mut state = owner.state.lock().unwrap();
                    let rewind = cursor < state.floor;
                    let request = rewind && state.rewind.is_none_or(|old| cursor < old);
                    if request {
                        state.rewind = Some(state.rewind.map_or(cursor, |old| old.min(cursor)));
                    }
                    let mut records = Vec::new();
                    if !accepted {
                        if let Some(record) = &state.accepted {
                            records.push(StreamRecord::Accepted(record.clone()));
                            accepted = true;
                        }
                    }
                    if !rewind {
                        for (at, _, record) in &state.ring {
                            if *at < index {
                                continue;
                            }
                            index = at + 1;
                            if let Some(seq) = sequence(record) {
                                if seq < cursor {
                                    continue;
                                }
                                cursor = seq.saturating_add(1);
                                records.push(record.clone());
                            } else if original {
                                records.push(record.clone());
                            }
                        }
                    }
                    (
                        records,
                        (!rewind).then(|| state.terminal.clone()).flatten(),
                        state
                            .subscriber_errors
                            .get(&subscriber)
                            .cloned()
                            .or_else(|| state.finished.then(|| state.error.clone()).flatten()),
                        request,
                    )
                };
                if request {
                    owner.wake.send_replace(true);
                    owner.changed.send_replace(());
                }
                for record in records {
                    if tx.send(Ok(record)).await.is_err() {
                        return;
                    }
                }
                let advanced = {
                    let mut state = owner.state.lock().unwrap();
                    let previous = state.subscribers.insert(subscriber, cursor);
                    previous != Some(cursor)
                };
                if advanced {
                    owner.changed.send_replace(());
                }
                if let Some(error) = error {
                    let _ = tx.send(Err(io::Error::other(error))).await;
                    return;
                }
                if let Some(terminal) = terminal {
                    let _ = tx.send(Ok(StreamRecord::Terminal(terminal))).await;
                    return;
                }
                tokio::select! {
                    _ = tx.closed() => return,
                    result = changed.changed() => if result.is_err() { return; },
                }
            }
        });
    }
}

impl Motor {
    pub(super) fn collect_hosted_result(
        &self,
        id: Uuid,
        owner: Arc<Owner>,
        tx: mpsc::Sender<io::Result<StreamRecord>>,
        mut records: mpsc::Receiver<io::Result<StreamRecord>>,
        mut producer: tokio::task::JoinHandle<()>,
    ) {
        let motor = self.clone();
        tokio::spawn(async move {
            // Unsettled hosted state already prevents pruning. A JobLease would
            // falsely mark a detached collector as a caller during cancel recovery.
            let mut shutdown = motor.hosted_cleanup.shutdown.subscribe();
            let mut changed = owner.changed.subscribe();
            let mut running = true;
            let mut restart_delay = 1;
            loop {
                if *shutdown.borrow() {
                    break;
                }
                if !running {
                    let (complete, subscribers, rewind, cursor) = {
                        let state = owner.state.lock().unwrap();
                        (
                            state.terminal.is_some(),
                            state.subscribers.len(),
                            state.rewind.is_some(),
                            state.cursor,
                        )
                    };
                    if complete && subscribers == 0 {
                        break;
                    }
                    if rewind || (!complete && motor.hosted_state(id).is_some_and(|s| s.run_sent)) {
                        if !rewind {
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_secs(restart_delay)) => {},
                                _ = shutdown.changed() => break,
                            }
                            restart_delay = (restart_delay * 2).min(30);
                        }
                        {
                            let mut state = owner.state.lock().unwrap();
                            state.finished = false;
                            state.error = None;
                        }
                        producer =
                            motor.hosted_result_follow(id, owner.clone(), tx.clone(), cursor, None);
                        running = true;
                    } else if !complete {
                        break;
                    }
                }
                tokio::select! {
                    record = records.recv() => if let Some(record) = record { owner.publish(record); },
                    result = &mut producer, if running => {
                        if let Err(error) = result { owner.publish(Err(io::Error::other(error))); }
                        while let Ok(record) = records.try_recv() { owner.publish(record); }
                        {
                            let mut state = owner.state.lock().unwrap();
                            state.finished = true;
                            state.live = false;
                        }
                        running = false;
                        owner.changed.send_replace(());
                    },
                    _ = changed.changed() => {},
                    _ = shutdown.changed() => {},
                }
            }
            if running {
                producer.abort();
                let _ = producer.await;
            }
            {
                let mut owners = motor.hosted_results.lock().unwrap();
                if owners
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(current, &owner))
                {
                    owners.remove(&id);
                }
            }
            owner.state.lock().unwrap().finished = true;
            owner.stopped.send_replace(true);
            owner.changed.send_replace(());
            motor.recover_hosted(id);
        });
    }

    fn hosted_result_follow(
        &self,
        id: Uuid,
        owner: Arc<Owner>,
        tx: mpsc::Sender<io::Result<StreamRecord>>,
        cursor: u64,
        probe_at: Option<u64>,
    ) -> tokio::task::JoinHandle<()> {
        let motor = self.clone();
        tokio::spawn(async move {
            let delivery = motor.hosted_cancels.lock().unwrap().get(&id).cloned();
            if let Some(mut delivery) = delivery {
                loop {
                    if delivery.borrow().is_some() {
                        break;
                    }
                    if delivery.changed().await.is_err() {
                        break;
                    }
                }
            }
            if let Some(now) = probe_at {
                let reply = motor.probe_hosted_result(id, now, &owner).await;
                owner.state.lock().unwrap().reply = Some(reply.unwrap_or_else(|| empty_status(id)));
                owner.changed.send_replace(());
            }
            match motor.recover_recorded_unknown(id, &owner, &tx).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
            if let Err(error) = motor
                .follow(id, cursor, None, &tx, owner.wake.subscribe())
                .await
            {
                let _ = tx.send(Err(error)).await;
            }
        })
    }

    async fn recover_recorded_unknown(
        &self,
        id: Uuid,
        owner: &Arc<Owner>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) -> io::Result<bool> {
        let recorded = self.hosted_state(id).is_some_and(|state| {
            !state.cancel_settlement_ready()
                && state.terminal.is_some_and(|terminal| {
                    matches!(terminal.outcome, motor_protocol::Outcome::OutcomeUnknown)
                        && terminal.refusal_detail.is_some()
                })
        });
        if !recorded {
            return Ok(false);
        }
        // Older clients can have published an unknown outcome while cancellation
        // remains unconfirmed. Preserve that result and retry durable cancellation;
        // this is not permission to synthesize a new unknown for a live job.
        let mut backoff = 1;
        loop {
            if !self.claim_hosted_result_transport(id, owner) {
                if let Some(terminal) = self.storage.job(id).and_then(|job| job.local_terminal) {
                    emit(tx, StreamRecord::Terminal(terminal)).await?;
                }
                return Ok(true);
            }
            let result = self.finish_hosted(id, false, tx).await;
            owner.live(false);
            result?;
            let Some(state) = self.hosted_state(id) else {
                return Ok(true);
            };
            if !self.pending(id) || state.finish_done {
                return Ok(!state.booked_or_attempted()
                    || state.finish_done
                    || state.billing != crate::BillingStatus::Pending);
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
        }
    }

    pub(super) fn ensure_hosted_result(&self, id: Uuid, probe_at: Option<u64>) -> Arc<Owner> {
        let mut owners = self.hosted_results.lock().unwrap();
        if let Some(owner) = owners.get(&id) {
            return owner.clone();
        }
        let owner = Owner::new(u64::MAX);
        owners.insert(id, owner.clone());
        drop(owners);
        let (tx, records) = mpsc::channel(32);
        let producer = self.hosted_result_follow(id, owner.clone(), tx.clone(), u64::MAX, probe_at);
        self.collect_hosted_result(id, owner.clone(), tx, records, producer);
        owner
    }
}

impl Motor {
    async fn probe_hosted_result(
        &self,
        id: Uuid,
        now: u64,
        owner: &Owner,
    ) -> Option<JobStatusReply> {
        if !self.claim_hosted_result_transport(id, owner) {
            return None;
        }
        let mut failure = None;
        let probe = async {
            let mut channel = match self.open_job_channel_at(id, now).await {
                Ok(channel) => channel,
                Err(error) => {
                    failure = Some(error);
                    return None;
                }
            };
            let runner_id = self.build_server_id(id);
            channel
                .send(Frame::JobStatus { job_id: runner_id })
                .await
                .ok()?;
            match channel.next().await.ok()? {
                Incoming::Frame(Frame::JobStatusReply(mut reply)) if reply.job_id == runner_id => {
                    reply.job_id = id;
                    if let Some(terminal) = &mut reply.terminal {
                        if terminal.job_id != runner_id {
                            return None;
                        }
                        terminal.job_id = id;
                        terminal.refusal_detail =
                            terminal.refusal_detail.take().map(bounded_detail);
                        self.storage
                            .hosted_update(id, |state| {
                                if state
                                    .terminal
                                    .as_ref()
                                    .is_none_or(|t| matches!(t.outcome, Outcome::OutcomeUnknown))
                                {
                                    state.terminal = Some(terminal.clone());
                                }
                            })
                            .ok()?;
                        self.storage.terminal_observed(id).ok()?;
                    }
                    Some(reply)
                }
                _ => None,
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(2), probe)
            .await
            .ok()
            .flatten();
        owner.live(false);
        if let Some(error) = failure {
            let _ = self.retire_failed_hosted_result(id, error, now);
        }
        result
    }

    pub(super) async fn hosted_result_status(&self, id: Uuid, now: u64) -> JobStatusReply {
        let already_owned = self.hosted_results.lock().unwrap().contains_key(&id);
        let owner = self.ensure_hosted_result(id, Some(now));
        {
            let mut state = owner.state.lock().unwrap();
            state.observed_now_ms = state.observed_now_ms.max(now);
        }
        let mut changed = owner.changed.subscribe();
        loop {
            let reply = {
                let state = owner.state.lock().unwrap();
                let terminal = self.hosted_state(id).and_then(|s| s.terminal);
                if already_owned
                    || terminal.is_some()
                    || state.reply.is_some()
                    || state.accepted.is_some()
                    || state.error.is_some()
                {
                    let mut reply = state.reply.clone().unwrap_or_else(|| empty_status(id));
                    if state.accepted.is_some() && reply.state == "unknown" {
                        reply.state = "uploading".into();
                    }
                    if state.started {
                        reply.state = "running".into();
                    }
                    if let Some(terminal) = terminal {
                        reply.state = "terminal".into();
                        reply.queue_wait_ms = Some(terminal.queue_wait_ms);
                        reply.terminal = Some(terminal);
                    }
                    Some(reply)
                } else {
                    None
                }
            };
            if let Some(reply) = reply {
                let retired_without_subscribers =
                    self.storage.job(id).is_some_and(|job| job.health_retired())
                        && owner.state.lock().unwrap().subscribers.is_empty();
                if retired_without_subscribers {
                    let mut stopped = owner.stopped.subscribe();
                    let _ = stopped.wait_for(|done| *done).await;
                }
                return reply;
            }
            if changed.changed().await.is_err() {
                return empty_status(id);
            }
        }
    }

    pub(super) async fn stop_hosted_results(&self) {
        let owners: Vec<_> = self
            .hosted_results
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for owner in owners {
            let mut stopped = owner.stopped.subscribe();
            let _ = stopped.wait_for(|done| *done).await;
        }
    }
}

fn empty_status(id: Uuid) -> JobStatusReply {
    JobStatusReply {
        job_id: id,
        state: "unknown".into(),
        waiting_on: None,
        queue_wait_ms: None,
        terminal: None,
        unit: None,
        unit_alive: None,
    }
}

impl Owner {
    pub(super) async fn forward_wake(
        &self,
        mut wake: watch::Receiver<bool>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) {
        let mut stopped = self.stopped.subscribe();
        while !*stopped.borrow_and_update() {
            tokio::select! {
                _ = tx.closed() => break,
                _ = stopped.changed() => {},
                changed = wake.changed() => {
                    if changed.is_err() { break; }
                    self.wake.send_replace(true);
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn wait_stopped(&self) {
        let mut stopped = self.stopped.subscribe();
        let _ = stopped.wait_for(|done| *done).await;
    }

    /// Reporting final usage for a Boat booking removes its replay endpoint. Give
    /// callers attached at terminal time a bounded chance to drain or rewind.
    pub(super) async fn wait_boat_subscribers(&self, cursor: u64) -> bool {
        let mut changed = self.changed.subscribe();
        let (deadline, cohort) = {
            let mut state = self.state.lock().unwrap();
            let subscribers: Vec<_> = state.subscribers.keys().copied().collect();
            state
                .boat_drain
                .get_or_insert_with(|| {
                    (
                        tokio::time::Instant::now() + Duration::from_secs(60),
                        subscribers,
                    )
                })
                .clone()
        };
        loop {
            {
                let mut state = self.state.lock().unwrap();
                let behind: Vec<_> = cohort
                    .iter()
                    .filter_map(|id| {
                        state
                            .subscribers
                            .get(id)
                            .filter(|next| **next < cursor)
                            .map(|next| (*id, *next))
                    })
                    .collect();
                if behind.is_empty() {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    for (id, next) in behind {
                        state.subscriber_errors.insert(id, format!(
                            "hosted replay ended while subscriber cursor {next} was behind terminal cursor {cursor}; query exec.status or exec.attach at u64::MAX"
                        ));
                    }
                    state.rewind = None;
                    drop(state);
                    self.changed.send_replace(());
                    return true;
                }
                if state.rewind.is_some() {
                    return false;
                }
            }
            tokio::select! {
                _ = changed.changed() => {},
                _ = tokio::time::sleep_until(deadline) => {},
            }
        }
    }
}

impl Motor {
    pub(super) fn retire_idle_hosted_result(
        &self,
        id: Uuid,
        now: u64,
        cause: &str,
        broker_finished: bool,
    ) -> io::Result<bool> {
        let owners = self.hosted_results.lock().unwrap();
        // Hold the owner's transport gate through the store write, not just the
        // liveness check: reconnects can use an Arc obtained before retirement.
        let state = owners.get(&id).map(|owner| owner.state.lock().unwrap());
        if state.as_ref().is_some_and(|state| state.live)
            || self
                .hosted_state(id)
                .is_some_and(|state| state.terminal.is_some())
        {
            return Ok(false);
        }
        self.storage
            .retire_hosted_job(id, cause, now, broker_finished)
    }

    pub(super) fn claim_hosted_result_transport(&self, id: Uuid, owner: &Owner) -> bool {
        let owners = self.hosted_results.lock().unwrap();
        if !owners
            .get(&id)
            .is_some_and(|current| std::ptr::eq(current.as_ref(), owner))
        {
            return false;
        }
        let mut state = owner.state.lock().unwrap();
        if self.storage.job(id).is_none_or(|job| job.health_retired()) {
            return false;
        }
        state.live = true;
        true
    }
}

impl Motor {
    pub(super) fn retire_failed_hosted_result(
        &self,
        id: Uuid,
        error: crate::hosted::Error,
        now: u64,
    ) -> io::Result<bool> {
        let expired = now >= self.hosted_result_deadline(id);
        let (cause, broker_finished) = match error {
            crate::hosted::Error::Finished => ("broker reports the job finished; no result reached ck-motor", true),
            crate::hosted::Error::AttachAuth if expired => ("host refused reattach with auth past the job's deadline; no result reached ck-motor", false),
            crate::hosted::Error::ReattachUnknown if expired => ("broker no longer knows the job past its deadline; no result reached ck-motor", true),
            _ => return Ok(false),
        };
        self.retire_idle_hosted_result(id, now, cause, broker_finished)
    }

    pub(super) fn hosted_result_deadline(&self, id: Uuid) -> u64 {
        self.hosted_state(id)
            .and_then(|state| state.reattach_deadline_s())
            .map(|seconds| seconds.saturating_mul(1000))
            .or_else(|| self.storage.job(id).map(|job| health::deadline(id, &job)))
            .unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retirement_and_new_transport_claim_are_mutually_exclusive() {
        let scratch = cortexkit_test_support::ScratchDir::new("hosted-owner-retirement-claim");
        let motor = Motor::with_connector(
            crate::Config::default(),
            &scratch,
            Arc::new(crate::hosted::tests::harness::NoSsh(
                std::sync::atomic::AtomicUsize::new(0),
            )),
        )
        .unwrap();
        let id = Uuid::now_v7();
        motor.storage.create(id, 60, None, None, false).unwrap();
        let mut route = crate::HostedJobState::new(&motor.config);
        route.run_sent = true;
        motor.storage.hosted_route(id, route).unwrap();
        let owner = motor.reserve_hosted_result(id, u64::MAX);
        owner.live(false);
        assert!(motor.claim_hosted_result_transport(id, &owner));
        assert!(!motor
            .retire_idle_hosted_result(id, u64::MAX, "expired", false)
            .unwrap());
        assert!(motor.storage.job(id).unwrap().local_terminal.is_none());
        owner.live(false);
        assert!(motor
            .retire_idle_hosted_result(id, u64::MAX, "expired", false)
            .unwrap());
        assert!(!motor.claim_hosted_result_transport(id, &owner));
        motor.hosted_results.lock().unwrap().remove(&id);
        motor.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn boat_finish_waits_only_for_terminal_time_subscribers_and_detach_unblocks_it() {
        let owner = Owner::new(0);
        owner.state.lock().unwrap().subscribers.insert(0, 7);
        let drain = tokio::spawn({
            let owner = owner.clone();
            async move { owner.wait_boat_subscribers(10).await }
        });
        tokio::task::yield_now().await;
        assert!(owner.state.lock().unwrap().boat_drain.is_some());
        assert!(!drain.is_finished());
        {
            let mut state = owner.state.lock().unwrap();
            state.subscribers.insert(1, 0);
            state.subscribers.remove(&0);
        }
        owner.changed.send_replace(());
        assert!(drain.await.unwrap());
        assert!(owner.state.lock().unwrap().subscriber_errors.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn boat_finish_bounds_lagging_subscriber_wait_to_sixty_seconds() {
        let owner = Owner::new(0);
        owner.state.lock().unwrap().subscribers.insert(0, 7);
        let drain = tokio::spawn({
            let owner = owner.clone();
            async move { owner.wait_boat_subscribers(10).await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!drain.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(drain.await.unwrap());
        let state = owner.state.lock().unwrap();
        assert!(state.subscriber_errors[&0].contains("cursor 7"));
        assert!(state.subscriber_errors[&0].contains("terminal cursor 10"));
    }

    #[tokio::test(start_paused = true)]
    async fn boat_finish_without_subscribers_never_waits() {
        let owner = Owner::new(0);
        let start = tokio::time::Instant::now();
        assert!(owner.wait_boat_subscribers(10).await);
        assert_eq!(tokio::time::Instant::now(), start);
    }
}
