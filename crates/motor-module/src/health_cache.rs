use crate::Config;
use motor_protocol::RunnerHealth;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use subc_client_rs::{HealthReport, HealthStatus};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
pub(crate) const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_AFTER: Duration = Duration::from_secs(120);
const FAILURE_LIMIT: u32 = 3;

pub(crate) struct HealthCache {
    created: Instant,
    last_success: Option<Instant>,
    last_refresh: Option<Instant>,
    sample: Option<RunnerHealth>,
    failures: u32,
    pub(crate) config: Config,
    pub(crate) windows: Vec<Value>,
    pub(crate) macos: Option<Value>,
    pub(crate) account: Option<Value>,
    pub(crate) jobs: Option<Value>,
}

impl Default for HealthCache {
    fn default() -> Self {
        Self {
            created: Instant::now(),
            last_success: None,
            last_refresh: None,
            sample: None,
            failures: 0,
            config: Config::default(),
            windows: Vec::new(),
            macos: None,
            account: None,
            jobs: None,
        }
    }
}

pub(crate) fn availability(status: &str, reason: Option<&str>) -> Value {
    let mut value = json!({"status": status});
    if let Some(reason) = reason {
        value["reason"] = json!(reason);
    }
    value
}

pub(crate) fn account_health(check: crate::hosted::check::Check) -> Value {
    let refused = matches!(check.broker_status, Some(401 | 403));
    json!({
        "sign_in": match check.status.as_str() {
            "ok" if !refused => "signed in",
            "transient" => "not confirmed; retry sign-in",
            _ => "not signed in; sign in with ck",
        },
        "broker": if check.broker_status.is_some_and(|status| (200..500).contains(&status)) { "ok" } else { "unavailable" },
    })
}

impl HealthCache {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    pub(crate) fn update(&mut self, sample: Option<RunnerHealth>, now: Instant) {
        self.last_refresh = Some(now);
        if sample.is_some() {
            self.last_success = Some(now);
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        self.sample = sample;
    }

    pub(crate) fn report(&self, now: Instant) -> HealthReport {
        self.report_with_refusal(now, None)
    }

    pub(crate) fn report_with_refusal(&self, now: Instant, refusal: Option<&str>) -> HealthReport {
        let age = now.saturating_duration_since(self.last_success.unwrap_or(self.created));
        let degraded = age >= STALE_AFTER || self.failures >= FAILURE_LIMIT;
        let fresh = self
            .last_refresh
            .is_some_and(|at| now.saturating_duration_since(at) < STALE_AFTER);
        let unavailable = || {
            availability(
                "unavailable",
                Some("connection not confirmed; check network and retry"),
            )
        };
        let linux = if !fresh {
            unavailable()
        } else if let Some(sample) = &self.sample {
            // Hourly percentiles describe jobs that already started, not the current queue.
            // A long historical wait must not make an idle platform look busy.
            let full = matches!((sample.permits_held, sample.permits_cap), (Some(held), Some(cap)) if held >= cap);
            if sample.queue_depth > 0 || full {
                availability("busy (queueing)", None)
            } else {
                availability("available", None)
            }
        } else {
            unavailable()
        };
        let windows = if self
            .config
            .target(Some(&motor_protocol::Platform::Windows))
            .is_err()
        {
            availability("unavailable", Some("not set up on this machine"))
        } else if !fresh {
            unavailable()
        } else if self
            .windows
            .iter()
            .any(|runner| runner["reachable"] == true && runner["busy"] == false)
        {
            availability("available", None)
        } else if self
            .windows
            .iter()
            .any(|runner| runner["reachable"] == true && runner["busy"] == true)
        {
            availability("busy (queueing)", None)
        } else {
            unavailable()
        };
        let macos = if self.config.macos_runner.is_none() {
            availability("unavailable", Some("not set up on this machine"))
        } else if fresh
            && self
                .macos
                .as_ref()
                .is_some_and(|sample| sample["reachable"] == true)
        {
            if self
                .macos
                .as_ref()
                .is_some_and(|sample| sample["busy"] == true)
            {
                availability("busy (queueing)", None)
            } else {
                availability("available", None)
            }
        } else {
            unavailable()
        };
        let hosted = self.config.hosted_runners
            || !self.config.hosted_repositories.is_empty()
            || self.config.spill_to_hosted;
        let mut connection = json!({
            "runner": if fresh && self.sample.is_some() { "ok" } else { "unavailable" },
            "broker": "not required",
            "sign_in": "not required (direct runner)",
        });
        let mut platforms = json!({"linux": linux, "windows": windows, "macos": macos});
        if hosted {
            connection["sign_in"] = self
                .account
                .as_ref()
                .filter(|_| fresh)
                .map_or(json!("not confirmed; sign in with ck"), |account| {
                    account["sign_in"].clone()
                });
            connection["broker"] = self
                .account
                .as_ref()
                .filter(|_| fresh)
                .map_or(json!("unavailable"), |account| account["broker"].clone());
            if self.config.hosted_runners {
                platforms["linux"] = if connection["sign_in"] != "signed in" {
                    availability("unavailable", Some("sign in with ck, then retry"))
                } else if connection["broker"] != "ok" {
                    availability(
                        "unavailable",
                        Some("broker connection not confirmed; check network and retry"),
                    )
                } else {
                    // The broker does not expose live fleet inventory to clients. A working
                    // connection can accept requests; local placement waits show queueing.
                    availability("available", Some("requests accepted"))
                };
            }
        }
        let jobs = self.jobs.as_ref().filter(|_| fresh);
        if let Some(jobs) = jobs {
            for platform in ["linux", "windows", "macos"] {
                if jobs[platform]["waiting_for_capacity"].as_u64().unwrap_or(0) > 0
                    && platforms[platform]["status"] == "available"
                {
                    platforms[platform] = availability("busy (queueing)", None);
                }
            }
        }
        if let Some(reason) = refusal {
            connection["error"] = json!(reason);
            for platform in ["linux", "windows", "macos"] {
                platforms[platform] = availability(
                    "unavailable",
                    Some("module not ready; check ck daemon and retry"),
                );
            }
        }
        let machine = machine_name();
        let mut detail = format!("{machine}: daemon running");
        if !hosted {
            detail.push_str(&format!(
                "; Linux runner {}",
                connection["runner"].as_str().unwrap()
            ));
        }
        detail.push_str(&format!(
            "; broker {}; sign-in: {}",
            connection["broker"].as_str().unwrap(),
            connection["sign_in"].as_str().unwrap()
        ));
        if let Some(reason) = refusal {
            detail.push_str(&format!("; module error: {reason}"));
        }
        for platform in ["linux", "windows", "macos"] {
            let status = platforms[platform]["status"].as_str().unwrap();
            let reason = platforms[platform]["reason"].as_str();
            if status == "unavailable" && reason == Some("not set up on this machine") {
                detail.push_str(&format!("; {platform}: not set up on this machine"));
                continue;
            }
            let label = if status == "busy (queueing)" {
                "busy"
            } else {
                status
            };
            detail.push_str(&format!("; {platform}: {label}"));
            if let Some(reason) = reason {
                detail.push_str(&format!(" ({reason})"));
            }
            if status == "unavailable" {
                continue;
            }
            if let Some(jobs) = jobs {
                for (key, label) in [
                    ("running", "running"),
                    ("waiting_for_capacity", "waiting for capacity"),
                ] {
                    let count = jobs[platform][key].as_u64().unwrap_or(0);
                    if count > 0 {
                        detail.push_str(&format!(", {count} {label}"));
                    }
                }
                let unknown = jobs[platform]["unknown"].as_u64().unwrap_or(0);
                if unknown > 0 {
                    detail.push_str(&format!(
                        ", {unknown} job states not confirmed; check ck motor status"
                    ));
                }
            }
        }
        if jobs.is_none() {
            detail.push_str("; job state not confirmed yet");
        }
        HealthReport {
            status: if refusal.is_some() {
                HealthStatus::Failing
            } else if degraded {
                HealthStatus::Degraded
            } else {
                HealthStatus::Ok
            },
            detail: Some(detail),
            metrics: Some(json!({
                "daemon": "running", "connection": connection, "platforms": platforms,
                "machine": {"name": machine, "jobs": jobs},
            })),
        }
    }
}

fn machine_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "this machine".into())
    })
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
#[path = "health_cache_tests.rs"]
mod tests;
