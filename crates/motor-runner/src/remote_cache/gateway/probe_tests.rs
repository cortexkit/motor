use super::*;
use motor_protocol::CreateOnlyProbeDetail as Detail;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Fault {
    IgnoreCondition,
    WrongWinner,
    OverwriteDespite412,
    PartialObject,
    MixedFill,
    Concurrent409,
}

#[tokio::test(start_paused = true)]
async fn all_four_store_criteria_pass_through_the_gateway_with_provisioned_keys() {
    let h = Harness::new();
    let report = h.gateway.create_only_probe(TENANT).await;
    assert!(report.valid());
    assert!(report.ok, "{report:?}");
    assert_eq!(report.exit_code, 0);
    assert_eq!(report.cases.len(), 4);
    assert!(report.cases.iter().all(|case| case.ok));
    let seen = h.store.seen();
    assert_eq!(seen.iter().filter(|r| r.method == "PUT").count(), 9);
    for request in &seen {
        assert!(request
            .path
            .starts_with(&stored(Tool::Sccache, ".motor-create-only-probe/")));
        assert!(matches!(request.method.as_str(), "GET" | "HEAD" | "PUT"));
        assert_eq!(
            request.access_key_id(),
            if request.method == "PUT" {
                "WRITE-1"
            } else {
                "READ-1"
            }
        );
        if request.method == "PUT" {
            let fill = request.path.ends_with("/race") && request.body[0] == b'F';
            assert_eq!(
                request.header("if-none-match"),
                if fill { None } else { Some("*") }
            );
            assert_eq!(
                request
                    .header("authorization")
                    .unwrap()
                    .contains("if-none-match"),
                !fill
            );
        }
    }
    assert_eq!(h.store.interrupted_puts.lock().unwrap().len(), 1);
    assert_eq!(
        h.store.objects.lock().unwrap().len(),
        4,
        "objects await lifecycle expiry"
    );
    assert!(h.gateway.shared.job_puts.lock().unwrap().is_empty());
    let first_paths: Vec<_> = seen.iter().map(|r| r.path.clone()).collect();
    assert!(h.gateway.create_only_probe(TENANT).await.ok);
    for request in h.store.seen().iter().skip(seen.len()) {
        assert!(
            !first_paths.contains(&request.path),
            "each invocation uses fresh names"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_store_ignoring_conditions_has_a_nonzero_probe_result() {
    let h = Harness::new();
    h.store
        .set(|k| k.probe_fault = Some(Fault::IgnoreCondition));
    let report = h.gateway.create_only_probe(TENANT).await;
    assert!(report.valid());
    assert!(!report.ok);
    assert_eq!(report.exit_code, 1);
    assert!(!report.cases[0].ok, "multiple creates were accepted");
    assert!(!report.cases[1].ok, "the existing key was overwritten");
    assert!(!serde_json::to_string(&report).unwrap().contains(LEAK));
}

#[tokio::test(start_paused = true)]
async fn every_criterion_detects_its_store_fault_without_hiding_other_case_reports() {
    for (fault, failed_case) in [
        (Fault::WrongWinner, "concurrent_creates"),
        (Fault::OverwriteDespite412, "existing_object"),
        (Fault::PartialObject, "interrupted_body"),
        (Fault::MixedFill, "fill_race"),
    ] {
        let h = Harness::new();
        h.store.set(|k| k.probe_fault = Some(fault));
        let report = h.gateway.create_only_probe(TENANT).await;
        assert!(report.valid());
        assert_eq!(report.exit_code, 1, "{failed_case}: {report:?}");
        for case in &report.cases {
            assert_eq!(
                case.ok,
                case.name != failed_case,
                "{failed_case}: {report:?}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_conflicts_may_be_409_but_existing_key_must_be_412() {
    let h = Harness::new();
    h.store.set(|k| k.probe_fault = Some(Fault::Concurrent409));
    let report = h.gateway.create_only_probe(TENANT).await;
    assert!(report.ok, "{report:?}");
}

#[tokio::test(start_paused = true)]
async fn block_missing_keys_and_wrong_accounts_refuse_without_contacting_the_store() {
    let h = Harness::new();
    h.gateway.set_job_write_block(true).await;
    let blocked = h.gateway.create_only_probe(TENANT).await;
    assert_eq!(blocked.cases[0].detail, Detail::RemoteCacheWriteBlocked);
    assert_eq!(blocked.exit_code, 1);
    h.gateway.set_job_write_block(false).await;
    assert_eq!(
        h.gateway.create_only_probe("other").await.cases[0].detail,
        Detail::RemoteCacheAccountMismatch
    );
    let mut no_write = generation("read-only", TENANT);
    no_write.write_key = None;
    h.generations.set(Some(no_write));
    assert_eq!(
        h.gateway.create_only_probe(TENANT).await.cases[0].detail,
        Detail::RemoteCacheWriteKeyMissing
    );
    h.generations.set(None);
    assert_eq!(
        h.gateway.create_only_probe(TENANT).await.cases[0].detail,
        Detail::RemoteCacheUnavailable
    );
    assert_eq!(h.store.connects(), 0);
}

#[tokio::test(start_paused = true)]
async fn unavailable_store_fails_all_cases_in_bounded_time_without_leaking_error_bodies() {
    let h = Harness::new();
    h.store.status(Some(503));
    let report = h.gateway.create_only_probe(TENANT).await;
    assert_eq!(report.exit_code, 1);
    assert_eq!(report.cases.len(), 4);
    assert!(report.cases.iter().all(|case| !case.ok));
    assert!(!serde_json::to_string(&report).unwrap().contains(LEAK));
}

#[tokio::test(start_paused = true)]
async fn an_interruption_refused_before_upload_is_not_evidence_of_store_atomicity() {
    let h = Harness::with_limits(Limits {
        max_object_bytes: 128 * 1024,
        ..Limits::default()
    });
    let report = h.gateway.create_only_probe(TENANT).await;
    assert_eq!(report.exit_code, 1);
    assert!(!report.cases[2].ok);
    assert!(report
        .cases
        .iter()
        .enumerate()
        .all(|(index, case)| case.ok == (index != 2)));
    assert!(h.store.interrupted_puts.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_probe_discards_its_transient_accounting_state() {
    let h = Harness::new();
    h.store.set(|k| k.silent = true);
    let gateway = h.gateway.clone();
    let task = tokio::spawn(async move { gateway.create_only_probe(TENANT).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let changed = h.store.upload_changed.notified();
            if !h.store.put_heads.lock().unwrap().is_empty() {
                break;
            }
            changed.await;
        }
    })
    .await
    .unwrap();
    assert!(!h.gateway.shared.job_puts.lock().unwrap().is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(h.gateway.shared.job_puts.lock().unwrap().is_empty());
    assert!(h.gateway.shared.jobs.lock().unwrap().is_empty());
}
