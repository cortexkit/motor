use super::*;
use crate::{hosted::test_http::Http, transport::Duplex, ShadowConfig};
use cortexkit_test_support::ScratchDir;
use std::{future::Future, pin::Pin, sync::atomic::Ordering};
use tokio::sync::Notify;

struct PrimaryPeer {
    shadow_requested: Option<Arc<Notify>>,
}
impl Connector for PrimaryPeer {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let shadow_requested = self.shadow_requested.clone();
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
                        platforms: vec!["linux".into()],
                        return_paths: true,
                        return_chunked: false,
                        sibling_paths: false,
                    },
                )
                .await
                .unwrap();
                loop {
                    match read_frame_async(&mut peer).await.unwrap() {
                        Some(Frame::Heartbeat { .. }) => {}
                        Some(Frame::Have { repositories, .. }) => write_frame_async(
                            &mut peer,
                            &Frame::HaveReply {
                                repositories,
                                git_dependencies: Some(vec![]),
                            },
                        )
                        .await
                        .unwrap(),
                        Some(Frame::Run(_)) => {
                            write_frame_async(&mut peer, &Frame::Accepted(Accepted::new(id, 0)))
                                .await
                                .unwrap();
                            if let Some(notify) = shadow_requested {
                                notify.notified().await;
                            }
                            for (seq, stream, bytes) in [
                                (0, OutputStream::Stdout, b"primary stdout\n".as_slice()),
                                (1, OutputStream::Stderr, b"primary stderr\n".as_slice()),
                            ] {
                                write_frame_async(
                                    &mut peer,
                                    &Frame::Output(Output::new(
                                        seq,
                                        stream,
                                        BytePayload(bytes.to_vec()),
                                    )),
                                )
                                .await
                                .unwrap();
                            }
                            write_frame_async(
                                &mut peer,
                                &Frame::Terminal(TerminalRecord::new(
                                    id,
                                    Outcome::Exit { code: 17 },
                                    2,
                                    45,
                                    0,
                                )),
                            )
                            .await
                            .unwrap();
                            return;
                        }
                        other => panic!("unexpected primary frame: {other:?}"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}
struct Repository(PathBuf);
impl std::ops::Deref for Repository {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
impl Drop for Repository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn repo(_dir: &Path) -> Repository {
    // Hosted paths are restricted to /Users or /home. The mirrored source tree
    // is under one of these allowed roots; the generic scratch root is not.
    let repo =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(".shadow-fixture-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        vec!["init", "-q", "--initial-branch=main"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    Repository(repo)
}
fn config(broker: &str) -> Config {
    Config {
        runner_instance: "test".into(),
        broker_token_source: crate::BrokerTokenSource::Fake,
        broker_url: Some(broker.into()),
        spill_avoid_host: Some("build-server".into()),
        shadow_to_hosted: ShadowConfig {
            enabled: true,
            sample_rate: 1.0,
            max_concurrent: 2,
        },
        ..Config::default()
    }
}
async fn consume(stream: &mut JobStream) {
    let records = tokio::time::timeout(Duration::from_secs(5), async {
        let mut records = Vec::new();
        while let Some(record) = stream.records.recv().await {
            records.push(record.unwrap());
        }
        records
    })
    .await
    .expect("shadow must not delay the caller");
    assert!(matches!(
        records.as_slice(),
        [
            StreamRecord::Accepted(_),
            StreamRecord::Output(_),
            StreamRecord::Output(_),
            StreamRecord::Terminal(_)
        ]
    ));
    assert!(
        matches!(&records[1], StreamRecord::Output(output) if output.bytes.0 == b"primary stdout\n")
    );
    assert!(
        matches!(&records[2], StreamRecord::Output(output) if output.bytes.0 == b"primary stderr\n")
    );
    assert!(
        matches!(&records[3], StreamRecord::Terminal(t) if t.outcome == Outcome::Exit { code: 17 })
    );
}

#[tokio::test]
async fn shadow_hanging_or_refused_never_changes_callers_stream_or_exit() {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    for hanging in [true, false] {
        let dir = ScratchDir::new("shadow-isolation");
        let repo = repo(&dir);
        let requested = Arc::new(Notify::new());
        let seen = requested.clone();
        let http = Http::new(move |path, _| {
            if path.ends_with("/release") {
                return (404, serde_json::json!({"error":"job_unknown"}));
            }
            assert_eq!(path, "/v1/leases");
            seen.notify_one();
            if hanging {
                (
                    202,
                    serde_json::json!({"status":"provisioning","retry_after_ms":1000}),
                )
            } else {
                (403, serde_json::json!({"error":"shadow_not_allowed"}))
            }
        })
        .await;
        let root = dir.join("state");
        let mut config = config(&http.url);
        config.hosted_repositories_placement_hint =
            Some(crate::motor::repository_routing_tests::control_placement());
        let motor = Motor::with_connector(
            config,
            &root,
            Arc::new(PrimaryPeer {
                shadow_requested: Some(requested),
            }),
        )
        .unwrap();
        let path = repo.to_str().unwrap();
        let mut stream = motor
            .run(RunRequest::new(path, path, path, "exit 17").with_timeout(30))
            .unwrap();
        consume(&mut stream).await;
        if hanging {
            tokio::time::timeout(Duration::from_secs(3), motor.shutdown_shadows())
                .await
                .expect("restart must drop provisioning shadows");
        } else {
            tokio::time::timeout(Duration::from_secs(3), motor.shadows.join_for_test())
                .await
                .unwrap();
            let records = motor.shadow_report(0).unwrap();
            assert_eq!(records.len(), 1);
            assert!(records[0]
                .mismatch
                .iter()
                .any(|m| m == "shadow_refused_before_start"));
            assert!(records[0]
                .shadow_failure
                .as_ref()
                .unwrap()
                .contains("shadow_not_allowed"));
            assert_eq!(
                records[0].build_server.stderr.tail.as_deref(),
                Some("primary stderr\n")
            );
            assert_eq!(records[0].shadow.stderr.tail.as_deref(), Some(""));
        }
        let requests = http.requests.lock().unwrap();
        let leases: Vec<_> = requests.iter().filter(|(p, _)| p == "/v1/leases").collect();
        assert!(!leases.is_empty(), "exercise the actual shadow lease path");
        if !hanging {
            assert_eq!(leases.len(), 1, "403 never retries or falls back");
        }
        assert_eq!(leases[0].1["shadow"], true);
        assert_eq!(leases[0].1["avoid_host"], "build-server");
        assert_eq!(leases[0].1["placement_hint"], serde_json::Value::Null);
        let shadow_id = leases[0].1["job_id"].as_str().unwrap();
        let state = crate::storage::durable_value(&root).unwrap().to_string();
        assert!(
            !state.contains(shadow_id),
            "shadows must not write job state"
        );
        assert!(
            requests.iter().any(|(p, _)| p.ends_with("/release")),
            "restart/refusal must release even a lease whose response was lost"
        );
    }
}

#[tokio::test]
async fn shadow_runner_refusal_reason_and_detail_survive_admission_and_terminal_frames() {
    for after_accept in [false, true] {
        let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
        *boat.run_refusal.lock().unwrap() = Some(crate::hosted::boat_tests::RunRefusal {
            reason: RefusalReason::WorkspaceSetupFailed,
            detail: "workspace setup: C cache is read-only".into(),
            after_accept,
        });
        let dir = ScratchDir::new("shadow-runner-refusal");
        let repo = repo(&dir);
        let motor = Motor::with_connector(
            config(boat.config.broker_url.as_ref().unwrap()),
            &dir.join("state"),
            Arc::new(PrimaryPeer {
                shadow_requested: None,
            }),
        )
        .unwrap();
        let path = repo.to_str().unwrap();
        let mut stream = motor
            .run(RunRequest::new(path, path, path, "exit 17").with_timeout(30))
            .unwrap();
        consume(&mut stream).await;
        tokio::time::timeout(Duration::from_secs(15), motor.shadows.join_for_test())
            .await
            .unwrap();
        assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
        let records = motor.shadow_report(0).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].shadow.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::WorkspaceSetupFailed
            }
        );
        assert_eq!(
            records[0].shadow.refusal_detail.as_deref(),
            Some("workspace setup: C cache is read-only")
        );
        let recorded = crate::shadow::records(&dir.join("state"), 0).unwrap();
        assert_eq!(
            recorded[0].shadow.refusal_detail,
            records[0].shadow.refusal_detail
        );
    }
}

#[tokio::test]
async fn shadow_reuses_pinned_boat_path_and_records_both_tails() {
    let _ = crate::logging::init();
    let boat = crate::hosted::boat_tests::Boat::new(1, false).await;
    let dir = ScratchDir::new("shadow-boat");
    let repo = repo(&dir);
    let root = dir.join("state");
    let motor = Motor::with_connector(
        config(boat.config.broker_url.as_ref().unwrap()),
        &root,
        Arc::new(PrimaryPeer {
            shadow_requested: None,
        }),
    )
    .unwrap();
    let path = repo.to_str().unwrap();
    let mut stream = motor
        .run(RunRequest::new(path, path, path, "exit 17").with_timeout(30))
        .unwrap();
    consume(&mut stream).await;
    tokio::time::timeout(Duration::from_secs(15), motor.shadows.join_for_test())
        .await
        .unwrap();
    assert_eq!(
        boat.runs.load(Ordering::SeqCst),
        1,
        "{:?}; requests={:?}",
        motor.shadow_report(0),
        boat.requests.lock().unwrap()
    );
    assert_eq!(boat.attaches.load(Ordering::SeqCst), 0);
    let records = motor.shadow_report(0).unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.build_server.exit_code, Some(17));
    assert_eq!(record.shadow.exit_code, Some(23));
    assert_eq!(
        record.build_server.stdout.tail.as_deref(),
        Some("primary stdout\n")
    );
    assert_eq!(record.shadow.stdout.tail.as_deref(), Some("same output\n"));
    assert_eq!(record.shadow.stderr.tail.as_deref(), Some(""));
    assert!(boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _)| p.ends_with("/finish")));
    for (_, body) in boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "/v1/leases")
    {
        assert_eq!(body["shadow"], true);
    }
    let frames = boat.frames.lock().unwrap();
    let run = frames
        .iter()
        .find_map(|f| {
            if let Frame::Run(run) = f {
                Some(run)
            } else {
                None
            }
        })
        .unwrap();
    assert!(!run.gate_job);
    assert!(run.return_paths.is_empty());
    assert!(run.integration_tip.is_empty());
    assert_eq!(run.command, "exit 17");
    assert_eq!(run.timeout, Some(30));
    // The request named no vCPUs; the hosted runner refuses a Run whose vCPU
    // count differs from the lease's, so the shadow must send the default.
    assert_eq!(run.weight_hint, Some(2));
}

#[tokio::test]
async fn shadow_deadline_after_booking_finishes_lost_with_zero_runtime() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.hang_run.store(true, Ordering::SeqCst);
    let dir = ScratchDir::new("shadow-deadline-booked");
    let repo = repo(&dir);
    let mut motor = Motor::with_connector(
        config(boat.config.broker_url.as_ref().unwrap()),
        &dir.join("state"),
        Arc::new(PrimaryPeer {
            shadow_requested: None,
        }),
    )
    .unwrap();
    // Expire only the outer deadline. Advancing through the reader's 60-second
    // inactivity window would make the winning error depend on poll order.
    motor.shadow_deadline = Some(Duration::from_secs(30));
    let path = repo.to_str().unwrap();
    let mut stream = motor
        .run(RunRequest::new(path, path, path, "exit 17").with_timeout(30))
        .unwrap();
    consume(&mut stream).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while boat.runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), motor.shadows.join_for_test())
        .await
        .unwrap();
    let requests = boat.requests.lock().unwrap();
    let finish = requests
        .iter()
        .find(|(path, _)| path.ends_with("/finish"))
        .expect("a booked shadow must be settled after its deadline");
    assert_eq!(finish.1, serde_json::json!({"outcome":"lost", "run_ms":0}));
    let records = motor.shadow_report(0).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].shadow_failure.as_deref(),
        Some("shadow deadline exceeded")
    );
}

#[tokio::test]
async fn restart_cancels_running_shadow_finishes_lease_and_never_resumes_it() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.hang_run.store(true, Ordering::SeqCst);
    let dir = ScratchDir::new("shadow-restart");
    let repo = repo(&dir);
    let root = dir.join("state");
    let config = config(boat.config.broker_url.as_ref().unwrap());
    let motor = Motor::with_connector(
        config.clone(),
        &root,
        Arc::new(PrimaryPeer {
            shadow_requested: None,
        }),
    )
    .unwrap();
    let path = repo.to_str().unwrap();
    let mut stream = motor
        .run(RunRequest::new(path, path, path, "exit 17").with_timeout(30))
        .unwrap();
    consume(&mut stream).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while boat.runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), motor.shutdown_shadows())
        .await
        .unwrap();
    assert!(boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _)| p.ends_with("/finish")));
    let finish = boat
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|(path, _)| path.ends_with("/finish"))
        .unwrap()
        .1
        .clone();
    assert_eq!(finish, serde_json::json!({"outcome":"lost", "run_ms":0}));
    drop(stream);
    drop(motor);
    let reopened = Motor::with_connector(
        config,
        &root,
        Arc::new(PrimaryPeer {
            shadow_requested: None,
        }),
    )
    .unwrap();
    reopened.shutdown_shadows().await;
    assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
    assert_eq!(boat.attaches.load(Ordering::SeqCst), 0);
    assert!(boat
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Cancel(_))));
}
