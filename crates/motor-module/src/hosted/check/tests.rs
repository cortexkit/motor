use super::*;
use crate::hosted::account::tests::{refusal, token, FakeVault};

#[tokio::test]
async fn hosted_check_maps_vault_refusals_exactly_without_private_text() {
    for (class, expected) in [
        ("auth_required", "auth_required"),
        ("not_found", "not_found"),
        ("transient", "transient"),
        ("permanent", "permanent"),
        ("future", "invalid"),
    ] {
        let vault = FakeVault::new(vec![Ok(refusal(class))]);
        let result = check(Some(&vault.client()), &crate::Config::default()).await;
        assert_eq!(result.status, expected);
        assert_eq!(
            result.retry_after_ms,
            (class == "transient").then_some(4321)
        );
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("private-vault-code"));
    }
}

#[tokio::test]
async fn hosted_check_returns_only_nonsecret_claims_with_switch_off() {
    let logs = crate::logging::Captured::default();
    let _guard = crate::logging::scoped(logs.clone());
    tracing::warn!("hosted check log capture active");
    let payload = motor_lease::base64url(serde_json::to_vec(&serde_json::json!({"aud":"cortexkit-runners", "sub":"acct:test", "iat":1,"exp":u64::MAX,"secret":"not-for-output"})).unwrap().as_slice());
    let jwt = format!("secret-header.{payload}.secret-signature");
    let vault = FakeVault::new(vec![Ok(token(&jwt))]);
    let result = check(Some(&vault.client()), &crate::Config::default()).await;
    assert_eq!(result.status, "ok");
    let output = serde_json::to_string(&result).unwrap();
    for secret in [
        &jwt,
        "secret-header",
        "secret-signature",
        "not-for-output",
        &payload,
    ] {
        assert!(!output.contains(secret));
        assert!(!logs.text().contains(secret));
    }
    assert!(output.contains("acct:test"));
    assert!(logs.text().contains("hosted check log capture active"));
}

#[tokio::test]
async fn hosted_check_probes_invalid_lease_body_and_returns_only_http_status() {
    let http = crate::hosted::test_http::Http::new(|path, body| {
        assert_eq!(path, "/v1/leases");
        assert_eq!(body, &serde_json::json!({}));
        (400, serde_json::json!({"error":"private-broker-text"}))
    })
    .await;
    let payload=motor_lease::base64url(&serde_json::to_vec(&serde_json::json!({"aud":"cortexkit-runners","sub":"acct:test","iat":1,"exp":u64::MAX})).unwrap());
    let jwt = format!("secret-header.{payload}.secret-signature");
    let vault = FakeVault::new(vec![Ok(token(&jwt))]);
    let config = crate::Config {
        runner_instance: "test".into(),
        broker_url: Some(http.url.clone()),
        broker_token_source: crate::BrokerTokenSource::Fake,
        ..crate::Config::default()
    };
    let result = check(Some(&vault.client()), &config).await;
    assert_eq!(result.broker_status, Some(400));
    let output = serde_json::to_string(&result).unwrap();
    assert!(!output.contains("private-broker-text"));
    assert!(!output.contains(&jwt));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}
