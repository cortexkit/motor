//! Broker credentials and saved routes are independent of the build-server SSH
//! connector. Hosted jobs keep their route even after hosted routing is disabled.
pub(crate) mod account;
pub(crate) mod check;
pub(crate) mod fence_probe;
mod finish;
pub(crate) mod placement;
mod ssh;
#[cfg(test)]
pub(crate) mod test_http;
mod tls;

use crate::{config::BrokerTokenSource, storage::Storage, transport::Channel, Config};
use ed25519_dalek::SigningKey;
use hmac::{Hmac, Mac};
use motor_lease::{LeaseRequest, Placement, ReattachRequest, ReleaseRequest, RequesterJwk};
use motor_protocol::{HostedTerminal, RefusalReason, RunRequest, TerminalRecord, Uuid};
use placement::Lease;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    io,
    path::{Component, Path},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) fn runner_refusal_code(reason: &RefusalReason, detail: Option<&str>) -> String {
    // Tenant admission rejects a Run whose resource claims disagree with its
    // lease as RequestRejected without detail. Other refusals describe their
    // own failure; a workspace or capacity error is not a claims mismatch.
    if *reason == RefusalReason::RequestRejected && detail.is_none() {
        "lease_mismatch".into()
    } else {
        String::from(reason.clone())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingStatus {
    #[default]
    Pending,
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedJobState {
    /// Informational placement latency only. Recovery must not depend on it:
    /// an older client can drop this field when rewriting the store on rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) placement_wait_ms: Option<u64>,
    pub hosted_no_fallback: bool,
    pub hosted_code: Option<String>,
    pub placement: Option<Placement>,
    pub endpoint: Option<String>,
    pub endpoint_spki_sha256: Option<String>,
    pub max_runtime_s: Option<u64>,
    pub billed_vcpu_ms: Option<u64>,
    pub billing: BillingStatus,
    pub terminal: Option<TerminalRecord>,
    /// Exclusive end of the stream preceding the hosted terminal. Zero proves
    /// an empty stream; older stores and summary-only observations are unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) replay_end_seq: Option<u64>,
    /// Set when this process read an OutcomeUnknown terminal from the host or
    /// a status reply. The stored observation timestamp can't show that: older
    /// clients also stamp it on unknown summaries they made up themselves.
    /// Not persisted, so after a restart an unknown terminal counts as unproven
    /// until it is read again.
    #[serde(skip)]
    pub(crate) observed_unknown_terminal: bool,
    #[serde(default)]
    pub(crate) pending_finish: Option<placement::Finish>,
    #[serde(default)]
    pub(crate) finish_done: bool,
    #[serde(default)]
    pub spill_wait_ms: Option<u64>,
    #[serde(default)]
    pub(crate) lease_attempted: bool,
    #[serde(default)]
    pub(crate) placement_attempts: u8,
    #[serde(default)]
    pub(crate) retry_iat: Option<u64>,
    #[serde(default, deserialize_with = "stored_lease_request")]
    pub(crate) lease_request: Option<LeaseRequest>,
    /// The host the broker must not place this job's lease on. Only a spill
    /// sets it, to the build server's host id; it is fixed when the route is
    /// created and copied into the stored lease body, so every re-request of
    /// the job repeats it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) avoid_host: Option<String>,
    /// The job was sent straight to a hosted lease by a direct caller's
    /// `ck motor run --hosted`, to test the hosted path. Its lease is not
    /// retried for capacity and the job never falls back to the build server.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) hosted_test: bool,
    /// Selected by hosted_repositories, rather than a spill or manual test.
    /// Persist the decision so retries and restarts cannot enable local fallback.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) repository_routed: bool,
    /// The configured placement for a repository-routed job. Save it before
    /// leasing so a restart or config change cannot unpin the job or enable
    /// the no_host_fits correction to the build server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repository_placement_hint: Option<Placement>,
    /// Selected by the global fleet route. Persist alongside repository routing
    /// so capacity waits and pre-Run correction survive configuration changes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) fleet_routed: bool,
    /// Gate and outbound-network fleet jobs cannot run on Boat. Store the restriction
    /// before requesting a lease so retries and restarts keep the same policy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) fleet_no_spill: bool,
    /// Hosts refused before Run for which no further replacement is attempted.
    /// Pending revocation fences do not count as refusals.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub(crate) replacement_hosts: std::collections::BTreeSet<String>,
    /// Set durably just before ck-motor sends the Run frame to a hosted
    /// runner. Until then no runner holds the command, so whatever happened to
    /// the lease, the job provably never ran; afterwards it may have.
    #[serde(default)]
    pub(crate) run_sent: bool,
    /// Preserve the first runner timestamp across detach and restart so a
    /// synthesized terminal can report elapsed time without guessing from Run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at_ms: Option<u64>,
    pub(crate) broker_url: String,
    pub(crate) token_source: BrokerTokenSource,
}
/// Reads a stored lease body. Bodies stored before the broker required
/// `avoid_host` lack the key; they are read as avoiding no host, so an older
/// job record still loads instead of making the whole store unreadable.
fn stored_lease_request<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<LeaseRequest>, D::Error> {
    let Some(mut body) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if let Some(fields) = body.as_object_mut() {
        fields
            .entry("avoid_host")
            .or_insert(serde_json::Value::Null);
    }
    serde_json::from_value(body)
        .map(Some)
        .map_err(serde::de::Error::custom)
}
impl HostedJobState {
    /// Only received runner terminals can use the replay cursor as evidence.
    /// Locally synthesized summaries do not prove what the runner emitted.
    pub(crate) fn received_terminal(&self, terminal: TerminalRecord) -> TerminalRecord {
        if terminal.last_seq().is_none()
            && terminal.ran != Some(motor_protocol::Ran::None)
            && !matches!(
                terminal.outcome,
                motor_protocol::Outcome::RefusedBeforeStart { .. }
                    | motor_protocol::Outcome::HistoryExpired
            )
        {
            if let Some(last_seq) = self.replay_end_seq.and_then(|end| end.checked_sub(1)) {
                return terminal.with_last_seq(last_seq);
            }
        }
        terminal
    }

    pub(crate) fn needs_output_replay(&self, cursor: u64) -> bool {
        cursor != u64::MAX
            && !(self.terminal.is_some() && !self.has_real_terminal())
            && self.replay_end_seq.is_none_or(|end| cursor < end)
    }
    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.started_at_ms.map_or(0, |started| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            now.saturating_sub(started)
        })
    }
    /// The runner and broker use the current booking's ID. The caller keeps
    /// its original ID even when a refused Boat booking needed a fresh lease.
    pub(crate) fn runner_id(&self, caller: Uuid) -> Uuid {
        self.lease_request
            .as_ref()
            .and_then(|request| Uuid::parse_str(&request.job_id).ok())
            .unwrap_or(caller)
    }
    pub(crate) fn awaiting_cancel_terminal(&self) -> bool {
        self.terminal
            .as_ref()
            .is_some_and(|t| t.refusal_detail.as_deref() == Some(CANCEL_ACKNOWLEDGED))
    }
    pub(crate) fn cancel_settlement_ready(&self) -> bool {
        self.awaiting_cancel_terminal()
            || self
                .pending_finish
                .as_ref()
                .is_some_and(|finish| finish.outcome == "cancelled")
    }
    pub(crate) fn recovering_recorded_unknown(&self) -> bool {
        !self.cancel_settlement_ready()
            && self.terminal.as_ref().is_some_and(|terminal| {
                matches!(terminal.outcome, motor_protocol::Outcome::OutcomeUnknown)
                    && !self.observed_unknown_terminal
            })
    }
    pub(crate) fn has_real_terminal(&self) -> bool {
        self.terminal.as_ref().is_some_and(|terminal| {
            !matches!(terminal.outcome, motor_protocol::Outcome::OutcomeUnknown)
                || self.observed_unknown_terminal
        })
    }
    pub(crate) fn booked_or_attempted(&self) -> bool {
        self.lease_attempted
            || self.placement.is_some()
            || self.placement_attempts > 0
            || self.retry_iat.is_some()
            || self.run_sent
            || self.pending_finish.is_some()
    }

    pub(crate) fn host_deadline_s(&self) -> Option<u64> {
        if !self.run_sent || !self.placement.as_ref()?.host.starts_with("boat:") {
            return None;
        }
        self.reattach_deadline_s()
    }

    pub(crate) fn reattach_deadline_s(&self) -> Option<u64> {
        if !self.run_sent {
            return None;
        }
        // Run is recorded only after its token's issue time is durable.
        // Attach refreshes neither that time nor the original request limit.
        // The requested limit is conservative if the broker granted less time.
        Some(
            self.retry_iat?
                .saturating_add(
                    self.lease_request
                        .as_ref()
                        .map(|r| r.timeout_s)
                        .or(self.max_runtime_s)?,
                )
                .saturating_add(HOST_LOST_GRACE_S),
        )
    }

    pub(crate) fn needs_cleanup(&self) -> bool {
        self.booked_or_attempted() && !self.settlement_done()
    }

    pub(crate) fn settlement_done(&self) -> bool {
        self.pending_finish.is_none()
            && if self.run_sent {
                self.terminal.is_some()
                    && self.billing != BillingStatus::Pending
                    && (self.finish_done
                        || self
                            .placement
                            .as_ref()
                            .is_none_or(|p| !p.host.starts_with("boat:")))
            } else {
                self.finish_done
            }
    }
    pub(crate) fn capacity_routed(&self) -> bool {
        self.repository_routed || self.fleet_routed
    }
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            broker_url: config
                .broker_url
                .clone()
                .unwrap_or_else(|| motor_lease::ISSUER.into()),
            token_source: config.broker_token_source,
            ..Self::default()
        }
    }
    pub(crate) fn lease(&mut self, lease: &Lease) {
        self.placement = Some(lease.placement.clone());
        self.endpoint = Some(
            lease
                .ssh
                .as_ref()
                .map_or_else(|| lease.endpoint.clone(), |ssh| ssh.endpoint.clone()),
        );
        self.endpoint_spki_sha256 = Some(lease.endpoint_spki_sha256.clone());
        self.max_runtime_s = Some(lease.max_s);
    }
    pub(crate) fn settled(&mut self, terminal: HostedTerminal) {
        self.billed_vcpu_ms = Some(terminal.billed_vcpu_ms);
        self.max_runtime_s = Some(terminal.max_runtime_s);
        self.billing = BillingStatus::Available;
    }
    pub fn billed_line(&self) -> String {
        match self.billed_vcpu_ms {
            Some(ms) => format!("billed: {}.{:03} vCPU-s", ms / 1000, ms % 1000),
            None => format!(
                "billed: {}",
                if self.billing == BillingStatus::Unavailable {
                    "unavailable"
                } else {
                    "pending"
                }
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Shutdown,
    Cancelled,
    Auth,
    /// An attach token was issued, but the host explicitly refused it as auth.
    AttachAuth,
    Expired,
    Spki,
    SshKey,
    Provisioning(u64),
    ReplacePending(u64),
    SandboxLost,
    BrokerRefused,
    HostFull,
    BoatHostFull,
    Capacity,
    NoCapacity(u64),
    /// No registered host can fit the job; valid broker limits are retained.
    NoHostFits(Option<u64>, Option<u64>),
    /// The broker refused a new lease with 403 `account_closed`: the hosted
    /// account was closed for good, so no job of it may run anywhere. Final:
    /// never retried, never sent back to the build server, never run locally.
    AccountClosed,
    InternalFailure,
    /// The host refused the lease because its disk is full. It recorded
    /// nothing and no runner saw the job, so this attempt never started it
    /// and the job may be placed elsewhere.
    StorageFull,
    Connect,
    Broker,
    /// A reattach returned 404 job_unknown, which can also mean provisioning.
    ReattachUnknown,
    NoTimeout,
    Short,
    Path,
    Admitted,
    Finished,
    Rejected,
    Released,
    NotGranted,
    Budget,
    Platform,
    AccountNotFound,
    AccountAuthRequired,
    AccountTransient(Option<u64>),
    AccountPermanent,
    AccountInvalid,
    /// The lease body breaks CLOUD's lease schema, so the broker would refuse
    /// it, or did, with 400 `bad_request`. Carries the offending body member
    /// when ck-motor found it before sending or the broker named a safe field
    /// in its refusal. Nothing was booked either way.
    RequestInvalid(Option<RequestField>),
}

/// A broker-supplied member name safe to put in logs and refusal details.
/// Inline storage keeps lease errors copyable without retaining reply bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestField {
    bytes: [u8; 64],
    len: u8,
}
impl RequestField {
    fn new(field: &str) -> Option<Self> {
        if field.len() > 64 || !field.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            return None;
        }
        let mut bytes = [0; 64];
        bytes[..field.len()].copy_from_slice(field.as_bytes());
        Some(Self {
            bytes,
            len: field.len() as u8,
        })
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("validated ASCII field")
    }
}
/// A spill has already surrendered its build-server queue slot. Healthy Boat
/// starts take about 110 seconds; allow some headroom, but return a stuck start
/// to the build server instead of spending the entire remaining queue budget.
const SPILL_PROVISIONING_BUDGET: Duration = Duration::from_secs(180);
const MIN_PROVISIONING_RETRY_MS: u64 = 1_000;
const MAX_PROVISIONING_RETRY_MS: u64 = 30_000;
/// How long a repository-routed job waits before asking again after the
/// granted host refused it as full. The host sends no Retry-After, and its
/// slots free as soon as a running job ends, so this is shorter than the
/// broker's usual 5-second `no_capacity` wait.
const HOST_FULL_RETRY_MS: u64 = 2_000;
pub(crate) const HOST_LOST_GRACE_S: u64 = 600;
pub(crate) const CANCEL_ACKNOWLEDGED: &str = "hosted_cancel_acknowledged: awaiting runner terminal";
fn bounded_provisioning_retry_ms(ms: u64) -> u64 {
    ms.clamp(MIN_PROVISIONING_RETRY_MS, MAX_PROVISIONING_RETRY_MS)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisioningReply {
    status: String,
    retry_after_ms: u64,
}
impl Error {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Shutdown => "hosted_shutdown",
            Self::Cancelled => "hosted_cancelled",
            Self::Auth | Self::AttachAuth => "hosted_auth",
            Self::Expired => "hosted_expired",
            Self::Spki => "hosted_spki_mismatch",
            Self::SshKey => "hosted_ssh_host_key_mismatch",
            Self::Provisioning(_) => "hosted_provisioning",
            Self::ReplacePending(_) => "hosted_replace_pending",
            Self::SandboxLost => "hosted_sandbox_lost",
            Self::BrokerRefused => "hosted_broker_refused",
            Self::HostFull | Self::BoatHostFull => "hosted_host_full",
            Self::Capacity | Self::NoCapacity(_) => "hosted_capacity",
            Self::NoHostFits(_, _) => "hosted_no_host_fits",
            Self::AccountClosed => "hosted_account_closed",
            Self::InternalFailure => "hosted_internal_failure",
            Self::StorageFull => "hosted_storage_full",
            Self::Connect => "hosted_connect_failed",
            Self::Broker | Self::ReattachUnknown => "hosted_broker_unreachable",
            Self::NoTimeout => "hosted_no_timeout",
            Self::Short => "hosted_max_s_too_short",
            Self::Path => "hosted_path_outside_roots",
            Self::Admitted => "hosted_job_admitted",
            Self::Finished => "hosted_job_finished",
            Self::Rejected => "lease_mismatch",
            Self::Released => "hosted_job_released",
            Self::NotGranted => "hosted_not_granted",
            Self::Budget => "hosted_budget_exhausted",
            Self::Platform => "hosted_platform_unsupported",
            Self::AccountNotFound => "hosted_account_not_found",
            Self::AccountAuthRequired => "hosted_account_auth_required",
            Self::AccountTransient(_) => "hosted_account_transient",
            Self::AccountPermanent => "hosted_account_permanent",
            Self::AccountInvalid => "hosted_account_invalid",
            Self::RequestInvalid(_) => "hosted_request_invalid",
        }
    }
    pub(crate) fn reason(self) -> RefusalReason {
        match self {
            Self::Auth
            | Self::AttachAuth
            | Self::Expired
            | Self::Spki
            | Self::SshKey
            | Self::AccountNotFound
            | Self::AccountAuthRequired
            | Self::AccountPermanent
            | Self::AccountInvalid => RefusalReason::RunnerAuthenticationFailed,
            Self::HostFull
            | Self::BoatHostFull
            | Self::Capacity
            | Self::NoCapacity(_)
            | Self::StorageFull => RefusalReason::RunnerFull,
            Self::Connect
            | Self::Broker
            | Self::ReattachUnknown
            | Self::AccountTransient(_)
            | Self::ReplacePending(_)
            | Self::Provisioning(_) => RefusalReason::Unreachable,
            Self::Platform => RefusalReason::PlatformUnsupported,
            // RequestRejected tells the caller to avoid the request rather
            // than repeat it, which is what a closed account needs.
            Self::AccountClosed => RefusalReason::RequestRejected,
            _ => RefusalReason::RequestRejected,
        }
    }
    pub(crate) fn detail(self) -> String {
        if self == Self::AccountAuthRequired {
            format!("{}: a person must run `ck account login`", self.code())
        } else if self == Self::AccountClosed {
            format!("{}: hosted account closed", self.code())
        } else if self == Self::BoatHostFull {
            format!("{}; spill_attempted={}", self.code(), self.code())
        } else if let Self::NoHostFits(vcpus, memory_mib) = self {
            let capacity = match (vcpus, memory_mib) {
                (Some(vcpus), Some(memory)) => {
                    format!("{vcpus} vCPUs and {} GiB memory", memory / 1024)
                }
                (Some(vcpus), None) => format!("{vcpus} vCPUs"),
                (None, Some(memory)) => format!("{} GiB memory", memory / 1024),
                (None, None) => return self.code().into(),
            };
            format!(
                "{}: the largest job a server can hold now is {capacity}",
                self.code()
            )
        } else if let Some(field) = self.request_field() {
            format!("{}: {field}", self.code())
        } else {
            self.code().into()
        }
    }
    fn request_field(&self) -> Option<&str> {
        match self {
            Self::RequestInvalid(Some(field)) => Some(field.as_str()),
            _ => None,
        }
    }
    /// [`Self::detail`], followed by the broker's own error code when it
    /// sent one, as `hosted_capacity: broker no_capacity`.
    pub(crate) fn detail_with_broker_code(self, broker_code: Option<&str>) -> String {
        match broker_code {
            Some(code) => format!("{}: broker {code}", self.detail()),
            None => self.detail(),
        }
    }
    pub(crate) fn retry_after_ms(self) -> Option<u64> {
        match self {
            Self::AccountTransient(delay) => delay,
            Self::Provisioning(delay) | Self::NoCapacity(delay) | Self::ReplacePending(delay) => {
                Some(delay)
            }
            _ => None,
        }
    }
    fn replaceable(self) -> bool {
        matches!(
            self,
            Self::HostFull | Self::Capacity | Self::StorageFull | Self::Connect
        )
    }
    fn from_host(reason: &str) -> Self {
        match reason {
            "host_full" => Self::HostFull,
            "capacity" => Self::Capacity,
            "internal_failure" => Self::InternalFailure,
            "storage_full" => Self::StorageFull,
            "expired" => Self::Expired,
            _ => Self::Auth,
        }
    }
}

pub(crate) fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
/// Linux jobs use the x86-64 hosted toolchain independently of the caller's CPU.
/// Windows and other wire platforms must not acquire a hosted lease.
fn lease_platform(platform: Option<&motor_protocol::Platform>) -> Result<&'static str, Error> {
    match platform {
        None | Some(motor_protocol::Platform::Linux) => Ok("linux-x64"),
        _ => Err(Error::Rejected),
    }
}
/// CLOUD's inclusive bounds on a lease body's `timeout_s` (seconds) and
/// `vcpus`. A body outside them is refused with 400 `bad_request`.
const LEASE_TIMEOUT_S: std::ops::RangeInclusive<u64> = 10..=21600;
pub(crate) const BOAT_MAX_TIMEOUT_S: u64 = 7200;
const LEASE_VCPUS: std::ops::RangeInclusive<u32> = 1..=64;
/// The vCPU count a lease for `request` asks for.
fn lease_vcpus(request: &RunRequest) -> u32 {
    request.weight_hint.unwrap_or(2)
}
pub(crate) fn validate(request: &RunRequest) -> Result<(), Error> {
    if request.timeout.is_none_or(|s| s == 0) {
        return Err(Error::NoTimeout);
    }
    // Fields of the lease body that come straight from the request. A job
    // whose lease the broker would refuse never spills, and a job
    // routed to hosted is refused naming the field instead of sending it.
    if !request
        .timeout
        .is_some_and(|s| LEASE_TIMEOUT_S.contains(&s))
    {
        return Err(Error::RequestInvalid(RequestField::new("timeout_s")));
    }
    if !LEASE_VCPUS.contains(&lease_vcpus(request)) {
        return Err(Error::RequestInvalid(RequestField::new("vcpus")));
    }
    if request
        .memory_mib
        .is_some_and(|mib| mib == 0 || mib > motor_protocol::MEMORY_MAX_MIB || mib % 1024 != 0)
    {
        return Err(Error::RequestInvalid(RequestField::new("memory_mib")));
    }
    let paths = [
        &request.workspace_key,
        &request.repository_root,
        &request.cwd,
    ]
    .into_iter()
    .chain(request.siblings.iter());
    for path in paths {
        let p = Path::new(path);
        if !allowed_path(p) || std::fs::canonicalize(p).is_ok_and(|p| !allowed_path(&p)) {
            return Err(Error::Path);
        }
    }
    Ok(())
}
pub(crate) fn validation_detail(request: &RunRequest, error: Error) -> String {
    let remedy = match error {
        Error::NoTimeout => "timeout_s: set a timeout in 10..=21600 seconds".into(),
        Error::RequestInvalid(Some(field)) if field.as_str() == "timeout_s" => {
            "set timeout_s in 10..=21600 seconds".into()
        }
        Error::RequestInvalid(Some(field)) if field.as_str() == "vcpus" => {
            format!(
                "set vcpus in {}..={}",
                LEASE_VCPUS.start(),
                LEASE_VCPUS.end()
            )
        }
        Error::RequestInvalid(Some(field)) if field.as_str() == "memory_mib" => format!(
            "memory_mib={}: set whole GiB (a multiple of 1024 MiB), at most 262144 MiB",
            request.memory_mib.unwrap_or_default()
        ),
        Error::Path => {
            let invalid = [
                ("workspace_key", &request.workspace_key),
                ("repository_root", &request.repository_root),
                ("cwd", &request.cwd),
            ]
            .into_iter()
            .chain(request.siblings.iter().map(|path| ("sibling", path)))
            .find(|(_, path)| {
                let path = Path::new(path);
                !allowed_path(path)
                    || std::fs::canonicalize(path).is_ok_and(|path| !allowed_path(&path))
            });
            let remedy =
                "use a path under /Users or /home, including its symlink target, without '..'";
            match invalid {
                Some((field, path)) => format!("{field}={path:?}: {remedy}"),
                None => remedy.into(),
            }
        }
        _ => return error.detail(),
    };
    format!("{}; {remedy}", error.detail())
}
fn allowed_path(path: &Path) -> bool {
    (path.starts_with("/Users") || path.starts_with("/home"))
        && !path.components().any(|c| matches!(c, Component::ParentDir))
}
pub(crate) fn keyed_hash(salt: &[u8; 32], domain: &str, path: &str) -> io::Result<String> {
    let canonical = std::fs::canonicalize(path)?;
    let canonical = canonical
        .to_str()
        .ok_or_else(|| io::Error::other("path is not UTF-8"))?;
    Ok(keyed_hash_input(salt, &format!("{domain}\n{canonical}")))
}
fn keyed_hash_input(salt: &[u8; 32], input: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(salt).expect("HMAC accepts a 32-byte key");
    mac.update(input.as_bytes());
    motor_lease::base64url(&mac.finalize().into_bytes())
}

fn spill_wait_budget_ms(job: &crate::storage::Job) -> u64 {
    // Local permit waiting has already spent part of the job's queue-wait allowance.
    let waited_ms = job
        .hosted
        .as_ref()
        .and_then(|state| state.spill_wait_ms)
        .unwrap_or(0);
    let remaining = job
        .queue_wait_limit_s
        .saturating_mul(1000)
        .saturating_sub(waited_ms);
    if job
        .hosted
        .as_ref()
        .is_some_and(|state| state.spill_wait_ms.is_some())
    {
        remaining.min(SPILL_PROVISIONING_BUDGET.as_millis() as u64)
    } else {
        remaining
    }
}
/// The first leased resource whose token claim differs from what ck-motor
/// requested, as (field, expected, actual). A first-party host's token states
/// the broker-raised statement when memory was specified. Otherwise its memory
/// must pass [`motor_lease::first_party_mem_mib_accepted`]; the host also checks
/// its own tenant's ceiling. A Boat token states
/// the whole sandbox's usable memory, which must lie in
/// [`motor_lease::boat_memory_range`]. Each check is the one the host or
/// in-sandbox endpoint also applies.
fn resource_mismatch(
    claims: &motor_lease::Claims,
    body: &LeaseRequest,
    boat: bool,
) -> Option<(&'static str, String, String)> {
    let (memory_ok, expected) = if boat {
        let range = motor_lease::boat_memory_range(body.vcpus);
        (
            range.contains(&claims.mem_mib)
                && body
                    .mem_mib
                    .is_none_or(|mib| claims.mem_mib >= mib.max(2048 * u64::from(body.vcpus))),
            format!("{}..={}", range.start(), range.end()),
        )
    } else if let Some(stated) = body.mem_mib {
        let effective = stated.max(2048 * u64::from(body.vcpus));
        (claims.mem_mib == effective, effective.to_string())
    } else {
        (
            motor_lease::first_party_mem_mib_accepted(body.vcpus, claims.mem_mib),
            format!(
                "whole GiB >= {} MiB, or retained {} or {} MiB",
                2048 * u64::from(body.vcpus),
                motor_lease::job_memory_mib(body.vcpus),
                motor_lease::memory_max_mib(body.vcpus)
            ),
        )
    };
    if claims.vcpus != body.vcpus {
        Some(("vcpus", body.vcpus.to_string(), claims.vcpus.to_string()))
    } else if !memory_ok {
        Some(("mem_mib", expected, claims.mem_mib.to_string()))
    } else if claims.platform != body.platform {
        Some(("platform", body.platform.clone(), claims.platform.clone()))
    } else if claims.net != body.net {
        Some(("net", body.net.to_string(), claims.net.to_string()))
    } else if claims.max_s > body.timeout_s {
        Some((
            "max_s",
            format!("<={}", body.timeout_s),
            claims.max_s.to_string(),
        ))
    } else {
        None
    }
}
/// Whether CLOUD accepts `placement` as a lease's `placement_hint`: a host in
/// the name grammar, optionally with the `boat:` prefix of a Boat placement,
/// and a vm in the name grammar (never null).
pub(crate) fn valid_hint(placement: &Placement) -> bool {
    let host = placement
        .host
        .strip_prefix("boat:")
        .unwrap_or(&placement.host);
    motor_lease::valid_name(host) && placement.vm.as_deref().is_some_and(motor_lease::valid_name)
}
/// The first member of a lease body that CLOUD's lease parser refuses with
/// 400 `bad_request`, or `None` when it accepts the body. It mirrors the
/// rules of the pinned runners contract (POST /v1/leases and the common wire
/// grammar). Every lease request (spill, repository-routed and direct hosted)
/// is checked here before it is sent.
pub(crate) fn invalid_lease_field(body: &LeaseRequest) -> Option<&'static str> {
    let base64url_43 = |value: &str| {
        value.len() == 43
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    // An unpadded base64url encoding of 32 bytes is 43 characters whose last
    // one carries only 2 data bits, so it is one of these 16.
    let jwk_x = |x: &str| base64url_43(x) && x.ends_with(|c| "AEIMQUYcgkosw048".contains(c));
    let jwk = &body.requester_jwk;
    if !motor_lease::valid_job_id(&body.job_id) {
        Some("job_id")
    } else if !LEASE_VCPUS.contains(&body.vcpus) {
        Some("vcpus")
    } else if !motor_lease::valid_name(&body.platform) {
        Some("platform")
    } else if !base64url_43(&body.workspace_key_hash) {
        Some("workspace_key_hash")
    } else if !base64url_43(&body.repo_hash) {
        Some("repo_hash")
    } else if body.placement_hint.as_ref().is_some_and(|p| !valid_hint(p)) {
        Some("placement_hint")
    } else if body
        .avoid_host
        .as_deref()
        .is_some_and(|host| !motor_lease::valid_name(host))
    {
        Some("avoid_host")
    } else if !LEASE_TIMEOUT_S.contains(&body.timeout_s) {
        Some("timeout_s")
    } else if jwk.kty != "OKP" || jwk.crv != "Ed25519" || !jwk_x(&jwk.x) {
        Some("requester_jwk")
    } else if body.allow_spill.is_none() {
        Some("allow_spill")
    } else if body
        .mem_mib
        .is_some_and(|mib| mib == 0 || mib > 9_007_199_254_740_991 || mib % 1024 != 0)
    {
        Some("mem_mib")
    } else {
        None
    }
}
/// Logs a lease body by its non-secret members, as a warning: either
/// ck-motor found it invalid and did not send it (`http_status` is `None`,
/// `field` names the member), or the broker refused it with a 4xx status.
/// The requester key and the keyed path hashes are never logged. `route`
/// says why the job wanted a hosted lease (spill, repository_routed,
/// hosted_test or hosted).
fn warn_lease_body(
    id: Uuid,
    body: &LeaseRequest,
    route: &'static str,
    http_status: Option<u16>,
    broker_code: Option<&str>,
    field: Option<&str>,
) {
    let placement_hint = body.placement_hint.as_ref().map_or_else(
        || "none".to_owned(),
        |p| format!("{}/{}", p.host, p.vm.as_deref().unwrap_or("none")),
    );
    let allow_spill = body
        .allow_spill
        .map_or("missing", |b| if b { "true" } else { "false" });
    let avoid_host = body.avoid_host.as_deref().unwrap_or("none");
    match http_status {
        None => tracing::warn!(job_id = %id, route, field = field.unwrap_or("unknown"),
            vcpus = body.vcpus, timeout_s = body.timeout_s, platform = %body.platform,
            net = body.net, allow_spill, placement_hint = %placement_hint, avoid_host,
            "hosted lease body invalid; not sent"),
        Some(http_status) => tracing::warn!(job_id = %id, route, http_status,
            broker_code = broker_code.unwrap_or("none"),
            field = field.unwrap_or("unknown"),
            vcpus = body.vcpus, timeout_s = body.timeout_s, platform = %body.platform,
            net = body.net, allow_spill, placement_hint = %placement_hint, avoid_host,
            "hosted lease body refused by broker"),
    }
}
fn lease_body(
    id: Uuid,
    request: &RunRequest,
    hashes: (String, String),
    hint: Option<Placement>,
    avoid_host: Option<String>,
    requester_jwk: RequesterJwk,
    allow_spill: Option<bool>,
) -> Result<LeaseRequest, Error> {
    Ok(LeaseRequest {
        job_id: id.to_string(),
        vcpus: lease_vcpus(request),
        platform: lease_platform(request.platform.as_ref())?.into(),
        net: request.network.is_some(),
        workspace_key_hash: hashes.0,
        repo_hash: hashes.1,
        // Configured hints can be binding. Refuse an invalid pin during
        // validation rather than silently placing the job somewhere else.
        placement_hint: hint,
        avoid_host,
        timeout_s: request.timeout.ok_or(Error::NoTimeout)?,
        requester_jwk,
        allow_spill,
        mem_mib: request.memory_mib,
    })
}
fn reattach_body(key: &SigningKey) -> ReattachRequest {
    ReattachRequest {
        requester_jwk: RequesterJwk::from_key(&key.verifying_key()),
    }
}

/// Observations from one queue wait, not lease identity. A replacement fence
/// says nothing about why placement is waiting; keep the last definite answer
/// so its deadline cannot turn a full host into an unreachable host.
#[derive(Default)]
struct WaitAnswers {
    last: Option<Error>,
    last_response: Option<Error>,
    mixed: bool,
    expired: bool,
}
impl WaitAnswers {
    fn observe(&mut self, error: Error) {
        let answer = match error {
            Error::HostFull | Error::Capacity => Error::NoCapacity(HOST_FULL_RETRY_MS),
            // A fence is not a new placement or transport answer.
            Error::ReplacePending(_) => {
                self.last_response = Some(error);
                return;
            }
            answer @ (Error::NoCapacity(_)
            | Error::Connect
            | Error::Broker
            | Error::ReattachUnknown
            | Error::Provisioning(_)) => answer,
            _ => return,
        };
        self.last_response = Some(error);
        if let Some(last) = self.last {
            self.mixed |= last.code() != answer.code();
        }
        self.last = Some(answer);
    }

    fn expire(&mut self, pending: Error) -> Error {
        self.expired = true;
        // An overflow sandbox (Boat) that was full keeps its own refusal, whose
        // detail says the job tried overflow capacity, instead of becoming the
        // ordinary refusal for a full registered host.
        if pending == Error::BoatHostFull {
            return pending;
        }
        match self.last {
            Some(Error::NoCapacity(delay)) => {
                Error::NoCapacity(pending.retry_after_ms().unwrap_or(delay))
            }
            Some(answer) if answer.code() == pending.code() => pending,
            Some(answer) => answer,
            None => pending,
        }
    }

    fn detail(&self, error: Error, mut detail: String) -> String {
        if self.expired {
            detail.push_str(&format!(
                "; queue wait expired; final_cause={}",
                error.code()
            ));
            if let Some(last) = self.last_response {
                detail.push_str(&format!("; last_response={}", last.code()));
            }
            if self.mixed {
                detail.push_str("; mixed_answers=true; using last definite answer");
            }
        }
        detail
    }
}

pub(crate) struct Client {
    http: reqwest::Client,
    route: HostedJobState,
    account_tokens: Option<Arc<account::AccountTokens>>,
    /// The `error` code of the broker's last refusal, when it was a valid
    /// name. A direct hosted test shows it to the operator.
    broker_code: std::sync::Mutex<Option<String>>,
    /// The HTTP status of the broker's answer to the latest request when it
    /// was not a success, so a lease refusal can be logged with its body.
    broker_status: std::sync::Mutex<Option<u16>>,
    broker_failure: std::sync::Mutex<Option<String>>,
    wait_answers: std::sync::Mutex<WaitAnswers>,
    probe_replies: Option<std::sync::Mutex<fence_probe::Replies>>,
    booking_open: Option<Arc<std::sync::Mutex<bool>>>,
    cancellation: Option<(Uuid, Arc<Storage>, tokio::sync::watch::Receiver<bool>)>,
    progress: crate::motor::progress::Reporter,
    #[cfg(test)]
    connector: Option<Arc<dyn crate::transport::Connector>>,
}
impl Client {
    pub(crate) fn new(route: HostedJobState) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(account::BROKER_TIMEOUT)
                .build()
                .expect("HTTP client configuration"),
            route,
            account_tokens: None,
            broker_code: std::sync::Mutex::new(None),
            broker_status: std::sync::Mutex::new(None),
            broker_failure: std::sync::Mutex::new(None),
            wait_answers: std::sync::Mutex::new(WaitAnswers::default()),
            probe_replies: None,
            booking_open: None,
            cancellation: None,
            progress: crate::motor::progress::Reporter::new(false, 2),
            #[cfg(test)]
            connector: None,
        }
    }
    pub(crate) fn with_cancellation(
        mut self,
        id: Uuid,
        storage: Arc<Storage>,
        wake: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        self.cancellation = Some((id, storage, wake));
        self
    }

    fn cancel_pending(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|(id, storage, _)| storage.job(*id).is_some_and(|job| job.pending_cancel))
    }

    async fn cancelled(&self) {
        let Some((_, _, wake)) = &self.cancellation else {
            return std::future::pending().await;
        };
        let mut wake = wake.clone();
        loop {
            wake.borrow_and_update();
            if self.cancel_pending() {
                return;
            }
            if wake.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    async fn while_placing<T>(
        &self,
        future: impl std::future::Future<Output = T>,
    ) -> Result<T, Error> {
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(Error::Cancelled),
            result = future => Ok(result),
        }
    }

    /// The `error` code of the broker's last refusal seen by this client.
    pub(crate) fn broker_code(&self) -> Option<String> {
        self.broker_code.lock().unwrap().clone()
    }
    pub(crate) fn refusal_detail(&self, error: Error, detail: String) -> String {
        let mut detail = self.wait_answers.lock().unwrap().detail(error, detail);
        if !self.route.run_sent
            && matches!(error, Error::HostFull | Error::Capacity)
            && self.broker_code().as_deref() == Some("job_finished")
        {
            detail.push_str(": server refused admission while restarting or full");
        }
        detail
    }
    fn wait_expired(&self, pending: Error) -> Error {
        self.wait_answers.lock().unwrap().expire(pending)
    }
    /// The broker's 4xx status for the latest request, if it answered one.
    fn broker_client_error(&self) -> Option<u16> {
        self.broker_status
            .lock()
            .unwrap()
            .filter(|status| (400..500).contains(status))
    }
    pub(crate) fn with_account_tokens(
        mut self,
        tokens: Option<Arc<account::AccountTokens>>,
    ) -> Self {
        self.account_tokens = tokens;
        self
    }
    pub(crate) fn with_booking_gate(mut self, gate: Arc<std::sync::Mutex<bool>>) -> Self {
        self.booking_open = Some(gate);
        self
    }
    pub(crate) fn with_progress(mut self, progress: crate::motor::progress::Reporter) -> Self {
        self.progress = progress;
        self
    }
    #[cfg(test)]
    pub(crate) fn with_connector(
        mut self,
        connector: Option<Arc<dyn crate::transport::Connector>>,
    ) -> Self {
        self.connector = connector;
        self
    }
    async fn post<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, Error> {
        *self.broker_failure.lock().unwrap() = Some("bearer acquisition pending".into());
        let bearer = self.bearer().await.inspect_err(|error| {
            *self.broker_failure.lock().unwrap() =
                Some(format!("bearer acquisition failed: {}", error.code()));
        })?;
        self.post_authorized(path, body, bearer).await
    }
    async fn bearer(&self) -> Result<String, Error> {
        match self.route.token_source {
            BrokerTokenSource::Fake => {
                std::env::var("MOTOR_FAKE_BROKER_TOKEN").map_err(|_| Error::Auth)
            }
            BrokerTokenSource::CkAccount => {
                self.account_tokens
                    .as_ref()
                    .ok_or(Error::AccountNotFound)?
                    .bearer()
                    .await
            }
        }
    }
    async fn post_once<T: Serialize>(
        &self,
        path: &str,
        body: &T,
        bearer: &str,
    ) -> Result<reqwest::Response, Error> {
        *self.broker_failure.lock().unwrap() = Some("broker HTTP request pending".into());
        *self.broker_status.lock().unwrap() = None;
        let reply = self
            .http
            .post(format!(
                "{}{path}",
                self.route.broker_url.trim_end_matches('/')
            ))
            .bearer_auth(bearer)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                *self.broker_failure.lock().unwrap() = Some(format!("transport: {error}"));
                tracing::info!(
                    path,
                    connect = error.is_connect(),
                    timeout = error.is_timeout(),
                    "hosted broker request failed"
                );
                Error::Broker
            })?;
        tracing::info!(
            path,
            http_status = reply.status().as_u16(),
            "hosted broker replied"
        );
        self.observe_probe_reply(path, reply.status().as_u16());
        Ok(reply)
    }
    async fn post_with_refresh<T: Serialize>(
        &self,
        path: &str,
        body: &T,
        mut bearer: String,
    ) -> Result<reqwest::Response, Error> {
        let mut reply = self.post_once(path, body, &bearer).await?;
        if self.route.token_source == BrokerTokenSource::CkAccount
            && reply.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            let tokens = self.account_tokens.as_ref().ok_or(Error::AccountNotFound)?;
            tokens.invalidate(&bearer).await;
            self.discard_probe_reply(path, reply).await;
            bearer = tokens.bearer().await?;
            reply = self.post_once(path, body, &bearer).await?;
            if reply.status() == reqwest::StatusCode::UNAUTHORIZED {
                self.discard_probe_reply(path, reply).await;
                tokens.invalidate(&bearer).await;
                *self.broker_status.lock().unwrap() = Some(401);
                *self.broker_failure.lock().unwrap() =
                    Some("HTTP 401, account bearer rejected".into());
                return Err(Error::AccountInvalid);
            }
        }
        Ok(reply)
    }
    async fn post_authorized<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        bearer: String,
    ) -> Result<R, Error> {
        let reply = self.post_with_refresh(path, body, bearer).await?;
        if reply.status().as_u16() == 202 && path == "/v1/leases" {
            let provisioning: ProvisioningReply = reply.json().await.map_err(|_| Error::Broker)?;
            return if provisioning.status == "provisioning" {
                Err(Error::Provisioning(provisioning.retry_after_ms))
            } else {
                Err(Error::Broker)
            };
        }
        if !reply.status().is_success() {
            let status = reply.status().as_u16();
            let retry_after_ms = reply
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|seconds| seconds.saturating_mul(1000))
                .unwrap_or(MIN_PROVISIONING_RETRY_MS);
            let code = reply.json::<serde_json::Value>().await.ok();
            self.observe_probe_error(path, code.as_ref());
            let broker_code = code
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|v| v.as_str())
                .filter(|code| motor_lease::valid_name(code));
            tracing::info!(
                path,
                http_status = status,
                broker_code,
                "hosted broker refused"
            );
            *self.broker_code.lock().unwrap() = broker_code.map(str::to_owned);
            *self.broker_status.lock().unwrap() = Some(status);
            *self.broker_failure.lock().unwrap() =
                Some(format!("HTTP {status}, broker_code={broker_code:?}"));
            // A closed account's new lease is the one 403 that is not an
            // authentication failure. The contract sends it on lease creation
            // only; anywhere else a 403 stays an authentication failure.
            if status == 403
                && path == "/v1/leases"
                && code
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                    == Some("account_closed")
            {
                return Err(Error::AccountClosed);
            }
            if matches!(status, 401 | 403) {
                return Err(Error::Auth);
            }
            return Err(
                match code
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                {
                    Some("job_admitted") => Error::Admitted,
                    Some("job_finished") if status == 409 => Error::Finished,
                    Some("replace_pending")
                        if status == 503
                            && (path == "/v1/leases" || path.ends_with("/replace")) =>
                    {
                        Error::ReplacePending(retry_after_ms)
                    }
                    Some("no_capacity")
                        if status == 503
                            && path == "/v1/leases"
                            && self.route.capacity_routed() =>
                    {
                        Error::NoCapacity(bounded_provisioning_retry_ms(retry_after_ms))
                    }
                    Some("capacity" | "no_capacity") => Error::Capacity,
                    // Keyed on the error code alone: the broker sends it with
                    // HTTP 422, but no status makes it worth retrying. The
                    // limits are informational; missing or malformed
                    // values are recorded independently as unknown.
                    Some("no_host_fits") if path == "/v1/leases" => Error::NoHostFits(
                        code.as_ref()
                            .and_then(|v| v.get("max_vcpus"))
                            .and_then(serde_json::Value::as_u64),
                        code.as_ref()
                            .and_then(|v| v.get("max_mem_mib"))
                            .and_then(serde_json::Value::as_u64),
                    ),
                    Some("host_full") => Error::HostFull,
                    Some("job_unknown") if status == 404 && path.ends_with("/replace") => {
                        Error::NotGranted
                    }
                    Some("job_released") => Error::Released,
                    // A refused lease has no granted lease for release to cancel.
                    Some("job_unknown") if status == 404 && path.ends_with("/release") => {
                        Error::NotGranted
                    }
                    Some("job_unknown") if status == 404 && path.ends_with("/reattach") => {
                        Error::ReattachUnknown
                    }
                    Some("budget_exhausted") => Error::Budget,
                    Some("platform_unsupported") => Error::Platform,
                    Some("sandbox_lost") if status == 409 => Error::SandboxLost,
                    // The broker could not parse the lease body. Nothing was
                    // booked, so a spilled job can go back to the build
                    // server. Old brokers may omit the offending member.
                    Some("bad_request") if status == 400 && path == "/v1/leases" => {
                        Error::RequestInvalid(
                            code.as_ref()
                                .and_then(|v| v.get("field"))
                                .and_then(serde_json::Value::as_str)
                                .and_then(RequestField::new),
                        )
                    }
                    Some("job_conflict" | "bad_request") => Error::Rejected,
                    other if status == 409 => {
                        let broker_code = other
                            .filter(|code| motor_lease::valid_name(code))
                            .unwrap_or("invalid_error_code");
                        tracing::warn!(broker_code, "unrecognized hosted broker conflict");
                        Error::BrokerRefused
                    }
                    _ => Error::Broker,
                },
            );
        }
        reply.json().await.map_err(|error| {
            *self.broker_failure.lock().unwrap() =
                Some(format!("invalid broker response: {error}"));
            Error::Broker
        })
    }
    pub(crate) async fn release(&self, id: Uuid) -> Result<(), Error> {
        if self
            .route
            .placement
            .as_ref()
            .is_some_and(|p| p.host.starts_with("boat:"))
        {
            return Err(Error::Admitted);
        }
        let id = self.route.runner_id(id);
        self.post::<_, motor_lease::ReleaseReply>(
            &format!("/v1/leases/{id}/release"),
            &ReleaseRequest::default(),
        )
        .await
        .map(|_| ())
    }
    pub(crate) async fn attach(&self, id: Uuid, storage: &Storage) -> Result<Channel, Error> {
        let key = requester()?;
        let runner_id = self.route.runner_id(id);
        let lease: Lease = self
            .post_ready(
                &format!("/v1/leases/{runner_id}/reattach"),
                &reattach_body(&key),
            )
            .await?;
        if self
            .route
            .placement
            .as_ref()
            .is_some_and(|p| p != &lease.placement)
            || (lease.ssh.is_none()
                && (self
                    .route
                    .endpoint
                    .as_ref()
                    .is_some_and(|e| e != &lease.endpoint)
                    || self
                        .route
                        .endpoint_spki_sha256
                        .as_ref()
                        .is_some_and(|p| p != &lease.endpoint_spki_sha256)))
        {
            return Err(Error::Auth);
        }
        storage
            .hosted_update(id, |state| state.lease(&lease))
            .map_err(|_| Error::Broker)?;
        self.connect(runner_id, &lease, &key, motor_lease::Purpose::Attach)
            .await
            .map_err(|failure| {
                tracing::info!(job_id = %id, code = failure.error.code(), step = failure.step,
                    cause = failure.host_reason.as_deref().unwrap_or(failure.error.code()),
                    host_reason = failure.host_reason.as_deref().unwrap_or(""), "hosted attach failed");
                if failure.host_reason.as_deref() == Some("auth") {
                    Error::AttachAuth
                } else {
                    failure.error
                }
            })
    }
    #[cfg(test)]
    pub(crate) async fn run(
        &self,
        id: Uuid,
        request: &RunRequest,
        storage: &Arc<Storage>,
    ) -> Result<(Channel, Lease), Error> {
        self.run_for_caller(id, request, storage, "direct").await
    }
    pub(crate) async fn run_for_caller(
        &self,
        id: Uuid,
        request: &RunRequest,
        storage: &Arc<Storage>,
        caller: &str,
    ) -> Result<(Channel, Lease), Error> {
        *self.wait_answers.lock().unwrap() = WaitAnswers::default();
        self.progress
            .phase(motor_protocol::ProgressPhase::PlacementWait);
        let job = storage.job(id).ok_or(Error::Broker)?;
        let placement_started = tokio::time::Instant::now();
        let provisioning_deadline =
            tokio::time::Instant::now() + Duration::from_millis(spill_wait_budget_ms(&job));
        let salt = storage.hosted_salt().map_err(|_| Error::Auth)?;
        let avoid_host = job
            .hosted
            .as_ref()
            .and_then(|state| state.avoid_host.clone());
        let allow_spill = request.timeout.is_some_and(|s| s <= BOAT_MAX_TIMEOUT_S)
            && !job
                .hosted
                .as_ref()
                .is_some_and(|state| state.repository_routed || state.fleet_no_spill);
        let route = job.hosted.as_ref().map_or("hosted", |state| {
            if state.spill_wait_ms.is_some() {
                "spill"
            } else if state.repository_routed {
                "repository_routed"
            } else if state.fleet_routed {
                "fleet_routed"
            } else if state.hosted_test {
                "hosted_test"
            } else {
                "hosted"
            }
        });
        let repository_hint = job
            .hosted
            .as_ref()
            .and_then(|state| state.repository_placement_hint.clone());
        let mut body = match job.hosted.and_then(|state| state.lease_request) {
            // The broker hashes placement_hint. Replay legacy requests unchanged
            // (apart from requester_jwk below), even if they contain a cached hint:
            // clearing it could turn an in-flight lease into job_conflict.
            Some(request) => request,
            None => {
                // Only configured hints may opt into an explicit-only host.
                // Workspace affinity belongs to the broker's current warm inventory,
                // matched by workspace_key_hash, not a previous job's placement.
                lease_body(
                    id,
                    request,
                    (
                        keyed_hash(&salt, "ck-ws-v1", &request.workspace_key)
                            .map_err(|_| Error::Path)?,
                        keyed_hash(&salt, "ck-repo-v1", &request.repository_root)
                            .map_err(|_| Error::Path)?,
                    ),
                    repository_hint,
                    avoid_host,
                    RequesterJwk::from_key(&requester()?.verifying_key()),
                    Some(allow_spill),
                )?
            }
        };
        // Never send a body the broker would refuse. The requester key that
        // each attempt below puts in is freshly generated and always valid.
        if let Some(field) = invalid_lease_field(&body) {
            warn_lease_body(id, &body, route, None, None, Some(field));
            return Err(Error::RequestInvalid(RequestField::new(field)));
        }
        storage
            .hosted_update(id, |state| state.lease_request = Some(body.clone()))
            .map_err(|_| Error::Broker)?;
        // The refusal returned if the queue-wait deadline passes before the
        // broker answers the next lease request. By default a broker that
        // has not answered in time is treated as unreachable. After the
        // granted host refused the job as full, the deadline ends a wait for
        // that host to free a slot, so a request cut off by the deadline
        // returns the host-full refusal (runner full) rather than the
        // transport failure the cut-off causes.
        let mut cut_off = Error::Provisioning(MIN_PROVISIONING_RETRY_MS);
        let mut boat_host_full = false;
        'lease_requests: loop {
            if self.cancel_pending() {
                return Err(Error::Cancelled);
            }
            self.progress
                .phase(motor_protocol::ProgressPhase::PlacementWait);
            let state = storage
                .job(id)
                .and_then(|j| j.hosted)
                .ok_or(Error::Broker)?;
            if state.placement_attempts >= 3 {
                return Err(Error::Connect);
            }
            if let Some(iat) = state.retry_iat {
                if self
                    .while_placing(tokio::time::timeout_at(
                        provisioning_deadline,
                        wait_after_iat(iat),
                    ))
                    .await?
                    .is_err()
                {
                    return Err(self.wait_expired(cut_off));
                }
            }
            let attempt = state.placement_attempts;
            let spilled = state.spill_wait_ms.is_some();
            let hosted_test = state.hosted_test;
            let capacity_routed = state.capacity_routed();
            let runner_id = state.runner_id(id);
            let key = requester()?;
            body.requester_jwk = RequesterJwk::from_key(&key.verifying_key());
            // Acquire credentials before recording this attempt: a vault refusal
            // means no HTTP request was sent to create a reservation for it.
            let bearer = match self
                .while_placing(tokio::time::timeout_at(
                    provisioning_deadline,
                    self.bearer(),
                ))
                .await?
            {
                Ok(result) => result?,
                Err(_) => return Err(self.wait_expired(cut_off)),
            };
            // Even if the lease response is lost, the broker may have issued a
            // token. Wait past its possible issue second before requesting another:
            // the host accepts a replacement run token only with a greater iat.
            {
                let gate = self.booking_open.as_ref().map(|gate| gate.lock().unwrap());
                if gate.as_ref().is_some_and(|open| !**open) {
                    return Err(Error::Shutdown);
                }
                storage
                    .hosted_update(id, |state| {
                        state.lease_attempted = true;
                        state.placement_attempts += 1;
                        state.retry_iat = Some(now_s());
                    })
                    .map_err(|_| Error::Broker)?;
            }
            tracing::info!(job_id = %id, attempt, vcpus = body.vcpus, platform = %body.platform,
                caller, repo = %Path::new(&request.repository_root).file_name().unwrap_or_default().to_string_lossy(),
                // A prefix of the salted hash the broker already receives: enough to
                // follow one workspace across hosts in the log, and reveals no path.
                workspace = %body.workspace_key_hash.get(..8).unwrap_or_default(),
                spilled, "hosted lease requested");
            let mut lease: Lease = match self
                .post_ready_authorized(
                    "/v1/leases",
                    &body,
                    bearer,
                    provisioning_deadline,
                    // The host-full wait ends with this request: whatever the
                    // broker answers decides what happens next.
                    std::mem::replace(&mut cut_off, Error::Provisioning(MIN_PROVISIONING_RETRY_MS)),
                )
                .await
            {
                Ok(l) => l,
                Err(error) => {
                    tracing::info!(job_id = %id, attempt, code = error.code(), "hosted lease not granted");
                    if let Some(status) = self.broker_client_error() {
                        warn_lease_body(
                            id,
                            &body,
                            route,
                            Some(status),
                            self.broker_code().as_deref(),
                            error.request_field(),
                        );
                    }
                    match error {
                        Error::NoCapacity(_) | Error::Provisioning(_) if boat_host_full => {
                            return Err(Error::BoatHostFull);
                        }
                        // No host can fit this job's shape, so the same
                        // request would be refused again. The caller decides
                        // where the job goes instead.
                        Error::NoHostFits(_, _) => return Err(error),
                        // The account is closed for good; asking again is
                        // refused the same way.
                        Error::AccountClosed => return Err(error),
                        // A broker without capacity for a spilled job will not
                        // gain it within seconds (its Boat start cap is hourly);
                        // the build server is the better place to retry.
                        Error::Capacity if spilled => return Err(error),
                        // A direct hosted test reports the broker's answer at
                        // once rather than waiting for capacity to appear.
                        Error::Capacity | Error::HostFull if hosted_test => return Err(error),
                        Error::Broker | Error::ReattachUnknown if attempt < 2 => continue,
                        error if error.replaceable() && attempt < 2 => continue,
                        error => return Err(error),
                    }
                }
            };
            loop {
                let boat = lease.ssh.is_some() || lease.placement.host.starts_with("boat:");
                // Record the placement before judging the lease. A booked Boat job
                // can only be settled with finish (release answers job_admitted), so
                // whatever is decided next must know where the broker placed it.
                storage
                    .hosted_update(id, |state| state.lease(&lease))
                    .map_err(|_| Error::Broker)?;
                if self.cancel_pending() {
                    return Err(Error::Cancelled);
                }
                tracing::info!(job_id = %id, attempt, provider = if boat { "boat" } else { "first_party" },
                host = %lease.placement.host, vm = lease.placement.vm.as_deref().unwrap_or(""),
                max_s = lease.max_s, "hosted lease granted");
                let claims = tls::claims(runner_id, &lease, &key, motor_lease::Purpose::Run)?;
                if let Some((field, expected, actual)) = resource_mismatch(&claims, &body, boat) {
                    tracing::info!(job_id = %id, field, expected = %expected, actual = %actual,
                    "hosted lease claims mismatch");
                    return Err(Error::Auth);
                }
                storage
                    .hosted_update(id, |state| state.retry_iat = Some(claims.iat))
                    .map_err(|_| Error::Broker)?;
                if lease.max_s < 10 {
                    return Err(Error::Short);
                }
                tracing::info!(job_id = %id, attempt, transport = if lease.ssh.is_some() { "ssh" } else { "tls" },
                "hosted connect started");
                self.progress.phase(motor_protocol::ProgressPhase::Connect);
                let connected = self
                    .while_placing(self.connect(runner_id, &lease, &key, motor_lease::Purpose::Run))
                    .await?;
                match &connected {
                    Ok(_) => tracing::info!(job_id = %id, attempt, "hosted connect succeeded"),
                    Err(failure) => {
                        self.wait_answers.lock().unwrap().observe(failure.error);
                        tracing::info!(job_id = %id, attempt, code = failure.error.code(), step = failure.step,
                        cause = failure.host_reason.as_deref().unwrap_or(failure.error.code()),
                        host_reason = failure.host_reason.as_deref().unwrap_or(""), "hosted connect failed")
                    }
                }
                if matches!(&connected, Err(failure) if matches!(failure.error, Error::HostFull | Error::Capacity))
                {
                    self.progress
                        .phase(motor_protocol::ProgressPhase::PlacementWait);
                }
                if boat
                    && matches!(&connected, Err(failure) if failure.error == Error::HostFull)
                    && !state.run_sent
                {
                    // Boat admits at booking, so release cannot free this grant.
                    // A lost finish retires the busy sandbox; only after settlement
                    // may a fresh booking use another internal ID. Keep the caller's
                    // submission and snapshot, and charge every grant to the same
                    // placement limit and queue deadline.
                    let settled = tokio::time::timeout_at(
                        provisioning_deadline,
                        self.finish_never_ran(id, storage),
                    )
                    .await;
                    if !matches!(settled, Ok(Ok(())))
                        || !storage
                            .job(id)
                            .and_then(|j| j.hosted)
                            .is_some_and(|s| s.finish_done)
                        || attempt >= 2
                    {
                        return Err(Error::BoatHostFull);
                    }
                    let retry_at =
                        tokio::time::Instant::now() + Duration::from_millis(HOST_FULL_RETRY_MS);
                    if retry_at >= provisioning_deadline {
                        self.while_placing(tokio::time::sleep_until(provisioning_deadline))
                            .await?;
                        return Err(Error::BoatHostFull);
                    }
                    tracing::info!(job_id = %id, attempt, retry_after_ms = HOST_FULL_RETRY_MS,
                        "Boat host full; waiting for a fresh booking");
                    self.while_placing(tokio::time::sleep_until(retry_at))
                        .await?;
                    body.job_id = Uuid::now_v7().to_string();
                    storage
                        .hosted_update(id, |s| {
                            s.lease_request = Some(body.clone());
                            s.placement = None;
                            s.endpoint = None;
                            s.endpoint_spki_sha256 = None;
                            s.max_runtime_s = None;
                            s.billed_vcpu_ms = None;
                            s.billing = BillingStatus::Pending;
                            s.finish_done = false;
                            s.lease_attempted = false;
                        })
                        .map_err(|_| Error::Broker)?;
                    boat_host_full = true;
                    cut_off = Error::BoatHostFull;
                    continue 'lease_requests;
                }
                if matches!(&connected, Err(failure) if matches!(failure.error, Error::HostFull | Error::Capacity))
                    && !boat
                {
                    let state = storage
                        .job(id)
                        .and_then(|j| j.hosted)
                        .ok_or(Error::Broker)?;
                    if !state.run_sent
                        && state.repository_placement_hint.is_none()
                        && !state.replacement_hosts.contains(&lease.placement.host)
                    {
                        tracing::info!(job_id = %id, host_id = %lease.placement.host,
                            "hosted capacity refusal; requesting replacement");
                        // A refusal proves this connection never reached Run. Keep
                        // the reservation and requester key while the broker tries
                        // another host; any refusal leaves the old placement intact.
                        let replacement = async {
                            let bearer = self
                                .while_placing(tokio::time::timeout_at(
                                    provisioning_deadline,
                                    self.bearer(),
                                ))
                                .await?
                                .map_err(|_| Error::HostFull)??;
                            self.post_ready_authorized::<_, Lease>(
                                &format!("/v1/leases/{runner_id}/replace"),
                                &serde_json::json!({}),
                                bearer,
                                provisioning_deadline,
                                Error::HostFull,
                            )
                            .await
                        }
                        .await;
                        match replacement {
                            Ok(next) => {
                                if next.lease_id != lease.lease_id
                                    || next.job_id != lease.job_id
                                    || next.max_s != lease.max_s
                                    || next.reserved_vcpu_ms != lease.reserved_vcpu_ms
                                    || next.placement.host == lease.placement.host
                                    || next.placement.host.starts_with("boat:")
                                    || next.ssh.is_some()
                                {
                                    return Err(Error::Auth);
                                }
                                storage
                                    .hosted_update(id, |state| {
                                        state
                                            .replacement_hosts
                                            .insert(lease.placement.host.clone());
                                    })
                                    .map_err(|_| Error::Broker)?;
                                lease = next;
                                continue;
                            }
                            Err(Error::Finished) => {
                                // No Run was sent, and the settled lease cannot be
                                // replaced or admitted later. Report the host's
                                // refusal instead of waiting on the closed booking.
                                return Err(connected.err().unwrap().error);
                            }
                            Err(error) if self.wait_answers.lock().unwrap().expired => {
                                return Err(error);
                            }
                            Err(error) => {
                                if matches!(
                                    error,
                                    Error::Admitted
                                        | Error::Finished
                                        | Error::Released
                                        | Error::Rejected
                                        | Error::NotGranted
                                        | Error::Capacity
                                        | Error::BrokerRefused
                                ) {
                                    storage
                                        .hosted_update(id, |state| {
                                            state
                                                .replacement_hosts
                                                .insert(lease.placement.host.clone());
                                        })
                                        .map_err(|_| Error::Broker)?;
                                }
                                tracing::info!(job_id = %id, code = error.code(),
                                    "hosted replace refused; waiting on current placement");
                            }
                        }
                    }
                }
                match connected.map_err(|failure| failure.error) {
                    Ok(channel) => {
                        storage
                            .hosted_update(id, |state| {
                                state.placement_wait_ms = Some(
                                    placement_started
                                        .elapsed()
                                        .as_millis()
                                        .try_into()
                                        .unwrap_or(u64::MAX),
                                );
                            })
                            .map_err(|_| Error::Broker)?;
                        return Ok((channel, lease));
                    }
                    Err(e) if lease.ssh.is_some() => return Err(e),
                    Err(e @ (Error::HostFull | Error::Capacity)) if hosted_test => return Err(e),
                    // The broker's view of the host lags its heartbeats, so it can
                    // grant a lease the host then refuses as full. A routed job
                    // would have queued on the build server, so it waits for the
                    // host instead, within its queue-wait budget, the way it waits
                    // out 503 no_capacity. The host stored nothing for the
                    // refused token, and the broker keeps the reservation, so
                    // asking again with the same job ID and body returns the same
                    // placement with a fresh run token, which the host accepts
                    // because its issue time is later. These waits are not failed
                    // placements, so they don't use up the three placement
                    // attempts.
                    Err(e @ (Error::HostFull | Error::Capacity)) if capacity_routed => {
                        let retry_at =
                            tokio::time::Instant::now() + Duration::from_millis(HOST_FULL_RETRY_MS);
                        if retry_at >= provisioning_deadline {
                            self.while_placing(tokio::time::sleep_until(provisioning_deadline))
                                .await?;
                            return Err(self.wait_expired(Error::NoCapacity(HOST_FULL_RETRY_MS)));
                        }
                        storage
                            .hosted_update(id, |state| {
                                state.placement_attempts =
                                    state.placement_attempts.saturating_sub(1)
                            })
                            .map_err(|_| Error::Broker)?;
                        tracing::info!(job_id = %id, attempt, code = e.code(),
                        retry_after_ms = HOST_FULL_RETRY_MS, "hosted host full; waiting to ask again");
                        cut_off = Error::NoCapacity(HOST_FULL_RETRY_MS);
                        self.while_placing(tokio::time::sleep_until(retry_at))
                            .await?;
                        continue 'lease_requests;
                    }
                    Err(e) if e.replaceable() && attempt < 2 => continue 'lease_requests,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    async fn post_ready<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, Error> {
        self.post(path, body).await
    }
    /// Posts until the broker grants or refuses, waiting out `202
    /// provisioning`, routed `503 no_capacity` and `replace_pending` answers until `deadline`.
    /// When the deadline passes first, the request in flight is dropped and
    /// the last definite placement or transport answer is returned. Replacement
    /// fences preserve that answer; before any answer, `cut_off` describes the
    /// caller's own wait.
    async fn post_ready_authorized<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        mut bearer: String,
        deadline: tokio::time::Instant,
        cut_off: Error,
    ) -> Result<R, Error> {
        let mut pending = cut_off;
        loop {
            if self.cancel_pending() {
                return Err(Error::Cancelled);
            }
            let request = self.post_authorized(path, body, bearer);
            tokio::pin!(request);
            // Disconnect is not rollback: the broker commits a reservation before
            // replying (runners-contract.md, lease transaction and Boat booking).
            // On cancel, await this reply and let the submission record and settle
            // any grant before returning; never discard it to stop placement.
            let reply = tokio::select! {
                biased;
                reply = &mut request => Some(reply),
                _ = self.cancelled() => Some(request.await),
                _ = tokio::time::sleep_until(deadline) => None,
            };
            let reply = match reply {
                Some(Err(Error::HostFull | Error::Capacity)) if cut_off == Error::BoatHostFull => {
                    Some(Err(Error::NoCapacity(HOST_FULL_RETRY_MS)))
                }
                reply => reply,
            };
            if let Some(Err(error)) = &reply {
                self.wait_answers.lock().unwrap().observe(*error);
            }
            if self.cancel_pending() && !matches!(&reply, Some(Ok(_))) {
                return Err(Error::Cancelled);
            }
            match reply {
                None => return Err(self.wait_expired(pending)),
                Some(Err(
                    error @ (Error::Provisioning(_)
                    | Error::NoCapacity(_)
                    | Error::ReplacePending(_)),
                )) => {
                    let retry_after_ms = if let Error::ReplacePending(delay) = error {
                        delay.max(MIN_PROVISIONING_RETRY_MS)
                    } else {
                        bounded_provisioning_retry_ms(error.retry_after_ms().unwrap())
                    };
                    pending = match error {
                        Error::NoCapacity(_) => Error::NoCapacity(retry_after_ms),
                        Error::ReplacePending(_) => Error::ReplacePending(retry_after_ms),
                        _ => Error::Provisioning(retry_after_ms),
                    };
                    let retry_at = tokio::time::Instant::now()
                        .checked_add(Duration::from_millis(retry_after_ms))
                        .unwrap_or(deadline);
                    // Held-clock tests must observe a parsed reply and a fixed
                    // wake time before advancing time; a server-side send is too early.
                    #[cfg(test)]
                    tracing::info!(path, "hosted broker retry wait");
                    if retry_at >= deadline {
                        self.while_placing(tokio::time::sleep_until(deadline))
                            .await?;
                        return Err(self.wait_expired(pending));
                    }
                    self.while_placing(tokio::time::sleep_until(retry_at))
                        .await?;
                    bearer = match self
                        .while_placing(tokio::time::timeout_at(deadline, self.bearer()))
                        .await?
                    {
                        Ok(result) => result?,
                        Err(_) => return Err(self.wait_expired(pending)),
                    };
                }
                Some(result) => return result,
            }
        }
    }
    async fn connect(
        &self,
        id: Uuid,
        lease: &Lease,
        key: &SigningKey,
        purpose: motor_lease::Purpose,
    ) -> Result<Channel, tls::ConnectFailure> {
        #[cfg(test)]
        if let Some(connector) = &self.connector {
            let pipe = connector
                .connect(id)
                .await
                .map_err(|_| tls::ConnectFailure::at("test transport", Error::Connect))?;
            return Channel::from_hosted(pipe, placement::fills_cache(&lease.placement))
                .await
                .map_err(|_| tls::ConnectFailure::at("test hello", Error::Connect));
        }
        if lease.ssh.is_some() {
            let claims = tls::claims(id, lease, key, purpose)
                .map_err(|e| tls::ConnectFailure::at("claims", e))?;
            tokio::time::timeout(Duration::from_secs(60), async {
                let pipe = ssh::connect(lease, key)
                    .await
                    .map_err(|e| tls::ConnectFailure::at("ssh", e))?;
                tls::authenticate(pipe, lease, key, &claims).await
            })
            .await
            .unwrap_or(Err(tls::ConnectFailure::at("timeout", Error::Connect)))
        } else {
            tls::connect(id, lease, key, purpose).await
        }
    }
}
fn requester() -> Result<SigningKey, Error> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Auth)?;
    Ok(SigningKey::from_bytes(&bytes))
}
pub(crate) async fn wait_after_iat(iat: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let target = Duration::from_secs(iat.saturating_add(1));
    if target > now {
        tokio::time::sleep(target - now).await;
    }
}

#[cfg(test)]
pub(crate) mod boat_tests;
#[cfg(all(test, target_os = "linux"))]
mod relay_close_tests;
#[cfg(test)]
mod sandbox_tests;
#[cfg(all(test, target_os = "linux"))]
mod tenant_heartbeat_tests;
#[cfg(test)]
pub(crate) mod tests;
