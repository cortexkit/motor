use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostMode {
    #[default]
    Tenant,
    PerJob,
}

impl HostMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::PerJob => "per_job",
        }
    }
}

/// Operator-selected bounds. None of these values has a compiled-in default.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PerJobSettings {
    pub host_reserve_mib: u64,
    pub boot_reserve_mib: u64,
    pub per_vm_overhead_mib: u64,
    pub reclaim_timeout_ms: u64,
    pub hysteresis_mib: u64,
    pub workspace_disk_cap_bytes: u64,
    pub boot_deadline_ms: u64,
    pub recovery_timeout_ms: u64,
    pub exit_evidence_bound_ms: u64,
    pub teardown_timeout_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HostSettings {
    #[default]
    Tenant,
    PerJob(PerJobSettings),
}

impl HostSettings {
    pub fn from_config(mode: HostMode, per_job: Option<PerJobSettings>) -> Result<Self> {
        Ok(match mode {
            HostMode::Tenant => Self::Tenant,
            HostMode::PerJob => Self::PerJob(per_job.context("per_job settings are required")?),
        })
    }

    pub fn mode(&self) -> HostMode {
        match self {
            Self::Tenant => HostMode::Tenant,
            Self::PerJob(_) => HostMode::PerJob,
        }
    }
}
