use super::*;
use crate::{
    hosted::{
        boat_tests::committed_repo,
        tests::harness::{Behavior, Harness},
    },
    transport::Duplex,
    Connector, Motor,
};
use motor_protocol::*;
use std::{
    io,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

type ConnectFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Box<dyn Duplex>>> + Send + 'a>>;

#[derive(Clone)]
struct Peer {
    returns: bool,
    platforms: Vec<String>,
    echo: Option<Platform>,
    frames: Arc<Mutex<Vec<Frame>>>,
    visits: Arc<Mutex<Vec<(Uuid, MacosRunner)>>>,
    cancelled: Arc<Mutex<Vec<Uuid>>>,
    starts: Arc<AtomicUsize>,
}
impl Peer {
    fn new() -> Self {
        Self {
            returns: false,
            platforms: vec!["macos".into()],
            echo: Some(Platform::Macos),
            frames: Default::default(),
            visits: Default::default(),
            cancelled: Default::default(),
            starts: Default::default(),
        }
    }
}
impl Connector for Peer {
    fn connect(&self, _id: Uuid) -> ConnectFuture<'_> {
        panic!("macOS must not use the legacy SSH endpoint")
    }
    fn connect_macos<'a>(&'a self, id: Uuid, runner: &'a MacosRunner) -> ConnectFuture<'a> {
        self.visits.lock().unwrap().push((id, runner.clone()));
        let state = self.clone();
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: true,
                        platforms: state.platforms.clone(),
                        return_paths: state.returns,
                        return_chunked_max_bytes: None,
                        return_chunked: state.returns,
                        sibling_paths: true,
                    },
                )
                .await
                .unwrap();
                let mut remaining = 0;
                let mut admitted = false;
                let mut return_request = None;
                while let Ok(Some(frame)) = read_frame_async(&mut peer).await {
                    state.frames.lock().unwrap().push(frame.clone());
                    match frame {
                        Frame::Have { .. } => {
                            write_frame_async(
                                &mut peer,
                                &Frame::HaveReply {
                                    repositories: Vec::new(),
                                    git_dependencies: Some(Vec::new()),
                                },
                            )
                            .await
                            .unwrap();
                        }
                        Frame::Run(run) => {
                            return_request = Some(run.clone());
                            assert_eq!(run.platform, Some(Platform::Macos));
                            remaining = run.bundles.iter().map(|b| b.length).sum::<u64>();
                            assert!(remaining > 0, "the echo gate must protect a real upload");
                            admitted = true;
                            let mut accepted = Accepted::new(id, 0);
                            accepted.platform = state.echo.clone();
                            write_frame_async(&mut peer, &Frame::Accepted(accepted))
                                .await
                                .unwrap();
                        }
                        Frame::BundleChunk { data, .. } => {
                            remaining -= data.0.len() as u64;
                        }
                        Frame::Cancel(_) => {
                            state.cancelled.lock().unwrap().push(id);
                            write_frame_async(&mut peer, &Frame::CancelReply(CancelReply::new(id)))
                                .await
                                .unwrap();
                            return;
                        }
                        Frame::Attach(_) => {
                            assert!(state.cancelled.lock().unwrap().contains(&id));
                            write_frame_async(
                                &mut peer,
                                &Frame::Terminal(TerminalRecord::new(
                                    id,
                                    Outcome::Cancelled,
                                    0,
                                    0,
                                    0,
                                )),
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        Frame::Heartbeat { .. } => {}
                        Frame::Status => {
                            write_frame_async(
                                &mut peer,
                                &Frame::StatusReply(RunnerStatusReply {
                                    status: StatusReply::new(
                                        0,
                                        Vec::new(),
                                        false,
                                        Vec::new(),
                                        "macos",
                                    ),
                                    disk: None,
                                    health: None,
                                }),
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        other => panic!("unexpected macOS frame: {other:?}"),
                    }
                    if admitted && remaining == 0 {
                        assert!(!state.cancelled.lock().unwrap().contains(&id));
                        state.starts.fetch_add(1, Ordering::SeqCst);
                        write_frame_async(
                            &mut peer,
                            &Frame::Started {
                                job_id: id,
                                seq: 0,
                                queue_wait_ms: 0,
                                started_at_ms: 1,
                                path_mapping: None,
                            },
                        )
                        .await
                        .unwrap();
                        let mut terminal_seq = 1;
                        if let Some(run) = return_request
                            .as_ref()
                            .filter(|run: &&Run| !run.return_paths.is_empty())
                        {
                            let sha256 =
                                "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
                            if run.return_chunked {
                                write_frame_async(
                                    &mut peer,
                                    &Frame::ReturnChunk(ReturnChunk::new(
                                        id,
                                        1,
                                        0,
                                        0,
                                        BytePayload(b"x".to_vec()),
                                    )),
                                )
                                .await
                                .unwrap();
                                write_frame_async(
                                    &mut peer,
                                    &Frame::ReturnManifest(ReturnManifest::new(
                                        id,
                                        2,
                                        vec![ReturnManifestEntry::returned(
                                            run.return_paths[0].clone(),
                                            1,
                                            sha256,
                                        )
                                        .with_chunks(1)],
                                        1,
                                        run.return_max_bytes,
                                    )),
                                )
                                .await
                                .unwrap();
                                terminal_seq = 3;
                            } else {
                                write_frame_async(
                                    &mut peer,
                                    &Frame::ReturnResult(ReturnResult {
                                        job_id: id,
                                        seq: 1,
                                        returned_files: Some(vec![ReturnedFile {
                                            path: run.return_paths[0].clone(),
                                            sha256: sha256.into(),
                                            data: "eA==".into(),
                                        }]),
                                        return_paths_failed: None,
                                    }),
                                )
                                .await
                                .unwrap();
                                terminal_seq = 2;
                            }
                        }
                        write_frame_async(
                            &mut peer,
                            &Frame::Terminal(TerminalRecord::new(
                                id,
                                Outcome::Signal { signal: 11 },
                                terminal_seq,
                                0,
                                0,
                            )),
                        )
                        .await
                        .unwrap();
                        return;
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

fn request(repo: &Path) -> RunRequest {
    crate::cli::build_request(&crate::cli::RunArgs {
        cwd: Some(repo.into()),
        command: vec!["kill -SEGV $$".into()],
        timeout: 30,
        platform: Some(Platform::Macos),
        ..Default::default()
    })
    .unwrap()
}
async fn records(mut stream: crate::JobStream) -> Vec<StreamRecord> {
    tokio::time::timeout(Duration::from_secs(20), async move {
        let mut records = Vec::new();
        while let Some(record) = stream.records.recv().await {
            let record = record.unwrap();
            let done = matches!(record, StreamRecord::Terminal(_));
            records.push(record);
            if done {
                return records;
            }
        }
        panic!("stream ended without a terminal")
    })
    .await
    .expect("macOS test timed out")
}
fn terminal(records: &[StreamRecord]) -> &TerminalRecord {
    let Some(StreamRecord::Terminal(terminal)) = records.last() else {
        panic!("no terminal")
    };
    terminal
}
fn no_placement(h: &Harness) {
    assert!(
        h.lease_bodies.lock().unwrap().is_empty(),
        "macOS reached hosted placement"
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn macos_routes_directly_despite_all_hosted_settings_and_does_not_reserve_local_slots() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    for mode in 0..4 {
        let peer = Arc::new(Peer::new());
        let config = Config {
            macos_runner: Some(runner()),

            hosted_repositories: if mode == 1 || mode == 3 {
                vec![repo.to_str().unwrap().into()]
            } else {
                vec![]
            },

            ..h.config()
        };
        let endpoint = config.macos_runner.clone().unwrap();
        let motor = Motor::with_connector(
            config,
            &scratch.0.join(format!("state-{mode}")),
            peer.clone(),
        )
        .unwrap();
        // The runner, not ck-motor, owns macOS admission. More than two concurrent
        // submissions must reach it rather than a local capacity refusal.
        let mut streams = Vec::new();
        for _ in 0..4 {
            streams.push(motor.run(request(&repo)).unwrap());
        }
        for stream in streams {
            let id = stream.job_id;
            let records = records(stream).await;
            assert!(
                matches!(records.first(), Some(StreamRecord::Accepted(a)) if a.platform == Some(Platform::Macos))
            );
            assert_eq!(terminal(&records).outcome, Outcome::Signal { signal: 11 });
            assert_eq!(crate::cli::exit_code(terminal(&records)), 139);
            assert!(motor.hosted_state(id).is_none());
        }
        assert_eq!(peer.starts.load(Ordering::SeqCst), 4);
        assert!(peer
            .visits
            .lock()
            .unwrap()
            .iter()
            .all(|(_, r)| r == &endpoint));
        assert!(!peer
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Frame::Status)));
        motor.close().await;
        no_placement(&h);
    }
}

#[tokio::test]
async fn macos_hosted_test_is_refused_without_placement_or_direct_connection() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(Peer::new());
    let config = Config {
        macos_runner: Some(runner()),

        hosted_repositories: vec![repo.to_str().unwrap().into()],
        ..h.config()
    };
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    let records = records(
        motor
            .run_direct_hosted(request(&repo), None, crate::ReturnOptions::default())
            .unwrap(),
    )
    .await;
    assert_eq!(records.len(), 1);
    let terminal = terminal(&records);
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    assert_eq!(terminal.refusal_detail.as_deref(), Some("macos_not_hosted"));
    assert!(peer.visits.lock().unwrap().is_empty());
    motor.close().await;
    no_placement(&h);
}

#[tokio::test]
async fn macos_missing_config_and_network_refuse_before_connecting() {
    let (scratch, repo) = committed_repo();
    for (name, endpoint, network, reason, detail) in [
        (
            "missing",
            None,
            false,
            RefusalReason::PlatformUnsupported,
            "requested platform macos is not configured",
        ),
        (
            "network",
            Some(runner()),
            true,
            RefusalReason::NetworkUnsupported,
            "outbound network",
        ),
    ] {
        let peer = Arc::new(Peer::new());
        let config = Config {
            runner_instance: "test".into(),
            macos_runner: endpoint,
            network_routing: true,
            ..Default::default()
        };
        let motor = Motor::with_connector(config, &scratch.0.join(name), peer.clone()).unwrap();
        let mut request = request(&repo);
        if network {
            request.network = Some(Network::Outbound);
        }
        let records = records(motor.run(request).unwrap()).await;
        assert_eq!(records.len(), 1);
        let terminal = terminal(&records);
        assert_eq!(terminal.outcome, Outcome::RefusedBeforeStart { reason });
        assert!(terminal.refusal_detail.as_deref().unwrap().contains(detail));
        assert!(peer.visits.lock().unwrap().is_empty());
        motor.close().await;
    }
}

#[tokio::test]
async fn macos_requires_hello_support_before_have_or_run() {
    let (scratch, repo) = committed_repo();
    for (index, platforms) in [vec![], vec!["linux".into()], vec!["windows".into()]]
        .into_iter()
        .enumerate()
    {
        let peer = Arc::new(Peer {
            platforms,
            ..Peer::new()
        });
        let config = Config {
            runner_instance: "test".into(),
            macos_runner: Some(runner()),
            ..Default::default()
        };
        let motor = Motor::with_connector(
            config,
            &scratch.0.join(format!("state-{index}")),
            peer.clone(),
        )
        .unwrap();
        let records = records(motor.run(request(&repo)).unwrap()).await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            terminal(&records).outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::PlatformUnsupported
            }
        );
        assert!(terminal(&records)
            .refusal_detail
            .as_deref()
            .unwrap()
            .contains("macos is not listed"));
        assert!(!peer
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Frame::Have { .. } | Frame::Run(_))));
        motor.close().await;
    }
}

#[tokio::test]
async fn macos_wrong_or_absent_echo_cancels_before_any_upload_or_start() {
    let (scratch, repo) = committed_repo();
    for (index, echo) in [None, Some(Platform::Linux), Some(Platform::Windows)]
        .into_iter()
        .enumerate()
    {
        let peer = Arc::new(Peer {
            echo,
            ..Peer::new()
        });
        let config = Config {
            runner_instance: "test".into(),
            macos_runner: Some(runner()),
            ..Default::default()
        };
        let motor = Motor::with_connector(
            config,
            &scratch.0.join(format!("state-{index}")),
            peer.clone(),
        )
        .unwrap();
        let records = records(motor.run(request(&repo)).unwrap()).await;
        assert!(matches!(records.first(), Some(StreamRecord::Accepted(_))));
        assert_eq!(terminal(&records).outcome, Outcome::Cancelled);
        assert_eq!(peer.starts.load(Ordering::SeqCst), 0);
        assert_eq!(peer.cancelled.lock().unwrap().len(), 1);
        assert!(!peer
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Frame::BundleChunk { .. })));
        assert!(
            peer.visits.lock().unwrap().len() >= 3,
            "submit, cancel and follow must all route to macOS"
        );
        motor.close().await;
    }
}

#[tokio::test]
async fn macos_return_paths_are_refused_before_run() {
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(Peer::new());
    let config = Config {
        runner_instance: "test".into(),
        macos_runner: Some(runner()),
        ..Default::default()
    };
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    let returns = crate::ReturnOptions {
        return_paths: vec![repo.join("result").to_str().unwrap().into()],
        ..Default::default()
    };
    let records = records(motor.run_with_returns(request(&repo), returns).unwrap()).await;
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    );
    assert_eq!(
        terminal(&records).refusal_detail.as_deref(),
        Some("return_paths_unsupported")
    );
    assert!(!peer
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Run(_))));
    motor.close().await;
}

#[tokio::test]
async fn macos_capable_peer_returns_legacy_and_chunked_files() {
    for chunked in [false, true] {
        let (scratch, repo) = committed_repo();
        let peer = Arc::new(Peer {
            returns: true,
            ..Peer::new()
        });
        let config = Config {
            runner_instance: "test".into(),
            macos_runner: Some(runner()),
            ..Default::default()
        };
        let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
        let returns = crate::ReturnOptions {
            return_paths: vec![repo.join("result").to_str().unwrap().into()],
            return_chunked: chunked,
            ..Default::default()
        };
        let records = records(motor.run_with_returns(request(&repo), returns).unwrap()).await;
        assert_eq!(terminal(&records).outcome, Outcome::Signal { signal: 11 });
        assert!(peer.frames.lock().unwrap().iter().any(|frame| matches!(frame, Frame::Run(run) if run.return_chunked == chunked && run.return_paths.len() == 1)));
        if chunked {
            assert!(records
                .iter()
                .any(|record| matches!(record, StreamRecord::ReturnChunk(_))));
            assert!(records
                .iter()
                .any(|record| matches!(record, StreamRecord::ReturnManifest(_))));
        } else {
            let result = motor.return_result(terminal(&records).job_id).unwrap();
            assert_eq!(result.return_paths_failed, None);
            let files = result.returned_files.unwrap();
            assert_eq!(files[0].path, repo.join("result").to_str().unwrap());
            assert_eq!(files[0].data, "eA==");
            assert_eq!(
                files[0].sha256,
                "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
            );
        }
        motor.close().await;
    }
}

#[tokio::test]
async fn macos_status_stays_direct_when_linux_uses_hosted_status() {
    let (scratch, _) = committed_repo();
    let peer = Arc::new(Peer::new());
    let config = Config {
        runner_instance: "test".into(),
        macos_runner: Some(runner()),

        ..Default::default()
    };
    let motor = Motor::with_connector(config, &scratch.0.join("state"), peer.clone()).unwrap();
    assert_eq!(
        motor
            .runner_status_for(None)
            .await
            .unwrap()
            .status
            .rustc_version,
        "hosted"
    );
    assert!(peer.visits.lock().unwrap().is_empty());
    assert_eq!(
        motor
            .runner_status_for(Some(&Platform::Macos))
            .await
            .unwrap()
            .status
            .rustc_version,
        "macos"
    );
    assert_eq!(peer.visits.lock().unwrap().len(), 1);
    motor.close().await;
}
