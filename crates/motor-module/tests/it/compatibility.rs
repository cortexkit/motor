use super::recovery::{Cut, Fixture, Proxy};
use crate::{Connector, Duplex};
use motor_protocol::*;
use std::{
    fs,
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::{net::TcpListener, process::Command};

struct RunnerHello {
    proxy: Arc<Proxy>,
    frame: Arc<Mutex<Option<Frame>>>,
    legacy_reply: bool,
}

fn legacy_hello() -> Frame {
    serde_json::from_value(serde_json::json!({
        "type": "hello",
        "protocol_version": PROTOCOL_VERSION,
    }))
    .unwrap()
}

fn runner_gate_jobs(hello: &Mutex<Option<Frame>>) -> bool {
    let Some(Frame::Hello { gate_jobs, .. }) = *hello.lock().unwrap() else {
        panic!("runner Hello must be observed on the direct job connection");
    };
    gate_jobs
}

impl Connector for RunnerHello {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async move {
            let upstream = self.proxy.connect(id).await?;
            let (client, bridge) = tokio::io::duplex(262144);
            let hello = self.frame.clone();
            let legacy_reply = self.legacy_reply;
            tokio::spawn(async move {
                let (mut cr, mut cw) = tokio::io::split(bridge);
                let (mut sr, mut sw) = tokio::io::split(upstream);
                let up = tokio::io::copy(&mut cr, &mut sw);
                let down = async {
                    let mut frame = read_frame_async(&mut sr)
                        .await?
                        .ok_or_else(|| io::Error::other("pinned runner closed before Hello"))?;
                    if !matches!(frame, Frame::Hello { .. }) {
                        return Err(io::Error::other("pinned runner did not send Hello first"));
                    }
                    // Ordinary legacy tests replace only the runner's capability advertisement.
                    if legacy_reply {
                        frame = legacy_hello();
                    }
                    // The pinned-runner test observes and relays its real Hello unchanged.
                    *hello.lock().unwrap() = Some(frame.clone());
                    write_frame_async(&mut cw, &frame).await?;
                    cw.flush().await?;
                    tokio::io::copy(&mut sr, &mut cw).await.map(|_| ())
                };
                tokio::select! {
                    result = up => result.map(|_| ()),
                    result = down => result,
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

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

async fn refuse_non_neighbour_before_transfer(motor: &crate::Motor, f: &Fixture) {
    let path = f.scratch.join("side-checkouts/commons");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    crate::snapshot::git(
        path.parent().unwrap(),
        &["clone", "-q", f.sibling.to_str().unwrap(), "commons"],
    )
    .unwrap();
    f.proxy.frames.lock().unwrap().clear();
    let mut request = f.request("touch should-not-run");
    request.siblings = vec![path.to_str().unwrap().into()];
    let records = super::recovery::collect(motor.run(request).unwrap()).await;
    let terminal = super::recovery::terminal(&records);
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceKeyRejected
        }
    ));
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("does not advertise sibling_paths"));
    let reply = motor.prepare(serde_json::from_value(serde_json::json!({
        "workspace_key": f.key, "repository_root": f.main, "base_commit": "HEAD", "siblings": [path]
    })).unwrap()).await.unwrap();
    assert!(matches!(
        reply.outcome,
        PrepareOutcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceKeyRejected
        }
    ));
    assert!(reply
        .refusal_detail()
        .unwrap()
        .contains("does not advertise sibling_paths"));
    assert!(!f.proxy.frames.lock().unwrap().iter().any(|frame| matches!(
        frame,
        Frame::Run(_) | Frame::Prepare(_) | Frame::Have { .. } | Frame::BundleChunk { .. }
    )));
    assert!(!f.remote_key.join("should-not-run").exists());
}

#[tokio::test]
async fn legacy_runner_refuses_non_neighbour_siblings_before_run_and_prepare() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = crate::Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("module-state"),
        Arc::new(RunnerHello {
            proxy: f.proxy.clone(),
            frame: Arc::new(Mutex::new(None)),
            legacy_reply: true,
        }),
    )
    .unwrap();
    refuse_non_neighbour_before_transfer(&motor, &f).await;
}

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production runner"]
async fn new_direct_client_negotiates_gate_mark_with_pinned_runner() {
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
    let hello = Arc::new(Mutex::new(None));
    let motor = crate::Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("module-state"),
        Arc::new(RunnerHello {
            proxy: f.proxy.clone(),
            frame: hello.clone(),
            legacy_reply: false,
        }),
    )
    .unwrap();
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
    let gate_jobs = runner_gate_jobs(&hello);
    {
        let frames = f.proxy.frames.lock().unwrap();
        let run = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Run(run) if run.job_id == id => Some(run),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            run.gate_job, gate_jobs,
            "gate mark must follow runner Hello"
        );
        let wire = serde_json::to_value(run).unwrap();
        if gate_jobs {
            assert_eq!(wire["gate_job"], true);
        } else {
            assert!(wire.get("gate_job").is_none());
        }
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
    if gate_jobs {
        assert_eq!(record["request"]["gate_job"], true);
    } else {
        assert!(record["request"].get("gate_job").is_none());
    }
    assert_eq!(record["base"], false);
    assert_eq!(record["terminal"]["outcome"]["code"], 0);
    refuse_non_neighbour_before_transfer(&motor, &f).await;
    helper.kill().await.unwrap();
    helper.wait().await.unwrap();
}

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production runner"]
async fn new_agent_chunked_return_is_refused_by_pinned_runner_before_run() {
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
    let hello = Arc::new(Mutex::new(None));
    let motor = crate::Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("module-state"),
        Arc::new(RunnerHello {
            proxy: f.proxy.clone(),
            frame: hello.clone(),
            legacy_reply: false,
        }),
    )
    .unwrap();
    // An agent asking for chunked returned files from a runner that predates
    // them is refused with a clear reason, before any Run frame is sent.
    let mut request = f
        .request("touch should-not-run")
        .with_return_paths(vec!["out".into()])
        .with_return_chunked(true);
    let returns = crate::agent_route::take_return_request(&mut request).unwrap();
    let records = super::recovery::collect(motor.run_agent(request, None, returns).unwrap()).await;
    let terminal = super::recovery::terminal(&records);
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    ));
    let Some(Frame::Hello {
        return_paths,
        return_chunked,
        ..
    }) = hello.lock().unwrap().clone()
    else {
        panic!("runner Hello must be observed")
    };
    assert!(!return_chunked, "the pinned runner predates chunked return");
    // A runner that predates returned files altogether is refused for that
    // first; one that has them but not chunking, for chunking.
    let expected = if return_paths {
        "return_chunked_unsupported"
    } else {
        "return_paths_unsupported"
    };
    assert_eq!(terminal.refusal_detail.as_deref(), Some(expected));
    assert!(!f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| matches!(frame, Frame::Run(_))));
    assert!(!f.remote_key.join("should-not-run").exists());
    helper.kill().await.unwrap();
    helper.wait().await.unwrap();
}

#[tokio::test]
async fn legacy_runner_hello_omits_direct_gate_mark() {
    let f = Fixture::new(Cut::None, 28).await;
    let hello = Arc::new(Mutex::new(None));
    let motor = crate::Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("module-state"),
        Arc::new(RunnerHello {
            proxy: f.proxy.clone(),
            frame: hello.clone(),
            legacy_reply: true,
        }),
    )
    .unwrap();
    let records = super::recovery::collect(
        motor
            .run_direct(f.request("printf legacy-runner-gate"), None)
            .unwrap(),
    )
    .await;
    let terminal = super::recovery::terminal(&records);
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    let output: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            StreamRecord::Output(o) => Some(o.bytes.0.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(output, b"legacy-runner-gate");
    let gate_jobs = runner_gate_jobs(&hello);
    assert!(!gate_jobs, "legacy Hello must omit gate_jobs");
    let frames = f.proxy.frames.lock().unwrap();
    let run = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Run(run) if run.job_id == terminal.job_id => Some(run),
            _ => None,
        })
        .unwrap();
    assert_eq!(run.gate_job, gate_jobs);
    assert!(serde_json::to_value(run).unwrap().get("gate_job").is_none());
    assert_eq!(run.queue_wait_limit_s, 1800);
    let job = f
        .jobs()
        .into_iter()
        .find(|job| job["id"] == terminal.job_id.to_string())
        .unwrap();
    assert!(job["request"].get("gate_job").is_none());
    assert_eq!(job["base"], false);
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
async fn current_client_root_and_linked_branch_fill_only_remote_confirmed_default_tip() {
    let f = Fixture::with_runner_config(Cut::None, 28, None, |config| {
        config.base_commands = vec!["test \"$(cat Cargo.lock)\" = '# reviewed lockfile'".into()];
    })
    .await;
    crate::snapshot::git(&f.main, &["branch", "-m", "main"]).unwrap();
    fs::write(f.main.join("Cargo.lock"), "# reviewed lockfile\n").unwrap();
    let tip = commit_checkout(&f.main);
    let remote = f.scratch.join("origin.git");
    crate::snapshot::git(
        &f.main,
        &[
            "clone",
            "--bare",
            "-q",
            f.main.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    )
    .unwrap();
    crate::snapshot::git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
    crate::snapshot::git(
        &f.main,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    )
    .unwrap();
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

#[tokio::test]
async fn current_client_unverified_default_tip_runs_and_prepares_without_fills() {
    let f = Fixture::new(Cut::None, 28).await;
    crate::snapshot::git(&f.main, &["branch", "-m", "main"]).unwrap();
    let remote = f.scratch.join("origin.git");
    crate::snapshot::git(
        &f.main,
        &[
            "clone",
            "--bare",
            "-q",
            f.main.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    )
    .unwrap();
    crate::snapshot::git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
    crate::snapshot::git(
        &f.main,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    )
    .unwrap();
    fs::write(f.main.join("Cargo.lock"), "# unmerged lockfile\n").unwrap();
    let ahead = commit_checkout(&f.main);
    crate::snapshot::git(&f.main, &["update-ref", "refs/remotes/origin/main", &ahead]).unwrap();
    let motor = f.motor();
    let base = crate::snapshot::git(&f.key, &["rev-parse", "HEAD"]).unwrap();
    for offline in [false, true] {
        if offline {
            fs::rename(&remote, remote.with_extension("offline")).unwrap();
        }
        successful_run(&motor, f.request("true")).await;
        assert_eq!(f.recorded_run().integration_tip, "");
        let prepared = motor
            .prepare(PrepareRequest::new(
                f.key.to_str().unwrap(),
                f.main.to_str().unwrap(),
                base.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(prepared.outcome, PrepareOutcome::Prepared);
    }
    {
        let frames = f.proxy.frames.lock().unwrap();
        let prepares: Vec<_> = frames
            .iter()
            .filter_map(|frame| match frame {
                Frame::Prepare(request) => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(prepares.len(), 2);
        assert!(prepares
            .iter()
            .all(|request| request.integration_tip.is_empty()));
    }
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

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production client"]
async fn oldest_production_client_reads_current_module_store() {
    let helper = std::env::var_os("MOTOR_COMPAT_CLIENT")
        .expect("run python3 scripts/mixed-version.py to build the real pinned client");
    let f = Fixture::new(Cut::None, 0).await;
    let motor = f.motor();
    let live = super::recovery::collect(
        motor
            .refuse_before_start(RefusalReason::RunnerFull, None)
            .unwrap(),
    )
    .await;
    let terminal = super::recovery::terminal(&live);
    motor.shutdown_storage().unwrap();
    drop(motor);

    let config = f.scratch.join("rollback-config.json");
    fs::write(&config, serde_json::to_vec(&f.config).unwrap()).unwrap();
    let mut command = Command::new(helper);
    // Replaying a stored local refusal must not need a runner connection.
    command
        .args(["127.0.0.1:1", config.to_str().unwrap()])
        .arg(f.scratch.join("module-state"))
        .args(["attach", &terminal.job_id.to_string(), "0"])
        .kill_on_drop(true);
    let replay = records(command).await;
    assert_eq!(super::recovery::terminal(&replay), terminal);
}

fn assert_stream(records: &[StreamRecord], from: u64, started: bool) {
    let mut cursor = from;
    let mut started_count = 0;
    let mut output_count = 0;
    for record in records {
        match record {
            StreamRecord::Started(s) => {
                assert!(started, "Started requires client Hello support");
                assert_eq!(from, 0, "Started must not precede the replay cursor");
                assert_eq!(s.seq, 0, "Started must be the first sequenced record");
                assert_eq!(cursor, 0, "Started must precede output");
                started_count += 1;
                cursor += 1;
            }
            StreamRecord::Output(o) => {
                assert_eq!(
                    o.seq, cursor,
                    "caller-visible records must be contiguous, starting at the requested cursor"
                );
                assert_eq!(o.truncated_before_seq, None, "Started is not lost output");
                output_count += 1;
                cursor += 1;
            }
            _ => {}
        }
    }
    assert_eq!(started_count, usize::from(started && from == 0));
    assert!(output_count > 0, "real pinned client must receive Output");
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.outcome == Outcome::Exit { code: 0 })
    );
}

fn client_started(f: &Fixture) -> bool {
    f.proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|frame| match frame {
            Frame::Hello { started, .. } => Some(*started),
            _ => None,
        })
        .expect("pinned client Hello must be observed on the job connection")
}

#[tokio::test]
async fn legacy_client_hello_projects_started_and_replay_cursors() {
    let f = Fixture::new(Cut::None, 28).await;
    let native = super::recovery::collect(
        f.motor()
            .run(f.request("printf legacy-output; printf legacy-error >&2"))
            .unwrap(),
    )
    .await;
    assert!(native
        .iter()
        .any(|record| matches!(record, StreamRecord::Started(s) if s.seq == 0)));
    let terminal = super::recovery::terminal(&native);
    let mut replays = Vec::new();
    for cursor in [0, 1] {
        let mut peer = f.proxy.connect(terminal.job_id).await.unwrap();
        write_frame_async(&mut peer, &legacy_hello()).await.unwrap();
        // The runner's writer can send its first heartbeat before the Hello reply.
        let reply = loop {
            match read_frame_async(&mut peer).await.unwrap() {
                Some(Frame::Heartbeat { .. }) => continue,
                other => break other,
            }
        };
        assert!(
            matches!(reply, Some(Frame::Hello { .. })),
            "expected the runner's Hello reply, got {reply:?}"
        );
        write_frame_async(
            &mut peer,
            &Frame::Attach(AttachRequest::new(terminal.job_id, cursor)),
        )
        .await
        .unwrap();
        let replay = tokio::time::timeout(Duration::from_secs(15), async {
            let mut records = Vec::new();
            loop {
                match read_frame_async(&mut peer).await.unwrap().unwrap() {
                    Frame::Output(o) => records.push(StreamRecord::Output(o)),
                    Frame::Terminal(t) => {
                        records.push(StreamRecord::Terminal(t));
                        break records;
                    }
                    Frame::Heartbeat { .. } => {}
                    other => panic!("unexpected legacy replay frame: {other:?}"),
                }
            }
        })
        .await
        .expect("legacy replay hung");
        assert!(!client_started(&f), "legacy Hello must omit started");
        assert_stream(&replay, cursor, client_started(&f));
        assert_eq!(super::recovery::terminal(&replay), terminal);
        replays.push(replay);
    }
    for bytes in [b"legacy-output".as_slice(), b"legacy-error".as_slice()] {
        assert!(replays[0]
            .iter()
            .any(|record| matches!(record, StreamRecord::Output(o) if o.bytes.0 == bytes)));
    }
    assert_eq!(
        replays[1],
        replays[0]
            .iter()
            .filter(|r| !matches!(r, StreamRecord::Output(o) if o.seq < 1))
            .cloned()
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production client"]
async fn oldest_production_client_receives_output_and_terminal_after_started() {
    let helper = std::env::var_os("MOTOR_COMPAT_CLIENT")
        .expect("run python3 scripts/mixed-version.py to build the real pinned client");
    let f = Fixture::new(Cut::None, 28).await;
    // A client that supports cache fills sends the repository's default-branch tip,
    // and the runner then queues a cache fill job. Renaming the only branch to
    // `feature` leaves no main, master or origin/HEAD, so the pinned client sends no
    // tip whatever its age, and the no-fill check at the end stays meaningful.
    crate::snapshot::git(&f.main, &["branch", "-m", "feature"]).unwrap();
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
    assert_stream(&live, 0, client_started(&f));
    for bytes in [b"legacy-output".as_slice(), b"legacy-error".as_slice()] {
        assert!(live
            .iter()
            .any(|record| matches!(record, StreamRecord::Output(o) if o.bytes.0 == bytes)));
    }
    let replay = records(invoke("attach", &id, "0")).await;
    assert_stream(&replay, 0, client_started(&f));
    assert_eq!(
        replay,
        live.into_iter()
            .filter(|r| !matches!(r, StreamRecord::Accepted(_)))
            .collect::<Vec<_>>()
    );
    let middle = records(invoke("attach", &id, "1")).await;
    assert_stream(&middle, 1, client_started(&f));
    assert_eq!(
        middle,
        replay
            .into_iter()
            .filter(|r| match r {
                StreamRecord::Started(s) => s.seq >= 1,
                StreamRecord::Output(o) => o.seq >= 1,
                _ => true,
            })
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
    let Some(StreamRecord::Terminal(terminal)) = refusal.last() else {
        panic!("refusal Terminal required");
    };
    let detail = terminal.refusal_detail().expect("terminal refusal detail");
    assert!(detail.contains("vcpu request 40"), "{detail}");
    assert!(
        detail.contains("outside the supported range 1..="),
        "{detail}"
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
        "without a default-branch tip, the pinned client must not trigger a cache fill"
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

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production client"]
async fn oldest_production_client_keeps_typed_queue_and_setup_refusal_details() {
    let helper = std::env::var_os("MOTOR_COMPAT_CLIENT")
        .expect("run python3 scripts/mixed-version.py to build the real pinned client");
    for setup_failure in [false, true] {
        let f = Fixture::new(Cut::None, if setup_failure { 28 } else { 0 }).await;
        crate::snapshot::git(&f.main, &["branch", "-m", "feature"]).unwrap();
        if setup_failure {
            fs::create_dir_all(f.remote_key.parent().unwrap()).unwrap();
            fs::write(&f.remote_key, "not a directory").unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let proxy = f.proxy.clone();
        let bridge = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut upstream = proxy.connect(Uuid::nil()).await.unwrap();
                tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
                });
            }
        });
        let config = f.scratch.join("legacy-config.json");
        let request_path = f.scratch.join("legacy-request.json");
        fs::write(&config, serde_json::to_vec(&f.config).unwrap()).unwrap();
        let mut request = f.request("echo forbidden > typed-refusal-marker");
        if !setup_failure {
            request.queue_wait_limit_s = Some(0);
        }
        fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
        let invoke = |mode: &str, argument: &str| {
            let mut command = Command::new(&helper);
            command
                .args([&address, config.to_str().unwrap()])
                .arg(f.scratch.join("legacy-state"))
                .args([mode, argument, "0"])
                .kill_on_drop(true);
            command
        };
        let live = records(invoke("run", request_path.to_str().unwrap())).await;
        let terminal = super::recovery::terminal(&live);
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart {
                reason: if setup_failure {
                    RefusalReason::WorkspaceSetupFailed
                } else {
                    RefusalReason::QueueWaitExceeded
                }
            }
        );
        let detail = terminal
            .refusal_detail()
            .expect("old client lost refusal detail");
        if setup_failure {
            assert!(
                detail.starts_with("prepare job workspace: populate workspace:"),
                "{detail}"
            );
            assert!(detail.contains("destination path"), "{detail}");
        } else {
            assert!(detail.starts_with("waiting_on="), "{detail}");
            assert!(detail.contains("held_permits=0/1"), "{detail}");
        }
        assert_eq!(terminal.ran, Some(Ran::None));
        assert_eq!(terminal.retry_after_ms(), None);
        assert!(!live
            .iter()
            .any(|record| matches!(record, StreamRecord::Started(_) | StreamRecord::Output(_))));
        assert!(!f.remote_key.join("typed-refusal-marker").exists());
        let replay = records(invoke("attach", &terminal.job_id.to_string())).await;
        assert_eq!(super::recovery::terminal(&replay), terminal);
        bridge.abort();
    }
}

#[tokio::test]
#[ignore = "dedicated CI gate: python3 scripts/mixed-version.py builds the pinned production client"]
async fn oldest_production_client_decodes_new_refusal_tags_as_unknown_with_detail() {
    let helper =
        std::env::var_os("MOTOR_COMPAT_CLIENT").expect("run python3 scripts/mixed-version.py");
    let f = Fixture::new(Cut::None, 28).await;
    for (reason, tag, detail, retry_after_ms) in [
        (
            RefusalReason::LostToRestart,
            "lost_to_restart",
            "runner restarted before the job could start",
            None,
        ),
        (
            RefusalReason::RequestRejected,
            "request_rejected",
            "prepare_unsupported: Prepare is unsupported by the Windows instance",
            None,
        ),
        (
            RefusalReason::RunnerAuthenticationFailed,
            "runner_authentication_failed",
            "runner_sign_missing",
            None,
        ),
        // A transient signing failure carries a retry hint, which the oldest
        // client must read as written.
        (
            RefusalReason::RunnerAuthenticationFailed,
            "runner_authentication_failed",
            "runner_sign_unavailable",
            Some(1500),
        ),
    ] {
        let path = f.scratch.join(format!("{tag}-{detail}.json"));
        let mut terminal = TerminalRecord::new(
            Uuid::now_v7(),
            Outcome::RefusedBeforeStart { reason },
            0,
            0,
            0,
        )
        .with_refusal_detail(detail)
        .with_ran(Ran::None);
        terminal.retry_after_ms = retry_after_ms;
        fs::write(
            &path,
            serde_json::to_vec(&StreamRecord::Terminal(terminal)).unwrap(),
        )
        .unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(&helper)
                .arg("--decode-refusal")
                .arg(&path)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let old: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            old["unknown"], true,
            "the pinned helper must use its own older reason enum"
        );
        assert_eq!(old["reason"], tag);
        assert_eq!(old["refusal_detail"], detail);
        assert_eq!(old["ran"], "none");
        assert_eq!(old["retry_after_ms"], serde_json::json!(retry_after_ms));
    }
}
