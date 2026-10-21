//! These comparisons execute Motor against immutable, externally supplied bytes.
use super::common::*;
use ed25519_dalek::SigningKey;
use motor_lease::{
    fake::{BrokerRequest, FakeBroker, FakeConfig},
    *,
};
use serde_json::{json, Value};

fn vector(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("contracts/cloud")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn seed(text: &str) -> SigningKey {
    let bytes: Vec<_> = text
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    SigningKey::from_bytes(&bytes.try_into().unwrap())
}

#[test]
fn cloud_job_tokens_and_requester_thumbprint_match_code_for_both_purposes() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let v = vector("job-tokens.json");
    let mut keys = KeySet::default();
    keys.refresh(Ok(serde_json::from_value(v["jwks"].clone()).unwrap()))
        .unwrap();
    let requester = RequesterJwk::from_key(
        &seed(v["test_only_requester_seed_hex"].as_str().unwrap()).verifying_key(),
    );
    assert_eq!(requester.thumbprint().unwrap(), v["requester_jkt"]);
    for (name, purpose, other) in [
        ("run", Purpose::Run, Purpose::Attach),
        ("attach", Purpose::Attach, Purpose::Run),
    ] {
        let entry = &v["tokens"][name];
        let token = entry["job_token"].as_str().unwrap();
        let claims: Claims = serde_json::from_str(entry["claims"].as_str().unwrap()).unwrap();
        let clock = v["clock_ms"].as_u64().unwrap() / 1000;
        assert_eq!(
            verify_token(
                token,
                &keys,
                &claims.placement.host,
                Some(&claims.jti),
                purpose,
                clock
            )
            .unwrap(),
            claims
        );
        assert_eq!(
            verify_token(
                token,
                &keys,
                &claims.placement.host,
                Some(&claims.jti),
                other,
                clock
            ),
            Err(AuthError::Auth)
        );
        assert_eq!(
            bind_requester(&claims, &requester).unwrap(),
            requester.verifying_key().unwrap()
        );
        let minted = sign_token(
            &claims,
            v["key_id"].as_str().unwrap(),
            &seed(v["seed_hex"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(minted, token, "{name} JWS differs from CLOUD");
        let parts: Vec<_> = minted.split('.').collect();
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[0]).unwrap(),
            entry["header"].as_str().unwrap().as_bytes()
        );
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[1]).unwrap(),
            entry["claims"].as_str().unwrap().as_bytes()
        );
    }
}

/// The in-sandbox endpoint must accept CLOUD's recorded Boat token exactly as
/// issued. Its `mem_mib` is the recorded test size's usable memory (14336 MiB
/// for 2 vCPUs), not the fleet formula (16384 MiB), as CLOUD's contract states
/// for Boat. The same memory in a first-party host's token stays a refusal.
#[test]
fn cloud_boat_token_verifies_for_its_sandbox_identity() {
    let tokens = vector("job-tokens.json");
    let boat = vector("boat-lease.json");
    let mut keys = KeySet::default();
    keys.refresh(Ok(serde_json::from_value(tokens["jwks"].clone()).unwrap()))
        .unwrap();
    let reply = &boat["booked"]["reply"]["body"];
    let token = reply["job_token"].as_str().unwrap();
    let host = reply["placement"]["host"].as_str().unwrap();
    let job = reply["job_id"].as_str().unwrap();
    let claims = check_token(token, &keys, host, Some(job), Purpose::Run, 1900000010).unwrap();
    assert_eq!(claims.mem_mib, boat["boat_sizes"][0]["mem_mib"]);
    assert_ne!(claims.mem_mib, memory_max_mib(claims.vcpus));
    assert!(boat_memory_range(claims.vcpus).contains(&claims.mem_mib));
    assert_eq!(
        check_token(
            token,
            &keys,
            "boat:bx_33333333",
            Some(job),
            Purpose::Run,
            1900000010
        )
        .unwrap_err()
        .check,
        "aud"
    );
    let mut first_party = claims;
    first_party.aud = "host-a".into();
    first_party.placement.host = "host-a".into();
    let resigned = sign_token(
        &first_party,
        tokens["key_id"].as_str().unwrap(),
        &seed(tokens["seed_hex"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(
        check_token(
            &resigned,
            &keys,
            "host-a",
            Some(job),
            Purpose::Run,
            1900000010
        ),
        Err(TokenRefusal {
            error: AuthError::Auth,
            check: "mem_mib",
            jti: Some(job.into()),
        })
    );
}

#[test]
fn cloud_host_signature_bytes_header_and_verification_match() {
    let v = vector("host-signature.json");
    let key = seed(v["seed_hex"].as_str().unwrap());
    let path = v["pathname"].as_str().unwrap();
    let body = v["body"].as_str().unwrap().as_bytes();
    let timestamp = v["timestamp"].as_str().unwrap().parse().unwrap();
    let counter = v["counter"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        host_message_bytes("POST", path, body, timestamp, counter),
        v["signed_string"].as_str().unwrap().as_bytes()
    );
    let headers =
        HostHeaders::sign("host-1", "POST", path, body, timestamp, counter, &key).unwrap();
    assert_eq!(headers.signature, v["ck_signature_header"]);
    let mut auth = HostAuthenticator::default();
    auth.register("host-1".into(), key.verifying_key());
    let mut changed = body.to_vec();
    changed[0] ^= 1;
    assert_eq!(
        auth.verify("POST", path, &changed, &headers, timestamp),
        Err(AuthError::Auth)
    );
    auth.verify("POST", path, body, &headers, timestamp)
        .unwrap();
    let mut bad = headers.clone();
    bad.signature.replace_range(
        ..2,
        if headers.signature.starts_with("00") {
            "01"
        } else {
            "00"
        },
    );
    let mut fresh = HostAuthenticator::default();
    fresh.register("host-1".into(), key.verifying_key());
    assert_eq!(
        fresh.verify("POST", path, body, &bad, timestamp),
        Err(AuthError::Auth)
    );
}

#[test]
fn cloud_lease_reattach_release_and_nullable_usage_settlement_match_fake() {
    let v = vector("lease.json");
    let tokens = vector("job-tokens.json");
    let mut broker = FakeBroker::new(
        "cloud-vector",
        FakeConfig {
            worker_bearer: BEARER.into(),
            subject: format!("acct:{}", v["account"].as_str().unwrap()),
            placement: serde_json::from_value(v["reply"]["body"]["placement"].clone()).unwrap(),
            endpoint: v["host"]["endpoint"].as_str().unwrap().into(),
            endpoint_spki_sha256: v["host"]["endpoint_spki_sha256"].as_str().unwrap().into(),
            kid: v["key_id"].as_str().unwrap().into(),
            signing_key: seed(v["seed_hex"].as_str().unwrap()),
            announced_keys: vec![],
        },
    )
    .unwrap();
    broker.knobs.allowance_vcpu_ms = v["allowance_vcpu_ms"].as_u64();
    broker.knobs.lease_id = Some(v["lease_id"].as_str().unwrap().into());
    let clock = v["clock_ms"].as_u64().unwrap();
    let request: LeaseRequest = serde_json::from_value(v["request"]["body"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        v["request"]["body"]
    );
    let issued = post(
        &mut broker,
        v["request"]["pathname"].as_str().unwrap(),
        &request,
        clock,
    );
    assert_eq!(issued.status, v["reply"]["status"].as_u64().unwrap() as u16);
    assert_eq!(issued.body, v["reply"]["body"]);
    let attach: ReattachRequest =
        serde_json::from_value(v["reattach"]["request"]["body"].clone()).unwrap();
    let attached = post(
        &mut broker,
        v["reattach"]["request"]["pathname"].as_str().unwrap(),
        &attach,
        clock,
    );
    assert_eq!(
        attached.status,
        v["reattach"]["reply"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(attached.body, v["reattach"]["reply"]["body"]);
    assert_eq!(
        attached.body["job_token"],
        tokens["tokens"]["attach"]["job_token"]
    );
    let release = post(
        &mut broker,
        v["release"]["request"]["pathname"].as_str().unwrap(),
        &ReleaseRequest::default(),
        clock,
    );
    assert_eq!(
        release.status,
        v["release"]["reply"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(release.body, v["release"]["reply"]["body"]);
    let u = vector("usage-settlement.json");
    let key = seed(u["seed_hex"].as_str().unwrap());
    broker.register_host("host-a".into(), key.verifying_key());
    let usage: Usage = serde_json::from_value(u["request"]["body"].clone()).unwrap();
    assert_eq!(usage.mem_peak_mib, None);
    assert_eq!(usage.egress_bytes, None);
    let body = serde_json::to_vec(&usage).unwrap();
    let headers = &u["request"]["headers"];
    let timestamp = headers["CK-Timestamp"].as_str().unwrap().parse().unwrap();
    let counter = headers["CK-Counter"].as_str().unwrap().parse().unwrap();
    let path = u["request"]["pathname"].as_str().unwrap();
    assert_eq!(
        host_message_bytes("POST", path, &body, timestamp, counter),
        u["signed_string"].as_str().unwrap().as_bytes()
    );
    let signed =
        HostHeaders::sign("host-a", "POST", path, &body, timestamp, counter, &key).unwrap();
    assert_eq!(signed.signature, headers["CK-Signature"]);
    let settled = broker.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path,
            bearer: None,
            host_headers: Some(&signed),
            body: &body,
        },
        u["clock_ms"].as_u64().unwrap(),
    );
    assert_eq!(
        settled.status,
        u["reply"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(settled.body, u["reply"]["body"]);
    assert_eq!(serde_json::to_value(&usage).unwrap(), u["request"]["body"]);
    let mut reordered = serde_json::to_vec(&u["request"]["body"]).unwrap();
    reordered.push(b' ');
    let signed = HostHeaders::sign(
        "host-a",
        "POST",
        path,
        &reordered,
        timestamp,
        counter + 1,
        &key,
    )
    .unwrap();
    assert_eq!(
        broker.handle(
            BrokerRequest {
                content_type: Some("application/json"),
                method: "POST",
                path,
                bearer: None,
                host_headers: Some(&signed),
                body: &reordered
            },
            timestamp
        ),
        settled
    );
    let mut missing = u["request"]["body"].clone();
    missing.as_object_mut().unwrap().remove("mem_peak_mib");
    assert!(serde_json::from_value::<Usage>(missing).is_err());
    let mut unexpected = v["request"]["body"].clone();
    unexpected["mem_mib"] = json!(16384);
    assert!(serde_json::from_value::<LeaseRequest>(unexpected).is_err());
}

/// CLOUD's `settled-reattach.json`: a registered host settles a job by usage
/// report, and a later reattach still gets 200 with an `attach` token and the
/// lease's unchanged fields. The fake, seeded with the vector's keys and
/// clocks, reproduces the lease, settlement and reattach replies byte for byte
/// and leaves the settlement as it was.
#[test]
fn cloud_settled_reattach_reply_matches_fake() {
    let v = vector("settled-reattach.json");
    let lease_reply = &v["lease"]["reply"]["body"];
    let mut broker = FakeBroker::new(
        "cloud-vector",
        FakeConfig {
            worker_bearer: BEARER.into(),
            subject: format!("acct:{}", v["account"].as_str().unwrap()),
            placement: serde_json::from_value(lease_reply["placement"].clone()).unwrap(),
            endpoint: v["host"]["endpoint"].as_str().unwrap().into(),
            endpoint_spki_sha256: v["host"]["endpoint_spki_sha256"].as_str().unwrap().into(),
            kid: v["key_id"].as_str().unwrap().into(),
            signing_key: seed(v["test_only_job_seed_hex"].as_str().unwrap()),
            announced_keys: vec![],
        },
    )
    .unwrap();
    broker.knobs.allowance_vcpu_ms = v["tenant_before"]["body"]["allowance"].as_u64();
    broker.knobs.lease_id = Some(v["lease_id"].as_str().unwrap().into());
    let machine = seed(v["test_only_machine_seed_hex"].as_str().unwrap());
    let host_id = v["host"]["host_id"].as_str().unwrap();
    assert_eq!(
        machine.verifying_key().to_bytes().to_vec(),
        (0..32)
            .map(|i| u8::from_str_radix(
                &v["host"]["public_key_hex"].as_str().unwrap()[i * 2..i * 2 + 2],
                16
            )
            .unwrap())
            .collect::<Vec<_>>()
    );
    broker.register_host(host_id.into(), machine.verifying_key());
    let requester = seed(v["test_only_requester_seed_hex"].as_str().unwrap());

    let request: LeaseRequest =
        serde_json::from_value(v["lease"]["request"]["body"].clone()).unwrap();
    assert_eq!(
        request.requester_jwk,
        RequesterJwk::from_key(&requester.verifying_key())
    );
    let leased = post(
        &mut broker,
        v["lease"]["request"]["pathname"].as_str().unwrap(),
        &request,
        v["lease_clock_ms"].as_u64().unwrap(),
    );
    assert_eq!(
        (leased.status, &leased.body),
        (
            v["lease"]["reply"]["status"].as_u64().unwrap() as u16,
            lease_reply
        )
    );

    let u = &v["usage"]["request"];
    let usage: Usage = serde_json::from_value(u["body"].clone()).unwrap();
    let body = serde_json::to_vec(&usage).unwrap();
    let path = u["pathname"].as_str().unwrap();
    let timestamp = u["headers"]["CK-Timestamp"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let counter = u["headers"]["CK-Counter"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let signed =
        HostHeaders::sign(host_id, "POST", path, &body, timestamp, counter, &machine).unwrap();
    assert_eq!(signed.signature, u["headers"]["CK-Signature"]);
    let settled = broker.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path,
            bearer: None,
            host_headers: Some(&signed),
            body: &body,
        },
        v["usage_clock_ms"].as_u64().unwrap(),
    );
    assert_eq!(
        (settled.status, &settled.body),
        (
            v["usage"]["reply"]["status"].as_u64().unwrap() as u16,
            &v["usage"]["reply"]["body"]
        )
    );
    let settlement = |broker: &FakeBroker| {
        broker
            .settlement(&request.job_id)
            .map(|(reply, reason)| (serde_json::to_value(reply).unwrap(), reason))
    };
    let before = settlement(&broker);
    assert_eq!(before, Some((v["usage"]["reply"]["body"].clone(), "usage")));
    // The tenant read before and after the reattach is identical in CLOUD, and
    // its settled total is this job's settlement.
    assert_eq!(v["tenant_before"], v["tenant_after"]);
    assert_eq!(
        v["tenant_before"]["body"]["settled_vcpu_ms"],
        v["usage"]["reply"]["body"]["settled_vcpu_ms"]
    );

    let attach: ReattachRequest =
        serde_json::from_value(v["reattach"]["request"]["body"].clone()).unwrap();
    let reattached = post(
        &mut broker,
        v["reattach"]["request"]["pathname"].as_str().unwrap(),
        &attach,
        v["clock_ms"].as_u64().unwrap(),
    );
    let expected = &v["reattach"]["reply"]["body"];
    assert_eq!(
        (reattached.status, &reattached.body),
        (
            v["reattach"]["reply"]["status"].as_u64().unwrap() as u16,
            expected
        )
    );
    for field in [
        "lease_id",
        "job_id",
        "placement",
        "endpoint",
        "endpoint_spki_sha256",
        "max_s",
        "reserved_vcpu_ms",
    ] {
        assert_eq!(expected[field], lease_reply[field], "{field}");
    }
    let mut keys = KeySet::default();
    keys.refresh(Ok(broker.jwks())).unwrap();
    let claims = verify_token(
        expected["job_token"].as_str().unwrap(),
        &keys,
        host_id,
        Some(&request.job_id),
        Purpose::Attach,
        v["clock_ms"].as_u64().unwrap() / 1000,
    )
    .unwrap();
    assert_eq!(claims.purpose, Purpose::Attach);
    assert_eq!(settlement(&broker), before);
}

/// CLOUD's `no-host-fits-lease.json` is a no-spill 16-vCPU lease against a
/// single registered host whose per-job ceiling is 8 vCPUs. The fake broker,
/// limited to that ceiling, answers the same request with the same status and
/// the same reply bytes, and books nothing.
#[test]
fn cloud_no_host_fits_reply_bytes_match_fake() {
    let v = vector("no-host-fits-lease.json");
    let tokens = vector("job-tokens.json");
    let host = &v["host_registry"];
    let mut broker = FakeBroker::new(
        "cloud-vector",
        FakeConfig {
            worker_bearer: BEARER.into(),
            subject: format!("acct:{}", v["account"].as_str().unwrap()),
            placement: serde_json::from_value(json!({"host": host["host_id"], "vm": null}))
                .unwrap(),
            endpoint: "127.0.0.1:4443".into(),
            endpoint_spki_sha256: base64url(&[8; 32]),
            kid: v["key_id"].as_str().unwrap().into(),
            signing_key: seed(tokens["seed_hex"].as_str().unwrap()),
            announced_keys: vec![],
        },
    )
    .unwrap();
    broker.knobs.max_vcpus = Some(host["max_job_vcpus"].as_u64().unwrap() as u32);
    let request: LeaseRequest = serde_json::from_value(v["request"]["body"].clone()).unwrap();
    assert_eq!(request.allow_spill, Some(false));
    // Send CLOUD's recorded request bytes, not a re-serialization of them.
    let reply = broker.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: v["request"]["method"].as_str().unwrap(),
            path: v["request"]["pathname"].as_str().unwrap(),
            bearer: Some(BEARER),
            host_headers: None,
            body: v["request"]["body_bytes"].as_str().unwrap().as_bytes(),
        },
        v["clock_ms"].as_u64().unwrap(),
    );
    assert_eq!(reply.status, v["reply"]["status"].as_u64().unwrap() as u16);
    assert_eq!(
        serde_json::to_string(&reply.body).unwrap(),
        v["reply"]["body_bytes"].as_str().unwrap()
    );
    assert_eq!(v["reply"]["headers"]["Retry-After"], Value::Null);
    assert!(broker.settlement(&request.job_id).is_none());
}
