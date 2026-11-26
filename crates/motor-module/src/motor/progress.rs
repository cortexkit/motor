use motor_protocol::{Progress, ProgressPhase, ProgressWaitingOn, QueueStatus, StreamRecord, Uuid};
use std::{
    future::Future,
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

pub(crate) const UPDATE_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Phase {
    kind: ProgressPhase,
    started: Instant,
    upload: Option<UploadCount>,
}

#[derive(Clone)]
struct UploadCount {
    sent: Arc<AtomicU64>,
    total: u64,
}

pub(super) struct Upload<'a> {
    reporter: &'a Reporter,
    sent: Arc<AtomicU64>,
}

impl Upload<'_> {
    pub(super) fn advance(&self, bytes: u64) {
        self.sent.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for Upload<'_> {
    fn drop(&mut self) {
        // An interrupted upload must stop reporting just like a finished one.
        self.reporter.clear();
    }
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
        self.record(
            &Phase {
                kind: ProgressPhase::PlacementWait,
                started: Instant::now(),
                upload: None,
            },
            None,
        )
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
                upload: None,
            });
            true
        });
    }

    pub(crate) fn clear(&self) {
        self.phase.send_replace(None);
    }

    pub(super) fn upload(&self, total: u64) -> Option<Upload<'_>> {
        if !self.enabled || total == 0 {
            return None;
        }
        // The timer samples the counter; chunks do not wake the display task.
        let sent = Arc::new(AtomicU64::new(0));
        self.phase.send_replace(Some(Phase {
            kind: ProgressPhase::Upload,
            started: Instant::now(),
            upload: Some(UploadCount {
                sent: sent.clone(),
                total,
            }),
        }));
        Some(Upload {
            reporter: self,
            sent,
        })
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

    fn upload_detail(sent: u64, total: u64) -> String {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        let (divisor, unit) = if total < MIB {
            (KIB, "KB")
        } else {
            (MIB, "MB")
        };
        format!(
            "uploading {} of {} {unit}",
            sent / divisor,
            total.div_ceil(divisor)
        )
    }

    fn record(&self, phase: &Phase, upload: Option<(u64, u64)>) -> StreamRecord {
        let elapsed = u64::try_from(phase.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Only fixed wording and a resource count enter detail, never routing or request text.
        let detail = match phase.kind {
            ProgressPhase::PlacementWait => {
                Some(format!("waiting for a host with {} free vCPUs", self.vcpus))
            }
            ProgressPhase::Connect => Some("connecting to a runner".into()),
            ProgressPhase::Upload => Some(match upload {
                Some((sent, total)) => Self::upload_detail(sent, total),
                None => "uploading inputs".into(),
            }),
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
        // A slow caller can delay ordered output but must not stall the upload.
        // Leave further ordered records in the submission channel and coalesce
        // display updates into a single slot.
        let mut pending_ordered = None;
        let mut pending_display = None;
        let mut last_tick = None;
        let mut stopped = false;
        // Queue-wait reporting runs from the first Accepted until Started or
        // the terminal record; `queue_since` holds when it began.
        let mut queue_since: Option<Instant> = None;
        let mut queue_done = false;
        let mut last_queue_wait: Option<QueueWait> = None;
        let mut upload_sample: Option<(Instant, u64, bool)> = None;
        tokio::pin!(future);
        let result = loop {
            let phase = phases.borrow_and_update().clone();
            let upload = phase.as_ref().and_then(|phase| {
                let count = phase.upload.as_ref()?;
                queue_since.map(|_| (phase.started, count))
            });
            let previous_sample =
                upload.and_then(|(started, _)| upload_sample.filter(|(at, _, _)| *at == started));
            // Give the upload a reporting interval to move. A host can stop
            // reading once it has buffered all it can for a queued job. If
            // the count stops increasing, let the queue reason explain why.
            let moving = upload.is_some_and(|(_, count)| {
                previous_sample.is_none_or(|(_, previous, stalled)| {
                    !stalled || previous != count.sent.load(Ordering::Relaxed)
                })
            });
            if upload.is_none()
                && matches!(&pending_display, Some(StreamRecord::Progress(p)) if stopped && p.phase() == &ProgressPhase::Upload)
            {
                pending_display = None;
            }
            if moving
                && matches!(&pending_display, Some(StreamRecord::Progress(p)) if p.phase() == &ProgressPhase::QueueWait)
            {
                pending_display = None;
                last_queue_wait = None;
            }
            // Report once per change of what the job waits on, never on a timer:
            // an unchanged wait repeats nothing unless Upload replaced it.
            if let Some(since) = queue_since {
                let state = queue.borrow_and_update().clone();
                match state {
                    QueueState::Waiting(wait)
                        if !moving && last_queue_wait.as_ref() != Some(&wait) =>
                    {
                        pending_display = Some(self.queue_record(&wait, since));
                        last_queue_wait = Some(wait);
                    }
                    QueueState::Started => {
                        queue_done = true;
                        queue_since = None;
                        pending_display = None;
                    }
                    _ => {}
                }
            }
            let deadline = phase
                .as_ref()
                .filter(|phase| {
                    if phase.upload.is_some() {
                        queue_since.is_some()
                    } else {
                        !stopped
                    }
                })
                .map(|phase| {
                    let quiet = phase.started + Duration::from_secs(2);
                    last_tick.map_or(quiet, |last: Instant| quiet.max(last + UPDATE_INTERVAL))
                });
            let timer = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                result = &mut future => break result,
                record = records.recv(), if pending_ordered.is_none() => {
                    let Some(record) = record else { return Err(io::Error::other("submission stream closed")); };
                    // Placement and connection progress cannot resume after acceptance.
                    if !stopped && matches!(&record, Ok(StreamRecord::Accepted(_) | StreamRecord::Terminal(_))) {
                        stopped = true;
                        pending_display = None;
                    }
                    if matches!(&record, Ok(StreamRecord::Accepted(_))) && !queue_done {
                        // Only deliver a queue wait after the Accepted record.
                        queue_since.get_or_insert_with(Instant::now);
                    }
                    if matches!(&record, Ok(StreamRecord::Started(_) | StreamRecord::Terminal(_))) {
                        queue_done = true;
                        if queue_since.take().is_some() {
                            pending_display = None;
                        }
                    }
                    pending_ordered = Some(record);
                }
                _ = queue.changed(), if queue_since.is_some() => {},
                _ = phases.changed() => {},
                // Reserving can be interrupted without consuming either record.
                permit = tx.reserve(), if pending_ordered.is_some() || pending_display.is_some() => {
                    if let Some(record) = pending_ordered.take() {
                        permit.map_err(|_| io::Error::other("caller detached"))?.send(record);
                    } else if let Some(record) = pending_display.take() {
                        match permit {
                            Ok(permit) => permit.send(Ok(record)),
                            Err(_) => {
                                // Losing display progress must not cancel the submission.
                                stopped = true;
                                queue_done = true;
                                queue_since = None;
                            }
                        }
                    }
                }
                _ = timer => {
                    last_tick = Some(Instant::now());
                    let phase = phase.as_ref().unwrap();
                    if let Some((started, count)) = upload {
                        let sent = count.sent.load(Ordering::Relaxed);
                        let stalled = previous_sample.map_or(0, |(_, previous, _)| previous) == sent;
                        upload_sample = Some((started, sent, stalled));
                        if !stalled {
                            pending_display = Some(self.record(phase, Some((sent, count.total))));
                            last_queue_wait = None;
                        }
                    } else {
                        pending_display = Some(self.record(phase, None));
                    }
                }
            }
        };
        drop(pending_display);
        // The submission no longer needs polling, but its final ordered records
        // still need to reach the caller.
        if let Some(record) = pending_ordered {
            tx.send(record)
                .await
                .map_err(|_| io::Error::other("caller detached"))?;
        }
        while let Ok(record) = records.try_recv() {
            tx.send(record)
                .await
                .map_err(|_| io::Error::other("caller detached"))?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::Reporter;

    #[test]
    fn upload_detail_rounds_sent_down_and_total_up_in_readable_units() {
        const MIB: u64 = 1024 * 1024;
        for (sent, total, expected) in [
            (0, 0, "uploading 0 of 0 KB"),
            (0, 1, "uploading 0 of 1 KB"),
            (MIB - 2, MIB - 1, "uploading 1023 of 1024 KB"),
            (MIB - 1, MIB, "uploading 0 of 1 MB"),
            (MIB, MIB + 1, "uploading 1 of 2 MB"),
            (513 * MIB - 1, 1089 * MIB + 1, "uploading 512 of 1090 MB"),
        ] {
            assert_eq!(Reporter::upload_detail(sent, total), expected);
        }
    }
}
