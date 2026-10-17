use ed25519_dalek::SigningKey;
use motor_lease::*;
use serde_json::{json, Value};

fn vectors() -> Value {
    serde_json::from_str(include_str!("../../contracts/vectors/interim.json")).unwrap()
}
fn claims() -> Claims {
    serde_json::from_value(vectors()["claims"].clone()).unwrap()
}
fn keys() -> KeySet {
    let mut keys = KeySet::default();
    keys.refresh(Ok(
        serde_json::from_value(vectors()["jwks"].clone()).unwrap()
    ))
    .unwrap();
    keys
}

#[test]
fn every_mirrored_token_vector() {
    let fixture = vectors();
    let cases = fixture["token_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 19);
    for case in cases {
        let result = verify_token(
            case["token"].as_str().unwrap(),
            &keys(),
            "test-host",
            Some("test-job"),
            case["now_s"].as_u64().unwrap(),
        );
        let actual = match result {
            Ok(_) => "ok",
            Err(AuthError::Expired) => "expired",
            Err(_) => "auth",
        };
        assert_eq!(actual, case["result"].as_str().unwrap(), "{}", case["name"]);
    }
}

#[test]
fn key_rotation_cache_keeps_last_good_and_removes_retired_keys() {
    let fixture = vectors();
    let token = fixture["token_cases"][0]["token"].as_str().unwrap();
    let mut cache = KeySet::default();
    assert_eq!(
        verify_token(token, &cache, "test-host", None, 1700000000),
        Err(AuthError::Auth)
    );
    let jwks: Jwks = serde_json::from_value(fixture["jwks"].clone()).unwrap();
    cache.refresh(Ok(jwks.clone())).unwrap();
    cache.refresh(Err(AuthError::Auth)).unwrap_err();
    cache.refresh(Ok(Jwks { keys: vec![] })).unwrap_err();
    assert!(verify_token(token, &cache, "test-host", None, 1700000000).is_ok());
    let mut malformed = jwks.clone();
    malformed.keys[1].x = "bad".into();
    cache.refresh(Ok(malformed)).unwrap_err();
    assert!(verify_token(token, &cache, "test-host", None, 1700000000).is_ok());
    cache
        .refresh(Ok(Jwks {
            keys: vec![jwks.keys[1].clone()],
        }))
        .unwrap();
    assert_eq!(
        verify_token(token, &cache, "test-host", None, 1700000000),
        Err(AuthError::Auth)
    );
    let next = fixture["token_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "announced_next")
        .unwrap();
    assert!(verify_token(
        next["token"].as_str().unwrap(),
        &cache,
        "test-host",
        None,
        1700000000
    )
    .is_ok());
}

#[test]
fn token_shape_signature_integer_and_resource_constraints() {
    let key = SigningKey::from_bytes(&[1; 32]);
    for token in ["", "a.b", "a.b.c.d", "e30.e30.bad", "=.x.y"] {
        assert!(verify_token(token, &keys(), "test-host", None, 1700000000).is_err());
    }
    for mutate in [
        |c: &mut Claims| c.vcpus = 0,
        |c: &mut Claims| c.max_s = 0,
        |c: &mut Claims| c.sub = "acct:invalid".into(),
        |c: &mut Claims| c.exp = c.iat,
        |c: &mut Claims| c.cnf.jkt = "not-a-thumbprint".into(),
    ] {
        let mut c = claims();
        mutate(&mut c);
        let token = sign_token(&c, "current", &key).unwrap();
        assert_eq!(
            verify_token(&token, &keys(), "test-host", None, 1700000000),
            Err(AuthError::Auth)
        );
    }
    assert_eq!(memory_max_mib(1), 16384);
    assert_eq!(memory_max_mib(2), 16384);
    assert_eq!(memory_max_mib(5), 17920);
    assert_eq!(memory_max_mib(80), 286720);
    let mut c = claims();
    c.vcpus = 5;
    c.mem_mib = 17920;
    let token = sign_token(&c, "current", &key).unwrap();
    assert!(verify_token(&token, &keys(), "test-host", None, c.iat).is_ok());
    assert!(verify_token(&token, &keys(), "other-host", None, c.iat).is_err());
}

#[test]
fn requester_thumbprint_and_challenge_vectors() {
    let v = vectors();
    let requester: RequesterJwk = serde_json::from_value(v["requester_jwk"].clone()).unwrap();
    let challenge: Challenge = serde_json::from_value(v["challenge"].clone()).unwrap();
    assert_eq!(requester.thumbprint().unwrap(), v["thumbprint"]);
    assert_eq!(
        challenge_bytes(&challenge, &claims()).unwrap(),
        v["challenge_input"].as_str().unwrap().as_bytes()
    );
    let sig = sign_challenge(
        &challenge,
        &claims(),
        &SigningKey::from_bytes(&[3; 32]),
        1700000000,
    )
    .unwrap();
    assert_eq!(sig, v["challenge_signature"]);
    let mut nonces = Nonces::default();
    nonces
        .verify(&challenge, &claims(), &requester, &sig, 1700000000)
        .unwrap();
    assert_eq!(
        nonces.verify(&challenge, &claims(), &requester, &sig, 1700000000),
        Err(AuthError::Auth)
    );
}

#[test]
fn requester_and_challenge_fail_closed() {
    let v = vectors();
    let requester: RequesterJwk = serde_json::from_value(v["requester_jwk"].clone()).unwrap();
    let challenge: Challenge = serde_json::from_value(v["challenge"].clone()).unwrap();
    let wrong_key = SigningKey::from_bytes(&[9; 32]);
    let wrong_jwk = RequesterJwk::from_key(&wrong_key.verifying_key());
    assert!(bind_requester(&claims(), &wrong_jwk).is_err());
    for field in ["d", "kid", "alg", "use", "extra"] {
        let mut jwk = v["requester_jwk"].clone();
        jwk[field] = json!("unexpected");
        assert!(
            serde_json::from_value::<RequesterJwk>(jwk).is_err(),
            "{field}"
        );
    }
    for (field, value) in [("kty", "RSA"), ("crv", "X25519"), ("x", "invalid")] {
        let mut jwk = v["requester_jwk"].clone();
        jwk[field] = json!(value);
        assert!(serde_json::from_value::<RequesterJwk>(jwk)
            .unwrap()
            .verifying_key()
            .is_err());
    }
    let bad_sig = sign_challenge(&challenge, &claims(), &wrong_key, 1700000000).unwrap();
    let mut nonces = Nonces::default();
    assert!(nonces
        .verify(&challenge, &claims(), &requester, &bad_sig, 1700000000)
        .is_err());
    let good_sig = v["challenge_signature"].as_str().unwrap();
    assert!(nonces
        .verify(&challenge, &claims(), &requester, good_sig, 1700000061)
        .is_err());
    nonces
        .verify(&challenge, &claims(), &requester, good_sig, 1700000060)
        .unwrap();
    let mut wrong_host = challenge.clone();
    wrong_host.host_id = "other-host".into();
    assert!(sign_challenge(&wrong_host, &claims(), &wrong_key, 1700000000).is_err());
    assert!(Nonces::default()
        .verify(&wrong_host, &claims(), &requester, good_sig, 1700000000)
        .is_err());
    for nonce in ["ab".repeat(31), "AB".repeat(32), "zz".repeat(32)] {
        let mut bad = challenge.clone();
        bad.nonce = nonce;
        assert!(challenge_bytes(&bad, &claims()).is_err());
    }
}

#[test]
fn prelude_is_closed_for_version_and_type_but_allows_extension_fields() {
    let mut hello = json!({"type":"lease_hello","v":1,"token":"token","requester_jwk":vectors()["requester_jwk"],"extension":true});
    assert!(serde_json::from_value::<Prelude>(hello.clone()).is_ok());
    hello["v"] = json!(2);
    assert!(serde_json::from_value::<Prelude>(hello.clone()).is_err());
    hello["v"] = json!(1);
    hello["type"] = json!("future_hello");
    assert!(serde_json::from_value::<Prelude>(hello).is_err());
    assert!(serde_json::from_value::<Prelude>(json!({"type":"lease_ok","extra":true})).is_ok());
}

#[test]
fn host_message_signing_vector_and_counter_seed() {
    let v = vectors();
    let body = v["host_body"].as_str().unwrap().as_bytes();
    let headers: HostHeaders = serde_json::from_value(v["host_headers"].clone()).unwrap();
    assert_eq!(
        host_message_bytes(
            "POST",
            "/v1/hosts/test-host/heartbeat",
            body,
            headers.timestamp,
            headers.counter
        ),
        v["host_input"].as_str().unwrap().as_bytes()
    );
    let signed = HostHeaders::sign(
        "test-host",
        "POST",
        "/v1/hosts/test-host/heartbeat",
        body,
        headers.timestamp,
        headers.counter,
        &SigningKey::from_bytes(&[4; 32]),
    )
    .unwrap();
    assert_eq!(signed.signature, headers.signature);
    assert_eq!(seed_counter(1, 1000), Ok(1_000_000));
    assert_eq!(seed_counter(1_000_001, 1000), Ok(1_000_002));
    assert_eq!(seed_counter(0, 0), Ok(1));
    assert_eq!(seed_counter(u64::MAX, 0), Err(AuthError::CounterOverflow));
    assert_eq!(seed_counter(0, u64::MAX), Err(AuthError::CounterOverflow));
}

#[test]
fn host_auth_rejects_replay_skew_unknown_host_and_tampering_without_consuming_counter() {
    let v = vectors();
    let host: RequesterJwk = serde_json::from_value(v["host_jwk"].clone()).unwrap();
    let mut auth = HostAuthenticator::default();
    auth.register("test-host".into(), host.verifying_key().unwrap());
    let headers: HostHeaders = serde_json::from_value(v["host_headers"].clone()).unwrap();
    let body = v["host_body"].as_str().unwrap().as_bytes();
    let path = "/v1/hosts/test-host/heartbeat";
    assert!(auth
        .verify("POST", path, b"tampered", &headers, headers.timestamp)
        .is_err());
    assert!(auth
        .verify("GET", path, body, &headers, headers.timestamp)
        .is_err());
    assert!(auth
        .verify("POST", "/wrong", body, &headers, headers.timestamp)
        .is_err());
    assert!(auth
        .verify("POST", path, body, &headers, headers.timestamp + 60001)
        .is_err());
    assert!(auth
        .verify("POST", path, body, &headers, headers.timestamp - 60001)
        .is_err());
    let mut unknown = headers.clone();
    unknown.host = "unknown-host".into();
    assert!(auth
        .verify("POST", path, body, &unknown, headers.timestamp)
        .is_err());
    auth.verify("POST", path, body, &headers, headers.timestamp + 60000)
        .unwrap();
    assert!(auth
        .verify("POST", path, body, &headers, headers.timestamp)
        .is_err());
}
