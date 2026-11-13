use super::exit_tests::fixture;
#[cfg(feature = "linux-root-tests")]
use super::exit_tests::peer;
use super::*;
use std::sync::{mpsc, Arc};

fn pause(jobs: &LinuxJobs, step: ExitStep) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(step, move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    (waiting, release)
}

fn fill() -> job::Fill {
    job::Fill {
        account: "account-1".into(),
        fill_id: job::test_id(1),
        repository_key: "repo-1".into(),
        size_bytes: 1 << 20,
    }
}

/// A fill whose VM exited cleanly and whose disk was already released.
fn finished_fill(device: u64, inode: u64) -> Record {
    let mut record = record();
    record.start.kind = job::VmKind::Fill;
    record.start.job_id = fill().fill_id;
    record.start.workspace_disk_id = fill().disk_id();
    record.disk_device = device;
    record.disk_inode = inode;
    record.released = true;
    record.oom_kill = Some(0);
    record.fill_exit_success = Some(true);
    record
}

#[test]
fn late_finish_fill_after_tombstone_pruning_neither_panics_nor_publishes() {
    let base = scratch();
    let jobs = Arc::new(fixture(&base));
    let mut tombstone = finished_fill(8, 99);
    tombstone.exit_acknowledged = true;
    let id = tombstone.start.job_id.clone();
    jobs.save(&tombstone).unwrap();
    File::open(jobs.records_path.join(format!("{id}.json")))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
        .unwrap();
    jobs.adopt_record(tombstone).unwrap();
    let cell = jobs.cell(&id).unwrap();
    // The pruner pauses holding the tombstone's writer lock (J), before it
    // removes anything.
    let (waiting, release) = pause(&jobs, ExitStep::Prune);
    let pruning = jobs.clone();
    let pruner = std::thread::spawn(move || pruning.prune_records(SystemTime::now()));
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    // Registry, this test and the pruner's snapshot each hold the cell.
    let holders = Arc::strong_count(&cell);
    let finishing = jobs.clone();
    let finish = std::thread::spawn(move || finishing.finish_fill(&fill(), true));
    // FinishFill has looked the cell up once it holds another reference; it
    // then waits for J, which the pruner still holds.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while Arc::strong_count(&cell) == holders {
        assert!(
            std::time::Instant::now() < deadline,
            "FinishFill never looked up the cell"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    release.send(()).unwrap();
    pruner.join().unwrap().unwrap();
    let finished = finish
        .join()
        .expect("FinishFill panicked on a retired cell");
    assert!(!finished.unwrap(), "a pruned fill was published");
    // The registry lock is still usable and the pruned id stays retired.
    assert!(jobs.view(&id).is_none());
    assert!(jobs.records().is_empty());
    assert!(!jobs.records_path.join(format!("{id}.json")).exists());
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(feature = "linux-root-tests")]
#[test]
fn isolated_fill_promotion_refuses_a_disk_recreated_after_execution() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = cortexkit_test_support::ScratchDir::new("fill-replacement");
    let jobs = Arc::new(fixture(&base));
    let prepare = || {
        jobs.handle(Operation::PrepareFill(fill()), peer()).unwrap();
        let meta = std::fs::metadata(jobs.workspaces.join(fill().disk_id()).join("workspace.img"))
            .unwrap();
        (meta.dev(), meta.ino())
    };
    let executed = prepare();
    let record = finished_fill(executed.0, executed.1);
    jobs.save(&record).unwrap();
    jobs.adopt_record(record).unwrap();
    // The publishing FinishFill pauses after releasing the cell's writer lock
    // (J) and before taking admission.
    let (waiting, release) = pause(&jobs, ExitStep::FillAdmission);
    let publishing = jobs.clone();
    let publish = std::thread::spawn(move || {
        publishing.handle(
            Operation::FinishFill {
                fill: fill(),
                published: true,
            },
            peer(),
        )
    });
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    // A second FinishFill discards the same fill and a new PrepareFill recreates its disk with
    // identical fill.json.
    let discarded = jobs
        .handle(
            Operation::FinishFill {
                fill: fill(),
                published: false,
            },
            peer(),
        )
        .unwrap();
    assert!(matches!(
        discarded.reply,
        Reply::FillFinished { published: false }
    ));
    let replacement = prepare();
    assert_ne!(replacement, executed, "the replacement reused the inode");
    release.send(()).unwrap();
    let error = publish.join().unwrap().err().expect("replacement promoted");
    assert!(error.0.contains("replaced"), "{error}");
    let (registry, _) =
        workspace::open_disk(&jobs.workspaces, &fill().account, &fill().disk_id()).unwrap();
    assert_eq!(registry.role, job::WorkspaceRole::Fill);
}
