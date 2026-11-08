//! Connection-independent job VMs. Callers name identities, never host paths.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

pub const WORKSPACES: &str = "/var/lib/motor-vm/workspaces";
pub const IMAGES: &str = "/var/lib/motor-vm/job-images";
pub const RECORDS: &str = "/var/lib/motor-vm/jobs";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub job_id: String,
    pub account: String,
    pub workspace_disk_id: String,
    pub image_generation_id: String,
    pub vcpus: u32,
    /// Guest RAM, already sized by the host for the job limit and boot overhead.
    pub memory_mib: u64,
    pub burst_mib: Option<u64>,
}

impl Start {
    pub fn validate(&self) -> Result<()> {
        validate_job_id(&self.job_id)?;
        for id in [
            &self.account,
            &self.workspace_disk_id,
            &self.image_generation_id,
        ] {
            validate_id(id)?;
        }
        if !(1..=32).contains(&self.vcpus) || self.memory_mib == 0 {
            return Err(Error("invalid job VM size".into()));
        }
        // Firecracker converts MiB to bytes. Reject overflow before launch.
        self.memory_mib
            .checked_mul(1048576)
            .ok_or_else(|| Error("job VM RAM overflow".into()))?;
        Ok(())
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(Error("invalid job resource identity".into()));
    }
    Ok(())
}

pub fn validate_job_id(id: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(id).map_err(|_| Error("invalid job id".into()))?;
    if parsed.to_string() != id {
        return Err(Error("job id must be a canonical UUID".into()));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub account: String,
    pub workspace_disk_id: String,
}

/// Immutable evidence for one job VMM exit. The helper proves the cgroup empty
/// and syncs this count before removing the cgroup or releasing the disk. Polls
/// replay it until the host acknowledges durable receipt. Released, acknowledged
/// tombstones reject start_job replays for 48 hours after their last record
/// update. This outlasts the longest job plus the remaining validity of its lease
/// token: job ids are fresh UUIDv7 values per lease and the host refuses expired
/// tokens before helper calls.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitReport {
    pub job_id: String,
    pub oom_kill: u64,
}

pub enum Operation {
    Start(Start),
    Adopt(String),
    Stop(String),
    Relay(String, u32),
    Exits,
    AckExit(ExitReport),
}

impl Operation {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Start(start) => start.validate(),
            Self::Adopt(id) | Self::Stop(id) => validate_job_id(id),
            Self::Exits => Ok(()),
            Self::AckExit(report) => validate_job_id(&report.job_id),
            Self::Relay(id, port) => {
                validate_job_id(id)?;
                crate::tenant::validate_port(*port)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_identities_cannot_select_paths() {
        for id in ["", ".", "..", "a/b", "/a", "a\\b", "a\n", "a.json"] {
            assert!(validate_id(id).is_err(), "{id:?}");
        }
        validate_id("account-1_workspace").unwrap();
        assert!(validate_job_id("00000000000000000000000000000000").is_err());
    }
}
