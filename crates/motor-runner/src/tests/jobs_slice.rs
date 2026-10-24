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
            other => panic!("unexpected systemctl arguments {other:?}"),
        };
        Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout,
            stderr: Vec::new(),
        })
    })
}

/// A refusal keeps a job record and no execution: it is admitted nowhere,
/// so its cpus, process and setup time stay empty and its command never ran.
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
