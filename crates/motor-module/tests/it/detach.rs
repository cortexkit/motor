use super::recovery::{collect, terminal, Cut, Fixture};
use motor_protocol::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
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

#[tokio::test]
async fn route_close_interrupts_blocked_output_and_reattach_replays_everything() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let stream = motor
        .run(f.request("printf started; sleep 1; printf finished"))
        .unwrap();
    let id = stream.job_id;
    let (close, closed) = tokio::sync::oneshot::channel();
    let (entered, sending) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    let writable = Arc::new(tokio::sync::Semaphore::new(0));
    let sends = Arc::new(AtomicUsize::new(0));
    let task_sends = sends.clone();
    let task_writable = writable.clone();
    let writer = tokio::spawn(crate::module::forward(
        async { closed.await.unwrap() },
        stream,
        move |record| {
            let entered = if matches!(record, StreamRecord::Output(_)) {
                entered.take()
            } else {
                None
            };
            let writable = task_writable.clone();
            let sends = task_sends.clone();
            async move {
                if let Some(entered) = entered {
                    entered.send(()).unwrap();
                    // Model a caller whose transport stops accepting output before
                    // its route-close notice reaches the writer.
                    let _permit = writable.acquire().await.unwrap();
                }
                sends.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
    ));
    tokio::time::timeout(Duration::from_secs(30), sending)
        .await
        .unwrap()
        .unwrap();
    let before_close = sends.load(Ordering::SeqCst);
    close.send(()).unwrap();
    writable.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("route close must interrupt a pending output send")
        .unwrap()
        .unwrap();
    assert_eq!(
        sends.load(Ordering::SeqCst),
        before_close,
        "no sends after close"
    );
    assert!(!motor.pending(id), "detach must not cancel the running job");

    let replay = Arc::new(Mutex::new(Vec::new()));
    let fresh_route = replay.clone();
    crate::module::forward(
        std::future::pending(),
        motor.attach(AttachRequest::new(id, 0)).unwrap(),
        move |record| {
            fresh_route.lock().unwrap().push(record);
            async { Ok(()) }
        },
    )
    .await
    .unwrap();
    let replay = replay.lock().unwrap();
    let output: Vec<_> = replay
        .iter()
        .filter_map(|record| match record {
            StreamRecord::Output(output) => Some(output.bytes.0.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(output, b"startedfinished");
    assert_eq!(terminal(&replay).outcome, Outcome::Exit { code: 0 });
    assert_eq!(sends.load(Ordering::SeqCst), before_close);
}
