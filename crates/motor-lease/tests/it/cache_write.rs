use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use motor_lease::{check_token, sign_token, AuthError, Claims, KeySet, LeaseResponse, Purpose};
use serde_json::Value;

fn vector() -> Value {
    serde_json::from_str(include_str!(
        "../../contracts/cloud/job-tokens-cache-write.json"
    ))
    .unwrap()
}

fn keys(v: &Value) -> KeySet {
    let mut keys = KeySet::default();
    keys.refresh(Ok(serde_json::from_value(v["jwks"].clone()).unwrap()))
        .unwrap();
    keys
}

fn signer(v: &Value) -> SigningKey {
    let seed = v["seed_hex"].as_str().unwrap();
    SigningKey::from_bytes(&std::array::from_fn(|i| {
        u8::from_str_radix(&seed[i * 2..i * 2 + 2], 16).unwrap()
    }))
}

fn decode(token: &str) -> Claims {
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap()
}

fn verify(token: &str, keys: &KeySet, c: &Claims) {
    let result = check_token(token, keys, &c.aud, Some(&c.jti), c.purpose, c.iat);
    if !c.aud.starts_with("boat:") && c.workspace_key_hash.is_none() {
        // Cache authorization does not waive the registered host's workspace binding.
        assert_eq!(
            result.unwrap_err().error,
            AuthError::MissingWorkspaceKeyHash
        );
    } else {
        assert_eq!(result.unwrap(), *c);
    }
}

#[test]
fn published_registered_tokens_preserve_claim_order_and_signed_bytes() {
    let v = vector();
    let keys = keys(&v);
    let key = signer(&v);
    let kid = v["key_id"].as_str().unwrap();
    let cases = v["registered"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for (case, (name, workspace, cache_write, burst)) in cases.iter().zip([
        ("switch_off", true, None, None),
        ("account_off", true, None, None),
        ("cache_only", false, Some(true), None),
        ("workspace_cache", true, Some(true), None),
        ("workspace_cache_burst", true, Some(true), Some(2048)),
    ]) {
        assert_eq!(case["name"], name);
        for (bytes_name, purpose) in [
            ("run_bytes", Purpose::Run),
            ("resigned_bytes", Purpose::Run),
            ("attach_bytes", Purpose::Attach),
        ] {
            let bytes = &case[bytes_name];
            let claims_json = bytes["claims"].as_str().unwrap();
            let c: Claims = serde_json::from_str(claims_json).unwrap();
            assert_eq!(c.purpose, purpose);
            assert_eq!(c.workspace_key_hash.is_some(), workspace);
            assert_eq!(c.cache_write, cache_write);
            assert_eq!(c.mem_burst_mib, burst);
            assert_eq!(serde_json::to_string(&c).unwrap(), claims_json);
            let token = bytes["job_token"].as_str().unwrap();
            assert_eq!(sign_token(&c, kid, &key).unwrap(), token);
            verify(token, &keys, &c);
        }
        for name in ["run", "rerequest", "resigned", "attach"] {
            let reply: LeaseResponse = serde_json::from_value(case[name]["body"].clone()).unwrap();
            let c = decode(&reply.job_token);
            assert_eq!(c.cache_write, cache_write);
            assert_eq!(sign_token(&c, kid, &key).unwrap(), reply.job_token);
            verify(&reply.job_token, &keys, &c);
        }
    }
}

#[test]
fn published_switch_off_and_legacy_tokens_remain_byte_identical() {
    let v = vector();
    let keys = keys(&v);
    let key = signer(&v);
    for (group, names) in [
        ("legacy", &["run", "attach"][..]),
        (
            "emission_disabled_after_reserve",
            &["run", "attach", "reenabled"][..],
        ),
    ] {
        for name in names {
            let token = v[group][name]["body"]["job_token"].as_str().unwrap();
            let c = decode(token);
            assert_eq!(c.cache_write, (*name == "reenabled").then_some(true));
            assert_eq!(
                sign_token(&c, v["key_id"].as_str().unwrap(), &key).unwrap(),
                token
            );
            verify(token, &keys, &c);
        }
    }
}

#[test]
fn published_reader_cases_accept_false_and_reject_invalid_values() {
    let v = vector();
    let keys = keys(&v);
    for case in v["reader_cases"].as_array().unwrap() {
        let token = case["job_token"].as_str().unwrap();
        let parsed = serde_json::from_str::<Claims>(case["claims"].as_str().unwrap());
        let accepted = case["registered_accepts"].as_bool().unwrap();
        assert_eq!(parsed.is_ok(), accepted, "{case}");
        let result = check_token(
            token,
            &keys,
            "host-a",
            None,
            Purpose::Run,
            v["clock_ms"].as_u64().unwrap() / 1000,
        );
        assert_eq!(result.is_ok(), accepted, "{case}");
        if accepted {
            let c = result.unwrap();
            assert_eq!(c.cache_write, Some(false));
            assert_eq!(c.cache_write == Some(true), case["grants_writes"]);
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                case["claims"].as_str().unwrap()
            );
            assert_eq!(
                sign_token(&c, v["key_id"].as_str().unwrap(), &signer(&v)).unwrap(),
                token
            );
        } else {
            assert_eq!(result.unwrap_err().check, "claims");
        }
    }
}

#[test]
fn published_boat_tokens_reject_cache_write_even_when_false() {
    let v = vector();
    let keys = keys(&v);
    let key = signer(&v);
    for name in ["run", "attach"] {
        let token = v["boat"][name]["body"]["job_token"].as_str().unwrap();
        let c = decode(token);
        assert_eq!(c.cache_write, None);
        verify(token, &keys, &c);
        assert_eq!(
            sign_token(&c, v["key_id"].as_str().unwrap(), &key).unwrap(),
            token
        );
        for value in [Some(false), Some(true)] {
            let mut c = c.clone();
            c.cache_write = value;
            let token = sign_token(&c, v["key_id"].as_str().unwrap(), &key).unwrap();
            assert_eq!(
                check_token(&token, &keys, &c.aud, Some(&c.jti), c.purpose, c.iat)
                    .unwrap_err()
                    .check,
                "cache_write"
            );
        }
        for case in v["reader_cases"].as_array().unwrap() {
            let mut raw = serde_json::to_value(&c).unwrap();
            raw["cache_write"] = case["cache_write"].clone();
            // Keep every other Boat binding intact and sign the malformed payload,
            // so a refusal cannot be explained by its signature or audience.
            let input = format!(
                "{}.{}",
                token.split('.').next().unwrap(),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&raw).unwrap())
            );
            let malformed = format!(
                "{input}.{}",
                URL_SAFE_NO_PAD.encode(key.sign(input.as_bytes()).to_bytes())
            );
            let result = check_token(&malformed, &keys, &c.aud, Some(&c.jti), c.purpose, c.iat);
            assert_eq!(result.is_ok(), case["boat_accepts"].as_bool().unwrap());
            assert_eq!(
                result.unwrap_err().check,
                if case["registered_accepts"] == true {
                    "cache_write"
                } else {
                    "claims"
                }
            );
        }
    }
}
