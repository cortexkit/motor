use super::*;

fn peer() -> Peer {
    Peer {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    }
}

fn fixture(base: &Path) -> LinuxJobs {
    let mut jobs = machine(base);
    jobs.settings = Some(JobVms {
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
        uid_base: unsafe { libc::geteuid() },
        uid_count: 1,
        memory_max_bytes: 1048576,
    });
    std::fs::create_dir_all(&jobs.records_path).unwrap();
    std::fs::set_permissions(&jobs.records_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    jobs
}

fn record_path(jobs: &LinuxJobs) -> PathBuf {
    jobs.records_path.join(format!("{}.json", start().job_id))
}

fn read_record(jobs: &LinuxJobs) -> Record {
    serde_json::from_slice(&std::fs::read(record_path(jobs)).unwrap()).unwrap()
}

fn empty_group(jobs: &LinuxJobs, count: u64) -> PathBuf {
    let group = jobs.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::fs::write(
        group.join("memory.events"),
        format!("oom 9\noom_kill {count}\n"),
    )
    .unwrap();
    group
}

fn restart(base: &Path) -> LinuxJobs {
    let mut jobs = fixture(base);
    // Exercise the persisted record and the production adoption checks without
    // requiring root ownership of the portable test's scratch ancestors.
    jobs.adopt_record(read_record(&jobs)).unwrap();
    jobs
}

fn reports(jobs: &mut LinuxJobs) -> Vec<ExitReport> {
    match jobs.handle(Operation::Exits, peer()).unwrap().reply {
        Reply::JobExits { exits } => exits,
        reply => panic!("unexpected exit poll: {reply:?}"),
    }
}

fn expected(count: u64) -> ExitReport {
    ExitReport {
        job_id: start().job_id,
        oom_kill: count,
        egress_bytes: None,
    }
}

fn fault_replays_one_report(step: ExitStep) {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    let group = empty_group(&jobs, 3);
    jobs.exit_fault = Some(step);
    let error = if step == ExitStep::Delivery {
        jobs.handle(Operation::Exits, peer()).err().unwrap()
    } else {
        jobs.collect_exit(&start().job_id).unwrap_err()
    };
    assert!(error.0.contains(&format!("{step:?}")), "{error}");
    assert!(!read_record(&jobs).released);
    assert_eq!(
        read_record(&jobs).oom_kill,
        if step == ExitStep::Captured {
            None
        } else {
            Some(3)
        }
    );
    assert_eq!(
        group.exists(),
        matches!(step, ExitStep::Captured | ExitStep::Recorded)
    );
    if step == ExitStep::Recorded {
        // Once captured durably, retries must not replace the final count.
        std::fs::write(group.join("memory.events"), "oom_kill 99\n").unwrap();
    }
    drop(jobs);
    let mut jobs = restart(&base);
    assert_eq!(reports(&mut jobs), [expected(3)]);
    assert!(!group.exists());
    assert_eq!(reports(&mut jobs), [expected(3)]);
    let bytes = std::fs::read(record_path(&jobs)).unwrap();
    let metadata = std::fs::metadata(record_path(&jobs)).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert!(jobs.acknowledge_exit(expected(4)).is_err());
    let mut unknown = expected(3);
    unknown.job_id = job::test_id(2);
    assert!(jobs.acknowledge_exit(unknown).is_err());
    assert_eq!(std::fs::read(record_path(&jobs)).unwrap(), bytes);
    assert_eq!(reports(&mut jobs), [expected(3)]);
    assert!(matches!(
        jobs.acknowledge_exit(expected(3)).unwrap().reply,
        Reply::Ok
    ));
    assert!(matches!(
        jobs.acknowledge_exit(expected(3)).unwrap().reply,
        Reply::Ok
    ));
    assert!(reports(&mut jobs).is_empty());
    drop(jobs);
    let mut jobs = restart(&base);
    assert!(reports(&mut jobs).is_empty());
    assert!(jobs
        .start(start(), peer())
        .err()
        .unwrap()
        .0
        .contains("already booted"));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn fault_at_count_capture_replays_one_immutable_exit_report() {
    fault_replays_one_report(ExitStep::Captured);
}

#[test]
fn fault_after_durable_write_replays_one_immutable_exit_report() {
    fault_replays_one_report(ExitStep::Recorded);
}

#[test]
fn fault_after_cgroup_removal_replays_one_immutable_exit_report() {
    fault_replays_one_report(ExitStep::Removed);
}

#[test]
fn fault_at_delivery_replays_one_immutable_exit_report() {
    fault_replays_one_report(ExitStep::Delivery);
}

#[test]
fn failed_durable_write_keeps_cgroup_evidence_and_disk_reservation() {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    let group = empty_group(&jobs, 5);
    let records = jobs.records_path.clone();
    jobs.records_path = base.join("missing-directory");
    jobs.collect_exits().unwrap();
    assert!(!jobs.collection_failures[&start().job_id].is_empty());
    assert!(group.exists());
    assert_eq!(jobs.records[&start().job_id].oom_kill, None);
    assert!(jobs.disk_available(&start(), 8, 99).is_err());
    jobs.records_path = records;
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert_eq!(reports(&mut jobs), [expected(5)]);
    assert!(!read_record(&jobs).released);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn failed_cgroup_removal_keeps_durable_count_for_restart() {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    let group = empty_group(&jobs, 6);
    jobs.remove_test_group =
        Some(|_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
    jobs.collect_exits().unwrap();
    assert!(!jobs.collection_failures[&start().job_id].is_empty());
    assert_eq!(read_record(&jobs).oom_kill, Some(6));
    assert!(group.exists());
    drop(jobs);
    let mut jobs = restart(&base);
    assert_eq!(reports(&mut jobs), [expected(6)]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn failed_ack_write_replays_until_durable_ack_and_survives_lost_ack_reply() {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    empty_group(&jobs, 0);
    assert_eq!(reports(&mut jobs), [expected(0)]);
    let records = jobs.records_path.clone();
    jobs.records_path = base.join("missing-directory");
    assert!(jobs
        .handle(Operation::AckExit(expected(0)), peer())
        .is_err());
    assert!(!jobs.records[&start().job_id].exit_acknowledged);
    drop(jobs);
    let mut jobs = restart(&base);
    assert_eq!(jobs.records_path, records);
    assert_eq!(reports(&mut jobs), [expected(0)]);
    jobs.handle(Operation::AckExit(expected(0)), peer())
        .unwrap();
    // After losing the reply, retry the acknowledgement on another helper.
    drop(jobs);
    let mut jobs = restart(&base);
    assert!(reports(&mut jobs).is_empty());
    assert!(matches!(
        jobs.handle(Operation::AckExit(expected(0)), peer())
            .unwrap()
            .reply,
        Reply::Ok
    ));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn restart_adopts_running_job_and_captures_its_exit_while_helper_is_down() {
    let mut vmm = Vmm::new();
    let mut jobs = fixture(&vmm.base);
    jobs.save(&record()).unwrap();
    let group = empty_group(&jobs, 7);
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    jobs.adopt_record(record()).unwrap();
    assert!(reports(&mut jobs).is_empty());
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert!(group.exists());
    drop(jobs);
    let mut jobs = restart(&vmm.base);
    assert!(matches!(
        jobs.status(&start().job_id).unwrap().reply,
        Reply::Job {
            running: true,
            exit_report: None,
            ..
        }
    ));
    assert!(reports(&mut jobs).is_empty());
    drop(jobs);
    vmm.child.kill().unwrap();
    vmm.child.wait().unwrap();
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    let mut jobs = restart(&vmm.base);
    // Maintenance, independently of host connections, captures the exit.
    jobs.collect_exits().unwrap();
    assert_eq!(read_record(&jobs).oom_kill, Some(7));
    assert_eq!(reports(&mut jobs), [expected(7)]);
    assert!(matches!(jobs.status(&start().job_id).unwrap().reply,
        Reply::Job { running: false, exit_report: Some(report), .. } if report == expected(7)));
}

#[test]
fn absent_or_nonempty_cgroup_never_invents_an_exit_count() {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    assert!(reports(&mut jobs).is_empty());
    assert_eq!(read_record(&jobs).oom_kill, None);
    let group = empty_group(&jobs, 8);
    // Even a process that cannot be found in /proc is not proof of an empty
    // cgroup: the cgroup's own membership must confirm that it has exited.
    std::fs::write(group.join("cgroup.procs"), "4294967295").unwrap();
    assert!(reports(&mut jobs).is_empty());
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert!(group.exists());
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::fs::write(group.join("memory.events"), "oom 8\n").unwrap();
    jobs.collect_exits().unwrap();
    assert!(!jobs.collection_failures[&start().job_id].is_empty());
    assert!(group.exists());
    assert_eq!(read_record(&jobs).oom_kill, None);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn job_records_default_acknowledgement_and_accept_newer_fields() {
    let mut value = serde_json::to_value(record()).unwrap();
    value.as_object_mut().unwrap().remove("exit_acknowledged");
    value["newer_helper_field"] = json!({"future":true});
    let record: Record = serde_json::from_value(value).unwrap();
    assert!(!record.exit_acknowledged);
    assert_eq!(record.oom_kill, None);
    assert!(serde_json::from_value::<Start>(json!({
        "job_id":start().job_id, "account":"a", "workspace_disk_id":"d",
        "image_generation_id":"g", "vcpus":1, "memory_mib":1, "extra":true
    }))
    .is_err());
}

#[test]
fn socket_delivery_failure_replays_after_restart() {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    empty_group(&jobs, 2);
    let response = jobs.handle(Operation::Exits, peer()).unwrap();
    let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(reader);
    assert!(crate::linux::server::send_response(&mut writer, response).is_err());
    drop(jobs);
    let mut jobs = restart(&base);
    assert_eq!(reports(&mut jobs), [expected(2)]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn concurrent_job_exits_keep_their_own_counts() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let first = record();
    let mut second = first.clone();
    second.start.job_id = job::test_id(2);
    for record in [&first, &second] {
        jobs.save(record).unwrap();
        jobs.adopt_record(record.clone()).unwrap();
    }
    empty_group(&jobs, 11);
    let group = jobs.group(&second.start.job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::fs::write(group.join("memory.events"), "oom_kill 0\n").unwrap();
    assert_eq!(
        reports(&mut jobs),
        [
            expected(11),
            ExitReport {
                job_id: second.start.job_id.clone(),
                oom_kill: 0,
                egress_bytes: None,
            }
        ]
    );
    jobs.acknowledge_exit(expected(11)).unwrap();
    assert_eq!(
        reports(&mut jobs),
        [ExitReport {
            job_id: second.start.job_id,
            oom_kill: 0,
            egress_bytes: None,
        }]
    );
    std::fs::remove_dir_all(base).unwrap();
}

fn tombstone() -> Record {
    Record {
        released: true,
        oom_kill: Some(3),
        exit_acknowledged: true,
        ..record()
    }
}

fn save_with_mtime(jobs: &mut LinuxJobs, record: Record, modified: SystemTime) -> PathBuf {
    jobs.save(&record).unwrap();
    let path = jobs
        .records_path
        .join(format!("{}.json", record.start.job_id));
    File::open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    jobs.adopt_record(record).unwrap();
    path
}

#[test]
fn completed_tombstone_does_not_probe_its_cgroup() {
    let base = scratch();
    let mut jobs = fixture(&base);
    jobs.save(&tombstone()).unwrap();
    jobs.adopt_record(tombstone()).unwrap();
    std::fs::create_dir_all(&jobs.groups).unwrap();
    // A probe below this file would fail, even for root. Completed tombstones
    // must not perform any cgroup IO during exit collection or requests.
    std::fs::write(jobs.group(&start().job_id), "not a directory").unwrap();
    jobs.collect_exits().unwrap();
    assert!(reports(&mut jobs).is_empty());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn younger_than_48_hour_tombstone_remains_and_refuses_replayed_start() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let path = save_with_mtime(&mut jobs, tombstone(), modified);
    jobs.maintain_at(modified + Duration::from_secs(48 * 60 * 60 - 1))
        .unwrap();
    assert!(path.exists());
    assert!(jobs.records.contains_key(&start().job_id));
    let error = jobs
        .handle(Operation::Start(start()), peer())
        .err()
        .unwrap();
    assert!(error.0.contains("already booted"), "{error}");
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn expired_tombstone_is_pruned_only_by_maintenance_not_requests() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let path = save_with_mtime(&mut jobs, tombstone(), modified);
    assert!(reports(&mut jobs).is_empty());
    assert!(path.exists());
    assert!(jobs
        .handle(Operation::Start(start()), peer())
        .err()
        .unwrap()
        .0
        .contains("already booted"));
    jobs.maintain_at(modified + Duration::from_secs(48 * 60 * 60 + 1))
        .unwrap();
    assert!(!path.exists());
    assert!(!jobs.records.contains_key(&start().job_id));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn unacknowledged_or_unreleased_records_are_never_pruned_regardless_of_age() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let mut unacknowledged = tombstone();
    unacknowledged.exit_acknowledged = false;
    let first = save_with_mtime(&mut jobs, unacknowledged, modified);
    let mut unreleased = tombstone();
    unreleased.start.job_id = job::test_id(2);
    unreleased.released = false;
    let second = save_with_mtime(&mut jobs, unreleased, modified);
    jobs.maintain_at(modified + Duration::from_secs(365 * 24 * 60 * 60))
        .unwrap();
    assert!(first.exists());
    assert!(second.exists());
    assert_eq!(jobs.records.len(), 2);
    assert_eq!(reports(&mut jobs), [expected(3)]);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn prune_waits_for_full_48_hours_and_keeps_future_timestamps() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let path = save_with_mtime(&mut jobs, tombstone(), modified);
    jobs.prune_records(modified - Duration::from_secs(1))
        .unwrap();
    assert!(path.exists());
    jobs.prune_records(modified + Duration::from_secs(48 * 60 * 60 - 1))
        .unwrap();
    assert!(path.exists());
    jobs.prune_records(modified + Duration::from_secs(48 * 60 * 60))
        .unwrap();
    assert!(!path.exists());
    assert!(jobs.records.is_empty());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn frequent_maintenance_ticks_prune_at_most_once_per_minute() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let path = save_with_mtime(&mut jobs, tombstone(), modified);
    let first_tick = modified + Duration::from_secs(48 * 60 * 60 - 10);
    jobs.maintain_at(first_tick).unwrap();
    assert!(path.exists());
    jobs.maintain_at(first_tick + Duration::from_secs(59))
        .unwrap();
    assert!(path.exists());
    jobs.maintain_at(first_tick + Duration::from_secs(60))
        .unwrap();
    assert!(!path.exists());
    std::fs::remove_dir_all(base).unwrap();
}
