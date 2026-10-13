use super::recovery::{collect, terminal, Cut, Fixture};
use motor_protocol::*;
use std::time::Duration;

/// A caller that drops its stream mid-run must leave the job running. That
/// happens when the calling tool (AFT, which runs agents' remote commands)
/// detaches on Esc, on a tool timeout or on its own restart, and when this
/// module shuts down. Only an explicit exec.cancel stops a job. Here the
/// caller detaches after the first output; the job still finishes normally and a
/// later attach replays everything through the terminal.
#[tokio::test]
async fn cancelled_request_detaches_and_the_job_keeps_running() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let stream = motor
        // Long enough that the reattach below happens while the job still runs,
        // so a cancel recorded on detach would be delivered and end it early.
        .run(f.request("echo started; sleep 5; echo finished"))
        .unwrap();
    let id = stream.job_id;
    let (detach, detached) = tokio::sync::oneshot::channel::<()>();
    let mut detach = Some(detach);
    let forwarded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = forwarded.clone();
    tokio::time::timeout(
        Duration::from_secs(30),
        crate::module::forward(
            async {
                let _ = detached.await;
            },
            stream,
            move |record| {
                // Detach as soon as the command has produced output.
                if matches!(&record, StreamRecord::Output(o) if o.bytes.0.starts_with(b"started")) {
                    if let Some(detach) = detach.take() {
                        let _ = detach.send(());
                    }
                }
                seen.lock().unwrap().push(record);
                async { Ok(()) }
            },
        ),
    )
    .await
    .expect("forward returns promptly on detach")
    .unwrap();
    assert!(
        !motor.pending(id),
        "a detach must not record a cancel for the job"
    );
    assert!(
        !forwarded
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r, StreamRecord::Terminal(_))),
        "the caller detached before the job ended"
    );
    let replay = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&replay).outcome, Outcome::Exit { code: 0 });
    assert!(
        replay
            .iter()
            .any(|r| matches!(r, StreamRecord::Output(o) if o.bytes.0.starts_with(b"finished"))),
        "the job ran to completion after the caller detached: {replay:?}"
    );
}
