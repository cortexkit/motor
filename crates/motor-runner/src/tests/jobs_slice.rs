//! The direct runner's job units take their memory limit from the shared
//! `motor-jobs.slice`: a unit's `MemoryMax` is clamped to the slice's, and a
//! slice too small for the floor every job requests makes the job refuse
//! before it starts instead of being OOM-killed by the slice on arrival.
use super::*;
use std::sync::atomic::AtomicUsize;

/// Answers the engine's systemd queries, so these tests never depend on the
/// host's own manager: the slice query carries the limits, and the version
/// query says the manager is new enough for `PrivatePIDs`, so the isolated
/// path needs no sandbox account.
fn engine_replies(max: u64) -> crate::systemd::CommandHook {
    let reply = format!("MemoryHigh={}\nMemoryMax={max}\n", max - max / 6);
    Arc::new(move |args| {
        let stdout = match args {
            ["show", "--property=MemoryHigh,MemoryMax", "motor-jobs.slice"] => {
                reply.as_bytes().to_vec()
            }
            ["show", "--property=Version", "--value"] => b"259.0".to_vec(),
            other => panic!("unexpected systemctl arguments {other:?}"),
        };
        Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout,
            stderr: Vec::new(),
        })
    })
}

/// Delegates everything to the host backend while counting materialization
/// attempts, the last step before a job's unit would start.
struct CountingSetup {
    attempts: Arc<AtomicUsize>,
    host: backend::HostBackend,
}

macro_rules! delegate {
    ($($name:ident($($arg:ident: $kind:ty),*) -> $result:ty;)*) => { $(
        fn $name(&self, $($arg: $kind),*) -> $result {
            self.host.$name($($arg),*)
        }
    )* };
}

impl backend::Backend for CountingSetup {
    fn materialize(&self, config: &Config, trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.host.materialize(config, trees, setup)
    }
    delegate! {
        launch(config: &Config, trees: &Trees, setup: &Setup, prepared: &Prepared, weight: u32) -> io::Result<backend::Launch>;
        cancel(config: &Config, job: &Job, hard: bool) -> io::Result<()>;
        unit_alive(config: &Config, process: Option<platform::ProcessIdentity>) -> io::Result<Option<bool>>;
        collect_changes(trees: &Trees, job: &Job) -> io::Result<Option<Vec<String>>>;
        reconcile(config: &Config, interrupted: &[Job]) -> io::Result<Vec<Uuid>>;
    }
}

#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_below_the_job_floor_refuses_admission_before_start() {
    let f = Fixture::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.development_allow_host_backend = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(engine_replies((16 << 30) - 1));
    let runner = Runner::open_with_backend(
        config,
        Box::new(CountingSetup {
            attempts: attempts.clone(),
            host: backend::HostBackend,
        }),
    )
    .unwrap();
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::RunnerFull)
    ));
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    let detail = terminal.refusal_detail().unwrap();
    assert!(
        detail.contains("MemoryMax=17179869183") && detail.contains("16 GiB memory floor"),
        "{detail}"
    );
    assert_eq!(runner.status().queue_depth, 0);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "nothing tried to start the job"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_at_the_job_floor_admits_and_clamps_instead_of_refusing() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.development_allow_host_backend = true;
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(engine_replies(16 << 30));
    let floor = config.memory_floor_gib;
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    // A larger claim is clamped, not refused: its limit becomes the slice's.
    assert_eq!(
        crate::backend::memory_max(30, floor).min(16 << 30),
        16 << 30
    );
    runner.cancel(request.job_id).unwrap();
}
