//! Job-VM lifecycle boundaries. Storage managers reserve a job's disk, spool and
//! assigned image at admission; preparing an account's image happens at boot.
use crate::state::jobs::Execution;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

pub const SPOOL_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Assignment {
    pub image_generation_id: String,
    /// The assigned image's recorded runner Hello, not host-invented capabilities.
    pub hello: Value,
    /// Host-owned, job-exclusive storage covered by the admission reservation.
    pub spool: PathBuf,
    /// Image-specific guest identity, DNS and runner configuration.
    pub guest_init: Value,
}

pub trait Resources: Send + Sync {
    /// Atomically acquire all storage and generation reservations, or none.
    /// Repeating this after a restart returns the same assignment for the job id.
    /// This must not copy images or recover disks.
    fn acquire(&self, execution: &Execution) -> Result<Assignment>;
    /// Full physical account-image copies belong here, inside the boot deadline.
    /// This may not start a VM or perform disk recovery.
    fn prepare(&self, execution: &Execution) -> Result<()>;
    /// Idempotent durable release, called only after stop confirmation or when
    /// no launch was authorized. It also removes any abandoned spool file.
    fn release(&self, job: &str) -> Result<()>;
}

pub struct Unavailable;
impl Resources for Unavailable {
    fn acquire(&self, _: &Execution) -> Result<Assignment> {
        anyhow::bail!("image generations not available")
    }
    fn prepare(&self, _: &Execution) -> Result<()> {
        anyhow::bail!("image generations not available")
    }
    fn release(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

/// A recorded promise may be narrower than the current runner. True capability
/// flags, platform lists and numeric ceilings cannot be silently downgraded.
pub fn hello_covers(actual: &Value, promised: &Value) -> bool {
    if actual["type"] != "hello" || promised["type"] != "hello" {
        return false;
    }
    let Some(fields) = promised.as_object() else {
        return false;
    };
    fields.iter().all(|(key, value)| match value {
        Value::Bool(true) => actual[key] == true,
        Value::Bool(false) | Value::Null => true,
        Value::Array(values) => actual[key]
            .as_array()
            .is_some_and(|a| values.iter().all(|v| a.contains(v))),
        Value::Number(n) if key == "return_chunked_max_bytes" => {
            actual[key]
                .as_u64()
                .unwrap_or(motor_protocol::RETURN_CHUNKED_MAX_BYTES_CAP)
                >= n.as_u64().unwrap_or(u64::MAX)
        }
        _ => actual[key] == *value,
    })
}
