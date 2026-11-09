//! Internal guest transfers, separate from the caller's job/result stream.
use crate::{BundleManifest, GitDependency};
use serde::{Deserialize, Serialize};
use std::io;

pub const FILL_SOURCE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Export holds a completed job's VM, not its billable execution, for at most
/// this long. A timeout discards only the new source, never the job result.
pub const FILL_SOURCE_EXPORT_TIMEOUT_S: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FillSource {
    pub repository_root: String,
    pub integration_tip: String,
    pub bundles: Vec<BundleManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_dependencies: Vec<GitDependency>,
}

impl FillSource {
    pub fn validate(&self, cap: u64) -> io::Result<u64> {
        let invalid = || io::Error::other("invalid or oversized fill source manifest");
        if !self.repository_root.starts_with('/')
            || !matches!(self.integration_tip.len(), 40 | 64)
            || !self.integration_tip.bytes().all(|b| b.is_ascii_hexdigit())
            || self.bundles.is_empty()
            || self.bundles.len() > 1024
            || self.bundles[0].repository != self.repository_root
        {
            return Err(invalid());
        }
        let mut total = 0u64;
        for (index, bundle) in self.bundles.iter().enumerate() {
            if bundle.index as usize != index
                || bundle.length == 0
                || bundle.sha256.len() != 64
                || !bundle
                    .sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(invalid());
            }
            total = total.checked_add(bundle.length).ok_or_else(invalid)?;
        }
        if total > cap || total > FILL_SOURCE_MAX_BYTES {
            return Err(invalid());
        }
        if self.git_dependencies.len() + 1 != self.bundles.len() {
            return Err(invalid());
        }
        for (dependency, bundle) in self.git_dependencies.iter().zip(&self.bundles[1..]) {
            if dependency.bundle.as_ref() != Some(bundle) || dependency.url != bundle.repository {
                return Err(invalid());
            }
        }
        Ok(total)
    }
}
