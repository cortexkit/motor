use super::*;
use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

fn copy_store(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        if name == "state.json" || name.starts_with("journal-") {
            fs::copy(entry.path(), to.join(name)).unwrap();
        }
    }
}
fn log(root: &Path) -> PathBuf {
    fs::read_dir(root)
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name().to_str().unwrap().starts_with("journal-"))
        .unwrap()
        .path()
}
fn stop_worker(storage: &Storage) {
    storage.persistence.stop();
    storage
        .worker
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .join()
        .unwrap();
}
fn terminal(id: Uuid) -> TerminalRecord {
    TerminalRecord::new(
        id,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull,
        },
        0,
        0,
        0,
    )
    .with_ran(Ran::None)
}

#[test]
fn journal_replays_atomic_upserts_and_deletions_without_shutdown() {
    let root = ScratchDir::new("motor-journal-maps");
    let crash = ScratchDir::new("motor-journal-maps-crash");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    let deleted = Uuid::now_v7();
    storage
        .update(|s| {
            s.jobs.insert(
                id,
                Job {
                    direct_secret_hash: Some([7; 32]),
                    ..Job::default()
                },
            );
            s.jobs.insert(deleted, Job::default());
            s.snapshots.insert(
                "kept".into(),
                BTreeMap::from([("repo".into(), "commit".into())]),
            );
            s.snapshots.insert("removed".into(), BTreeMap::new());
            s.verified_tips.insert(
                "repo".into(),
                VerifiedTip {
                    remote_url: "origin".into(),
                    branch: "main".into(),
                    commit: "sha".into(),
                    verified_at_ms: now_ms(),
                },
            );
        })
        .unwrap();
    storage
        .update(|s| {
            s.jobs.get_mut(&id).unwrap().pending_cancel = true;
            s.jobs.remove(&deleted);
            s.snapshots.remove("removed");
            s.snapshot_used_at_ms.remove("removed");
            s.verified_tips.remove("repo");
        })
        .unwrap();
    copy_store(&root, &crash);
    assert!(
        !fs::read_to_string(crash.join("state.json"))
            .unwrap()
            .contains(&id.to_string()),
        "exercise journal recovery, not a shutdown checkpoint"
    );
    let recovered = Storage::open(&crash).unwrap();
    assert!(recovered.job(id).unwrap().pending_cancel);
    assert_eq!(recovered.job(id).unwrap().direct_secret_hash, Some([7; 32]));
    assert!(recovered.job(deleted).is_none());
    assert_eq!(recovered.previous("kept")["repo"], "commit");
    assert!(recovered.previous("removed").is_empty());
    assert!(recovered.verified_tip("repo").is_none());
}

#[test]
fn torn_final_frames_are_truncated_before_later_appends() {
    let root = ScratchDir::new("motor-journal-torn");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    storage.create(id, 60, None, None, false).unwrap();
    storage.cancel(id, true).unwrap();
    let bytes = fs::read(log(&root)).unwrap();
    let start = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
    let frame_len = bytes.len() - start;
    for cut in [1, 32, 65, frame_len / 2, frame_len - 1, frame_len] {
        let crash = ScratchDir::new("motor-journal-tail-crash");
        copy_store(&root, &crash);
        let mut torn = bytes[..start + cut].to_vec();
        if cut == frame_len {
            torn[start] ^= 1;
        }
        fs::write(log(&crash), torn).unwrap();
        let recovered = Storage::open(&crash).unwrap();
        assert!(!recovered.job(id).unwrap().pending_cancel, "cut={cut}");
        recovered
            .retain_job(id, Some("after-repair".into()))
            .unwrap();
        let second_crash = ScratchDir::new("motor-journal-tail-reappend");
        copy_store(&crash, &second_crash);
        let recovered = Storage::open(&second_crash).unwrap();
        assert_eq!(
            recovered.job(id).unwrap().workspace_key.as_deref(),
            Some("after-repair")
        );
    }
}

#[test]
fn corrupt_interior_frames_and_sequence_gaps_are_errors() {
    let root = ScratchDir::new("motor-journal-interior");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    storage.create(id, 60, None, None, false).unwrap();
    storage.cancel(id, true).unwrap();
    for gap in [false, true] {
        let crash = ScratchDir::new("motor-journal-interior-crash");
        copy_store(&root, &crash);
        let mut bytes = fs::read(log(&crash)).unwrap();
        if gap {
            let start = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
            bytes.drain(..start);
        } else {
            bytes[0] ^= 1;
        }
        fs::write(log(&crash), bytes).unwrap();
        assert!(Storage::open(&crash).is_err());
    }
}

#[test]
fn checkpoint_rotation_publication_and_cleanup_keep_the_suffix() {
    let root = ScratchDir::new("motor-checkpoint-windows");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    storage.create(id, 60, None, None, false).unwrap();
    storage.cancel(id, true).unwrap();
    let (snapshot, position) = {
        let state = storage.state.lock().unwrap();
        let mut backend = storage.persistence.backend.lock().unwrap();
        let journal::Backend::Journal(j) = &mut *backend else {
            panic!()
        };
        (state.clone(), j.begin_checkpoint(&root).unwrap().unwrap())
    };
    let rotated = ScratchDir::new("motor-checkpoint-rotated");
    copy_store(&root, &rotated);
    assert!(
        Storage::open(&rotated)
            .unwrap()
            .job(id)
            .unwrap()
            .pending_cancel
    );
    storage.cancel(id, false).unwrap();
    journal::snapshot(
        &root,
        &snapshot,
        Some(&position),
        &storage.persistence.metrics,
    )
    .unwrap();
    let published = ScratchDir::new("motor-checkpoint-published");
    copy_store(&root, &published);
    assert!(
        !Storage::open(&published)
            .unwrap()
            .job(id)
            .unwrap()
            .pending_cancel
    );
    if let journal::Backend::Journal(j) = &mut *storage.persistence.backend.lock().unwrap() {
        j.finish_checkpoint(&root, &position).unwrap();
    }
    let cleaned = ScratchDir::new("motor-checkpoint-cleaned");
    copy_store(&root, &cleaned);
    assert!(
        !Storage::open(&cleaned)
            .unwrap()
            .job(id)
            .unwrap()
            .pending_cancel
    );
}

#[test]
fn migration_failure_is_nonfatal_and_old_writer_state_is_authoritative() {
    let root = ScratchDir::new("motor-journal-migration");
    let id = Uuid::now_v7();
    fs::write(root.join("state.json"), serde_json::to_vec(&json!({"jobs": {id.to_string(): {"queue_wait_limit_s": 60, "pending_cancel": false, "local_terminal": null}}, "snapshots": {}})).unwrap()).unwrap();
    fs::create_dir(root.join("state.tmp")).unwrap();
    let storage = Storage::open(&root).unwrap();
    assert!(matches!(
        *storage.persistence.backend.lock().unwrap(),
        journal::Backend::Legacy
    ));
    fs::remove_dir(root.join("state.tmp")).unwrap();
    storage.cancel(id, true).unwrap();
    drop(storage);
    let storage = Storage::open(&root).unwrap();
    assert!(matches!(
        *storage.persistence.backend.lock().unwrap(),
        journal::Backend::Journal(_)
    ));
    storage.cancel(id, false).unwrap();
    let crash = ScratchDir::new("motor-journal-old-writer");
    copy_store(&root, &crash);
    let mut old: Value =
        serde_json::from_slice(&fs::read(crash.join("state.json")).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("_journal");
    old["jobs"][id.to_string()]["pending_cancel"] = json!(true);
    fs::write(crash.join("state.json"), serde_json::to_vec(&old).unwrap()).unwrap();
    assert!(
        Storage::open(&crash)
            .unwrap()
            .job(id)
            .unwrap()
            .pending_cancel,
        "do not resurrect the stale journal's false cancellation state"
    );
}

#[test]
fn checkpoint_age_triggers_at_five_minutes_even_below_byte_threshold() {
    let root = ScratchDir::new("motor-journal-age");
    let storage = Storage::open(&root).unwrap();
    storage
        .create(Uuid::now_v7(), 60, None, None, false)
        .unwrap();
    {
        let mut backend = storage.persistence.backend.lock().unwrap();
        let journal::Backend::Journal(j) = &mut *backend else {
            panic!()
        };
        j.last_checkpoint = Instant::now() - journal::MAX_AGE + Duration::from_secs(1);
        assert!(!backend.due());
        let journal::Backend::Journal(j) = &mut *backend else {
            panic!()
        };
        j.last_checkpoint = Instant::now() - journal::MAX_AGE;
        assert!(backend.bytes() < journal::SOFT_BYTES);
        assert!(backend.due());
    }
    storage.persistence.request_if_due();
    assert!(storage.persistence.wait_until_checkpointed());
    assert!(!storage.persistence.backend.lock().unwrap().pending());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(root.join("state.json")).unwrap()).unwrap()
            ["jobs"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn hard_budget_rejects_effects_when_background_compaction_fails() {
    let root = ScratchDir::new("motor-journal-budget");
    let storage = Storage::open(&root).unwrap();
    let retained = Uuid::now_v7();
    storage.create(retained, 60, None, None, false).unwrap();
    // Fix the first two access times so they round to the same saved deadline
    // and the second needs no journal write. One millisecond past that bucket
    // requires a new deadline, which the full journal must reject while
    // checkpointing is blocked.
    let start = now_ms().div_ceil(RETENTION_RENEWAL_MS) * RETENTION_RENEWAL_MS;
    storage.retain_job_at(retained, None, start + 1).unwrap();
    let until = storage.job(retained).unwrap().retain_until_ms;
    fs::create_dir(root.join("state.tmp")).unwrap();
    storage
        .accepted(
            "large".into(),
            BTreeMap::from([("repo".into(), "a".repeat(journal::HARD_BYTES as usize))]),
        )
        .unwrap();
    assert!(storage.persistence.backend.lock().unwrap().bytes() >= journal::HARD_BYTES);
    assert!(storage.persistence.wait_for_compaction().is_err());
    let writes = storage.write_count.load(Ordering::SeqCst);
    storage
        .retain_job_at(retained, None, start + RETENTION_RENEWAL_MS)
        .unwrap();
    storage.retain_job(Uuid::now_v7(), None).unwrap();
    assert!(storage
        .retain_job_at(retained, None, start + RETENTION_RENEWAL_MS + 1)
        .is_err());
    assert_eq!(storage.job(retained).unwrap().retain_until_ms, until);
    let id = Uuid::now_v7();
    assert!(storage.cancel(id, true).is_err());
    assert!(storage.job(id).is_none());
    assert_eq!(storage.write_count.load(Ordering::SeqCst), writes);
    fs::remove_dir(root.join("state.tmp")).unwrap();
}

#[test]
fn direct_hash_creation_is_atomic_and_append_failure_is_not_published() {
    let root = ScratchDir::new("motor-journal-create-hash");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    if let journal::Backend::Journal(j) = &mut *storage.persistence.backend.lock().unwrap() {
        j.set_append_readonly(true);
    }
    assert!(storage
        .create_authorized(id, 60, None, None, true, Some([9; 32]))
        .is_err());
    assert!(storage.job(id).is_none());
    if let journal::Backend::Journal(j) = &mut *storage.persistence.backend.lock().unwrap() {
        j.set_append_readonly(false);
    }
    storage
        .create_authorized(id, 60, None, None, true, Some([9; 32]))
        .unwrap();
    let crash = ScratchDir::new("motor-journal-create-hash-crash");
    copy_store(&root, &crash);
    assert_eq!(
        Storage::open(&crash)
            .unwrap()
            .job(id)
            .unwrap()
            .direct_secret_hash,
        Some([9; 32])
    );
}

#[test]
fn offline_checkpoint_takes_the_lease_and_fails_closed_on_export_error() {
    let root = ScratchDir::new("motor-offline-checkpoint");
    let storage = Storage::open(&root).unwrap();
    let args = vec!["--root".into(), root.to_str().unwrap().into()];
    assert_eq!(
        crate::cli::store_checkpoint_main(&args),
        1,
        "live lease must refuse export"
    );
    let id = Uuid::now_v7();
    storage.cancel(id, true).unwrap();
    let crash = ScratchDir::new("motor-offline-checkpoint-crash");
    copy_store(&root, &crash);
    let args = vec!["--root".into(), crash.to_str().unwrap().into()];
    fs::create_dir(crash.join("state.tmp")).unwrap();
    assert_eq!(crate::cli::store_checkpoint_main(&args), 1);
    fs::remove_dir(crash.join("state.tmp")).unwrap();
    assert_eq!(crate::cli::store_checkpoint_main(&args), 0);
    let value: Value =
        serde_json::from_slice(&fs::read(crash.join("state.json")).unwrap()).unwrap();
    assert_eq!(value["jobs"][id.to_string()]["pending_cancel"], true);
    assert_eq!(
        crate::cli::store_checkpoint_main(&[]),
        crate::cli::EXIT_USAGE
    );
}

fn seed_state() -> State {
    let mut state = State::default();
    for n in 0..8000 {
        state.jobs.insert(
            Uuid::now_v7(),
            Job {
                queue_wait_limit_s: 900,
                runner_instance: Some("build-server".into()),
                direct_secret_hash: Some([7; 32]),
                workspace_key: Some(format!("workspace-{}", n % 547)),
                retain_until_ms: Some(now_ms() + RETENTION_MS),
                attribution: Some(JobAttribution {
                    session: format!("session-{n}"),
                    preset: Some("worker".into()),
                    scope_ref: "scope".into(),
                    scope_epoch: 1,
                }),
                ..Job::default()
            },
        );
    }
    for n in 0..547 {
        let key = format!("workspace-{n}");
        state.snapshots.insert(
            key.clone(),
            BTreeMap::from([("repo".into(), "a".repeat(40))]),
        );
        state.snapshot_used_at_ms.insert(key, now_ms());
    }
    state
}
fn lifecycle(storage: &Storage, id: Uuid) {
    storage
        .create_authorized(
            id,
            900,
            None,
            Some("build-server".into()),
            true,
            Some([3; 32]),
        )
        .unwrap();
    storage.retain_job(id, Some("workspace-0".into())).unwrap();
    storage
        .hosted_route(id, crate::HostedJobState::new(&crate::Config::default()))
        .unwrap();
    storage.hosted_update(id, |s| s.run_sent = true).unwrap();
    storage
        .hosted_update(id, |s| {
            s.terminal = Some(terminal(id));
            s.pending_finish = Some(crate::hosted::placement::Finish {
                outcome: "lost".into(),
                run_ms: 0,
            });
        })
        .unwrap();
    storage
        .hosted_update(id, |s| {
            s.pending_finish = None;
            s.finish_done = true;
        })
        .unwrap();
    storage.terminal_observed(id).unwrap();
    storage.terminal_delivered(id).unwrap();
}

#[test]
fn lifecycle_writes_record_sized_bytes_with_8000_retained_jobs() {
    let seed = seed_state();
    let mut results = Vec::new();
    for legacy in [true, false] {
        let root = ScratchDir::new("motor-journal-write-volume");
        fs::write(root.join("state.json"), serde_json::to_vec(&seed).unwrap()).unwrap();
        let storage = Storage::open(&root).unwrap();
        if legacy {
            *storage.persistence.backend.lock().unwrap() = journal::Backend::Legacy;
        }
        let metrics = &storage.persistence.metrics;
        metrics.append_bytes.store(0, Ordering::Relaxed);
        metrics.checkpoint_bytes.store(0, Ordering::Relaxed);
        metrics.syncs.store(0, Ordering::Relaxed);
        lifecycle(&storage, Uuid::now_v7());
        let hot = metrics.append_bytes.load(Ordering::Relaxed)
            + metrics.checkpoint_bytes.load(Ordering::Relaxed);
        let syncs = metrics.syncs.load(Ordering::Relaxed);
        metrics.checkpoint_bytes.store(0, Ordering::Relaxed);
        if !legacy {
            storage.persistence.checkpoint(&storage.state).unwrap();
        }
        let background = metrics.checkpoint_bytes.load(Ordering::Relaxed);
        eprintln!("8000 retained jobs + 547 snapshots; legacy={legacy}: lifecycle_bytes={hot}, lifecycle_syncs={syncs}, explicit_checkpoint_bytes={background}");
        results.push(hot);
    }
    assert!(
        results[0] > 8_000_000,
        "legacy comparison must actually rewrite the seeded store"
    );
    assert!(
        results[1] < 32_768,
        "eight real lifecycle transitions must write only their changed records"
    );
    assert!(results[0] > results[1] * 100);
}

fn rollback_fixture(root: &Path) -> (Storage, Uuid, Value) {
    let storage = Storage::open(root).unwrap();
    let id = Uuid::now_v7();
    lifecycle(&storage, id);
    storage.local_terminal(terminal(id)).unwrap();
    storage.cancel(id, true).unwrap();
    storage
        .accepted(
            "rollback".into(),
            BTreeMap::from([("repo".into(), "commit".into())]),
        )
        .unwrap();
    storage
        .verify_tip(
            "repo".into(),
            VerifiedTip {
                remote_url: "origin".into(),
                branch: "main".into(),
                commit: "sha".into(),
                verified_at_ms: now_ms(),
            },
        )
        .unwrap();
    storage
        .update(|s| {
            let job = s.jobs.get_mut(&id).unwrap();
            job.terminal_delivered_at_ms = None;
            job.retain_until_ms = Some(1);
            job.hosted.as_mut().unwrap().pending_finish = Some(crate::hosted::placement::Finish {
                outcome: "lost".into(),
                run_ms: 0,
            });
        })
        .unwrap();
    let expected = durable_value(root).unwrap();
    (storage, id, expected)
}
fn assert_old_client(root: &Path, id: Uuid, expected: &Value) {
    let helper = std::env::var_os("MOTOR_ROLLBACK_CLIENT")
        .expect("run python3 scripts/mixed-version.py for the real 222d833 client");
    let config = root.join("rollback-config.json");
    fs::write(
        &config,
        serde_json::to_vec(&crate::Config::default()).unwrap(),
    )
    .unwrap();
    let output = std::process::Command::new(helper)
        .args([
            "127.0.0.1:1",
            config.to_str().unwrap(),
            root.to_str().unwrap(),
            "store",
            &id.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut actual_job = actual["jobs"][id.to_string()].clone();
    let mut expected_job = expected["jobs"][id.to_string()].clone();
    assert_eq!(actual_job["pending_cancel"], true);
    assert_eq!(actual_job["hosted"]["run_sent"], true);
    assert!(
        actual.get("_journal").is_none(),
        "old attach must actually rewrite the compatible JSON"
    );
    assert!(actual_job["retain_until_ms"].as_u64().unwrap() > 1);
    actual_job
        .as_object_mut()
        .unwrap()
        .remove("retain_until_ms");
    expected_job
        .as_object_mut()
        .unwrap()
        .remove("retain_until_ms");
    assert_eq!(
        actual_job, expected_job,
        "old reader/writer must retain every job transition"
    );
    for map in ["snapshots", "snapshot_used_at_ms", "verified_tips"] {
        assert_eq!(actual[map], expected[map]);
    }
    let reopened = Storage::open(root).unwrap();
    assert!(reopened.job(id).unwrap().pending_cancel);
    assert!(reopened.job(id).unwrap().hosted.unwrap().run_sent);
    assert!(reopened.job(id).unwrap().retain_until_ms.unwrap() > 1);
}

#[test]
#[ignore = "mixed-version gate builds the real rollback client"]
fn rollback_clean_shutdown_exports_every_transition_for_222d833() {
    let root = ScratchDir::new("motor-clean-rollback");
    let (storage, id, expected) = rollback_fixture(&root);
    storage.shutdown().unwrap();
    drop(storage);
    assert_old_client(&root, id, &expected);
}

#[test]
#[ignore = "mixed-version gate builds the real rollback client"]
fn rollback_offline_checkpoint_exports_crash_suffix_for_222d833() {
    let root = ScratchDir::new("motor-crash-rollback");
    let (storage, id, expected) = rollback_fixture(&root);
    stop_worker(&storage);
    let crash = ScratchDir::new("motor-crash-rollback-copy");
    copy_store(&root, &crash);
    let stale: Value =
        serde_json::from_slice(&fs::read(crash.join("state.json")).unwrap()).unwrap();
    assert!(stale["jobs"].as_object().unwrap().is_empty());
    assert_eq!(
        crate::cli::store_checkpoint_main(&["--root".into(), crash.to_str().unwrap().into()]),
        0
    );
    assert_old_client(&crash, id, &expected);
}

#[test]
fn failed_rotation_cannot_append_behind_an_unpublished_segment() {
    let root = ScratchDir::new("motor-rotation-failure");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    storage.create(id, 60, None, None, false).unwrap();
    storage.cancel(id, true).unwrap();
    let checkpoint: Value =
        serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap();
    let epoch = checkpoint["_journal"]["epoch"].as_str().unwrap();
    // Model creation succeeding before its directory sync reports an error.
    fs::write(root.join(format!("journal-{epoch}-{:020}.jsonl", 3)), b"").unwrap();
    assert!(storage.persistence.checkpoint(&storage.state).is_err());
    assert!(storage.cancel(id, false).is_err());
    let crash = ScratchDir::new("motor-rotation-failure-crash");
    copy_store(&root, &crash);
    let recovered = Storage::open(&crash).unwrap();
    assert!(recovered.job(id).unwrap().pending_cancel);
    recovered.cancel(id, false).unwrap();
    let again = ScratchDir::new("motor-rotation-failure-reappend");
    copy_store(&crash, &again);
    assert!(
        !Storage::open(&again)
            .unwrap()
            .job(id)
            .unwrap()
            .pending_cancel
    );
}
