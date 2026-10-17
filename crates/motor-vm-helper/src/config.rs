use crate::{Error, Result};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub vmstate: PathBuf,
    pub rootfs: PathBuf,
    pub memory: PathBuf,
    pub record: PathBuf,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotConfig {
    pub cpus: String,
    pub mems: String,
    pub cpu_max: String,
    pub memory_max: u64,
    pub io_weight: u32,
    pub io_max: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UidRange {
    pub base: u32,
    pub count: u32,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub allowed_uids: Vec<u32>,
    pub socket_gid: u32,
    #[serde(default = "default_uid_base")]
    pub uid_base: u32,
    pub job_account_range: UidRange,
    pub slots: Vec<SlotConfig>,
    pub jailer: PathBuf,
    pub firecracker: PathBuf,
    pub loader: PathBuf,
    pub job_snapshot: Snapshot,
    pub scan_snapshot: Snapshot,
    pub hugepages_per_slot: u64,
    pub agent_port: u32,
    pub proxy_port: u32,
    #[serde(default)]
    pub proxy_for_jobs: bool,
}
fn default_uid_base() -> u32 {
    200000
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        let count = u32::try_from(self.slots.len()).map_err(|_| Error("too many slots".into()))?;
        let end = self
            .uid_base
            .checked_add(count)
            .ok_or_else(|| Error("slot uid range overflow".into()))?;
        let job_end = self
            .job_account_range
            .base
            .checked_add(self.job_account_range.count)
            .ok_or_else(|| Error("job account uid range overflow".into()))?;
        if count == 0
            || self.allowed_uids.is_empty()
            || self.uid_base == 0
            || (self.uid_base < job_end && self.job_account_range.base < end)
            || self
                .allowed_uids
                .iter()
                .any(|u| (*u >= self.uid_base && *u < end) || *u == 0)
        {
            return Err(Error("invalid or overlapping slot uid range".into()));
        }
        if self.hugepages_per_slot == 0
            || self.agent_port == 0
            || self.proxy_port == 0
            || self.agent_port == self.proxy_port
        {
            return Err(Error("invalid capacity or vsock ports".into()));
        }
        for slot in &self.slots {
            if !cpu_list(&slot.cpus)
                || !cpu_list(&slot.mems)
                || slot.memory_max == 0
                || !(1..=10000).contains(&slot.io_weight)
            {
                return Err(Error("invalid cgroup limits".into()));
            }
            let parts: Vec<_> = slot.cpu_max.split_whitespace().collect();
            if parts.len() != 2 || !parts.iter().all(|s| s.parse::<u64>().is_ok_and(|n| n > 0)) {
                return Err(Error("cpu_max must be numeric quota and period".into()));
            }
        }
        for path in [
            &self.jailer,
            &self.firecracker,
            &self.loader,
            &self.job_snapshot.vmstate,
            &self.job_snapshot.rootfs,
            &self.job_snapshot.memory,
            &self.job_snapshot.record,
            &self.scan_snapshot.vmstate,
            &self.scan_snapshot.rootfs,
            &self.scan_snapshot.memory,
            &self.scan_snapshot.record,
        ] {
            if !path.is_absolute() {
                return Err(Error("helper config paths must be absolute".into()));
            }
        }
        Ok(())
    }
}
pub(crate) fn cpu_list(s: &str) -> bool {
    !s.is_empty()
        && s.split(',').all(|range| {
            let parts: Vec<_> = range.split('-').collect();
            match parts.as_slice() {
                [n] => n.parse::<u32>().is_ok(),
                [a, b] => a
                    .parse::<u32>()
                    .ok()
                    .zip(b.parse::<u32>().ok())
                    .is_some_and(|(a, b)| a <= b),
                _ => false,
            }
        })
}
/// Only fixed image sizes are consumed here; the runner validates the complete
/// snapshot settings and manifest before submitting a job.
#[derive(Debug, Deserialize)]
pub struct SnapshotRecord {
    pub image_sizes: BTreeMap<String, u64>,
}
