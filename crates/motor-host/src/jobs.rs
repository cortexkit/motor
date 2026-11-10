//! Job-VM lifecycle boundaries. Storage managers reserve a job's disk, spool and
//! assigned image at admission; preparing an account's image happens at boot.
use crate::state::jobs::Execution;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

pub mod fill_source;
pub mod fills;
pub mod images;

/// The guest agent's `attach_tenant` request for a job or fill VM: the image's
/// guest init plus this VM's network slot and identity. Both kinds go through
/// here because the guest refuses an attach that lacks any identity field.
pub(crate) fn guest_attach_request(
    guest_init: &Value,
    resolvers: &std::sync::RwLock<Vec<String>>,
    vm_id: &str,
    tap: u64,
) -> Result<Value> {
    use anyhow::ensure;
    use serde_json::json;
    let mut request = guest_init.clone();
    let resolvers = resolvers.read().expect("job resolvers").clone();
    ensure!(!resolvers.is_empty(), "job VM resolvers are not configured");
    request["resolvers"] = json!(resolvers);
    request["hostname"] = json!("motor");
    let identity = motor_lease::sha256_hex(format!("motor-job-machine-id-v1\n{vm_id}").as_bytes());
    ensure!(
        identity[..32].bytes().any(|b| b != b'0'),
        "invalid derived job machine identity"
    );
    request["machine_id"] = json!(&identity[..32]);
    request["op"] = json!("attach_tenant");
    request["generation"] = json!(1);
    request["unix_ms"] = json!(crate::server::unix_ms());
    request["seed"] = json!(motor_lease::base64url(&rand::random::<[u8; 32]>()));
    request["address"] = json!(format!("10.200.{tap}.2/30"));
    request["gateway"] = json!(format!("10.200.{tap}.1"));
    // Recovery never belongs to a job or fill boot; only a recovery VM's own
    // request may carry it.
    ensure!(
        request.get("recovery_required").is_none() && request.get("recovery_budget_ms").is_none(),
        "job or fill boot cannot recover disks"
    );
    Ok(request)
}

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
    fn acquire_fill(&self, _fill: &crate::state::jobs::fills::Reservation) -> Result<Assignment> {
        anyhow::bail!("fill images unavailable")
    }
    /// Atomically acquire all storage and generation reservations, or none.
    /// Repeating this after a restart returns the same assignment for the job id.
    /// This must not copy images or recover disks.
    fn acquire(&self, execution: &Execution) -> Result<Assignment>;
    /// Full physical account-image copies belong here, before the boot clock.
    /// Admission reserves their space first; preparation has its own bound.
    /// This may not start a VM or perform disk recovery.
    fn prepare(&self, execution: &Execution) -> Result<()>;
    /// Idempotent durable release, called only after stop confirmation or when
    /// no launch was authorized. It also removes any abandoned spool file.
    fn release(&self, job: &str) -> Result<()>;
    /// Reserve the quarantined disk and return a boot-ready image, including
    /// its full account copy. Recovery has no job token, client connection or
    /// upload spool. Reopening a recovery returns its same image. Preparation
    /// precedes the VM's clock.
    fn acquire_recovery(
        &self,
        _disk_id: &str,
        _disk: &crate::state::jobs::WorkspaceDisk,
        _recovery: &crate::state::jobs::recovery::Recovery,
    ) -> Result<Assignment> {
        anyhow::bail!("recovery images unavailable")
    }
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
    if !matches!(
        serde_json::from_value::<motor_protocol::Frame>(actual.clone()),
        Ok(motor_protocol::Frame::Hello { .. })
    ) {
        return false;
    }
    if promised["return_chunked"] == true {
        let ceiling = |hello: &Value| {
            hello["return_chunked_max_bytes"]
                .as_u64()
                .unwrap_or(motor_protocol::RETURN_CHUNKED_MAX_BYTES_CAP)
        };
        if ceiling(actual) < ceiling(promised) {
            return false;
        }
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
