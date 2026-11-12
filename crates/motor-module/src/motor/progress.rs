use motor_protocol::{Progress, ProgressPhase, ProgressWaitingOn, QueueStatus, StreamRecord, Uuid};
use std::{future::Future, io, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

#[derive(Clone)]
struct Phase {
    kind: ProgressPhase,
    started: Instant,
}

/// What a queued job is waiting on, as the caller sees it in `queue_wait`
/// progress. Two observations that map to the same value are one wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueueWait {
    pub(crate) waiting_on: Option<ProgressWaitingOn>,
    pub(crate) blocker_job_id: Option<Uuid>,
}

impl QueueWait {
    /// Maps a runner or host queue status onto the caller's wait reasons.
    /// Only a job still queued is waiting; upload and running states are not
    /// queue waits and return `None`.
    pub(crate) fn from_status(status: &QueueStatus) -> Option<Self> {
        if status.state != "queued" {
            return None;
        }
        Some(Self {
            waiting_on: status.waiting_on.as_deref().and_then(waiting_on),
            // The runner and the per-job host name a blocker only from the
            // waiting job's own account, never another account's job, so it
            // can be passed on as is.
            blocker_job_id: status.blocker.as_ref().map(|blocker| blocker.job_id),
        })
    }
}

/// Maps a runner or host `waiting_on` label onto the caller's wait reason.
/// Labels with no matching reason, such as workspace setup or a queued
/// workspace job, report no reason rather than a guessed one.
pub(crate) fn waiting_on(label: &str) -> Option<ProgressWaitingOn> {
    match label {
        "workspace owner" => Some(ProgressWaitingOn::Checkout),
        "sibling user of this checkout" | "sibling checkout owner" => {
            Some(ProgressWaitingOn::SiblingCheckout)
        }
        "permits"
        | "gate job ahead"
        | "cache fill ahead needs more free permits"
        | "older job ahead needs more free permits"
        | "demand exceeds capacity" => Some(ProgressWaitingOn::Capacity),
        // The runner appends the byte counts to this label.
        label if label.starts_with("returned bytes budget") => Some(ProgressWaitingOn::Capacity),
        "memory" => Some(ProgressWaitingOn::Memory),
        "checkout disk" => Some(ProgressWaitingOn::CheckoutDisk),
        _ => None,
    }
}

/// Lives only on the original submission stream, never in durable job state.
#[derive(Clone)]
pub(crate) struct Reporter {
    phase: watch::Sender<Option<Phase>>,
    queue: watch::Sender<QueueState>,
    enabled: bool,
    vcpus: u32,
    job_id: Option<motor_protocol::Uuid>,
}

/// What the heartbeats have said about the queue so far.
#[derive(Clone, PartialEq)]
enum QueueState {
    /// No queue status has arrived.
    Unreported,
    Waiting(QueueWait),
    /// The job started; later heartbeats are not queue waits.
    Started,
}

impl Reporter {
    pub(crate) fn new(enabled: bool, vcpus: u32) -> Self {
        let (phase, _) = watch::channel(None);
        let (queue, _) = watch::channel(QueueState::Unreported);
        Self {
            phase,
            queue,
            enabled,
            vcpus,
            job_id: None,
        }
    }

    pub(crate) fn with_job_id(mut self, id: motor_protocol::Uuid) -> Self {
        self.job_id = Some(id);
        self
    }

    pub(crate) fn initial(&self) -> StreamRecord {
        self.record(&Phase {
            kind: ProgressPhase::PlacementWait,
            started: Instant::now(),
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn current_phase(&self) -> Option<ProgressPhase> {
        self.phase.borrow().as_ref().map(|phase| phase.kind.clone())
    }

    pub(crate) fn phase(&self, kind: ProgressPhase) {
        self.phase.send_if_modified(|current| {
            if current.as_ref().is_some_and(|phase| phase.kind == kind) {
                return false;
            }
            *current = Some(Phase {
                kind,
                started: Instant::now(),
            });
            true
        });
    }

    pub(crate) fn clear(&self) {
        self.phase.send_replace(None);
    }

    /// Records the queue status from a heartbeat. Only a change in what the
    /// job waits on wakes the forwarder, so repeated heartbeats send nothing.
    pub(crate) fn queue_status(&self, status: &QueueStatus) {
        let Some(wait) = QueueWait::from_status(status) else {
            return;
        };
        self.queue.send_if_modified(|current| match current {
            QueueState::Started => false,
            QueueState::Waiting(current) if *current == wait => false,
            _ => {
                *current = QueueState::Waiting(wait);
                true
            }
        });
    }

    /// Marks the job started, in the order the job stream reader saw it among the
    /// heartbeats, so a heartbeat read after `Started` is never reported even
    /// while the `Started` record is still on its way to the forwarder.
    pub(crate) fn queue_started(&self) {
        self.queue.send_if_modified(|current| {
            let changed = *current != QueueState::Started;
            *current = QueueState::Started;
            changed
        });
    }

    fn queue_record(&self, wait: &QueueWait, since: Instant) -> StreamRecord {
        let elapsed = u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut record = Progress::new(ProgressPhase::QueueWait, elapsed, None)
            .expect("progress without detail is valid");
        if let Some(id) = self.job_id {
            record = record.with_job_id(id);
        }
        if let Some(waiting_on) = wait.waiting_on.clone() {
            record = record.with_waiting_on(waiting_on);
        }
        if let Some(blocker) = wait.blocker_job_id {
            record = record.with_blocker_job_id(blocker);
        }
        StreamRecord::Progress(record)
    }

    fn record(&self, phase: &Phase) -> StreamRecord {
        let elapsed = u64::try_from(phase.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Only fixed wording and a resource count enter detail, never routing or request text.
        let detail = match phase.kind {
            ProgressPhase::PlacementWait => {
                Some(format!("waiting for a host with {} free vCPUs", self.vcpus))
            }
            ProgressPhase::Connect => Some("connecting to a runner".into()),
            ProgressPhase::Upload => Some("uploading inputs".into()),
            _ => None,
        };
        let record = Progress::new(phase.kind.clone(), elapsed, detail)
            .unwrap_or_else(|_| Progress::new(phase.kind.clone(), elapsed, None).unwrap());
        let record = match self.job_id {
            Some(id) => record.with_job_id(id),
            None => record,
        };
        StreamRecord::Progress(record)
    }

    pub(crate) async fn forward<F>(
        &self,
        future: F,
        mut records: mpsc::Receiver<io::Result<StreamRecord>>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) -> io::Result<()>
    where
        F: Future<Output = io::Result<()>>,
    {
        let mut phases = self.phase.subscribe();
        let mut queue = self.queue.subscribe();
        let mut last_sent = None;
        let mut stopped = false;
        // Queue-wait reporting runs from the first Accepted until Started or
        // the terminal record; `queue_since` holds when it began.
        let mut queue_since: Option<Instant> = None;
        let mut queue_done = false;
        let mut queue_sent: Option<QueueWait> = None;
        tokio::pin!(future);
        loop {
            let phase = phases.borrow_and_update().clone();
            let deadline = phase.as_ref().filter(|_| !stopped).map(|phase| {
                let quiet = phase.started + Duration::from_secs(2);
                last_sent.map_or(quiet, |last: Instant| {
                    quiet.max(last + Duration::from_secs(10))
                })
            });
            let timer = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                result = &mut future => {
                    // A completed submission may still have buffered its final records.
                    while let Ok(record) = records.try_recv() {
                        tx.send(record).await.map_err(|_| io::Error::other("caller detached"))?;
                    }
                    return result;
                }
                record = records.recv() => {
                    let Some(record) = record else { return Err(io::Error::other("submission stream closed")); };
                    // Forward acceptance and progress in one task so a timer cannot race
                    // an Accepted record or resume reporting when a queued job moves hosts.
                    stopped |= matches!(&record, Ok(StreamRecord::Accepted(_) | StreamRecord::Terminal(_)));
                    if matches!(&record, Ok(StreamRecord::Accepted(_))) && !queue_done {
                        // A heartbeat that arrived before Accepted is reported
                        // below, once this record has gone out.
                        queue_since.get_or_insert_with(Instant::now);
                    }
                    if matches!(&record, Ok(StreamRecord::Started(_) | StreamRecord::Terminal(_))) {
                        queue_done = true;
                        queue_since = None;
                    }
                    tx.send(record).await.map_err(|_| io::Error::other("caller detached"))?;
                }
                _ = phases.changed() => {},
                _ = queue.changed(), if queue_since.is_some() => {},
                _ = timer => {
                    // Losing a display-only record must not cancel an in-flight booking.
                    if tx.send(Ok(self.record(phase.as_ref().unwrap()))).await.is_err() {
                        stopped = true;
                    }
                    last_sent = Some(Instant::now());
                }
            }
            // Sent once per change of what the job waits on, never on a timer:
            // an unchanged wait repeats nothing.
            if let Some(since) = queue_since {
                let state = queue.borrow_and_update().clone();
                match state {
                    QueueState::Waiting(wait) if queue_sent.as_ref() != Some(&wait) => {
                        // Losing a display-only record must not cancel the job; stop
                        // reporting queue waits to a caller that no longer reads them.
                        if tx.send(Ok(self.queue_record(&wait, since))).await.is_err() {
                            queue_done = true;
                            queue_since = None;
                        }
                        queue_sent = Some(wait);
                    }
                    QueueState::Started => {
                        queue_done = true;
                        queue_since = None;
                    }
                    _ => {}
                }
            }
        }
    }
}
