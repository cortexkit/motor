use motor_protocol::{HealthCounts, RefusalReason};
use std::collections::BTreeMap;

pub(super) const WINDOW_MINUTES: u32 = 60;
const MINUTE_MS: u64 = 60_000;

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
