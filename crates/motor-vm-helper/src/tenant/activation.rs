#[cfg(any(target_os = "linux", test))]
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

// Only the Linux helper reads a staged file; other builds keep the portable
// type for their tests.
#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
impl ResizeActivation {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let activation: Self = serde_json::from_slice(bytes).map_err(|e| Error(e.to_string()))?;
        validate_runner_args(&activation.runner_args)?;
        Ok(activation)
    }
}

// Keep the option grammar aligned with the guest's RunnerArgs::parse. These
// are whitespace-separated arguments, never shell source or executable paths.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn validate_runner_args(text: &str) -> Result<()> {
    if text.len() > MAX_RUNNER_ARGS {
        return Err(Error("runner_args exceeds 16 KiB".into()));
    }
    let mut words = text.split_whitespace();
    let (mut host, mut tenant, mut network) = (false, false, false);
    let (mut weight, mut slots, mut cache, mut floor) = (None, None, None, None);
    let (mut roots, mut owned) = (Vec::new(), Vec::new());
    let mut fixed_path_repositories = std::collections::BTreeSet::new();
    let mut account_subject = None;
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
            // Provisioning sets an eviction goal above the floor (a share of
            // the tenant disk); the guest's RunnerArgs::parse accepts it too.
            "--free-space-eviction-bytes" => {
                value.parse::<u64>().map_err(|e| Error(e.to_string()))?;
            }
            // The guest checks the maximum's range; the runner checks budget coverage at startup.
            "--return-chunked-max-bytes" | "--return-budget-bytes" => {
                if value.parse::<u64>().map_err(|e| Error(e.to_string()))? == 0 {
                    return Err(Error("return byte count must be positive".into()));
                }
            }
            "--workspace-root" => roots.push(std::path::PathBuf::from(value)),
            "--owned-workspace-root" => owned.push(std::path::PathBuf::from(value)),
            "--account-subject" => account_subject = Some(value),
            "--fixed-path-repository" => {
                if (value != "*" && !value.starts_with('/'))
                    || value.chars().any(char::is_whitespace)
                    || !fixed_path_repositories.insert(value)
                {
                    return Err(Error("invalid fixed-path repository".into()));
                }
            }
            "--state-root" if value == "/tenant/state" => {}
            // A tenant installed with remote cache files runs with the
            // runner's cache switch on; provisioning never writes `false`.
            "--remote-cache" if value == "true" => {}
            _ => return Err(Error("unsupported runner-args option".into())),
        }
    }
    if !fixed_path_repositories.is_empty() {
        if fixed_path_repositories.contains("*") && fixed_path_repositories.len() != 1 {
            return Err(Error("fixed-path * must be used alone".into()));
        }
        let valid_subject = account_subject
            .and_then(|sub| sub.strip_prefix("acct:").or_else(|| sub.strip_prefix("org:")))
            .is_some_and(|id| {
                id.len() == 26
                    && id.bytes().all(|b| {
                        matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
                    })
            });
        if !valid_subject {
            return Err(Error("fixed paths require a valid account subject".into()));
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
mod fixed_path_tests {
    use super::validate_runner_args;

    const LEGACY: &str = "--backend host --tenant-vm --job-network veth --max-weight 4 --rustc-slots 4 --sccache-size-gib 8 --free-space-floor-bytes 100 --workspace-root /Users --workspace-root /home --owned-workspace-root /Users --owned-workspace-root /home --state-root /tenant/state";
    const SUBJECT: &str = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn fixed_path_arguments_accept_opt_in_and_preserve_legacy() {
        assert!(validate_runner_args(LEGACY).is_ok());
        assert!(validate_runner_args(&format!("{LEGACY} --account-subject ignored")).is_ok());
        for subject in [SUBJECT, "org:01ARZ3NDEKTSV4RRFFQ69G5FAV"] {
            for roots in [
                "--fixed-path-repository *",
                "--fixed-path-repository /repo/a --fixed-path-repository /repo/b",
            ] {
                assert!(validate_runner_args(&format!(
                    "{LEGACY} {roots} --account-subject {subject}"
                ))
                .is_ok());
            }
        }
    }

    #[test]
    fn fixed_path_arguments_reject_malformed_values() {
        for roots in [
            "relative",
            "/repo a",
            "/repo\ta",
            "/repo\u{2003}a",
            "/repo --fixed-path-repository /repo",
            "* --fixed-path-repository /repo",
            "/repo --fixed-path-repository *",
            "* --fixed-path-repository *",
            "",
        ] {
            let args =
                format!("{LEGACY} --account-subject {SUBJECT} --fixed-path-repository {roots}");
            assert!(validate_runner_args(&args).is_err(), "{args}");
        }
        assert!(validate_runner_args(&format!("{LEGACY} --fixed-path-repository /repo")).is_err());
        for subject in [
            "acct:short",
            "user:01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "acct:01arz3ndektsv4rrffq69g5fav",
            "acct:01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "",
        ] {
            let args =
                format!("{LEGACY} --fixed-path-repository /repo --account-subject {subject}");
            assert!(validate_runner_args(&args).is_err(), "{args}");
        }
    }
}

#[cfg(test)]
mod tests;
