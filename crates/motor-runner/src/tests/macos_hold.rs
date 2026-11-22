use super::*;
use std::{os::unix::process::ExitStatusExt, sync::mpsc};

struct HoldBackend(Mutex<BTreeMap<Uuid, mpsc::Receiver<()>>>);
struct HoldExecution(mpsc::Receiver<()>);

// Only execution is simulated: admission, uploads, queue dispatch and terminal
// persistence still run through the engine, without touching a host checkout.
impl Backend for HoldBackend {
    fn materialize(&self, config: &Config, _: &Trees, setup: &Setup) -> io::Result<Prepared> {
        let cwd = config
            .state_root
            .join("fake-jobs")
            .join(setup.id.to_string());
        fs::create_dir_all(&cwd)?;
        Ok(Prepared {
            target: cwd.join("target"),
            host_cwd: cwd,
            wrapper: None,
            sandbox: None,
            vm: None,
        })
    }
    fn launch(
        &self,
        _: &Config,
        _: &Trees,
        setup: &Setup,
        _: &Prepared,
        _: u32,
    ) -> io::Result<backend::Launch> {
        Ok(backend::Launch {
            process: None,
            durable_output: false,
            cache_ledgers: Vec::new(),
            execution: Box::new(HoldExecution(
                self.0.lock().unwrap().remove(&setup.id).unwrap(),
            )),
        })
    }
    fn cancel(&self, _: &Config, _: &Job, _: bool) -> io::Result<()> {
        Ok(())
    }
    fn unit_alive(
        &self,
        _: &Config,
        _: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        Ok(None)
    }
    fn collect_changes(&self, _: &Config, _: &Trees, _: &Job) -> io::Result<Option<Vec<String>>> {
        Ok(None)
    }
    fn reconcile(&self, _: &Config, _: &[Job]) -> io::Result<Vec<Uuid>> {
        Ok(Vec::new())
    }
}

impl backend::Execution for HoldExecution {
    fn release(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn output(&self) -> io::Result<backend::Output> {
        Ok(backend::Output {
            stdout: Box::new(io::empty()),
            stderr: Box::new(io::empty()),
        })
    }
    fn wait(&mut self) -> io::Result<backend::ExecutionOutcome> {
        self.0
            .recv_timeout(Duration::from_secs(15))
            .map_err(io::Error::other)?;
        Ok(backend::ExecutionOutcome::Unix(
            std::process::ExitStatus::from_raw(0),
        ))
    }
    fn reap(&self) {}
}

fn config(f: &Fixture) -> Config {
    let mut config = platform_config(f, BackendKind::MacosVm);
    let path = config.vm_config.as_ref().unwrap();
    let text = fs::read_to_string(path).unwrap();
    fs::write(path, format!("hold_window = '03:00-05:00'\n{text}")).unwrap();
    let hold = f.scratch.join("hold");
    let grant = f.scratch.join("grant");
    fs::create_dir_all(&hold).unwrap();
    fs::create_dir_all(&grant).unwrap();
    config.macos_hold_paths = Some((hold, grant));
    config.manual_tick = true;
    f.clock.store(1_767_238_200_000, Ordering::SeqCst); // 2026-01-01 03:30 UTC.
    config
}

fn write_hold(runner: &Runner) {
    write_hold_for(runner, "nightly-check");
}

fn write_hold_for(runner: &Runner, owner: &str) {
    let paths = runner.config().macos_hold_paths.as_ref().unwrap();
    fs::write(
        paths.0.join("hold.json"),
        serde_json::json!({"owner": owner, "expires_at": "2026-01-01T04:00:00Z"}).to_string(),
    )
    .unwrap();
}

fn granted(runner: &Runner) -> bool {
    runner
        .config()
        .macos_hold_paths
        .as_ref()
        .unwrap()
        .1
        .join("granted.json")
        .exists()
}

fn wait_for(runner: &Runner, predicate: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        runner.tick().unwrap();
        if predicate() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job lifecycle did not settle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn running(runner: &Runner, id: Uuid) -> bool {
    runner.core.inner.lock().unwrap().jobs[&id].state == JobState::Running
}

fn finished(runner: &Runner, id: Uuid) -> bool {
    runner.job_status(id).unwrap().terminal.is_some()
}

fn submit(f: &Fixture, runner: &Runner, request: &Run) {
    let (manifest, bytes) = f.bundle(&f.main, &request.repository_root, 0);
    let mut request = request.clone();
    request.bundles = vec![manifest];
    f.submit(runner, &request, &[bytes]);
}

#[test]
fn honoured_hold_refuses_new_run_while_running_job_finishes() {
    let f = Fixture::new();
    let request = macos_request(&f);
    let (finish, receive) = mpsc::channel();
    let backend = HoldBackend(Mutex::new(BTreeMap::from([(request.job_id, receive)])));
    let runner = Runner::open_with_backend(config(&f), Box::new(backend)).unwrap();
    submit(&f, &runner, &request);
    wait_for(&runner, || running(&runner, request.job_id));
    write_hold(&runner);
    runner.tick().unwrap();
    assert!(!granted(&runner));
    assert!(matches!(
        runner.accept(macos_request(&f)).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));
    assert!(running(&runner, request.job_id));
    finish.send(()).unwrap();
    wait_for(&runner, || {
        finished(&runner, request.job_id) && granted(&runner)
    });
    assert_eq!(
        runner
            .job_status(request.job_id)
            .unwrap()
            .terminal
            .unwrap()
            .outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .admission_blocked
            .as_deref(),
        Some("held for nightly-check until 2026-01-01T04:00:00Z (granted: true)")
    );
}

#[test]
fn refused_job_retries_between_holds_and_finishes_before_next_grant() {
    let f = Fixture::new();
    let retry = macos_request(&f);
    let (finish, receive) = mpsc::channel();
    let backend = HoldBackend(Mutex::new(BTreeMap::from([(retry.job_id, receive)])));
    let runner = Runner::open_with_backend(config(&f), Box::new(backend)).unwrap();
    write_hold(&runner);
    runner.tick().unwrap();
    assert!(granted(&runner));
    let mut first_attempt = retry.clone();
    first_attempt.job_id = Uuid::now_v7();
    assert!(matches!(
        runner.accept(first_attempt).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));

    write_hold_for(&runner, "next-check");
    submit(&f, &runner, &retry);
    assert!(!granted(&runner));
    wait_for(&runner, || running(&runner, retry.job_id));

    f.clock.fetch_add(60_000, Ordering::SeqCst);
    runner.tick().unwrap();
    assert!(!granted(&runner));
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .admission_blocked
            .as_deref(),
        Some("held for next-check until 2026-01-01T04:00:00Z (granted: false)")
    );

    finish.send(()).unwrap();
    wait_for(&runner, || {
        finished(&runner, retry.job_id) && granted(&runner)
    });
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .admission_blocked
            .as_deref(),
        Some("held for next-check until 2026-01-01T04:00:00Z (granted: true)")
    );
}

#[test]
fn queued_job_accepted_before_hold_runs_before_grant() {
    let f = Fixture::new();
    let first = macos_request(&f);
    let second = macos_request(&f);
    let (finish_first, receive_first) = mpsc::channel();
    let (finish_second, receive_second) = mpsc::channel();
    let backend = HoldBackend(Mutex::new(BTreeMap::from([
        (first.job_id, receive_first),
        (second.job_id, receive_second),
    ])));
    let runner = Runner::open_with_backend(config(&f), Box::new(backend)).unwrap();
    submit(&f, &runner, &first);
    wait_for(&runner, || running(&runner, first.job_id));
    submit(&f, &runner, &second);
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&second.job_id].state,
        JobState::Queued
    );
    write_hold(&runner);
    runner.tick().unwrap();
    assert!(!granted(&runner));
    finish_first.send(()).unwrap();
    wait_for(&runner, || running(&runner, second.job_id));
    assert!(
        !granted(&runner),
        "the queued job must run before granting the hold"
    );
    assert_eq!(
        runner
            .job_status(first.job_id)
            .unwrap()
            .terminal
            .unwrap()
            .outcome,
        Outcome::Exit { code: 0 }
    );
    finish_second.send(()).unwrap();
    wait_for(&runner, || {
        finished(&runner, second.job_id) && granted(&runner)
    });
    assert_eq!(
        runner
            .job_status(second.job_id)
            .unwrap()
            .terminal
            .unwrap()
            .outcome,
        Outcome::Exit { code: 0 }
    );
}

#[test]
fn deleting_hold_reopens_admission_but_never_clears_operator_drain() {
    let f = Fixture::new();
    let runner = Runner::open_with_backend(config(&f), Box::new(MacosTerminalBackend)).unwrap();
    write_hold(&runner);
    runner.tick().unwrap();
    assert!(granted(&runner));
    runner
        .try_begin_drain_request(Duration::from_secs(30), false)
        .unwrap();
    runner.try_resume().unwrap();
    assert!(matches!(
        runner.accept(macos_request(&f)).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));
    runner
        .try_begin_drain_request(Duration::from_secs(30), false)
        .unwrap();
    fs::remove_file(
        runner
            .config()
            .macos_hold_paths
            .as_ref()
            .unwrap()
            .0
            .join("hold.json"),
    )
    .unwrap();
    runner.tick().unwrap();
    assert!(!granted(&runner));
    assert!(runner.is_draining());
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .admission_blocked
            .as_deref(),
        Some("runner_draining")
    );
    assert!(matches!(
        runner.accept(macos_request(&f)).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));
    runner.try_resume().unwrap();
    assert!(matches!(
        runner.accept(macos_request(&f)).unwrap(),
        Admission::Accepted(_)
    ));
}

#[test]
fn restart_rereads_hold_and_removes_stale_grant_when_hold_ends() {
    let f = Fixture::new();
    let config = config(&f);
    let open = || {
        reopen_with(|| Runner::open_with_backend(config.clone(), Box::new(MacosTerminalBackend)))
    };
    let runner = open();
    write_hold(&runner);
    runner.tick().unwrap();
    assert!(granted(&runner));
    drop(runner);
    let runner = open();
    assert!(matches!(
        runner.accept(macos_request(&f)).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));
    assert!(granted(&runner));
    drop(runner);
    f.clock.fetch_add(30 * 60 * 1000, Ordering::SeqCst);
    let runner = open();
    assert!(!granted(&runner));
    assert!(matches!(
        runner.accept(macos_request(&f)).unwrap(),
        Admission::Accepted(_)
    ));
}
