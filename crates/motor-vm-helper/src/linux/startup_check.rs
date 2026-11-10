//! Read-only prerequisites shared by service startup and the operator preflight.
use super::{fs, job::network, process, BUILD_ROOT};
use crate::{config::Config, Error, Result};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

/// Check inputs without creating jails, changing cgroups, or adopting job VMs.
/// Existing job records are parsed, but startup must still validate their
/// compatibility with the configured UID pool while adopting them.
pub fn check(config: &Config) -> Result<File> {
    check_settings(
        config,
        Path::new(crate::tenant::REGISTRY),
        Path::new(crate::job::RECORDS),
    )?;
    process::refuse_passwd_entries(config.uid_base, config.slots.len())?;
    for path in [&config.jailer, &config.firecracker] {
        fs::trusted_file(path)?;
    }
    if config.firecracker.file_name().and_then(|n| n.to_str()) != Some("firecracker") {
        return Err(Error(
            "configured Firecracker executable must be named firecracker".into(),
        ));
    }
    let base = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(BUILD_ROOT)?;
    fs::trusted(Path::new(BUILD_ROOT), &base.metadata()?)?;
    for ancestor in Path::new(BUILD_ROOT).ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
    }
    let jail = Path::new(BUILD_ROOT).join("jail");
    if jail.try_exists()? {
        fs::trusted(&jail, &std::fs::symlink_metadata(&jail)?)?;
    }
    Ok(base)
}

fn check_settings(config: &Config, registries: &Path, records: &Path) -> Result<()> {
    config.validate_resources()?;
    network::check_startup(config, registries, records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_checks_tap_capacity_without_creating_state() {
        let scratch = cortexkit_test_support::scratch_root().join(format!(
            "helper-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let registries = scratch.join("registries");
        let records = scratch.join("jobs");
        let mut config: Config =
            serde_json::from_str(include_str!("../../fixtures/helper.json")).unwrap();
        config.job_vms = Some(crate::config::JobVms {
            uid_base: 500000,
            uid_count: 254,
            memory_max_bytes: 4096,
            workspace_disk_max_bytes: Some(512),
        });
        check_settings(&config, &registries, &records).unwrap();
        config.job_vms.as_mut().unwrap().uid_count = 255;
        assert!(check_settings(&config, &registries, &records)
            .unwrap_err()
            .0
            .contains("255 + registered tenants 0 exceeds TAP range 254"));
        assert!(!scratch.exists());
    }
}
