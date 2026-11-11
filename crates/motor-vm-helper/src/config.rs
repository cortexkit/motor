use crate::{Error, Result};
use serde::{de::IgnoredAny, Deserialize};
use std::{collections::BTreeMap, net::Ipv4Addr, path::PathBuf};

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
pub struct JobVms {
    pub uid_base: u32,
    pub uid_count: u32,
    /// Aggregate ceiling leaves memory reserved for host services. VM leaves
    /// deliberately have no memory limit, so account-shared persistent-memory
    /// (virtio-pmem) image pages are not reclaimed against one VM's limit.
    pub memory_max_bytes: u64,
    /// Root-controlled ceiling for sparse workspace provisioning. Missing means
    /// creation is disabled, rather than accepting an unbounded host request.
    #[serde(default)]
    pub workspace_disk_max_bytes: Option<u64>,
    /// Root-owned repository allowlists, keyed by the workspace's account.
    #[serde(default)]
    pub fixed_path_repositories: BTreeMap<String, Vec<String>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub job_vms: Option<JobVms>,
    #[serde(default)]
    pub uplink: String,
    #[serde(default = "missing_public_ipv4")]
    pub public_ipv4: Ipv4Addr,
    pub allowed_uids: Vec<u32>,
    pub socket_gid: u32,
    #[serde(default = "default_uid_base")]
    pub uid_base: u32,
    pub job_account_range: UidRange,
    pub slots: Vec<SlotConfig>,
    pub jailer: PathBuf,
    pub firecracker: PathBuf,
    // Installed tenant-host configs still contain these retired slot settings.
    // Accept and discard them so an upgrade needs no config rewrite or loader.
    pub loader: Option<IgnoredAny>,
    pub job_snapshot: Option<IgnoredAny>,
    pub scan_snapshot: Option<IgnoredAny>,
    pub hugepages_per_slot: Option<IgnoredAny>,
    pub agent_port: Option<IgnoredAny>,
    pub proxy_port: Option<IgnoredAny>,
    pub proxy_for_jobs: Option<IgnoredAny>,
}
fn missing_public_ipv4() -> Ipv4Addr {
    Ipv4Addr::UNSPECIFIED
}

fn default_uid_base() -> u32 {
    200000
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        self.validate_resources()?;
        self.validate_network()
    }

    pub(crate) fn validate_network(&self) -> Result<()> {
        if !valid_uplink(&self.uplink) || !public_ipv4(self.public_ipv4) {
            return Err(Error(format!(
                "invalid uplink {} or public IPv4 address",
                if self.uplink.is_empty() {
                    "<missing>"
                } else {
                    &self.uplink
                }
            )));
        }
        Ok(())
    }

    pub(crate) fn validate_resources(&self) -> Result<()> {
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
        if let Some(jobs) = &self.job_vms {
            jobs.validate_fixed_path_repositories()?;
            let jobs_end = jobs
                .uid_base
                .checked_add(jobs.uid_count)
                .ok_or_else(|| Error("job VMM uid range overflow".into()))?;
            if jobs.uid_base == 0
                || jobs.uid_count == 0
                || jobs.memory_max_bytes == 0
                || jobs.memory_max_bytes % 4096 != 0
                || [
                    (self.uid_base, end),
                    (self.job_account_range.base, job_end),
                    (400000, 400256),
                ]
                .iter()
                .any(|(base, end)| jobs.uid_base < *end && *base < jobs_end)
                || self
                    .allowed_uids
                    .iter()
                    .any(|uid| (jobs.uid_base..jobs_end).contains(uid))
            {
                return Err(Error(
                    "invalid or overlapping job VMM uid range or memory ceiling".into(),
                ));
            }
            if jobs
                .workspace_disk_max_bytes
                .is_some_and(|maximum| maximum == 0 || maximum % 512 != 0)
            {
                return Err(Error(
                    "workspace_disk_max_bytes must be positive and sector aligned".into(),
                ));
            }
        }
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
        for path in [&self.jailer, &self.firecracker] {
            if !path.is_absolute() {
                return Err(Error("helper config paths must be absolute".into()));
            }
        }
        Ok(())
    }

    #[cfg(any(test, target_os = "linux"))]
    pub(crate) fn deprecated_slot_fields(&self) -> Vec<&'static str> {
        [
            ("loader", self.loader),
            ("job_snapshot", self.job_snapshot),
            ("scan_snapshot", self.scan_snapshot),
            ("hugepages_per_slot", self.hugepages_per_slot),
            ("agent_port", self.agent_port),
            ("proxy_port", self.proxy_port),
            ("proxy_for_jobs", self.proxy_for_jobs),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|_| name))
        .collect()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn warn_deprecated_slot_fields(&self) {
        static WARNING: std::sync::Once = std::sync::Once::new();
        let fields = self.deprecated_slot_fields();
        if !fields.is_empty() {
            WARNING.call_once(|| {
                eprintln!(
                    "motor-vm-helper: deprecated legacy slot config fields ignored: {}",
                    fields.join(", ")
                );
            });
        }
    }
}
impl JobVms {
    pub(crate) fn validate_fixed_path_repositories(&self) -> Result<()> {
        for (account, roots) in &self.fixed_path_repositories {
            crate::tenant::validate_account_binding(account)?;
            let mut seen = std::collections::BTreeSet::new();
            for root in roots {
                if (root == "*" && roots.len() != 1)
                    || (root != "*" && !std::path::Path::new(root).is_absolute())
                    || root.chars().any(char::is_whitespace)
                    || root.contains('\0')
                    || !seen.insert(root)
                {
                    return Err(Error(
                        "job_vms.fixed_path_repositories requires distinct absolute roots without whitespace, or '*' alone".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}
pub(crate) fn valid_uplink(value: &str) -> bool {
    (1..=15).contains(&value.len())
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value != "any"
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && value != "."
        && value != ".."
        && value != "lo"
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (b == 18 || b == 19)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job_config() -> Config {
        let mut config: Config =
            serde_json::from_str(include_str!("../fixtures/helper.json")).unwrap();
        config.job_vms = Some(JobVms {
            uid_base: 500000,
            uid_count: 128,
            memory_max_bytes: 4096,
            workspace_disk_max_bytes: Some(512),
            fixed_path_repositories: Default::default(),
        });
        config
    }

    #[test]
    fn job_workspace_maximum_must_be_positive_and_sector_aligned_when_enabled() {
        let mut config = job_config();
        for maximum in [None, Some(512), Some(1024)] {
            config.job_vms.as_mut().unwrap().workspace_disk_max_bytes = maximum;
            config.validate_resources().unwrap();
        }
        for maximum in [0, 1, 511, 513, u64::MAX] {
            config.job_vms.as_mut().unwrap().workspace_disk_max_bytes = Some(maximum);
            assert!(config
                .validate_resources()
                .unwrap_err()
                .0
                .contains("workspace_disk_max_bytes"));
        }
    }

    #[test]
    fn job_uid_pool_excludes_both_edges_of_reserved_tenant_range() {
        let mut config = job_config();
        for (base, count) in [(399999, 1), (400256, 1)] {
            let jobs = config.job_vms.as_mut().unwrap();
            jobs.uid_base = base;
            jobs.uid_count = count;
            config.validate_resources().unwrap();
        }
        for (base, count) in [(399999, 2), (400000, 1), (400255, 1)] {
            let jobs = config.job_vms.as_mut().unwrap();
            jobs.uid_base = base;
            jobs.uid_count = count;
            assert!(config.validate_resources().is_err());
        }
    }

    #[test]
    fn job_fixed_path_map_defaults_off_and_accepts_roots_or_wildcard_per_subject() {
        let account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let org = "org:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let settings: JobVms = serde_json::from_value(serde_json::json!({
            "uid_base": 500000, "uid_count": 128, "memory_max_bytes": 4096
        }))
        .unwrap();
        assert!(settings.fixed_path_repositories.is_empty());
        let mut config = job_config();
        for map in [
            serde_json::json!({}),
            serde_json::json!({account: []}),
            serde_json::json!({account: ["/home/repo", "/Users/repo"], org: ["*"]}),
        ] {
            config.job_vms.as_mut().unwrap().fixed_path_repositories =
                serde_json::from_value(map).unwrap();
            config.validate_resources().unwrap();
        }
    }

    #[test]
    fn job_fixed_path_map_refuses_malformed_subjects_roots_and_json_shapes() {
        let account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let mut config = job_config();
        for map in [
            serde_json::json!({"account-1": ["/repo"]}),
            serde_json::json!({"acct:bad": ["/repo"]}),
            serde_json::json!({"user:01ARZ3NDEKTSV4RRFFQ69G5FAV": ["/repo"]}),
            serde_json::json!({account: [""]}),
            serde_json::json!({account: ["relative"]}),
            serde_json::json!({account: ["/repo", "/repo"]}),
            serde_json::json!({account: ["*", "/repo"]}),
            serde_json::json!({account: ["*", "*"]}),
            serde_json::json!({account: ["/with space"]}),
            serde_json::json!({account: ["/with\t tab"]}),
            serde_json::json!({account: ["/with\nnewline"]}),
            serde_json::json!({account: ["/with\u{2003}space"]}),
            serde_json::json!({account: ["/with\0nul"]}),
        ] {
            config.job_vms.as_mut().unwrap().fixed_path_repositories =
                serde_json::from_value(map.clone()).unwrap();
            assert!(config.validate_resources().is_err(), "accepted {map}");
        }
        for map in [
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!({account: "*"}),
            serde_json::json!({account: [1]}),
        ] {
            let settings = serde_json::json!({
                "uid_base": 500000, "uid_count": 128, "memory_max_bytes": 4096,
                "fixed_path_repositories": map
            });
            assert!(serde_json::from_value::<JobVms>(settings).is_err());
        }
    }
}
