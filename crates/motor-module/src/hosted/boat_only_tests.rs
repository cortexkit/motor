use super::*;
use crate::hosted::test_http::Http;
use motor_protocol::{Network, Platform};
use serde_json::{json, Value};

fn vector() -> Value {
    serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/boat-only-lease.json"
    ))
    .unwrap()
}

#[test]
fn boat_only_body_matches_cloud_canonical_bytes_and_keeps_placement_last() {
    let vector = vector();
    let expected: LeaseRequest =
        serde_json::from_value(vector["granted"]["request"]["body"].clone()).unwrap();
    let mut request = RunRequest::new(
        "/home/test/repo",
        "/home/test/repo",
        "/home/test/repo",
        "true",
    )
    .with_platform(Platform::Linux)
    .with_weight_hint(expected.vcpus)
    .with_timeout(expected.timeout_s);
    // The broker vector allows networking; Motor's operator policy is tested separately.
    request.network = Some(Network::Outbound);
    let mut body = lease_body(
        Uuid::parse_str(&expected.job_id).unwrap(),
        &request,
        (
            expected.workspace_key_hash.clone(),
            expected.repo_hash.clone(),
        ),
        expected.placement_hint.clone(),
        expected.avoid_host.clone(),
        expected.requester_jwk.clone(),
        Some(true),
    )
    .unwrap();
    body.placement = Some(motor_lease::LeasePlacement::Boat);
    assert_eq!(body, expected);
    let canonical = tests::canonical_lease_bytes(&body);
    assert_eq!(canonical, vector["canonical_lease_bytes"]);
    assert_eq!(
        motor_lease::sha256_hex(canonical.as_bytes()),
        vector["lease_hash"]
    );
    assert!(serde_json::to_string(&body)
        .unwrap()
        .ends_with(r#","placement":"boat"}"#));
    body.mem_mib = Some(4096);
    assert!(serde_json::to_string(&body)
        .unwrap()
        .ends_with(r#","mem_mib":4096,"placement":"boat"}"#));
    body.allow_spill = Some(false);
    assert_eq!(invalid_lease_field(&body), Some("placement"));
}

#[tokio::test]
async fn stored_boat_only_body_replays_identically_and_policy_refusal_is_final() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let vector = vector();
    let forbidden = vector["forbidden"]["reply"]["body"].clone();
    let http = Http::new(move |_, _| (403, forbidden.clone())).await;
    let scratch = cortexkit_test_support::ScratchDir::new("boat-only-replay");
    let config = Config {
        broker_url: Some(http.url.clone()),
        broker_token_source: BrokerTokenSource::Fake,
        runner_instance: "boat-only-test".into(),
        ..Config::default()
    };
    let body: LeaseRequest =
        serde_json::from_value(vector["granted"]["request"]["body"].clone()).unwrap();
    let id = Uuid::parse_str(&body.job_id).unwrap();
    let original = tests::canonical_lease_bytes(&body);
    let storage = Arc::new(Storage::open(&scratch).unwrap());
    storage.create(id, 300, None, None, false).unwrap();
    let mut route = HostedJobState::new(&config);
    route.boat_only = true;
    route.hosted_test = true;
    route.hosted_no_fallback = true;
    route.lease_request = Some(body);
    storage.hosted_route(id, route).unwrap();
    storage.shutdown().unwrap();
    drop(storage);
    let stored: Value =
        serde_json::from_slice(&std::fs::read(scratch.join("state.json")).unwrap()).unwrap();
    let saved = &stored["jobs"][id.to_string()]["hosted"];
    assert_eq!(saved["boat_only"], true);
    assert!(saved["lease_request"].get("placement").is_none());
    let storage = Arc::new(Storage::open(&scratch).unwrap());
    let route = storage.job(id).unwrap().hosted.unwrap();
    assert!(route.boat_only && route.hosted_no_fallback);
    let client = Client::new(route);
    let request = RunRequest::new(
        "/home/test/repo",
        "/home/test/repo",
        "/home/test/repo",
        "true",
    )
    .with_timeout(100);
    assert_eq!(
        client.run(id, &request, &storage).await.err(),
        Some(Error::BoatOnlyNotAllowed)
    );
    let sent = http.requests.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let replay: LeaseRequest = serde_json::from_value(sent[0].1.clone()).unwrap();
    assert_eq!(tests::canonical_lease_bytes(&replay), original);
    assert!(serde_json::to_string(&replay)
        .unwrap()
        .ends_with(r#","placement":"boat"}"#));
    let error = Error::BoatOnlyNotAllowed;
    assert_eq!(error.code(), "boat_only_not_allowed");
    assert_eq!(error.reason(), RefusalReason::RequestRejected);
    assert_eq!(error.retry_after_ms(), None);
    assert!(!error.replaceable());
    assert_eq!(
        client
            .post::<_, Value>("/v1/leases/test/reattach", &json!({}))
            .await,
        Err(Error::Auth)
    );
    storage.shutdown().unwrap();
}

#[tokio::test]
async fn placement_bad_request_uses_existing_typed_refusal() {
    let reply = vector()["allow_spill_conflict"]["reply"].clone();
    let http = Http::new(move |_, _| {
        (
            reply["status"].as_u64().unwrap() as u16,
            reply["body"].clone(),
        )
    })
    .await;
    let route = HostedJobState::new(&Config {
        broker_url: Some(http.url.clone()),
        ..Config::default()
    });
    let client = Client::new(route);
    assert_eq!(
        client
            .post_authorized::<_, Value>("/v1/leases", &json!({}), "test".into())
            .await,
        Err(Error::RequestInvalid(RequestField::new("placement")))
    );
}
