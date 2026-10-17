//! Interim fixtures pending a pinned `cortexkit-cloud/test-vectors/runners/` mirror.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn token(header: &Value, claims: &Value, key: &SigningKey) -> String {
    let input = format!(
        "{}.{}",
        b64(serde_json::to_vec(header).unwrap()),
        b64(serde_json::to_vec(claims).unwrap())
    );
    format!("{input}.{}", b64(key.sign(input.as_bytes()).to_bytes()))
}

fn main() {
    assert_eq!(
        std::env::args().nth(1).as_deref(),
        Some("--write"),
        "pass --write to regenerate fixtures"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts");
    let current = SigningKey::from_bytes(&[1; 32]);
    let next = SigningKey::from_bytes(&[2; 32]);
    let requester = SigningKey::from_bytes(&[3; 32]);
    let host_key = SigningKey::from_bytes(&[4; 32]);
    let x = b64(requester.verifying_key().as_bytes());
    let thumbprint_input = format!("{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{x}\"}}");
    let thumbprint = b64(Sha256::digest(thumbprint_input.as_bytes()));
    let header = json!({"alg":"EdDSA", "kid":"current", "typ":"ck-job+jwt"});
    let claims = json!({
        "iss":"https://runners.cortexkit.io", "aud":"test-host", "jti":"test-job",
        "sub":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", "purpose":"run", "iat":1700000000u64,
        "exp":1700000300u64, "vcpus":2, "mem_mib":16384, "platform":"linux", "net":false,
        "placement":{"host":"test-host","vm":"test-vm"}, "max_s":30, "cnf":{"jkt":thumbprint}
    });
    let mut cases = vec![
        json!({"name":"valid_run", "token":token(&header,&claims,&current), "now_s":1700000000u64, "result":"ok"}),
    ];
    for (name, field, value) in [
        ("wrong_alg", "alg", json!("HS256")),
        ("wrong_typ", "typ", json!("JWT")),
        ("unknown_kid", "kid", json!("unknown")),
        ("crit_empty", "crit", json!([])),
        ("crit_null", "crit", Value::Null),
    ] {
        let mut changed = header.clone();
        changed[field] = value;
        cases.push(json!({"name":name,"token":token(&changed,&claims,&current),"now_s":1700000000u64,"result":"auth"}));
    }
    for (name, pointer, value) in [
        ("wrong_issuer", "/iss", json!("https://invalid.example")),
        ("wrong_audience", "/aud", json!("other-host")),
        ("wrong_job", "/jti", json!("other-job")),
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
        let mut changed = claims.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        cases.push(json!({"name":name,"token":token(&header,&changed,&current),"now_s":1700000000u64,"result":"auth"}));
    }
    cases.push(json!({"name":"expired_at_connect", "token":token(&header,&claims,&current),"now_s":1700000300u64,"result":"expired"}));
    let mut boundary = claims.clone();
    boundary["iat"] = json!(1700000060u64);
    cases.push(json!({"name":"iat_skew_boundary", "token":token(&header,&boundary,&current),"now_s":1700000000u64,"result":"ok"}));
    let mut next_header = header.clone();
    next_header["kid"] = json!("next");
    cases.push(json!({"name":"announced_next", "token":token(&next_header,&claims,&next),"now_s":1700000000u64,"result":"ok"}));
    let mut attach = claims.clone();
    attach["purpose"] = json!("attach");
    cases.push(json!({"name":"valid_attach", "token":token(&header,&attach,&current),"now_s":1700000000u64,"result":"ok"}));
    let mut tampered: Vec<_> = token(&header, &claims, &current)
        .split('.')
        .map(str::to_owned)
        .collect();
    let mut changed = claims.clone();
    changed["max_s"] = json!(300);
    tampered[1] = b64(serde_json::to_vec(&changed).unwrap());
    cases.push(json!({"name":"tampered_claims", "token":tampered.join("."), "now_s":1700000000u64,"result":"auth"}));

    let nonce = "ab".repeat(32);
    let challenge_input =
        format!("motor-host-challenge-v1\n{nonce}\ntest-host\ntest-job\n1700000000\n");
    let heartbeat_body =
        r#"{"free_vcpus":2,"free_mem_mib":18432,"queued":0,"warm":[],"running":[],"healthy":true}"#;
    let host_input = format!(
        "ck-host-v1\nPOST\n/v1/hosts/test-host/heartbeat\n{}\n1700000000000\n1700000000000000",
        hex(heartbeat_body)
    );
    let vectors = json!({
        "provenance":"Generated from reviewed interim formats; not CLOUD canonical vectors",
        "jwks":{"keys":[
            {"kid":"current","kty":"OKP","crv":"Ed25519","x":b64(current.verifying_key().as_bytes())},
            {"kid":"next","kty":"OKP","crv":"Ed25519","x":b64(next.verifying_key().as_bytes())}
        ]},
        "claims":claims, "token_cases":cases,
        "requester_jwk":{"kty":"OKP","crv":"Ed25519","x":x},
        "thumbprint_input":thumbprint_input, "thumbprint":thumbprint,
        "challenge":{"nonce":nonce,"host_id":"test-host","ts":1700000000u64},
        "challenge_input":challenge_input,
        "challenge_signature":b64(requester.sign(challenge_input.as_bytes()).to_bytes()),
        "host_jwk":{"kty":"OKP","crv":"Ed25519","x":b64(host_key.verifying_key().as_bytes())},
        "host_body":heartbeat_body, "host_input":host_input,
        "host_headers":{"host":"test-host","timestamp":1700000000000u64,"counter":1700000000000000u64,"signature":b64(host_key.sign(host_input.as_bytes()).to_bytes())}
    });
    std::fs::create_dir_all(root.join("vectors")).unwrap();
    let vector_bytes = format!("{}\n", serde_json::to_string_pretty(&vectors).unwrap());
    std::fs::write(root.join("vectors/interim.json"), vector_bytes).unwrap();
    let files: Vec<_> = [
        "interim-pins.txt",
        "lease-details.txt",
        "vectors/interim.json",
    ]
    .iter()
    .map(|path| json!({"path":path,"sha256":hex(std::fs::read(root.join(path)).unwrap())}))
    .collect();
    let manifest = json!({
        "source":{"kind":"reviewed-interim-pins", "date":"2026-10-17", "canonical_repository":"cortexkit-cloud", "canonical_contract":"docs/runners-contract.md", "canonical_vectors":"test-vectors/runners/", "revision":null, "replacement_required_before":"hosted client integration", "confirmed_details":"lease-details.txt"},
        "files":files
    });
    std::fs::write(
        root.join("digest-manifest.json"),
        format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
    )
    .unwrap();
    println!(
        "generated 19 token vectors, requester and host signing vectors; hashed 3 mirror files"
    );
}
