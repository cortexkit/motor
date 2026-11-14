use super::*;

const ROOT: &str = "/home/repository";

fn setup() -> (Arc<Runtime>, Arc<Fake>, Arc<Storage>, Claims, u64) {
    let (runtime, fake, storage) = fixture();
    hold_boot(&runtime, &fake, "unused");
    let mut c = claims(JOB);
    c.cache_write = Some(true);
    let id = claim(&runtime, &c);
    (runtime, fake, storage, c, id)
}

fn donor(host: &mut Host, id: &str, used: u64) {
    let mut execution = host.snapshot.per_job.executions[JOB].clone();
    let mut disk = host.snapshot.per_job.disks[&execution.disk_id].clone();
    disk.queue.clear();
    disk.attached_job = None;
    disk.workspace_key_hash = motor_lease::base64url(&[6; 32]);
    disk.repository_key = Some(crate::jobs::fill_source::repository_key(ROOT));
    disk.storage.as_mut().unwrap().last_used_ms = used;
    disk.have = vec![motor_protocol::MirrorRef {
        repository: Some(ROOT.into()),
        dependency: None,
        reference: format!("refs/heads/{id}"),
        commit: format!("commit-{id}"),
    }];
    execution.disk_id = id.into();
    execution.claims.jti = format!("history-{id}");
    execution.boot_started = true;
    execution.runner_hello = true;
    execution.ended = true;
    host.snapshot
        .per_job
        .executions
        .insert(execution.claims.jti.clone(), execution);
    host.snapshot.per_job.disks.insert(id.into(), disk);
}

fn excludes(change: impl FnOnce(&mut Host)) {
    let (runtime, _, _storage, _, _) = setup();
    let mut host = runtime.host.lock().unwrap();
    donor(&mut host, "donor", 1);
    assert!(host.job_fork_plan(JOB, ROOT).unwrap().is_some_and(|plan| {
        host.release_job_fork(&plan);
        plan.request.donor_ids == ["donor"]
    }));
    change(&mut host);
    assert!(host.job_fork_plan(JOB, ROOT).unwrap().is_none());
}

macro_rules! excluded_disk {
    ($name:ident, $disk:ident, $change:expr) => {
        #[test]
        fn $name() {
            excludes(|host| {
                let $disk = host.snapshot.per_job.disks.get_mut("donor").unwrap();
                $change;
            });
        }
    };
}
excluded_disk!(excludes_attached, d, d.attached_job = Some("other".into()));
excluded_disk!(
    excludes_queued_without_attachment,
    d,
    d.queue.push("queued".into())
);
excluded_disk!(
    excludes_evicting,
    d,
    d.storage.as_mut().unwrap().evicting = true
);
excluded_disk!(excludes_missing_storage, d, d.storage = None);
excluded_disk!(
    excludes_larger_cap,
    d,
    d.storage.as_mut().unwrap().cap_bytes += 1
);
excluded_disk!(excludes_other_account, d, d.sub = "other-account".into());
excluded_disk!(
    excludes_other_repository,
    d,
    d.repository_key = Some("other-key".into())
);
excluded_disk!(excludes_missing_repository, d, d.repository_key = None);
excluded_disk!(excludes_quarantined, d, d.quarantined = true);
excluded_disk!(
    excludes_recovering,
    d,
    d.recovery = Some(crate::state::jobs::recovery::Recovery {
        id: "recovery".into(),
        memory_debit_mib: 1,
        guest_ram_mib: 1,
        started_at_ms: Some(1),
        retry_after_ms: None,
        phase: crate::state::jobs::recovery::Phase::Running,
    })
);
excluded_disk!(excludes_deleted, d, d.deleted = true);

#[test]
fn excludes_never_booted() {
    excludes(|h| {
        let e = h
            .snapshot
            .per_job
            .executions
            .get_mut("history-donor")
            .unwrap();
        e.boot_started = false;
        e.runner_hello = false;
    });
}
#[test]
fn excludes_failed_boot() {
    excludes(|h| {
        h.snapshot
            .per_job
            .executions
            .get_mut("history-donor")
            .unwrap()
            .runner_hello = false
    });
}
#[test]
fn excludes_history_not_ended() {
    excludes(|h| {
        h.snapshot
            .per_job
            .executions
            .get_mut("history-donor")
            .unwrap()
            .ended = false
    });
}

#[test]
fn donor_order_and_cap_are_deterministic_and_target_is_excluded() {
    let (runtime, _, _storage, _, _) = setup();
    let mut host = runtime.host.lock().unwrap();
    for i in 0..10 {
        donor(&mut host, &format!("donor-{i}"), i / 2);
    }
    let plan = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
    assert_eq!(
        plan.request.donor_ids,
        ["donor-8", "donor-9", "donor-6", "donor-7", "donor-4", "donor-5", "donor-2", "donor-3"]
    );
    host.release_job_fork(&plan);
}

#[test]
fn no_fork_without_cache_write_or_with_empty_root() {
    for (write, blocked, root) in [
        (None, false, ROOT),
        (Some(false), false, ROOT),
        (Some(true), true, ROOT),
        (Some(true), false, ""),
    ] {
        let (runtime, _, _storage, _, _) = setup();
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "donor", 1);
        host.snapshot
            .per_job
            .executions
            .get_mut(JOB)
            .unwrap()
            .claims
            .cache_write = write;
        host.snapshot.remote_cache_write_blocked = blocked;
        assert!(host.job_fork_plan(JOB, root).unwrap().is_none());
    }
}

#[test]
fn all_listed_donors_hold_queued_jobs_until_commit_or_failure() {
    for commit in [false, true] {
        let (runtime, _, _storage, _, _) = setup();
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "donor", 1);
        donor(&mut host, "second", 2);
        host.snapshot
            .per_job
            .disks
            .get_mut("second")
            .unwrap()
            .workspace_key_hash = motor_lease::base64url(&[7; 32]);
        let plan = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
        let mut second_job = claims(JOB2);
        second_job.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
        host.claim(second_job, unix_ms()).unwrap();
        assert_eq!(host.snapshot.per_job.executions[JOB2].disk_id, "donor");
        assert!(!host.job_boot_disk_ready(JOB2));
        let mut third = claims("00000000-0000-7000-8000-000000000003");
        third.workspace_key_hash = Some(motor_lease::base64url(&[7; 32]));
        let third_id = third.jti.clone();
        host.claim(third, unix_ms()).unwrap();
        assert!(!host.job_boot_disk_ready(&third_id));
        if commit {
            host.job_forked(JOB, &plan, "second").unwrap();
        } else {
            host.release_job_fork(&plan);
        }
        assert!(host.job_boot_disk_ready(JOB2));
        assert!(host.job_boot_disk_ready(&third_id));
    }
}

#[test]
fn overlapping_plans_keep_donors_held_until_the_last_plan_releases() {
    let (runtime, _, _storage, _, _) = setup();
    let mut host = runtime.host.lock().unwrap();
    donor(&mut host, "donor", 1);
    let first = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
    let second = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
    let mut c = claims(JOB2);
    c.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
    host.claim(c, unix_ms()).unwrap();
    host.release_job_fork(&first);
    assert!(!host.job_boot_disk_ready(JOB2));
    host.release_job_fork(&second);
    assert!(host.job_boot_disk_ready(JOB2));
}

#[test]
fn non_first_donor_copies_its_planned_have_and_releases_all_holds() {
    let (runtime, _, _storage, _, _) = setup();
    let mut host = runtime.host.lock().unwrap();
    donor(&mut host, "first", 2);
    donor(&mut host, "second", 1);
    let plan = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
    assert_eq!(plan.request.donor_ids, ["first", "second"]);
    let expected = host.snapshot.per_job.disks["second"].have.clone();
    host.job_forked(JOB, &plan, "second").unwrap();
    let disk = &host.snapshot.per_job.disks[&plan.request.disk_id];
    assert_eq!(disk.have, expected);
    assert_eq!(disk.forked_from.as_deref(), Some("second"));
}

#[test]
fn invalidated_or_changed_then_restored_have_is_never_copied() {
    for restore in [false, true] {
        let (runtime, _, _storage, _, _) = setup();
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "donor", 1);
        let plan = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
        let original = host.snapshot.per_job.disks["donor"].have.clone();
        let mut next = host.snapshot.clone();
        crate::state::jobs::have::invalidate(&mut next, "history-donor");
        host.save(next).unwrap();
        if restore {
            let mut next = host.snapshot.clone();
            next.per_job.disks.get_mut("donor").unwrap().have = original;
            host.save(next).unwrap();
        }
        assert!(host.job_forked(JOB, &plan, "donor").is_err());
        let disk = &host.snapshot.per_job.disks[&plan.request.disk_id];
        assert!(disk.have.is_empty());
        assert!(disk.forked_from.is_none());
    }
}

#[test]
fn unlisted_reply_and_lost_holds_do_not_commit_possession() {
    for restart in [false, true] {
        let (runtime, _, _storage, _, _) = setup();
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "donor", 1);
        let plan = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
        if restart {
            let mut restarted =
                Host::new(BTreeMap::new(), host.snapshot.clone(), Box::new(Journal));
            assert!(restarted.job_forked(JOB, &plan, "donor").is_err());
            host.release_job_fork(&plan);
        } else {
            assert!(host.job_forked(JOB, &plan, "unlisted").is_err());
        }
        assert!(host.snapshot.per_job.disks[&plan.request.disk_id]
            .have
            .is_empty());
        assert!(host.snapshot.per_job.disks[&plan.request.disk_id]
            .forked_from
            .is_none());
    }
}

#[test]
fn target_changes_during_clone_refuse_commit_and_leave_it_cold() {
    for change in ["end", "quarantine", "block_cache_write", "detach_identity"] {
        let (runtime, _, _storage, _, _) = setup();
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "donor", 1);
        let plan = host.job_fork_plan(JOB, ROOT).unwrap().unwrap();
        let mut next = host.snapshot.clone();
        match change {
            "end" => next.per_job.executions.get_mut(JOB).unwrap().ended = true,
            "quarantine" => {
                next.per_job
                    .disks
                    .get_mut(&plan.request.disk_id)
                    .unwrap()
                    .quarantined = true
            }
            "block_cache_write" => next.remote_cache_write_blocked = true,
            _ => {
                next.per_job
                    .disks
                    .get_mut(&plan.request.disk_id)
                    .unwrap()
                    .sub = "other".into()
            }
        }
        host.save(next).unwrap();
        assert!(host.job_forked(JOB, &plan, "donor").is_err(), "{change}");
        let disk = &host.snapshot.per_job.disks[&plan.request.disk_id];
        assert!(disk.have.is_empty());
        assert!(disk.forked_from.is_none());
    }
}

#[test]
fn fork_log_records_selected_position_clone_time_and_commit_outcome() {
    let (runtime, fake, _storage, _, _) = setup();
    let execution = {
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "first", 2);
        donor(&mut host, "second", 1);
        host.snapshot.per_job.executions[JOB].clone()
    };
    *fake.fork_reply.lock().unwrap() = Some("second".into());
    let logs = crate::test_logs::capture(|| seed_workspace(&runtime, &execution, ROOT));
    assert!(
        logs.iter().any(|line| line.contains("workspace_forked")
            && line.contains("second")
            && line.contains("candidate_count=2")
            && line.contains("selected_position=Some(1)")
            && line.contains("clone_elapsed_ms=")
            && line.contains("committed=true")
            && line.contains("size rule applied by the helper")),
        "{logs:?}"
    );
}

#[test]
fn older_disk_state_round_trips_without_new_fields() {
    let old = json!({"sub":"account","workspace_key_hash":"hash","queue":[],"attached_job":null,"quarantined":false});
    let disk: crate::state::jobs::WorkspaceDisk = serde_json::from_value(old.clone()).unwrap();
    assert!(disk.repository_key.is_none());
    assert!(disk.forked_from.is_none());
    assert_eq!(serde_json::to_value(disk).unwrap(), old);
}

#[test]
fn fork_marker_survives_preboot_cancel_and_failed_boot_but_not_a_booted_end() {
    for phase in ["cancel", "failed", "booted"] {
        let (runtime, _, _storage, _, _) = setup();
        let mut host = runtime.host.lock().unwrap();
        let disk_id = host.snapshot.per_job.executions[JOB].disk_id.clone();
        host.snapshot
            .per_job
            .disks
            .get_mut(&disk_id)
            .unwrap()
            .forked_from = Some("donor".into());
        if phase != "cancel" {
            boot_at(&mut host, JOB, unix_ms());
        }
        if phase == "booted" {
            host.job_runner_hello(JOB).unwrap();
        } else if phase == "failed" {
            host.job_boot_failed(JOB, unix_ms()).unwrap();
        }
        host.job_stopped_at(JOB, false, unix_ms()).unwrap();
        assert_eq!(
            host.snapshot.per_job.disks[&disk_id].forked_from.is_some(),
            phase != "booted"
        );
    }
}

async fn run_at_root(client: &mut tokio::io::DuplexStream, root: &str) {
    next_type(client, "lease_ok").await;
    write_frame(client, &hello()).await.unwrap();
    next_type(client, "hello").await;
    let mut request = run(JOB);
    request["repository_root"] = json!(root);
    request["integration_tip"] = json!("");
    write_frame(client, &request).await.unwrap();
    next_type(client, "accepted").await;
    next_type(client, "output").await;
}

#[tokio::test]
async fn helper_forks_unlocked_and_attach_carries_marker_until_booted_job_ends() {
    let (runtime, fake, _storage, c, id) = setup();
    hold_boot(&runtime, &fake, "fork");
    {
        let mut host = runtime.host.lock().unwrap();
        donor(&mut host, "first", 2);
        donor(&mut host, "second", 1);
    }
    *fake.fork_reply.lock().unwrap() = Some("second".into());
    let (mut client, task) = start(runtime.clone(), c.clone(), id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    let mut request = run(JOB);
    request["repository_root"] = json!(ROOT);
    write_frame(&mut client, &request).await.unwrap();
    next_type(&mut client, "accepted").await;
    fake.entered.notified().await;
    let r = runtime.clone();
    let unlocked = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            let mut host = r.host.lock().unwrap();
            let mut queued = claims(JOB2);
            queued.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
            host.claim(queued, unix_ms()).unwrap();
            assert!(!host.job_boot_disk_ready(JOB2));
        }),
    )
    .await;
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    unlocked
        .expect("host lock must be available during fork")
        .unwrap();
    next_type(&mut client, "output").await;
    let disk_id = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    assert_eq!(
        *fake.forks.lock().unwrap(),
        [motor_vm_helper::job::Fork {
            account: c.sub,
            disk_id: disk_id.clone(),
            repository_root: ROOT.into(),
            donor_ids: vec!["first".into(), "second".into()],
            cache_write_job_id: Some(JOB.into()),
        }]
    );
    assert!(fake.seeds.lock().unwrap().is_empty());
    assert_eq!(fake.guest_identity.lock().unwrap()[0]["forked"], true);
    {
        let host = runtime.host.lock().unwrap();
        assert_eq!(
            host.snapshot.per_job.disks[&disk_id].have,
            host.snapshot.per_job.disks["second"].have
        );
        assert!(host.job_boot_disk_ready(JOB2));
    }
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
    assert!(
        runtime.host.lock().unwrap().snapshot.per_job.disks[&disk_id]
            .forked_from
            .is_none()
    );
}

#[tokio::test]
async fn fork_errors_and_unknown_operation_fall_back_without_have_or_quarantine_and_job_runs() {
    for error in ["clone failed", "unknown variant `fork_workspace`"] {
        for use_base in [false, true] {
            let (runtime, fake, _storage, c, id) = setup();
            let disk_id = {
                let mut host = runtime.host.lock().unwrap();
                donor(&mut host, "donor", 1);
                if use_base {
                    let have = host.snapshot.per_job.disks["donor"].have.clone();
                    host.snapshot.per_job.bases.insert(
                        "base".into(),
                        crate::state::jobs::storage::Base {
                            sub: c.sub.clone(),
                            repository_key: Some(crate::jobs::fill_source::repository_key(ROOT)),
                            have,
                            last_used_ms: 1,
                            evicting: false,
                        },
                    );
                }
                host.snapshot.per_job.executions[JOB].disk_id.clone()
            };
            *fake.fork_error.lock().unwrap() = Some(error.into());
            let (mut client, task) = start(runtime.clone(), c, id).await;
            run_at_root(&mut client, ROOT).await;
            assert_eq!(fake.forks.lock().unwrap().len(), 1);
            assert_eq!(
                fake.workspace_contents.lock().unwrap()[&disk_id],
                use_base.then(|| "base".into())
            );
            {
                let host = runtime.host.lock().unwrap();
                let disk = &host.snapshot.per_job.disks[&disk_id];
                assert!(disk.have.is_empty());
                assert!(disk.forked_from.is_none());
                assert!(!disk.quarantined);
                assert!(host.snapshot.leases[JOB].delivered);
            }
            assert_ne!(fake.guest_identity.lock().unwrap()[0]["forked"], true);
            drop(client);
            task.await.unwrap().unwrap();
            runtime.stop_job(JOB, true, false).unwrap();
        }
    }
}

#[tokio::test]
async fn preboot_cancel_preserves_marker_for_next_attach() {
    let (runtime, fake, _storage, c, _) = setup();
    let mut next_claim = c.clone();
    next_claim.jti = JOB2.into();
    let next_id = {
        let mut host = runtime.host.lock().unwrap();
        let disk_id = host.snapshot.per_job.executions[JOB].disk_id.clone();
        host.snapshot
            .per_job
            .disks
            .get_mut(&disk_id)
            .unwrap()
            .forked_from = Some("donor".into());
        host.job_stopped_at(JOB, false, unix_ms()).unwrap();
        host.claim(next_claim.clone(), unix_ms()).unwrap().0
    };
    let (mut client, task) = start(runtime.clone(), next_claim, next_id).await;
    queued(&mut client, JOB2).await;
    next_type(&mut client, "output").await;
    assert_eq!(fake.guest_identity.lock().unwrap()[0]["forked"], true);
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB2, true, false).unwrap();
}

#[tokio::test]
async fn both_run_relay_sites_record_repository_without_tips_and_last_nonempty_root_wins() {
    let (runtime, fake, _storage, c, id) = setup();
    let disk_id = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    let (mut client, task) = start(runtime.clone(), c, id).await;
    run_at_root(&mut client, ROOT).await;
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.disks[&disk_id].repository_key,
        Some(crate::jobs::fill_source::repository_key(ROOT))
    );
    let stream = runtime.job_stream(JOB).unwrap();
    let mut request = run(JOB);
    request["repository_root"] = json!("/home/other");
    request["integration_tip"] = json!("");
    stream.enqueue_for_test(&request).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if runtime.host.lock().unwrap().snapshot.per_job.disks[&disk_id].repository_key
                == Some(crate::jobs::fill_source::repository_key("/home/other"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    request["repository_root"] = json!("");
    stream.enqueue_for_test(&request).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if fake.inputs.lock().unwrap().iter().any(|bytes| {
                let frame = crate::replay::decode(bytes).unwrap();
                frame["type"] == "run" && frame["repository_root"] == ""
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.disks[&disk_id].repository_key,
        Some(crate::jobs::fill_source::repository_key("/home/other"))
    );
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn empty_repository_root_records_nothing_and_never_forks() {
    let (runtime, fake, _storage, c, id) = setup();
    let disk_id = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    let (mut client, task) = start(runtime.clone(), c, id).await;
    run_at_root(&mut client, "").await;
    assert!(
        runtime.host.lock().unwrap().snapshot.per_job.disks[&disk_id]
            .repository_key
            .is_none()
    );
    assert!(fake.forks.lock().unwrap().is_empty());
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}
