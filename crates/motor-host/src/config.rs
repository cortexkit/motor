use anyhow::{ensure, Context, Result};
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
    pub vcpu_ceiling: u32,
    pub permit_budget: u32,
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

impl PerJobSettings {
    pub(crate) fn validate_cpus(&self, online: u32) -> Result<()> {
        ensure!(
            self.vcpu_ceiling >= 1 && self.vcpu_ceiling <= online,
            "per_job vcpu_ceiling must be between 1 and online CPUs"
        );
        ensure!(
            self.permit_budget >= self.vcpu_ceiling
                && u64::from(self.permit_budget) <= 4 * u64::from(online),
            "per_job permit_budget must be between vcpu_ceiling and 4 * online CPUs"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HostSettings {
    #[default]
    Tenant,
    PerJob(PerJobSettings),
}

impl HostSettings {
    pub fn from_config(
        mode: HostMode,
        per_job: Option<PerJobSettings>,
        control: &dyn crate::helper::Control,
    ) -> Result<Self> {
        Ok(match mode {
            HostMode::Tenant => Self::Tenant,
            HostMode::PerJob => {
                let settings = per_job.context("per_job settings are required")?;
                settings.validate_cpus(control.online_cpus()?)?;
                Self::PerJob(settings)
            }
        })
    }

    pub fn mode(&self) -> HostMode {
        match self {
            Self::Tenant => HostMode::Tenant,
            Self::PerJob(_) => HostMode::PerJob,
        }
    }
}
