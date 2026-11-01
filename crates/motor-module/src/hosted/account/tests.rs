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
        Box::pin(async move {
            tokio::task::yield_now().await;
            reply
        })
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
async fn account_token_repeated_calls_hit_vault_once() {
    let vault = FakeVault::new(vec![
        Ok(token("first.jwt.fixture")),
        Ok(token("second.jwt.fixture")),
    ]);
    let client = vault.client();
    assert_eq!(client.bearer().await.unwrap(), "first.jwt.fixture");
    assert_eq!(client.bearer().await.unwrap(), "first.jwt.fixture");
    assert_eq!(
        *vault.requests.lock().unwrap(),
        vec![json!({
            "method":"credential.account_token", "params":{"audience":"cortexkit-runners", "min_ttl_ms":90000}
        })]
    );
}

#[tokio::test]
async fn account_token_below_ttl_floor_is_refetched() {
    let vault = FakeVault::new(vec![
        Ok(token("first.jwt.fixture")),
        Ok(token("second.jwt.fixture")),
    ]);
    let client = vault.client();
    assert_eq!(client.bearer().await.unwrap(), "first.jwt.fixture");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    client.cached.lock().await.as_mut().unwrap().expires_at_ms = now_ms + 89_999;
    assert_eq!(client.bearer().await.unwrap(), "second.jwt.fixture");
    assert_eq!(vault.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn account_token_concurrent_callers_share_one_refresh() {
    let vault = FakeVault::new((0..16).map(|_| Ok(token("shared.jwt.fixture"))).collect());
    let client = vault.client();
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let client = client.clone();
        let barrier = barrier.clone();
        calls.spawn(async move {
            barrier.wait().await;
            client.bearer().await
        });
    }
    barrier.wait().await;
    while let Some(call) = calls.join_next().await {
        assert_eq!(call.unwrap().unwrap(), "shared.jwt.fixture");
    }
    assert_eq!(vault.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn account_token_failed_fetch_is_not_cached() {
    for failure in [
        Ok(refusal("auth_required")),
        Ok(refusal("transient")),
        Ok(json!({"result":{"jwt":"invalid"}})),
        Err(io::Error::other("secret-transport-text")),
    ] {
        let vault = FakeVault::new(vec![failure, Ok(token("recovered.jwt.fixture"))]);
        let client = vault.client();
        assert!(client.bearer().await.is_err());
        assert_eq!(client.bearer().await.unwrap(), "recovered.jwt.fixture");
        assert_eq!(vault.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn account_token_delayed_401_preserves_newer_cache() {
    let vault = FakeVault::new(vec![
        Ok(token("first.jwt.fixture")),
        Ok(token("second.jwt.fixture")),
    ]);
    let client = vault.client();
    let old = client.bearer().await.unwrap();
    client.invalidate(&old).await;
    assert_eq!(client.bearer().await.unwrap(), "second.jwt.fixture");
    client.invalidate(&old).await;
    assert_eq!(client.bearer().await.unwrap(), "second.jwt.fixture");
    assert_eq!(vault.requests.lock().unwrap().len(), 2);
}

async fn account_broker(
    responses: Vec<(&'static str, u16)>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for (bearer, status) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
                assert!(head.len() < 32768);
            }
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("POST /v1/leases HTTP/1.1\r\n"));
            assert!(head
                .lines()
                .any(|line| line == format!("authorization: Bearer {bearer}")));
            let len = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; len];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"fixture":true})
            );
            let body = r#"{"ok":true}"#;
            socket.write_all(format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (url, task)
}

#[tokio::test]
async fn account_broker_401_refetches_once_and_stops_on_second_401() {
    for status in [200, 401] {
        let (url, server) = account_broker(vec![
            ("first.jwt.fixture", 401),
            ("second.jwt.fixture", status),
        ])
        .await;
        let vault = FakeVault::new(vec![
            Ok(token("first.jwt.fixture")),
            Ok(token("second.jwt.fixture")),
            Ok(token("third.jwt.fixture")),
        ]);
        let tokens = vault.client();
        let mut route = super::super::HostedJobState::new(&crate::Config::default());
        route.broker_url = url;
        let client = super::super::Client::new(route).with_account_tokens(Some(tokens.clone()));
        let result: Result<Value, Error> =
            client.post("/v1/leases", &json!({"fixture":true})).await;
        assert_eq!(
            result,
            if status == 200 {
                Ok(json!({"ok":true}))
            } else {
                Err(Error::AccountInvalid)
            }
        );
        assert_eq!(vault.requests.lock().unwrap().len(), 2);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        // Success keeps the replacement; another refusal leaves no rejected bearer cached.
        assert_eq!(
            tokens.bearer().await.unwrap(),
            if status == 200 {
                "second.jwt.fixture"
            } else {
                "third.jwt.fixture"
            }
        );
        assert_eq!(
            vault.requests.lock().unwrap().len(),
            if status == 200 { 2 } else { 3 }
        );
    }
}

#[tokio::test]
async fn account_broker_403_does_not_refresh() {
    let http = super::super::test_http::Http::new(|_, _| (403, json!({"error":"denied"}))).await;
    let vault = FakeVault::new(vec![Ok(token("first.jwt.fixture"))]);
    let mut route = super::super::HostedJobState::new(&crate::Config::default());
    route.broker_url = http.url.clone();
    let client = super::super::Client::new(route).with_account_tokens(Some(vault.client()));
    let result: Result<Value, Error> = client.post("/v1/leases", &json!({})).await;
    assert_eq!(result, Err(Error::Auth));
    assert_eq!(vault.requests.lock().unwrap().len(), 1);
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn account_check_401_refetches_once_and_stops_on_second_401() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for status in [200, 401] {
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let http = super::super::test_http::Http::new(move |_, body| {
            assert_eq!(body, &json!({}));
            let status = if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                401
            } else {
                status
            };
            (status, json!({}))
        })
        .await;
        let jwt = |iat| {
            let payload = motor_lease::base64url(
                &serde_json::to_vec(&json!({
                    "aud": AUDIENCE, "account": "fixture-account", "iat": iat, "exp": u64::MAX
                }))
                .unwrap(),
            );
            format!("fixture.{payload}.signature")
        };
        let vault = FakeVault::new(vec![Ok(token(&jwt(1))), Ok(token(&jwt(2)))]);
        let tokens = vault.client();
        let config = crate::Config {
            broker_url: Some(http.url.clone()),
            ..crate::Config::default()
        };
        let result = super::super::check::check(Some(&tokens), &config).await;
        assert_eq!(result.status, if status == 200 { "ok" } else { "invalid" });
        assert_eq!(result.broker_status, (status == 200).then_some(200));
        assert_eq!(
            result.claims.map(|claims| claims.iat),
            (status == 200).then_some(2)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(vault.requests.lock().unwrap().len(), 2);
        if status == 401 {
            assert!(tokens.cached.lock().await.is_none());
        }
    }
}

#[tokio::test]
async fn account_finish_second_401_stops_retries_and_keeps_settlement_pending() {
    use crate::{hosted::HostedJobState, storage::Storage};
    use motor_protocol::{Outcome, TerminalRecord, Uuid};
    let http = super::super::test_http::Http::new(|path, _| {
        assert!(path.ends_with("/finish"));
        (401, json!({"error":"unauthorized"}))
    })
    .await;
    let scratch = cortexkit_test_support::ScratchDir::new("account-finish");
    let storage = Storage::open(&scratch.join("state")).unwrap();
    let config = crate::Config {
        broker_url: Some(http.url.clone()),
        ..crate::Config::default()
    };
    let mut route = HostedJobState::new(&config);
    route.placement = Some(motor_lease::Placement {
        host: "boat:test".into(),
        vm: Some("test".into()),
    });
    route.max_runtime_s = Some(30);
    let id = Uuid::now_v7();
    storage.hosted_route(id, route.clone()).unwrap();
    let vault = FakeVault::new(vec![
        Ok(token("first.jwt.fixture")),
        Ok(token("second.jwt.fixture")),
    ]);
    let tokens = vault.client();
    let client = super::super::Client::new(route).with_account_tokens(Some(tokens.clone()));
    let terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0);
    assert!(!client.finish(id, Some(&terminal), &storage).await.unwrap());
    assert_eq!(http.requests.lock().unwrap().len(), 2);
    assert_eq!(vault.requests.lock().unwrap().len(), 2);
    assert!(tokens.cached.lock().await.is_none());
    let state = storage.job(id).unwrap().hosted.unwrap();
    assert!(!state.finish_done);
    assert!(state.pending_finish.is_some());
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
        assert!(matches!(decode(reply), Err(Error::AccountInvalid)));
    }
    for (field, value) in [
        ("audience", json!("another-service")),
        ("expires_at_ms", json!(0)),
        ("expires_at_ms", json!(super::super::now_s() * 1000 + 15000)),
        ("expires_at_ms", json!(super::super::now_s() * 1000 + 60000)),
        ("account", json!("")),
        ("jwt", json!("")),
        ("jwt", json!("jwt\nheader")),
    ] {
        let mut reply = token("fixture.jwt");
        reply["result"][field] = value;
        assert!(
            matches!(decode(reply), Err(Error::AccountInvalid)),
            "{field}"
        );
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
    client.invalidate("secret.jwt.non_disclosure").await;
    let error = client.bearer().await.unwrap_err();
    tracing::warn!(detail = %error.detail(), "account request refused");
    let lines = captured.text();
    assert!(lines.contains("account log capture is active"));
    assert!(lines.contains("ck account login"));
    for secret in ["secret.jwt.non_disclosure", "private-vault-code"] {
        assert!(!lines.contains(secret), "secret reached logs");
    }
}
