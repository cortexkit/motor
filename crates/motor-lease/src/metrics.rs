//! The metrics route is diagnostic only; it never advertises placement capacity.
use serde::{Deserialize, Serialize};

pub const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMetrics {
    pub at_ms: u64,
    pub cpu: Cpu,
    pub mem: Memory,
    pub disks: Vec<Disk>,
    pub tenants: Vec<Tenant>,
    pub runners: Vec<Runner>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cpu {
    pub threads: u64,
    pub busy_pct: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub load1: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    pub total_mib: u64,
    pub avail_mib: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    pub mount: String,
    pub total_gib: u64,
    pub free_gib: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    pub vm: String,
    pub state: TenantState,
    pub vcpus: u64,
    pub mem_mib: u64,
    pub slots_total: u64,
    pub slots_used: u64,
    pub jobs_running: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub recovery: Option<Recovery>,
    #[serde(deserialize_with = "required_nullable")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantState {
    Ready,
    Starting,
    Recovering,
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub step: String,
    pub since_ms: u64,
    pub attempts: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    pub name: String,
    pub state: RunnerState,
    pub slots_total: u64,
    pub slots_used: u64,
    pub queued: u64,
    pub oldest_wait_s: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Ready,
    Draining,
    Stopped,
    Error,
}

fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

impl HostMetrics {
    /// Refuse oversized or invalid samples locally instead of consuming broker
    /// counters for bodies that can never be accepted.
    pub fn encode(&self, now_ms: u64) -> Result<Vec<u8>, &'static str> {
        if self.at_ms.abs_diff(now_ms) > 300_000 {
            return Err("at_ms");
        }
        if self.cpu.busy_pct > 100
            || self.cpu.load1.is_some_and(|load| {
                !load.is_finite()
                    || load < 0.0
                    || (load * 100.0 - (load * 100.0).round()).abs() > 0.000001
            })
        {
            return Err("cpu");
        }
        if self.disks.len() > 8 || self.tenants.len() > 8 || self.runners.len() > 8 {
            return Err("body");
        }
        if self.disks.iter().any(|d| !text(&d.mount, 128)) {
            return Err("disks.mount");
        }
        for t in &self.tenants {
            if !crate::valid_name(&t.vm)
                || t.detail.as_ref().is_some_and(|d| !text(d, 200))
                || (t.state == TenantState::Recovering) != t.recovery.is_some()
                || t.recovery
                    .as_ref()
                    .is_some_and(|r| r.step.is_empty() || r.step.chars().count() > 64)
            {
                return Err("tenants");
            }
        }
        for r in &self.runners {
            if r.name.is_empty()
                || r.name.len() > 32
                || !r
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || r.detail.as_ref().is_some_and(|d| !text(d, 200))
            {
                return Err("runners");
            }
        }
        let value = serde_json::to_value(self).map_err(|_| "body")?;
        fn safe(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Number(n) => {
                    n.as_u64().is_none_or(|n| n <= crate::MAX_SAFE_INTEGER)
                }
                serde_json::Value::Array(a) => a.iter().all(safe),
                serde_json::Value::Object(o) => o.values().all(safe),
                _ => true,
            }
        }
        if !safe(&value) {
            return Err("body");
        }
        let bytes = serde_json::to_vec(self).map_err(|_| "body")?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err("body_size");
        }
        Ok(bytes)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MetricsReply {
    Accepted,
    TooLarge,
    BadRequest { field: Option<String> },
    AuthFailed,
    Other,
}

/// Keep only a bounded schema field, never arbitrary broker text, for logging.
pub fn parse_reply(status: u16, body: &[u8]) -> MetricsReply {
    if status == 204 && body.is_empty() {
        return MetricsReply::Accepted;
    }
    if status == 413 && body.is_empty() {
        return MetricsReply::TooLarge;
    }
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    match (status, value["error"].as_str()) {
        (400, Some("bad_request")) => MetricsReply::BadRequest {
            field: value["field"]
                .as_str()
                .filter(|f| text(f, 64) && f.is_ascii())
                .map(str::to_owned),
        },
        (401, Some("host_auth_failed")) => MetricsReply::AuthFailed,
        _ => MetricsReply::Other,
    }
}
