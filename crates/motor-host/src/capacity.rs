//! The host capacity file, `/etc/motor-host/capacity.json`.
//!
//! The operator writes it once per host; provisioning, the VM helper and
//! motor-host all read the same six byte values. motor-host uses two of them:
//! `storage_margin_bytes`, which sets the storage refusal threshold and its
//! release point, and `tenant_memory_reserve_bytes`, which is subtracted from
//! every tenant's advertised free memory. A file that is missing, unreadable,
//! malformed, or not protected from other users is never used: the caller
//! treats it like a failed free-space probe and refuses new leases.
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Where the operator installs the file.
pub const CONFIG: &str = "/etc/motor-host/capacity.json";

/// The six keys are well under this; a larger file is not a capacity file.
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    pub windows_vm_bytes: u64,
    pub host_reserve_bytes: u64,
    pub jobs_min_bytes: u64,
    pub storage_margin_bytes: u64,
    pub growth_allowance_bytes: u64,
    pub tenant_memory_reserve_bytes: u64,
}

impl Capacity {
    /// Parses the file body. Every key is required and no other key is
    /// allowed; each value must be a non-negative integer that fits in u64.
    /// The bounds match the VM helper's reading of the same file, so the two
    /// never disagree about whether a host is configured.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let capacity: Self = serde_json::from_slice(bytes).context("capacity.json is invalid")?;
        ensure!(
            capacity.jobs_min_bytes >= 1,
            "capacity.json requires jobs_min_bytes >= 1"
        );
        ensure!(
            capacity
                .jobs_min_bytes
                .checked_add(capacity.windows_vm_bytes)
                .is_some(),
            "capacity.json jobs_min_bytes + windows_vm_bytes overflows"
        );
        Ok(capacity)
    }
}

/// Reads and validates the capacity file at `path`. The file must be a
/// regular file (a symlink is refused) owned by `owner` and writable by
/// neither its group nor other users. Production passes uid 0 (root).
pub fn read(path: &Path, owner: u32) -> Result<Capacity> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o022 != 0
        || metadata.len() > MAX_BYTES
    {
        bail!(
            "{} must be a regular file owned by uid {owner} and not group- or world-writable: uid={} mode={:04o} bytes={}",
            path.display(),
            metadata.uid(),
            metadata.mode() & 0o7777,
            metadata.len()
        );
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    Capacity::parse(&bytes)
}
