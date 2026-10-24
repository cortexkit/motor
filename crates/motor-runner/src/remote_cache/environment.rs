//! The job environment the remote compile cache adds on each host type.
//!
//! `script/runner/remote-cache-environment.py` holds the same data for the
//! toolchain parity check, and a test here fails if the two disagree. Only
//! values that do not depend on the cache tenant's provisioning files live
//! here: the bucket, region, signing endpoint and credentials are written into
//! a unit's environment by the runner from the active generation.
use std::collections::BTreeMap;

use super::gateway::Tool;
use super::provisioning::HostRole;
use super::transport::Forwarder;

/// Settings inherited from the runner's own environment are removed from
/// every job before the runner adds anything, on every host type.
pub(crate) const INHERITED_PREFIXES: [&str; 3] = ["SCCACHE_", "AWS_", "CCACHE_"];

pub(crate) const READ_ONLY_MODE: &str = "READ_ONLY";
pub(crate) const READ_WRITE_MODE: &str = "READ_WRITE";

/// The switch-on job environment of one host type, as that role's switch-off
/// environment plus exactly this delta. `forwarders` are the unit's cache
/// forwarders, one per active tool.
pub(crate) fn delta(
    role: HostRole,
    forwarders: &[Forwarder],
    bucket: &str,
    region: &str,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    // Each tool's forwarder exposes that tool's own socket as a URL.
    if let Some(forwarder) = forwarders.iter().find(|f| f.tool() == Tool::Sccache) {
        env.extend(small_values(role));
        env.insert("SCCACHE_BUCKET".into(), bucket.into());
        env.insert("SCCACHE_REGION".into(), region.into());
        env.insert("SCCACHE_ENDPOINT".into(), forwarder.url());
    }
    env
}

/// The delta's values that do not depend on the active generation.
pub(crate) fn small_values(role: HostRole) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = [
        ("SCCACHE_S3_USE_SSL", "false"),
        ("SCCACHE_S3_ENABLE_VIRTUAL_HOST_STYLE", "false"),
        ("SCCACHE_S3_RW_MODE", READ_ONLY_MODE),
        // The gateway signs every request, so the tool only needs credentials
        // that are present. They are not secrets and never leave the unit.
        ("AWS_ACCESS_KEY_ID", "remote-cache"),
        ("AWS_SECRET_ACCESS_KEY", "remote-cache"),
        // Without this the SDK tries the instance metadata service first.
        ("AWS_EC2_METADATA_DISABLED", "true"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    match role {
        // The read-only shared cache is level 1 and the store is level 2. The
        // local settings themselves keep their baseline values.
        HostRole::BuildServer => {
            env.insert("SCCACHE_MULTILEVEL_CHAIN".into(), "disk,s3".into());
        }
        // Boat and tenant VMs have no level 1, so there is no chain. They also
        // have no compile-cache baseline at all, so the runner's wrapper is
        // part of the delta there.
        HostRole::Boat | HostRole::TenantVm => {
            env.insert("RUSTC_WRAPPER".into(), "/motor-tools/sccache".into());
        }
    }
    env
}

/// What a fill adds to a job's delta: each put waits for every read-write
/// level and fails if any fails, so a remote write failure shows up in the
/// fill's own statistics.
pub(crate) fn fill_values() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY".to_string(),
        "all".to_string(),
    )])
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
