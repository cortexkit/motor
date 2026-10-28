//! In-memory broker state machine for tests. HTTP transport lives in the binary.
use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_json::{json, Value};

use crate::*;

pub struct FakeConfig {
    pub worker_bearer: String,
    pub subject: String,
    pub placement: Placement,
    pub endpoint: String,
    pub endpoint_spki_sha256: String,
    pub kid: String,
    pub signing_key: SigningKey,
    pub announced_keys: Vec<JwksKey>,
}

#[derive(Clone, Default)]
pub struct Knobs {
    pub allowance_vcpu_ms: Option<u64>,
    pub lease_id: Option<String>,
    /// Refuse lease requests before creating or changing a reservation.
    pub refusal: Option<(u16, String)>,
    /// Shorten the granted runtime; the reservation follows the granted cap.
    pub max_s: Option<u64>,
    /// The most vCPUs any host behind this broker could ever hold. A new
    /// lease asking for more is answered 422 `no_host_fits` with this number
    /// as `max_vcpus`, before any reservation is made.
    pub max_vcpus: Option<u32>,
    /// Account subjects (`acct:<ULID>`) whose accounts are closed. A new lease
    /// for the configured subject is then refused 403 `account_closed`, while
    /// jobs it already holds keep their normal answers; heartbeat replies list
    /// the closed subjects among the tenants the host reports.
    pub closed_subjects: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerReply {
    pub status: u16,
    pub body: Value,
}
impl BrokerReply {
    fn json(status: u16, body: impl serde::Serialize) -> Self {
        Self {
            status,
            body: serde_json::to_value(body).expect("broker reply is JSON serializable"),
        }
    }
    pub fn error(status: u16, error: &str) -> Self {
        Self::json(status, json!({"error": error}))
    }
}

pub struct BrokerRequest<'a> {
    pub content_type: Option<&'a str>,
    pub method: &'a str,
    pub path: &'a str,
    pub bearer: Option<&'a str>,
    pub host_headers: Option<&'a HostHeaders>,
    pub body: &'a [u8],
}

struct Reservation {
    request: LeaseRequest,
    lease_id: String,
    max_s: u64,
    reserved: u64,
    run_iat: u64,
    original_iat: u64,
    state: JobState,
    elapsed_ms: u64,
    release: Option<ReleaseReply>,
    settlement: Option<Settlement>,
}

enum Settlement {
    /// Settled by the placement host's usage report at `settled_at_ms`.
    Usage {
        body: Vec<u8>,
        reply: UsageReply,
        settled_at_ms: u64,
    },
    HostLost(UsageReply),
    Released(UsageReply),
}

/// How long after a usage-report settlement a reattach still gets an `attach`
/// token instead of 409 `job_finished`: 86400 s, as the runners contract states.
pub const SETTLED_REATTACH_WINDOW_MS: u64 = 86_400_000;

pub struct FakeBroker {
    config: FakeConfig,
    jobs: BTreeMap<String, Reservation>,
    auth: HostAuthenticator,
    pub knobs: Knobs,
}

impl FakeBroker {
    /// Refusing `main` is mandatory even when this model is embedded in a test server.
    pub fn new(runner_instance: &str, config: FakeConfig) -> Result<Self, &'static str> {
        if runner_instance == "main" || runner_instance.is_empty() {
            return Err("fake broker is forbidden for the main runner instance");
        }
        Ok(Self {
            config,
            jobs: BTreeMap::new(),
            auth: HostAuthenticator::default(),
            knobs: Knobs::default(),
        })
    }

    pub fn register_host(&mut self, host: String, key: VerifyingKey) {
        self.auth.register(host, key);
    }

    pub fn jwks(&self) -> Jwks {
        let mut keys = vec![JwksKey::from_key(
            &self.config.kid,
            &self.config.signing_key.verifying_key(),
        )];
        keys.extend(self.config.announced_keys.clone());
        Jwks { keys }
    }

    /// `now_ms` is supplied by the transport or a deterministic test clock.
    pub fn handle(&mut self, request: BrokerRequest<'_>, now_ms: u64) -> BrokerReply {
        if request.path.split_once('?').is_some_and(|(path, _)| {
            path.strip_prefix("/v1/hosts/")
                .and_then(|p| p.split_once('/'))
                .is_some_and(|(_, route)| matches!(route, "heartbeat" | "usage"))
        }) {
            return BrokerReply::error(400, "bad_request");
        }
        if request.method == "GET" && request.path == "/.well-known/jwks.json" {
            return BrokerReply::json(200, self.jwks());
        }
        if request.method != "POST" {
            return BrokerReply::error(404, "not_found");
        }
        let client_job = request.path.strip_prefix("/v1/leases/").and_then(|path| {
            path.strip_suffix("/release")
                .or_else(|| path.strip_suffix("/reattach"))
        });
        if request.path == "/v1/leases" || client_job.is_some() {
            if request.bearer != Some(self.config.worker_bearer.as_str()) {
                return BrokerReply::error(401, "unauthorized");
            }
            if !json_request(&request) || client_job.is_some_and(|id| !valid_job_id(id)) {
                return BrokerReply::error(400, "bad_request");
            }
            self.settle_due(now_ms);
            if request.path == "/v1/leases" {
                return match serde_json::from_slice(request.body) {
                    Ok(body) => self.lease(body, now_ms / 1000),
                    Err(_) => BrokerReply::error(400, "bad_request"),
                };
            }
            if let Some(job_id) = request
                .path
                .strip_prefix("/v1/leases/")
                .and_then(|p| p.strip_suffix("/release"))
            {
                return match serde_json::from_slice::<ReleaseRequest>(request.body) {
                    Ok(_) => self.release(job_id),
                    Err(_) => BrokerReply::error(400, "bad_request"),
                };
            }
            if let Some(job_id) = request
                .path
                .strip_prefix("/v1/leases/")
                .and_then(|p| p.strip_suffix("/reattach"))
            {
                return match serde_json::from_slice::<ReattachRequest>(request.body) {
                    Ok(body) => self.reattach(job_id, body, now_ms),
                    Err(_) => BrokerReply::error(400, "bad_request"),
                };
            }
            return BrokerReply::error(404, "not_found");
        }
        if let Some((host_id, route)) = request
            .path
            .strip_prefix("/v1/hosts/")
            .and_then(|p| p.split_once('/'))
        {
            if !matches!(route, "heartbeat" | "usage") {
                return BrokerReply::error(404, "not_found");
            }
            if !valid_name(host_id) || !json_request(&request) {
                return BrokerReply::error(400, "bad_request");
            }
            let Some(headers) = request.host_headers else {
                return BrokerReply::error(401, "host_auth_failed");
            };
            if headers.host != host_id
                || self
                    .auth
                    .verify(request.method, request.path, request.body, headers, now_ms)
                    .is_err()
            {
                return BrokerReply::error(401, "host_auth_failed");
            }
            self.settle_due(now_ms);
            return match route {
                "heartbeat" => match serde_json::from_slice(request.body) {
                    Ok(body) => self.heartbeat(host_id, body),
                    Err(_) => BrokerReply::error(400, "bad_request"),
                },
                "usage" => match serde_json::from_slice(request.body) {
                    Ok(body) => self.usage(host_id, body, now_ms),
                    Err(_) => BrokerReply::error(400, "bad_request"),
                },
                _ => BrokerReply::error(404, "not_found"),
            };
        }
        BrokerReply::error(404, "not_found")
    }

    fn lease(&mut self, request: LeaseRequest, now_s: u64) -> BrokerReply {
        // The broker requires an explicit spill policy on every lease body and
        // refuses one without it before looking up or claiming the job.
        if request.allow_spill.is_none()
            || request.requester_jwk.verifying_key().is_err()
            || !valid_job_id(&request.job_id)
            || !(1..=64).contains(&request.vcpus)
            || !(10..=21600).contains(&request.timeout_s)
            || !valid_name(&request.platform)
            || request.placement_hint.as_ref().is_some_and(|p| {
                !valid_name(&p.host) || p.vm.as_deref().is_none_or(|vm| !valid_name(vm))
            })
            || !valid_keyed_hash(&request.workspace_key_hash)
            || !valid_keyed_hash(&request.repo_hash)
            || request
                .avoid_host
                .as_deref()
                .is_some_and(|h| !valid_name(h))
        {
            return BrokerReply::error(400, "bad_request");
        }
        if let Some(job) = self.jobs.get(&request.job_id) {
            if job.release.is_some() {
                return BrokerReply::error(409, "job_released");
            }
            if job.settlement.is_some() {
                return BrokerReply::error(409, "job_finished");
            }
            if job.state != JobState::Reserved {
                return BrokerReply::error(409, "job_admitted");
            }
            if job.release.is_some() {
                return BrokerReply::error(409, "job_released");
            }
            if lease_identity(&request) != lease_identity(&job.request) {
                return BrokerReply::error(409, "job_conflict");
            }
        } else {
            // A closed account gets no new job; one it already holds (the
            // branch above) keeps its answer so it can run to its end.
            if self.knobs.closed_subjects.contains(&self.config.subject) {
                return BrokerReply::error(403, "account_closed");
            }
            if let Some((status, code)) = &self.knobs.refusal {
                return BrokerReply::error(*status, code);
            }
            if let Some(max_vcpus) = self.knobs.max_vcpus.filter(|max| request.vcpus > *max) {
                return BrokerReply::json(
                    422,
                    json!({"error": "no_host_fits", "max_vcpus": max_vcpus}),
                );
            }
            // The fake serves a single host. When the lease avoids it, nothing
            // else is selectable, which the contract answers as no capacity.
            if request.avoid_host.as_deref() == Some(self.config.placement.host.as_str()) {
                return BrokerReply::error(503, "no_capacity");
            }
            let used: u64 = self
                .jobs
                .values()
                .map(|job| match &job.settlement {
                    None => job.reserved,
                    Some(
                        Settlement::Usage { reply, .. }
                        | Settlement::HostLost(reply)
                        | Settlement::Released(reply),
                    ) => reply.settled_vcpu_ms,
                })
                .sum();
            let mut max_s = self
                .knobs
                .max_s
                .unwrap_or(request.timeout_s)
                .min(request.timeout_s);
            if let Some(allowance) = self.knobs.allowance_vcpu_ms {
                max_s =
                    max_s.min(allowance.saturating_sub(used) / (u64::from(request.vcpus) * 1000));
                if max_s < 10 {
                    return BrokerReply::error(402, "budget_exhausted");
                }
            }
            let Some(reserved) = u64::from(request.vcpus)
                .checked_mul(max_s)
                .and_then(|n| n.checked_mul(1000))
            else {
                return BrokerReply::error(400, "bad_request");
            };
            self.jobs.insert(
                request.job_id.clone(),
                Reservation {
                    request: request.clone(),
                    lease_id: self
                        .knobs
                        .lease_id
                        .clone()
                        .unwrap_or_else(|| request.job_id.clone()),
                    max_s,
                    reserved,
                    run_iat: now_s,
                    original_iat: now_s,
                    state: JobState::Reserved,
                    elapsed_ms: 0,
                    release: None,
                    settlement: None,
                },
            );
        }
        self.issue(
            &request.job_id,
            &request.requester_jwk,
            Purpose::Run,
            now_s,
            201,
        )
    }

    fn reattach(&mut self, job_id: &str, request: ReattachRequest, now_ms: u64) -> BrokerReply {
        if request.requester_jwk.verifying_key().is_err() {
            return BrokerReply::error(400, "bad_request");
        }
        let Some(job) = self.jobs.get(job_id) else {
            return BrokerReply::error(404, "job_unknown");
        };
        if job.release.is_some() {
            return BrokerReply::error(409, "job_released");
        }
        match &job.settlement {
            None => {}
            // The host keeps a job it settled by usage report, with its output
            // and terminal record, for a day, so a client that lost the end of
            // the stream can attach and have it replayed. This fake serves one
            // registered host and no Boat sandboxes, so every usage-report
            // settlement here is a registered host's.
            Some(Settlement::Usage { settled_at_ms, .. })
                if settled_at_ms.saturating_add(SETTLED_REATTACH_WINDOW_MS) > now_ms => {}
            Some(_) => return BrokerReply::error(409, "job_finished"),
        }
        self.issue(
            job_id,
            &request.requester_jwk,
            Purpose::Attach,
            now_ms / 1000,
            200,
        )
    }

    fn release(&mut self, job_id: &str) -> BrokerReply {
        let Some(job) = self.jobs.get_mut(job_id) else {
            return BrokerReply::error(404, "job_unknown");
        };
        if job.settlement.is_some() {
            return BrokerReply::error(409, "job_finished");
        }
        if job.state != JobState::Reserved {
            return BrokerReply::error(409, "job_admitted");
        }
        // Only run-token expiry matters: attach tokens cannot admit execution.
        let reply = job.release.get_or_insert_with(|| ReleaseReply {
            job_id: job_id.into(),
            release_at_ms: job
                .run_iat
                .saturating_add(MAX_TOKEN_LIFETIME_S)
                .saturating_mul(1000)
                .saturating_add(CLOCK_SKEW_S * 1000),
        });
        BrokerReply::json(202, reply)
    }

    fn issue(
        &mut self,
        job_id: &str,
        requester: &RequesterJwk,
        purpose: Purpose,
        now_s: u64,
        status: u16,
    ) -> BrokerReply {
        let job = self.jobs.get_mut(job_id).expect("reservation exists");
        let iat = now_s;
        let Some(exp) = iat
            .checked_add(MAX_TOKEN_LIFETIME_S)
            .filter(|exp| exp.checked_mul(1000).is_some())
        else {
            return BrokerReply::error(400, "bad_request");
        };
        let claims = Claims {
            iss: ISSUER.into(),
            aud: self.config.placement.host.clone(),
            jti: job_id.into(),
            sub: self.config.subject.clone(),
            purpose,
            iat,
            exp,
            vcpus: job.request.vcpus,
            mem_mib: memory_max_mib(job.request.vcpus),
            platform: job.request.platform.clone(),
            net: job.request.net,
            placement: self.config.placement.clone(),
            max_s: job.max_s,
            cnf: Confirmation {
                jkt: requester.thumbprint().expect("validated requester"),
            },
        };
        let Ok(job_token) = sign_token(&claims, &self.config.kid, &self.config.signing_key) else {
            return BrokerReply::error(500, "internal");
        };
        if purpose == Purpose::Run {
            job.run_iat = iat;
        }
        BrokerReply::json(
            status,
            LeaseResponse {
                lease_id: job.lease_id.clone(),
                job_id: job_id.into(),
                placement: self.config.placement.clone(),
                endpoint: self.config.endpoint.clone(),
                endpoint_spki_sha256: self.config.endpoint_spki_sha256.clone(),
                job_token,
                token_expires_at_ms: exp * 1000,
                max_s: job.max_s,
                reserved_vcpu_ms: job.reserved,
            },
        )
    }

    fn heartbeat(&mut self, host: &str, heartbeat: Heartbeat) -> BrokerReply {
        if heartbeat.free_vcpus > MAX_SAFE_INTEGER
            || heartbeat.free_mem_mib > MAX_SAFE_INTEGER
            || heartbeat.queued > MAX_SAFE_INTEGER
            || heartbeat.warm.len() > 64
            || heartbeat.running.len() > 64
            || heartbeat
                .running
                .iter()
                .any(|job| !valid_job_id(&job.job_id) || job.elapsed_ms > MAX_SAFE_INTEGER)
            || heartbeat.warm.iter().any(|vm| {
                !valid_name(&vm.vm)
                    || !valid_keyed_hash(&vm.workspace_key_hash)
                    || !crate::auth::valid_subject(&vm.tenant)
            })
        {
            return BrokerReply::error(400, "bad_request");
        }
        // The closed accounts among the tenants this heartbeat reports: each
        // warm entry's tenant, and the owner of each running job the broker
        // knows, whether or not that job is ignored. Sorted and unique.
        let mut closed: BTreeSet<&String> = heartbeat
            .warm
            .iter()
            .map(|vm| &vm.tenant)
            .filter(|tenant| self.knobs.closed_subjects.contains(*tenant))
            .collect();
        if self.knobs.closed_subjects.contains(&self.config.subject)
            && heartbeat
                .running
                .iter()
                .any(|job| self.jobs.contains_key(&job.job_id))
        {
            closed.insert(&self.config.subject);
        }
        let closed_subjects: Vec<String> = closed.into_iter().cloned().collect();
        let mut ignored = Vec::new();
        for running in heartbeat.running {
            if host != self.config.placement.host {
                ignored.push(running.job_id);
                continue;
            }
            match self.jobs.get_mut(&running.job_id) {
                Some(job) if job.settlement.is_none() => {
                    // Once Admitted or Started, returning to Reserved would wrongly
                    // allow a lease retry to issue another run token.
                    if job.state == JobState::Reserved || running.state == JobState::Started {
                        job.state = running.state;
                    }
                    if running.state != JobState::Reserved {
                        job.release = None;
                    }
                    job.elapsed_ms = job.elapsed_ms.max(running.elapsed_ms.min(job.max_s * 1000));
                }
                Some(_) => {}
                None => ignored.push(running.job_id),
            }
        }
        BrokerReply::json(
            200,
            json!({"ignored": ignored, "closed_subjects": closed_subjects}),
        )
    }

    fn usage(&mut self, host: &str, usage: Usage, now_ms: u64) -> BrokerReply {
        if !valid_job_id(&usage.job_id)
            || !(1..=64).contains(&usage.vcpus)
            || usage.started_at_ms > MAX_SAFE_INTEGER
            || usage.ended_at_ms > MAX_SAFE_INTEGER
            || usage.ended_at_ms < usage.started_at_ms
            || usage.mem_peak_mib.is_some_and(|v| v > MAX_SAFE_INTEGER)
            || usage.egress_bytes.is_some_and(|v| v > MAX_SAFE_INTEGER)
        {
            return BrokerReply::error(400, "bad_request");
        }
        let body = serde_json::to_vec(&usage).expect("usage is serializable");
        let Some(job) = self
            .jobs
            .get_mut(&usage.job_id)
            .filter(|_| host == self.config.placement.host)
        else {
            return BrokerReply::error(404, "job_unknown");
        };
        match &job.settlement {
            Some(Settlement::HostLost(_) | Settlement::Released(_)) => {
                return BrokerReply::error(409, "job_settled")
            }
            Some(Settlement::Usage {
                body: original,
                reply,
                ..
            }) => {
                return if original == &body {
                    BrokerReply::json(200, reply)
                } else {
                    BrokerReply::error(409, "usage_conflict")
                };
            }
            None => {}
        }
        if usage.vcpus != job.request.vcpus || usage.ended_at_ms < usage.started_at_ms {
            return BrokerReply::error(400, "bad_request");
        }
        let settled = u64::from(usage.vcpus)
            .saturating_mul(usage.ended_at_ms - usage.started_at_ms)
            .min(job.reserved);
        let reply = UsageReply {
            job_id: usage.job_id,
            settled_vcpu_ms: settled,
            released_vcpu_ms: job.reserved - settled,
        };
        job.settlement = Some(Settlement::Usage {
            body,
            reply: reply.clone(),
            settled_at_ms: now_ms,
        });
        BrokerReply::json(200, reply)
    }

    pub fn settle_due(&mut self, now_ms: u64) {
        for (job_id, job) in &mut self.jobs {
            if job.settlement.is_some() {
                continue;
            }
            if job
                .release
                .as_ref()
                .is_some_and(|release| now_ms >= release.release_at_ms)
            {
                job.settlement = Some(Settlement::Released(UsageReply {
                    job_id: job_id.clone(),
                    settled_vcpu_ms: 0,
                    released_vcpu_ms: job.reserved,
                }));
                continue;
            }
            let deadline_ms = job
                .original_iat
                .saturating_add(job.max_s)
                .saturating_add(600)
                .saturating_mul(1000);
            if now_ms >= deadline_ms {
                let settled = u64::from(job.request.vcpus)
                    .saturating_mul(job.elapsed_ms)
                    .min(job.reserved);
                job.settlement = Some(Settlement::HostLost(UsageReply {
                    job_id: job_id.clone(),
                    settled_vcpu_ms: settled,
                    released_vcpu_ms: job.reserved - settled,
                }));
            }
        }
    }

    /// Inspect settlement without adding a non-contract HTTP route.
    pub fn settlement(&self, job_id: &str) -> Option<(&UsageReply, &'static str)> {
        match self.jobs.get(job_id)?.settlement.as_ref()? {
            Settlement::Usage { reply, .. } => Some((reply, "usage")),
            Settlement::HostLost(reply) => Some((reply, "host_lost")),
            Settlement::Released(reply) => Some((reply, "released")),
        }
    }
}

fn json_request(request: &BrokerRequest<'_>) -> bool {
    request.body.len() <= 65536
        && request.content_type.is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
}

fn valid_keyed_hash(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The ordinary lease-hash projection: requester keys can rotate on a re-request,
/// but placement and resource choices (including the allow_spill value) cannot.
/// `lease` refuses a body without allow_spill before this is used, so the member
/// is always present here.
fn lease_identity(request: &LeaseRequest) -> Value {
    let mut identity = json!({
        "job_id": request.job_id,
        "vcpus": request.vcpus,
        "platform": request.platform,
        "net": request.net,
        "workspace_key_hash": request.workspace_key_hash,
        "repo_hash": request.repo_hash,
        "placement_hint": request.placement_hint,
        "avoid_host": request.avoid_host,
        "timeout_s": request.timeout_s,
    });
    if let Some(allow_spill) = request.allow_spill {
        identity["allow_spill"] = json!(allow_spill);
    }
    identity
}
