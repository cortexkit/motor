//! CLOUD conformance comparisons and explicitly derived lease-hash cross-checks.
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

// Retain legacy contract vectors on disk while exercising their other claims
// under the current registered-host requirement.
fn with_workspace_hash(token: &str, hash: &str, kid: &str, key: &SigningKey) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let payload = URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1).unwrap())
        .unwrap();
    let mut claims: Claims = serde_json::from_slice(&payload).unwrap();
    assert_eq!(claims.workspace_key_hash, None);
    claims.workspace_key_hash = Some(hash.into());
    sign_token(&claims, kid, key).unwrap()
}

#[test]
fn cloud_host_metrics_serializer_signer_and_refusals_match_pinned_bytes() {
    use metrics::*;
    let v = vector("host-metrics.json");
    let key = seed(v["seed_hex"].as_str().unwrap());
    let now = v["clock_ms"].as_u64().unwrap();
    let mut metrics = HostMetrics {
        at_ms: now,
        cpu: Cpu {
            threads: 32,
            busy_pct: 41,
            load1: Some(12.4),
        },
        mem: Memory {
            total_mib: 128000,
            avail_mib: 22000,
        },
        disks: vec![Disk {
            mount: "/".into(),
            total_gib: 900,
            free_gib: 310,
        }],
        tenants: vec![Tenant {
            vm: "vm-1".into(),
            state: TenantState::Ready,
            vcpus: 30,
            mem_mib: 106496,
            slots_total: 90,
            slots_used: 41,
            jobs_running: 12,
            recovery: None,
            detail: None,
        }],
        runners: vec![Runner {
            name: "direct".into(),
            state: RunnerState::Ready,
            slots_total: 16,
            slots_used: 3,
            queued: 0,
            oldest_wait_s: 0,
            detail: None,
        }],
    };
    assert_eq!(
        metrics.encode(now).unwrap(),
        v["cases"][0]["request"]["body"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
    metrics.cpu.load1 = None;
    assert_eq!(
        metrics.encode(now).unwrap(),
        v["cases"][5]["request"]["body"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
    let expected = [
        MetricsReply::Accepted,
        MetricsReply::TooLarge,
        MetricsReply::BadRequest {
            field: Some("runners[0].state".into()),
        },
        MetricsReply::BadRequest {
            field: Some("tenants[0].detail".into()),
        },
        MetricsReply::BadRequest {
            field: Some("at_ms".into()),
        },
        MetricsReply::Accepted,
    ];
    for (case, reply) in v["cases"].as_array().unwrap().iter().zip(expected) {
        let request = &case["request"];
        let headers = &request["headers"];
        let body = request["body"].as_str().unwrap();
        let timestamp = headers["CK-Timestamp"].as_str().unwrap().parse().unwrap();
        let counter = headers["CK-Counter"].as_str().unwrap().parse().unwrap();
        let path = request["pathname"].as_str().unwrap();
        let signed = host_message_bytes("POST", path, body.as_bytes(), timestamp, counter);
        assert_eq!(signed, case["signed_string"].as_str().unwrap().as_bytes());
        let signed = HostHeaders::sign(
            "metrics-vector",
            "POST",
            path,
            body.as_bytes(),
            timestamp,
            counter,
            &key,
        )
        .unwrap();
        assert_eq!(signed.signature, headers["CK-Signature"]);
        assert_eq!(
            parse_reply(
                case["reply"]["status"].as_u64().unwrap() as u16,
                case["reply"]["body"].as_str().unwrap().as_bytes()
            ),
            reply
        );
    }
    assert_eq!(
        parse_reply(400, br#"{"error":"bad_request"}"#),
        MetricsReply::BadRequest { field: None }
    );
    assert_eq!(
        parse_reply(401, br#"{"error":"host_auth_failed"}"#),
        MetricsReply::AuthFailed
    );
}

#[test]
fn cloud_host_metrics_invalid_samples_are_not_signed() {
    let v = vector("host-metrics.json");
    let now = v["clock_ms"].as_u64().unwrap();
    for index in [0, 3, 4] {
        let sample: metrics::HostMetrics =
            serde_json::from_str(v["cases"][index]["request"]["body"].as_str().unwrap()).unwrap();
        assert_eq!(sample.encode(now).is_ok(), index == 0);
    }
    assert!(serde_json::from_str::<metrics::HostMetrics>(
        v["cases"][2]["request"]["body"].as_str().unwrap()
    )
    .is_err());
    assert_eq!(
        v["cases"][1]["request"]["body"].as_str().unwrap().len(),
        metrics::MAX_BODY_BYTES + 1
    );
    let mut sample: metrics::HostMetrics =
        serde_json::from_str(v["cases"][0]["request"]["body"].as_str().unwrap()).unwrap();
    sample.cpu.busy_pct = 101;
    assert!(sample.encode(now).is_err());
    sample.cpu.busy_pct = 100;
    sample.tenants[0].state = metrics::TenantState::Recovering;
    assert!(sample.encode(now).is_err());
    sample.tenants[0].state = metrics::TenantState::Ready;
    sample.tenants[0].detail = Some("job output\nwith control".into());
    assert!(sample.encode(now).is_err());
}

fn canonical_lease_hash(body_bytes: &str) -> String {
    let body: LeaseRequest = serde_json::from_str(body_bytes).unwrap();
    let wire = serde_json::to_string(&body).unwrap();
    // LeaseRequest orders the hash members before requester_jwk. Requester keys
    // can rotate without changing a lease, so the broker excludes the final key.
    let requester = format!(
        ",\"requester_jwk\":{}}}",
        serde_json::to_string(&body.requester_jwk).unwrap()
    );
    let canonical = format!(
        "{}}}",
        wire.strip_suffix(&requester)
            .expect("requester key is the final wire member")
    );
    sha256_hex(canonical.as_bytes())
}

#[test]
fn cloud_explicit_only_leases_match_their_pinned_hashes() {
    let vector = vector("explicit-only-leases.json");
    let exchanges = vector["exchanges"].as_array().unwrap();
    assert_eq!(exchanges.len(), 2);
    for exchange in exchanges {
        let body_bytes = exchange["request"]["body_bytes"].as_str().unwrap();
        let body: Value = serde_json::from_str(body_bytes).unwrap();
        assert_eq!(body, exchange["request"]["body"]);
        assert_eq!(
            body["placement_hint"],
            json!({"host": "explicit", "vm": "warm"})
        );
        assert_eq!(canonical_lease_hash(body_bytes), exchange["lease_hash"]);
    }
}

#[test]
fn derived_non_null_placement_hint_matches_the_cross_check_hash() {
    // This body is a derived cross-check, not a CLOUD lease-route exchange.
    let body_bytes = r#"{"job_id":"019e4567-89ab-7cde-8012-3456789abcd8","vcpus":2,"platform":"linux-x64","net":true,"workspace_key_hash":"WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW","repo_hash":"RRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRR","placement_hint":{"host":"staging-fra-1","vm":"t1"},"avoid_host":null,"timeout_s":100,"requester_jwk":{"kty":"OKP","crv":"Ed25519","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},"allow_spill":true}"#;
    assert_eq!(
        canonical_lease_hash(body_bytes),
        "bf25b1ca5d91ce3edfb35e7bf66efb259bb5b2877e4e3fc870b090d23b1f1282"
    );
}

#[test]
fn cloud_long_lease_token_accepts_six_hours_but_boat_keeps_two() {
    let v = vector("long-lease.json");
    let key = seed(v["test_only_job_seed_hex"].as_str().unwrap());
    let kid = v["key_id"].as_str().unwrap();
    let mut keys = KeySet::default();
    keys.refresh(Ok(Jwks {
        keys: vec![JwksKey::from_key(kid, &key.verifying_key())],
    }))
    .unwrap();
    let reply = &v["lease"]["reply"]["body"];
    let now = v["clock_ms"].as_u64().unwrap() / 1000;
    let legacy = reply["job_token"].as_str().unwrap();
    assert_eq!(
        verify_token(legacy, &keys, "host-a", None, Purpose::Run, now),
        Err(AuthError::MissingWorkspaceKeyHash)
    );
    let token = with_workspace_hash(
        legacy,
        v["lease"]["request"]["body"]["workspace_key_hash"]
            .as_str()
            .unwrap(),
        kid,
        &key,
    );
    let mut claims = verify_token(&token, &keys, "host-a", None, Purpose::Run, now).unwrap();
    assert_eq!(claims.max_s, 21600);
    assert_eq!(reply["reserved_vcpu_ms"], 43_200_000);
    claims.max_s = 21601;
    let token = sign_token(&claims, kid, &key).unwrap();
    assert_eq!(
        check_token(&token, &keys, "host-a", None, Purpose::Run, now)
            .unwrap_err()
            .check,
        "max_s"
    );
    claims.aud = "boat:sandbox-1".into();
    claims.placement.host = claims.aud.clone();
    for (max_s, accepted) in [(7200, true), (7201, false)] {
        claims.max_s = max_s;
        let token = sign_token(&claims, kid, &key).unwrap();
        assert_eq!(
            verify_token(&token, &keys, &claims.aud, None, Purpose::Run, now).is_ok(),
            accepted
        );
    }
}

#[test]
fn cloud_job_tokens_and_requester_thumbprint_match_code_for_both_purposes() {
    for name in ["job-tokens.json", "job-tokens-workspace.json"] {
        check_cloud_job_tokens(name);
    }
}

fn check_cloud_job_tokens(name: &str) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let v = vector(name);
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
        let verified = verify_token(
            token,
            &keys,
            &claims.placement.host,
            Some(&claims.jti),
            purpose,
            clock,
        );
        if claims.workspace_key_hash.is_some() {
            assert_eq!(verified.unwrap(), claims);
        } else {
            // Keep checking the broker's exact legacy bytes without accepting
            // a host credential from before workspace hashes became required.
            assert_eq!(verified, Err(AuthError::MissingWorkspaceKeyHash));
        }
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

#[test]
fn cloud_job_tokens_refuse_malformed_workspace_hash() {
    let v = vector("job-tokens-workspace.json");
    let mut keys = KeySet::default();
    keys.refresh(Ok(serde_json::from_value(v["jwks"].clone()).unwrap()))
        .unwrap();
    let mut claims: Claims =
        serde_json::from_str(v["tokens"]["run"]["claims"].as_str().unwrap()).unwrap();
    for hash in [
        "".into(),
        "A".repeat(42),
        "A".repeat(44),
        format!("{}=", "A".repeat(42)),
        "é".repeat(43),
    ] {
        claims.workspace_key_hash = Some(hash);
        let token = sign_token(
            &claims,
            v["key_id"].as_str().unwrap(),
            &seed(v["seed_hex"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(
            check_token(
                &token,
                &keys,
                "host-a",
                None,
                Purpose::Run,
                v["clock_ms"].as_u64().unwrap() / 1000
            )
            .unwrap_err()
            .check,
            "workspace_key_hash"
        );
    }
    let mut raw: Value =
        serde_json::from_str(v["tokens"]["run"]["claims"].as_str().unwrap()).unwrap();
    raw["workspace_key_hash"] = Value::Null;
    assert!(serde_json::from_value::<Claims>(raw).is_err());
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
    assert_eq!(claims.workspace_key_hash, None);
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
    let mut v = vector("lease.json");
    let tokens = vector("job-tokens-workspace.json");
    // The lease response predates hash emission. Keep its other fields exact,
    // but compare current issuance against the pinned hash-bearing tokens.
    v["reply"]["body"]["job_token"] = tokens["tokens"]["run"]["job_token"].clone();
    v["reattach"]["reply"]["body"]["job_token"] = tokens["tokens"]["attach"]["job_token"].clone();
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
/// lease's unchanged fields. Seeded with the vector's keys and clocks, the fake
/// returns the same statuses and JSON bodies apart from the now-required token
/// hash, and the reattach leaves the settlement as it was.
#[test]
fn cloud_settled_reattach_reply_matches_fake() {
    let mut v = vector("settled-reattach.json");
    let hash = v["lease"]["request"]["body"]["workspace_key_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = seed(v["test_only_job_seed_hex"].as_str().unwrap());
    for phase in ["lease", "reattach"] {
        let token = with_workspace_hash(
            v[phase]["reply"]["body"]["job_token"].as_str().unwrap(),
            &hash,
            v["key_id"].as_str().unwrap(),
            &key,
        );
        v[phase]["reply"]["body"]["job_token"] = json!(token);
    }
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

/// CLOUD's `account-closure.json`: a closed account's new lease is refused
/// with the final 403 `account_closed`, and a signed heartbeat whose warm
/// entries name that closed account and an open one is answered with exactly
/// the closed one in `closed_subjects`. A third closed account the host does
/// not report stays out of the reply. The host's own `Heartbeat` encoding of
/// the snapshot signs to CLOUD's recorded signature, and the fake broker,
/// seeded with the same closures, gives the same statuses and bodies.
#[test]
fn cloud_account_closure_refusal_and_heartbeat_reply_match_fake() {
    let v = vector("account-closure.json");
    let tokens = vector("job-tokens.json");
    let acct = |field: &str| format!("acct:{}", v[field].as_str().unwrap());
    let host_id = v["host"]["host_id"].as_str().unwrap();
    let broker_for = |closed: &[String]| {
        let mut broker = FakeBroker::new(
            "cloud-vector",
            FakeConfig {
                worker_bearer: BEARER.into(),
                subject: acct("closed_account"),
                placement: serde_json::from_value(json!({"host": host_id, "vm": "warm-vm-1"}))
                    .unwrap(),
                endpoint: v["host"]["endpoint"].as_str().unwrap().into(),
                endpoint_spki_sha256: v["host"]["endpoint_spki_sha256"].as_str().unwrap().into(),
                kid: tokens["key_id"].as_str().unwrap().into(),
                signing_key: seed(tokens["seed_hex"].as_str().unwrap()),
                announced_keys: vec![],
            },
        )
        .unwrap();
        broker.knobs.closed_subjects = closed.iter().cloned().collect();
        broker
    };
    let closed = [acct("closed_account"), acct("unreported_closed_account")];
    let clock = v["clock_ms"].as_u64().unwrap();

    // The lease: refused for good, with no reservation behind it.
    let lease = &v["lease"];
    let request: LeaseRequest = serde_json::from_value(lease["request"]["body"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        lease["request"]["body"]
    );
    let mut broker = broker_for(&closed);
    let refused = post(
        &mut broker,
        lease["request"]["pathname"].as_str().unwrap(),
        &request,
        clock,
    );
    assert_eq!(
        refused.status,
        lease["reply"]["status"].as_u64().unwrap() as u16
    );
    assert_eq!(refused.body, lease["reply"]["body"]);
    assert_eq!(
        serde_json::to_string(&refused.body).unwrap(),
        r#"{"error":"account_closed"}"#
    );
    assert_eq!(lease["reply"]["retry_after"], Value::Null);
    assert!(broker.settlement(&request.job_id).is_none());

    // A job placed before the closure keeps its normal answer on re-request;
    // only a new job is refused.
    let mut open_then_closed = broker_for(&[]);
    let path = lease["request"]["pathname"].as_str().unwrap();
    assert_eq!(
        post(&mut open_then_closed, path, &request, clock).status,
        201
    );
    open_then_closed.knobs.closed_subjects = closed.iter().cloned().collect();
    assert_eq!(
        post(&mut open_then_closed, path, &request, clock).status,
        201
    );
    let mut fresh = request.clone();
    fresh.job_id = "019e4567-89ab-7cde-8012-3456789abcdf".into();
    assert_eq!(
        post(&mut open_then_closed, path, &fresh, clock).body,
        lease["reply"]["body"]
    );

    // The heartbeat: the host's encoding signs to CLOUD's bytes.
    let hb = &v["heartbeat"];
    let heartbeat: Heartbeat = serde_json::from_value(hb["request"]["body"].clone()).unwrap();
    let body = serde_json::to_vec(&heartbeat).unwrap();
    let path = hb["request"]["pathname"].as_str().unwrap();
    let headers = &hb["request"]["headers"];
    let timestamp = headers["CK-Timestamp"].as_str().unwrap().parse().unwrap();
    let counter = headers["CK-Counter"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        host_message_bytes("POST", path, &body, timestamp, counter),
        hb["signed_string"].as_str().unwrap().as_bytes()
    );
    let key = seed(v["test_only_host_seed_hex"].as_str().unwrap());
    let signed = HostHeaders::sign(host_id, "POST", path, &body, timestamp, counter, &key).unwrap();
    assert_eq!(signed.signature, headers["CK-Signature"]);
    assert_eq!(signed.host, headers["CK-Host"]);
    broker.register_host(host_id.into(), key.verifying_key());
    let reply = broker.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path,
            bearer: None,
            host_headers: Some(&signed),
            body: &body,
        },
        clock,
    );
    assert_eq!(reply.status, hb["reply"]["status"].as_u64().unwrap() as u16);
    assert_eq!(reply.body, hb["reply"]["body"]);
    assert_eq!(
        hb["reply"]["body"]["closed_subjects"],
        json!([acct("closed_account")])
    );
    assert!(!hb["request"]["body"]["warm"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warm| warm["tenant"] == acct("unreported_closed_account")));
}
