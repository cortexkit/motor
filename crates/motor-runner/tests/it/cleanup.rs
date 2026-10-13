use cortexkit_test_support::ScratchDir;
use motor_protocol::Uuid;
use motor_runner::Runner;
use std::{
    fs,
    mem::ManuallyDrop,
    ops::Deref,
    path::PathBuf,
    time::{Duration, Instant},
};

type UnitCommand = dyn Fn(&str, &str) + Send;

/// The public-protocol fixtures link a non-test runner, so their teardown cannot
/// use the library's test-only launch recorder. Persisted job ids cover writers
/// as well as foreground jobs, including units whose monitors were abandoned.
pub(crate) struct OperatorScratch {
    scratch: ManuallyDrop<ScratchDir>,
    runner: Option<Runner>,
    server: Option<tokio::task::AbortHandle>,
    state: Option<PathBuf>,
    unit_command: Box<UnitCommand>,
}
impl OperatorScratch {
    pub(crate) fn new(label: &str) -> Self {
        Self {
            scratch: ManuallyDrop::new(ScratchDir::new(label)),
            runner: None,
            server: None,
            state: None,
            unit_command: Box::new(systemctl),
        }
    }
    pub(crate) fn attach(&mut self, runner: &Runner, server: tokio::task::AbortHandle) {
        self.state = Some(runner.config().state_dir());
        self.runner = Some(runner.clone());
        self.server = Some(server);
    }
    pub(crate) fn release_runner(&mut self) {
        // drain_fixture_runner proves all Core owners are gone by acquiring
        // daemon.lock. Release this extra clone before it waits for that lock.
        self.runner.take();
    }
    fn job_ids(&self) -> Vec<Uuid> {
        self.state
            .as_ref()
            .and_then(|state| fs::read_dir(state.join("jobs")).ok())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    }
    fn unit_ids(&self) -> std::collections::BTreeSet<Uuid> {
        let Some(state) = &self.state else {
            return Default::default();
        };
        self.job_ids()
            .into_iter()
            .filter_map(|id| {
                let record: serde_json::Value = serde_json::from_slice(
                    &fs::read(state.join("jobs").join(id.to_string()).join("record.json")).ok()?,
                )
                .ok()?;
                record.get("process")?.get("unit")?.as_str()?.parse().ok()
            })
            .collect()
    }
    fn quiesce(&self) -> bool {
        let Some(runner) = &self.runner else {
            return true;
        };
        runner.begin_drain(Duration::from_secs(60), false);
        let started = Instant::now();
        loop {
            let status = runner.status();
            let reaped = self.job_ids().iter().all(|id| {
                !runner
                    .config()
                    .state_dir()
                    .join("sandboxes")
                    .join(id.to_string())
                    .exists()
            });
            if status.queue_depth == 0
                && status.running_jobs.is_empty()
                && reaped
                && runner.drain_result() == Some(false)
            {
                return true;
            }
            if started.elapsed() >= Duration::from_secs(30) {
                // Cancel queued setup too: it must not launch a new unit after
                // this guard has taken its final snapshot of recorded units.
                for id in self.job_ids() {
                    let _ = runner.cancel(id);
                }
            }
            if started.elapsed() >= Duration::from_secs(60) {
                return false;
            }
            std::thread::park_timeout(Duration::from_millis(25));
        }
    }
}
fn systemctl(operation: &str, unit: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/systemctl", operation, unit])
        .status();
    #[cfg(not(target_os = "linux"))]
    let _ = (operation, unit);
}
impl Deref for OperatorScratch {
    type Target = ScratchDir;
    fn deref(&self) -> &ScratchDir {
        &self.scratch
    }
}
impl Drop for OperatorScratch {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
        let quiet = self.quiesce();
        for id in self.unit_ids() {
            for operation in ["stop", "reset-failed"] {
                (self.unit_command)(operation, &format!("motor-job-{id}.service"));
            }
        }
        self.runner.take();
        if quiet {
            unsafe { ManuallyDrop::drop(&mut self.scratch) };
        } else {
            eprintln!(
                "operator fixture did not quiesce; retaining {}",
                self.scratch.path().display()
            );
            if !std::thread::panicking() {
                panic!("operator fixture did not quiesce before scratch cleanup");
            }
        }
    }
}

#[test]
fn public_fixture_backstop_resets_recorded_writer_and_job_units_on_panic() {
    use std::sync::{Arc, Mutex};
    let commands = Arc::new(Mutex::new(Vec::new()));
    let trace = commands.clone();
    let unit_ids = [Uuid::now_v7(), Uuid::now_v7()];
    let result = std::panic::catch_unwind(|| {
        let mut scratch = OperatorScratch::new("public-unit-cleanup");
        let state = scratch.join("state");
        for (index, unit) in unit_ids.iter().enumerate() {
            let job = state.join("jobs").join(Uuid::now_v7().to_string());
            fs::create_dir_all(&job).unwrap();
            fs::write(
                job.join("record.json"),
                serde_json::to_vec(&serde_json::json!({
                    "base": index == 0, "process": { "unit": unit }
                }))
                .unwrap(),
            )
            .unwrap();
        }
        scratch.state = Some(state);
        scratch.unit_command = Box::new(move |operation, unit| {
            trace
                .lock()
                .unwrap()
                .push((operation.to_owned(), unit.to_owned()));
        });
        assert!(commands.lock().unwrap().is_empty());
        panic!("deliberate inner test failure");
    });
    assert!(result.is_err());
    let mut expected = Vec::new();
    for id in unit_ids {
        let unit = format!("motor-job-{id}.service");
        expected.push(("stop".to_owned(), unit.clone()));
        expected.push(("reset-failed".to_owned(), unit));
    }
    assert_eq!(*commands.lock().unwrap(), expected);
}

#[tokio::test]
async fn public_fixture_waits_for_a_background_fill_before_scratch_removal() {
    use super::{git, snapshot_bundle, snapshot_run, submit_with_capability};
    use motor_protocol::Outcome;
    let signals = ScratchDir::new("public-fill-signals");
    let started = signals.join("started");
    let release = signals.join("release");
    let completed = signals.join("completed");
    let mut scratch = OperatorScratch::new("public-fill-cleanup");
    let path = scratch.path().to_path_buf();
    let source = scratch.join("source");
    fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let tree = git(&source, &["rev-parse", "HEAD^{tree}"]);
    let bundle = snapshot_bundle(&source, &scratch.join("snapshot.bundle"));
    let mut config = motor_runner::Config::new(scratch.join("state"), "cleanup");
    config.free_space_floor_bytes = 0;
    config.workspace_roots = vec![scratch.join("workspaces")];
    config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
    config.base_commands = vec![format!(
        "printf started > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; test -d \"$CARGO_TARGET_DIR\" && printf complete > '{}'",
        started.display(), release.display(), completed.display()
    )];
    let runner = Runner::open(config).unwrap();
    let socket_scratch = super::SocketScratch::new();
    let socket = socket_scratch.path().join("cleanup.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(motor_runner::serve(runner.clone(), listener));
    scratch.attach(&runner, server.abort_handle());
    let mut request = snapshot_run(
        &scratch.join("workspaces/job"),
        &source,
        &commit,
        &tree,
        "true",
        &bundle,
    );
    request.base_commit = Some(commit);
    let (terminal, _) = submit_with_capability(&socket, request, &bundle, true).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (done, received) = std::sync::mpsc::channel();
    let teardown = std::thread::spawn(move || {
        drop(runner);
        drop(scratch);
        done.send(()).unwrap();
    });
    let early = received.recv_timeout(Duration::from_millis(200));
    let retained = path.exists();
    fs::write(release, b"release").unwrap();
    teardown.join().unwrap();
    server.abort();
    let _ = server.await;
    assert!(matches!(
        early,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(retained);
    assert_eq!(fs::read(completed).unwrap(), b"complete");
    assert!(!path.exists());
}
