use motor_protocol::{Progress, ProgressPhase, StreamRecord};
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

/// Lives only on the original submission stream, never in durable job state.
#[derive(Clone)]
pub(crate) struct Reporter {
    phase: watch::Sender<Option<Phase>>,
    enabled: bool,
    vcpus: u32,
}

impl Reporter {
    pub(crate) fn new(enabled: bool, vcpus: u32) -> Self {
        let (phase, _) = watch::channel(None);
        Self {
            phase,
            enabled,
            vcpus,
        }
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
        let mut last_sent = None;
        let mut stopped = false;
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
                    tx.send(record).await.map_err(|_| io::Error::other("caller detached"))?;
                }
                _ = phases.changed() => {},
                _ = timer => {
                    // Losing a display-only record must not cancel an in-flight booking.
                    if tx.send(Ok(self.record(phase.as_ref().unwrap()))).await.is_err() {
                        stopped = true;
                    }
                    last_sent = Some(Instant::now());
                }
            }
        }
    }
}
