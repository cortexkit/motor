use super::*;
use crate::storage::{Job, JobAttribution, VerifiedTip, RETENTION_MS};
use cortexkit_test_support::ScratchDir;
use motor_protocol::{Outcome, Ran, RefusalReason, ReturnResult, TerminalRecord};
use serde_json::json;
use std::hint::black_box;

// Retain the whole-state implementation as an independent compatibility oracle.
fn whole_state_delta(before: &State, after: &State) -> io::Result<Delta> {
    let before = serde_json::to_value(before).map_err(io::Error::other)?;
    let after = serde_json::to_value(after).map_err(io::Error::other)?;
    let mut delta = BTreeMap::new();
    for name in ["jobs", "snapshots", "snapshot_used_at_ms", "verified_tips"] {
        let empty = serde_json::Map::new();
        let old = before[name].as_object().unwrap_or(&empty);
        let new = after[name].as_object().unwrap_or(&empty);
        let mut changes = BTreeMap::new();
        for (key, value) in new {
            if old.get(key) != Some(value) {
                changes.insert(key.clone(), Some(value.clone()));
            }
        }
        for key in old.keys() {
            if !new.contains_key(key) {
                changes.insert(key.clone(), None);
            }
        }
        if !changes.is_empty() {
            delta.insert(name.to_owned(), changes);
        }
    }
    Ok(Delta(delta))
}

fn assert_equivalent(label: &str, before: &State, after: &State) {
    let expected = whole_state_delta(before, after).unwrap();
    let actual = Delta::between(before, after).unwrap();
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&expected).unwrap(),
        "delta bytes: {label}"
    );
    let mut replayed = serde_json::to_value(before).unwrap();
    let mut reference = replayed.clone();
    actual.apply(&mut replayed).unwrap();
    expected.apply(&mut reference).unwrap();
    assert_eq!(replayed, reference, "replay: {label}");
    let epoch = Uuid::from_u128(1);
    assert_eq!(
        serde_json::to_vec(&Frame {
            epoch,
            seq: 42,
            delta: Delta::between(before, after).unwrap(),
        })
        .unwrap(),
        serde_json::to_vec(&Frame {
            epoch,
            seq: 42,
            delta: whole_state_delta(before, after).unwrap(),
        })
        .unwrap(),
        "frame bytes: {label}"
    );
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

fn job(id: Uuid, now: u64) -> Job {
    Job {
        queue_wait_limit_s: 900,
        runner_instance: Some("build-server".into()),
        direct_secret_hash: Some([7; 32]),
        workspace_key: Some("workspace".into()),
        retain_until_ms: Some(now + RETENTION_MS),
        attribution: Some(JobAttribution {
            session: id.to_string(),
            preset: Some("worker".into()),
            scope_ref: "scope".into(),
            scope_epoch: 1,
        }),
        ..Job::default()
    }
}

#[test]
fn typed_delta_matches_whole_state_reference() {
    let now = 2 * RETENTION_MS;
    let id = Uuid::now_v7();
    let mut before = State::default();
    let mut after = before.clone();
    after.jobs.insert(id, job(id, now));
    assert_equivalent("new job", &before, &after);

    before = after.clone();
    let job = after.jobs.get_mut(&id).unwrap();
    job.pending_cancel = true;
    job.hosted = Some(crate::HostedJobState::new(&crate::Config::default()));
    assert_equivalent("job update", &before, &after);

    before = after.clone();
    let hosted = after.jobs.get_mut(&id).unwrap().hosted.as_mut().unwrap();
    hosted.run_sent = true;
    hosted.terminal = Some(terminal(id));
    hosted.pending_finish = Some(crate::hosted::placement::Finish {
        outcome: "lost".into(),
        run_ms: 123,
    });
    assert_equivalent("hosted terminal update", &before, &after);

    before = after.clone();
    after.snapshots.insert(
        "workspace".into(),
        BTreeMap::from([("repo".into(), "a".repeat(40))]),
    );
    after.snapshot_used_at_ms.insert("workspace".into(), now);
    assert_equivalent("snapshot add", &before, &after);

    before = after.clone();
    after
        .snapshots
        .get_mut("workspace")
        .unwrap()
        .insert("repo".into(), "b".repeat(40));
    after
        .snapshot_used_at_ms
        .insert("workspace".into(), now + 1);
    assert_equivalent("snapshot update", &before, &after);

    before = after.clone();
    after.snapshots.remove("workspace");
    after.snapshot_used_at_ms.remove("workspace");
    assert_equivalent("snapshot remove", &before, &after);

    before = after.clone();
    after.verified_tips.insert(
        "repo".into(),
        VerifiedTip {
            remote_url: "https://example.com/repo.git".into(),
            branch: "main".into(),
            commit: "c".repeat(40),
            verified_at_ms: now,
        },
    );
    assert_equivalent("first verified tip", &before, &after);

    before = after.clone();
    after.verified_tips.get_mut("repo").unwrap().commit = "d".repeat(40);
    assert_equivalent("verified tip update", &before, &after);

    before = after.clone();
    after.verified_tips.clear();
    assert_equivalent("last verified tip removed", &before, &after);

    before = after.clone();
    let expired = Uuid::now_v7();
    before.jobs.insert(
        expired,
        Job {
            local_terminal: Some(terminal(expired)),
            terminal_observed_at_ms: Some(1),
            terminal_delivered_at_ms: Some(2),
            retain_until_ms: Some(now),
            ..Job::default()
        },
    );
    before.snapshots.insert("expired".into(), BTreeMap::new());
    before.snapshot_used_at_ms.insert("expired".into(), 1);
    after = before.clone();
    assert!(after.prune(now));
    assert!(!after.jobs.contains_key(&expired));
    assert!(!after.snapshots.contains_key("expired"));
    assert_equivalent("retention prune", &before, &after);

    before = after.clone();
    after.jobs.get_mut(&id).unwrap().legacy_shape = true;
    after.active_jobs.insert(id, 2);
    assert_equivalent("only skipped in-memory fields changed", &before, &after);
    assert!(Delta::between(&before, &after).unwrap().0.is_empty());
    assert_equivalent("no-op", &after, &after);
}

#[test]
fn typed_delta_compares_every_serialized_job_field() {
    let id = Uuid::now_v7();
    let mut rich = job(id, 2 * RETENTION_MS);
    rich.hosted = Some(crate::HostedJobState::new(&crate::Config::default()));
    rich.return_result = Some(ReturnResult {
        job_id: id,
        seq: 3,
        returned_files: Some(vec![]),
        return_paths_failed: None,
    });
    rich.return_cwd = Some("/workspace".into());
    rich.return_requested = Some(vec!["result.txt".into()]);
    rich.gate_job = true;
    rich.windows_runner = Some(crate::WindowsRunner::default());
    rich.pending_cancel = true;
    rich.local_terminal = Some(terminal(id));
    rich.terminal_observed_at_ms = Some(1);
    rich.terminal_delivered_at_ms = Some(2);
    rich.build_server_job_id = Some(Uuid::now_v7());
    rich.routed_direct_reason = Some("no_host_fits".into());
    rich.routed_direct_hosted_max_vcpus = Some(21);
    let baseline = serde_json::to_value(Job::default()).unwrap();
    let mut before = State::default();
    before.jobs.insert(id, Job::default());
    for (field, value) in serde_json::to_value(rich).unwrap().as_object().unwrap() {
        assert_ne!(baseline.get(field), Some(value), "fixture field: {field}");
        let mut changed = baseline.clone();
        changed[field] = value.clone();
        let mut after = before.clone();
        after
            .jobs
            .insert(id, serde_json::from_value(changed).unwrap());
        assert_equivalent(field, &before, &after);
    }
}

#[test]
fn typed_delta_serializes_only_changed_entries() {
    struct Record(u64, Arc<AtomicU64>);
    impl PartialEq for Record {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }
    impl Serialize for Record {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.1.fetch_add(1, Ordering::Relaxed);
            serializer.serialize_u64(self.0)
        }
    }
    let calls = Arc::new(AtomicU64::new(0));
    let before = BTreeMap::from([
        ("unchanged", Record(1, calls.clone())),
        ("updated", Record(2, calls.clone())),
        ("deleted", Record(3, calls.clone())),
    ]);
    let after = BTreeMap::from([
        ("unchanged", Record(1, calls.clone())),
        ("updated", Record(4, calls.clone())),
        ("inserted", Record(5, calls.clone())),
    ]);
    let mut delta = BTreeMap::new();
    Delta::map_changes(&mut delta, "jobs", &before, &after).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        serde_json::to_value(Delta(delta)).unwrap(),
        json!({"jobs": {"updated": 4, "inserted": 5, "deleted": null}})
    );
    calls.store(0, Ordering::Relaxed);
    let mut delta = BTreeMap::new();
    Delta::map_changes(&mut delta, "jobs", &before, &before).unwrap();
    assert!(delta.is_empty());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

fn fleet_state() -> State {
    let now = 2 * RETENTION_MS;
    let mut state = State::default();
    for n in 0..8000 {
        let id = Uuid::now_v7();
        let mut job = job(id, now);
        job.workspace_key = Some(format!("workspace-{}", n % 607));
        if n % 5 == 0 {
            job.hosted = Some(crate::HostedJobState::new(&crate::Config::default()));
        }
        state.jobs.insert(id, job);
    }
    for n in 0..607 {
        let key = format!("workspace-{n}");
        state.snapshots.insert(
            key.clone(),
            BTreeMap::from([("repo".into(), "a".repeat(40))]),
        );
        state.snapshot_used_at_ms.insert(key, now);
    }
    for n in 0..30 {
        state.verified_tips.insert(
            format!("repo-{n}"),
            VerifiedTip {
                remote_url: format!("https://example.com/repo-{n}.git"),
                branch: "main".into(),
                commit: "b".repeat(40),
                verified_at_ms: now,
            },
        );
    }
    state
}

#[test]
#[ignore = "append timing measurement; run optimized with --ignored --nocapture"]
fn append_timings_with_8000_jobs() {
    const ITERATIONS: u32 = 40;
    let before = fleet_state();
    let mut after = before.clone();
    after.jobs.values_mut().next().unwrap().pending_cancel = true;
    assert_equivalent("fleet single-job update", &before, &after);
    for noop in [false, true] {
        let old_root = ScratchDir::new("motor-delta-old-timing");
        let new_root = ScratchDir::new("motor-delta-new-timing");
        let old_metrics = Arc::new(Metrics::default());
        let new_metrics = Arc::new(Metrics::default());
        // Both algorithms use the production frame writer, checksum and fsync.
        // State creation, checkpointing, cloning and pruning are outside this timer.
        let checkpoint = serde_json::to_vec(&before).unwrap();
        fs::write(old_root.join("state.json"), &checkpoint).unwrap();
        fs::write(new_root.join("state.json"), &checkpoint).unwrap();
        let (_, mut old_backend) = open(&old_root, &old_metrics).unwrap();
        let (_, mut new_backend) = open(&new_root, &new_metrics).unwrap();
        assert!(matches!(old_backend, Backend::Journal(_)));
        assert!(matches!(new_backend, Backend::Journal(_)));
        let mut old_elapsed = Duration::ZERO;
        let mut new_elapsed = Duration::ZERO;
        for n in 0..ITERATIONS + 4 {
            let (from, to) = if noop {
                (&before, &before)
            } else if n % 2 == 0 {
                (&before, &after)
            } else {
                (&after, &before)
            };
            // Alternate order and discard four warmup pairs to reduce drift.
            for old_first in [n % 2 == 0, n % 2 != 0] {
                let start = Instant::now();
                if old_first {
                    let Backend::Journal(j) = &mut old_backend else {
                        unreachable!()
                    };
                    j.append_delta(
                        whole_state_delta(black_box(from), black_box(to)).unwrap(),
                        &old_metrics,
                    )
                    .unwrap();
                    if n >= 4 {
                        old_elapsed += start.elapsed();
                    }
                } else {
                    new_backend
                        .append(&new_root, black_box(from), black_box(to), &new_metrics)
                        .unwrap();
                    if n >= 4 {
                        new_elapsed += start.elapsed();
                    }
                }
            }
        }
        assert_eq!(
            old_metrics.append_bytes.load(Ordering::Relaxed),
            new_metrics.append_bytes.load(Ordering::Relaxed)
        );
        let syncs = new_metrics.syncs.load(Ordering::Relaxed);
        assert_eq!(syncs, old_metrics.syncs.load(Ordering::Relaxed));
        assert_eq!(
            syncs,
            if noop {
                2
            } else {
                u64::from(ITERATIONS + 4) + 2
            }
        );
        let old_us = old_elapsed.as_secs_f64() * 1_000_000.0 / f64::from(ITERATIONS);
        let new_us = new_elapsed.as_secs_f64() * 1_000_000.0 / f64::from(ITERATIONS);
        eprintln!("8000 jobs, 607 snapshots, 607 timestamps, 30 tips; noop={noop}; {ITERATIONS} appends: whole_state_us={old_us:.2}, typed_us={new_us:.2}, speedup={:.2}x", old_us / new_us);
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(before.clone());
    }
    eprintln!(
        "8000 jobs: clone_and_drop_us={:.2}",
        start.elapsed().as_secs_f64() * 1_000_000.0 / f64::from(ITERATIONS)
    );
}
