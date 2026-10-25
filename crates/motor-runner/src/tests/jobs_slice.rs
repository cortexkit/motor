//! A Linux job unit takes its memory limit from the shared `motor-jobs.slice`:
//! the unit asks for the slice's `MemoryMax` at most, and a slice below the
//! floor every job unit requests refuses the job before start.
//!
//! These run on the plain host backend, with no job isolation, no `motor-job`
//! account, no sudo and no systemd: the decisions are plain functions the
//! isolated path also calls, and the runner fixtures only read the ceiling the
//! runner recorded. The one check that starts a real unit under the account is
//! `isolated_job_unit_memory_max_is_clamped_to_the_jobs_slice`.
use super::*;
use std::sync::atomic::AtomicU64;

const FLOOR: u64 = 16 << 30;

/// A runner that records `ceiling` as the jobs slice's `MemoryMax` without a
/// manager to ask. The ceiling travels through the same cache admission reads
/// on the isolated path: `refresh_slice_budget` at startup and on every
/// maintenance pass.
fn runner_recording(f: &Fixture, ceiling: u64) -> (Runner, Arc<AtomicU64>) {
    let mut config = f.config();
    config.memory_floor_gib = Some(16);
    let current = Arc::new(AtomicU64::new(ceiling));
    let read = current.clone();
    config.slice_ceiling_probe = Some(Arc::new(move || Ok(read.load(Ordering::SeqCst))));
    (Runner::open(config).unwrap(), current)
}

/// A refusal keeps a job record and no execution: its cpus, process and
/// setup time stay empty, the queue stays empty, and the command never ran.
fn assert_refused_without_start(
    runner: &Runner,
    f: &Fixture,
    request: &Run,
    reason: RefusalReason,
) -> String {
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: reason.clone()
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert!(job.cpus.is_empty() && job.process.is_none() && job.setup_ms.is_none());
    assert_eq!(runner.status().queue_depth, 0);
    assert!(!Path::new(&f.key).join("must-not-start").exists());
    terminal.refusal_detail().unwrap_or_default().to_owned()
}

/// The slice decision at the boundary the runner uses: one byte below the
/// floor refuses as a setup fault, at and above it nothing is refused, and a
/// missing reading refuses nothing.
#[test]
fn a_slice_below_the_floor_is_a_setup_refusal_and_a_missing_reading_is_none() {
    let (reason, detail) = crate::backend::slice_admission_refusal(FLOOR, FLOOR - 1).unwrap();
    assert_eq!(reason, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("MemoryMax=17179869183")
            && detail.contains("16 GiB memory floor")
            && detail.contains("jobs_min_bytes"),
        "{detail}"
    );
    assert_eq!(crate::backend::slice_admission_refusal(FLOOR, FLOOR), None);
    assert_eq!(
        crate::backend::slice_admission_refusal(FLOOR, crate::systemd::NO_JOB_MEMORY_BUDGET),
        None
    );
}

/// The limit the unit asks for: a request above the slice is clamped to it,
/// one below keeps its own limit, and with no reading the request stands.
#[test]
fn the_unit_limit_is_clamped_to_the_slice_and_never_above_it() {
    let above = crate::backend::memory_max(30, Some(16));
    assert!(above > FLOOR, "the fixture needs a request above the slice");
    assert_eq!(crate::backend::job_memory_limit(above, FLOOR), FLOOR);
    assert_eq!(
        crate::backend::job_memory_limit(FLOOR - 1, FLOOR),
        FLOOR - 1
    );
    assert_eq!(
        crate::backend::job_memory_limit(above, crate::systemd::NO_JOB_MEMORY_BUDGET),
        above
    );
}

/// Admission refuses the job before start when the recorded ceiling is below
/// the floor: no unit starts, and the record proves nothing ran.
#[test]
fn admission_refuses_before_start_when_the_recorded_slice_is_below_the_floor() {
    let f = Fixture::new();
    let (runner, _) = runner_recording(&f, FLOOR - 1);
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let detail =
        assert_refused_without_start(&runner, &f, &request, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("MemoryMax=17179869183") && detail.contains("jobs_min_bytes"),
        "{detail}"
    );
}

/// A recording that covers the floor admits the job as usual.
#[test]
fn admission_accepts_when_the_recorded_slice_covers_the_floor() {
    let f = Fixture::new();
    let (runner, _) = runner_recording(&f, 24 << 30);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    runner.cancel(request.job_id).unwrap();
}

/// Every maintenance pass re-reads the slice, and admission refuses by the
/// ceiling it records.
#[test]
fn maintenance_records_the_slice_ceiling_that_admission_refuses_by() {
    let f = Fixture::new();
    let (runner, ceiling) = runner_recording(&f, 24 << 30);
    let (request, _) = f.run("touch must-not-start");
    ceiling.store(FLOOR - 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let slow: Vec<_> = runner
        .core
        .inner
        .site_holds()
        .into_iter()
        .filter(|(_, holds)| holds.longest >= crate::state_lock::SLOW_HOLD)
        .collect();
    assert!(
        slow.is_empty(),
        "maintenance held the lock slowly: {slow:?}"
    );
}

/// Admission re-reads the ceiling for each arrival, before it takes the state
/// lock, so a slice that shrank since the last pass refuses the very next job
/// and systemd is never spawned under that lock.
#[test]
fn admission_rereads_the_slice_before_deciding_each_job() {
    let f = Fixture::new();
    let mut config = f.config();
    config.memory_floor_gib = Some(16);
    let ceiling = Arc::new(AtomicU64::new(24 << 30));
    let reads = Arc::new(AtomicU64::new(0));
    let read = ceiling.clone();
    let counted = reads.clone();
    config.slice_ceiling_probe = Some(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(read.load(Ordering::SeqCst))
    }));
    let runner = Runner::open(config).unwrap();
    let first = reads.load(Ordering::SeqCst);
    // A slice shrunk since startup, with no maintenance pass in between.
    ceiling.store(FLOOR - 1, Ordering::SeqCst);
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    assert!(
        reads.load(Ordering::SeqCst) > first,
        "admission decided without reading the slice"
    );
}
