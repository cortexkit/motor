use super::*;

#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_jobs_receive_private_shm_that_disappears_after_they_end() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.base_commands.clear();
    let runner = Runner::open(config).unwrap();

    let (mut first, bytes) = f.run("true");
    first.workspace_key = text(&f.scratch.join("remote/worktrees/shm-first")).into();
    first.cwd = first.workspace_key.clone();
    let marker = format!("motor-runner-shm-{}", first.job_id);
    let host_marker = Path::new("/dev/shm").join(&marker);
    assert!(Path::new("/dev/shm").is_dir());
    assert!(
        !host_marker.exists(),
        "the unique host probe already exists"
    );
    first.command =
        format!("set -eu; printf 'private\n' > /dev/shm/{marker}; printf 'shm-ready\n'; sleep 8");
    f.submit(&runner, &first, &bytes);
    until(|| job_output(&runner, first.job_id).contains("shm-ready")).await;

    let (mut second, bytes) = f.run("true");
    second.workspace_key = text(&f.scratch.join("remote/worktrees/shm-second")).into();
    second.cwd = second.workspace_key.clone();
    second.command = format!(
        "set -eu; test ! -e /dev/shm/{marker}; printf 'shm-concurrent-isolated\\n'; sleep 12; test ! -e /dev/shm/{marker}; printf 'shm-after-isolated\\n'"
    );
    f.submit(&runner, &second, &bytes);
    until(|| job_output(&runner, second.job_id).contains("shm-concurrent-isolated")).await;
    assert!(
        runner.core.inner.lock().unwrap().jobs[&first.job_id]
            .terminal
            .is_none(),
        "the first job ended before the second job's concurrent check"
    );

    let first_terminal = terminal(&runner, first.job_id).await;
    assert_eq!(first_terminal.outcome, Outcome::Exit { code: 0 });
    assert_eq!(first_terminal.ran, Some(Ran::Remote));
    assert!(
        !host_marker.exists(),
        "the first job's /dev/shm file leaked onto the host"
    );
    assert!(
        !job_output(&runner, second.job_id).contains("shm-after-isolated"),
        "the second job finished its post-exit probe before the first job ended"
    );

    let second_terminal = terminal(&runner, second.job_id).await;
    assert_eq!(second_terminal.outcome, Outcome::Exit { code: 0 });
    assert_eq!(second_terminal.ran, Some(Ran::Remote));
    assert!(job_output(&runner, second.job_id).contains("shm-after-isolated"));
    assert!(!host_marker.exists());
    base_done(&runner).await;
}
