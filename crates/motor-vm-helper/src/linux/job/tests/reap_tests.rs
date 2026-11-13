use super::exit_tests::{empty_group, fixture, peer, read_record};
use super::*;
use std::sync::{mpsc, Arc};

/// A fixture VMM that runs until the test closes its stdin, then exits 0.
fn owned_child(jobs: &LinuxJobs, id: &str) -> std::process::ChildStdin {
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let pidfd = memory::pin_pidfd(child.id()).unwrap();
    let cell = jobs.cell(id).unwrap();
    let mut state = cell.0.lock();
    state.child = Some(child);
    state.pidfd = Some(Arc::new(pidfd));
    jobs.publish(&state);
    stdin
}

fn fill_fixture(base: &Path) -> (LinuxJobs, String, PathBuf) {
    let jobs = fixture(base);
    let mut record = record();
    record.start.kind = job::VmKind::Fill;
    let id = record.start.job_id.clone();
    jobs.save(&record).unwrap();
    jobs.insert_record(record);
    // The kernel has already taken the VMM out of cgroup.procs.
    let group = empty_group(&jobs, 0);
    (jobs, id, group)
}

#[test]
fn vmm_that_left_its_cgroup_is_reaped_before_evidence_and_keeps_fill_success() {
    let base = scratch();
    let (jobs, id, group) = fill_fixture(&base);
    let mut stdin = Some(owned_child(&jobs, &id));
    // The VMM finishes exiting only while collection waits for it, as one does
    // between leaving its cgroup and becoming a reapable zombie.
    jobs.pause_exit_at(ExitStep::Reap, move || drop(stdin.take()));
    jobs.collect_exit(&id).unwrap();
    {
        let cell = jobs.cell(&id).unwrap();
        let state = cell.0.lock();
        assert!(state.child.is_none(), "collection left the VMM unreaped");
        assert!(state.pidfd.is_none());
    }
    assert!(jobs.view(&id).unwrap().pidfd.is_none());
    let durable = read_record(&jobs);
    assert_eq!(durable.fill_exit_success, Some(true), "clean fill lost");
    assert_eq!(durable.oom_kill, Some(0));
    assert!(!group.exists());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn unreaped_vmm_keeps_exit_evidence_unpublished_until_a_later_collection() {
    let base = scratch();
    let (jobs, id, group) = fill_fixture(&base);
    let stdin = owned_child(&jobs, &id);
    jobs.hooks.lock().unwrap().child_wait = Some(Duration::from_millis(100));
    let error = jobs.collect_exit(&id).unwrap_err();
    assert!(error.0.contains("not reaped"), "{error}");
    assert!(jobs.failures(&id).contains(&CollectionFailure::Reap));
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert_eq!(jobs.view(&id).unwrap().state, State::Running);
    assert!(jobs.records()[&id].pending_exit().is_none());
    assert!(group.exists());
    drop(stdin);
    jobs.hooks.lock().unwrap().child_wait = None;
    jobs.collect_exit(&id).unwrap();
    let durable = read_record(&jobs);
    assert_eq!(durable.oom_kill, Some(0));
    assert_eq!(durable.fill_exit_success, Some(true));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn finished_jobs_do_not_keep_their_pidfds_until_pruning() {
    fn open_fds() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }
    let base = scratch();
    let jobs = fixture(&base);
    let cycle = |n: u64| {
        let mut start = start();
        start.job_id = job::test_id(n);
        start.workspace_disk_id = format!("disk-{n}");
        let id = start.job_id.clone();
        let intent = start_record(&jobs, start, peer().uid, None);
        let group = jobs.group(&id);
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(group.join("cgroup.procs"), "").unwrap();
        std::fs::write(group.join("memory.events"), "oom_kill 0\n").unwrap();
        let admission = jobs.admission.lock();
        jobs.start_available(&intent.start, intent.disk_device, intent.disk_inode)
            .unwrap();
        jobs.launch_record(admission, intent, |state| {
            let child = Command::new("/bin/true").spawn()?;
            state.pidfd = Some(Arc::new(memory::pin_pidfd(child.id())?));
            state.child = Some(child);
            Ok(())
        })
        .unwrap();
        let stopped = jobs.handle(Operation::Stop(id.clone()), peer());
        if peer().uid == 0 {
            stopped.unwrap();
        } else {
            // Release needs a root-owned jail; collection has finished by then.
            assert!(stopped.err().unwrap().0.contains("must be root-owned"));
        }
        jobs.handle(
            Operation::AckExit(ExitReport {
                job_id: id.clone(),
                oom_kill: 0,
                egress_bytes: None,
            }),
            peer(),
        )
        .unwrap();
        assert!(jobs.view(&id).unwrap().pidfd.is_none());
    };
    // The first cycle opens anything the process creates lazily.
    cycle(1);
    let baseline = open_fds();
    for n in 2..=33 {
        cycle(n);
    }
    assert_eq!(open_fds(), baseline, "finished jobs kept descriptors open");
    assert_eq!(jobs.records().len(), 33);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn stop_wait_for_a_killed_vmm_is_bounded_and_releases_nothing() {
    let base = scratch();
    let jobs = Arc::new(fixture(&base));
    let record = start_record(&jobs, start(), peer().uid, None);
    let id = record.start.job_id.clone();
    jobs.save(&record).unwrap();
    jobs.insert_record(record);
    empty_group(&jobs, 4);
    // Fixture cgroups cannot kill, so this child models a VMM stuck in the
    // kernel after cgroup.kill.
    let stdin = owned_child(&jobs, &id);
    jobs.hooks.lock().unwrap().child_wait = Some(Duration::from_millis(200));
    let stopping = jobs.clone();
    let stop_id = id.clone();
    let (sent, stopped) = mpsc::channel();
    let stop = std::thread::spawn(move || {
        sent.send(stopping.handle(Operation::Stop(stop_id), peer()))
            .unwrap();
    });
    let result = stopped.recv_timeout(Duration::from_secs(5));
    // Close stdin before asserting so even a failure cannot leave the stop
    // thread waiting for this child.
    drop(stdin);
    stop.join().unwrap();
    let error = result
        .expect("stop waited without a bound for the VMM")
        .err()
        .unwrap();
    assert!(error.0.contains("did not exit within"), "{error}");
    let durable = read_record(&jobs);
    assert!(!durable.released);
    assert_eq!(durable.oom_kill, None);
    assert!(jobs.records()[&id].pending_exit().is_none());
    assert!(jobs.cell(&id).unwrap().0.lock().child.is_some());
    jobs.hooks.lock().unwrap().child_wait = None;
    let retried = jobs.handle(Operation::Stop(id.clone()), peer());
    if peer().uid == 0 {
        retried.unwrap();
        assert!(read_record(&jobs).released);
    } else {
        assert!(retried.err().unwrap().0.contains("must be root-owned"));
    }
    assert_eq!(read_record(&jobs).oom_kill, Some(4));
    assert!(jobs.cell(&id).unwrap().0.lock().child.is_none());
    std::fs::remove_dir_all(base).unwrap();
}
