use crate::{Error, Result};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_RUNNER_ARGS: usize = 16 * 1024;

/// Durable provisioning intent, retained until the guest and host barriers pass.
/// After a successful grow the device and filesystem sizes match, so retries
/// must use the persisted intent to decide whether a post-grow check is needed.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeActivation {
    pub runner_args: String,
    pub quota_grew: bool,
}

impl ResizeActivation {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let activation: Self = serde_json::from_slice(bytes).map_err(|e| Error(e.to_string()))?;
        validate_runner_args(&activation.runner_args)?;
        Ok(activation)
    }
}

// Keep the option grammar aligned with the guest's RunnerArgs::parse. These
// are whitespace-separated arguments, never shell source or executable paths.
fn validate_runner_args(text: &str) -> Result<()> {
    if text.len() > MAX_RUNNER_ARGS {
        return Err(Error("runner_args exceeds 16 KiB".into()));
    }
    let mut words = text.split_whitespace();
    let (mut host, mut tenant, mut network) = (false, false, false);
    let (mut weight, mut slots, mut cache, mut floor) = (None, None, None, None);
    let (mut roots, mut owned) = (Vec::new(), Vec::new());
    while let Some(option) = words.next() {
        if option == "--tenant-vm" {
            tenant = true;
            continue;
        }
        let value = words
            .next()
            .ok_or_else(|| Error("runner-args option lacks value".into()))?;
        match option {
            "--backend" if value == "host" => host = true,
            "--job-network" if value == "veth" => network = true,
            "--max-weight" => {
                weight = Some(value.parse::<u32>().map_err(|e| Error(e.to_string()))?)
            }
            "--rustc-slots" => {
                slots = Some(value.parse::<u32>().map_err(|e| Error(e.to_string()))?)
            }
            "--sccache-size-gib" => {
                cache = Some(value.parse::<u64>().map_err(|e| Error(e.to_string()))?)
            }
            "--free-space-floor-bytes" => {
                floor = Some(value.parse::<u64>().map_err(|e| Error(e.to_string()))?)
            }
            "--workspace-root" => roots.push(std::path::PathBuf::from(value)),
            "--owned-workspace-root" => owned.push(std::path::PathBuf::from(value)),
            "--state-root" if value == "/tenant/state" => {}
            _ => return Err(Error("unsupported runner-args option".into())),
        }
    }
    roots.sort_unstable();
    owned.sort_unstable();
    if !host
        || !tenant
        || !network
        || weight.unwrap_or(0) == 0
        || slots != weight
        || roots
            != [
                std::path::PathBuf::from("/Users"),
                std::path::PathBuf::from("/home"),
            ]
        || owned != roots
        || cache.is_none()
        || floor.is_none()
    {
        return Err(Error(
            "runner-args does not describe a tenant runner".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
