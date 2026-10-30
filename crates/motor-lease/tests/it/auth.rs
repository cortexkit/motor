use ed25519_dalek::SigningKey;
use motor_lease::*;
use serde_json::{json, Value};

// These adversarial credentials are created only for tests. The separate cloud.rs
// tests compare production code with the broker's fixed inputs in contracts/cloud/.
fn vectors() -> Value {
    use ed25519_dalek::Signer;
    let current = SigningKey::from_bytes(&[1; 32]);
    let next = SigningKey::from_bytes(&[2; 32]);
    let requester = RequesterJwk::from_key(&SigningKey::from_bytes(&[3; 32]).verifying_key());
    let header = json!({"alg":"EdDSA","typ":"ck-job+jwt","kid":"current"});
    let c = json!({"iss":ISSUER,"aud":"test-host","jti":"019e4567-89ab-7cde-8012-3456789abcde","sub":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV","purpose":"run","iat":1700000000u64,"exp":1700000300u64,"vcpus":2,"mem_mib":16384,"platform":"linux-x64","net":false,"placement":{"host":"test-host","vm":"test-vm"},"max_s":30,"cnf":{"jkt":requester.thumbprint().unwrap()}});
    let token = |h: &Value, c: &Value, key: &SigningKey| {
        let input = format!(
            "{}.{}",
            base64url(&serde_json::to_vec(h).unwrap()),
            base64url(&serde_json::to_vec(c).unwrap())
        );
        format!(
            "{input}.{}",
            base64url(&key.sign(input.as_bytes()).to_bytes())
        )
    };
    let mut cases = vec![
        json!({"name":"valid_run","token":token(&header,&c,&current),"now_s":1700000000u64,"result":"ok"}),
    ];
    for (name, field, value) in [
        ("wrong_alg", "alg", json!("HS256")),
        ("wrong_typ", "typ", json!("JWT")),
        ("unknown_kid", "kid", json!("unknown")),
        ("crit_empty", "crit", json!([])),
        ("crit_null", "crit", Value::Null),
    ] {
        let mut h = header.clone();
        h[field] = value;
        cases.push(json!({"name":name,"token":token(&h,&c,&current),"now_s":1700000000u64,"result":"auth"}));
    }
    for (name, pointer, value) in [
        ("wrong_issuer", "/iss", json!("https://invalid.example")),
        ("wrong_audience", "/aud", json!("other-host")),
        (
            "wrong_job",
            "/jti",
            json!("019e4567-89ab-7cde-8012-3456789abcdf"),
        ),
        (
            "wrong_placement_host",
            "/placement/host",
            json!("other-host"),
        ),
        ("mem_mismatch", "/mem_mib", json!(16383)),
        ("future_iat", "/iat", json!(1700000061u64)),
        ("invalid_purpose", "/purpose", json!("admin")),
        ("zero_vcpus", "/vcpus", json!(0)),
    ] {
        let mut changed = c.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        cases.push(json!({"name":name,"token":token(&header,&changed,&current),"now_s":1700000000u64,"result":"auth"}));
    }
    cases.push(json!({"name":"expired_at_connect","token":token(&header,&c,&current),"now_s":1700000300u64,"result":"expired"}));
    let mut boundary = c.clone();
    boundary["iat"] = json!(1700000060u64);
    cases.push(json!({"name":"iat_skew_boundary","token":token(&header,&boundary,&current),"now_s":1700000000u64,"result":"ok"}));
    let mut h = header.clone();
    h["kid"] = json!("next");
    cases.push(json!({"name":"announced_next","token":token(&h,&c,&next),"now_s":1700000000u64,"result":"ok"}));
    let mut attach = c.clone();
    attach["purpose"] = json!("attach");
    cases.push(json!({"name":"valid_attach","token":token(&header,&attach,&current),"now_s":1700000000u64,"result":"ok"}));
    let mut tampered: Vec<String> = token(&header, &c, &current)
        .split('.')
        .map(str::to_owned)
        .collect();
    let mut changed = c.clone();
    changed["max_s"] = json!(31);
    tampered[1] = base64url(&serde_json::to_vec(&changed).unwrap());
    cases.push(json!({"name":"tampered_claims","token":tampered.join("."),"now_s":1700000000u64,"result":"auth"}));
    let challenge = json!({"nonce":"ab".repeat(32),"host_id":"test-host","ts":1700000000u64});
    let challenge_input=format!("motor-host-challenge-v1\n{}\ntest-host\n019e4567-89ab-7cde-8012-3456789abcde\n1700000000\n","ab".repeat(32));
    let body =
        r#"{"free_vcpus":2,"free_mem_mib":18432,"queued":0,"warm":[],"running":[],"healthy":true}"#;
    let host_input = format!(
        "ck-host-v1\nPOST\n/v1/hosts/test-host/heartbeat\n{}\n1700000000000\n1700000000000000",
        sha256_hex(body.as_bytes())
    );
    let host_key = SigningKey::from_bytes(&[4; 32]);
    json!({"claims":c,"jwks":{"keys":[JwksKey::from_key("current",&current.verifying_key()),JwksKey::from_key("next",&next.verifying_key())]},"token_cases":cases,"requester_jwk":requester,"thumbprint":requester.thumbprint().unwrap(),"challenge":challenge,"challenge_input":challenge_input,"challenge_signature":base64url(&SigningKey::from_bytes(&[3;32]).sign(challenge_input.as_bytes()).to_bytes()),"host_body":body,"host_input":host_input,"host_jwk":RequesterJwk::from_key(&host_key.verifying_key()),"host_headers":HostHeaders::sign("test-host","POST","/v1/hosts/test-host/heartbeat",body.as_bytes(),1700000000000,1700000000000000,&host_key).unwrap()})
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
            Some("019e4567-89ab-7cde-8012-3456789abcde"),
            if case["name"] == "valid_attach" {
                Purpose::Attach
            } else {
                Purpose::Run
            },
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
fn boat_audience_accepts_only_its_namespace_and_valid_sandbox_names() {
    let key = SigningKey::from_bytes(&[1; 32]);
    for (audience, accepted) in [
        ("test-host".to_owned(), true),
        ("boat:bx_test-1.2".to_owned(), true),
        (format!("boat:{}", "a".repeat(64)), true),
        (format!("boat:{}", "a".repeat(65)), false),
        ("boat:".to_owned(), false),
        ("boat:bad/id".to_owned(), false),
        ("boat:bad id".to_owned(), false),
        ("boat:boat:bx_test".to_owned(), false),
        ("other:bx_test".to_owned(), false),
        ("Boat:bx_test".to_owned(), false),
    ] {
        let mut c = claims();
        c.aud = audience.clone();
        c.placement.host = audience.clone();
        let token = sign_token(&c, "current", &key).unwrap();
        assert_eq!(
            verify_token(&token, &keys(), &audience, None, Purpose::Run, c.iat).is_ok(),
            accepted,
            "{audience}"
        );
    }
}

#[test]
fn key_rotation_cache_keeps_last_good_and_removes_retired_keys() {
    let fixture = vectors();
    let token = fixture["token_cases"][0]["token"].as_str().unwrap();
    let mut cache = KeySet::default();
    assert_eq!(
        verify_token(token, &cache, "test-host", None, Purpose::Run, 1700000000),
        Err(AuthError::Auth)
    );
    let jwks: Jwks = serde_json::from_value(fixture["jwks"].clone()).unwrap();
    cache.refresh(Ok(jwks.clone())).unwrap();
    cache.refresh(Err(AuthError::Auth)).unwrap_err();
    cache.refresh(Ok(Jwks { keys: vec![] })).unwrap_err();
    assert!(verify_token(token, &cache, "test-host", None, Purpose::Run, 1700000000).is_ok());
    let mut malformed = jwks.clone();
    malformed.keys[1].x = "bad".into();
    cache.refresh(Ok(malformed)).unwrap();
    assert!(verify_token(token, &cache, "test-host", None, Purpose::Run, 1700000000).is_ok());
    cache
        .refresh(Ok(Jwks {
            keys: vec![jwks.keys[1].clone()],
        }))
        .unwrap();
    assert_eq!(
        verify_token(token, &cache, "test-host", None, Purpose::Run, 1700000000),
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
        Purpose::Run,
        1700000000
    )
    .is_ok());
}

#[test]
fn token_shape_signature_integer_and_resource_constraints() {
    let key = SigningKey::from_bytes(&[1; 32]);
    for token in ["", "a.b", "a.b.c.d", "e30.e30.bad", "=.x.y"] {
        assert!(verify_token(token, &keys(), "test-host", None, Purpose::Run, 1700000000).is_err());
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
            verify_token(&token, &keys(), "test-host", None, Purpose::Run, 1700000000),
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
    assert!(verify_token(&token, &keys(), "test-host", None, Purpose::Run, c.iat).is_ok());
    assert!(verify_token(&token, &keys(), "other-host", None, Purpose::Run, c.iat).is_err());
}

#[test]
fn token_lifetime_is_at_most_five_minutes() {
    let key = SigningKey::from_bytes(&[1; 32]);
    for lifetime in [299, 300, 301, 86400] {
        let mut c = claims();
        c.exp = c.iat + lifetime;
        let token = sign_token(&c, "current", &key).unwrap();
        let verified = verify_token(&token, &keys(), "test-host", None, Purpose::Run, c.iat);
        assert_eq!(verified.is_ok(), lifetime <= 300, "lifetime {lifetime}");
    }
}

#[test]
fn token_purpose_must_match_the_callers_required_credential() {
    let key = SigningKey::from_bytes(&[1; 32]);
    for purpose in [Purpose::Run, Purpose::Attach] {
        let mut c = claims();
        c.purpose = purpose;
        let token = sign_token(&c, "current", &key).unwrap();
        for required in [Purpose::Run, Purpose::Attach] {
            let verified = verify_token(&token, &keys(), "test-host", None, required, c.iat);
            assert_eq!(verified.is_ok(), purpose == required);
        }
    }
}

#[test]
fn jwks_skips_unsupported_or_invalid_keys_and_retains_cache_if_no_usable_key_remains() {
    let mut fixture = vectors();
    fixture["jwks"]["keys"].as_array_mut().unwrap().extend([
        json!({"kty":"RSA","kid":"future-rsa","n":"modulus","e":"AQAB"}),
        json!({"kty":"future-type"}),
        json!({"kty":"OKP","kid":"other-curve","crv":"X25519","x":"unused"}),
        json!({"kty":"OKP","kid":"bad-ed25519","crv":"Ed25519","x":"bad"}),
    ]);
    let mut cache = KeySet::default();
    cache
        .refresh(Ok(serde_json::from_value(fixture["jwks"].clone()).unwrap()))
        .unwrap();
    for name in ["valid_run", "announced_next"] {
        let vector = fixture["token_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        assert!(verify_token(
            vector["token"].as_str().unwrap(),
            &cache,
            "test-host",
            None,
            Purpose::Run,
            1700000000
        )
        .is_ok());
    }
    for key in [
        json!({"kty":"RSA","kid":"future-rsa"}),
        json!({"kty":"OKP","kid":"bad-ed25519","crv":"Ed25519","x":"bad"}),
    ] {
        cache
            .refresh(Ok(serde_json::from_value(json!({"keys":[key]})).unwrap()))
            .unwrap_err();
        assert!(verify_token(
            fixture["token_cases"][0]["token"].as_str().unwrap(),
            &cache,
            "test-host",
            None,
            Purpose::Run,
            1700000000
        )
        .is_ok());
    }
    fixture["jwks"]["keys"].as_array_mut().unwrap().remove(0);
    cache
        .refresh(Ok(serde_json::from_value(fixture["jwks"].clone()).unwrap()))
        .unwrap();
    assert!(verify_token(
        fixture["token_cases"][0]["token"].as_str().unwrap(),
        &cache,
        "test-host",
        None,
        Purpose::Run,
        1700000000
    )
    .is_err());
    let next = fixture["token_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "announced_next")
        .unwrap();
    assert!(verify_token(
        next["token"].as_str().unwrap(),
        &cache,
        "test-host",
        None,
        Purpose::Run,
        1700000000
    )
    .is_ok());
}

#[test]
fn jwks_conflicting_kids_are_not_trusted_but_do_not_block_other_keys() {
    let mut jwks: Jwks = serde_json::from_value(vectors()["jwks"].clone()).unwrap();
    jwks.keys.push(JwksKey::from_key(
        "current",
        &SigningKey::from_bytes(&[9; 32]).verifying_key(),
    ));
    let mut cache = KeySet::default();
    cache.refresh(Ok(jwks)).unwrap();
    let fixture = vectors();
    assert!(verify_token(
        fixture["token_cases"][0]["token"].as_str().unwrap(),
        &cache,
        "test-host",
        None,
        Purpose::Run,
        1700000000
    )
    .is_err());
    let next = fixture["token_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "announced_next")
        .unwrap();
    assert!(verify_token(
        next["token"].as_str().unwrap(),
        &cache,
        "test-host",
        None,
        Purpose::Run,
        1700000000
    )
    .is_ok());
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
fn nonce_pruning_does_not_reopen_replay_inside_the_skew_window() {
    let c = claims();
    let key = SigningKey::from_bytes(&[3; 32]);
    let jwk = RequesterJwk::from_key(&key.verifying_key());
    let mut challenge: Challenge = serde_json::from_value(vectors()["challenge"].clone()).unwrap();
    let sig = sign_challenge(&challenge, &c, &key, c.iat).unwrap();
    let mut nonces = Nonces::default();
    nonces.verify(&challenge, &c, &jwk, &sig, c.iat).unwrap();
    assert_eq!(nonces.prune(c.iat + CLOCK_SKEW_S), 0);
    assert!(nonces
        .verify(&challenge, &c, &jwk, &sig, c.iat + CLOCK_SKEW_S)
        .is_err());
    challenge.ts += CLOCK_SKEW_S;
    let fresh_sig = sign_challenge(&challenge, &c, &key, challenge.ts).unwrap();
    assert!(nonces
        .verify(&challenge, &c, &jwk, &fresh_sig, challenge.ts)
        .is_err());
    assert_eq!(nonces.prune(c.iat + 2 * CLOCK_SKEW_S), 0);
    assert_eq!(nonces.prune(c.iat + 2 * CLOCK_SKEW_S + 1), 1);
    assert!(nonces
        .verify(
            &challenge,
            &c,
            &jwk,
            &fresh_sig,
            c.iat + 2 * CLOCK_SKEW_S + 1
        )
        .is_err());
}

#[test]
fn nonce_verification_automatically_prunes_old_entries() {
    let c = claims();
    let key = SigningKey::from_bytes(&[3; 32]);
    let jwk = RequesterJwk::from_key(&key.verifying_key());
    let mut challenge: Challenge = serde_json::from_value(vectors()["challenge"].clone()).unwrap();
    let mut nonces = Nonces::default();
    let first_sig = sign_challenge(&challenge, &c, &key, c.iat).unwrap();
    nonces
        .verify(&challenge, &c, &jwk, &first_sig, c.iat)
        .unwrap();
    challenge.nonce = "cd".repeat(32);
    challenge.ts += 2 * CLOCK_SKEW_S + 1;
    let second_sig = sign_challenge(&challenge, &c, &key, challenge.ts).unwrap();
    nonces
        .verify(&challenge, &c, &jwk, &second_sig, challenge.ts)
        .unwrap();
    assert_eq!(nonces.prune(challenge.ts), 0);
    assert_eq!(nonces.prune(challenge.ts + 2 * CLOCK_SKEW_S + 1), 1);
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
fn host_auth_bad_signatures_preserve_counter_but_signed_skew_consumes_it() {
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
    let mut unknown = headers.clone();
    unknown.host = "unknown-host".into();
    assert!(auth
        .verify("POST", path, body, &unknown, headers.timestamp)
        .is_err());
    assert!(auth
        .verify("POST", path, body, &headers, headers.timestamp + 60000)
        .is_err());
    let fresh = HostHeaders::sign(
        "test-host",
        "POST",
        path,
        body,
        headers.timestamp,
        headers.counter + 1,
        &SigningKey::from_bytes(&[4; 32]),
    )
    .unwrap();
    auth.verify("POST", path, body, &fresh, headers.timestamp + 60000)
        .unwrap();
    assert!(auth
        .verify("POST", path, body, &headers, headers.timestamp)
        .is_err());
}

#[test]
fn jwks_refetch_schedule_limits_unknown_keys_and_retains_last_good_set() {
    let token = sign_token(&claims(), "current", &SigningKey::from_bytes(&[1; 32])).unwrap();
    let unknown = sign_token(&claims(), "unknown", &SigningKey::from_bytes(&[9; 32])).unwrap();
    let mut cache = KeySet::default();
    assert!(cache.should_refetch(&token, 1000));
    cache
        .refresh(Ok(
            serde_json::from_value(vectors()["jwks"].clone()).unwrap()
        ))
        .unwrap();
    assert!(!cache.should_refetch(&unknown, 1059));
    assert!(cache.should_refetch(&unknown, 1060));
    assert!(cache.refresh(Err(AuthError::Auth)).is_err());
    assert!(!cache.should_refetch(&unknown, 1119));
    assert!(cache.should_refetch(&unknown, 1120));
    assert!(cache.refresh(Ok(Jwks { keys: vec![] })).is_err());
    assert!(!cache.should_refetch(&token, 1719));
    assert!(cache.should_refetch(&token, 1720));
    assert!(verify_token(&token, &cache, "test-host", None, Purpose::Run, 1700000000).is_ok());
}

#[test]
fn exact_claims_and_nullable_cold_placement_are_enforced() {
    use ed25519_dalek::Signer;
    let mut c = claims();
    c.placement.vm = None;
    let key = SigningKey::from_bytes(&[1; 32]);
    let token = sign_token(&c, "current", &key).unwrap();
    assert_eq!(
        verify_token(&token, &keys(), "test-host", None, Purpose::Run, c.iat)
            .unwrap()
            .placement
            .vm,
        None
    );
    let mut value = serde_json::to_value(&c).unwrap();
    value["extra"] = json!(true);
    let header = base64url(br#"{"alg":"EdDSA","kid":"current","typ":"ck-job+jwt"}"#);
    let input = format!(
        "{header}.{}",
        base64url(&serde_json::to_vec(&value).unwrap())
    );
    let token = format!(
        "{input}.{}",
        base64url(&key.sign(input.as_bytes()).to_bytes())
    );
    assert_eq!(
        verify_token(&token, &keys(), "test-host", None, Purpose::Run, c.iat),
        Err(AuthError::Auth)
    );
    value.as_object_mut().unwrap().remove("extra");
    value["placement"].as_object_mut().unwrap().remove("vm");
    assert!(serde_json::from_value::<Claims>(value).is_err());
}

/// A refusal names the first check that failed, and carries the job id only
/// once the broker's signature over it has verified.
#[test]
fn refused_tokens_name_their_first_failed_check() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let c = claims();
    let now = c.iat;
    let check = |token: &str, host: &str, now: u64| {
        check_token(token, &keys(), host, None, Purpose::Run, now).map(|_| ())
    };
    let job = Some(c.jti.clone());
    let refused = |check, jti: &Option<String>| {
        Err(TokenRefusal {
            error: AuthError::Auth,
            check,
            jti: jti.clone(),
        })
    };
    let valid = sign_token(&c, "current", &key).unwrap();
    assert_eq!(check(&valid, "test-host", now), Ok(()));
    assert_eq!(check("a.b", "test-host", now), refused("format", &None));
    let unknown = sign_token(&c, "unknown", &key).unwrap();
    assert_eq!(check(&unknown, "test-host", now), refused("kid", &None));
    let forged = sign_token(&c, "current", &SigningKey::from_bytes(&[9; 32])).unwrap();
    assert_eq!(
        check(&forged, "test-host", now),
        refused("signature", &None)
    );
    assert_eq!(check(&valid, "other-host", now), refused("aud", &job));
    let mut wrong = c.clone();
    wrong.mem_mib = 14336;
    let wrong = sign_token(&wrong, "current", &key).unwrap();
    assert_eq!(check(&wrong, "test-host", now), refused("mem_mib", &job));
    assert_eq!(
        check(&valid, "test-host", c.exp),
        Err(TokenRefusal {
            error: AuthError::Expired,
            check: "exp",
            jti: job,
        })
    );
}

/// A first-party host accepts exactly the broker's two per-job figures: the
/// current one (3.5 GiB per vCPU, 4 GiB to 16 GiB) and the previous
/// `max(16 GiB, 3.5 GiB per vCPU)`, which the runners contract keeps valid
/// until the operator retires it. Anything else is refused as `mem_mib`.
#[test]
fn first_party_host_accepts_the_current_and_previous_memory_figures_only() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let host = claims().aud;
    for (vcpus, current) in [
        (1, 4096),
        (2, 7168),
        (3, 10752),
        (4, 14336),
        (5, 16384),
        (21, 16384),
    ] {
        assert_eq!(job_memory_mib(vcpus), current, "{vcpus} vCPUs");
        let mut c = claims();
        c.vcpus = vcpus;
        for (mem_mib, accepted) in [
            (current, true),
            (memory_max_mib(vcpus), true),
            (current - 1, false),
            (current + 1, false),
            (memory_max_mib(vcpus) + 1, false),
        ] {
            c.mem_mib = mem_mib;
            let token = sign_token(&c, "current", &key).unwrap();
            let result = check_token(&token, &keys(), &host, None, Purpose::Run, c.iat);
            assert_eq!(
                result.map(|_| ()).map_err(|refusal| refusal.check),
                if accepted { Ok(()) } else { Err("mem_mib") },
                "{vcpus} vCPUs, {mem_mib} MiB"
            );
        }
    }
}

/// A Boat sandbox accepts its size's usable memory between 1 GiB per vCPU and
/// the fleet formula.
#[test]
fn boat_audience_accepts_sandbox_memory_within_the_shared_range() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let mut c = claims();
    c.aud = "boat:bx_test".into();
    c.placement.host = c.aud.clone();
    c.vcpus = 8;
    for (mem_mib, accepted) in [
        (8191, false),
        (8192, true),
        (13312, true),
        (13444, true),
        (memory_max_mib(8), true),
        (memory_max_mib(8) + 1, false),
    ] {
        c.mem_mib = mem_mib;
        let token = sign_token(&c, "current", &key).unwrap();
        let result = check_token(&token, &keys(), "boat:bx_test", None, Purpose::Run, c.iat);
        assert_eq!(
            result.map(|_| ()).map_err(|refusal| refusal.check),
            if accepted { Ok(()) } else { Err("mem_mib") },
            "{mem_mib} MiB"
        );
    }
    assert_eq!(boat_memory_range(8), 8192..=28672);
    assert_eq!(boat_memory_range(4), 4096..=16384);
}
