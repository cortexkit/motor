use super::*;

#[test]
fn hosted_config_defaults_omit_all_keys_and_fake_is_forbidden_on_main() {
    let config = Config::default();
    let json = serde_json::to_value(&config).unwrap();
    for key in [
        "hosted_runners",
        "hosted_repositories",
        "broker_url",
        "broker_token_source",
        "spill_to_hosted",
        "spill_after_s",
        "spill_max_vcpus",
        "spill_avoid_host",
    ] {
        assert!(json.get(key).is_none(), "{key}");
    }
    let mut config = config;
    config.broker_token_source = BrokerTokenSource::Fake;
    assert!(config.validate().is_err());
    config.runner_instance = "test".into();
    assert!(config.validate().is_ok());
}

#[test]
fn hosted_paths_are_component_bounded() {
    for path in ["/Users/a/repo", "/home/a/repo"] {
        assert!(allowed_path(Path::new(path)));
    }
    for path in [
        "/Users2/repo",
        "/home2/repo",
        "/tmp/repo",
        "home/repo",
        "/home/../etc",
    ] {
        assert!(!allowed_path(Path::new(path)), "{path}");
    }
}

#[test]
fn hosted_refusals_use_legacy_reason_and_distinct_codes() {
    for (error, reason, code) in [
        (
            Error::Auth,
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_auth",
        ),
        (
            Error::Expired,
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_expired",
        ),
        (
            Error::Spki,
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_spki_mismatch",
        ),
        (
            Error::HostFull,
            RefusalReason::RunnerFull,
            "hosted_host_full",
        ),
        (
            Error::Capacity,
            RefusalReason::RunnerFull,
            "hosted_capacity",
        ),
        (
            Error::InternalFailure,
            RefusalReason::RequestRejected,
            "hosted_internal_failure",
        ),
        (
            Error::Connect,
            RefusalReason::Unreachable,
            "hosted_connect_failed",
        ),
        (
            Error::Broker,
            RefusalReason::Unreachable,
            "hosted_broker_unreachable",
        ),
        (
            Error::ReattachUnknown,
            RefusalReason::Unreachable,
            "hosted_broker_unreachable",
        ),
        (
            Error::NoTimeout,
            RefusalReason::RequestRejected,
            "hosted_no_timeout",
        ),
        (
            Error::Short,
            RefusalReason::RequestRejected,
            "hosted_max_s_too_short",
        ),
        (
            Error::Path,
            RefusalReason::RequestRejected,
            "hosted_path_outside_roots",
        ),
        (
            Error::Budget,
            RefusalReason::RequestRejected,
            "hosted_budget_exhausted",
        ),
        (
            Error::Platform,
            RefusalReason::PlatformUnsupported,
            "hosted_platform_unsupported",
        ),
    ] {
        assert_eq!(error.reason(), reason);
        assert_eq!(error.code(), code);
    }
    assert!(!Error::Auth.replaceable());
    assert!(!Error::Expired.replaceable());
    assert!(!Error::InternalFailure.replaceable());
}

#[tokio::test]
async fn refused_lease_release_job_unknown_is_not_unreachable() {
    let h = Harness::new(Behavior::Normal).await;
    h.broker.lock().unwrap().knobs.refusal = Some((503, "no_capacity".into()));
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let mut route = h.route.clone();
    route.spill_wait_ms = Some(0);
    let storage = stored(&scratch, id, route.clone());
    let client = Client::new(route);
    assert_eq!(
        client
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .err(),
        Some(Error::Capacity)
    );
    assert_eq!(
        client.release(id).await.err().map(Error::code),
        Some("hosted_not_granted")
    );
    // An unknown route is not evidence of a refused or already-released lease.
    assert_eq!(
        client
            .post::<_, serde_json::Value>("/missing", &serde_json::json!({}))
            .await,
        Err(Error::Broker)
    );
    assert_eq!(
        *h.requests.lock().unwrap(),
        vec![
            "/v1/leases".to_string(),
            format!("/v1/leases/{id}/release"),
            "/missing".to_string()
        ]
    );
}

#[tokio::test]
async fn job_unknown_is_distinct_only_for_404_reattach() {
    let h = Harness::new(Behavior::Normal).await;
    for (status, code, path, expected) in [
        (
            404,
            "job_unknown",
            "/v1/leases/job/reattach",
            Error::ReattachUnknown,
        ),
        (
            404,
            "job_unknown",
            "/v1/leases/job/release",
            Error::NotGranted,
        ),
        (404, "job_unknown", "/v1/leases", Error::Broker),
        (404, "job_unknown", "/v1/leases/job/finish", Error::Broker),
        (404, "job_unknown", "/missing", Error::Broker),
        (503, "job_unknown", "/v1/leases/job/reattach", Error::Broker),
        (404, "other", "/v1/leases/job/reattach", Error::Broker),
    ] {
        let http =
            super::test_http::Http::new(move |_, _| (status, serde_json::json!({"error": code})))
                .await;
        let mut route = h.route.clone();
        route.broker_url = http.url.clone();
        let client = Client::new(route);
        assert_eq!(
            client
                .post::<_, serde_json::Value>(path, &serde_json::json!({}))
                .await,
            Err(expected),
            "{status} {code} {path}",
        );
    }
    assert_eq!(
        Error::ReattachUnknown.retry_after_ms(),
        Error::Broker.retry_after_ms()
    );
    assert_eq!(
        Error::ReattachUnknown.replaceable(),
        Error::Broker.replaceable()
    );
}

#[tokio::test]
async fn hosted_internal_failure_is_not_retried_before_or_after_lease_ok() {
    for behavior in [
        Behavior::Refuse("internal_failure"),
        Behavior::RefuseHello("internal_failure"),
    ] {
        let h = Harness::new(behavior).await;
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        let storage = stored(&scratch, id, h.route.clone());
        assert_eq!(
            Client::new(h.route.clone())
                .run(id, &Harness::request(&scratch.0), &storage)
                .await
                .err(),
            Some(Error::InternalFailure)
        );
        assert_eq!(h.requests.lock().unwrap().len(), 1);
        assert_eq!(h.tokens.load(Ordering::SeqCst), 1);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn hosted_cutover_config_is_opt_in_and_roundtrips_without_changing_ssh_defaults() {
    let config: Config = serde_json::from_value(serde_json::json!({
        "hosted_runners": true, "broker_url": "https://runners.cortexkit.io"
    }))
    .unwrap();
    config.validate().unwrap();
    assert!(serde_json::to_value(&config)
        .unwrap()
        .get("hosted_runners")
        .is_none());
    assert_eq!(config.broker_token_source, BrokerTokenSource::CkAccount);
    let mut off = config.clone();
    off.broker_url = None;
    assert_eq!(
        serde_json::to_value(&off).unwrap(),
        serde_json::to_value(Config::default()).unwrap()
    );
    assert_eq!(config.runner_instance, Config::default().runner_instance);
    assert_eq!(
        config.ssh_config_override,
        Config::default().ssh_config_override
    );
}

#[test]
fn hosted_billing_seconds_keep_three_decimals() {
    let mut state = HostedJobState::default();
    assert_eq!(state.billed_line(), "billed: pending");
    state.billing = BillingStatus::Unavailable;
    assert_eq!(state.billed_line(), "billed: unavailable");
    state.billed_vcpu_ms = Some(6001);
    assert_eq!(state.billed_line(), "billed: 6.001 vCPU-s");
}

mod account;
pub(crate) mod capacity;
pub(crate) mod harness;
mod replacement;
mod settled_reattach;
use harness::*;
use motor_protocol::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn stored(scratch: &Scratch, id: Uuid, route: HostedJobState) -> Arc<Storage> {
    let storage = Arc::new(Storage::open(&scratch.0.join("state")).unwrap());
    storage
        .create(id, 30, None, Some("test".into()), false)
        .unwrap();
    storage.hosted_route(id, route).unwrap();
    storage
}

#[test]
fn spill_provisioning_budget_caps_only_spills_and_respects_remaining_wait() {
    let scratch = Scratch::new();
    let storage = Storage::open(&scratch.0.join("state")).unwrap();
    for (limit_s, spill_wait_ms, expected_ms) in [
        (300, Some(45_000), 180_000),
        (120, Some(45_000), 75_000),
        (30, Some(45_000), 0),
        (300, None, 300_000),
    ] {
        let id = Uuid::now_v7();
        storage.create(id, limit_s, None, None, false).unwrap();
        storage
            .hosted_route(
                id,
                HostedJobState {
                    spill_wait_ms,
                    ..HostedJobState::default()
                },
            )
            .unwrap();
        assert_eq!(spill_wait_budget_ms(&storage.job(id).unwrap()), expected_ms);
    }
}

#[tokio::test]
async fn hosted_self_signed_pin_challenge_and_protocol_work() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let storage = stored(&scratch, id, h.route.clone());
    let (channel, lease) = Client::new(h.route.clone())
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    assert_eq!(lease.max_s, 30);
    assert_eq!(h.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(channel.platforms, vec!["linux"]);
}

#[tokio::test]
async fn hosted_wrong_spki_sends_no_token_and_does_not_replace() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    // Keep the token bound to the same requester key, but replace the expected
    // certificate identity in endpoint_spki_sha256 with a different digest.
    let key = requester().unwrap();
    let client = Client::new(h.route.clone());
    let salt = [9; 32];
    let mut lease: motor_lease::LeaseResponse = client
        .post(
            "/v1/leases",
            &motor_lease::LeaseRequest {
                job_id: id.to_string(),
                vcpus: 2,
                platform: "linux-x64".into(),
                net: false,
                workspace_key_hash: keyed_hash(&salt, "ck-ws-v1", scratch.0.to_str().unwrap())
                    .unwrap(),
                repo_hash: keyed_hash(&salt, "ck-repo-v1", scratch.0.to_str().unwrap()).unwrap(),
                placement_hint: None,
                avoid_host: None,
                timeout_s: 30,
                requester_jwk: motor_lease::RequesterJwk::from_key(&key.verifying_key()),
                allow_spill: Some(true),
            },
        )
        .await
        .unwrap();
    lease.endpoint_spki_sha256 = motor_lease::base64url(&[0; 32]);
    assert_eq!(
        super::tls::connect(id, &lease, &key, motor_lease::Purpose::Run)
            .await
            .err()
            .map(|failure| failure.error),
        Some(Error::Spki)
    );
    assert_eq!(h.tokens.load(Ordering::SeqCst), 0);
    assert_eq!(h.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn hosted_capacity_replaces_three_times_but_auth_and_expiry_do_not() {
    for (reason, error, count) in [
        ("host_full", Error::HostFull, 3),
        ("capacity", Error::Capacity, 3),
        ("auth", Error::Auth, 1),
        ("expired", Error::Expired, 1),
    ] {
        let h = Harness::new(Behavior::Refuse(reason)).await;
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        let storage = stored(&scratch, id, h.route.clone());
        assert_eq!(
            Client::new(h.route.clone())
                .run(id, &Harness::request(&scratch.0), &storage)
                .await
                .err(),
            Some(error)
        );
        let requests = h.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|p| p.as_str() == "/v1/leases")
                .count(),
            count
        );
        assert_eq!(
            requests.iter().filter(|p| p.ends_with("/replace")).count(),
            usize::from(reason == "host_full")
        );
        assert_eq!(h.tokens.load(Ordering::SeqCst), count);
        assert!(h
            .iats
            .lock()
            .unwrap()
            .windows(2)
            .all(|pair| pair[1] > pair[0]));
    }
}

#[tokio::test]
async fn hosted_wrong_challenge_host_is_terminal_auth() {
    let h = Harness::new(Behavior::WrongHost).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let storage = stored(&scratch, id, h.route.clone());
    assert_eq!(
        Client::new(h.route.clone())
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .err(),
        Some(Error::Auth)
    );
    assert_eq!(h.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn hosted_lost_lease_response_rerequests_and_short_cap_stops_before_upload() {
    let h = Harness::new(Behavior::Normal).await;
    h.drop_lease.store(true, Ordering::SeqCst);
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let storage = stored(&scratch, id, h.route.clone());
    let (channel, _) = Client::new(h.route.clone())
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    drop(channel);
    assert_eq!(h.requests.lock().unwrap().len(), 2);
    assert_eq!(h.tokens.load(Ordering::SeqCst), 1);
    h.broker.lock().unwrap().knobs.max_s = Some(5);
    let id2 = Uuid::now_v7();
    storage
        .create(id2, 30, None, Some("test".into()), false)
        .unwrap();
    storage.hosted_route(id2, h.route.clone()).unwrap();
    assert_eq!(
        Client::new(h.route.clone())
            .run(id2, &Harness::request(&scratch.0), &storage)
            .await
            .err(),
        Some(Error::Short)
    );
    assert_eq!(h.tokens.load(Ordering::SeqCst), 1);
    Client::new(h.route.clone()).release(id2).await.unwrap();
    assert!(h
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .ends_with("/release"));
}

#[test]
fn hosted_salt_persists_private_and_domain_separated_hashes() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let storage = Storage::open(&scratch.0.join("state")).unwrap();
    let salt = storage.hosted_salt().unwrap();
    let ws = keyed_hash(&salt, "ck-ws-v1", scratch.0.to_str().unwrap()).unwrap();
    let repo = keyed_hash(&salt, "ck-repo-v1", scratch.0.to_str().unwrap()).unwrap();
    assert_eq!(ws.len(), 43);
    assert_ne!(ws, repo);
    assert_eq!(
        std::fs::metadata(scratch.0.join("state/hosted-salt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(storage);
    let storage = Storage::open(&scratch.0.join("state")).unwrap();
    assert_eq!(storage.hosted_salt().unwrap(), salt);
    assert_eq!(
        keyed_hash(&salt, "ck-ws-v1", &format!("{}/.", scratch.0.display())).unwrap(),
        ws
    );
}

async fn seed_running(h: &Harness, scratch: &Scratch, terminal: Option<TerminalRecord>, id: Uuid) {
    let storage = stored(scratch, id, h.route.clone());
    let (_, lease) = Client::new(h.route.clone())
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    storage
        .hosted_update(id, |s| {
            s.lease(&lease);
            s.terminal = terminal;
        })
        .unwrap();
}

async fn terminal(mut stream: crate::JobStream) -> TerminalRecord {
    let record = tokio::time::timeout(Duration::from_secs(30), stream.records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match record {
        StreamRecord::Terminal(t) => t,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn hosted_route_survives_switch_flip_restart_cancel_status_and_attach() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    seed_running(&h, &scratch, None, id).await;
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let config = Config {
        runner_instance: "other".into(),
        ..Config::default()
    };
    let motor =
        crate::Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    let t = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 23 });
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.billing, BillingStatus::Available);
    assert_eq!(state.billed_vcpu_ms, Some(6001));
    assert_eq!(
        motor
            .job_status(id)
            .await
            .unwrap()
            .terminal
            .unwrap()
            .outcome,
        t.outcome
    );
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert!(h
        .requests
        .lock()
        .unwrap()
        .iter()
        .skip(1)
        .all(|p| p.ends_with("/reattach")));
}

#[tokio::test]
async fn hosted_terminal_disconnect_reattaches_three_times_and_preserves_exit_pending() {
    let h = Harness::new(Behavior::DropTerminal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    seed_running(&h, &scratch, None, id).await;
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    let t = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 23 });
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        BillingStatus::Pending
    );
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.ends_with("/reattach"))
            .count(),
        3
    );
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    h.finished.store(true, Ordering::SeqCst);
    assert_eq!(
        terminal(motor.attach(AttachRequest::new(id, 0)).unwrap())
            .await
            .outcome,
        t.outcome
    );
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        BillingStatus::Unavailable
    );
}

#[tokio::test]
async fn hosted_finished_without_terminal_is_outcome_unknown() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    seed_running(&h, &scratch, None, id).await;
    h.finished.store(true, Ordering::SeqCst);
    let motor = crate::Motor::with_connector(
        h.config(),
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    assert_eq!(
        terminal(motor.attach(AttachRequest::new(id, 0)).unwrap())
            .await
            .outcome,
        Outcome::OutcomeUnknown
    );
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        BillingStatus::Unavailable
    );
}

#[tokio::test]
async fn hosted_invalid_requests_refuse_before_snapshot_lease_or_ssh() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    for (request, code) in [
        (
            RunRequest::new("/home/missing", "/home/missing", "/home/missing", "true"),
            "hosted_no_timeout",
        ),
        (
            RunRequest::new("/tmp/missing", "/tmp/missing", "/tmp/missing", "true")
                .with_timeout(30),
            "hosted_path_outside_roots",
        ),
    ] {
        let stream = motor.run(request).unwrap();
        let id = stream.job_id;
        assert!(matches!(
            terminal(stream).await.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected
            }
        ));
        let state = motor.hosted_state(id).unwrap();
        assert!(state.hosted_no_fallback);
        assert_eq!(state.hosted_code.as_deref(), Some(code));
    }
    let reply = motor
        .prepare(PrepareRequest::new("/tmp/missing", "/tmp/missing", "base"))
        .await
        .unwrap();
    assert!(reply.refusal_detail.unwrap().contains("hosted_no_fallback"));
    assert!(motor.hosted_state(reply.transfer_id).is_none());
    motor.status().await;
    assert_eq!(h.requests.lock().unwrap().len(), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn return_paths_hosted_limits_refuse_before_lease_and_keep_no_fallback() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    for returns in [
        crate::ReturnOptions {
            return_paths: vec!["/home/missing/out".into()],
            return_max_bytes: RETURN_MAX_BYTES + 1,
            return_chunked: false,
        },
        crate::ReturnOptions {
            return_paths: vec!["/home/missing/out".into(); 65],
            return_max_bytes: 1,
            return_chunked: false,
        },
        crate::ReturnOptions {
            return_paths: vec![format!("/home/missing/{}", "x".repeat(4096))],
            return_max_bytes: 1,
            return_chunked: false,
        },
        crate::ReturnOptions {
            return_paths: vec!["/home/elsewhere/out".into()],
            return_max_bytes: 1,
            return_chunked: false,
        },
    ] {
        let stream = motor
            .run_with_returns(
                RunRequest::new("/home/missing", "/home/missing", "/home/missing", "true")
                    .with_timeout(30),
                returns,
            )
            .unwrap();
        let id = stream.job_id;
        let t = terminal(stream).await;
        assert_eq!(
            t.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected
            }
        );
        let state = motor.hosted_state(id).unwrap();
        assert!(state.hosted_no_fallback);
        assert_eq!(state.hosted_code.as_deref(), Some("return_paths_exceeded"));
    }
    assert_eq!(h.requests.lock().unwrap().len(), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn return_paths_hosted_failure_is_stored_without_changing_remote_terminal() {
    let h = Harness::new(Behavior::ReturnCapFailure).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    seed_running(&h, &scratch, None, id).await;
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    let t = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 23 });
    assert_eq!(t.ran, Some(Ran::Remote));
    let result = motor.return_result(id).unwrap();
    assert_eq!(result.returned_files, None);
    assert_eq!(
        result.return_paths_failed,
        Some(ReturnPathsFailed {
            measured_bytes: 2,
            cap_bytes: 1,
            cause: ReturnFailureCause::OverCap
        })
    );
    let replayed = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(replayed, t);
    assert_eq!(motor.return_result(id), Some(result));
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

// This ignored test snapshots a real repository below the current checkout.
// Hosted workspace paths must be under /Users or /home, so it requires a checkout
// in one of those roots; the ordinary tests work from any checkout location.
#[tokio::test]
#[ignore = "requires checkout under /Users or /home"]
async fn hosted_real_snapshot_lost_run_ack_never_resubmits_and_refusals_release() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("hosted-fixture-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&path).unwrap();
    let scratch = Scratch(path);
    assert!(allowed_path(&std::fs::canonicalize(&scratch.0).unwrap()));
    let repo = scratch.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--initial-branch=main"]);
    std::fs::write(repo.join("source"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-m",
        "initial",
    ]);
    for behavior in [
        Behavior::DropRunAck,
        Behavior::Refuse("host_full"),
        Behavior::Refuse("auth"),
        Behavior::Refuse("capacity"),
        Behavior::ConnectFailure,
        Behavior::DropHave,
        Behavior::Normal,
    ] {
        let h = Harness::new(behavior).await;
        if matches!(behavior, Behavior::Refuse("capacity")) {
            h.release_admitted.store(true, Ordering::SeqCst);
        }
        if matches!(behavior, Behavior::DropRunAck) {
            h.broker.lock().unwrap().knobs.max_s = Some(30);
        }
        if matches!(behavior, Behavior::Normal) {
            h.broker.lock().unwrap().knobs.max_s = Some(5);
        }
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let root = scratch.0.join(format!("state-{}", Uuid::now_v7()));
        let motor = crate::Motor::with_connector(h.config(), &root, ssh.clone()).unwrap();
        let stream = motor
            .run(Harness::request(&repo).with_timeout(120))
            .unwrap();
        let id = stream.job_id;
        let t = terminal(stream).await;
        let state = motor.hosted_state(id).unwrap();
        if matches!(behavior, Behavior::DropRunAck) {
            assert_eq!(t.outcome, Outcome::Exit { code: 23 });
            assert_eq!(state.billing, BillingStatus::Available);
            assert_eq!(h.runs.load(Ordering::SeqCst), 1);
            let frames = h.frames.lock().unwrap();
            let run = frames
                .iter()
                .find_map(|f| if let Frame::Run(r) = f { Some(r) } else { None })
                .unwrap();
            assert_eq!(run.timeout, Some(30));
            assert_eq!(run.weight_hint, Some(2));
            drop(frames);
            assert_eq!(
                h.frames
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|f| matches!(f, Frame::Run(_)))
                    .count(),
                1
            );
            assert!(!h
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.ends_with("/release")));
        } else if matches!(behavior, Behavior::Refuse("capacity")) {
            assert_eq!(t.outcome, Outcome::Exit { code: 23 });
            assert_eq!(state.billing, BillingStatus::Available);
            assert!(!state.hosted_no_fallback);
            assert_eq!(h.runs.load(Ordering::SeqCst), 0);
            assert!(h
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.ends_with("/reattach")));
        } else {
            assert!(
                matches!(t.outcome, Outcome::RefusedBeforeStart { .. }),
                "{t:?}"
            );
            assert!(state.hosted_no_fallback);
            if matches!(behavior, Behavior::ConnectFailure | Behavior::DropHave) {
                assert_eq!(
                    h.requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|p| p.as_str() == "/v1/leases")
                        .count(),
                    3
                );
                assert_eq!(state.hosted_code.as_deref(), Some("hosted_connect_failed"));
            }
            assert_eq!(h.runs.load(Ordering::SeqCst), 0);
            assert!(h
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .ends_with("/release"));
        }
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn hosted_linux_platform_is_architecture_qualified_and_windows_is_not_hosted() {
    assert_eq!(lease_platform(None), Ok("linux-x64"));
    assert_eq!(lease_platform(Some(&Platform::Linux)), Ok("linux-x64"));
    assert_eq!(
        lease_platform(Some(&Platform::Windows)),
        Err(Error::Rejected)
    );
}

#[test]
fn cloud_hmac_and_client_request_bodies_match_canonical_lease_vector() {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/lease.json"
    ))
    .unwrap();
    let tokens: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/job-tokens.json"
    ))
    .unwrap();
    let decode_hex = |text: &str| -> [u8; 32] {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u8::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    };
    let salt = decode_hex(v["hash_derivation"]["test_only_salt_hex"].as_str().unwrap());
    let workspace = keyed_hash_input(
        &salt,
        v["hash_derivation"]["workspace_input"].as_str().unwrap(),
    );
    let repo = keyed_hash_input(&salt, v["hash_derivation"]["repo_input"].as_str().unwrap());
    assert_eq!(workspace, v["request"]["body"]["workspace_key_hash"]);
    assert_eq!(repo, v["request"]["body"]["repo_hash"]);
    let key = SigningKey::from_bytes(&decode_hex(
        tokens["test_only_requester_seed_hex"].as_str().unwrap(),
    ));
    let request = RunRequest::new(
        "/home/test/workspace",
        "/home/test/workspace/repo",
        "/home/test/workspace",
        "true",
    )
    .with_platform(Platform::Linux)
    .with_weight_hint(v["request"]["body"]["vcpus"].as_u64().unwrap() as u32)
    .with_timeout(v["request"]["body"]["timeout_s"].as_u64().unwrap())
    .with_network(Network::Outbound);
    let id = Uuid::parse_str(v["request"]["body"]["job_id"].as_str().unwrap()).unwrap();
    let body = lease_body(
        id,
        &request,
        (workspace, repo),
        Some(serde_json::from_value(v["request"]["body"]["placement_hint"].clone()).unwrap()),
        serde_json::from_value(v["request"]["body"]["avoid_host"].clone()).unwrap(),
        RequesterJwk::from_key(&key.verifying_key()),
        // An ordinary spilled lease, as `Client::run` builds it for a job that
        // was not routed to hosted runners by repository.
        Some(true),
    )
    .unwrap();
    assert_eq!(serde_json::to_value(&body).unwrap(), v["request"]["body"]);
    // The broker's lease hash takes the fields in this order, and
    // `avoid_host` comes right after `placement_hint`. ck-motor sends
    // `allow_spill` where the hash places it, before `requester_jwk`; the
    // broker accepts members in any order.
    let text = serde_json::to_string(&body).unwrap();
    let positions: Vec<_> = [
        "job_id",
        "vcpus",
        "platform",
        "net",
        "workspace_key_hash",
        "repo_hash",
        "placement_hint",
        "avoid_host",
        "timeout_s",
        "allow_spill",
        "requester_jwk",
    ]
    .iter()
    .map(|key| text.find(&format!("\"{key}\":")).unwrap())
    .collect();
    assert!(positions.is_sorted(), "{text}");
    assert_eq!(
        serde_json::to_value(reattach_body(&key)).unwrap(),
        v["reattach"]["request"]["body"]
    );
    assert_eq!(
        serde_json::to_value(ReleaseRequest::default()).unwrap(),
        v["release"]["request"]["body"]
    );
}

#[tokio::test]
async fn hosted_job_released_stops_reattaching_without_resubmitting() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    seed_running(&h, &scratch, None, id).await;
    Client::new(h.route.clone()).release(id).await.unwrap();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor =
        crate::Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone()).unwrap();
    let result = terminal(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(result.outcome, Outcome::OutcomeUnknown);
    assert_eq!(
        motor.hosted_state(id).unwrap().billing,
        BillingStatus::Unavailable
    );
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/reattach"))
            .count(),
        1
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

// The wire body places requester_jwk last. Remove exactly that member to
// project its serialized fields into the broker's ordinary lease-hash input.
// Hashing is a broker operation; this projection is only used by contract tests.
pub(crate) fn canonical_lease_bytes(body: &motor_lease::LeaseRequest) -> String {
    let wire = serde_json::to_string(body).unwrap();
    let requester = format!(
        ",\"requester_jwk\":{}}}",
        serde_json::to_string(&body.requester_jwk).unwrap()
    );
    format!(
        "{}}}",
        wire.strip_suffix(&requester)
            .expect("requester key is the final wire member")
    )
}

fn assert_client_lease_body_and_hash(body_bytes: &str, expected_hash: &str) {
    let expected: motor_lease::LeaseRequest = serde_json::from_str(body_bytes).unwrap();
    assert_eq!(expected.platform, "linux-x64");
    let mut request = RunRequest::new(
        "/home/test/repo",
        "/home/test/repo",
        "/home/test/repo",
        "true",
    )
    .with_platform(Platform::Linux)
    .with_timeout(expected.timeout_s)
    .with_weight_hint(expected.vcpus);
    if expected.net {
        request.network = Some(Network::Outbound);
    }
    let body = lease_body(
        Uuid::parse_str(&expected.job_id).unwrap(),
        &request,
        (
            expected.workspace_key_hash.clone(),
            expected.repo_hash.clone(),
        ),
        expected.placement_hint.clone(),
        expected.avoid_host.clone(),
        expected.requester_jwk.clone(),
        expected.allow_spill,
    )
    .unwrap();
    assert_eq!(body, expected);
    assert_eq!(
        motor_lease::sha256_hex(canonical_lease_bytes(&body).as_bytes()),
        expected_hash,
        "lease hash for {}",
        expected.job_id
    );
}

#[test]
fn cloud_explicit_only_lease_bodies_match_the_pinned_hashes() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/explicit-only-leases.json"
    ))
    .unwrap();
    let exchanges = vector["exchanges"].as_array().unwrap();
    assert_eq!(exchanges.len(), 2);
    for exchange in exchanges {
        assert_client_lease_body_and_hash(
            exchange["request"]["body_bytes"].as_str().unwrap(),
            exchange["lease_hash"].as_str().unwrap(),
        );
    }
}

#[test]
fn derived_non_null_placement_hint_lease_body_matches_the_cross_check_hash() {
    // This body is a derived cross-check, not a CLOUD lease-route exchange.
    let body_bytes = r#"{"job_id":"019e4567-89ab-7cde-8012-3456789abcd8","vcpus":2,"platform":"linux-x64","net":true,"workspace_key_hash":"WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW","repo_hash":"RRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRR","placement_hint":{"host":"staging-fra-1","vm":"t1"},"avoid_host":null,"timeout_s":100,"requester_jwk":{"kty":"OKP","crv":"Ed25519","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},"allow_spill":true}"#;
    assert_client_lease_body_and_hash(
        body_bytes,
        "bf25b1ca5d91ce3edfb35e7bf66efb259bb5b2877e4e3fc870b090d23b1f1282",
    );
}

#[test]
fn cloud_no_spill_request_serializes_to_the_exact_canonical_bytes_and_hash() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/no-spill-lease.json"
    ))
    .unwrap();
    let expected: motor_lease::LeaseRequest =
        serde_json::from_str(vector["request"]["body_bytes"].as_str().unwrap()).unwrap();
    let mut request = RunRequest::new(
        "/home/test/repo",
        "/home/test/repo",
        "/home/test/repo",
        "true",
    )
    .with_platform(Platform::Linux)
    .with_timeout(expected.timeout_s)
    .with_weight_hint(expected.vcpus);
    if expected.net {
        request.network = Some(Network::Outbound);
    }
    let body = lease_body(
        Uuid::parse_str(&expected.job_id).unwrap(),
        &request,
        (
            expected.workspace_key_hash.clone(),
            expected.repo_hash.clone(),
        ),
        expected.placement_hint.clone(),
        expected.avoid_host.clone(),
        expected.requester_jwk.clone(),
        Some(false),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&body).unwrap(),
        vector["request"]["body"]
    );
    let canonical = canonical_lease_bytes(&body);
    assert_eq!(canonical, vector["canonical_lease_bytes"].as_str().unwrap());
    assert_eq!(
        motor_lease::sha256_hex(canonical.as_bytes()),
        vector["lease_hash"]
    );
    let mut changed = body.clone();
    changed.requester_jwk =
        motor_lease::RequesterJwk::from_key(&SigningKey::from_bytes(&[8; 32]).verifying_key());
    assert_eq!(
        canonical_lease_bytes(&changed),
        canonical,
        "Requester rotation is not part of the lease hash"
    );
    changed.allow_spill = None;
    let absent = canonical_lease_bytes(&changed);
    changed.allow_spill = Some(true);
    let spill = canonical_lease_bytes(&changed);
    assert_ne!(
        absent, spill,
        "An absent member and true project to different lease-hash inputs"
    );
    assert_ne!(absent, canonical);
    assert_ne!(spill, canonical);
}

#[test]
fn non_null_hint_keeps_the_pinned_broker_canonical_member_order() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/no-spill-lease.json"
    ))
    .unwrap();
    let mut body: motor_lease::LeaseRequest =
        serde_json::from_str(vector["request"]["body_bytes"].as_str().unwrap()).unwrap();
    body.placement_hint = Some(Placement {
        host: "control-host".into(),
        vm: Some("t1".into()),
    });
    // This variant derives a non-null hint from the no-spill vector. Preserve its
    // projection order without claiming a broker-supplied hash for this variant.
    let expected = vector["canonical_lease_bytes"].as_str().unwrap().replace(
        "\"placement_hint\":null",
        "\"placement_hint\":{\"host\":\"control-host\",\"vm\":\"t1\"}",
    );
    let canonical = canonical_lease_bytes(&body);
    assert_eq!(canonical, expected);
    assert_ne!(
        motor_lease::sha256_hex(canonical.as_bytes()),
        vector["lease_hash"]
    );
}

/// CLOUD's `no-host-fits-lease.json` records a 16-vCPU repository-routed lease
/// (`allow_spill: false`) and its lease hash; ck-motor's body for the same job
/// has the same members and the same hash.
#[test]
fn cloud_no_host_fits_request_matches_the_vector_body_and_hash() {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/no-host-fits-lease.json"
    ))
    .unwrap();
    let expected: motor_lease::LeaseRequest =
        serde_json::from_str(vector["request"]["body_bytes"].as_str().unwrap()).unwrap();
    assert_eq!(expected.allow_spill, Some(false));
    let mut request = RunRequest::new(
        "/home/test/repo",
        "/home/test/repo",
        "/home/test/repo",
        "true",
    )
    .with_platform(Platform::Linux)
    .with_timeout(expected.timeout_s)
    .with_weight_hint(expected.vcpus);
    if expected.net {
        request.network = Some(Network::Outbound);
    }
    let body = lease_body(
        Uuid::parse_str(&expected.job_id).unwrap(),
        &request,
        (
            expected.workspace_key_hash.clone(),
            expected.repo_hash.clone(),
        ),
        expected.placement_hint.clone(),
        expected.avoid_host.clone(),
        expected.requester_jwk.clone(),
        Some(false),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&body).unwrap(),
        vector["request"]["body"]
    );
    assert_eq!(
        motor_lease::sha256_hex(canonical_lease_bytes(&body).as_bytes()),
        vector["lease_hash"]
    );
}

#[tokio::test]
async fn stored_legacy_cached_hint_is_replayed_without_changing_the_lease_hash() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let request = Harness::request(&scratch.0);
    let hint = Placement {
        host: "draining-host".into(),
        vm: Some("old-vm".into()),
    };
    let body = lease_body(
        id,
        &request,
        (
            motor_lease::base64url(&[1; 32]),
            motor_lease::base64url(&[2; 32]),
        ),
        Some(hint.clone()),
        None,
        RequesterJwk::from_key(&SigningKey::from_bytes(&[3; 32]).verifying_key()),
        Some(true),
    )
    .unwrap();
    let expected = canonical_lease_bytes(&body);
    let mut route = h.route.clone();
    route.fleet_routed = true;
    route.lease_request = Some(body);
    let storage = stored(&scratch, id, route.clone());
    drop(storage);

    let storage = Arc::new(Storage::open(&scratch.0.join("state")).unwrap());
    assert!(storage
        .job(id)
        .unwrap()
        .hosted
        .unwrap()
        .repository_placement_hint
        .is_none());
    for _ in 0..2 {
        drop(
            Client::new(route.clone())
                .run(id, &request, &storage)
                .await
                .unwrap()
                .0,
        );
    }
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    for bytes in bodies.iter() {
        let sent: LeaseRequest = serde_json::from_slice(bytes).unwrap();
        assert_eq!(sent.placement_hint, Some(hint.clone()));
        assert_eq!(canonical_lease_bytes(&sent), expected);
    }
    assert_eq!(
        canonical_lease_bytes(
            &storage
                .job(id)
                .unwrap()
                .hosted
                .unwrap()
                .lease_request
                .unwrap()
        ),
        expected
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stored_allow_spill_presence_and_value_survive_lease_re_requests() {
    for allow_spill in [None, Some(false), Some(true)] {
        let h = Harness::new(Behavior::Normal).await;
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        let request = Harness::request(&scratch.0);
        let body = lease_body(
            id,
            &request,
            (
                motor_lease::base64url(&[1; 32]),
                motor_lease::base64url(&[2; 32]),
            ),
            None,
            None,
            motor_lease::RequesterJwk::from_key(&SigningKey::from_bytes(&[3; 32]).verifying_key()),
            allow_spill,
        )
        .unwrap();
        let expected = canonical_lease_bytes(&body);
        let mut route = h.route.clone();
        // Deliberately disagree with the saved request: a config change must
        // not change either the presence or the value on a re-request.
        route.repository_routed = allow_spill != Some(false);
        route.lease_request = Some(body);
        let storage = stored(&scratch, id, route.clone());
        for _ in 0..2 {
            let result = Client::new(route.clone()).run(id, &request, &storage).await;
            if allow_spill.is_some() {
                drop(result.unwrap().0);
            } else {
                // A stored request missing allow_spill, which the broker
                // requires, is never sent: the client refuses it itself,
                // naming the member, instead of collecting the broker's 400.
                assert_eq!(
                    result.err(),
                    Some(Error::RequestInvalid(RequestField::new("allow_spill")))
                );
            }
        }
        let bodies = h.lease_bodies.lock().unwrap();
        assert_eq!(bodies.len(), if allow_spill.is_some() { 2 } else { 0 });
        for bytes in bodies.iter() {
            let sent: motor_lease::LeaseRequest = serde_json::from_slice(bytes).unwrap();
            assert_eq!(sent.allow_spill, allow_spill);
            assert_eq!(canonical_lease_bytes(&sent), expected);
        }
        assert_eq!(
            storage
                .job(id)
                .unwrap()
                .hosted
                .unwrap()
                .lease_request
                .unwrap()
                .allow_spill,
            allow_spill
        );
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    }
}

/// Exact broker reply bytes for lease-creation bad requests, including a 400
/// without a field so older brokers stay compatible.
pub(crate) fn cloud_bad_request_replies() -> Vec<(u16, String)> {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/bad-request.json"
    ))
    .unwrap();
    vector["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["request"]["pathname"] == "/v1/leases")
        .map(|case| {
            let status = case["reply"]["status"].as_u64().unwrap();
            assert_eq!(status, 400);
            (
                status as u16,
                case["reply"]["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The broker's recorded refusal of a closed account's new lease.
pub(crate) fn cloud_account_closed_reply() -> (u16, serde_json::Value) {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../../motor-lease/contracts/cloud/account-closure.json"
    ))
    .unwrap();
    let reply = &v["lease"]["reply"];
    assert_eq!(reply["retry_after"], serde_json::Value::Null);
    assert_eq!(
        serde_json::to_string(&reply["body"]).unwrap(),
        r#"{"error":"account_closed"}"#
    );
    (
        reply["status"].as_u64().unwrap() as u16,
        reply["body"].clone(),
    )
}

/// A 403 `account_closed` lease refusal is final whatever routed the job
/// hosted (configuration, a spill, repository routing or a direct hosted
/// test): one lease request, a typed refusal that tells the caller not to
/// repeat the request, no retry delay. A 403 on any other broker route is
/// still an authentication failure, since the contract sends `account_closed`
/// only on lease creation.
#[tokio::test]
async fn cloud_account_closed_lease_is_one_final_request_on_every_route() {
    let (status, body) = cloud_account_closed_reply();
    let broker = super::test_http::Http::new(move |path, _| {
        if path == "/v1/leases" || path.ends_with("/reattach") {
            (status, body.clone())
        } else {
            (404, serde_json::json!({"error": "job_unknown"}))
        }
    })
    .await;
    let h = Harness::new(Behavior::Normal).await;
    for kind in ["configured", "spilled", "repository_routed", "hosted_test"] {
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        let mut route = h.route.clone();
        route.broker_url = broker.url.clone();
        match kind {
            "spilled" => route.spill_wait_ms = Some(0),
            "repository_routed" => route.repository_routed = true,
            "hosted_test" => route.hosted_test = true,
            _ => {}
        }
        let storage = stored(&scratch, id, route.clone());
        let before = broker.requests.lock().unwrap().len();
        let client = Client::new(route);
        let error = client
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .err();
        assert_eq!(error, Some(Error::AccountClosed), "{kind}");
        let requests = broker.requests.lock().unwrap()[before..].to_vec();
        assert_eq!(requests.len(), 1, "{kind}: {requests:?}");
        assert_eq!(requests[0].0, "/v1/leases", "{kind}");
        assert_eq!(client.broker_code().as_deref(), Some("account_closed"));
        assert_eq!(
            client
                .post::<_, serde_json::Value>(
                    &format!("/v1/leases/{id}/reattach"),
                    &serde_json::json!({})
                )
                .await,
            Err(Error::Auth),
            "{kind}"
        );
    }
    let error = Error::AccountClosed;
    assert_eq!(error.reason(), RefusalReason::RequestRejected);
    assert_eq!(error.code(), "hosted_account_closed");
    assert_eq!(
        error.detail(),
        "hosted_account_closed: hosted account closed"
    );
    assert_eq!(error.retry_after_ms(), None);
    assert!(!error.replaceable());
    assert_eq!(h.tokens.load(Ordering::SeqCst), 0);
}

/// Closure refuses new jobs only. A job the broker placed before the account
/// closed is asked for again with its stored lease body and runs, while a new
/// job for the same account is refused `account_closed`.
#[tokio::test]
async fn closed_account_rerequest_of_a_placed_job_still_runs_but_a_new_job_is_refused() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let request = Harness::request(&scratch.0);
    let id = Uuid::now_v7();
    let path = scratch.0.to_str().unwrap();
    let salt = [9; 32];
    let body = lease_body(
        id,
        &request,
        (
            keyed_hash(&salt, "ck-ws-v1", path).unwrap(),
            keyed_hash(&salt, "ck-repo-v1", path).unwrap(),
        ),
        None,
        None,
        RequesterJwk::from_key(&requester().unwrap().verifying_key()),
        Some(true),
    )
    .unwrap();
    let placed: motor_lease::LeaseResponse = Client::new(h.route.clone())
        .post("/v1/leases", &body)
        .await
        .unwrap();
    h.broker
        .lock()
        .unwrap()
        .knobs
        .closed_subjects
        .insert("acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
    let mut route = h.route.clone();
    route.lease_request = Some(body);
    let storage = stored(&scratch, id, route);
    let (channel, lease) = Client::new(h.route.clone())
        .run(id, &request, &storage)
        .await
        .unwrap();
    drop(channel);
    assert_eq!(lease.lease_id, placed.lease_id);
    assert_eq!(h.tokens.load(Ordering::SeqCst), 1);

    let fresh = Uuid::now_v7();
    storage
        .create(fresh, 30, None, Some("test".into()), false)
        .unwrap();
    storage.hosted_route(fresh, h.route.clone()).unwrap();
    assert_eq!(
        Client::new(h.route.clone())
            .run(fresh, &request, &storage)
            .await
            .err(),
        Some(Error::AccountClosed)
    );
    assert_eq!(h.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == "/v1/leases")
            .count(),
        3
    );
}

/// `invalid_lease_field` names the member CLOUD's lease parser refuses with
/// 400 `bad_request`, for each rule of the pinned runners contract, and
/// accepts the bounds and the `boat:` placement hint the contract allows.
#[test]
fn invalid_lease_field_names_each_member_the_broker_refuses() {
    let request =
        RunRequest::new("/home/a/repo", "/home/a/repo", "/home/a/repo", "true").with_timeout(30);
    let valid = lease_body(
        Uuid::now_v7(),
        &request,
        ("A".repeat(43), "b-_9".repeat(10) + "abc"),
        None,
        Some("build-server-1".into()),
        RequesterJwk::from_key(&SigningKey::from_bytes(&[7; 32]).verifying_key()),
        Some(true),
    )
    .unwrap();
    assert_eq!(invalid_lease_field(&valid), None);
    type Mutation = fn(&mut LeaseRequest);
    let accepted: [(&str, Mutation); 8] = [
        ("timeout_s 10", |b| b.timeout_s = 10),
        ("timeout_s 7200", |b| b.timeout_s = 7200),
        ("timeout_s 7201", |b| b.timeout_s = 7201),
        ("timeout_s 21600", |b| b.timeout_s = 21600),
        ("vcpus 1", |b| b.vcpus = 1),
        ("vcpus 64", |b| b.vcpus = 64),
        ("avoid_host null", |b| b.avoid_host = None),
        ("boat hint", |b| {
            b.placement_hint = Some(Placement {
                host: "boat:bx_1".into(),
                vm: Some("bx_1".into()),
            })
        }),
    ];
    for (name, mutate) in accepted {
        let mut body = valid.clone();
        mutate(&mut body);
        assert_eq!(invalid_lease_field(&body), None, "{name}");
    }
    let refused: [(&str, Mutation); 16] = [
        ("job_id", |b| b.job_id.replace_range(14..15, "4")),
        ("job_id", |b| {
            b.job_id = "0190f0e2-1b2c-7d3e-8f4a-5b6c7d8e9F0a".into()
        }),
        ("vcpus", |b| b.vcpus = 0),
        ("vcpus", |b| b.vcpus = 65),
        ("platform", |b| b.platform = "linux x64".into()),
        ("workspace_key_hash", |b| {
            b.workspace_key_hash = "A".repeat(42)
        }),
        ("repo_hash", |b| {
            b.repo_hash = format!("{}=", "A".repeat(42))
        }),
        ("placement_hint", |b| {
            b.placement_hint = Some(Placement {
                host: "host-1".into(),
                vm: None,
            })
        }),
        ("placement_hint", |b| {
            b.placement_hint = Some(Placement {
                host: "host/1".into(),
                vm: Some("vm".into()),
            })
        }),
        ("avoid_host", |b| b.avoid_host = Some("boat:bx_1".into())),
        ("timeout_s", |b| b.timeout_s = 9),
        ("timeout_s", |b| b.timeout_s = 21601),
        ("requester_jwk", |b| b.requester_jwk.kty = "EC".into()),
        ("requester_jwk", |b| b.requester_jwk.crv = "X25519".into()),
        ("requester_jwk", |b| b.requester_jwk.x = "B".repeat(43)),
        ("allow_spill", |b| b.allow_spill = None),
    ];
    for (field, mutate) in refused {
        let mut body = valid.clone();
        mutate(&mut body);
        assert_eq!(invalid_lease_field(&body), Some(field), "{body:?}");
    }
    let mut no_spill = valid.clone();
    no_spill.allow_spill = Some(false);
    assert_eq!(invalid_lease_field(&no_spill), None);
}

/// A run request whose timeout or vCPU count no lease body can carry is
/// refused before any lease, naming the member; a missing timeout keeps its
/// own refusal.
#[test]
fn validate_refuses_requests_no_valid_lease_body_can_carry() {
    let request = |timeout: Option<u64>, weight: Option<u32>| {
        let mut request = RunRequest::new("/home/a/repo", "/home/a/repo", "/home/a/repo", "true");
        request.timeout = timeout;
        request.weight_hint = weight;
        request
    };
    assert_eq!(validate(&request(None, None)), Err(Error::NoTimeout));
    assert_eq!(validate(&request(Some(10), None)), Ok(()));
    assert_eq!(validate(&request(Some(7200), Some(64))), Ok(()));
    assert_eq!(validate(&request(Some(21600), Some(64))), Ok(()));
    for (timeout, weight, field) in [
        (9, None, "timeout_s"),
        (21601, None, "timeout_s"),
        (30, Some(0), "vcpus"),
        (30, Some(65), "vcpus"),
    ] {
        let refused = validate(&request(Some(timeout), weight));
        assert_eq!(
            refused,
            Err(Error::RequestInvalid(RequestField::new(field)))
        );
        assert_eq!(
            refused.unwrap_err().detail(),
            format!("hosted_request_invalid: {field}")
        );
    }
}
