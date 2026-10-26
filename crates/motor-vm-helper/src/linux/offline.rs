//! Root-only removal when network validation prevents the helper from serving.
//! Keep ordinary helper startup barriers intact; this path never starts a VM.
use super::{fs, tenant_network};
use crate::{removal, tenant::Registry, Error, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn stopped_units() -> Result<()> {
    match std::os::unix::net::UnixStream::connect("/run/motor-vm-helper.sock") {
        Ok(_) => {
            return Err(Error(
                "helper socket is serving; stop the helper before offline removal".into(),
            ))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
    }
    for unit in ["motor-host.service", "motor-vm-helper.service"] {
        let output = Command::new("/usr/bin/timeout")
            .args([
                "15s",
                "/usr/bin/systemctl",
                "show",
                "-p",
                "LoadState,ActiveState,SubState",
                unit,
            ])
            .env("LC_ALL", "C")
            .output()?;
        let text = String::from_utf8(output.stdout).map_err(|e| Error(e.to_string()))?;
        let value = |key: &str| text.lines().find_map(|line| line.strip_prefix(key));
        let absent = value("LoadState=") == Some("not-found");
        if (!output.status.success() && !absent)
            || value("ActiveState=") != Some("inactive")
            || value("SubState=") != Some("dead")
        {
            return Err(Error(format!(
                "stop {unit} before offline removal (including any auto-restart)"
            )));
        }
    }
    Ok(())
}

fn stopped_tenant(tenant: &str, registry: &Registry) -> Result<()> {
    stopped_units()?;
    let jail = Path::new(super::BUILD_ROOT)
        .join("jail/tenants")
        .join(tenant);
    match std::fs::symlink_metadata(&jail) {
        Ok(_) => {
            return Err(Error(
                "tenant jail still exists; recover/stop it before offline removal".into(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let uid = 400000 + u32::from(registry.tap_index);
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.as_encoded_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let status = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let ids = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .ok_or_else(|| Error("process status has no uid identity".into()))?;
        let ids: Vec<u32> = ids
            .split_whitespace()
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| Error("invalid process uid identity".into()))?;
        if ids.len() != 4 {
            return Err(Error("incomplete process uid identity".into()));
        }
        if ids.contains(&uid) {
            return Err(Error(format!(
                "tenant uid {uid} still has a process; stop it before offline removal"
            )));
        }
    }
    Ok(())
}

fn trusted_state(paths: &removal::Paths) -> Result<()> {
    let homes = paths.root.join("tenants");
    let removed = paths.removed();
    for path in [
        &paths.root,
        &homes,
        &removed,
        &paths.registry,
        &paths.provision,
        &paths.bindings,
    ] {
        for ancestor in path.ancestors() {
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) => fs::trusted(ancestor, &metadata)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub fn remove(tenant: &str, resume_only: bool) -> Result<PathBuf> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error("offline removal requires root".into()));
    }
    crate::tenant::validate_name(tenant)?;
    stopped_units()?;
    let paths = removal::Paths::live();
    trusted_state(&paths)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error(e.to_string()))?
        .as_secs();
    removal::offline_with(
        &paths,
        tenant,
        resume_only,
        now,
        |path| serde_json::from_reader(fs::trusted_file(path)?).map_err(|e| Error(e.to_string())),
        |registry| stopped_tenant(tenant, registry),
        tenant_network::remove,
    )
}
