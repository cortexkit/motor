//! Host-to-tenant control frames. These are not client credentials: the guest
//! accepts them only on its host-CID-only vsock listeners.
use crate::Uuid;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long the guest's tenant agent waits, after starting the tenant runner,
/// for that runner to listen on its socket. Attach fails when this expires.
pub const TENANT_RUNNER_READINESS_WAIT: Duration = Duration::from_secs(30);

/// Room for the rest of a fresh attach around the readiness wait: setting the
/// clock and network, mounting and laying out the tenant disk, binding tool
/// directories, the reflink probe, and writing the reply.
pub const TENANT_ATTACH_SETUP_MARGIN: Duration = Duration::from_secs(30);

/// How long the host waits for the guest's reply to a fresh attach. It must
/// outlast the guest's own readiness wait, so that a runner that never becomes
/// ready is reported by the guest's error instead of a host read timeout.
pub const TENANT_ATTACH_READ_DEADLINE: Duration = Duration::from_secs(
    TENANT_RUNNER_READINESS_WAIT.as_secs() + TENANT_ATTACH_SETUP_MARGIN.as_secs(),
);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LeasePurpose {
    Run,
    Attach,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifiedLease {
    pub jti: Uuid,
    pub purpose: LeasePurpose,
    pub sub: String,
    pub vcpus: u32,
    pub mem_mib: u64,
    pub platform: String,
    pub net: bool,
    pub max_s: u64,
    pub exp: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TenantEventKind {
    Admitted,
    Refused,
    Started,
    Terminal,
    Idle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefusedCause {
    Mismatch,
    Rejected,
    Disconnect,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantEvent {
    pub event_seq: u64,
    pub event: TenantEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RefusedCause>,
}

/// The event sequence allocated before a lifecycle connection began. Events at
/// or below this watermark are replay, even if they arrive much later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantReplayBoundary {
    pub through_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TenantAgentRequest {
    AttachTenant {
        generation: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        recovery_required: bool,
        unix_ms: u64,
        seed: String,
        address: String,
        gateway: String,
        resolvers: Vec<String>,
        hostname: String,
        machine_id: String,
    },
    PostRestore {
        unix_ms: u64,
        seed: String,
    },
    Cancel {
        job_id: Uuid,
    },
    Drain {
        deadline_s: u64,
        exit_after: bool,
    },
    Resume,
    Status,
    ArgsStatus,
    PrepareSnapshot {
        kind: SnapshotKind,
    },
    Thaw,
    RecoveryStatus {
        generation: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    Base,
    Suspend,
}
