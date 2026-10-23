//! Broker credentials and saved routes are independent of the build-server SSH
//! connector. Hosted jobs keep their route even after hosted routing is disabled.
pub(crate) mod account;
pub(crate) mod check;
mod finish;
pub(crate) use finish::SHADOW_CLEANUP_BOUND;
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
    pub hosted_no_fallback: bool,
    pub hosted_code: Option<String>,
    pub placement: Option<Placement>,
    pub endpoint: Option<String>,
    pub endpoint_spki_sha256: Option<String>,
    pub max_runtime_s: Option<u64>,
    pub billed_vcpu_ms: Option<u64>,
    pub billing: BillingStatus,
    pub terminal: Option<TerminalRecord>,
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
    /// Set durably just before ck-motor sends the Run frame to a hosted
    /// runner. Until then no runner holds the command, so whatever happened to
    /// the lease, the job provably never ran; afterwards it may have.
    #[serde(default)]
    pub(crate) run_sent: bool,
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
    Auth,
    Expired,
    Spki,
    SshKey,
    Provisioning(u64),
    SandboxLost,
    BrokerRefused,
    HostFull,
    Capacity,
    NoCapacity(u64),
    /// The broker answered a lease request with `no_host_fits`: no host it
    /// could ever place the job on has that many vCPUs, so asking again cannot
    /// succeed. Carries the broker's `max_vcpus`, the largest count it could
    /// place, when the reply included a valid one.
    NoHostFits(Option<u64>),
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
    /// when ck-motor found it before sending; the broker's refusal does not
    /// name one. Nothing was booked either way.
    RequestInvalid(Option<&'static str>),
}
/// How long a shadow waits for the broker to provision its sandbox. Resuming a
/// stopped Boat sandbox takes about 100 s and a cold start from the prepared
/// image up to about 3 minutes, so a shorter wait abandons most shadows that
/// don't land on a warm sandbox.
pub(crate) const SHADOW_PROVISIONING_BUDGET: Duration = Duration::from_secs(300);
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
            Self::Auth => "hosted_auth",
            Self::Expired => "hosted_expired",
            Self::Spki => "hosted_spki_mismatch",
            Self::SshKey => "hosted_ssh_host_key_mismatch",
            Self::Provisioning(_) => "hosted_provisioning",
            Self::SandboxLost => "hosted_sandbox_lost",
            Self::BrokerRefused => "hosted_broker_refused",
            Self::HostFull => "hosted_host_full",
            Self::Capacity | Self::NoCapacity(_) => "hosted_capacity",
            Self::NoHostFits(_) => "hosted_no_host_fits",
            Self::AccountClosed => "hosted_account_closed",
            Self::InternalFailure => "hosted_internal_failure",
            Self::StorageFull => "hosted_storage_full",
            Self::Connect => "hosted_connect_failed",
            Self::Broker => "hosted_broker_unreachable",
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
            | Self::Expired
            | Self::Spki
            | Self::SshKey
            | Self::AccountNotFound
            | Self::AccountAuthRequired
            | Self::AccountPermanent
            | Self::AccountInvalid => RefusalReason::RunnerAuthenticationFailed,
            Self::HostFull | Self::Capacity | Self::NoCapacity(_) | Self::StorageFull => {
                RefusalReason::RunnerFull
            }
            Self::Connect | Self::Broker | Self::AccountTransient(_) | Self::Provisioning(_) => {
                RefusalReason::Unreachable
            }
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
        } else if let Self::RequestInvalid(Some(field)) = self {
            format!("{}: {field}", self.code())
        } else {
            self.code().into()
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
            Self::Provisioning(delay) | Self::NoCapacity(delay) => Some(delay),
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
const LEASE_TIMEOUT_S: std::ops::RangeInclusive<u64> = 10..=7200;
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
    // whose lease the broker would refuse never spills or shadows, and a job
    // routed to hosted is refused naming the field instead of sending it.
    if !request
        .timeout
        .is_some_and(|s| LEASE_TIMEOUT_S.contains(&s))
    {
        return Err(Error::RequestInvalid(Some("timeout_s")));
    }
    if !LEASE_VCPUS.contains(&lease_vcpus(request)) {
        return Err(Error::RequestInvalid(Some("vcpus")));
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
/// the runner's per-job memory cap exactly. A Boat token states the whole
/// sandbox's usable memory, which may be below that cap; it must lie in
/// [`motor_lease::boat_memory_range`], the range the in-sandbox endpoint
/// also accepts.
fn resource_mismatch(
    claims: &motor_lease::Claims,
    body: &LeaseRequest,
    boat: bool,
) -> Option<(&'static str, String, String)> {
    let cap = motor_lease::memory_max_mib(body.vcpus);
    let range = motor_lease::boat_memory_range(body.vcpus);
    let memory_ok = if boat {
        range.contains(&claims.mem_mib)
    } else {
        claims.mem_mib == cap
    };
    if claims.vcpus != body.vcpus {
        Some(("vcpus", body.vcpus.to_string(), claims.vcpus.to_string()))
    } else if !memory_ok {
        let expected = if boat {
            format!("{}..={}", range.start(), range.end())
        } else {
            cap.to_string()
        };
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
fn valid_hint(placement: &Placement) -> bool {
    let host = placement
        .host
        .strip_prefix("boat:")
        .unwrap_or(&placement.host);
    motor_lease::valid_name(host) && placement.vm.as_deref().is_some_and(motor_lease::valid_name)
}
/// The first member of a lease body that CLOUD's lease parser refuses with
/// 400 `bad_request`, or `None` when it accepts the body. It mirrors the
/// rules of the pinned runners contract (POST /v1/leases and the common wire
/// grammar). `shadow` says the body is sent with `"shadow": true`. Every
/// lease request (spill, shadow, repository-routed and direct hosted) is
/// checked here before it is sent.
pub(crate) fn invalid_lease_field(body: &LeaseRequest, shadow: bool) -> Option<&'static str> {
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
    } else if shadow && body.allow_spill == Some(false) {
        Some("shadow")
    } else {
        None
    }
}
/// Logs a lease body by its non-secret members, as a warning: either
/// ck-motor found it invalid and did not send it (`http_status` is `None`,
/// `field` names the member), or the broker refused it with a 4xx status.
/// The requester key and the keyed path hashes are never logged. `route`
/// says why the job wanted a hosted lease (spill, repository_routed,
/// hosted_test, hosted or shadow).
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
        // The hint is optional: one the broker would refuse is left out
        // rather than costing the job its lease.
        placement_hint: hint.filter(valid_hint),
        avoid_host,
        timeout_s: request.timeout.ok_or(Error::NoTimeout)?,
        requester_jwk,
        allow_spill,
    })
}
fn reattach_body(key: &SigningKey) -> ReattachRequest {
    ReattachRequest {
        requester_jwk: RequesterJwk::from_key(&key.verifying_key()),
    }
}

#[derive(Serialize)]
struct ShadowLeaseRequest<'a> {
    #[serde(flatten)]
    request: &'a LeaseRequest,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shadow: bool,
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
        }
    }
    /// The `error` code of the broker's last refusal seen by this client.
    pub(crate) fn broker_code(&self) -> Option<String> {
        self.broker_code.lock().unwrap().clone()
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
    async fn post_authorized<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        bearer: String,
    ) -> Result<R, Error> {
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
                    Some("job_finished") => Error::Finished,
                    Some("no_capacity")
                        if status == 503
                            && path == "/v1/leases"
                            && self.route.repository_routed =>
                    {
                        Error::NoCapacity(bounded_provisioning_retry_ms(retry_after_ms))
                    }
                    Some("capacity" | "no_capacity") => Error::Capacity,
                    // Keyed on the error code alone: the broker sends it with
                    // HTTP 422, but no status makes it worth retrying. The
                    // `max_vcpus` count is informational; a missing or
                    // malformed one is recorded as unknown.
                    Some("no_host_fits") if path == "/v1/leases" => Error::NoHostFits(
                        code.as_ref()
                            .and_then(|v| v.get("max_vcpus"))
                            .and_then(serde_json::Value::as_u64),
                    ),
                    Some("host_full") => Error::HostFull,
                    Some("job_released") => Error::Released,
                    // A refused lease has no granted lease for release to cancel.
                    Some("job_unknown") if status == 404 && path.ends_with("/release") => {
                        Error::NotGranted
                    }
                    Some("budget_exhausted") => Error::Budget,
                    Some("platform_unsupported") => Error::Platform,
                    Some("sandbox_lost") if status == 409 => Error::SandboxLost,
                    // The broker could not parse the lease body. Nothing was
                    // booked, so a spilled job can go back to the build
                    // server; the refusal does not say which member was wrong.
                    Some("bad_request") if status == 400 && path == "/v1/leases" => {
                        Error::RequestInvalid(None)
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
        self.post::<_, motor_lease::ReleaseReply>(
            &format!("/v1/leases/{id}/release"),
            &ReleaseRequest::default(),
        )
        .await
        .map(|_| ())
    }
    pub(crate) async fn attach(&self, id: Uuid, storage: &Storage) -> Result<Channel, Error> {
        let key = requester()?;
        let lease: Lease = self
            .post_ready(&format!("/v1/leases/{id}/reattach"), &reattach_body(&key))
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
        self.connect(id, &lease, &key, motor_lease::Purpose::Attach)
            .await
            .map_err(|failure| {
                tracing::info!(job_id = %id, code = failure.error.code(), step = failure.step,
                    cause = failure.host_reason.as_deref().unwrap_or(failure.error.code()),
                    host_reason = failure.host_reason.as_deref().unwrap_or(""), "hosted attach failed");
                failure.error
            })
    }
    pub(crate) async fn run(
        &self,
        id: Uuid,
        request: &RunRequest,
        storage: &Arc<Storage>,
    ) -> Result<(Channel, Lease), Error> {
        let job = storage.job(id).ok_or(Error::Broker)?;
        let provisioning_deadline =
            tokio::time::Instant::now() + Duration::from_millis(spill_wait_budget_ms(&job));
        let salt = storage.hosted_salt().map_err(|_| Error::Auth)?;
        let avoid_host = job
            .hosted
            .as_ref()
            .and_then(|state| state.avoid_host.clone());
        let allow_spill = !job
            .hosted
            .as_ref()
            .is_some_and(|state| state.repository_routed);
        let route = job.hosted.as_ref().map_or("hosted", |state| {
            if state.spill_wait_ms.is_some() {
                "spill"
            } else if state.repository_routed {
                "repository_routed"
            } else if state.hosted_test {
                "hosted_test"
            } else {
                "hosted"
            }
        });
        let mut body = match job.hosted.and_then(|state| state.lease_request) {
            Some(request) => request,
            None => lease_body(
                id,
                request,
                (
                    keyed_hash(&salt, "ck-ws-v1", &request.workspace_key)
                        .map_err(|_| Error::Path)?,
                    keyed_hash(&salt, "ck-repo-v1", &request.repository_root)
                        .map_err(|_| Error::Path)?,
                ),
                storage.hosted_placement(&request.workspace_key),
                avoid_host,
                RequesterJwk::from_key(&requester()?.verifying_key()),
                Some(allow_spill),
            )?,
        };
        // Never send a body the broker would refuse. The requester key that
        // each attempt below puts in is freshly generated and always valid.
        if let Some(field) = invalid_lease_field(&body, false) {
            warn_lease_body(id, &body, route, None, None, Some(field));
            return Err(Error::RequestInvalid(Some(field)));
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
        loop {
            let state = storage
                .job(id)
                .and_then(|j| j.hosted)
                .ok_or(Error::Broker)?;
            if state.placement_attempts >= 3 {
                return Err(Error::Connect);
            }
            if let Some(iat) = state.retry_iat {
                if tokio::time::timeout_at(provisioning_deadline, wait_after_iat(iat))
                    .await
                    .is_err()
                {
                    return Err(cut_off);
                }
            }
            let attempt = state.placement_attempts;
            let spilled = state.spill_wait_ms.is_some();
            let hosted_test = state.hosted_test;
            let repository_routed = state.repository_routed;
            let key = requester()?;
            body.requester_jwk = RequesterJwk::from_key(&key.verifying_key());
            // Acquire credentials before recording this attempt: a vault refusal
            // means no HTTP request was sent to create a reservation for it.
            let bearer = match tokio::time::timeout_at(provisioning_deadline, self.bearer()).await {
                Ok(result) => result?,
                Err(_) => return Err(cut_off),
            };
            // Even if the lease response is lost, the broker may have issued a
            // token. Wait past its possible issue second before requesting another:
            // the host accepts a replacement run token only with a greater iat.
            storage
                .hosted_update(id, |state| {
                    state.lease_attempted = true;
                    state.placement_attempts += 1;
                    state.retry_iat = Some(now_s());
                })
                .map_err(|_| Error::Broker)?;
            tracing::info!(job_id = %id, attempt, vcpus = body.vcpus, platform = %body.platform,
                spilled, "hosted lease requested");
            let lease: Lease = match self
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
                            None,
                        );
                    }
                    match error {
                        // No host can ever hold this many vCPUs, so the same
                        // request would be refused again. The caller decides
                        // where the job goes instead.
                        Error::NoHostFits(_) => return Err(error),
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
                        Error::Broker if attempt < 2 => continue,
                        error if error.replaceable() && attempt < 2 => continue,
                        error => return Err(error),
                    }
                }
            };
            let boat = lease.ssh.is_some() || lease.placement.host.starts_with("boat:");
            // Record the placement before judging the lease. A booked Boat job
            // can only be settled with finish (release answers job_admitted), so
            // whatever is decided next must know where the broker placed it.
            storage
                .hosted_update(id, |state| state.lease(&lease))
                .map_err(|_| Error::Broker)?;
            tracing::info!(job_id = %id, attempt, provider = if boat { "boat" } else { "first_party" },
                host = %lease.placement.host, vm = lease.placement.vm.as_deref().unwrap_or(""),
                max_s = lease.max_s, "hosted lease granted");
            let claims = tls::claims(id, &lease, &key, motor_lease::Purpose::Run)?;
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
            let connected = self
                .connect(id, &lease, &key, motor_lease::Purpose::Run)
                .await;
            match &connected {
                Ok(_) => tracing::info!(job_id = %id, attempt, "hosted connect succeeded"),
                Err(failure) => {
                    tracing::info!(job_id = %id, attempt, code = failure.error.code(), step = failure.step,
                        cause = failure.host_reason.as_deref().unwrap_or(failure.error.code()),
                        host_reason = failure.host_reason.as_deref().unwrap_or(""), "hosted connect failed")
                }
            }
            match connected.map_err(|failure| failure.error) {
                Ok(channel) => return Ok((channel, lease)),
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
                Err(e @ (Error::HostFull | Error::Capacity)) if repository_routed => {
                    let retry_at =
                        tokio::time::Instant::now() + Duration::from_millis(HOST_FULL_RETRY_MS);
                    if retry_at >= provisioning_deadline {
                        tokio::time::sleep_until(provisioning_deadline).await;
                        return Err(Error::NoCapacity(HOST_FULL_RETRY_MS));
                    }
                    storage
                        .hosted_update(id, |state| {
                            state.placement_attempts = state.placement_attempts.saturating_sub(1)
                        })
                        .map_err(|_| Error::Broker)?;
                    tracing::info!(job_id = %id, attempt, code = e.code(),
                        retry_after_ms = HOST_FULL_RETRY_MS, "hosted host full; waiting to ask again");
                    cut_off = Error::NoCapacity(HOST_FULL_RETRY_MS);
                    tokio::time::sleep_until(retry_at).await;
                    continue;
                }
                Err(e) if e.replaceable() && attempt < 2 => continue,
                Err(e) => return Err(e),
            }
        }
    }
    /// Shadows share lease validation and the pinned transport with spill, but
    /// have no durable job or attach credentials in the motor store.
    pub(crate) async fn shadow_run(
        &self,
        id: Uuid,
        request: &RunRequest,
        salt: &[u8; 32],
        booked: &mut Option<Lease>,
    ) -> Result<Channel, Error> {
        let key = requester()?;
        let body = lease_body(
            id,
            request,
            (
                keyed_hash(salt, "ck-ws-v1", &request.workspace_key).map_err(|_| Error::Path)?,
                keyed_hash(salt, "ck-repo-v1", &request.repository_root)
                    .map_err(|_| Error::Path)?,
            ),
            None,
            self.route.avoid_host.clone(),
            RequesterJwk::from_key(&key.verifying_key()),
            Some(true),
        )?;
        // Extend only this request: older brokers may reject shadows, but normal
        // leases must retain their closed wire schema.
        let wire = ShadowLeaseRequest {
            request: &body,
            shadow: true,
        };
        if let Some(field) = invalid_lease_field(&body, true) {
            warn_lease_body(id, &body, "shadow", None, None, Some(field));
            return Err(Error::RequestInvalid(Some(field)));
        }
        let deadline = tokio::time::Instant::now() + SHADOW_PROVISIONING_BUDGET;
        let lease: Lease = self
            .post_ready_authorized(
                "/v1/leases",
                &wire,
                self.bearer().await?,
                deadline,
                Error::Provisioning(MIN_PROVISIONING_RETRY_MS),
            )
            .await
            .inspect_err(|_| {
                if let Some(status) = self.broker_client_error() {
                    warn_lease_body(
                        id,
                        &body,
                        "shadow",
                        Some(status),
                        self.broker_code().as_deref(),
                        None,
                    );
                }
            })?;
        *booked = Some(lease.clone());
        let claims = tls::claims(id, &lease, &key, motor_lease::Purpose::Run)?;
        if lease.ssh.is_none()
            || self.route.avoid_host.as_ref() == Some(&lease.placement.host)
            || resource_mismatch(&claims, &body, true).is_some()
            || lease.max_s < body.timeout_s
        {
            return Err(Error::Auth);
        }
        self.connect(id, &lease, &key, motor_lease::Purpose::Run)
            .await
            .map_err(|failure| {
                tracing::warn!(job_id = %id, step = failure.step, code = failure.error.code(), "shadow connection failed");
                failure.error
            })
    }
    async fn post_ready<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, Error> {
        self.post(path, body).await
    }
    /// Posts until the broker grants or refuses, waiting out `202
    /// provisioning` and routed `503 no_capacity` answers until `deadline`.
    /// When the deadline passes first, the request in flight is dropped and
    /// the refusal for the wait the deadline ended is returned: the broker's
    /// latest provisioning or capacity answer or, before the broker has sent
    /// one, `cut_off`, which describes the caller's own wait.
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
            match tokio::time::timeout_at(deadline, self.post_authorized(path, body, bearer)).await
            {
                Err(_) => return Err(pending),
                Ok(Err(error @ (Error::Provisioning(_) | Error::NoCapacity(_)))) => {
                    let retry_after_ms =
                        bounded_provisioning_retry_ms(error.retry_after_ms().unwrap());
                    pending = match error {
                        Error::NoCapacity(_) => Error::NoCapacity(retry_after_ms),
                        _ => Error::Provisioning(retry_after_ms),
                    };
                    let retry_at =
                        tokio::time::Instant::now() + Duration::from_millis(retry_after_ms);
                    // Held-clock tests must observe a parsed reply and a fixed
                    // wake time before advancing time; a server-side send is too early.
                    #[cfg(test)]
                    tracing::info!(path, "hosted broker retry wait");
                    if retry_at >= deadline {
                        tokio::time::sleep_until(deadline).await;
                        return Err(pending);
                    }
                    tokio::time::sleep_until(retry_at).await;
                    bearer = match tokio::time::timeout_at(deadline, self.bearer()).await {
                        Ok(result) => result?,
                        Err(_) => return Err(pending),
                    };
                }
                Ok(result) => return result,
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

#[cfg(test)]
mod shadow_wire_tests {
    use super::*;
    #[tokio::test]
    async fn shadow_lease_explicitly_allows_spill() {
        let http = test_http::Http::new(|path, body| {
            if path == "/v1/leases" {
                assert_eq!(body["allow_spill"], true);
                assert_eq!(body["shadow"], true);
                (402, serde_json::json!({"error":"budget_exhausted"}))
            } else {
                panic!("unexpected path: {path}")
            }
        })
        .await;
        std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
        let client = Client::new(HostedJobState {
            broker_url: http.url.clone(),
            token_source: BrokerTokenSource::Fake,
            ..HostedJobState::default()
        });
        let (_scratch, repo) = boat_tests::committed_repo();
        let path = repo.to_str().unwrap();
        let request = RunRequest::new(path, path, path, "true").with_timeout(30);
        let mut booked = None;
        assert_eq!(
            client
                .shadow_run(Uuid::now_v7(), &request, &[1; 32], &mut booked)
                .await
                .err(),
            Some(Error::Budget)
        );
        assert!(booked.is_none());
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }
    /// A shadow lease answered `no_host_fits` is skipped after one request:
    /// nothing is booked and the broker is not asked again. The broker's
    /// `max_vcpus` is kept when it is a number and read as unknown otherwise.
    #[tokio::test]
    async fn shadow_no_host_fits_is_one_request_with_or_without_max_vcpus() {
        for (reply, expected) in [
            (
                serde_json::json!({"error":"no_host_fits","max_vcpus":8}),
                Some(8),
            ),
            (serde_json::json!({"error":"no_host_fits"}), None),
            (
                serde_json::json!({"error":"no_host_fits","max_vcpus":"eight"}),
                None,
            ),
            (
                serde_json::json!({"error":"no_host_fits","max_vcpus":-1}),
                None,
            ),
        ] {
            let http = test_http::Http::new(move |path, _| {
                assert_eq!(path, "/v1/leases");
                (422, reply.clone())
            })
            .await;
            std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
            let client = Client::new(HostedJobState {
                broker_url: http.url.clone(),
                token_source: BrokerTokenSource::Fake,
                ..HostedJobState::default()
            });
            let (_scratch, repo) = boat_tests::committed_repo();
            let path = repo.to_str().unwrap();
            let request = RunRequest::new(path, path, path, "true")
                .with_timeout(30)
                .with_weight_hint(16);
            let mut booked = None;
            assert_eq!(
                client
                    .shadow_run(Uuid::now_v7(), &request, &[1; 32], &mut booked)
                    .await
                    .err(),
                Some(Error::NoHostFits(expected))
            );
            assert!(booked.is_none());
            assert_eq!(http.requests.lock().unwrap().len(), 1);
            assert_eq!(client.broker_code().as_deref(), Some("no_host_fits"));
        }
    }
    #[test]
    fn shadow_wire_omits_false_appends_true_and_preserves_ordinary_fields() {
        let request =
            RunRequest::new("/tmp/repo", "/tmp/repo", "/tmp/repo", "true").with_timeout(30);
        let ordinary = lease_body(
            Uuid::nil(),
            &request,
            ("a".repeat(64), "b".repeat(64)),
            None,
            Some("build-server".into()),
            RequesterJwk::from_key(&SigningKey::from_bytes(&[1; 32]).verifying_key()),
            Some(true),
        )
        .unwrap();
        let absent = serde_json::to_string(&ordinary).unwrap();
        let false_body = serde_json::to_string(&ShadowLeaseRequest {
            request: &ordinary,
            shadow: false,
        })
        .unwrap();
        assert_eq!(absent, false_body);
        let true_body = serde_json::to_string(&ShadowLeaseRequest {
            request: &ordinary,
            shadow: true,
        })
        .unwrap();
        assert_eq!(
            true_body,
            format!("{},\"shadow\":true}}", &absent[..absent.len() - 1])
        );
        let mut value: serde_json::Value = serde_json::from_str(&true_body).unwrap();
        assert_eq!(
            value.as_object_mut().unwrap().remove("shadow"),
            Some(serde_json::json!(true))
        );
        assert_eq!(value, serde_json::to_value(&ordinary).unwrap());
    }
    /// CLOUD's `shadow-lease.json` records a shadow lease's request body and
    /// lease hash. ck-motor builds the shadow body the way `shadow_run` does
    /// (no placement hint, spill allowed, then `shadow: true`); its members
    /// match the recorded body, and its wire bytes without `requester_jwk`
    /// are the hashed projection: the ordinary projection with
    /// `"shadow":true` appended after `allow_spill`.
    #[test]
    fn cloud_shadow_lease_body_and_hash_match_the_shadow_vector() {
        let vector: serde_json::Value = serde_json::from_str(include_str!(
            "../../motor-lease/contracts/cloud/shadow-lease.json"
        ))
        .unwrap();
        let expected = &vector["booked"]["request"]["body"];
        assert_eq!(expected, &vector["provisioning"]["request"]["body"]);
        let seed: [u8; 32] = vector["requester"]["test_only_seed_hex"]
            .as_str()
            .unwrap()
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let key = SigningKey::from_bytes(&seed);
        let mut request = RunRequest::new(
            "/home/test/repo",
            "/home/test/repo",
            "/home/test/repo",
            "true",
        )
        .with_platform(motor_protocol::Platform::Linux)
        .with_weight_hint(expected["vcpus"].as_u64().unwrap() as u32)
        .with_timeout(expected["timeout_s"].as_u64().unwrap());
        if expected["net"] == true {
            request.network = Some(motor_protocol::Network::Outbound);
        }
        let body = lease_body(
            Uuid::parse_str(expected["job_id"].as_str().unwrap()).unwrap(),
            &request,
            (
                expected["workspace_key_hash"].as_str().unwrap().into(),
                expected["repo_hash"].as_str().unwrap().into(),
            ),
            None,
            serde_json::from_value(expected["avoid_host"].clone()).unwrap(),
            RequesterJwk::from_key(&key.verifying_key()),
            Some(true),
        )
        .unwrap();
        let wire = ShadowLeaseRequest {
            request: &body,
            shadow: true,
        };
        assert_eq!(&serde_json::to_value(&wire).unwrap(), expected);
        let text = serde_json::to_string(&wire).unwrap();
        let requester = format!(
            ",\"requester_jwk\":{}",
            serde_json::to_string(&body.requester_jwk).unwrap()
        );
        assert_eq!(text.matches(&requester).count(), 1, "{text}");
        let canonical = text.replacen(&requester, "", 1);
        let ordinary = super::tests::canonical_lease_bytes(&body);
        assert_eq!(
            canonical,
            format!("{},\"shadow\":true}}", &ordinary[..ordinary.len() - 1])
        );
        assert!(canonical.ends_with(",\"timeout_s\":100,\"allow_spill\":true,\"shadow\":true}"));
        assert_eq!(
            motor_lease::sha256_hex(canonical.as_bytes()),
            vector["lease_hash"]
        );
    }
}
