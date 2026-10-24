use crate::{
    tenant::{start_refused, Registry, StartRefusal, StartRefusalCase::*},
    Error, Result,
};
use serde::Deserialize;
use std::path::Path;

pub(super) const CONFIG: &str = "/etc/motor-host/capacity.json";
pub(super) const DROP_IN: &str = "/etc/systemd/system/motor-jobs.slice.d/50-capacity.conf";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Capacity {
    windows_vm_bytes: u64,
    host_reserve_bytes: u64,
    jobs_min_bytes: u64,
    #[serde(rename = "storage_margin_bytes")]
    _storage_margin_bytes: u64,
    #[serde(rename = "growth_allowance_bytes")]
    _growth_allowance_bytes: u64,
    #[serde(rename = "tenant_memory_reserve_bytes")]
    _tenant_memory_reserve_bytes: u64,
}

impl Capacity {
    fn parse(bytes: &[u8]) -> std::result::Result<Self, StartRefusal> {
        let capacity: Self =
            serde_json::from_slice(bytes).map_err(|error| start_refused(CapacityJson, error))?;
        if capacity.jobs_min_bytes == 0 || capacity.windows_vm_bytes > capacity.jobs_min_bytes {
            return Err(start_refused(
                CapacityJson,
                "requires jobs_min_bytes >= 1 and windows_vm_bytes <= jobs_min_bytes",
            ));
        }
        capacity.memory_max()?;
        Ok(capacity)
    }

    fn memory_max(&self) -> std::result::Result<u64, StartRefusal> {
        self.jobs_min_bytes
            .checked_add(self.windows_vm_bytes)
            .ok_or_else(|| {
                start_refused(
                    CapacityJson,
                    "jobs_min_bytes + windows_vm_bytes overflows u64",
                )
            })
    }

    pub(super) fn check_fit(
        &self,
        registries: &[Registry],
        meminfo: &str,
    ) -> std::result::Result<(), StartRefusal> {
        let check = || -> Result<()> {
            let mut totals = meminfo
                .lines()
                .filter_map(|line| line.strip_prefix("MemTotal:"));
            let mut fields = totals
                .next()
                .ok_or_else(|| Error("missing MemTotal".into()))?
                .split_whitespace();
            let kb = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| Error("invalid MemTotal".into()))?;
            if fields.next() != Some("kB") || fields.next().is_some() || totals.next().is_some() {
                return Err(Error("invalid MemTotal units or duplicate".into()));
            }
            let total = kb
                .checked_mul(1024)
                .ok_or_else(|| Error("MemTotal overflow".into()))?;
            let mut reserved = self
                .windows_vm_bytes
                .checked_add(self.host_reserve_bytes)
                .and_then(|v| v.checked_add(self.jobs_min_bytes))
                .ok_or_else(|| Error("memory reservation overflow".into()))?;
            for registry in registries {
                reserved = reserved
                    .checked_add(registry.memory_max()?)
                    .ok_or_else(|| Error("tenant memory sum overflow".into()))?;
            }
            if reserved > total {
                return Err(Error(format!(
                    "reserved_bytes={reserved} exceeds MemTotal_bytes={total}"
                )));
            }
            Ok(())
        };
        check().map_err(|error| start_refused(Fit, error))
    }

    fn drop_in(&self) -> std::result::Result<String, StartRefusal> {
        let max = self.memory_max()?;
        Ok(format!(
            "[Slice]\nMemoryMax={}\nMemoryHigh={}\n",
            max,
            max - max / 6
        ))
    }

    pub(super) fn check_live_cap(
        &self,
        output: &str,
        drop_in: impl FnOnce() -> Result<Vec<u8>>,
    ) -> std::result::Result<(), StartRefusal> {
        let mut load = None;
        let mut max = None;
        for line in output.lines() {
            match line.split_once('=') {
                Some(("LoadState", value)) if load.is_none() => load = Some(value),
                Some(("MemoryMax", value)) if max.is_none() => max = Some(value),
                _ => {
                    return Err(start_refused(
                        LiveCap,
                        format!("invalid systemctl show line {line:?}"),
                    ))
                }
            }
        }
        match load {
            Some("loaded") => {
                let bytes = max
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        start_refused(LiveCap, "MemoryMax missing, invalid or infinity")
                    })?;
                let memory_max = self.memory_max()?;
                if bytes > memory_max {
                    return Err(start_refused(
                        LiveCap,
                        format!("MemoryMax={bytes} exceeds {memory_max}"),
                    ));
                }
            }
            Some("not-found") => {
                let bytes = drop_in().map_err(|error| start_refused(DropIn, error))?;
                if bytes != self.drop_in()?.as_bytes() {
                    return Err(start_refused(
                        DropIn,
                        "motor-jobs.slice drop-in differs from capacity.json render",
                    ));
                }
            }
            _ => {
                return Err(start_refused(
                    LiveCap,
                    "motor-jobs.slice is not loaded or not-found",
                ))
            }
        }
        Ok(())
    }
}

pub(super) fn read_capacity(path: &Path) -> std::result::Result<Capacity, StartRefusal> {
    let file = super::fs::trusted_file(path).map_err(|error| start_refused(CapacityJson, error))?;
    let bytes =
        std::io::read_to_string(file).map_err(|error| start_refused(CapacityJson, error))?;
    Capacity::parse(bytes.as_bytes())
}

pub(super) fn check_journal(home: &Path) -> std::result::Result<(), StartRefusal> {
    let path = home.join("resize.json");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(start_refused(
            ResizeJournal,
            format!("{} exists", path.display()),
        )),
        Err(error) => Err(start_refused(ResizeJournal, error)),
    }
}

#[cfg(test)]
mod tests;
