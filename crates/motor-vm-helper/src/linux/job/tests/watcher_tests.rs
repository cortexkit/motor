use super::exit_tests::{empty_group, fixture, peer, read_record};
use super::network_tests::Fake;
use super::*;
use std::sync::{mpsc, Arc};
use std::time::Instant;

fn poll(jobs: &LinuxJobs) -> Vec<ExitReport> {
    match jobs.handle(Operation::Exits, peer()).unwrap().reply {
        Reply::JobExits { exits } => exits,
        reply => panic!("unexpected exit poll: {reply:?}"),
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "watcher did not finish collection"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn finish(jobs: Arc<LinuxJobs>, watcher: std::thread::JoinHandle<()>, base: PathBuf) {
    wait_for(|| {
        jobs.snapshot()
            .iter()
            .all(|(_, cell, _)| !cell.is_collecting())
    });
    drop(jobs);
    watcher.join().unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

fn pause(jobs: &LinuxJobs, step: ExitStep) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(step, move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    (waiting, release)
}

fn add(jobs: &LinuxJobs, record: Record, count: u64, procs: &str) -> PathBuf {
    let group = jobs.group(&record.start.job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), procs).unwrap();
    std::fs::write(group.join("memory.events"), format!("oom_kill {count}\n")).unwrap();
    jobs.save(&record).unwrap();
    jobs.insert_record(record);
    group
}

#[test]
fn watcher_reports_an_empty_running_job_without_an_earlier_request() {
    let base = scratch();
    let jobs = Arc::new(fixture(&base));
    let group = add(&jobs, record(), 7, "4294967295");
    let watcher = jobs.start_exit_watcher();
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let before = Instant::now();
    assert_eq!(
        poll(&jobs),
        [ExitReport {
            job_id: start().job_id,
            oom_kill: 7,
            egress_bytes: None,
        }]
    );
    assert!(before.elapsed() < Duration::from_millis(200));
    assert!(!read_record(&jobs).released);
    finish(jobs, watcher, base);
}

#[test]
fn paused_collector_does_not_delay_another_job_or_hide_its_durable_report() {
    for step in [ExitStep::Captured, ExitStep::Recorded] {
        let base = scratch();
        let jobs = Arc::new(fixture(&base));
        let x = record();
        let mut y = record();
        y.start.job_id = job::test_id(2);
        add(&jobs, x.clone(), 3, "");
        let ygroup = add(&jobs, y.clone(), 4, "4294967295");
        let (waiting, release) = pause(&jobs, step);
        let watcher = jobs.start_exit_watcher();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        std::fs::write(ygroup.join("cgroup.procs"), "").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let before = Instant::now();
        let reports = poll(&jobs);
        let spawns = jobs.hooks.lock().unwrap().collector_spawns;
        // Release before asserting so even a failed latency check cannot leave
        // a collector holding its job lock for the rest of the test process.
        release.send(()).unwrap();
        assert!(before.elapsed() < Duration::from_millis(200));
        assert_eq!(spawns, 2, "a paused collector was spawned more than once");
        let mut expected = vec![ExitReport {
            job_id: y.start.job_id,
            oom_kill: 4,
            egress_bytes: None,
        }];
        if step == ExitStep::Recorded {
            expected.insert(
                0,
                ExitReport {
                    job_id: x.start.job_id,
                    oom_kill: 3,
                    egress_bytes: None,
                },
            );
        }
        assert_eq!(reports, expected);
        wait_for(|| jobs.view(&start().job_id).unwrap().state == State::Exited);
        finish(jobs, watcher, base);
    }
}

#[test]
fn watcher_retries_failed_launch_cleanup_without_releasing_the_disk() {
    let base = scratch();
    let mut jobs = fixture(&base);
    let fake = Fake::default();
    jobs.network = Box::new(fake.clone());
    let intent = start_record(&jobs, start(), peer().uid, Some(17));
    let id = intent.start.job_id.clone();
    let group = empty_group(&jobs, 6);
    jobs.hooks.lock().unwrap().exit_fault = Some(ExitStep::Captured);
    let jobs = Arc::new(jobs);
    {
        let _admission = jobs.admission.lock();
        let error = jobs
            .launch_record(_admission, intent, |_| {
                jobs.network.create_namespace(&id)?;
                Err(Error("launch failed".into()))
            })
            .err()
            .unwrap();
        assert!(error.0.contains("job cleanup failed"), "{error}");
    }
    assert_eq!(jobs.view(&id).unwrap().state, State::Running);
    assert!(group.exists());
    let watcher = jobs.start_exit_watcher();
    wait_for(|| {
        jobs.view(&id).unwrap().state == State::Exited && !jobs.view(&id).unwrap().cleanup_pending
    });
    assert!(!fake.has_namespace(&id));
    assert!(!group.exists());
    assert!(!read_record(&jobs).released);
    assert!(jobs.disk_available(&start(), 0, 0).is_err());
    let result = jobs.handle(Operation::Stop(id), peer());
    if peer().uid == 0 {
        result.unwrap();
        assert!(read_record(&jobs).released);
    } else {
        assert!(result.err().unwrap().0.contains("must be root-owned"));
        assert!(!read_record(&jobs).released);
    }
    finish(jobs, watcher, base);
}

#[test]
fn missing_and_fully_cleaned_jobs_are_not_collected_again() {
    let base = scratch();
    let jobs = Arc::new(fixture(&base));
    jobs.save(&record()).unwrap();
    jobs.insert_record(record());
    let mut cleaned = record();
    cleaned.start.job_id = job::test_id(2);
    let group = add(&jobs, cleaned.clone(), 2, "");
    let mut released = record();
    released.start.job_id = job::test_id(3);
    released.released = true;
    jobs.save(&released).unwrap();
    jobs.insert_record(released.clone());
    let watcher = jobs.start_exit_watcher();
    wait_for(|| {
        jobs.view(&start().job_id).unwrap().state == State::Exited
            && !jobs.view(&cleaned.start.job_id).unwrap().cleanup_pending
            && jobs.view(&cleaned.start.job_id).unwrap().state == State::Exited
    });
    wait_for(|| {
        jobs.snapshot()
            .iter()
            .all(|(_, cell, _)| !cell.is_collecting())
    });
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert_eq!(
        poll(&jobs),
        [ExitReport {
            job_id: cleaned.start.job_id,
            oom_kill: 2,
            egress_bytes: None
        }]
    );
    assert_eq!(jobs.hooks.lock().unwrap().collector_spawns, 2);
    // Terminal views must not probe their old cgroup paths, even if a later
    // filesystem object there would make a probe fail.
    std::fs::write(&group, "not a directory").unwrap();
    std::fs::write(jobs.group(&start().job_id), "not a directory").unwrap();
    std::fs::write(jobs.group(&released.start.job_id), "not a directory").unwrap();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(jobs.hooks.lock().unwrap().collector_spawns, 2);
    finish(jobs, watcher, base);
}

#[test]
fn owned_child_pidfd_triggers_collection_but_is_not_the_empty_group_proof() {
    let base = scratch();
    let jobs = Arc::new(fixture(&base));
    let group = add(&jobs, record(), 8, "4294967295");
    let child = Command::new("/bin/sleep").arg("0.1").spawn().unwrap();
    let pidfd = memory::pin_pidfd(child.id()).unwrap();
    {
        let cell = jobs.cell(&start().job_id).unwrap();
        let mut state = cell.0.lock();
        state.child = Some(child);
        state.pidfd = Some(Arc::new(pidfd));
        jobs.publish(&state);
    }
    let watcher = jobs.start_exit_watcher();
    wait_for(|| jobs.hooks.lock().unwrap().collector_spawns > 0);
    assert!(poll(&jobs).is_empty());
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert!(group.exists());
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    wait_for(|| !poll(&jobs).is_empty());
    assert_eq!(poll(&jobs)[0].oom_kill, 8);
    finish(jobs, watcher, base);
}

#[test]
fn tombstone_pruning_cannot_delay_the_watcher() {
    let base = scratch();
    let jobs = Arc::new(fixture(&base));
    let mut tombstone = record();
    tombstone.released = true;
    tombstone.exit_acknowledged = true;
    tombstone.oom_kill = Some(3);
    jobs.save(&tombstone).unwrap();
    jobs.insert_record(tombstone);
    let path = jobs.records_path.join(format!("{}.json", start().job_id));
    File::open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
    let mut running = record();
    running.start.job_id = job::test_id(2);
    let group = add(&jobs, running.clone(), 9, "4294967295");
    let (waiting, release) = pause(&jobs, ExitStep::Prune);
    let pruning = jobs.clone();
    let pruner = std::thread::spawn(move || pruning.prune_records(SystemTime::now()).unwrap());
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(path.exists());
    assert!(jobs.view(&start().job_id).is_some());
    let watcher = jobs.start_exit_watcher();
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let before = Instant::now();
    let reports = poll(&jobs);
    let elapsed = before.elapsed();
    release.send(()).unwrap();
    pruner.join().unwrap();
    assert!(elapsed < Duration::from_millis(100));
    assert_eq!(
        reports,
        [ExitReport {
            job_id: running.start.job_id,
            oom_kill: 9,
            egress_bytes: None
        }]
    );
    assert!(!path.exists());
    assert!(jobs.view(&start().job_id).is_none());
    finish(jobs, watcher, base);
}
