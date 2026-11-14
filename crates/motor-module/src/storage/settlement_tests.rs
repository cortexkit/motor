use super::*;

#[test]
fn reopen_restores_no_run_records_without_replacing_real_terminals() {
    for case in [
        "never_started",
        "run_sent",
        "spill",
        "local_terminal",
        "hosted_terminal",
        "remote_terminal",
        "local_unknown",
        "hosted_unknown",
        "hosted_history_expired",
        "remote_unknown",
        "hosted_refusal",
        "unsettled_unknown",
        "unsettled_history_expired",
        "unsettled_real_terminal",
        "unsettled",
        "run_sent_unknown",
    ] {
        let scratch = cortexkit_test_support::ScratchDir::new("hosted-settlement-terminal");
        let storage = Storage::open(&scratch).unwrap();
        let id = Uuid::now_v7();
        let outcome = match case {
            "local_unknown" | "hosted_unknown" | "remote_unknown" | "unsettled_unknown"
            | "run_sent_unknown" => Outcome::OutcomeUnknown,
            "hosted_history_expired" | "unsettled_history_expired" => Outcome::HistoryExpired,
            "hosted_refusal" => Outcome::RefusedBeforeStart {
                reason: RefusalReason::RunnerFull,
            },
            _ => Outcome::Cancelled,
        };
        let existing = TerminalRecord::new(id, outcome, 0, 0, 0).with_ran(Ran::None);
        storage.create(id, 60, None, None, false).unwrap();
        storage.hosted_route(id, Default::default()).unwrap();
        storage
            .update(|s| {
                let job = s.jobs.get_mut(&id).unwrap();
                let hosted = job.hosted.as_mut().unwrap();
                hosted.finish_done = !case.starts_with("unsettled");
                hosted.billing = crate::hosted::BillingStatus::Pending;
                match case {
                    "never_started" | "unsettled" => {}
                    "run_sent" => hosted.run_sent = true,
                    "spill" => job.build_server_job_id = Some(Uuid::now_v7()),
                    "local_terminal" | "local_unknown" => {
                        job.local_terminal = Some(existing.clone())
                    }
                    "hosted_terminal"
                    | "hosted_unknown"
                    | "hosted_history_expired"
                    | "hosted_refusal"
                    | "unsettled_unknown"
                    | "unsettled_history_expired"
                    | "unsettled_real_terminal" => hosted.terminal = Some(existing.clone()),
                    "remote_terminal" | "remote_unknown" => {
                        job.remote_terminal = Some(existing.clone())
                    }
                    "run_sent_unknown" => {
                        hosted.run_sent = true;
                        hosted.terminal = Some(existing.clone());
                    }
                    _ => unreachable!(),
                }
            })
            .unwrap();
        let original = storage.job(id).unwrap();
        drop(storage);

        let storage = Storage::open(&scratch).unwrap();
        let job = storage.job(id).expect("settled job must survive restart");
        if matches!(
            case,
            "never_started"
                | "local_unknown"
                | "hosted_unknown"
                | "hosted_history_expired"
                | "remote_unknown"
                | "unsettled_unknown"
                | "unsettled_history_expired"
        ) {
            assert_eq!(
                job.hosted.as_ref().unwrap().finish_done,
                original.hosted.as_ref().unwrap().finish_done,
                "terminal repair must not skip broker settlement"
            );
            let terminal = job.local_terminal.as_ref().unwrap();
            assert_eq!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::LostToRestart
                }
            );
            assert_eq!(terminal.ran, Some(Ran::None));
            assert_eq!(
                job.hosted.as_ref().unwrap().billing,
                crate::hosted::BillingStatus::Unavailable
            );
            assert_eq!(
                job.hosted.as_ref().unwrap().terminal.as_ref(),
                Some(terminal)
            );
            assert!(job.hosted.as_ref().unwrap().hosted_no_fallback);
            if case == "remote_unknown" {
                assert_eq!(job.remote_terminal.as_ref(), Some(terminal));
            }
            let until = job.retain_until_ms.unwrap();
            assert!(until >= job.terminal_observed_at_ms.unwrap() + RETENTION_MS);
            assert!(job.terminal_delivered_at_ms.is_none());
            let mut state = storage.state.lock().unwrap().clone();
            state.prune(until - 1);
            assert!(state.jobs.contains_key(&id));
            state.prune(until);
            if !job.hosted.as_ref().unwrap().finish_done {
                assert!(
                    state.jobs.contains_key(&id),
                    "unsettled booking must survive terminal retention expiry"
                );
                state
                    .jobs
                    .get_mut(&id)
                    .unwrap()
                    .hosted
                    .as_mut()
                    .unwrap()
                    .finish_done = true;
                state.prune(until);
            }
            assert!(
                !state.jobs.contains_key(&id),
                "undelivered tombstone is bounded"
            );
            drop(storage);
            let storage = Storage::open(&scratch).unwrap();
            let reopened = storage.job(id).unwrap();
            assert_eq!(reopened.local_terminal, job.local_terminal);
            assert_eq!(reopened.retain_until_ms, job.retain_until_ms);
        } else {
            assert!(job == original, "startup must preserve {case}");
        }
    }
}

#[test]
fn settlement_terminal_preserves_existing_retention_and_is_idempotent() {
    let scratch = cortexkit_test_support::ScratchDir::new("hosted-settlement-retention");
    let storage = Storage::open(&scratch).unwrap();
    let id = Uuid::now_v7();
    storage.create(id, 60, None, None, false).unwrap();
    storage.hosted_route(id, Default::default()).unwrap();
    storage
        .update(|s| s.jobs.get_mut(&id).unwrap().retain_until_ms = Some(u64::MAX))
        .unwrap();
    assert!(storage.job(id).unwrap().local_terminal.is_none());
    storage
        .hosted_settlement_update(id, true, |s| s.finish_done = true)
        .unwrap();
    let job = storage.job(id).unwrap();
    assert!(job.hosted.as_ref().unwrap().finish_done);
    assert!(job.local_terminal.is_some());
    assert_eq!(job.retain_until_ms, Some(u64::MAX));
    let writes = storage
        .write_count
        .load(std::sync::atomic::Ordering::SeqCst);
    storage.record_hosted_settlement_terminal(id).unwrap();
    assert_eq!(
        storage
            .write_count
            .load(std::sync::atomic::Ordering::SeqCst),
        writes
    );
    assert!(storage.job(id).unwrap() == job);
}

#[test]
fn released_confirmation_rechecks_run_flag_and_excludes_archived_spills() {
    for case in ["never_started", "run_sent", "spill"] {
        let scratch = cortexkit_test_support::ScratchDir::new("hosted-release-confirmation");
        let storage = Storage::open(&scratch).unwrap();
        let id = Uuid::now_v7();
        storage.create(id, 60, None, None, false).unwrap();
        storage.hosted_route(id, Default::default()).unwrap();
        storage
            .update(|s| {
                let job = s.jobs.get_mut(&id).unwrap();
                if case == "run_sent" {
                    job.hosted.as_mut().unwrap().run_sent = true;
                } else if case == "spill" {
                    job.build_server_job_id = Some(Uuid::now_v7());
                }
            })
            .unwrap();
        let before = storage.job(id).unwrap();
        assert!(!before.hosted.as_ref().unwrap().finish_done);
        assert_eq!(
            storage.confirm_hosted_ended_before_run(id).unwrap(),
            case == "never_started"
        );
        let after = storage.job(id).unwrap();
        if case == "never_started" {
            assert!(after.hosted.as_ref().unwrap().finish_done);
            assert_eq!(after.local_terminal.unwrap().ran, Some(Ran::None));
        } else {
            assert!(after == before, "release must not change {case}");
        }
    }
}
