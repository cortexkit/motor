use super::*;
use crate::{
    hosted::{placement::Finish, test_http::Http, HostedJobState},
    BrokerTokenSource, Config,
};
use cortexkit_test_support::ScratchDir;
use motor_protocol::{Killed, Outcome};
use std::sync::{
    atomic::{AtomicU16, AtomicUsize, Ordering},
    Arc,
};

#[tokio::test]
async fn successful_shadow_finish_clamps_runtime_and_logs_bearer_failure_before_sending() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let http = Http::new(|path, _| (200, serde_json::json!({"job_id":path.split('/').nth(3).unwrap(), "settled_vcpu_ms":60000, "released_vcpu_ms":0}))).await;
    let config = Config {
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        ..Config::default()
    };
    let client = Client::new(HostedJobState::new(&config));
    for run_ms in [59_999, 60_000, 60_001, 378_761] {
        let id = Uuid::now_v7();
        let mut terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0);
        terminal.wall_ms = run_ms;
        client
            .cleanup_shadow_with_delays(id, Some(&terminal), 60, true, &[Duration::ZERO; 6])
            .await;
        assert_eq!(
            http.requests.lock().unwrap().last().unwrap().1,
            serde_json::json!({"outcome":"exit", "run_ms":run_ms.min(60_000)})
        );
    }
    assert_eq!(http.requests.lock().unwrap().len(), 4);
    let config = Config {
        broker_token_source: BrokerTokenSource::CkAccount,
        broker_url: Some(http.url.clone()),
        ..Config::default()
    };
    let captured = crate::logging::Captured::default();
    let _logging = crate::logging::scoped(captured.clone());
    Client::new(HostedJobState::new(&config))
        .cleanup_shadow_with_delays(Uuid::now_v7(), None, 60, true, &[Duration::ZERO])
        .await;
    assert_eq!(
        http.requests.lock().unwrap().len(),
        4,
        "bearer errors must occur before sending a request"
    );
    let text = captured.text();
    assert!(
        text.contains("shadow lease cleanup failed")
            && text.contains("finish bearer acquisition failed: hosted_account_not_found"),
        "{text}"
    );
}

#[tokio::test]
async fn shadow_cleanup_retries_finish_and_release_and_logs_broker_cause() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    for booked in [false, true] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let calls = attempts.clone();
        let http = Http::new(move |path, body| {
            assert!(path.ends_with(if booked { "/finish" } else { "/release" }));
            if booked {
                assert_eq!(body, &serde_json::json!({"outcome":"lost", "run_ms":0}));
            }
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                (503, serde_json::json!({"error":"cleanup_unavailable"}))
            } else if booked {
                (200, serde_json::json!({"job_id":path.split('/').nth(3).unwrap(), "settled_vcpu_ms":0, "released_vcpu_ms":60000}))
            } else {
                (404, serde_json::json!({"error":"job_unknown"}))
            }
        }).await;
        let config = Config {
            broker_token_source: BrokerTokenSource::Fake,
            broker_url: Some(http.url.clone()),
            ..Config::default()
        };
        let client = Client::new(HostedJobState::new(&config));
        let captured = crate::logging::Captured::default();
        let _logging = crate::logging::scoped(captured.clone());
        client
            .cleanup_shadow_with_delays(Uuid::now_v7(), None, 60, booked, &[Duration::ZERO; 6])
            .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        let text = captured.text();
        let warnings: Vec<_> = text
            .lines()
            .filter(|line| line.contains("shadow lease cleanup failed"))
            .collect();
        assert_eq!(warnings.len(), 2, "{text}");
        assert!(
            warnings.iter().all(|line| line.contains("WARN")
                && line.contains("HTTP 503")
                && line.contains("cleanup_unavailable")),
            "{text}"
        );
        assert!(!text.contains("exhausted"), "{text}");
    }
}

#[tokio::test]
async fn shadow_cleanup_exhausts_six_attempts_within_two_minute_policy() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let http = Http::new(|_, _| (503, serde_json::json!({"error":"still_unavailable"}))).await;
    let config = Config {
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        ..Config::default()
    };
    let captured = crate::logging::Captured::default();
    let _logging = crate::logging::scoped(captured.clone());
    Client::new(HostedJobState::new(&config))
        .cleanup_shadow_with_delays(Uuid::now_v7(), None, 60, true, &[Duration::ZERO; 6])
        .await;
    assert_eq!(http.requests.lock().unwrap().len(), 6);
    assert!(captured
        .text()
        .contains("shadow lease cleanup exhausted retries"));
    assert_eq!(SHADOW_RETRY_DELAYS.len(), 6);
    assert!(
        SHADOW_RETRY_DELAYS.iter().sum::<Duration>()
            + Duration::from_secs(10 * SHADOW_RETRY_DELAYS.len() as u64)
            <= SHADOW_CLEANUP_BOUND
    );
}

#[test]
fn boat_finish_outcomes_and_runtime_are_clamped() {
    let id = Uuid::now_v7();
    for (outcome, killed, expected) in [
        (Outcome::Exit { code: 23 }, None, "exit"),
        (Outcome::Signal { signal: 9 }, None, "signal"),
        (
            Outcome::Signal { signal: 15 },
            Some(Killed::Deadline),
            "timeout",
        ),
        (Outcome::Exit { code: 0 }, Some(Killed::Cancel), "cancelled"),
        (Outcome::Cancelled, None, "cancelled"),
        (Outcome::OutcomeUnknown, None, "lost"),
        (
            Outcome::RefusedBeforeStart {
                reason: motor_protocol::RefusalReason::RunnerFull,
            },
            None,
            "lost",
        ),
    ] {
        let mut terminal = TerminalRecord::new(id, outcome, 0, u64::MAX, 0);
        terminal.killed = killed;
        terminal.wall_ms = u64::MAX;
        let finish = Finish::from_terminal(&terminal, 30);
        assert_eq!(finish.outcome, expected);
        assert_eq!(finish.run_ms, 30000);
        assert_eq!(Finish::from_terminal(&terminal, 0).run_ms, 0);
    }
}

#[tokio::test]
async fn boat_finish_nonretry_statuses_and_restart_resume() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    let status = Arc::new(AtomicU16::new(503));
    let response_status = status.clone();
    let http = Http::new(move |path, body| {
        assert!(path.ends_with("/finish"));
        assert_eq!(body, &serde_json::json!({"outcome":"exit","run_ms":30000}));
        (response_status.load(Ordering::SeqCst), serde_json::json!({"job_id":path.split('/').nth(3).unwrap(),"settled_vcpu_ms":123,"released_vcpu_ms":59877}))
    }).await;
    let scratch = ScratchDir::new("boat-finish");
    let root = scratch.join("state");
    let storage = Storage::open(&root).unwrap();
    let mut config = Config {
        runner_instance: "test".into(),
        broker_token_source: BrokerTokenSource::Fake,
        broker_url: Some(http.url.clone()),
        ..Config::default()
    };
    let mut route = HostedJobState::new(&config);
    route.placement = Some(motor_lease::Placement {
        host: "boat:test".into(),
        vm: Some("test".into()),
    });
    route.max_runtime_s = Some(30);
    for code in [400, 404, 409] {
        status.store(code, Ordering::SeqCst);
        let id = Uuid::now_v7();
        storage.hosted_route(id, route.clone()).unwrap();
        let before = http.requests.lock().unwrap().len();
        let mut terminal = TerminalRecord::new(id, Outcome::Exit { code: 1 }, 0, 0, 0);
        terminal.wall_ms = 90000;
        Client::new(route.clone())
            .finish(id, Some(&terminal), &storage)
            .await
            .unwrap();
        assert_eq!(http.requests.lock().unwrap().len(), before + 1);
        assert!(storage.job(id).unwrap().hosted.unwrap().finish_done);
    }
    status.store(503, Ordering::SeqCst);
    let id = Uuid::now_v7();
    storage.hosted_route(id, route.clone()).unwrap();
    let mut terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0);
    terminal.wall_ms = 90000;
    Client::new(route.clone())
        .finish(id, Some(&terminal), &storage)
        .await
        .unwrap();
    assert!(storage
        .job(id)
        .unwrap()
        .hosted
        .unwrap()
        .pending_finish
        .is_some());
    drop(storage);
    status.store(200, Ordering::SeqCst);
    // Reopening the real Motor and installing account credentials performs the
    // same restart recovery used by the module, even with hosted routing off.
    config.hosted_runners = false;
    let vault = crate::hosted::account::tests::FakeVault::new(vec![]);
    let motor = crate::Motor::open(config, &root)
        .unwrap()
        .with_account_tokens(vault.client());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if motor.hosted_state(id).unwrap().finish_done {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(motor.hosted_state(id).unwrap().billed_vcpu_ms, Some(123));
    assert!(motor.hosted_state(id).unwrap().pending_finish.is_none());
    assert_eq!(
        motor.hosted_state(id).unwrap().terminal.unwrap().outcome,
        Outcome::Exit { code: 0 }
    );
}
