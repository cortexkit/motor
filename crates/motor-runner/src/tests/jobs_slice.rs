//! The direct runner's job units take their memory limit from the shared
//! `motor-jobs.slice`: a unit's `MemoryMax` is clamped to the slice's, and a
//! slice too small for the floor every job requests makes the job refuse
//! before it starts instead of being OOM-killed by the slice on arrival.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// Answers the engine's systemd queries from this test's fake manager and
/// records any slice query that ran while the runner's state lock was held.
struct FakeManager {
    slice: String,
    /// Set while the test holds the state lock itself, to model admission.
    lock_held: Arc<AtomicBool>,
    queries_under_lock: Arc<AtomicUsize>,
}

impl FakeManager {
    fn hook(slice: impl Into<String>) -> (crate::systemd::CommandHook, Arc<AtomicUsize>) {
        let under_lock = Arc::new(AtomicUsize::new(0));
        let manager = Arc::new(FakeManager {
            slice: slice.into(),
            lock_held: Arc::new(AtomicBool::new(false)),
            queries_under_lock: under_lock.clone(),
        });
        let hook: crate::systemd::CommandHook = Arc::new(move |args| {
            if args
                == [
                    "show",
                    "--property=MemoryHigh,MemoryMax",
                    "motor-jobs.slice",
                ]
                && manager.lock_held.load(AtomicOrdering::SeqCst)
            {
                manager
                    .queries_under_lock
                    .fetch_add(1, AtomicOrdering::SeqCst);
            }
            Ok(manager.reply(args))
        });
        (hook, under_lock)
    }

    fn reply(&self, args: &[&str]) -> std::process::Output {
        let stdout = match args {
            ["show", "--property=MemoryHigh,MemoryMax", "motor-jobs.slice"] => {
                self.slice.clone().into_bytes()
            }
            ["show", "--property=Version", "--value"] => b"259.0".to_vec(),
            // Unit listings and the like only matter to a start, which these
            // tests never reach; an empty reply keeps them quiet.
            _ => Vec::new(),
        };
        std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout,
            stderr: Vec::new(),
        }
    }
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
    terminal.refusal_detail().unwrap_or_default()
}

/// A slice one byte below the floor every job unit requests refuses the job.
/// The reason is a setup fault, not a full or busy runner: the slice will not
/// get bigger while the job waits, so nothing calls for a retry.
#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_below_the_job_floor_refuses_admission_before_start() {
    let f = Fixture::new();
    let (hook, _) = FakeManager::hook(format!("MemoryHigh=1\nMemoryMax={}\n", (16 << 30) - 1));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(hook);
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let detail =
        assert_refused_without_start(&runner, &f, &request, RefusalReason::WorkspaceSetupFailed);
    assert!(
        detail.contains("MemoryMax=17179869183") && detail.contains("16 GiB memory floor"),
        "{detail}"
    );
}

/// At exactly the floor the job is admitted as usual; only its unit's
/// `MemoryMax` is clamped, and the slice's own OOM killer reports an overrun.
#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_at_the_job_floor_admits_and_clamps_instead_of_refusing() {
    let f = Fixture::new();
    let (hook, _) = FakeManager::hook(format!("MemoryHigh=1\nMemoryMax={}\n", 16 << 30));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(hook);
    let floor = config.memory_floor_gib;
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    assert_eq!(
        crate::backend::memory_max(30, floor).min(16 << 30),
        16 << 30
    );
    runner.cancel(request.job_id).unwrap();
}

/// The probe answers a slice query only while the test holds the state lock,
/// which is exactly when `Runner::accept` holds it, so it does record a query
/// that admission issued there.
#[cfg(target_os = "linux")]
#[test]
fn state_lock_probe_answers_a_slice_query_issued_under_the_lock() {
    let f = Fixture::new();
    let (hook, under_lock) = FakeManager::hook(format!("MemoryHigh=1\nMemoryMax={}\n", 16 << 30));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(hook);
    let runner = Runner::open(config).unwrap();
    under_lock.store(true, AtomicOrdering::SeqCst);
    let counted = {
        let _ = runner.core.inner.lock().unwrap();
        runner.accept(f.run("true").0).unwrap();
        under_lock.load(AtomicOrdering::SeqCst)
    };
    under_lock.store(false, AtomicOrdering::SeqCst);
    assert_eq!(
        counted, 1,
        "the probe records a query issued under the lock"
    );
}

/// Admission runs under the runner's state lock; asking systemd for the slice
/// there would stall every other admission, status reply and cancel. The fake
/// manager records a slice query made while the lock is held, so this fails if
/// admission ever starts one there.
#[cfg(target_os = "linux")]
#[test]
fn admission_never_queries_the_slice_while_holding_the_state_lock() {
    let f = Fixture::new();
    let (hook, under_lock) = FakeManager::hook(format!("MemoryHigh=1\nMemoryMax={}\n", 16 << 30));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(hook);
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    under_lock.store(true, AtomicOrdering::SeqCst);
    let counted = {
        let _ = runner.core.inner.lock().unwrap();
        runner.accept(request.clone()).unwrap();
        under_lock.load(AtomicOrdering::SeqCst)
    };
    under_lock.store(false, AtomicOrdering::SeqCst);
    assert_eq!(
        counted, 0,
        "admission ran a slice query under the state lock"
    );
    runner.cancel(request.job_id).unwrap();
}
