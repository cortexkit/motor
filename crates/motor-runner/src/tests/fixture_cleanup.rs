use super::*;

#[tokio::test]
async fn fixture_waits_for_a_background_fill_before_removing_its_scratch() {
    let signals = ScratchDir::new("fill-teardown-signals");
    let started = signals.join("started");
    let release = signals.join("release");
    let completed = signals.join("completed");
    let f = Fixture::new();
    let scratch = f.scratch.to_path_buf();
    let mut config = f.config();
    // Direct Sandbox fixtures can open a runner without the Config recorder;
    // matching the registered scratch root must still enroll that runner.
    config.test_processes = None;
    config.base_commands = vec![format!(
        "printf started > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; test -d \"$CARGO_TARGET_DIR\" && printf complete > '{}'",
        started.display(), release.display(), completed.display()
    )];
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    until(|| started.exists()).await;
    let (done, received) = std::sync::mpsc::channel();
    let teardown = std::thread::spawn(move || {
        drop(runner);
        drop(f);
        done.send(()).unwrap();
    });
    let early = received.recv_timeout(Duration::from_millis(200));
    let retained = scratch.exists();
    // Release the real fill before asserting, so a failing control cannot leave
    // its teardown thread held waiting for a signal the test never sends.
    fs::write(&release, b"release").unwrap();
    teardown.join().unwrap();
    assert!(matches!(
        early,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(
        retained,
        "scratch disappeared while the warm fill was still running"
    );
    assert_eq!(fs::read(completed).unwrap(), b"complete");
    assert!(!scratch.exists());
}
