//! A Linux job unit takes its memory limit from the shared `motor-jobs.slice`:
//! the unit asks for the slice's `MemoryMax` at most. Only capacity-managed
//! hosts promise the per-job floor through an operator-declared minimum.
//!
//! These run on the plain host backend, with no job isolation, no `motor-job`
//! account, no sudo and no systemd: the decisions are plain functions the
//! isolated path also calls, and the runner fixtures only read the ceiling the
//! runner recorded. The one check that starts a real unit under the account is
//! `isolated_job_unit_memory_max_is_clamped_to_the_jobs_slice`.
use super::*;
use std::sync::atomic::AtomicU64;

const FLOOR: u64 = 16 << 30;

/// Replace systemd with a recorded ceiling and read a fixture capacity file
/// through the production parser. Both values travel through admission's cache.
fn runner_recording(
    f: &Fixture,
    ceiling: u64,
    declared_min: Option<u64>,
) -> (Runner, Arc<AtomicU64>) {
    let mut config = f.config();
    config.memory_floor_gib = Some(16);
    config.manual_tick = true;
    let capacity_file = f.scratch.join("capacity.json");
    if let Some(min) = declared_min {
        fs::write(&capacity_file, format!(r#"{{"jobs_min_bytes":{min}}}"#)).unwrap();
    }
    let current = Arc::new(AtomicU64::new(ceiling));
    let read = current.clone();
    config.slice_budget_probe = Some(Arc::new(move || {
        Ok((
            read.load(Ordering::SeqCst),
            crate::job_memory_budget::read_declared_jobs_min(&capacity_file),
        ))
    }));
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

/// A declaration below the floor refuses even when the combined slice is
/// larger. A missing declaration leaves small hosts free to clamp their jobs.
#[test]
fn a_declared_budget_below_the_floor_is_a_setup_refusal_and_no_declaration_is_none() {
    let (reason, detail) =
        crate::backend::slice_admission_refusal(FLOOR, 40 << 30, Some(FLOOR - 1)).unwrap();
    assert_eq!(reason, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("jobs_min_bytes=17179869183")
            && detail.contains("16 GiB memory floor")
            && detail.contains("jobs_min_bytes"),
        "{detail}"
    );
    assert_eq!(
        crate::backend::slice_admission_refusal(FLOOR, FLOOR, Some(FLOOR)),
        None
    );
    assert_eq!(
        crate::backend::slice_admission_refusal(FLOOR, FLOOR - 1, None),
        None
    );
    assert_eq!(
        crate::backend::slice_admission_refusal(
            FLOOR,
            crate::systemd::NO_JOB_MEMORY_BUDGET,
            Some(FLOOR)
        ),
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

/// The Linux minimum excludes the Windows allowance in the combined slice.
/// An operator declaration below the floor refuses without starting any work.
#[test]
fn admission_refuses_before_start_when_the_declared_minimum_is_below_the_floor() {
    let f = Fixture::new();
    let (runner, _) = runner_recording(&f, 40 << 30, Some(FLOOR - 1));
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let detail =
        assert_refused_without_start(&runner, &f, &request, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("jobs_min_bytes=17179869183")
            && detail.contains("/etc/motor-host/capacity.json")
            && detail.contains("provision.py apply-jobs-slice"),
        "{detail}"
    );
}

/// The 40 GiB slice ceiling and the operator-declared 24 GiB Linux minimum
/// together cover the 16 GiB per-job floor.
#[test]
fn admission_accepts_when_the_declared_minimum_and_slice_cover_the_floor() {
    let f = Fixture::new();
    let (runner, _) = runner_recording(&f, 40 << 30, Some(24 << 30));
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    runner.cancel(request.job_id).unwrap();
}

#[test]
fn an_undeclared_boat_sized_slice_admits_eight_vcpus_and_clamps_the_unit_limit() {
    let f = Fixture::new();
    let slice = 14_249_590_784;
    let (runner, _) = runner_recording(&f, slice, None);
    let (mut request, _) = f.run("true");
    request.weight_hint = Some(8);
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    assert_eq!(
        crate::backend::job_memory_limit(crate::backend::memory_max(8, Some(16)), slice),
        14_249_590_784
    );
    runner.cancel(request.job_id).unwrap();
}

/// A live slice too small to honour the declaration remains a setup fault.
/// Maintenance refreshes both inputs without holding the state lock; admission
/// reads the cache without performing either systemctl or capacity-file I/O.
#[test]
fn maintenance_picks_up_a_changed_ceiling_before_the_next_job_is_decided() {
    let f = Fixture::new();
    let mut config = f.config();
    config.memory_floor_gib = Some(16);
    config.manual_tick = true;
    let ceiling = Arc::new(AtomicU64::new(24 << 30));
    let capacity_file = f.scratch.join("capacity.json");
    fs::write(&capacity_file, r#"{"jobs_min_bytes":25769803776}"#).unwrap();
    let reads = Arc::new(AtomicU64::new(0));
    let core = Arc::new(std::sync::OnceLock::<std::sync::Weak<Core>>::new());
    let probed_core = core.clone();
    let read = ceiling.clone();
    let counted = reads.clone();
    config.slice_budget_probe = Some(Arc::new(move || {
        // Startup probes before the runner is available. On maintenance, test
        // the lock directly instead of timing cumulative holds from unrelated
        // operations. Manual ticking leaves no other state-lock contender.
        if let Some(core) = probed_core.get().and_then(std::sync::Weak::upgrade) {
            assert!(
                core.inner.try_lock().is_ok(),
                "slice ceiling probe ran with the state lock held"
            );
        }
        counted.fetch_add(1, Ordering::SeqCst);
        Ok((
            read.load(Ordering::SeqCst),
            crate::job_memory_budget::read_declared_jobs_min(&capacity_file),
        ))
    }));
    let runner = Runner::open(config).unwrap();
    core.set(Arc::downgrade(&runner.core)).unwrap();
    // Admission consumes the startup reading without probing the host again.
    let startup_reads = reads.load(Ordering::SeqCst);
    let (before, _) = f.run("true");
    assert!(matches!(
        runner.accept(before.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    assert_eq!(
        reads.load(Ordering::SeqCst),
        startup_reads,
        "admission probed the host"
    );
    runner.cancel(before.job_id).unwrap();
    // An operator shrinks the slice after startup, with no restart.
    ceiling.store(FLOOR - 1, Ordering::SeqCst);
    let seen = reads.load(Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(
        reads.load(Ordering::SeqCst) > seen,
        "maintenance did not re-read the slice"
    );
    // The next job is decided by the new ceiling.
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
}

#[test]
fn maintenance_picks_up_a_changed_declaration_without_changing_the_slice() {
    let f = Fixture::new();
    let (runner, _) = runner_recording(&f, 40 << 30, Some(24 << 30));
    fs::write(
        f.scratch.join("capacity.json"),
        r#"{"jobs_min_bytes":8589934592,"windows_vm_bytes":34359738368}"#,
    )
    .unwrap();
    runner.maintenance().unwrap();
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let detail =
        assert_refused_without_start(&runner, &f, &request, RefusalReason::WorkspaceSetupFailed);
    assert!(detail.contains("jobs_min_bytes=8589934592"), "{detail}");
}
