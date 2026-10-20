#![allow(dead_code)]
use ed25519_dalek::SigningKey;
use motor_lease::fake::{BrokerReply, BrokerRequest, FakeBroker, FakeConfig};
use motor_lease::*;
use serde::Serialize;

pub const NOW: u64 = 1_700_000_000_000;
pub const HOST: &str = "test-host";
pub const BEARER: &str = "test-worker-bearer";

pub fn config() -> FakeConfig {
    FakeConfig {
        worker_bearer: BEARER.into(),
        subject: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        placement: Placement {
            host: HOST.into(),
            vm: Some("test-vm".into()),
        },
        endpoint: "127.0.0.1:4443".into(),
        endpoint_spki_sha256: base64url(&[8; 32]),
        kid: "current".into(),
        signing_key: SigningKey::from_bytes(&[1; 32]),
        announced_keys: vec![JwksKey::from_key(
            "next",
            &SigningKey::from_bytes(&[2; 32]).verifying_key(),
        )],
    }
}
pub fn broker() -> FakeBroker {
    let mut b = FakeBroker::new("lease-test", config()).unwrap();
    b.register_host(
        HOST.into(),
        SigningKey::from_bytes(&[4; 32]).verifying_key(),
    );
    b
}
pub fn lease_request() -> LeaseRequest {
    LeaseRequest {
        job_id: "019e4567-89ab-7cde-8012-3456789abcde".into(),
        vcpus: 2,
        platform: "linux-x64".into(),
        net: false,
        workspace_key_hash: base64url(&[5; 32]),
        repo_hash: base64url(&[6; 32]),
        placement_hint: None,
        avoid_host: None,
        timeout_s: 10,
        requester_jwk: RequesterJwk::from_key(&SigningKey::from_bytes(&[3; 32]).verifying_key()),
    }
}
pub fn post(b: &mut FakeBroker, path: &str, body: &impl Serialize, now: u64) -> BrokerReply {
    let body = serde_json::to_vec(body).unwrap();
    b.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path,
            bearer: Some(BEARER),
            host_headers: None,
            body: &body,
        },
        now,
    )
}
pub fn host_post(
    b: &mut FakeBroker,
    route: &str,
    body: &impl Serialize,
    counter: u64,
    now: u64,
) -> BrokerReply {
    let body = serde_json::to_vec(body).unwrap();
    let path = format!("/v1/hosts/{HOST}/{route}");
    let headers = HostHeaders::sign(
        HOST,
        "POST",
        &path,
        &body,
        now,
        counter,
        &SigningKey::from_bytes(&[4; 32]),
    )
    .unwrap();
    b.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path: &path,
            bearer: None,
            host_headers: Some(&headers),
            body: &body,
        },
        now,
    )
}
pub fn heartbeat(state: JobState, elapsed_ms: u64) -> Heartbeat {
    Heartbeat {
        free_vcpus: 0,
        free_mem_mib: 18432,
        queued: 1,
        warm: vec![],
        healthy: true,
        running: vec![RunningJob {
            job_id: "019e4567-89ab-7cde-8012-3456789abcde".into(),
            state,
            elapsed_ms,
        }],
    }
}
pub fn usage() -> Usage {
    Usage {
        job_id: "019e4567-89ab-7cde-8012-3456789abcde".into(),
        vcpus: 2,
        started_at_ms: NOW,
        ended_at_ms: NOW + 3000,
        mem_peak_mib: Some(42),
        egress_bytes: Some(0),
        outcome: UsageOutcome::Exit,
    }
}
pub fn issued(b: &mut FakeBroker, request: &LeaseRequest, now: u64) -> LeaseResponse {
    let reply = post(b, "/v1/leases", request, now);
    assert_eq!(reply.status, 201, "{}", reply.body);
    serde_json::from_value(reply.body).unwrap()
}
pub fn verified(b: &FakeBroker, response: &LeaseResponse, purpose: Purpose, now: u64) -> Claims {
    let mut keys = KeySet::default();
    keys.refresh(Ok(b.jwks())).unwrap();
    verify_token(
        &response.job_token,
        &keys,
        HOST,
        Some(&response.job_id),
        purpose,
        now / 1000,
    )
    .unwrap()
}
