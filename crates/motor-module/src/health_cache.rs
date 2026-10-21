use motor_protocol::RunnerHealth;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subc_client_rs::{HealthReport, HealthStatus};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
pub(crate) const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_AFTER: Duration = Duration::from_secs(120);
const FAILURE_LIMIT: u32 = 3;
const HISTORY_LIMIT: usize = 180;
const MINUTE_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryPoint {
    minute_ms: u64,
    permits_held: u32,
    permits_cap: u32,
    queue_depth: u32,
    queue_p90_ms: u64,
}

pub(crate) struct HealthCache {
    created: Instant,
    last_success: Option<Instant>,
    sample: Option<RunnerHealth>,
    failures: u32,
    history: VecDeque<HistoryPoint>,
    pub(crate) windows: Vec<Value>,
}

impl Default for HealthCache {
    fn default() -> Self {
        Self {
            created: Instant::now(),
            last_success: None,
            sample: None,
            failures: 0,
            history: VecDeque::new(),
            windows: Vec::new(),
        }
    }
}

impl HealthCache {
    pub(crate) fn update(&mut self, sample: Option<RunnerHealth>, now: Instant) {
        self.update_at(sample, now, unix_ms_now());
    }

    fn update_at(&mut self, sample: Option<RunnerHealth>, now: Instant, unix_ms: u64) {
        if let Some(sample) = sample.as_ref() {
            self.record_history(sample, unix_ms);
        }
        if sample.is_some() {
            self.last_success = Some(now);
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        self.sample = sample;
    }

    pub(crate) fn report(&self, now: Instant) -> HealthReport {
        self.report_at(now, unix_ms_now())
    }

    fn report_at(&self, now: Instant, unix_ms: u64) -> HealthReport {
        let age = now.saturating_duration_since(self.last_success.unwrap_or(self.created));
        let degraded = age >= STALE_AFTER || self.failures >= FAILURE_LIMIT;
        let mut metrics = json!({
            "build_server": null,
            "queue_wait_1h": null,
            "top_repos": null,
            "sample_age_ms": self.last_success.map(|_| age.as_millis() as u64),
            "headline": ["build_server", "queue_wait_1h"],
            "history": self.history_json(minute_start_ms(unix_ms)),
        });
        if !self.windows.is_empty() {
            metrics["windows_runners"] = json!(self.windows);
        }
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
        for runner in &self.windows {
            let name = runner["name"].as_str().unwrap_or("windows");
            let state = if runner["reachable"] == false {
                "unreachable"
            } else if runner["busy"] == true {
                "busy"
            } else {
                "available"
            };
            detail.push_str(&format!("; Windows {name}: {state}"));
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

    /// The history as parallel per-minute columns from the oldest retained
    /// minute up to `newest_minute`, with `null` for a minute that has no
    /// sample so gaps stay visible. The subc daemon caches at most 16 KiB of a
    /// module's health metrics, and one object per minute for three hours
    /// exceeded that; columns keep it to a few kilobytes. `permits_cap` is a
    /// single number when it never changed in the window.
    fn history_json(&self, newest_minute: u64) -> Value {
        let oldest_minute = newest_minute.saturating_sub((HISTORY_LIMIT as u64 - 1) * MINUTE_MS);
        let points: Vec<_> = self
            .history
            .iter()
            .filter(|point| point.minute_ms >= oldest_minute && point.minute_ms <= newest_minute)
            .collect();
        let start = points
            .first()
            .map_or(newest_minute, |point| point.minute_ms);
        let slots = if points.is_empty() {
            0
        } else {
            ((newest_minute - start) / MINUTE_MS) as usize + 1
        };
        let mut held = vec![Value::Null; slots];
        let mut cap = vec![Value::Null; slots];
        let mut depth = vec![Value::Null; slots];
        let mut p90 = vec![Value::Null; slots];
        for point in &points {
            let slot = ((point.minute_ms - start) / MINUTE_MS) as usize;
            held[slot] = json!(point.permits_held);
            cap[slot] = json!(point.permits_cap);
            depth[slot] = json!(point.queue_depth);
            p90[slot] = json!(point.queue_p90_ms);
        }
        let constant_cap = points
            .first()
            .map(|first| first.permits_cap)
            .filter(|first| points.iter().all(|point| point.permits_cap == *first));
        json!({
            "start_minute_ms": start,
            "step_ms": MINUTE_MS,
            "permits_held": held,
            "permits_cap": constant_cap.map_or(Value::Array(cap), |cap| json!(cap)),
            "queue_depth": depth,
            "queue_p90_ms": p90,
        })
    }

    fn record_history(&mut self, sample: &RunnerHealth, unix_ms: u64) {
        let point = HistoryPoint {
            minute_ms: minute_start_ms(unix_ms),
            // Missing optional permit or queue-wait values from older runners are recorded as zero.
            permits_held: sample.permits_held.unwrap_or_default(),
            permits_cap: sample.permits_cap.unwrap_or_default(),
            queue_depth: sample.queue_depth,
            queue_p90_ms: sample
                .queue_wait_1h
                .as_ref()
                .map(|wait| wait.p90_ms)
                .unwrap_or_default(),
        };
        match self
            .history
            .binary_search_by_key(&point.minute_ms, |stored| stored.minute_ms)
        {
            Ok(index) => self.history[index] = point,
            Err(index) => self.history.insert(index, point),
        }
        while self.history.len() > HISTORY_LIMIT {
            self.history.pop_front();
        }
    }
}

fn minute_start_ms(unix_ms: u64) -> u64 {
    unix_ms / MINUTE_MS * MINUTE_MS
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
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

    #[test]
    fn windows_runner_samples_surface_reachability_and_busy_state() {
        let mut cache = HealthCache::default();
        cache.update(Some(sample()), Instant::now());
        cache.windows = vec![
            json!({"name": "portable", "reachable": false, "busy": null}),
            json!({"name": "fallback", "reachable": true, "busy": true}),
        ];
        let report = cache.report(Instant::now());
        assert_eq!(report.status, HealthStatus::Ok);
        let detail = report.detail.unwrap();
        assert!(detail.contains("Windows portable: unreachable"));
        assert!(detail.contains("Windows fallback: busy"));
        assert_eq!(
            report.metrics.unwrap()["windows_runners"],
            json!(cache.windows)
        );
    }

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
        let unix_ms = 1_234_567;
        let mut cache = HealthCache::default();
        cache.update_at(Some(sample()), now, unix_ms);
        let report = cache.report_at(now + Duration::from_millis(1500), unix_ms + 1500);
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
                "sample_age_ms": 1500, "headline": ["build_server", "queue_wait_1h"],
                "history": {"start_minute_ms": 1_200_000, "step_ms": 60_000,
                    "permits_held": [12], "permits_cap": 80, "queue_depth": [4],
                    "queue_p90_ms": [190000]}
            })
        );
    }

    #[test]
    fn history_keeps_the_last_sample_in_each_minute() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        let start_ms = MINUTE_MS;
        let mut first = sample();
        first.queue_depth = 2;
        first.permits_held = Some(3);
        first.queue_wait_1h.as_mut().unwrap().p90_ms = 100;
        cache.update_at(Some(first), now, start_ms + 1_000);

        let mut last = sample();
        last.queue_depth = 7;
        last.permits_held = Some(5);
        last.queue_wait_1h.as_mut().unwrap().p90_ms = 250;
        cache.update_at(Some(last), now + Duration::from_secs(20), start_ms + 50_000);

        let next_minute = sample();
        cache.update_at(
            Some(next_minute),
            now + Duration::from_secs(60),
            start_ms + MINUTE_MS,
        );
        let history = cache
            .report_at(now + Duration::from_secs(60), start_ms + 2 * MINUTE_MS)
            .metrics
            .unwrap()["history"]
            .clone();
        assert_eq!(
            history,
            json!({"start_minute_ms": 60_000, "step_ms": 60_000,
                "permits_held": [5, 12, null], "permits_cap": 80,
                "queue_depth": [7, 4, null], "queue_p90_ms": [250, 190_000, null]})
        );
    }

    #[test]
    fn history_is_bounded_to_180_points_and_evicts_the_oldest() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        for minute in 0..=HISTORY_LIMIT {
            cache.update_at(
                Some(sample()),
                now + Duration::from_secs(minute as u64 * 60),
                minute as u64 * MINUTE_MS,
            );
        }
        assert_eq!(cache.history.len(), HISTORY_LIMIT);
        assert_eq!(cache.history.front().unwrap().minute_ms, MINUTE_MS);
        let metrics = cache
            .report_at(now, HISTORY_LIMIT as u64 * MINUTE_MS)
            .metrics
            .unwrap();
        let history = &metrics["history"];
        assert_eq!(history["start_minute_ms"], MINUTE_MS);
        for column in ["permits_held", "queue_depth", "queue_p90_ms"] {
            let values = history[column].as_array().unwrap();
            assert_eq!(values.len(), HISTORY_LIMIT, "{column}");
            assert!(values.iter().all(|value| !value.is_null()), "{column}");
        }
        // The subc daemon drops a module's cached metrics above 16 KiB, so
        // a full three-hour history with realistic values must stay well under.
        let size = serde_json::to_vec(&metrics).unwrap().len();
        assert!(size < 8 * 1024, "metrics are {size} bytes");
    }

    #[test]
    fn history_reports_a_changing_cap_per_minute() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        cache.update_at(Some(sample()), now, 0);
        let mut raised = sample();
        raised.permits_cap = Some(96);
        cache.update_at(Some(raised), now, MINUTE_MS);
        let history = cache.report_at(now, MINUTE_MS).metrics.unwrap()["history"].clone();
        assert_eq!(history["permits_cap"], json!([80, 96]));
    }

    #[test]
    fn history_leaves_a_gap_for_a_minute_without_a_successful_sample() {
        let now = Instant::now();
        let mut cache = HealthCache::default();
        cache.update_at(Some(sample()), now, 1_000);
        cache.update_at(None, now + Duration::from_secs(60), MINUTE_MS + 1_000);
        cache.update_at(
            Some(sample()),
            now + Duration::from_secs(120),
            2 * MINUTE_MS + 1_000,
        );
        let history = cache
            .report_at(now + Duration::from_secs(120), 3 * MINUTE_MS)
            .metrics
            .unwrap()["history"]
            .clone();
        assert_eq!(
            history,
            json!({"start_minute_ms": 0, "step_ms": 60_000,
                "permits_held": [12, null, 12, null], "permits_cap": 80,
                "queue_depth": [4, null, 4, null],
                "queue_p90_ms": [190_000, null, 190_000, null]})
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
