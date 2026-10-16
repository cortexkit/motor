use crate::storage::Job;
use motor_protocol::{HealthCounts, HealthRepositoryUsage, Outcome, RefusalReason};
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;

pub(super) const WINDOW_MINUTES: u32 = 60;
const MINUTE_MS: u64 = 60_000;
pub(super) const REPOSITORY_WINDOW_MS: u64 = 24 * 60 * MINUTE_MS;

pub(super) struct Health {
    pub since_ms: u64,
    buckets: BTreeMap<u64, HealthCounts>,
}

impl Health {
    pub fn new(since_ms: u64) -> Self {
        Self {
            since_ms,
            buckets: BTreeMap::new(),
        }
    }

    fn prune(&mut self, now: u64) {
        let minute = now / MINUTE_MS;
        self.buckets
            .retain(|bucket, _| minute.saturating_sub(*bucket) < u64::from(WINDOW_MINUTES));
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
    /// only a map lookup; expired entries are dropped when a snapshot is taken.
    pub fn record_job(&mut self, job: &Job, now: u64) {
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
        let Some(repository) = Path::new(&request.repository_root)
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return;
        };
        let queue_wait_refusal = job.terminal.as_ref().is_some_and(|terminal| {
            matches!(
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
