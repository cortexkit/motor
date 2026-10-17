//! In-memory broker state machine for tests. HTTP transport lives in the binary.
use std::collections::BTreeMap;

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
    /// Refuse lease requests before creating or changing a reservation.
    pub refusal: Option<(u16, String)>,
    /// Shorten the granted runtime without changing the requested reservation.
    pub max_s: Option<u64>,
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
    last_token_iat: u64,
    run_iat: u64,
    state: JobState,
    elapsed_ms: u64,
    settlement: Option<Settlement>,
}

enum Settlement {
    Usage { body: Vec<u8>, reply: UsageReply },
    HostLost(UsageReply),
}

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
        self.settle_host_lost(now_ms);
        if request.method == "GET" && request.path == "/.well-known/jwks.json" {
            return BrokerReply::json(200, self.jwks());
        }
        if request.method != "POST" {
            return BrokerReply::error(404, "not_found");
        }
        if request.path == "/v1/leases" || request.path.starts_with("/v1/leases/") {
            if request.bearer != Some(self.config.worker_bearer.as_str()) {
                return BrokerReply::error(401, "auth");
            }
            if request.path == "/v1/leases" {
                return match serde_json::from_slice(request.body) {
                    Ok(body) => self.lease(body, now_ms / 1000),
                    Err(_) => BrokerReply::error(400, "invalid_request"),
                };
            }
            if let Some(job_id) = request
                .path
                .strip_prefix("/v1/leases/")
                .and_then(|p| p.strip_suffix("/reattach"))
            {
                return match serde_json::from_slice::<ReattachRequest>(request.body) {
                    Ok(body) => self.reattach(job_id, body, now_ms / 1000),
                    Err(_) => BrokerReply::error(400, "invalid_request"),
                };
            }
            return BrokerReply::error(404, "not_found");
        }
        if let Some((host_id, route)) = request
            .path
            .strip_prefix("/v1/hosts/")
            .and_then(|p| p.split_once('/'))
        {
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
            return match route {
                "heartbeat" => match serde_json::from_slice(request.body) {
                    Ok(body) => self.heartbeat(host_id, body),
                    Err(_) => BrokerReply::error(400, "invalid_request"),
                },
                "usage" => match serde_json::from_slice(request.body) {
                    Ok(body) => self.usage(host_id, body, request.body),
                    Err(_) => BrokerReply::error(400, "invalid_request"),
                },
                _ => BrokerReply::error(404, "not_found"),
            };
        }
        BrokerReply::error(404, "not_found")
    }

    fn lease(&mut self, request: LeaseRequest, now_s: u64) -> BrokerReply {
        if let Some((status, code)) = &self.knobs.refusal {
            return BrokerReply::error(*status, code);
        }
        if request.requester_jwk.verifying_key().is_err()
            || request.job_id.is_empty()
            || request.job_id.contains('/')
        {
            return BrokerReply::error(400, "invalid_request");
        }
        if let Some(job) = self.jobs.get(&request.job_id) {
            if job.settlement.is_some() {
                return BrokerReply::error(409, "job_finished");
            }
            if job.state != JobState::Reserved {
                return BrokerReply::error(409, "job_admitted");
            }
            let mut comparable = request.clone();
            comparable.requester_jwk = job.request.requester_jwk.clone();
            if comparable != job.request {
                return BrokerReply::error(409, "job_conflict");
            }
        } else {
            if request.vcpus == 0
                || request.mem_mib != memory_max_mib(request.vcpus)
                || request.timeout_s == 0
                || request.platform.is_empty()
            {
                return BrokerReply::error(400, "invalid_request");
            }
            let Some(reserved) = u64::from(request.vcpus)
                .checked_mul(request.timeout_s.max(10))
                .and_then(|n| n.checked_mul(1000))
            else {
                return BrokerReply::error(400, "invalid_request");
            };
            self.jobs.insert(
                request.job_id.clone(),
                Reservation {
                    request: request.clone(),
                    lease_id: format!("fake-{}", request.job_id),
                    max_s: self
                        .knobs
                        .max_s
                        .unwrap_or(request.timeout_s)
                        .min(request.timeout_s),
                    reserved,
                    last_token_iat: now_s.saturating_sub(1),
                    run_iat: now_s,
                    state: JobState::Reserved,
                    elapsed_ms: 0,
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

    fn reattach(&mut self, job_id: &str, request: ReattachRequest, now_s: u64) -> BrokerReply {
        let Some(job) = self.jobs.get(job_id) else {
            return BrokerReply::error(404, "job_unknown");
        };
        if job.settlement.is_some() {
            return BrokerReply::error(409, "job_finished");
        }
        if request.requester_jwk.verifying_key().is_err() {
            return BrokerReply::error(400, "invalid_request");
        }
        self.issue(job_id, &request.requester_jwk, Purpose::Attach, now_s, 200)
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
        let iat = now_s.max(job.last_token_iat.saturating_add(1));
        let Some(exp) = iat
            .checked_add(300)
            .filter(|exp| exp.checked_mul(1000).is_some())
        else {
            return BrokerReply::error(400, "invalid_request");
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
            mem_mib: job.request.mem_mib,
            platform: job.request.platform.clone(),
            net: job.request.net,
            placement: self.config.placement.clone(),
            max_s: job.max_s,
            cnf: Confirmation {
                jkt: requester.thumbprint().expect("validated requester"),
            },
        };
        let Ok(job_token) = sign_token(&claims, &self.config.kid, &self.config.signing_key) else {
            return BrokerReply::error(500, "signing_failed");
        };
        job.last_token_iat = iat;
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
                    job.elapsed_ms = running.elapsed_ms;
                }
                _ => ignored.push(running.job_id),
            }
        }
        BrokerReply::json(200, json!({"ignored": ignored}))
    }

    fn usage(&mut self, host: &str, usage: Usage, body: &[u8]) -> BrokerReply {
        let Some(job) = self
            .jobs
            .get_mut(&usage.job_id)
            .filter(|_| host == self.config.placement.host)
        else {
            return BrokerReply::error(404, "job_unknown");
        };
        match &job.settlement {
            Some(Settlement::HostLost(_)) => return BrokerReply::error(409, "job_settled"),
            Some(Settlement::Usage {
                body: original,
                reply,
            }) => {
                return if original == body {
                    BrokerReply::json(200, reply)
                } else {
                    BrokerReply::error(409, "usage_conflict")
                };
            }
            None => {}
        }
        if usage.vcpus != job.request.vcpus || usage.ended_at_ms < usage.started_at_ms {
            return BrokerReply::error(400, "invalid_request");
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
            body: body.to_vec(),
            reply: reply.clone(),
        });
        BrokerReply::json(200, reply)
    }

    pub fn settle_host_lost(&mut self, now_ms: u64) {
        for (job_id, job) in &mut self.jobs {
            let deadline_ms = job
                .run_iat
                .saturating_add(job.max_s)
                .saturating_add(600)
                .saturating_mul(1000);
            if job.settlement.is_none() && now_ms >= deadline_ms {
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
        }
    }
}
