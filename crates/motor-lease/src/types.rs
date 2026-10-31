use serde::{Deserialize, Serialize};

pub const ISSUER: &str = "https://runners.cortexkit.io";
pub const JWKS_URL: &str = "https://runners.cortexkit.io/.well-known/jwks.json";
pub const WORKER_AUDIENCE: &str = "cortexkit-runners";
pub const CLOCK_SKEW_S: u64 = 60;
pub const MAX_TOKEN_LIFETIME_S: u64 = 300;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn valid_job_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            14 => b == b'7',
            19 => b"89ab".contains(&b),
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// The runner's per-job kill cap, not an admission RAM reservation.
pub fn memory_max_mib(vcpus: u32) -> u64 {
    (u64::from(vcpus) * 3584).max(16384)
}

/// The runners contract's per-job memory figure, in MiB, for `vcpus`: 3.5 GiB
/// per vCPU, at least 4 GiB and at most 16 GiB. The broker counts it against a
/// first-party host's free memory when it places a job, and the job token
/// states it.
pub fn job_memory_mib(vcpus: u32) -> u64 {
    (u64::from(vcpus) * 3584).clamp(4096, 16384)
}

/// Whether a first-party job token's `mem_mib` is a figure the broker signs for
/// `vcpus`: the current [`job_memory_mib`], or [`memory_max_mib`], which the
/// broker signed before and the runners contract keeps valid until the
/// operator retires it. Hosts and ck-motor apply this same check, or a granted
/// lease is refused at connect.
pub fn first_party_mem_mib_accepted(vcpus: u32, mem_mib: u64) -> bool {
    mem_mib == job_memory_mib(vcpus) || mem_mib == memory_max_mib(vcpus)
}

/// The smallest memory per leased vCPU a Boat token may state. Below 1 GiB per
/// vCPU the token is malformed or names a sandbox in which builds would be
/// killed for memory.
pub const BOAT_MIN_MEM_MIB_PER_VCPU: u64 = 1024;

/// The `mem_mib` values a Boat job token may state for `vcpus`. CLOUD's
/// contract makes a Boat token's `mem_mib` the chosen sandbox size's measured
/// usable memory (13312 or 13444 MiB for the 8-vCPU large size, 14336 MiB in
/// the recorded test size), not [`memory_max_mib`]. That memory may be below
/// the formula, so it is accepted from the per-vCPU floor up to the formula.
/// The requester and the in-sandbox endpoint must apply this same range: a
/// token one side accepts and the other refuses wastes a paid Boat booking.
pub fn boat_memory_range(vcpus: u32) -> std::ops::RangeInclusive<u64> {
    u64::from(vcpus).saturating_mul(BOAT_MIN_MEM_MIB_PER_VCPU)..=memory_max_mib(vcpus)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequesterJwk {
    pub kty: String,
    pub crv: String,
    pub x: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub host: String,
    #[serde(deserialize_with = "required_nullable")]
    pub vm: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Run,
    Attach,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub jkt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub jti: String,
    pub iat: u64,
    pub exp: u64,
    pub vcpus: u32,
    pub mem_mib: u64,
    pub platform: String,
    pub net: bool,
    pub placement: Placement,
    pub max_s: u64,
    pub cnf: Confirmation,
    pub purpose: Purpose,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_workspace_hash"
    )]
    pub workspace_key_hash: Option<String>,
}

// Boat sandbox job tokens omit the hash; registered hosts require it during verification.
// An explicit null is not a hash on either path.
fn present_workspace_hash<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub job_id: String,
    pub vcpus: u32,
    pub platform: String,
    pub net: bool,
    /// Opaque base64url HMAC-SHA256 from the caller's per-machine salt.
    pub workspace_key_hash: String,
    /// Opaque base64url HMAC-SHA256 from the caller's per-machine salt.
    pub repo_hash: String,
    #[serde(deserialize_with = "required_nullable")]
    pub placement_hint: Option<Placement>,
    /// A host the broker must not place this lease on, even when
    /// `placement_hint` names it: null, or a host id in the common name
    /// grammar. Required on the wire, and covered by the lease hash, so every
    /// re-request of a job must repeat the value it was first sent with.
    #[serde(deserialize_with = "required_nullable")]
    pub avoid_host: Option<String>,
    pub timeout_s: u64,
    /// False restricts placement to registered hosts; true lets the broker
    /// spill the lease to Boat. The broker refuses a lease body without it
    /// (400 `bad_request`), so every newly built request must set it. It stays
    /// optional in this type so lease requests that ck-motor stored before it
    /// always sent the member still deserialize; re-sent unchanged, the broker
    /// refuses them.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_boolean"
    )]
    pub allow_spill: Option<bool>,
    pub requester_jwk: RequesterJwk,
}

fn optional_boolean<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReattachRequest {
    pub requester_jwk: RequesterJwk,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReleaseRequest {}

impl<'de> Deserialize<'de> for ReleaseRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let body = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        if !body.is_empty() {
            return Err(serde::de::Error::custom(
                "release body must be an empty object",
            ));
        }
        Ok(Self {})
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReply {
    pub job_id: String,
    pub release_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseResponse {
    pub lease_id: String,
    pub job_id: String,
    pub placement: Placement,
    pub endpoint: String,
    pub endpoint_spki_sha256: String,
    pub job_token: String,
    pub token_expires_at_ms: u64,
    pub max_s: u64,
    pub reserved_vcpu_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    pub nonce: String,
    pub host_id: String,
    pub ts: u64,
}

/// Prelude versions are closed even though additional object fields are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Version1;

impl TryFrom<u8> for Version1 {
    type Error = &'static str;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self)
        } else {
            Err("unsupported lease prelude version")
        }
    }
}
impl From<Version1> for u8 {
    fn from(_: Version1) -> Self {
        1
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Prelude {
    LeaseHello {
        v: Version1,
        token: String,
        requester_jwk: RequesterJwk,
    },
    Challenge(Challenge),
    ChallengeResponse {
        sig: String,
    },
    LeaseOk {},
    LeaseRefused {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Reserved,
    Admitted,
    Started,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunningJob {
    pub job_id: String,
    pub state: JobState,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmVm {
    pub tenant: String,
    pub workspace_key_hash: String,
    pub vm: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub free_vcpus: u64,
    pub free_mem_mib: u64,
    pub queued: u64,
    pub warm: Vec<WarmVm>,
    pub running: Vec<RunningJob>,
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draining: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOutcome {
    Exit,
    Signal,
    Timeout,
    Cancelled,
    Lost,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub job_id: String,
    pub vcpus: u32,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub mem_peak_mib: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub egress_bytes: Option<u64>,
    pub outcome: UsageOutcome,
}

fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageReply {
    pub job_id: String,
    pub settled_vcpu_ms: u64,
    pub released_vcpu_ms: u64,
}
