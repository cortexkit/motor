//! Bounded observation of cache setup running on one cancellable worker thread.

use serde::{Deserialize, Serialize};
use std::{
    fmt, io,
    sync::mpsc,
    time::{Duration, Instant},
};

fn diagnostic_message(message: &str) -> String {
    let mut bytes = 0;
    message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take_while(|c| {
            bytes += c.len_utf8();
            bytes <= 1024
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStep {
    UserLogon,
    DiskSelection,
    DiskOpen,
    DiskSerial,
    WindowsDirectory,
    WindowsMountPath,
    WindowsVolumeName,
    VolumeOpen,
    VolumeExtents,
    DiskLayout,
    GetAttributes,
    SetAttributes,
    UpdateProperties,
    FirstVolume,
    NextVolume,
    VolumeInformation,
    InitializeRaw,
    VolumeDiscovery,
    MountDirectory,
    MountPoint,
    DirtyOpen,
    DirtyCheck,
    FolderAcl,
    VolumeAcl,
}

/// Cumulative time per operation. Nested phases overlap and are not additive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepTiming {
    pub step: CacheStep,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheFailure {
    pub step: CacheStep,
    pub elapsed_ms: u64,
    pub message: String,
    pub completed: Vec<StepTiming>,
}
impl fmt::Display for CacheFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cache mount {:?} after {} ms: {}; completed={:?}",
            self.step, self.elapsed_ms, self.message, self.completed
        )
    }
}
impl CacheFailure {
    pub fn validate(&self) -> io::Result<()> {
        if self.message.len() > 1024
            || self.message.chars().any(char::is_control)
            || self.completed.len() > 32
        {
            return Err(io::Error::other("invalid cache failure diagnostic"));
        }
        for (i, timing) in self.completed.iter().enumerate() {
            if self.completed[..i].iter().any(|t| t.step == timing.step) {
                return Err(io::Error::other("duplicate cache step timing"));
            }
        }
        Ok(())
    }
}

pub enum StepEvent {
    Begin(CacheStep),
    End(CacheStep, u64, bool),
}
#[derive(Clone)]
pub struct CacheReporter(mpsc::Sender<StepEvent>);
impl CacheReporter {
    pub fn new(sender: mpsc::Sender<StepEvent>) -> Self {
        Self(sender)
    }
    pub fn step<T>(
        &self,
        step: CacheStep,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        self.0
            .send(StepEvent::Begin(step))
            .map_err(|_| io::Error::other("cache observer stopped"))?;
        let started = Instant::now();
        let result = operation();
        self.0
            .send(StepEvent::End(
                step,
                started.elapsed().as_millis() as u64,
                result.is_err(),
            ))
            .map_err(|_| io::Error::other("cache observer stopped"))?;
        result.map_err(|error| io::Error::new(error.kind(), format!("{step:?}: {error}")))
    }
}

pub struct CacheLimits {
    pub native: Duration,
    pub total: Duration,
    pub heartbeat: Duration,
}
impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            native: Duration::from_secs(3),
            total: Duration::from_secs(45),
            heartbeat: Duration::from_secs(1),
        }
    }
}
impl CacheLimits {
    fn step(&self, step: CacheStep) -> Duration {
        match step {
            CacheStep::InitializeRaw => Duration::from_secs(30),
            CacheStep::UserLogon => Duration::from_secs(10),
            CacheStep::DiskSelection | CacheStep::VolumeDiscovery => Duration::from_secs(5),
            _ => self.native,
        }
    }
}

pub fn watch_cache<T>(
    events: mpsc::Receiver<StepEvent>,
    result: mpsc::Receiver<io::Result<T>>,
    mut progress: impl FnMut(CacheStep, u64) -> io::Result<()>,
    cancel: impl FnOnce(),
    limits: CacheLimits,
) -> Result<T, CacheFailure> {
    let started = Instant::now();
    let mut active = Vec::new();
    let mut completed: Vec<StepTiming> = Vec::new();
    let mut last = CacheStep::UserLogon;
    let mut first_failed = None;
    let mut heartbeat = started;
    let mut cancel = Some(cancel);
    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                StepEvent::Begin(step) => {
                    active.push((step, Instant::now()));
                    last = step;
                    first_failed = None;
                }
                StepEvent::End(step, elapsed_ms, failed) => {
                    active.pop();
                    if failed && first_failed.is_none() {
                        first_failed = Some(step);
                    }
                    last = first_failed.unwrap_or(step);
                    if let Some(timing) = completed.iter_mut().find(|t| t.step == step) {
                        timing.elapsed_ms = timing.elapsed_ms.saturating_add(elapsed_ms);
                    } else {
                        completed.push(StepTiming { step, elapsed_ms });
                    }
                }
            }
        }
        let expired = active
            .iter()
            .find(|(step, at)| at.elapsed() >= limits.step(*step))
            .map(|(step, _)| *step);
        let timed_out = expired.is_some() || started.elapsed() >= limits.total;
        let progress_error = if !timed_out && Instant::now() >= heartbeat {
            heartbeat = Instant::now() + limits.heartbeat;
            progress(
                active.last().map(|(step, _)| *step).unwrap_or(last),
                started.elapsed().as_millis() as u64,
            )
            .err()
        } else {
            None
        };
        if timed_out || progress_error.is_some() {
            cancel.take().unwrap()();
            return Err(CacheFailure {
                step: expired
                    .unwrap_or_else(|| active.last().map(|(step, _)| *step).unwrap_or(last)),
                elapsed_ms: started.elapsed().as_millis() as u64,
                message: progress_error
                    .map(|e| diagnostic_message(&e.to_string()))
                    .unwrap_or_else(|| {
                        if expired.is_some() {
                            "native storage step timed out".into()
                        } else {
                            "total cache mount timed out".into()
                        }
                    }),
                completed,
            });
        }
        match result.try_recv() {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(error)) => {
                return Err(CacheFailure {
                    step: last,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    message: diagnostic_message(&error.to_string()),
                    completed,
                });
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(CacheFailure {
                    step: last,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    message: "cache worker disconnected".into(),
                    completed,
                });
            }
            Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise(fail: bool) -> (Result<(), CacheFailure>, usize, usize) {
        let (events_tx, events_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let reporter = CacheReporter::new(events_tx);
            let value = reporter
                .step(CacheStep::DiskSerial, || Ok(()))
                .and_then(|()| {
                    reporter.step(CacheStep::DiskLayout, || {
                        std::thread::sleep(Duration::from_millis(40));
                        if fail {
                            Err(io::Error::other("layout query failed"))
                        } else {
                            Ok(())
                        }
                    })
                });
            let _ = result_tx.send(value);
        });
        let mut progress_count = 0;
        let mut cancelled = 0;
        let result = watch_cache(
            events_rx,
            result_rx,
            |_, _| {
                progress_count += 1;
                Ok(())
            },
            || cancelled += 1,
            CacheLimits {
                native: Duration::from_millis(if fail { 100 } else { 10 }),
                total: Duration::from_secs(1),
                heartbeat: Duration::from_millis(2),
            },
        );
        worker.join().unwrap();
        (result, progress_count, cancelled)
    }
    #[test]
    fn cache_native_step_timeout_names_the_step_and_cancels() {
        let (result, _, cancelled) = exercise(false);
        let failure = result.unwrap_err();
        assert_eq!(failure.step, CacheStep::DiskLayout);
        assert!(failure.message.contains("timed out"));
        assert!(
            failure
                .completed
                .iter()
                .any(|t| t.step == CacheStep::DiskSerial)
        );
        assert_eq!(cancelled, 1);
    }
    #[test]
    fn cache_timeout_disconnects_worker_before_another_mutation() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mutated = Arc::new(AtomicBool::new(false));
        let probe = mutated.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let reporter = CacheReporter::new(events_tx);
            let value = reporter
                .step(CacheStep::DiskLayout, || {
                    std::thread::sleep(Duration::from_millis(40));
                    Ok(())
                })
                .and_then(|()| {
                    reporter.step(CacheStep::SetAttributes, || {
                        probe.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                });
            let _ = result_tx.send(value);
        });
        let result = watch_cache(
            events_rx,
            result_rx,
            |_, _| Ok(()),
            || {},
            CacheLimits {
                native: Duration::from_millis(10),
                total: Duration::from_secs(1),
                heartbeat: Duration::from_millis(2),
            },
        );
        assert!(result.is_err());
        worker.join().unwrap();
        assert!(!mutated.load(Ordering::SeqCst));
    }

    #[test]
    fn cache_total_timeout_is_independent_of_heartbeats() {
        let (events_tx, events_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let reporter = CacheReporter::new(events_tx);
            let value = reporter.step(CacheStep::InitializeRaw, || {
                std::thread::sleep(Duration::from_millis(40));
                Ok(())
            });
            let _ = result_tx.send(value);
        });
        let mut progress = 0;
        let failure = watch_cache(
            events_rx,
            result_rx,
            |_, _| {
                progress += 1;
                Ok(())
            },
            || {},
            CacheLimits {
                native: Duration::from_secs(1),
                total: Duration::from_millis(10),
                heartbeat: Duration::from_millis(2),
            },
        )
        .unwrap_err();
        worker.join().unwrap();
        assert!(progress > 1);
        assert_eq!(failure.step, CacheStep::InitializeRaw);
        assert_eq!(failure.message, "total cache mount timed out");
    }

    #[test]
    fn cache_slow_step_sends_progress_before_returning_its_named_error() {
        let (result, progress, cancelled) = exercise(true);
        assert!(progress > 1);
        let failure = result.unwrap_err();
        assert_eq!(failure.step, CacheStep::DiskLayout);
        assert!(failure.message.contains("layout query failed"));
        assert_eq!(cancelled, 0);
    }
}
