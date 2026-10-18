use super::*;
use crate::hosted::account::tests::{refusal, FakeVault};
use std::sync::atomic::{AtomicUsize, Ordering};

struct NoSsh(AtomicUsize);
impl Connector for NoSsh {
    fn connect(
        &self,
        _: Uuid,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + '_>,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(io::Error::other("SSH must not open")) })
    }
}

#[tokio::test]
async fn hosted_account_refusals_are_typed_no_run_and_keep_retry_delay() {
    let root =
        cortexkit_test_support::scratch_root().join(format!("hosted-account-{}", Uuid::now_v7()));
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(Config::default(), &root, ssh.clone()).unwrap();
    for (reply, reason, code, delay, detail) in [
        (
            refusal("not_found"),
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_account_not_found",
            None,
            "hosted_account_not_found",
        ),
        (
            refusal("auth_required"),
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_account_auth_required",
            None,
            "hosted_account_auth_required: a person must run `ck account login`",
        ),
        (
            refusal("transient"),
            RefusalReason::Unreachable,
            "hosted_account_transient",
            Some(4321),
            "hosted_account_transient",
        ),
        (
            refusal("permanent"),
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_account_permanent",
            None,
            "hosted_account_permanent",
        ),
        (
            serde_json::json!({"result":{"jwt":"unusable-secret"}}),
            RefusalReason::RunnerAuthenticationFailed,
            "hosted_account_invalid",
            None,
            "hosted_account_invalid",
        ),
    ] {
        let vault = FakeVault::new(vec![Ok(reply)]);
        let id = Uuid::now_v7();
        motor.storage.create(id, 30, None, None, false).unwrap();
        let route = crate::HostedJobState::new(&Config::default());
        motor.storage.hosted_route(id, route.clone()).unwrap();
        let client = crate::hosted::Client::new(route).with_account_tokens(Some(vault.client()));
        let path = root.to_str().unwrap();
        let request = RunRequest::new(path, path, path, "true").with_timeout(30);
        let error = match client.run(id, &request, &motor.storage).await {
            Err(error) => error,
            Ok(_) => panic!("account refusal reached a runner"),
        };
        let (tx, mut rx) = mpsc::channel(1);
        motor
            .refused_with_retry(
                id,
                error.reason(),
                None,
                Some(error.detail()),
                error.retry_after_ms(),
                &tx,
            )
            .await
            .unwrap();
        let record = rx.recv().await.unwrap().unwrap();
        // Decode the serialized boundary record, rather than inspecting the vault's error.
        let wire = serde_json::to_vec(&record).unwrap();
        let StreamRecord::Terminal(terminal) =
            serde_json::from_slice::<StreamRecord>(&wire).unwrap()
        else {
            panic!("expected a terminal refusal");
        };
        assert_eq!(terminal.outcome, Outcome::RefusedBeforeStart { reason });
        assert_eq!(terminal.ran, Some(Ran::None));
        assert_eq!(terminal.retry_after_ms, delay);
        assert_eq!(terminal.refusal_detail.as_deref(), Some(detail));
        assert!(!wire
            .windows(b"unusable-secret".len())
            .any(|s| s == b"unusable-secret"));
        assert_eq!(
            vault.requests.lock().unwrap().len(),
            1,
            "expected exactly one vault request for the refusal"
        );
        let state = motor.hosted_state(id).unwrap();
        assert!(state.hosted_no_fallback);
        assert_eq!(state.hosted_code.as_deref(), Some(code));
        assert!(!state.lease_attempted);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    }
}
