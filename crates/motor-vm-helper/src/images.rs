//! Identity-only image publication protocol. Manifests are installed by root in
//! the helper's staging directory, never supplied by an unprivileged RPC peer.
use crate::{job, Error, Result};
use serde::{Deserialize, Serialize};

pub const STAGING: &str = "/var/lib/motor-vm/job-image-staging";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Readiness {
    pub job_vms_configured: bool,
    pub workspace_disk_max_bytes: Option<u64>,
    pub published_generations: Vec<String>,
}

pub fn validate_generation(id: &str) -> Result<()> {
    job::validate_id(id)?;
    if matches!(id, "locks" | "pins") {
        return Err(Error("reserved image generation identity".into()));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub kernel: Artifact,
    pub rootfs: Artifact,
    pub toolchain: Artifact,
    pub hello: serde_json::Value,
    pub guest_init: serde_json::Value,
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        for artifact in [&self.kernel, &self.rootfs, &self.toolchain] {
            if artifact.bytes == 0
                || artifact.sha256.len() != 64
                || !artifact
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(Error("invalid pinned image artifact".into()));
            }
        }
        if !self.hello.is_object() || !self.guest_init.is_object() {
            return Err(Error("image runner configuration must be an object".into()));
        }
        if !matches!(
            serde_json::from_value::<motor_protocol::Frame>(self.hello.clone()),
            Ok(motor_protocol::Frame::Hello {
                protocol_version: motor_protocol::PROTOCOL_VERSION,
                ..
            })
        ) {
            return Err(Error("image manifest requires the runner's pinned Hello with type=hello and the supported protocol_version; rebuild the job image".into()));
        }
        Ok(())
    }

    pub fn publication_bytes(&self, accounts: usize) -> Option<u64> {
        self.kernel
            .bytes
            .checked_add(self.rootfs.bytes)?
            .checked_add(
                self.toolchain
                    .bytes
                    .checked_mul(u64::try_from(accounts).ok()?.checked_add(1)?)?,
            )
    }
}

#[derive(Debug)]
pub enum Operation {
    Readiness {
        job_vms_configured: bool,
        workspace_disk_max_bytes: Option<u64>,
    },
    Inspect(String),
    Publish(String),
    Prepare {
        generation: String,
        account: String,
    },
    Remove(String),
}

impl Operation {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Readiness { .. } => Ok(()),
            Self::Inspect(id) | Self::Publish(id) | Self::Remove(id) => validate_generation(id),
            Self::Prepare {
                generation,
                account,
            } => {
                validate_generation(generation)?;
                job::validate_account(account)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_requires_a_typed_compatible_runner_hello() {
        let artifact = Artifact {
            bytes: 4096,
            sha256: "a".repeat(64),
        };
        let mut manifest = Manifest {
            kernel: artifact.clone(),
            rootfs: artifact.clone(),
            toolchain: artifact,
            hello: serde_json::json!({"type":"hello","protocol_version": motor_protocol::PROTOCOL_VERSION}),
            guest_init: serde_json::json!({}),
        };
        manifest.validate().unwrap();
        for hello in [
            serde_json::json!({}),
            serde_json::json!({"type":"hello"}),
            serde_json::json!({"type":"hello","protocol_version":0}),
            serde_json::json!({"type":"heartbeat"}),
        ] {
            manifest.hello = hello;
            assert!(manifest.validate().unwrap_err().0.contains("pinned Hello"));
        }
    }
}
