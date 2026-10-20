use motor_protocol::RunnerHealth;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use subc_client_rs::{HealthReport, HealthStatus};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
pub(crate) const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_AFTER: Duration = Duration::from_secs(120);
const FAILURE_LIMIT: u32 = 3;

pub(crate) struct HealthCache {
    created: Instant,
    last_success: Option<Instant>,
    sample: Option<RunnerHealth>,
    failures: u32,
}

impl Default for HealthCache {
    fn default() -> Self {
        Self {
            created: Instant::now(),
            last_success: None,
            sample: None,
            failures: 0,
        }
    }
}

impl HealthCache {
    pub(crate) fn update(&mut self, sample: Option<RunnerHealth>, now: Instant) {
        if sample.is_some() {
            self.last_success = Some(now);
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        self.sample = sample;
    }

    pub(crate) fn report(&self, now: Instant) -> HealthReport {
        let age = now.saturating_duration_since(self.last_success.unwrap_or(self.created));
        let degraded = age >= STALE_AFTER || self.failures >= FAILURE_LIMIT;
        let mut metrics = json!({
            "build_server": null,
            "queue_wait_1h": null,
            "top_repos": null,
            "sample_age_ms": self.last_success.map(|_| age.as_millis() as u64),
            "headline": ["build_server", "queue_wait_1h"],
        });
        let mut detail = if self.failures > 0 {
            "build server unreachable".to_owned()
        } else {
            "build server sample pending".to_owned()
        };
        if let Some(sample) = &self.sample {
            metrics["build_server"] = json!({
                "load_1m": sample.load_1m,
                "threads": sample.threads,
                "mem_available_bytes": sample.mem_available_bytes,
                "disk_free_bytes": sample.free_bytes,
                "permits_held": sample.permits_held,
                "permits_cap": sample.permits_cap,
                "queue_depth": sample.queue_depth,
            });
            metrics["queue_wait_1h"] = json!(sample.queue_wait_1h);
            let mut repos: Vec<_> = sample.repository_usage.iter().collect();
            repos.sort_by(|(a_name, a), (b_name, b)| {
                b.reserved_vcpu_seconds
                    .cmp(&a.reserved_vcpu_seconds)
                    .then_with(|| a_name.cmp(b_name))
            });
            metrics["top_repos"] = Value::Array(repos.into_iter().take(5).map(|(name, usage)| {
                json!({"name": name, "vcpu_hours": usage.reserved_vcpu_seconds as f64 / 3600.0})
            }).collect());
            // A minute of capacity wait or any refusal is busy, not module impairment.
            if let Some(wait) = sample
                .queue_wait_1h
                .as_ref()
                .filter(|wait| wait.p90_ms > 60_000 || wait.refusals > 0)
            {
                detail = format!(
                    "build server busy: queue p90 {}, {} refusals in the last hour",
                    duration(wait.p90_ms),
                    wait.refusals
                );
            } else if let (Some(held), Some(cap)) = (sample.permits_held, sample.permits_cap) {
                let queue = if sample.queue_depth == 0 {
                    "no queue".to_owned()
                } else {
                    format!("{} jobs queued", sample.queue_depth)
                };
                detail = format!("build server: {held}/{cap} vCPU slots in use, {queue}");
            } else {
                detail = format!("build server: {} jobs queued", sample.queue_depth);
            }
        }
        if age >= STALE_AFTER {
            detail.push_str(", sample stale");
        }
        HealthReport {
            status: if degraded {
                HealthStatus::Degraded
            } else {
                HealthStatus::Ok
            },
            detail: Some(detail),
            metrics: Some(metrics),
        }
    }
}

fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[derive(Default)]
pub(crate) struct Sampler(pub Option<tokio::task::JoinHandle<()>>);

impl Drop for Sampler {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn sample() -> RunnerHealth {
        serde_json::from_value(json!({
            "runner_version": "test", "since_ms": 0, "window_minutes": 60,
            "free_bytes": 123456, "queue_depth": 4, "running": [],
            "counts": {"accepted": 0, "refusals": {}, "evictions": 0, "eviction_bytes": 0,
                "eviction_failures": 0, "soft_goal_unmet_passes": 0, "skipped_recent": 0},
            "load_1m": 1.25, "threads": 300, "mem_available_bytes": 987654,
            "permits_held": 12, "permits_cap": 80,
            "queue_wait_1h": {"p50_ms": 2000, "p90_ms": 190000, "refusals": 4},
            "repository_usage": (0..7).map(|i| (format!("repo-{i}"), json!({
                "reserved_vcpu_seconds": i * 3600, "job_count": 1,
                "jobs_by_vcpu_size": {"2": 1}, "queue_wait_refusals": 0,
            }))).collect::<serde_json::Map<String, Value>>()
        }))
        .unwrap()
    }

    #[test]
    fn busy_server_is_healthy_and_metrics_have_cli_shape() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        cache.update(Some(sample()), now);
        let report = cache.report(now + Duration::from_millis(1500));
        assert_eq!(report.status, HealthStatus::Ok);
        assert_eq!(
            report.detail.as_deref(),
            Some("build server busy: queue p90 3m 10s, 4 refusals in the last hour")
        );
        assert_eq!(
            report.metrics.unwrap(),
            json!({
                "build_server": {"load_1m": 1.25, "threads": 300, "mem_available_bytes": 987654,
                    "disk_free_bytes": 123456, "permits_held": 12, "permits_cap": 80, "queue_depth": 4},
                "queue_wait_1h": {"p50_ms": 2000, "p90_ms": 190000, "refusals": 4},
                "top_repos": [
                    {"name": "repo-6", "vcpu_hours": 6.0}, {"name": "repo-5", "vcpu_hours": 5.0},
                    {"name": "repo-4", "vcpu_hours": 4.0}, {"name": "repo-3", "vcpu_hours": 3.0},
                    {"name": "repo-2", "vcpu_hours": 2.0}],
                "sample_age_ms": 1500, "headline": ["build_server", "queue_wait_1h"]
            })
        );
    }

    #[test]
    fn spill_withdrawals_are_exposed_but_not_reported_as_refusals() {
        let now = Instant::now();
        let mut sample = sample();
        let wait = sample.queue_wait_1h.as_mut().unwrap();
        wait.refusals = 0;
        wait.spill_withdrawals = 65;
        let mut cache = HealthCache::default();
        cache.update(Some(sample), now);
        let report = cache.report(now);
        assert_eq!(
            report.detail.as_deref(),
            Some("build server busy: queue p90 3m 10s, 0 refusals in the last hour")
        );
        assert_eq!(
            report.metrics.unwrap()["queue_wait_1h"]["spill_withdrawals"],
            65
        );
    }

    #[test]
    fn idle_cache_stales_after_two_minutes_and_recovers() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        let mut sample = sample();
        sample.queue_depth = 0;
        sample.queue_wait_1h = Some(Default::default());
        cache.update(Some(sample.clone()), now);
        let report = cache.report(now);
        assert_eq!(report.status, HealthStatus::Ok);
        assert_eq!(
            report.detail.as_deref(),
            Some("build server: 12/80 vCPU slots in use, no queue")
        );
        assert_eq!(
            cache.report(now + Duration::from_secs(119)).status,
            HealthStatus::Ok
        );
        let stale = cache.report(now + STALE_AFTER);
        assert_eq!(stale.status, HealthStatus::Degraded);
        assert!(stale.detail.unwrap().contains("sample stale"));
        assert_eq!(stale.metrics.unwrap()["sample_age_ms"], 120_000);
        cache.update(Some(sample), now + STALE_AFTER);
        assert_eq!(cache.report(now + STALE_AFTER).status, HealthStatus::Ok);
    }

    #[test]
    fn unreachable_samples_are_null_and_three_failures_degrade() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        cache.update(Some(sample()), now);
        for failure in 1..=3 {
            cache.update(None, now + Duration::from_secs(15 * failure));
            let report = cache.report(now + Duration::from_secs(15 * failure));
            assert_eq!(
                report.status,
                if failure == 3 {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::Ok
                }
            );
            let metrics = report.metrics.unwrap();
            for key in ["build_server", "queue_wait_1h", "top_repos"] {
                assert!(metrics[key].is_null(), "{key}");
            }
            assert_eq!(metrics["sample_age_ms"], 15_000 * failure);
        }
        cache.update(Some(sample()), now + Duration::from_secs(46));
        assert_eq!(
            cache.report(now + Duration::from_secs(46)).status,
            HealthStatus::Ok
        );
    }

    #[test]
    fn never_sampled_cache_has_startup_grace_but_not_forever() {
        let cache = HealthCache::default();
        assert_eq!(cache.report(cache.created).status, HealthStatus::Ok);
        assert!(cache.report(cache.created).metrics.unwrap()["sample_age_ms"].is_null());
        assert_eq!(
            cache.report(cache.created + STALE_AFTER).status,
            HealthStatus::Degraded
        );
    }
}
