//! Host-to-tenant control frames. These are not client credentials: the guest
//! accepts them only on its host-CID-only vsock listeners.
use crate::Uuid;
use serde::{Deserialize, Serialize};

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
