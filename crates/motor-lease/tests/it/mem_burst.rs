use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::SigningKey;
use motor_lease::*;
use serde_json::{json, Value};

fn vector() -> Value {
    serde_json::from_str(include_str!("../../contracts/cloud/mem-burst.json")).unwrap()
}

fn decode(token: &str) -> Claims {
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn burst_vectors_decode_and_verify() {
    let vector = vector();
    let seed = vector["test_only_job_seed_hex"].as_str().unwrap();
    let key = SigningKey::from_bytes(&std::array::from_fn(|i| {
        u8::from_str_radix(&seed[i * 2..i * 2 + 2], 16).unwrap()
    }));
    let mut keys = KeySet::default();
    keys.refresh(Ok(Jwks {
        keys: vec![JwksKey::from_key(
            vector["key_id"].as_str().unwrap(),
            &key.verifying_key(),
        )],
    }))
    .unwrap();
    for (case, (mem, burst)) in vector["registered"].as_array().unwrap().iter().zip([
        (4096, 2048),
        (16384, 8192),
        (65536, 8192),
    ]) {
        for pointer in [
            "/run/body",
            "/rerequest/body",
            "/resigned/body",
            "/reattach/reply/body",
            "/replace/reply/body",
        ] {
            let response: LeaseResponse =
                serde_json::from_value(case.pointer(pointer).unwrap().clone()).unwrap();
            let claims = decode(&response.job_token);
            assert_eq!(claims.mem_mib, mem);
            assert_eq!(claims.mem_burst_mib, Some(burst));
            assert_eq!(
                sign_token(&claims, vector["key_id"].as_str().unwrap(), &key).unwrap(),
                response.job_token
            );
            assert_eq!(
                check_token(
                    &response.job_token,
                    &keys,
                    &response.placement.host,
                    Some(&response.job_id),
                    claims.purpose,
                    claims.iat
                )
                .unwrap(),
                claims
            );
        }
    }
    for name in ["run", "attach"] {
        let token = vector["boat"][name]["body"]["job_token"].as_str().unwrap();
        let claims = decode(token);
        assert_eq!(claims.mem_burst_mib, None);
        check_token(
            token,
            &keys,
            &claims.aud,
            Some(&claims.jti),
            claims.purpose,
            claims.iat,
        )
        .unwrap();
    }
}

#[test]
fn optional_burst_claim_validation_table() {
    let mut claims = decode(
        vector()["registered"][0]["run"]["body"]["job_token"]
            .as_str()
            .unwrap(),
    );
    let key = SigningKey::from_bytes(&[7; 32]);
    let mut keys = KeySet::default();
    keys.refresh(Ok(Jwks {
        keys: vec![JwksKey::from_key("test", &key.verifying_key())],
    }))
    .unwrap();
    for (mem, burst, accepted) in [
        (4096, None, true),
        (4096, Some(2048), true),
        (7168, Some(3584), true),
        (16384, Some(8192), true),
        (65536, Some(8192), true),
        (4096, Some(0), false),
        (4096, Some(2047), false),
        (4096, Some(2049), false),
        (65536, Some(8193), false),
        (4096, Some(u64::MAX), false),
    ] {
        claims.mem_mib = mem;
        claims.mem_burst_mib = burst;
        let token = sign_token(&claims, "test", &key).unwrap();
        let result = check_token(
            &token,
            &keys,
            &claims.aud,
            Some(&claims.jti),
            claims.purpose,
            claims.iat,
        );
        assert_eq!(
            result.is_ok(),
            accepted,
            "mem={mem} burst={burst:?}: {result:?}"
        );
        if !accepted {
            assert_eq!(result.unwrap_err().check, "mem_burst_mib");
        }
    }
    let mut body = serde_json::to_value(&claims).unwrap();
    for invalid in [
        json!(null),
        json!(-1),
        json!(2048.5),
        json!(2048.0),
        json!("2048"),
    ] {
        body["mem_burst_mib"] = invalid;
        assert!(serde_json::from_value::<Claims>(body.clone()).is_err());
    }
    claims.mem_burst_mib = None;
    assert!(!serde_json::to_string(&claims)
        .unwrap()
        .contains("mem_burst_mib"));
    claims.aud = "boat:test".into();
    claims.mem_mib = 14336;
    claims.mem_burst_mib = Some(7168);
    assert!(!claims.mem_burst_mib_accepted());
}

#[test]
fn burst_usage_vectors_preserve_canonical_bytes_and_hashes() {
    let vector = vector();
    for report in vector["reports"].as_array().unwrap() {
        let usage: Usage = serde_json::from_value(report["request"]["body"].clone()).unwrap();
        let bytes = serde_json::to_vec(&usage).unwrap();
        assert_eq!(sha256_hex(&bytes), report["usage_hash"].as_str().unwrap());
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            report["request"]["body"]
        );
    }
    let mut invalid = vector["invalid_oom_kill"]["request"]["body"].clone();
    assert!(serde_json::from_value::<Usage>(invalid.clone()).is_err());
    invalid["oom_kill"] = Value::Null;
    assert!(serde_json::from_value::<Usage>(invalid).is_err());
}
