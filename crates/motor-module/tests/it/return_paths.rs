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
