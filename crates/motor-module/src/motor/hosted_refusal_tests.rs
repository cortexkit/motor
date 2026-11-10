use super::*;
use crate::hosted::tests::harness::{Behavior, Harness, NoSsh};
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn hosted_runner_refusals_preserve_typed_code_reason_and_detail() {
    let cases = [
        (
            RefusalReason::WorkspaceSetupFailed,
            "workspace_setup_failed",
            Some("cargo prefetch failed: registry unavailable"),
            "workspace_setup_failed",
            None,
        ),
        (
            RefusalReason::RunnerFull,
            "runner_full",
            Some("runner has no free slots"),
            "runner_full",
            None,
        ),
        (
            RefusalReason::RunnerDraining,
            "runner_draining",
            Some("runner is draining for maintenance"),
            "runner_draining",
            Some(999),
        ),
        // Tenant admission rejects mismatched lease claims with this exact
        // reason and absent detail, rather than a workspace or capacity error.
        (
            RefusalReason::RequestRejected,
            "request_rejected",
            None,
            "lease_mismatch",
            None,
        ),
        (
            RefusalReason::RequestRejected,
            "request_rejected",
            Some("request option rejected"),
            "request_rejected",
            None,
        ),
        (
            RefusalReason::WorkspaceSetupFailed,
            "workspace_setup_failed",
            None,
            "workspace_setup_failed",
            None,
        ),
    ];
    for (reason, wire_reason, detail, code, retry_after_ms) in cases {
        let h = Harness::new(Behavior::RefuseRun(wire_reason, detail, Some(999))).await;
        let root = ScratchDir::new("hosted-runner-refusal");
        let repo = queue_tests::fixture_repo(root.path());
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(h.config(), &root.join("state"), ssh.clone()).unwrap();
        let id = Uuid::now_v7();
        motor.storage.create(id, 30, None, None, false).unwrap();
        motor.storage.hosted_route(id, h.route.clone()).unwrap();
        let path = repo.to_str().unwrap();
        let snapshots = Arc::new(
            Snapshots::build(
                &motor.config,
                path,
                path,
                &[],
                root.path(),
                id,
                None,
                &motor.storage,
            )
            .unwrap(),
        );
        let tree_hash = snapshots.main.tree_hash.clone();
        let (tx, mut records) = mpsc::channel(32);
        let (_wake, wake) = watch::channel(false);
        assert!(!tokio::time::timeout(
            Duration::from_secs(10),
            motor.run_snapshot(
                id,
                (Harness::request(&repo), crate::ReturnOptions::default()),
                Vec::new(),
                30,
                false,
                snapshots,
                &tx,
                wake,
            ),
        )
        .await
        .unwrap()
        .unwrap());
        let StreamRecord::Terminal(terminal) = records.recv().await.unwrap().unwrap() else {
            panic!("runner refusal must produce a terminal");
        };
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart {
                reason: reason.clone()
            }
        );
        assert_eq!(terminal.refusal_detail.as_deref(), detail, "{reason:?}");
        assert_eq!(terminal.retry_after_ms, retry_after_ms, "{reason:?}");
        assert_eq!(terminal.ran, Some(Ran::None));
        assert_eq!(terminal.tree_hash.as_deref(), Some(tree_hash.as_str()));
        assert_eq!(
            motor.storage.job(id).unwrap().local_terminal,
            Some(terminal)
        );
        let state = motor.hosted_state(id).unwrap();
        assert_eq!(state.hosted_code.as_deref(), Some(code), "{reason:?}");
        assert!(state.hosted_no_fallback);
        assert_eq!(h.runs.load(Ordering::SeqCst), 1);
        assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
        assert!(h
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .ends_with("/release"));

        // Status probes and followed terminals must publish the same refusal
        // as admission, without relabelling the runner's explanation.
        let probe_id = Uuid::now_v7();
        motor
            .storage
            .create(probe_id, 30, None, None, false)
            .unwrap();
        motor
            .storage
            .hosted_route(probe_id, h.route.clone())
            .unwrap();
        let mut probe = TerminalRecord::new(
            probe_id,
            Outcome::RefusedBeforeStart {
                reason: reason.clone(),
            },
            0,
            0,
            0,
        )
        .with_ran(Ran::None);
        probe.refusal_detail = detail.map(str::to_owned);
        probe.retry_after_ms = Some(999);
        let persisted = motor.persist_hosted_terminal(probe_id, probe).unwrap();
        assert_eq!(persisted.refusal_detail.as_deref(), detail, "{reason:?}");
        assert_eq!(persisted.retry_after_ms, retry_after_ms, "{reason:?}");
        assert_eq!(persisted.outcome, Outcome::RefusedBeforeStart { reason });
        let state = motor.hosted_state(probe_id).unwrap();
        assert_eq!(state.hosted_code.as_deref(), Some(code));
        assert!(state.hosted_no_fallback);
        motor.close().await;
    }
}
