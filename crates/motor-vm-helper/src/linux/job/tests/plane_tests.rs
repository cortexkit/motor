use super::exit_tests::{empty_group, fixture, peer, read_record};
use super::network_tests::Fake;
use super::*;
use std::sync::{mpsc, Arc};

struct Maintenance {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Maintenance {
    fn start(jobs: Arc<LinuxJobs>) -> Self {
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            while stopped.recv_timeout(Duration::from_millis(100))
                == Err(mpsc::RecvTimeoutError::Timeout)
            {
                jobs.maintain().unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

fn pause(jobs: &LinuxJobs, at: ExitStep) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(at, move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    (waiting, release)
}

fn poll(jobs: &LinuxJobs) -> Vec<ExitReport> {
    match jobs.handle(Operation::Exits, peer()).unwrap().reply {
        Reply::JobExits { exits } => exits,
        reply => panic!("unexpected exit poll: {reply:?}"),
    }
}

#[test]
fn exit_poll_reads_only_published_views_without_collecting() {
    let base = scratch();
    let jobs = fixture(&base);
    jobs.save(&record()).unwrap();
    jobs.adopt_record(record()).unwrap();
    let group = empty_group(&jobs, 4);
    assert!(poll(&jobs).is_empty());
    assert_eq!(read_record(&jobs).oom_kill, None);
    assert!(group.exists());
    assert!(jobs.failures(&start().job_id).is_empty());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn paused_stop_does_not_delay_another_jobs_collection_or_exit_poll() {
    let vmm = Vmm::new();
    let jobs = fixture(&vmm.base);
    let x = start_record(&jobs, start(), peer().uid, None);
    jobs.save(&x).unwrap();
    jobs.adopt_record(x.clone()).unwrap();
    empty_group(&jobs, 1);
    let mut y = record();
    y.start.job_id = job::test_id(2);
    let group = jobs.group(&y.start.job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    std::fs::write(group.join("memory.events"), "oom_kill 7\n").unwrap();
    jobs.save(&y).unwrap();
    jobs.adopt_record(y.clone()).unwrap();
    let (waiting, release) = pause(&jobs, ExitStep::Stop);
    let jobs = Arc::new(jobs);
    let stopping = jobs.clone();
    let stop = std::thread::spawn(move || stopping.handle(Operation::Stop(x.start.job_id), peer()));
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let maintenance = Maintenance::start(jobs.clone());
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let (sent, reply) = mpsc::channel();
    let polling = jobs.clone();
    let request = std::thread::spawn(move || sent.send(poll(&polling)).unwrap());
    let result = reply.recv_timeout(Duration::from_millis(200));
    release.send(()).unwrap();
    let _ = stop.join().unwrap();
    request.join().unwrap();
    drop(maintenance);
    assert_eq!(
        result.expect("job_exits waited for an unrelated stop"),
        [ExitReport {
            job_id: y.start.job_id,
            oom_kill: 7,
            egress_bytes: None,
        }]
    );
}

#[test]
fn booting_job_keeps_network_and_adoption_waits_for_launch() {
    for empty in [false, true] {
        let vmm = Vmm::new();
        let mut jobs = fixture(&vmm.base);
        let fake = Fake::default();
        jobs.network = Box::new(fake.clone());
        let mut intent = record();
        intent.tap_index = Some(17);
        let id = intent.start.job_id.clone();
        if empty {
            empty_group(&jobs, 9);
        }
        let (waiting, release) = pause(&jobs, ExitStep::Launch);
        let jobs = Arc::new(jobs);
        let starting = jobs.clone();
        let pid = vmm.child.id();
        let launch_thread = std::thread::spawn(move || {
            let _admission = starting.admission.lock();
            starting.launch_record(intent, |_| {
                starting.network.create_namespace(&id)?;
                starting.exit_checkpoint(ExitStep::Launch)?;
                let group = starting.group(&id);
                std::fs::create_dir_all(&group)?;
                std::fs::write(group.join("cgroup.procs"), pid.to_string())?;
                Ok(())
            })
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        let maintenance = Maintenance::start(jobs.clone());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(jobs.view(&start().job_id).unwrap().state, State::Booting);
        assert_eq!(read_record(&jobs).oom_kill, None);
        assert_eq!(read_record(&jobs).tap_index, Some(17));
        assert!(fake.has_namespace(&start().job_id));
        assert!(poll(&jobs).is_empty());
        let (begun, begins) = mpsc::channel();
        jobs.pause_exit_at(ExitStep::Adopt, move || begun.send(()).unwrap());
        let (adopted, adoption) = mpsc::channel();
        let adopting = jobs.clone();
        let adopt = std::thread::spawn(move || {
            adopted
                .send(adopting.handle(Operation::Adopt(start().job_id), peer()))
                .unwrap()
        });
        begins.recv_timeout(Duration::from_secs(2)).unwrap();
        let early = adoption.recv_timeout(Duration::from_millis(100));
        let (sent, refused) = mpsc::channel();
        let probing = jobs.clone();
        let probes = std::thread::spawn(move || {
            let errors: Vec<_> = [
                Operation::Relay(start().job_id, 53),
                Operation::Memory(start().job_id),
                Operation::Balloon(start().job_id, 0),
            ]
            .into_iter()
            .map(|op| probing.handle(op, peer()).err().map(|error| error.0))
            .collect();
            sent.send(errors).unwrap();
        });
        let errors = refused.recv_timeout(Duration::from_millis(200));
        release.send(()).unwrap();
        assert!(matches!(
            launch_thread.join().unwrap().unwrap().reply,
            Reply::Job { running: true, .. }
        ));
        adopt.join().unwrap();
        probes.join().unwrap();
        drop(maintenance);
        assert!(
            matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
            "adopt answered during launch"
        );
        assert!(matches!(
            adoption
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .reply,
            Reply::Job {
                running: true,
                exit_report: None,
                ..
            }
        ));
        assert_eq!(
            errors.expect("running-only operations waited for launch"),
            vec![Some("job VM is not running".to_owned()); 3]
        );
        assert_eq!(read_record(&jobs).oom_kill, None);
        assert!(fake.has_namespace(&start().job_id));
    }
}

#[test]
fn stop_publishes_exit_before_disk_release_even_if_ownership_release_fails() {
    let base = scratch();
    let jobs = fixture(&base);
    let record = start_record(&jobs, start(), peer().uid, None);
    jobs.save(&record).unwrap();
    jobs.adopt_record(record).unwrap();
    empty_group(&jobs, 4);
    let result = jobs.handle(Operation::Stop(start().job_id), peer());
    if peer().uid == 0 {
        assert!(matches!(
            result.unwrap().reply,
            Reply::Job { running: false, .. }
        ));
        assert!(read_record(&jobs).released);
    } else {
        let error = result
            .err()
            .expect("non-root fixture unexpectedly passed disk ownership checks");
        assert!(error.0.contains("must be root-owned"), "{error}");
        assert!(!read_record(&jobs).released);
        assert!(
            jobs.disk_available(&start(), 0, 0).is_err(),
            "failed release lost the disk reservation"
        );
    }
    assert_eq!(
        poll(&jobs),
        [ExitReport {
            job_id: start().job_id,
            oom_kill: 4,
            egress_bytes: None
        }]
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "lock order violation")]
fn admission_while_holding_job_lock_panics() {
    let base = cortexkit_test_support::ScratchDir::new("job-lock-order");
    let jobs = fixture(&base);
    jobs.insert_record(record());
    let cell = jobs.cell(&start().job_id).unwrap();
    let _job = cell.0.lock();
    let _admission = jobs.admission.lock();
}
