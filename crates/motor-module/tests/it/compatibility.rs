use super::recovery::{Cut, Fixture};
use crate::Connector;
use motor_protocol::*;
use std::{fs, time::Duration};
use tokio::{net::TcpListener, process::Command};

fn commit_checkout(path: &std::path::Path) -> String {
    crate::snapshot::git(path, &["add", "-A"]).unwrap();
    crate::snapshot::git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "lockfile change",
        ],
    )
    .unwrap();
    crate::snapshot::git(path, &["rev-parse", "HEAD"]).unwrap()
}

async fn successful_run(motor: &crate::Motor, request: RunRequest) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = motor.run(request).unwrap();
        while let Some(record) = stream.records.recv().await {
            if let StreamRecord::Terminal(terminal) = record.unwrap() {
                assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
                return;
            }
        }
        panic!("current client stream closed without Terminal");
    })
    .await
    .expect("current client Run hung");
}

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production runner"]
async fn new_direct_client_omits_gate_mark_and_old_runner_admits_ordinary_job() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let helper = std::env::var_os("MOTOR_COMPAT_RUNNER")
        .expect("run python3 scripts/mixed-version.py to build the unchanged pinned runner");
    let mut f = Fixture::new(Cut::None, 28).await;
    let state = f.scratch.join("old-runner-state");
    let socket = cortexkit_test_support::scratch_root()
        .parent()
        .unwrap()
        .join(format!(
            "old-motor-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
    let mut helper = Command::new(helper)
        .args([&state, &socket, f.remote_key.parent().unwrap()])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut ready = String::new();
    tokio::time::timeout(
        Duration::from_secs(15),
        BufReader::new(helper.stdout.take().unwrap()).read_line(&mut ready),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ready, "ready\n", "pinned runner failed before serving");
    f.use_external_runner_socket(socket);
    let motor = f.motor();
    let mut request = f.request("printf old-runner-gate");
    request.timeout = Some(60);
    let mut stream = motor.run_direct(request, None).unwrap();
    let id = stream.job_id;
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(record) = stream.records.recv().await {
            match record.unwrap() {
                StreamRecord::Output(record) => output.extend(record.bytes.0),
                StreamRecord::Terminal(record) => {
                    assert_eq!(record.outcome, Outcome::Exit { code: 0 });
                    return;
                }
                _ => {}
            }
        }
        panic!("old runner must admit and finish the direct job");
    })
    .await
    .unwrap();
    assert_eq!(output, b"old-runner-gate");
    {
        let frames = f.proxy.frames.lock().unwrap();
        let run = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Run(run) if run.job_id == id => Some(run),
                _ => None,
            })
            .unwrap();
        assert!(!run.gate_job, "an old runner must receive no gate mark");
        assert!(serde_json::to_value(run).unwrap().get("gate_job").is_none());
        assert_eq!(run.queue_wait_limit_s, 1800);
    }
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(
            state
                .join("compat/jobs")
                .join(id.to_string())
                .join("record.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(record["request"].get("gate_job").is_none());
    assert_eq!(record["base"], false);
    assert_eq!(record["terminal"]["outcome"]["code"], 0);
    helper.kill().await.unwrap();
    helper.wait().await.unwrap();
}

async fn idle(f: &Fixture) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = f.runner.runner_status();
            if status.queue_depth == 0 && status.running_jobs.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("runner never became idle");
}

#[tokio::test]
async fn current_client_root_and_linked_branch_fill_only_local_default_tip() {
    let f = Fixture::with_runner_config(Cut::None, 28, None, |config| {
        config.base_commands = vec!["test \"$(cat Cargo.lock)\" = '# reviewed lockfile'".into()];
    })
    .await;
    crate::snapshot::git(&f.main, &["branch", "-m", "main"]).unwrap();
    fs::write(f.main.join("Cargo.lock"), "# reviewed lockfile\n").unwrap();
    let tip = commit_checkout(&f.main);
    crate::snapshot::git(&f.main, &["checkout", "-b", "unmerged"]).unwrap();
    fs::write(f.main.join("Cargo.lock"), "# unmerged lockfile\n").unwrap();
    let root_feature = commit_checkout(&f.main);
    fs::write(f.key.join("Cargo.lock"), "# unmerged lockfile\n").unwrap();
    let linked_feature = commit_checkout(&f.key);
    assert_ne!(root_feature, tip);
    assert_ne!(linked_feature, tip);

    let motor = f.motor();
    let mut root_request = f.request("test \"$(cat Cargo.lock)\" = '# unmerged lockfile'");
    root_request.workspace_key = f.main.to_str().unwrap().into();
    root_request.cwd = root_request.workspace_key.clone();
    successful_run(&motor, root_request).await;
    successful_run(
        &motor,
        f.request("test \"$(cat Cargo.lock)\" = '# unmerged lockfile'"),
    )
    .await;
    let prepared = motor
        .prepare(PrepareRequest::new(
            f.key.to_str().unwrap(),
            f.main.to_str().unwrap(),
            linked_feature,
        ))
        .await
        .unwrap();
    assert_eq!(prepared.outcome, PrepareOutcome::Prepared);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if f.jobs()
                .iter()
                .any(|job| job["base"] == true && !job["terminal"].is_null())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("current client never completed a fill");
    idle(&f).await;
    let jobs = f.jobs();
    let writers: Vec<_> = jobs.iter().filter(|job| job["base"] == true).collect();
    assert_eq!(writers.len(), 1, "repeated default tips must coalesce");
    let writer: Run = serde_json::from_value(writers[0]["request"].clone()).unwrap();
    assert_eq!(writer.snapshot_commit, tip);
    assert!(writer.siblings.is_empty());
    assert_eq!(
        fs::read_to_string(std::path::Path::new(&writer.cwd).join("Cargo.lock")).unwrap(),
        "# reviewed lockfile\n"
    );
    let terminal: TerminalRecord = serde_json::from_value(writers[0]["terminal"].clone()).unwrap();
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    let frames = f.proxy.frames.lock().unwrap();
    let runs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Run(run) => Some(run),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.integration_tip == tip));
    assert!(frames
        .iter()
        .any(|frame| matches!(frame, Frame::Prepare(request) if request.integration_tip == tip)));
}

#[tokio::test]
async fn current_client_without_default_branch_sends_empty_tips_and_never_fills() {
    let f = Fixture::new(Cut::None, 28).await;
    crate::snapshot::git(&f.main, &["branch", "-m", "feature"]).unwrap();
    let motor = f.motor();
    successful_run(&motor, f.request("true")).await;
    assert_eq!(f.recorded_run().integration_tip, "");
    let base = crate::snapshot::git(&f.key, &["rev-parse", "HEAD"]).unwrap();
    let prepared = motor
        .prepare(PrepareRequest::new(
            f.key.to_str().unwrap(),
            f.main.to_str().unwrap(),
            base,
        ))
        .await
        .unwrap();
    assert_eq!(prepared.outcome, PrepareOutcome::Prepared);
    assert!(f.proxy.frames.lock().unwrap().iter().any(
        |frame| matches!(frame, Frame::Prepare(request) if request.integration_tip.is_empty())
    ));
    idle(&f).await;
    assert!(f.jobs().iter().all(|job| job["base"] != true));
}

async fn records(mut command: Command) -> Vec<StreamRecord> {
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("old ck-motor hung; no Output/Terminal within 15s")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_stream(records: &[StreamRecord], from: u64) {
    let mut cursor = from;
    for record in records {
        if let StreamRecord::Output(o) = record {
            assert_eq!(
                o.seq, cursor,
                "caller-visible output must be contiguous, starting at its requested cursor"
            );
            assert_eq!(o.truncated_before_seq, None, "Started is not lost output");
            cursor += 1;
        }
    }
    assert!(cursor > from, "real old client must receive Output");
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.outcome == Outcome::Exit { code: 0 })
    );
}

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production client"]
async fn oldest_production_client_receives_output_and_terminal_after_started() {
    let helper = std::env::var_os("MOTOR_COMPAT_CLIENT")
        .expect("run python3 scripts/mixed-version.py to build the real pinned client");
    let f = Fixture::new(Cut::None, 28).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let proxy = f.proxy.clone();
    let bridge = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut upstream = proxy.connect(uuid::Uuid::nil()).await.unwrap();
            tokio::spawn(async move {
                // This bridge only relays bytes between the test socket and the runner connection; it never rewrites frames, so sequences and frame types reach the client unchanged.
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            });
        }
    });
    let config = f.scratch.join("legacy-config.json");
    let request = f.scratch.join("legacy-request.json");
    fs::write(&config, serde_json::to_vec(&f.config).unwrap()).unwrap();
    fs::write(
        &request,
        serde_json::to_vec(&f.request("printf legacy-output; printf legacy-error >&2")).unwrap(),
    )
    .unwrap();
    let invoke = |mode: &str, id: &str, cursor: &str| {
        let mut command = Command::new(&helper);
        command
            .args([&address, config.to_str().unwrap()])
            .arg(f.scratch.join("legacy-state"))
            .args([mode, id, cursor])
            .kill_on_drop(true);
        command
    };
    let live = records(invoke("run", request.to_str().unwrap(), "0")).await;
    let Some(StreamRecord::Terminal(terminal)) = live.last() else {
        panic!("Terminal required")
    };
    let id = terminal.job_id.to_string();
    let jobs = f.jobs();
    assert!(
        jobs.iter()
            .any(|job| job["id"] == id && job["started"].is_object()),
        "the actual job record must retain Started"
    );
    assert_stream(&live, 0);
    for bytes in [b"legacy-output".as_slice(), b"legacy-error".as_slice()] {
        assert!(live
            .iter()
            .any(|record| matches!(record, StreamRecord::Output(o) if o.bytes.0 == bytes)));
    }
    let replay = records(invoke("attach", &id, "0")).await;
    assert_stream(&replay, 0);
    assert_eq!(
        replay,
        live.into_iter()
            .filter(|r| !matches!(r, StreamRecord::Accepted(_)))
            .collect::<Vec<_>>()
    );
    let middle = records(invoke("attach", &id, "1")).await;
    assert_stream(&middle, 1);
    assert_eq!(
        middle,
        replay
            .into_iter()
            .filter(|r| !matches!(r, StreamRecord::Output(o) if o.seq < 1))
            .collect::<Vec<_>>()
    );

    let refusal_request = f.scratch.join("legacy-refusal-request.json");
    let mut oversized = f.request("true");
    oversized.weight_hint = Some(40);
    fs::write(&refusal_request, serde_json::to_vec(&oversized).unwrap()).unwrap();
    let refusal = records(invoke("run", refusal_request.to_str().unwrap(), "0")).await;
    assert!(
        matches!(
            refusal.last(),
            Some(StreamRecord::Terminal(terminal))
                if matches!(
                    &terminal.outcome,
                    Outcome::RefusedBeforeStart {
                        reason: RefusalReason::RunnerFull
                    }
                ) && terminal.ran == Some(Ran::None)
        ),
        "old production client must decode the new runner's refusal: {refusal:?}"
    );

    assert!(
        f.runner.runner_status().health.is_some(),
        "new runner must send health fields"
    );
    let base = crate::snapshot::git(&f.main, &["rev-parse", "HEAD"]).unwrap();
    let prepare_request = f.scratch.join("legacy-prepare-request.json");
    fs::write(
        &prepare_request,
        serde_json::to_vec(&PrepareRequest::new(
            f.key.to_str().unwrap(),
            f.main.to_str().unwrap(),
            base.clone(),
        ))
        .unwrap(),
    )
    .unwrap();
    let prepared = tokio::time::timeout(
        Duration::from_secs(15),
        invoke("prepare", prepare_request.to_str().unwrap(), "").output(),
    )
    .await
    .expect("old client Prepare hung")
    .unwrap();
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let prepared: PrepareReply =
        serde_json::from_slice(&prepared.stdout).expect("old client must return a PrepareReply");
    assert_eq!(prepared.outcome, PrepareOutcome::Prepared);
    assert!(
        f.proxy.frames.lock().unwrap().iter().any(|frame| {
            matches!(frame, Frame::Prepare(request)
            if request.transfer_id == prepared.transfer_id && request.base_commit == base)
        }),
        "the real old client must send Prepare to the runner"
    );
    // Wait for foreground bookkeeping to settle before comparing the old client's idle status.
    let idle = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let s = f.runner.runner_status();
            if s.queue_depth == 0 && s.running_jobs.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(idle.is_ok(), "runner never reached idle status");
    assert!(
        f.jobs().iter().all(|job| job["base"] != true),
        "the pinned production client must never trigger a cache fill"
    );
    let status = tokio::time::timeout(Duration::from_secs(15), invoke("status", "", "").output())
        .await
        .expect("old client status hung")
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["server_reachable"], true);
    assert_eq!(status["queue_depth"], 0);
    assert_eq!(status["running_jobs"], serde_json::json!([]));
    assert!(status["rustc_version"]
        .as_str()
        .is_some_and(|version| !version.is_empty()));
    bridge.abort();
}
