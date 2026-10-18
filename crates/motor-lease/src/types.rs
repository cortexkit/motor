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
    pub timeout_s: u64,
    pub requester_jwk: RequesterJwk,
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
