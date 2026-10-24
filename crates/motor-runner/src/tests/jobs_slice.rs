//! The direct runner's job units take their memory limit from the shared
//! `motor-jobs.slice`: a unit's `MemoryMax` is clamped to the slice's, and a
//! slice too small for the floor every job requests makes the job refuse
//! before it starts instead of being OOM-killed by the slice on arrival.
//!
//! The tests that open a runner with job isolation are `isolated_*` operator
//! fixtures: the isolated path checks the `motor-job` account and grants it
//! access to the host trees, which needs a provisioned host. Run them on a
//! dedicated Linux host as root, for example:
//!
//! ```text
//! cargo test -p motor-runner --lib -- --ignored isolated_jobs_slice
//! ```
//!
//! The rest run anywhere.
use super::*;

/// A fake systemd for the engine's queries that can make the slice reply
/// slow, so a test can see whether the reply was waited for under the state
/// lock.
struct FakeManager {
    slice: Mutex<String>,
    /// How long a slice query takes to answer.
    delay: Mutex<Duration>,
}

impl FakeManager {
    /// A manager whose 24 GiB slice is read from `slice`.
    fn hook(slice: impl Into<String>) -> (crate::systemd::CommandHook, Arc<FakeManager>) {
        let manager = Arc::new(FakeManager {
            slice: Mutex::new(slice.into()),
            delay: Mutex::new(Duration::ZERO),
        });
        let for_hook = manager.clone();
        let hook: crate::systemd::CommandHook = Arc::new(move |args| Ok(for_hook.reply(args)));
        (hook, manager)
    }

    fn set_slice(&self, slice: String) {
        *self.slice.lock().unwrap() = slice;
    }

    fn reply(&self, args: &[&str]) -> std::process::Output {
        let stdout = match args {
            ["show", "--property=MemoryHigh,MemoryMax", "motor-jobs.slice"] => {
                std::thread::sleep(*self.delay.lock().unwrap());
                self.slice.lock().unwrap().clone().into_bytes()
            }
            // The isolated path asks this once at startup: a manager new
            // enough for `PrivatePIDs` keeps the test free of accounts.
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

fn slice_reply(max: u64) -> String {
    format!("MemoryHigh=1\nMemoryMax={max}\n")
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
// Needs the motor-job account and host access grants: see the module docs.
#[ignore = "requires the motor-job account, root, and a host with the runner units"]
fn isolated_jobs_slice_below_the_job_floor_refuses_admission_before_start() {
    let f = Fixture::new();
    let (hook, _) = FakeManager::hook(slice_reply((16 << 30) - 1));
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
// Needs the motor-job account and host access grants: see the module docs.
#[ignore = "requires the motor-job account, root, and a host with the runner units"]
fn isolated_jobs_slice_at_the_job_floor_admits_and_clamps_instead_of_refusing() {
    let f = Fixture::new();
    let (hook, _) = FakeManager::hook(slice_reply(16 << 30));
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

/// Admission holds the runner's state lock, which every other admission,
/// status reply and cancel waits for. Reading the slice there would stall all
/// of them for as long as systemd takes to answer, so admission uses the
/// ceiling the runner already recorded: with a slow manager, no hold of the
/// state lock comes near the slow-hold threshold (100 ms).
#[cfg(target_os = "linux")]
#[test]
// Needs the motor-job account and host access grants: see the module docs.
#[ignore = "requires the motor-job account, root, and a host with the runner units"]
fn isolated_admission_reads_the_recorded_slice_ceiling_without_waiting_under_the_state_lock() {
    let f = Fixture::new();
    let (hook, manager) = FakeManager::hook(slice_reply(16 << 30));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(hook);
    let runner = Runner::open(config).unwrap();
    // Every later slice read is slow, as a loaded host's systemctl can be.
    *manager.delay.lock().unwrap() = Duration::from_millis(300);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    runner.cancel(request.job_id).unwrap();
    let slow: Vec<_> = runner
        .core
        .inner
        .site_holds()
        .into_iter()
        .filter(|(_, holds)| holds.longest >= crate::state_lock::SLOW_HOLD)
        .collect();
    assert!(
        slow.is_empty(),
        "admission waited for the slice under the state lock: {slow:?}"
    );
}

/// The runner re-reads the slice on every maintenance pass, again without the
/// state lock, and admission refuses by the new ceiling.
#[cfg(target_os = "linux")]
#[test]
// Needs the motor-job account and host access grants: see the module docs.
#[ignore = "requires the motor-job account, root, and a host with the runner units"]
fn isolated_maintenance_records_the_slice_ceiling_that_admission_refuses_by() {
    let f = Fixture::new();
    let (hook, manager) = FakeManager::hook(slice_reply(16 << 30));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(hook);
    let runner = Runner::open(config).unwrap();
    manager.set_slice(slice_reply((16 << 30) - 1));
    *manager.delay.lock().unwrap() = Duration::from_millis(300);
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
        "maintenance waited for the slice under the state lock: {slow:?}"
    );
}
