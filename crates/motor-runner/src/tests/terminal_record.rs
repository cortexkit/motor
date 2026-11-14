use super::*;

#[tokio::test]
async fn oversized_workspace_report_keeps_real_exit_and_replays_bounded_terminal() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    // Five thousand distinct 240-byte names exceed a frame before the terminal
    // envelope is added. Use real untracked files, not a mocked change scan.
    let prefix = "x".repeat(235);
    let (request, bytes) = f.run(&format!(
        "i=0; while [ \"$i\" -lt 5000 ]; do printf -v name '{prefix}%05d' \"$i\"; : > \"$name\"; i=$((i+1)); done; exit 37"
    ));
    f.submit(&runner, &request, &bytes);
    let terminal = terminal(&runner, request.job_id).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 37 });
    assert_eq!(terminal.ran, Some(Ran::Remote));
    assert!(terminal.workspace_changes.is_none());
    let job = runner.core.inner.lock().unwrap().jobs[&request.job_id].clone();
    let changes = runner
        .core
        .backend
        .collect_changes(runner.config(), &runner.core.trees, &job)
        .unwrap()
        .unwrap();
    assert_eq!(changes.len(), 5000);
    assert!(serde_json::to_vec(&changes).unwrap().len() > MAX_FRAME_BYTES);
    assert!(job.terminal.as_ref().unwrap().workspace_changes.is_none());

    let stored: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(stored.terminal.as_ref(), Some(&terminal));
    let mut wire = Vec::new();
    write_terminal_with_memory_async(
        &mut wire,
        &terminal,
        &motor_protocol::TerminalMemory {
            mem_peak_mib: Some(u64::MAX),
            mem_working_peak_mib: Some(u64::MAX),
            mem_over_mib_ms: Some(u64::MAX),
        },
    )
    .await
    .unwrap();
    assert!(wire.len() - 4 < MAX_FRAME_BYTES);
    drop(runner);
    let runner = reopen(f.config());
    assert_eq!(super::terminal(&runner, request.job_id).await, terminal);
}

#[test]
fn storage_and_legacy_replay_apply_the_same_terminal_backstop() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let id = Uuid::now_v7();
    let mut job = Job::new(id, None, 0, 0, 1);
    job.started_ms = Some(1);
    job.terminal(Outcome::Exit { code: 19 }, 2);
    job.terminal.as_mut().unwrap().untracked_files =
        Some(motor_protocol::UntrackedFiles::new(vec![
            "u".repeat(MAX_FRAME_BYTES)
        ]));
    runner.save(&job).unwrap();
    let stored: Job = read_json(&runner.record_path(id)).unwrap();
    assert!(stored.terminal.as_ref().unwrap().untracked_files.is_none());
    // An old record may still be present in memory or on disk after upgrade.
    runner.core.inner.lock().unwrap().jobs.insert(id, job);
    let frames = runner.replay(id, 0).unwrap();
    assert_eq!(
        frames.last(),
        Some(&Frame::Terminal(stored.terminal.unwrap()))
    );
}
