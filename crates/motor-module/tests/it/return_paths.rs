use super::recovery::{collect, terminal, Cut, Fixture};
use crate::*;
use motor_protocol::*;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn return_paths_ssh_stores_result_and_replays_after_restart_without_resubmitting() {
    let f = Fixture::new(Cut::None, 2).await;
    let motor = f.motor();
    let stream = motor
        .run_with_returns(
            f.request("printf x > returned; exit 23"),
            ReturnOptions {
                return_paths: vec![f.key.join("returned").to_str().unwrap().into()],
                return_max_bytes: 1,
                return_chunked: false,
            },
        )
        .unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 23 });
    assert_eq!(terminal(&records).ran, Some(Ran::Remote));
    let result = motor.return_result(id).unwrap();
    assert_eq!(result.return_paths_failed, None);
    assert_eq!(result.returned_files.as_ref().unwrap()[0].data, "eA==");
    assert_eq!(
        result.returned_files.as_ref().unwrap()[0].sha256,
        "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
    );
    drop(motor);
    let motor = super::reopen_motor(|| {
        Motor::with_connector(
            f.config.clone(),
            &f.scratch.join("module-state"),
            std::sync::Arc::new(f.proxy.clone()),
        )
    })
    .await;
    assert_eq!(motor.return_result(id).as_ref(), Some(&result));
    let records = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 23 });
    assert_eq!(motor.return_result(id), Some(result));
    assert_eq!(
        f.proxy
            .frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| matches!(f, Frame::Run(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn return_paths_ssh_cap_one_and_missing_preserve_real_remote_outcome() {
    let f = Fixture::new(Cut::None, 2).await;
    let motor = f.motor();
    for (command, file, cause, measured) in [
        (
            "printf xy > returned; exit 17",
            "returned",
            ReturnFailureCause::OverCap,
            2,
        ),
        ("exit 17", "missing", ReturnFailureCause::Missing, 0),
    ] {
        let stream = motor
            .run_with_returns(
                f.request(command),
                ReturnOptions {
                    return_paths: vec![f.key.join(file).to_str().unwrap().into()],
                    return_max_bytes: 1,
                    return_chunked: false,
                },
            )
            .unwrap();
        let id = stream.job_id;
        let records = collect(stream).await;
        assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 17 });
        assert_eq!(terminal(&records).ran, Some(Ran::Remote));
        let result = motor.return_result(id).unwrap();
        assert_eq!(result.returned_files, None);
        assert_eq!(
            result.return_paths_failed,
            Some(ReturnPathsFailed {
                cause,
                measured_bytes: measured,
                cap_bytes: 1
            })
        );
    }
}

#[tokio::test]
async fn return_paths_missing_capability_refuses_before_run_but_empty_request_works() {
    let f = Fixture::new(Cut::None, 2).await;
    f.proxy.strip_return_paths.store(true, Ordering::SeqCst);
    let motor = f.motor();
    let records = collect(
        motor
            .run_with_returns(
                f.request("touch should-not-run"),
                ReturnOptions {
                    return_paths: vec![f.key.join("returned").to_str().unwrap().into()],
                    return_max_bytes: 1,
                    return_chunked: false,
                },
            )
            .unwrap(),
    )
    .await;
    assert!(matches!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    ));
    assert_eq!(
        terminal(&records).refusal_detail.as_deref(),
        Some("return_paths_unsupported")
    );
    assert!(!f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Run(_))));
    assert!(!f.remote_key.join("should-not-run").exists());
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
}

/// Submit an `exec.run` the way the agent route does, and return what its
/// caller receives through the terminal record.
async fn agent_run(motor: &Motor, mut request: RunRequest) -> Vec<StreamRecord> {
    let returns = crate::agent_route::take_return_request(&mut request).unwrap();
    let stream = motor.run_agent(request, None, returns).unwrap();
    crate::agent_route::agent_stream(stream, 0).await
}

#[tokio::test]
async fn return_chunked_agent_caller_gets_a_verified_430_kb_file() {
    let f = Fixture::new(Cut::None, 2).await;
    let motor = f.motor();
    let requested = vec!["./generated.bin".to_string(), "missing.bin".to_string()];
    let records = agent_run(
        &motor,
        f.request("head -c 440320 /dev/urandom > generated.bin; exit 4")
            .with_return_paths(requested.clone())
            .with_return_chunked(true),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 4 });
    let mut assembler = ReturnAssembler::new(&requested, RETURN_CHUNKED_MAX_BYTES_CAP);
    let mut received = Vec::new();
    let mut manifest = None;
    for record in &records {
        match record {
            StreamRecord::ReturnChunk(chunk) => {
                received.extend_from_slice(assembler.accept_chunk(chunk).unwrap());
            }
            StreamRecord::ReturnManifest(record) => manifest = Some(record.clone()),
            StreamRecord::ReturnResult(_) => panic!("a chunked caller never gets return_result"),
            _ => {}
        }
    }
    let manifest = manifest.unwrap();
    let id = terminal(&records).job_id;
    assert_eq!(manifest.job_id, id);
    // The manifest echoes the caller's paths exactly, not the runner's.
    assert_eq!(manifest.files[0].path(), "./generated.bin");
    assert_eq!(
        manifest.files[1].cause(),
        Some(&ReturnPathsFailureCause::Missing)
    );
    let verified = assembler.finish(&manifest).unwrap();
    assert!(verified[0].is_returned());
    let on_disk = std::fs::read(f.remote_key.join("generated.bin")).unwrap();
    assert_eq!(received, on_disk);
    assert!(matches!(
        &records[records.len() - 2],
        StreamRecord::ReturnManifest(_)
    ));
    let run = f.recorded_run();
    assert!(run.return_chunked);
    assert_eq!(run.return_max_bytes, RETURN_CHUNKED_MAX_BYTES_CAP);
    // A reattach replays the same chunks and manifest.
    let replay =
        crate::agent_route::agent_stream(motor.attach(AttachRequest::new(id, 0)).unwrap(), 0).await;
    let chunks = |records: &[StreamRecord]| {
        records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    StreamRecord::ReturnChunk(_) | StreamRecord::ReturnManifest(_)
                )
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(chunks(&replay), chunks(&records));
}

#[tokio::test]
async fn return_paths_agent_v1_caller_gets_only_return_result() {
    let f = Fixture::new(Cut::None, 2).await;
    let motor = f.motor();
    let records = agent_run(
        &motor,
        f.request("printf x > returned")
            .with_return_paths(vec!["returned".into()]),
    )
    .await;
    assert!(!records.iter().any(|r| matches!(
        r,
        StreamRecord::ReturnChunk(_) | StreamRecord::ReturnManifest(_)
    )));
    let StreamRecord::ReturnResult(result) = &records[records.len() - 2] else {
        panic!("v1 return_result precedes the terminal: {records:?}")
    };
    assert_eq!(result.returned_files().unwrap()[0].path(), "returned");
    assert_eq!(result.returned_files().unwrap()[0].data(), b"x");
    let wire = serde_json::to_value(Frame::Run(f.recorded_run())).unwrap();
    assert!(wire.get("return_chunked").is_none());
    assert!(wire.get("return_max_bytes").is_none());
}

#[tokio::test]
async fn return_chunked_runner_without_capability_refuses_before_run() {
    let f = Fixture::new(Cut::None, 2).await;
    f.proxy.strip_return_chunked.store(true, Ordering::SeqCst);
    let motor = f.motor();
    let records = agent_run(
        &motor,
        f.request("touch should-not-run")
            .with_return_paths(vec!["returned".into()])
            .with_return_chunked(true),
    )
    .await;
    assert!(matches!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RequestRejected
        }
    ));
    assert_eq!(
        terminal(&records).refusal_detail.as_deref(),
        Some("return_chunked_unsupported")
    );
    assert!(!f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Run(_))));
    assert!(!f.remote_key.join("should-not-run").exists());
    // The same runner still serves a v1 returned-file request.
    let records = agent_run(
        &motor,
        f.request("printf x > returned")
            .with_return_paths(vec!["returned".into()]),
    )
    .await;
    assert!(matches!(
        &records[records.len() - 2],
        StreamRecord::ReturnResult(_)
    ));
}
