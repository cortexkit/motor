use super::*;
use motor_protocol::{ReturnFailureCause, ReturnPathsFailed, ReturnedFile};

fn request() -> RunRequest {
    RunRequest::new("/w/key", "/w/main", "/w/key/sub", "true")
}

#[test]
fn over_cap_return_max_bytes_is_invalid_params_with_or_without_paths() {
    for paths in [Vec::new(), vec!["out".to_string()]] {
        let mut request = request()
            .with_return_paths(paths)
            .with_return_max_bytes(shared::RETURN_MAX_BYTES_CAP + 1);
        let Err(ReturnRequestError::InvalidParams(message)) = take_return_request(&mut request)
        else {
            panic!("over-cap request was not refused as invalid_params");
        };
        assert!(message.contains("return_max_bytes"), "{message}");
    }
}

#[test]
fn return_paths_join_cwd_and_the_fields_leave_the_request() {
    let mut request = request()
        .with_return_paths(vec!["out".into(), "nested/file.txt".into()])
        .with_return_max_bytes(shared::RETURN_MAX_BYTES_CAP);
    let ReturnRequest {
        options: returns,
        cwd,
        requested,
    } = take_return_request(&mut request).unwrap().unwrap();
    assert_eq!(
        requested, None,
        "only a chunked request keeps the caller's paths"
    );
    assert!(!returns.return_chunked);
    assert_eq!(
        returns.return_paths,
        ["/w/key/sub/out", "/w/key/sub/nested/file.txt"]
    );
    assert_eq!(returns.return_max_bytes, shared::RETURN_MAX_BYTES_CAP);
    assert_eq!(cwd, "/w/key/sub");
    assert!(request.return_paths.is_empty());
    assert_eq!(request.return_max_bytes, None);
}

#[test]
fn absent_cap_defaults_and_empty_paths_request_nothing() {
    let mut with_paths = request().with_return_paths(vec!["out".into()]);
    let returns = take_return_request(&mut with_paths)
        .unwrap()
        .unwrap()
        .options;
    assert_eq!(returns.return_max_bytes, RETURN_MAX_BYTES);
    let mut without = request().with_return_max_bytes(10);
    assert_eq!(take_return_request(&mut without), Ok(None));
    assert_eq!(without.return_max_bytes, None);
}

#[test]
fn chunked_request_raises_the_cap_to_ten_mib_and_keeps_the_paths_as_sent() {
    let mut chunked = request()
        .with_return_paths(vec!["./out".into(), "nested/file.txt".into()])
        .with_return_chunked(true);
    let ReturnRequest {
        options,
        cwd,
        requested,
    } = take_return_request(&mut chunked).unwrap().unwrap();
    assert!(options.return_chunked);
    assert!(!chunked.return_chunked, "the flag leaves the request");
    assert_eq!(
        options.return_max_bytes,
        shared::RETURN_CHUNKED_MAX_BYTES_CAP
    );
    assert_eq!(
        options.return_paths,
        ["/w/key/sub/./out", "/w/key/sub/nested/file.txt"]
    );
    assert_eq!(cwd, "/w/key/sub");
    assert_eq!(
        requested.as_deref(),
        Some(&["./out".to_string(), "nested/file.txt".to_string()][..])
    );
    let mut at_cap = request()
        .with_return_paths(vec!["out".into()])
        .with_return_chunked(true)
        .with_return_max_bytes(shared::RETURN_CHUNKED_MAX_BYTES_CAP);
    assert_eq!(
        take_return_request(&mut at_cap)
            .unwrap()
            .unwrap()
            .options
            .return_max_bytes,
        shared::RETURN_CHUNKED_MAX_BYTES_CAP
    );
    for paths in [Vec::new(), vec!["out".to_string()]] {
        let mut over = request()
            .with_return_paths(paths)
            .with_return_chunked(true)
            .with_return_max_bytes(shared::RETURN_CHUNKED_MAX_BYTES_CAP + 1);
        assert!(matches!(
            take_return_request(&mut over),
            Err(ReturnRequestError::InvalidParams(message)) if message.contains("return_max_bytes")
        ));
    }
    // Above 256 KiB is allowed only with the opt-in.
    let mut unchunked = request()
        .with_return_paths(vec!["out".into()])
        .with_return_max_bytes(shared::RETURN_MAX_BYTES_CAP + 1);
    assert!(matches!(
        take_return_request(&mut unchunked),
        Err(ReturnRequestError::InvalidParams(_))
    ));
}

#[test]
fn caller_manifest_echoes_requested_paths_by_position() {
    let manifest = shared::ReturnManifest::new(
        Uuid::nil(),
        7,
        vec![
            shared::ReturnManifestEntry::returned("/w/key/sub/./out", 1, "digest").with_chunks(1),
            shared::ReturnManifestEntry::failed(
                "/w/key/sub/missing",
                shared::ReturnPathsFailureCause::Missing,
            )
            .with_chunks(0),
        ],
        1,
        10,
    );
    let requested = ["./out".to_string(), "missing".to_string()];
    let caller = caller_manifest(manifest.clone(), Some(&requested));
    assert_eq!(caller.files[0].path(), "./out");
    assert_eq!(caller.files[1].path(), "missing");
    assert_eq!(caller.files[0].sha256(), Some("digest"));
    assert_eq!(caller.files[0].chunks(), Some(1));
    assert_eq!(caller.files[1].chunks(), Some(0));
    assert_eq!(
        (caller.seq, caller.total_bytes, caller.cap_bytes),
        (7, 1, 10)
    );
    // A stored list that does not match leaves the runner's paths alone.
    assert_eq!(
        caller_manifest(manifest.clone(), Some(&requested[..1])),
        manifest
    );
    assert_eq!(caller_manifest(manifest.clone(), None), manifest);
}

#[test]
fn absolute_return_paths_are_rejected_before_start() {
    let mut request = request().with_return_paths(vec!["/w/key/sub/out".into()]);
    assert_eq!(
        take_return_request(&mut request),
        Err(ReturnRequestError::Rejected("return_paths_exceeded"))
    );
}

fn stored(
    files: Option<Vec<ReturnedFile>>,
    failed: Option<ReturnPathsFailed>,
) -> motor_protocol::ReturnResult {
    motor_protocol::ReturnResult {
        job_id: Uuid::nil(),
        seq: 4,
        returned_files: files,
        return_paths_failed: failed,
    }
}

#[test]
fn returned_files_become_cwd_relative_decoded_bytes() {
    let record = return_record(
        stored(
            Some(vec![ReturnedFile {
                path: "/w/key/sub/nested/out".into(),
                sha256: "digest".into(),
                data: "eA==".into(),
            }]),
            None,
        ),
        "/w/key/sub",
    )
    .unwrap();
    assert_eq!(record.seq(), 4);
    assert_eq!(record.return_paths_failed(), None);
    let files = record.returned_files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path(), "nested/out");
    assert_eq!(files[0].sha256(), "digest");
    assert_eq!(files[0].data(), b"x");
    assert!(
        return_record(
            stored(
                Some(vec![ReturnedFile {
                    path: "/w/key/sub/out".into(),
                    sha256: "digest".into(),
                    data: "not base64!".into(),
                }]),
                None,
            ),
            "/w/key/sub",
        )
        .is_none(),
        "invalid stored data must not be forwarded"
    );
}

#[test]
fn collection_failures_keep_their_cause_and_byte_counts() {
    for (cause, expected) in [
        (
            ReturnFailureCause::OverCap,
            shared::ReturnPathsFailureCause::OverCap,
        ),
        (
            ReturnFailureCause::Missing,
            shared::ReturnPathsFailureCause::Missing,
        ),
        (
            ReturnFailureCause::Escape,
            shared::ReturnPathsFailureCause::Escape,
        ),
        (
            ReturnFailureCause::NotRegular,
            shared::ReturnPathsFailureCause::NotRegular,
        ),
        (
            ReturnFailureCause::FrameLimit,
            shared::ReturnPathsFailureCause::FrameLimit,
        ),
    ] {
        let record = return_record(
            stored(
                None,
                Some(ReturnPathsFailed {
                    measured_bytes: 9,
                    cap_bytes: 1,
                    cause,
                }),
            ),
            "/w/key/sub",
        )
        .unwrap();
        assert_eq!(record.returned_files(), None);
        let failed = record.return_paths_failed().unwrap();
        assert_eq!(
            (failed.measured_bytes(), failed.cap_bytes(), failed.cause()),
            (9, 1, &expected)
        );
    }
    assert!(return_record(stored(None, None), "/w").is_none());
}

fn route(run_sent: bool, billing: crate::BillingStatus, billed: Option<u64>) -> HostedJobState {
    HostedJobState {
        placement: Some(motor_lease::Placement {
            host: "boat:bx_1".into(),
            vm: Some("bx_1".into()),
        }),
        run_sent,
        billing,
        billed_vcpu_ms: billed,
        ..HostedJobState::default()
    }
}

#[test]
fn hosted_usage_needs_a_sent_run_and_reports_only_settled_billing() {
    use crate::BillingStatus::*;
    assert_eq!(hosted_usage(None), None);
    assert_eq!(hosted_usage(Some(&route(false, Available, Some(5)))), None);
    assert_eq!(
        hosted_usage(Some(&HostedJobState {
            placement: None,
            ..route(true, Pending, None)
        })),
        None
    );
    assert_eq!(
        hosted_usage(Some(&route(true, Pending, None))),
        Some(HostedUsage::new("boat:bx_1"))
    );
    assert_eq!(
        hosted_usage(Some(&route(true, Unavailable, None))),
        Some(HostedUsage::new("boat:bx_1"))
    );
    assert_eq!(
        hosted_usage(Some(&route(true, Available, Some(1234)))),
        Some(HostedUsage::new("boat:bx_1").with_billed_vcpu_ms(1234))
    );
}

/// Hosted jobs, against the fake broker and hosted runner of the hosted tests.
mod hosted {
    use super::*;
    use crate::hosted::{
        tests::harness::{Behavior, Harness, NoSsh, Scratch},
        Client,
    };
    use motor_protocol::{AttachRequest, Outcome, StreamRecord};
    use std::sync::{atomic::AtomicUsize, Arc};

    /// A hosted job whose Run frame was sent, left for `attach` to follow.
    /// `return_cwd` marks it as an agent job that asked for returned files.
    async fn seed_sent(h: &Harness, scratch: &Scratch, id: Uuid, return_cwd: Option<&str>) {
        let storage = Arc::new(Storage::open(&scratch.0.join("state")).unwrap());
        storage
            .create(id, 30, None, Some("test".into()), false)
            .unwrap();
        storage.hosted_route(id, h.route.clone()).unwrap();
        let (_, lease) = Client::new(h.route.clone())
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .unwrap();
        storage
            .hosted_update(id, |s| {
                s.lease(&lease);
                s.run_sent = true;
            })
            .unwrap();
        if let Some(cwd) = return_cwd {
            storage.return_cwd(id, cwd.into()).unwrap();
        }
    }

    /// Open the module state `seed_sent` wrote. A task spawned while seeding
    /// can hold the storage lease a moment longer, so wait for it.
    async fn motor(h: &Harness, scratch: &Scratch) -> crate::Motor {
        crate::tests::reopen_motor(|| {
            crate::Motor::with_connector(
                h.config(),
                &scratch.0.join("state"),
                Arc::new(NoSsh(AtomicUsize::new(0))),
            )
        })
        .await
    }

    fn terminal(records: &[StreamRecord]) -> &TerminalRecord {
        match records.last() {
            Some(StreamRecord::Terminal(terminal)) => terminal,
            other => panic!("expected a terminal record last, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hosted_terminal_carries_placement_and_settled_billing() {
        let h = Harness::new(Behavior::Normal).await;
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        seed_sent(&h, &scratch, id, None).await;
        let motor = motor(&h, &scratch).await;
        let records = agent_stream(motor.attach(AttachRequest::new(id, 0)).unwrap(), 0).await;
        assert_eq!(records.len(), 1, "{records:?}");
        let terminal = terminal(&records);
        assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
        assert_eq!(
            terminal.hosted(),
            Some(&HostedUsage::new("test-host").with_billed_vcpu_ms(6001))
        );
        let status = status_with_hosted(
            motor.job_status(id).await.unwrap(),
            motor.hosted_state(id).as_ref(),
        );
        assert_eq!(
            status.terminal.unwrap().hosted(),
            Some(&HostedUsage::new("test-host").with_billed_vcpu_ms(6001))
        );
    }

    #[tokio::test]
    async fn pending_billing_sends_placement_without_waiting_for_it() {
        let h = Harness::new(Behavior::DropTerminal).await;
        let scratch = Scratch::new();
        let id = Uuid::now_v7();
        seed_sent(&h, &scratch, id, None).await;
        let motor = motor(&h, &scratch).await;
        let records = agent_stream(motor.attach(AttachRequest::new(id, 0)).unwrap(), 0).await;
        assert_eq!(
            motor.hosted_state(id).unwrap().billing,
            crate::BillingStatus::Pending
        );
        assert_eq!(
            terminal(&records).hosted(),
            Some(&HostedUsage::new("test-host"))
        );
    }

    #[tokio::test]
    async fn hosted_return_result_reaches_only_a_caller_that_asked_for_files() {
        for asked in [true, false] {
            let h = Harness::new(Behavior::ReturnCapFailure).await;
            let scratch = Scratch::new();
            let id = Uuid::now_v7();
            let cwd = scratch.0.to_str().unwrap().to_owned();
            seed_sent(&h, &scratch, id, asked.then_some(cwd.as_str())).await;
            let motor = motor(&h, &scratch).await;
            let records = agent_stream(motor.attach(AttachRequest::new(id, 0)).unwrap(), 0).await;
            assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 23 });
            assert!(
                terminal(&records).hosted().is_some(),
                "a hosted terminal reports its placement"
            );
            let results: Vec<_> = records
                .iter()
                .filter_map(|record| match record {
                    StreamRecord::ReturnResult(result) => Some(result),
                    _ => None,
                })
                .collect();
            if asked {
                assert_eq!(records.len(), 2, "{records:?}");
                assert!(matches!(records[0], StreamRecord::ReturnResult(_)));
                let failed = results[0].return_paths_failed().unwrap();
                assert_eq!(failed.cause(), &shared::ReturnPathsFailureCause::OverCap);
                assert_eq!((failed.measured_bytes(), failed.cap_bytes()), (2, 1));
                assert_eq!(results[0].job_id(), id);
            } else {
                assert!(results.is_empty(), "{records:?}");
                assert!(
                    motor.return_result(id).is_some(),
                    "the runner still sent one"
                );
            }
        }
    }
}
