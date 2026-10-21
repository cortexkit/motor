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
fn spill_avoid_host_follows_the_broker_host_id_grammar() {
    let mut config = Config::default();
    for good in [
        "build-server-1",
        "a",
        "Host_2.example",
        "a".repeat(64).as_str(),
    ] {
        config.spill_avoid_host = Some(good.to_string());
        assert!(config.validate().is_ok(), "{good}");
        assert_eq!(
            serde_json::to_value(&config).unwrap()["spill_avoid_host"],
            good
        );
    }
    for bad in ["", "boat:test", "a b", "host/1", "a".repeat(65).as_str()] {
        config.spill_avoid_host = Some(bad.to_string());
        assert!(config.validate().is_err(), "{bad:?}");
    }
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
    assert!(config.hosted_runners);
    assert_eq!(config.broker_token_source, BrokerTokenSource::CkAccount);
    let mut off = config.clone();
    off.hosted_runners = false;
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
pub(crate) mod harness;
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
        assert_eq!(h.requests.lock().unwrap().len(), count);
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
    assert!(motor
        .drop_workspace(DropRequest::new("/tmp/missing"))
        .await
        .unwrap_err()
        .to_string()
        .contains("hosted_no_fallback"));
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
        },
        crate::ReturnOptions {
            return_paths: vec!["/home/missing/out".into(); 65],
            return_max_bytes: 1,
        },
        crate::ReturnOptions {
            return_paths: vec![format!("/home/missing/{}", "x".repeat(4096))],
            return_max_bytes: 1,
        },
        crate::ReturnOptions {
            return_paths: vec!["/home/elsewhere/out".into()],
            return_max_bytes: 1,
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
        None,
    )
    .unwrap();
    assert_eq!(serde_json::to_value(&body).unwrap(), v["request"]["body"]);
    // The broker's lease hash takes the fields in this order, and
    // `avoid_host` comes right after `placement_hint`.
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
        "Absent and true have different lease hashes despite the same placement policy"
    );
    assert_ne!(absent, canonical);
    assert_ne!(spill, canonical);
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
            let (channel, _) = Client::new(route.clone())
                .run(id, &request, &storage)
                .await
                .unwrap();
            drop(channel);
        }
        let bodies = h.lease_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
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
