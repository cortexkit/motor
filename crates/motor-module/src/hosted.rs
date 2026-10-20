//! Broker credentials and saved routes are independent of the build-server SSH
//! connector. Hosted jobs keep their route even after hosted routing is disabled.
pub(crate) mod account;
pub(crate) mod check;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingStatus {
    #[default]
    Pending,
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    #[serde(default)]
    pub(crate) lease_request: Option<LeaseRequest>,
    /// Set durably just before ck-motor sends the Run frame to a hosted
    /// runner. Until then no runner holds the command, so whatever happened to
    /// the lease, the job provably never ran; afterwards it may have.
    #[serde(default)]
    pub(crate) run_sent: bool,
    pub(crate) broker_url: String,
    pub(crate) token_source: BrokerTokenSource,
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
    InternalFailure,
    Connect,
    Broker,
    NoTimeout,
    Short,
    Path,
    Admitted,
    Finished,
    Rejected,
    Released,
    Budget,
    Platform,
    AccountNotFound,
    AccountAuthRequired,
    AccountTransient(Option<u64>),
    AccountPermanent,
    AccountInvalid,
}
const MIN_PROVISIONING_RETRY_MS: u64 = 1_000;
const MAX_PROVISIONING_RETRY_MS: u64 = 30_000;
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
            Self::Capacity => "hosted_capacity",
            Self::InternalFailure => "hosted_internal_failure",
            Self::Connect => "hosted_connect_failed",
            Self::Broker => "hosted_broker_unreachable",
            Self::NoTimeout => "hosted_no_timeout",
            Self::Short => "hosted_max_s_too_short",
            Self::Path => "hosted_path_outside_roots",
            Self::Admitted => "hosted_job_admitted",
            Self::Finished => "hosted_job_finished",
            Self::Rejected => "lease_mismatch",
            Self::Released => "hosted_job_released",
            Self::Budget => "hosted_budget_exhausted",
            Self::Platform => "hosted_platform_unsupported",
            Self::AccountNotFound => "hosted_account_not_found",
            Self::AccountAuthRequired => "hosted_account_auth_required",
            Self::AccountTransient(_) => "hosted_account_transient",
            Self::AccountPermanent => "hosted_account_permanent",
            Self::AccountInvalid => "hosted_account_invalid",
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
            Self::HostFull | Self::Capacity => RefusalReason::RunnerFull,
            Self::Connect | Self::Broker | Self::AccountTransient(_) | Self::Provisioning(_) => {
                RefusalReason::Unreachable
            }
            Self::Platform => RefusalReason::PlatformUnsupported,
            _ => RefusalReason::RequestRejected,
        }
    }
    pub(crate) fn detail(self) -> String {
        if self == Self::AccountAuthRequired {
            format!("{}: a person must run `ck account login`", self.code())
        } else {
            self.code().into()
        }
    }
    pub(crate) fn retry_after_ms(self) -> Option<u64> {
        match self {
            Self::AccountTransient(delay) => delay,
            Self::Provisioning(delay) => Some(delay),
            _ => None,
        }
    }
    fn replaceable(self) -> bool {
        matches!(self, Self::HostFull | Self::Capacity | Self::Connect)
    }
    fn from_host(reason: &str) -> Self {
        match reason {
            "host_full" => Self::HostFull,
            "capacity" => Self::Capacity,
            "internal_failure" => Self::InternalFailure,
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
pub(crate) fn validate(request: &RunRequest) -> Result<(), Error> {
    if request.timeout.is_none_or(|s| s == 0) {
        return Err(Error::NoTimeout);
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
    job.queue_wait_limit_s
        .saturating_mul(1000)
        .saturating_sub(waited_ms)
}
/// The smallest memory per leased vCPU a Boat token may state. CLOUD's
/// contract makes a Boat token's `mem_mib` the chosen sandbox size's measured
/// usable memory (13444 MiB for the 8-vCPU large size in production, 14336 MiB
/// in the recorded test size), and the chosen size has at least the job's
/// vCPUs, so a real token states well over 1.5 GiB per leased vCPU. Below
/// 1 GiB per vCPU the token is malformed or names a sandbox in which builds
/// would be killed for memory, so ck-motor refuses it rather than run there.
const BOAT_MIN_MEM_MIB_PER_VCPU: u64 = 1024;

/// The first leased resource whose token claim differs from what ck-motor
/// requested, as (field, expected, actual). A first-party host's token states
/// the runner's per-job memory cap exactly. A Boat token states the whole
/// sandbox's usable memory, which may be below that cap; it must still lie
/// between the floor above and the cap.
fn resource_mismatch(
    claims: &motor_lease::Claims,
    body: &LeaseRequest,
    boat: bool,
) -> Option<(&'static str, String, String)> {
    let cap = motor_lease::memory_max_mib(body.vcpus);
    let floor = u64::from(body.vcpus).saturating_mul(BOAT_MIN_MEM_MIB_PER_VCPU);
    let memory_ok = if boat {
        (floor..=cap).contains(&claims.mem_mib)
    } else {
        claims.mem_mib == cap
    };
    if claims.vcpus != body.vcpus {
        Some(("vcpus", body.vcpus.to_string(), claims.vcpus.to_string()))
    } else if !memory_ok {
        let expected = if boat {
            format!("{floor}..={cap}")
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
fn lease_body(
    id: Uuid,
    request: &RunRequest,
    hashes: (String, String),
    hint: Option<Placement>,
    requester_jwk: RequesterJwk,
) -> Result<LeaseRequest, Error> {
    Ok(LeaseRequest {
        job_id: id.to_string(),
        vcpus: request.weight_hint.unwrap_or(2),
        platform: lease_platform(request.platform.as_ref())?.into(),
        net: request.network.is_some(),
        workspace_key_hash: hashes.0,
        repo_hash: hashes.1,
        placement_hint: hint.filter(|placement| placement.vm.is_some()),
        timeout_s: request.timeout.ok_or(Error::NoTimeout)?,
        requester_jwk,
    })
}
fn reattach_body(key: &SigningKey) -> ReattachRequest {
    ReattachRequest {
        requester_jwk: RequesterJwk::from_key(&key.verifying_key()),
    }
}

pub(crate) struct Client {
    http: reqwest::Client,
    route: HostedJobState,
    account_tokens: Option<Arc<account::AccountTokens>>,
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
        }
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
        let bearer = self.bearer().await?;
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
        if matches!(reply.status().as_u16(), 401 | 403) {
            return Err(Error::Auth);
        }
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
            return Err(
                match code
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                {
                    Some("job_admitted") => Error::Admitted,
                    Some("job_finished") => Error::Finished,
                    Some("capacity" | "no_capacity") => Error::Capacity,
                    Some("host_full") => Error::HostFull,
                    Some("job_released") => Error::Released,
                    Some("budget_exhausted") => Error::Budget,
                    Some("platform_unsupported") => Error::Platform,
                    Some("sandbox_lost") if status == 409 => Error::SandboxLost,
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
        reply.json().await.map_err(|_| Error::Broker)
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
                RequesterJwk::from_key(&requester()?.verifying_key()),
            )?,
        };
        storage
            .hosted_update(id, |state| state.lease_request = Some(body.clone()))
            .map_err(|_| Error::Broker)?;
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
                    return Err(Error::Provisioning(MIN_PROVISIONING_RETRY_MS));
                }
            }
            let attempt = state.placement_attempts;
            let spilled = state.spill_wait_ms.is_some();
            let key = requester()?;
            body.requester_jwk = RequesterJwk::from_key(&key.verifying_key());
            // Acquire credentials before recording this attempt: a vault refusal
            // means no HTTP request was sent to create a reservation for it.
            let bearer = match tokio::time::timeout_at(provisioning_deadline, self.bearer()).await {
                Ok(result) => result?,
                Err(_) => return Err(Error::Provisioning(MIN_PROVISIONING_RETRY_MS)),
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
                .post_ready_authorized("/v1/leases", &body, bearer, provisioning_deadline)
                .await
            {
                Ok(l) => l,
                Err(error) => {
                    tracing::info!(job_id = %id, attempt, code = error.code(), "hosted lease not granted");
                    match error {
                        // A broker without capacity for a spilled job will not
                        // gain it within seconds (its Boat start cap is hourly);
                        // the build server is the better place to retry.
                        Error::Capacity if spilled => return Err(error),
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
                Err(error) => {
                    tracing::info!(job_id = %id, attempt, code = error.code(), "hosted connect failed")
                }
            }
            match connected {
                Ok(channel) => return Ok((channel, lease)),
                Err(e) if lease.ssh.is_some() => return Err(e),
                Err(e) if e.replaceable() && attempt < 2 => continue,
                Err(e) => return Err(e),
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
    async fn post_ready_authorized<T: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        mut bearer: String,
        deadline: tokio::time::Instant,
    ) -> Result<R, Error> {
        let mut retry_after_ms = MIN_PROVISIONING_RETRY_MS;
        loop {
            match tokio::time::timeout_at(deadline, self.post_authorized(path, body, bearer)).await
            {
                Err(_) => return Err(Error::Provisioning(retry_after_ms)),
                Ok(Err(Error::Provisioning(ms))) => {
                    retry_after_ms = bounded_provisioning_retry_ms(ms);
                    let retry_at =
                        tokio::time::Instant::now() + Duration::from_millis(retry_after_ms);
                    if retry_at >= deadline {
                        tokio::time::sleep_until(deadline).await;
                        return Err(Error::Provisioning(retry_after_ms));
                    }
                    tokio::time::sleep_until(retry_at).await;
                    bearer = match tokio::time::timeout_at(deadline, self.bearer()).await {
                        Ok(result) => result?,
                        Err(_) => return Err(Error::Provisioning(retry_after_ms)),
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
    ) -> Result<Channel, Error> {
        if lease.ssh.is_some() {
            let claims = tls::claims(id, lease, key, purpose)?;
            tokio::time::timeout(Duration::from_secs(60), async {
                let pipe = ssh::connect(lease, key).await?;
                tls::authenticate(pipe, lease, key, &claims).await
            })
            .await
            .unwrap_or(Err(Error::Connect))
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
#[cfg(test)]
pub(crate) mod tests;
