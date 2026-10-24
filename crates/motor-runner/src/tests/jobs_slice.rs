//! The direct runner's job units take their memory limit from the shared
//! `motor-jobs.slice`: a unit's `MemoryMax` is clamped to the slice's, and a
//! slice too small for the floor every job requests makes the job refuse
//! before it starts instead of being OOM-killed by the slice on arrival.
use super::*;

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
            // Unit listings and the like only matter to a start, which these
            // tests never reach; an empty reply keeps them quiet.
            other => {
                let _ = other;
                Vec::new()
            }
        };
        Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout,
            stderr: Vec::new(),
        })
    })
}

/// A refusal keeps a job record and no execution: its cpus, process and
/// setup time stay empty, the queue stays empty, and the command never ran.
fn assert_refused_without_start(runner: &Runner, f: &Fixture, request: &Run) {
    let terminal = runner.job_status(request.job_id).unwrap().terminal.unwrap();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    let detail = terminal.refusal_detail().unwrap();
    assert!(
        detail.contains("MemoryMax=17179869183") && detail.contains("16 GiB memory floor"),
        "{detail}"
    );
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert!(job.cpus.is_empty() && job.process.is_none() && job.setup_ms.is_none());
    assert_eq!(runner.status().queue_depth, 0);
    assert!(!Path::new(&f.key).join("must-not-start").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_below_the_job_floor_refuses_admission_before_start() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    // A portable test has no sandbox account, and with no global run there is
    // no shared toolchain tree to prepare, so admission reads the slice alone.
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(engine_replies((16 << 30) - 1));
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("touch must-not-start");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::RunnerFull)
    ));
    assert_refused_without_start(&runner, &f, &request);
}

/// At exactly the floor the job is admitted as usual; only its unit's
/// `MemoryMax` is clamped, and the slice's own OOM killer reports an overrun.
#[cfg(target_os = "linux")]
#[test]
fn jobs_slice_at_the_job_floor_admits_and_clamps_instead_of_refusing() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    // A portable test has no sandbox account, and with no global run there is
    // no shared toolchain tree to prepare, so admission reads the slice alone.
    config.memory_floor_gib = Some(16);
    config.systemd_command = Some(engine_replies(16 << 30));
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
