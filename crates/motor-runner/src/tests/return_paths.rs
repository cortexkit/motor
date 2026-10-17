use super::*;

#[tokio::test]
async fn return_paths_result_is_sequenced_before_real_terminal_and_durable_on_attach() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let (mut request, bytes) = f.run("printf x > returned; printf output; exit 23");
    request.return_paths = vec![format!("{}/returned", f.key)];
    request.return_max_bytes = 1;
    f.submit(&runner, &request, &bytes);
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 23 });
    assert_eq!(t.ran, Some(Ran::Remote));
    base_done(&runner).await;
    let frames = runner.replay(request.job_id, 0).unwrap();
    let Frame::ReturnResult(result) = &frames[frames.len() - 2] else {
        panic!("missing result: {frames:?}")
    };
    assert_eq!(result.seq, 2);
    assert_eq!(result.return_paths_failed, None);
    let file = &result.returned_files.as_ref().unwrap()[0];
    assert_eq!(
        file.sha256,
        "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
    );
    assert_eq!(file.data, "eA==");
    assert_eq!(
        runner.replay(request.job_id, result.seq).unwrap(),
        frames[frames.len() - 2..]
    );
    assert!(matches!(
        &runner.replay(request.job_id, result.seq + 1).unwrap()[..],
        [Frame::Terminal(_)]
    ));
    let legacy = runner
        .replay_legacy(request.job_id, result.seq - 1)
        .unwrap();
    assert!(
        matches!(&legacy[..], [Frame::ReturnResult(r), Frame::Terminal(_)] if r.seq == result.seq-1 && r.returned_files == result.returned_files)
    );
    drop(runner);
    let runner = reopen(config);
    assert_eq!(runner.replay(request.job_id, 0).unwrap(), frames);
}

#[tokio::test]
async fn return_paths_cap_failure_preserves_remote_exit() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("printf xy > returned; exit 17");
    request.return_paths = vec![format!("{}/returned", f.key)];
    request.return_max_bytes = 1;
    f.submit(&runner, &request, &bytes);
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 17 });
    assert_eq!(t.ran, Some(Ran::Remote));
    let frames = runner.replay(request.job_id, 0).unwrap();
    let Frame::ReturnResult(result) = &frames[frames.len() - 2] else {
        panic!("missing result")
    };
    assert_eq!(result.returned_files, None);
    assert_eq!(
        result.return_paths_failed,
        Some(ReturnPathsFailed {
            measured_bytes: 2,
            cap_bytes: 1,
            cause: ReturnFailureCause::OverCap
        })
    );
}

#[test]
fn return_paths_invalid_bounds_are_refused_before_admission() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, _) = f.run("touch should-not-run");
    request.return_paths = vec![format!("{}/../outside", f.key)];
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::RequestRejected)
    ));
    let frames = runner.replay(request.job_id, 0).unwrap();
    assert!(
        matches!(&frames[..], [Frame::Terminal(t)] if matches!(t.outcome, Outcome::RefusedBeforeStart { reason: RefusalReason::RequestRejected }) && t.refusal_detail.as_deref().unwrap().contains("return_paths_exceeded"))
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn return_paths_missing_escape_and_frame_limit_keep_real_remote_exit() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let secret = f.scratch.join("outside-file");
    fs::write(&secret, b"private").unwrap();
    let relative = format!("{}/returned", vec!["\u{1}".repeat(100); 34].join("/"));
    for (command, paths, cause, measured) in [
        (
            "exit 17".into(),
            vec![format!("{}/missing", f.key)],
            ReturnFailureCause::Missing,
            0,
        ),
        (
            format!("ln -s '{}' returned-link; exit 17", text(&secret)),
            vec![format!("{}/returned-link", f.key)],
            ReturnFailureCause::Escape,
            0,
        ),
        (
            format!(
                "mkdir -p '{}'; printf x > '{}'; exit 17",
                Path::new(&relative).parent().unwrap().display(),
                relative
            ),
            vec![format!("{}/{}", f.key, relative); 64],
            ReturnFailureCause::FrameLimit,
            64,
        ),
    ] {
        let (mut request, bytes) = f.run(&command);
        request.return_paths = paths;
        f.submit(&runner, &request, &bytes);
        let t = terminal(&runner, request.job_id).await;
        assert_eq!(t.outcome, Outcome::Exit { code: 17 });
        assert_eq!(t.ran, Some(Ran::Remote));
        let result = runner
            .replay(request.job_id, 0)
            .unwrap()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::ReturnResult(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(result.returned_files, None);
        assert_eq!(
            result.return_paths_failed,
            Some(ReturnPathsFailed {
                measured_bytes: measured,
                cap_bytes: RETURN_MAX_BYTES,
                cause
            })
        );
    }
}
