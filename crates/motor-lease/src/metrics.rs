//! The metrics route is diagnostic only; it never advertises placement capacity.
use serde::{Deserialize, Serialize};

pub const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMetrics {
    pub at_ms: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub cpu: Option<Cpu>,
    #[serde(deserialize_with = "required_nullable")]
    pub mem: Option<Memory>,
    pub disks: Vec<Disk>,
    pub tenants: Vec<Tenant>,
    pub runners: Vec<Runner>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_pressure"
    )]
    pub pressure: Option<Pressure>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cpu {
    pub threads: u64,
    pub busy_pct: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub load1: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pressure {
    #[serde(serialize_with = "pressure_number")]
    pub cpu_some: f64,
    #[serde(serialize_with = "pressure_number")]
    pub mem_some: f64,
    #[serde(serialize_with = "pressure_number")]
    pub mem_full: f64,
    #[serde(serialize_with = "pressure_number")]
    pub io_some: f64,
    #[serde(serialize_with = "pressure_number")]
    pub io_full: f64,
}

impl Pressure {
    pub fn is_valid(&self) -> bool {
        [
            self.cpu_some,
            self.mem_some,
            self.mem_full,
            self.io_some,
            self.io_full,
        ]
        .into_iter()
        .all(|value| (0.0..=100.0).contains(&value) && two_decimal_places(value))
    }
}

fn pressure_number<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    // The broker's canonical bodies write a whole pressure-stall (PSI) percentage
    // as an integer (`0`, not `0.0`), so ours must too for the bytes to match.
    if value.is_finite() && (0.0..=100.0).contains(value) && value.fract() == 0.0 {
        serializer.serialize_u64(*value as u64)
    } else {
        serializer.serialize_f64(*value)
    }
}

fn optional_pressure<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Pressure>, D::Error> {
    Pressure::deserialize(deserializer).map(Some)
}

fn two_decimal_places(value: f64) -> bool {
    value.is_finite() && (value * 100.0 - (value * 100.0).round()).abs() <= 0.000001
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
    // The outer option preserves omission; the inner option preserves null.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_nullable"
    )]
    pub disk_free_gib: Option<Option<u64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_nullable"
    )]
    pub disk_total_gib: Option<Option<u64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_pressure"
    )]
    pub pressure: Option<Pressure>,
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

fn optional_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
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
        if self.cpu.as_ref().is_some_and(|cpu| {
            cpu.busy_pct > 100
                || cpu
                    .load1
                    .is_some_and(|load| load < 0.0 || !two_decimal_places(load))
        }) {
            return Err("cpu");
        }
        if self.pressure.as_ref().is_some_and(|p| !p.is_valid()) {
            return Err("pressure");
        }
        if self.disks.len() > 8 || self.tenants.len() > 8 || self.runners.len() > 8 {
            return Err("body");
        }
        if self.disks.iter().any(|d| !text(&d.mount, 128)) {
            return Err("disks.mount");
        }
        for t in &self.tenants {
            if t.pressure.as_ref().is_some_and(|p| !p.is_valid()) {
                return Err("tenants.pressure");
            }
            if t.disk_free_gib.is_some() != t.disk_total_gib.is_some()
                || t.disk_free_gib
                    .flatten()
                    .zip(t.disk_total_gib.flatten())
                    .is_some_and(|(free, total)| free > total)
            {
                return Err("tenants.disk");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn sample() -> HostMetrics {
        let vector: Value =
            serde_json::from_str(include_str!("../contracts/cloud/host-metrics.json")).unwrap();
        serde_json::from_str(vector["cases"][12]["request"]["body"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn pressure_validation_checks_every_host_and_tenant_value() {
        let base = sample();
        for tenant in [false, true] {
            for field in ["cpu_some", "mem_some", "mem_full", "io_some", "io_full"] {
                for (number, valid) in [
                    (0.0, true),
                    (100.0, true),
                    (1.23, true),
                    (-0.01, false),
                    (100.01, false),
                    (1.234, false),
                ] {
                    let mut body = serde_json::to_value(&base).unwrap();
                    let pressure = if tenant {
                        &mut body["tenants"][0]["pressure"]
                    } else {
                        &mut body["pressure"]
                    };
                    pressure[field] = json!(number);
                    let parsed: HostMetrics = serde_json::from_value(body).unwrap();
                    assert_eq!(
                        parsed.encode(base.at_ms).is_ok(),
                        valid,
                        "{tenant} {field} {number}"
                    );
                }
            }
        }
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut body = base.clone();
            body.pressure.as_mut().unwrap().cpu_some = number;
            assert_eq!(body.encode(base.at_ms).unwrap_err(), "pressure");
            body.pressure = None;
            body.tenants[0].pressure.as_mut().unwrap().io_full = number;
            assert_eq!(body.encode(base.at_ms).unwrap_err(), "tenants.pressure");
        }
    }

    #[test]
    fn pressure_may_be_omitted_independently_but_never_null_or_partial() {
        let base = sample();
        for tenant in [false, true] {
            let mut body = serde_json::to_value(&base).unwrap();
            let object = if tenant {
                &mut body["tenants"][0]
            } else {
                &mut body
            };
            let pressure = object.as_object_mut().unwrap().remove("pressure").unwrap();
            let parsed: HostMetrics = serde_json::from_value(body.clone()).unwrap();
            let encoded: Value =
                serde_json::from_slice(&parsed.encode(base.at_ms).unwrap()).unwrap();
            assert_eq!(encoded, body);
            for invalid in [Value::Null, json!({}), json!({"cpu_some":0})] {
                let mut invalid_body = body.clone();
                let object = if tenant {
                    &mut invalid_body["tenants"][0]
                } else {
                    &mut invalid_body
                };
                object["pressure"] = invalid;
                assert!(serde_json::from_value::<HostMetrics>(invalid_body).is_err());
            }
            for field in ["cpu_some", "mem_some", "mem_full", "io_some", "io_full"] {
                for missing in [false, true] {
                    let mut invalid_body = body.clone();
                    let object = if tenant {
                        &mut invalid_body["tenants"][0]
                    } else {
                        &mut invalid_body
                    };
                    object["pressure"] = pressure.clone();
                    if missing {
                        object["pressure"].as_object_mut().unwrap().remove(field);
                    } else {
                        object["pressure"][field] = Value::Null;
                    }
                    assert!(serde_json::from_value::<HostMetrics>(invalid_body).is_err());
                }
            }
        }
    }
}
