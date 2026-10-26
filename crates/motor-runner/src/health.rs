use crate::storage::Job;
use motor_protocol::{
    HealthCounts, HealthQueueWait, HealthRepositoryUsage, HealthUnderusedReservation, Outcome,
    RefusalReason, RunningJob,
};
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;

pub(super) const WINDOW_MINUTES: u32 = 60;
const MINUTE_MS: u64 = 60_000;
pub(super) const REPOSITORY_WINDOW_MS: u64 = 24 * 60 * MINUTE_MS;

pub(super) struct Health {
    pub since_ms: u64,
    buckets: BTreeMap<u64, HealthCounts>,
    queue_waits: BTreeMap<u64, BTreeMap<u64, u64>>,
    spill_withdrawals: BTreeMap<u64, u64>,
}

impl Health {
    pub fn new(since_ms: u64) -> Self {
        Self {
            since_ms,
            buckets: BTreeMap::new(),
            queue_waits: BTreeMap::new(),
            spill_withdrawals: BTreeMap::new(),
        }
    }

    fn prune(&mut self, now: u64) {
        let minute = now / MINUTE_MS;
        self.buckets
            .retain(|bucket, _| minute.saturating_sub(*bucket) < u64::from(WINDOW_MINUTES));
        self.queue_waits
            .retain(|bucket, _| minute.saturating_sub(*bucket) < u64::from(WINDOW_MINUTES));
        self.spill_withdrawals
            .retain(|bucket, _| minute.saturating_sub(*bucket) < u64::from(WINDOW_MINUTES));
    }

    pub fn record_queue_wait(&mut self, now: u64, wait_ms: u64) {
        self.prune(now);
        *self
            .queue_waits
            .entry(now / MINUTE_MS)
            .or_default()
            .entry(wait_ms)
            .or_default() += 1;
    }

    /// Forget every recorded queue wait, so a caller can record exactly the
    /// samples it means to measure, without waits from other jobs.
    #[cfg(test)]
    pub fn reset_queue_waits(&mut self) {
        self.queue_waits.clear();
    }

    /// The median queue wait of jobs started in the window, or `None` when no
    /// job started in it. Zero samples is not a zero-length wait, and a caller
    /// that reads it as one would retry immediately.
    pub fn recent_queue_wait_p50(&mut self, now: u64) -> Option<u64> {
        let p50_ms = self.queue_wait(now).p50_ms;
        let sampled = self.queue_waits.values().any(|waits| !waits.is_empty());
        sampled.then_some(p50_ms)
    }

    pub fn queue_wait(&mut self, now: u64) -> HealthQueueWait {
        self.prune(now);
        let mut histogram = BTreeMap::<u64, u64>::new();
        for bucket in self.queue_waits.values() {
            for (&wait, &count) in bucket {
                *histogram.entry(wait).or_default() += count;
            }
        }
        let total: u64 = histogram.values().sum();
        let percentile = |percent: u64| {
            let rank = (total * percent).div_ceil(100);
            let mut seen = 0;
            histogram
                .iter()
                .find_map(|(&wait, &count)| {
                    seen += count;
                    (seen >= rank).then_some(wait)
                })
                .unwrap_or(0)
        };
        HealthQueueWait {
            p50_ms: percentile(50),
            p90_ms: percentile(90),
            refusals: self
                .counts(now)
                .refusals
                .get("queue_wait_exceeded")
                .copied()
                .unwrap_or(0),
            spill_withdrawals: self.spill_withdrawals.values().sum(),
        }
    }

    pub fn record_spill_withdrawal(&mut self, now: u64) {
        self.prune(now);
        *self.spill_withdrawals.entry(now / MINUTE_MS).or_default() += 1;
    }

    pub fn record(&mut self, now: u64, update: impl FnOnce(&mut HealthCounts)) {
        self.prune(now);
        update(self.buckets.entry(now / MINUTE_MS).or_default());
    }

    pub fn refusal(&mut self, now: u64, reason: &RefusalReason) {
        // Use the wire reason, including unknown strings, rather than Rust's
        // debug spelling so callers can group refusals by the protocol tag.
        let reason = serde_json::to_value(reason).expect("refusal reason serializes");
        let reason = reason.as_str().expect("refusal reason is a string");
        self.record(now, |counts| {
            *counts.refusals.entry(reason.to_owned()).or_default() += 1;
        });
    }

    pub fn counts(&mut self, now: u64) -> HealthCounts {
        self.prune(now);
        let mut total = HealthCounts::default();
        for counts in self.buckets.values() {
            total.accepted += counts.accepted;
            for (reason, count) in &counts.refusals {
                *total.refusals.entry(reason.clone()).or_default() += count;
            }
            total.evictions += counts.evictions;
            total.eviction_bytes += counts.eviction_bytes;
            total.eviction_failures += counts.eviction_failures;
            total.soft_goal_unmet_passes += counts.soft_goal_unmet_passes;
            total.skipped_recent += counts.skipped_recent;
            total.oom_stops += counts.oom_stops;
        }
        total
    }
}

#[derive(Clone)]
struct FinishedJob {
    repository: String,
    weight: u32,
    started_ms: Option<u64>,
    finished_ms: u64,
    queue_wait_refusal: bool,
}

#[derive(Default)]
pub(super) struct RepositoryUsage {
    jobs: BTreeMap<Uuid, FinishedJob>,
    pub active: BTreeMap<Uuid, (RunningJob, bool)>,
    pub underused: BTreeMap<Uuid, HealthUnderusedReservation>,
}

impl RepositoryUsage {
    pub fn from_jobs<'a>(jobs: impl IntoIterator<Item = &'a Job>, now: u64) -> Self {
        let mut usage = Self::default();
        for job in jobs {
            usage.record_job(job, now);
        }
        usage
    }

    /// Called on every job record save, often under the state lock, so it does
    /// only map lookups; expired entries are dropped when a snapshot is taken.
    pub fn record_job(&mut self, job: &Job, now: u64) {
        self.active.remove(&job.id);
        if job.pending() || job.state == crate::storage::JobState::Running {
            self.active.insert(
                job.id,
                (
                    RunningJob::new(job.id, crate::caller_path::job_key(job), job.weight),
                    job.pending(),
                ),
            );
        }
        self.underused.remove(&job.id);
        if let Some(usage) = &job.reservation_use {
            if job.terminal.is_some()
                && usage.wall_ms > 10 * MINUTE_MS
                && usage.average_use_ppm < 250_000
            {
                self.underused.insert(
                    job.id,
                    HealthUnderusedReservation {
                        job_id: job.id,
                        workspace_key: crate::caller_path::job_key(job).to_owned(),
                        vcpus: job.weight,
                        usage: usage.clone(),
                    },
                );
            }
        }
        self.jobs.remove(&job.id);
        let Some(finished_ms) = job.finished_ms else {
            return;
        };
        if now < finished_ms || now - finished_ms >= REPOSITORY_WINDOW_MS {
            return;
        }
        let Some(request) = &job.request else {
            return;
        };
        let Some(repository) = Path::new(
            crate::caller_path::request(request)
                .caller_repository_root
                .text(),
        )
        .file_name()
        .and_then(|name| name.to_str()) else {
            return;
        };
        let queue_wait_refusal = job.terminal.as_ref().is_some_and(|terminal| {
            !terminal.refusal_detail().is_some_and(|detail| {
                detail.starts_with(motor_protocol::SPILL_WITHDRAWAL_DETAIL_PREFIX)
            }) && matches!(
                &terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::QueueWaitExceeded
                }
            )
        });
        self.jobs.insert(
            job.id,
            FinishedJob {
                repository: repository.to_owned(),
                weight: job.weight,
                started_ms: job.started_ms,
                finished_ms,
                queue_wait_refusal,
            },
        );
    }

    pub fn remove(&mut self, id: Uuid) {
        self.active.remove(&id);
        self.underused.remove(&id);
        self.jobs.remove(&id);
    }

    pub fn snapshot(&mut self, now: u64) -> BTreeMap<String, HealthRepositoryUsage> {
        self.prune(now);
        let mut repositories: BTreeMap<String, HealthRepositoryUsage> = BTreeMap::new();
        let mut reserved_vcpu_millis = BTreeMap::<String, u128>::new();
        for job in self.jobs.values() {
            let repository = repositories.entry(job.repository.clone()).or_default();
            repository.job_count = repository.job_count.saturating_add(1);
            let count = repository
                .jobs_by_vcpu_size
                .entry(job.weight.to_string())
                .or_default();
            *count = count.saturating_add(1);
            if job.queue_wait_refusal {
                repository.queue_wait_refusals = repository.queue_wait_refusals.saturating_add(1);
            }
            if let Some(started_ms) = job.started_ms {
                let duration_ms = job.finished_ms.saturating_sub(started_ms);
                let weighted_ms = u128::from(duration_ms).saturating_mul(u128::from(job.weight));
                let total = reserved_vcpu_millis
                    .entry(job.repository.clone())
                    .or_default();
                *total = total.saturating_add(weighted_ms);
            }
        }
        for (repository, millis) in reserved_vcpu_millis {
            repositories
                .entry(repository)
                .or_default()
                .reserved_vcpu_seconds = u64::try_from(millis / 1000).unwrap_or(u64::MAX);
        }
        repositories
    }

    fn prune(&mut self, now: u64) {
        self.jobs.retain(|_, job| {
            now >= job.finished_ms && now - job.finished_ms < REPOSITORY_WINDOW_MS
        });
    }
}

pub(super) fn host_sample(
    loadavg: &str,
    cpuinfo: &str,
    meminfo: &str,
) -> (Option<serde_json::Number>, Option<u64>, Option<u64>) {
    let mut fields = loadavg.split_whitespace();
    let load = fields
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .and_then(serde_json::Number::from_f64);
    // Count logical CPU threads in /proc/cpuinfo so load can be read against host capacity.
    let threads = cpuinfo
        .lines()
        .filter(|line| {
            line.split_once(':').is_some_and(|(key, value)| {
                key.trim() == "processor" && value.trim().parse::<u64>().is_ok()
            })
        })
        .count() as u64;
    let threads = (threads > 0).then_some(threads);
    let available = meminfo.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "MemAvailable:").then_some(())?;
        let kib = fields.next()?.parse::<u64>().ok()?;
        (fields.next()? == "kB").then_some(())?;
        kib.checked_mul(1024)
    });
    (load, threads, available)
}
