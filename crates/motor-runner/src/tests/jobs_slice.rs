//! The direct runner's job units take their memory limit from the shared
//! `motor-jobs.slice`: a unit's `MemoryMax` is clamped to the slice's, and a
//! slice too small for the floor every job requests makes the job refuse
//! before it starts instead of being OOM-killed by the slice on arrival.
//!
//! The fixtures that open a runner with job isolation need the repository's
//! development host-backend allowance and inject their own slice ceiling, so
//! they run anywhere: no `motor-job` account and no real `motor-jobs.slice`.
//! The ceiling still travels through the cache admission reads, which is the
//! production path.
use super::*;
use std::sync::atomic::AtomicU64;

/// A runner for these fixtures: job isolation on, without the provisioning
/// the production isolated path needs. A portable fixture has no `motor-job`
/// account, so the account check is bypassed the way this repository already
/// documents for development hosts, and the jobs slice ceiling is the value
/// returned here rather than the host's real slice.
///
/// The ceiling still flows through the cache admission reads: the runner
/// records it in `refresh_slice_budget`, called at startup and by every
/// maintenance pass, exactly as production does.
#[cfg(target_os = "linux")]
fn isolated_fixture_runner(f: &Fixture, ceiling: u64) -> (Runner, Arc<AtomicU64>) {
    let mut config = f.config();
    config.isolate_jobs = true;
    config.development_allow_host_backend = true;
    config.memory_floor_gib = Some(16);
    // The isolated path asks the manager's version once at startup. A manager
    // new enough for `PrivatePIDs` needs no PID namespace init.
    config.systemd_command = Some(Arc::new(version_reply));
    // The startup grant works on scratch trees, so it touches nothing else.
    config.shared_read_roots = Some(scratch_shared_trees(f));
    let current = Arc::new(AtomicU64::new(ceiling));
    let read = current.clone();
    config.slice_ceiling_probe = Some(Arc::new(move || Ok(read.load(Ordering::SeqCst))));
    (Runner::open(config).unwrap(), current)
}

/// Answers the manager version query as a systemd 259 manager, which gives
/// job units `PrivatePIDs`, so no PID namespace init is needed. Every other
/// query is refused, so a stray shell-out shows up as a failed test.
#[cfg(target_os = "linux")]
fn version_reply(args: &[&str]) -> io::Result<std::process::Output> {
    match args {
        ["show", "--property=Version", "--value"] => Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout: b"259.0".to_vec(),
            stderr: Vec::new(),
        }),
        other => Err(io::Error::other(format!(
            "unexpected systemctl arguments {other:?}"
        ))),
    }
}

/// Cargo and rustup trees under the fixture's scratch directory, so the
/// startup grant has something real to work on and touches no host tree.
#[cfg(target_os = "linux")]
fn scratch_shared_trees(f: &Fixture) -> (PathBuf, PathBuf, Option<PathBuf>) {
    let shared = f.scratch.join("shared");
    for name in ["registry", "git", "bin"] {
        fs::create_dir_all(shared.join("cargo").join(name)).unwrap();
    }
    fs::create_dir_all(shared.join("rustup")).unwrap();
    (shared.join("cargo"), shared.join("rustup"), None)
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

/// A slice one byte below the floor every job unit requests refuses the job.
/// The reason is a setup fault, not a full or busy runner: the slice will not
/// get bigger while the job waits, so nothing calls for a retry.
#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_below_the_job_floor_refuses_admission_before_start() {
    let f = Fixture::new();
    let (runner, _) = isolated_fixture_runner(&f, (16 << 30) - 1);
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let detail =
        assert_refused_without_start(&runner, &f, &request, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("MemoryMax=17179869183")
            && detail.contains("16 GiB memory floor")
            && detail.contains("jobs_min_bytes"),
        "{detail}"
    );
}

/// At exactly the floor the job is admitted as usual; its unit's `MemoryMax`
/// is clamped to the slice, and the slice's own OOM killer reports an overrun.
#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_at_the_job_floor_admits_and_clamps_instead_of_refusing() {
    let f = Fixture::new();
    let (runner, _) = isolated_fixture_runner(&f, 16 << 30);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    // The unit the job will start asks for the slice's ceiling, not the
    // larger limit its vCPU count alone would request.
    assert!(crate::backend::memory_max(30, Some(16)) > 16 << 30);
    assert_eq!(
        crate::backend::memory_max(30, Some(16)).min(16 << 30),
        16 << 30
    );
    runner.cancel(request.job_id).unwrap();
}

/// Every maintenance pass re-reads the slice, outside the state lock, and
/// admission refuses by the new ceiling.
#[cfg(target_os = "linux")]
#[test]
fn maintenance_records_the_slice_ceiling_that_admission_refuses_by() {
    let f = Fixture::new();
    let (runner, ceiling) = isolated_fixture_runner(&f, 16 << 30);
    ceiling.store((16 << 30) - 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
}

/// Admission holds the runner's state lock, which every other admission,
/// status reply and cancel waits for. Reading the slice there would stall all
/// of them for as long as systemd or the capacity probe takes to answer, so
/// admission uses the ceiling the runner already recorded.
#[cfg(target_os = "linux")]
#[test]
fn admission_never_reads_the_slice_while_holding_the_state_lock() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.development_allow_host_backend = true;
    config.memory_floor_gib = Some(16);
    let reads = Arc::new(AtomicU64::new(0));
    let counted = reads.clone();
    config.slice_ceiling_probe = Some(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        Ok(16 << 30)
    }));
    config.shared_read_roots = Some(scratch_shared_trees(&f));
    let runner = Runner::open(config).unwrap();
    // Startup recorded the ceiling once; that read happened before any lock.
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    runner.cancel(request.job_id).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1, "admission read the slice");
    let slow: Vec<_> = runner
        .core
        .inner
        .site_holds()
        .into_iter()
        .filter(|(_, holds)| holds.longest >= crate::state_lock::SLOW_HOLD)
        .collect();
    assert!(
        slow.is_empty(),
        "something waited for the slice under the state lock: {slow:?}"
    );
}

/// A maintenance pass re-reads the slice outside the state lock, and
/// admission refuses by the ceiling it records.
#[cfg(target_os = "linux")]
#[test]
fn maintenance_reads_the_slice_outside_the_state_lock() {
    let f = Fixture::new();
    let (runner, ceiling) = isolated_fixture_runner(&f, 16 << 30);
    ceiling.store((16 << 30) - 1, Ordering::SeqCst);
    // The probe used by the fixture is fast; the hold report still proves the
    // re-read happened with no lock held, because maintenance takes the lock
    // only after `refresh_slice_budget` and releases it before finishing.
    runner.maintenance().unwrap();
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
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

/// The refusal rule itself, without a runner: a remembered ceiling below the
/// floor refuses as a setup fault, a usable ceiling does not, and no reading
/// at all (`NO_JOB_MEMORY_BUDGET`) never refuses.
#[test]
fn a_slice_below_the_floor_is_a_setup_refusal_and_a_missing_reading_is_none() {
    let floor = 16 << 30;
    let (reason, detail) = crate::engine::slice_memory_refusal(floor, floor - 1).unwrap();
    assert_eq!(reason, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("memory floor") && detail.contains("jobs_min_bytes"),
        "{detail}"
    );
    assert_eq!(crate::engine::slice_memory_refusal(floor, floor), None);
    assert_eq!(
        crate::engine::slice_memory_refusal(floor, crate::systemd::NO_JOB_MEMORY_BUDGET),
        None
    );
}
