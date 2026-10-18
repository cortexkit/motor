use super::*;
use std::{collections::VecDeque, sync::Mutex};

pub(crate) struct FakeVault {
    replies: Mutex<VecDeque<io::Result<Value>>>,
    pub requests: Mutex<Vec<Value>>,
}
impl FakeVault {
    pub fn new(replies: Vec<io::Result<Value>>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    pub fn client(self: &Arc<Self>) -> Arc<AccountTokens> {
        Arc::new(AccountTokens::with_target(self.clone()))
    }
}
impl Target for FakeVault {
    fn request(&self, request: Value) -> Reply<'_> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected vault retry");
        Box::pin(async move { reply })
    }
}
pub(crate) fn token(jwt: &str) -> Value {
    json!({"result": {
        "jwt": jwt, "expires_at_ms": u64::MAX, "audience": AUDIENCE,
        "account": "01ARZ3NDEKTSV4RRFFQ69G5FAV"
    }})
}
pub(crate) fn refusal(class: &str) -> Value {
    json!({"result":{"error":{"class":class, "code":"private-vault-code", "retry_after_ms":4321}}})
}

#[tokio::test]
async fn account_token_requests_are_exact_and_never_cached() {
    let vault = FakeVault::new(vec![
        Ok(token("first.jwt.fixture")),
        Ok(token("second.jwt.fixture")),
    ]);
    let client = vault.client();
    assert_eq!(client.bearer().await.unwrap(), "first.jwt.fixture");
    assert_eq!(client.bearer().await.unwrap(), "second.jwt.fixture");
    assert_eq!(
        *vault.requests.lock().unwrap(),
        vec![
            json!({
                "method":"credential.account_token", "params":{"audience":"cortexkit-runners", "min_ttl_ms":30000}
            });
            2
        ]
    );
}

#[tokio::test]
async fn account_token_classes_are_terminal_except_retryable_transient() {
    for (class, expected) in [
        ("not_found", Error::AccountNotFound),
        ("auth_required", Error::AccountAuthRequired),
        ("transient", Error::AccountTransient(Some(4321))),
        ("permanent", Error::AccountPermanent),
        ("future_class", Error::AccountInvalid),
    ] {
        let vault = FakeVault::new(vec![Ok(refusal(class))]);
        assert_eq!(vault.client().bearer().await, Err(expected));
        assert_eq!(vault.requests.lock().unwrap().len(), 1);
    }
    let vault = FakeVault::new(vec![Err(io::Error::other("secret-transport-text"))]);
    assert_eq!(
        vault.client().bearer().await,
        Err(Error::AccountTransient(None))
    );
}

#[test]
fn account_token_reply_validation_fails_closed() {
    for reply in [
        json!({}),
        json!({"result":{"jwt":"jwt"}}),
        json!({"result":{"error":{"class":"not_found"}}}),
        json!({"result":{"error":{"class":"transient","code":"x","retry_after_ms":-1}}}),
    ] {
        assert_eq!(decode(reply), Err(Error::AccountInvalid));
    }
    for (field, value) in [
        ("audience", json!("another-service")),
        ("expires_at_ms", json!(0)),
        ("expires_at_ms", json!(super::super::now_s() * 1000 + 15000)),
        ("account", json!("")),
        ("jwt", json!("")),
        ("jwt", json!("jwt\nheader")),
    ] {
        let mut reply = token("fixture.jwt");
        reply["result"][field] = value;
        assert_eq!(decode(reply), Err(Error::AccountInvalid), "{field}");
    }
}

#[tokio::test]
async fn account_jwt_and_raw_vault_errors_never_reach_logs() {
    let captured = crate::logging::Captured::default();
    let _guard = crate::logging::scoped(captured.clone());
    tracing::warn!("account log capture is active");
    let vault = FakeVault::new(vec![
        Ok(token("secret.jwt.non_disclosure")),
        Ok(refusal("auth_required")),
    ]);
    let client = vault.client();
    assert_eq!(client.bearer().await.unwrap(), "secret.jwt.non_disclosure");
    let error = client.bearer().await.unwrap_err();
    tracing::warn!(detail = %error.detail(), "account request refused");
    let lines = captured.text();
    assert!(lines.contains("account log capture is active"));
    assert!(lines.contains("ck account login"));
    for secret in ["secret.jwt.non_disclosure", "private-vault-code"] {
        assert!(!lines.contains(secret), "secret reached logs");
    }
}
