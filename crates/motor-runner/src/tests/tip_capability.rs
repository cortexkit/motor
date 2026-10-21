use super::*;

async fn connect_tip(socket: &Path, capable: bool) -> UnixStream {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let hello = if capable {
        Frame::Hello {
            return_paths: false,
            return_chunked: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: true,
            gate_jobs: false,
            platforms: Vec::new(),
        }
    } else {
        serde_json::from_str(r#"{"type":"hello","protocol_version":1,"started":true}"#).unwrap()
    };
    write_frame_async(&mut stream, &hello).await.unwrap();
    let mut expected = hello;
    if let Frame::Hello { return_paths, .. } = &mut expected {
        *return_paths = true;
    }
    assert_eq!(read_next(&mut stream).await, expected);
    stream
}

async fn request_over_wire(
    socket: &Path,
    run: &Run,
    bytes: &[Vec<u8>],
    capable: bool,
    prepare: bool,
) {
    let mut stream = connect_tip(socket, capable).await;
    if prepare {
        let mut request = prepare_for(run);
        request.bundles = run.bundles.clone();
        write_frame_async(&mut stream, &Frame::Prepare(request.clone()))
            .await
            .unwrap();
        assert_eq!(
            read_next(&mut stream).await,
            Frame::PrepareAccepted {
                transfer_id: request.transfer_id,
            }
        );
        chunks(&mut stream, bytes).await;
        assert_eq!(
            read_next(&mut stream).await,
            Frame::Prepared {
                transfer_id: request.transfer_id,
            }
        );
    } else {
        write_frame_async(&mut stream, &Frame::Run(run.clone()))
            .await
            .unwrap();
        assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
        chunks(&mut stream, bytes).await;
        assert_eq!(
            wire_terminal(&mut stream).await.outcome,
            Outcome::Exit { code: 0 }
        );
        assert_eq!(
            fs::read_to_string(Path::new(&run.workspace_key).join("marker")).unwrap(),
            "ran"
        );
    }
}

fn fill_state(runner: &Runner, root: &str) -> (usize, Option<String>) {
    let inner = runner.core.inner.lock().unwrap();
    (
        inner.jobs.values().filter(|job| job.base).count(),
        inner
            .catalog
            .repositories
            .get(root)
            .and_then(|record| record.desired.as_ref())
            .map(|request| request.tip.clone()),
    )
}

async fn legacy_does_not_feed_fills(prepare: bool) {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (socket, server) = listener(&f, &runner);
    let (run, bytes) = f.run("printf ran > marker");
    request_over_wire(&socket, &run, &bytes, false, prepare).await;
    assert_eq!(fill_state(&runner, &f.root), (0, None));
    assert_eq!(
        git(
            &runner.core.trees.mirror(&f.root),
            &["for-each-ref", "--format=%(objectname)", "refs/tips"]
        ),
        run.integration_tip,
        "legacy tips still retain their existing snapshot history"
    );

    let (capable, bytes) = f.run("printf ran > marker");
    request_over_wire(&socket, &capable, &bytes, true, prepare).await;
    base_done(&runner).await;
    let approved = fill_state(&runner, &f.root);
    assert_eq!(approved, (1, Some(capable.integration_tip.clone())));

    commit(&f.main, "unmerged\n");
    let (legacy, bytes) = f.run("printf ran > marker");
    assert_ne!(legacy.integration_tip, capable.integration_tip);
    request_over_wire(&socket, &legacy, &bytes, false, prepare).await;
    assert_eq!(
        fill_state(&runner, &f.root),
        approved,
        "a legacy request must neither queue a fill nor replace the approved desired tip"
    );
    server.abort();
}

#[tokio::test]
async fn legacy_hello_runs_jobs_without_queuing_or_updating_fills() {
    legacy_does_not_feed_fills(false).await;
}

#[tokio::test]
async fn legacy_hello_prepares_without_queuing_or_updating_fills() {
    legacy_does_not_feed_fills(true).await;
}

#[tokio::test]
async fn capable_hello_queues_fills_from_run_and_prepare_tips() {
    for prepare in [false, true] {
        let f = Fixture::new();
        let runner = Runner::open(f.config()).unwrap();
        let (socket, server) = listener(&f, &runner);
        let (run, bytes) = f.run("printf ran > marker");
        request_over_wire(&socket, &run, &bytes, true, prepare).await;
        assert_eq!(fill_state(&runner, &f.root), (1, Some(run.integration_tip)));
        base_done(&runner).await;
        server.abort();
    }
}

#[tokio::test]
async fn capable_empty_tip_skips_verification_pinning_and_fills() {
    for prepare in [false, true] {
        let f = Fixture::new();
        let runner = Runner::open(f.config()).unwrap();
        let (socket, server) = listener(&f, &runner);
        let (mut run, bytes) = f.run("printf ran > marker");
        run.integration_tip.clear();
        request_over_wire(&socket, &run, &bytes, true, prepare).await;
        assert_eq!(fill_state(&runner, &f.root), (0, None));
        let mirror = runner.core.trees.mirror(&f.root);
        assert!(git(&mirror, &["for-each-ref", "refs/tips"]).is_empty());
        assert_eq!(
            git(
                &mirror,
                &[
                    "rev-parse",
                    &format!("refs/snapshots/{}", run.snapshot_commit)
                ]
            ),
            run.snapshot_commit,
            "the job snapshot must still be pinned when there is no integration tip"
        );
        runner.maintenance().unwrap();
        assert!(git(&mirror, &["for-each-ref", "refs/tips"]).is_empty());
        server.abort();
    }
}
