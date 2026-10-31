use super::repository_routing_tests::{request, until, HeldClock};
use super::*;
use crate::hosted::{
    boat_tests::committed_repo,
    tests::harness::{Behavior, Harness, NoSsh},
};
use std::sync::{atomic::AtomicUsize, Mutex};

fn decoded(record: StreamRecord) -> StreamRecord {
    let bytes = serde_json::to_vec(&record).unwrap();
    let decoded: cortexkit_exec_remote_types::StreamRecord =
        serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, record);
    if matches!(record, StreamRecord::Progress(_)) {
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes)
            .unwrap()
            .get("seq")
            .is_none());
    }
    decoded
}

async fn collect(mut stream: JobStream, seen: Arc<Mutex<Vec<StreamRecord>>>) {
    while let Some(record) = stream.records.recv().await {
        let record = decoded(record.unwrap());
        let terminal = matches!(record, StreamRecord::Terminal(_));
        seen.lock().unwrap().push(record);
        if terminal {
            return;
        }
    }
    panic!("stream ended without a terminal");
}

async fn hosted_wait(opt_in: bool) -> Vec<StreamRecord> {
    let _clock = HeldClock::new();
    let waits = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(waits.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"no_capacity"}),
        "30",
    ));
    let (scratch, repo) = committed_repo();
    let config = Config {
        hosted_repositories: vec![repo.to_str().unwrap().into()],
        ..h.config()
    };
    let root = scratch.0.join("state");
    let motor =
        Motor::with_connector(config.clone(), &root, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    let mut request = request(&repo).with_progress(opt_in);
    request.timeout = Some(60);
    request.weight_hint = Some(16);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn(collect(stream, seen.clone()));
    until("the parsed capacity refusal", || {
        waits.text().contains("hosted broker retry wait")
    })
    .await;
    tokio::time::advance(Duration::from_millis(1999)).await;
    tokio::task::yield_now().await;
    assert!(seen.lock().unwrap().is_empty());
    for (advance, elapsed) in [(1, 2000), (9999, 2000), (1, 12000), (10000, 22000)] {
        tokio::time::advance(Duration::from_millis(advance)).await;
        tokio::task::yield_now().await;
        if opt_in {
            until("the progress record", || {
                seen.lock().unwrap().last().is_some_and(|record| {
                    matches!(record,
                StreamRecord::Progress(p) if p.phase_elapsed_ms() == elapsed)
                })
            })
            .await;
        }
    }
    tokio::time::advance(Duration::from_secs(8)).await;
    until("hosted completion", || task.is_finished()).await;
    task.await.unwrap();
    let original = seen.lock().unwrap().clone();
    let replay = Arc::new(Mutex::new(Vec::new()));
    collect(
        motor.attach(AttachRequest::new(id, 0)).unwrap(),
        replay.clone(),
    )
    .await;
    assert!(!replay
        .lock()
        .unwrap()
        .iter()
        .any(|r| matches!(r, StreamRecord::Progress(_))));
    motor.close().await;
    let reopened =
        Motor::with_connector(config, &root, Arc::new(NoSsh(AtomicUsize::new(0)))).unwrap();
    let replay = Arc::new(Mutex::new(Vec::new()));
    collect(
        reopened.attach(AttachRequest::new(id, 0)).unwrap(),
        replay.clone(),
    )
    .await;
    assert!(!replay
        .lock()
        .unwrap()
        .iter()
        .any(|r| matches!(r, StreamRecord::Progress(_))));
    reopened.close().await;
    original
}

#[tokio::test(start_paused = true)]
async fn hosted_no_capacity_progress_cadence_and_no_replay() {
    let records = hosted_wait(true).await;
    let progress: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            StreamRecord::Progress(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(
        progress
            .iter()
            .map(|p| p.phase_elapsed_ms())
            .collect::<Vec<_>>(),
        [2000, 12000, 22000]
    );
    for record in progress {
        assert_eq!(record.phase(), &ProgressPhase::PlacementWait);
        assert_eq!(
            record.detail(),
            Some("waiting for a host with 16 free vCPUs")
        );
    }
    let accepted = records
        .iter()
        .position(|r| matches!(r, StreamRecord::Accepted(_)))
        .unwrap();
    assert!(!records[accepted..]
        .iter()
        .any(|r| matches!(r, StreamRecord::Progress(_))));
}

#[tokio::test(start_paused = true)]
async fn opted_out_hosted_wait_has_no_progress() {
    let records = hosted_wait(false).await;
    assert!(!records
        .iter()
        .any(|r| matches!(r, StreamRecord::Progress(_))));
    assert!(records
        .iter()
        .any(|r| matches!(r, StreamRecord::Accepted(_))));
}

#[tokio::test(start_paused = true)]
async fn fast_hosted_run_has_no_progress() {
    let _clock = HeldClock::new();
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let config = Config {
        hosted_repositories: vec![repo.to_str().unwrap().into()],
        ..h.config()
    };
    let motor = Motor::with_connector(
        config,
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn(collect(
        motor.run(request(&repo).with_progress(true)).unwrap(),
        seen.clone(),
    ));
    until("fast hosted completion", || task.is_finished()).await;
    task.await.unwrap();
    assert!(!seen
        .lock()
        .unwrap()
        .iter()
        .any(|r| matches!(r, StreamRecord::Progress(_))));
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn phase_changes_reset_elapsed_but_not_the_run_rate_limit() {
    let _clock = HeldClock::new();
    let reporter = progress::Reporter::new(true, 4);
    reporter.phase(ProgressPhase::PlacementWait);
    let (events, receive) = mpsc::channel(8);
    let (tx, mut rx) = mpsc::channel(8);
    let (finish, done) = tokio::sync::oneshot::channel();
    let task_reporter = reporter.clone();
    let task = tokio::spawn(async move {
        task_reporter
            .forward(
                async {
                    done.await.unwrap();
                    Ok(())
                },
                receive,
                &tx,
            )
            .await
            .unwrap();
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let first = decoded(rx.recv().await.unwrap().unwrap());
    assert!(matches!(first, StreamRecord::Progress(p) if p.phase_elapsed_ms() == 2000));
    tokio::time::advance(Duration::from_secs(8)).await;
    reporter.phase(ProgressPhase::Connect);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(1999)).await;
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
    tokio::time::advance(Duration::from_millis(1)).await;
    let second = decoded(rx.recv().await.unwrap().unwrap());
    assert!(
        matches!(second, StreamRecord::Progress(p) if p.phase() == &ProgressPhase::Connect && p.phase_elapsed_ms() == 2000)
    );
    reporter.phase(ProgressPhase::Upload);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert!(
        rx.try_recv().is_err(),
        "a new phase cannot bypass the per-run rate limit"
    );
    tokio::time::advance(Duration::from_secs(8)).await;
    let third = decoded(rx.recv().await.unwrap().unwrap());
    assert!(
        matches!(third, StreamRecord::Progress(p) if p.phase() == &ProgressPhase::Upload && p.phase_elapsed_ms() == 10000)
    );
    events
        .send(Ok(StreamRecord::Accepted(Accepted::new(Uuid::now_v7(), 0))))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap(),
        StreamRecord::Accepted(_)
    ));
    reporter.phase(ProgressPhase::PlacementWait);
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    assert!(
        rx.try_recv().is_err(),
        "progress must never resume after acceptance"
    );
    finish.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn route_close_interrupts_blocked_progress_without_cancelling_submission() {
    let _clock = HeldClock::new();
    let scratch = cortexkit_test_support::ScratchDir::new("motor-progress-detach");
    let motor = Motor::open(Config::default(), &scratch).unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    let (stream, tx, _) = motor.stream(id);
    let reporter = progress::Reporter::new(true, 4);
    reporter.phase(ProgressPhase::Connect);
    let (_events, receive) = mpsc::channel(8);
    let (finish, done) = tokio::sync::oneshot::channel();
    let submission = tokio::spawn(async move {
        reporter
            .forward(
                async {
                    done.await.unwrap();
                    Ok(())
                },
                receive,
                &tx,
            )
            .await
            .unwrap();
    });
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
            assert!(matches!(record, StreamRecord::Progress(_)));
            let entered = entered.take().unwrap();
            let writable = task_writable.clone();
            let sends = task_sends.clone();
            async move {
                entered.send(()).unwrap();
                let _permit = writable.acquire().await.unwrap();
                sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        },
    ));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    sending.await.unwrap();
    close.send(()).unwrap();
    writable.add_permits(1);
    writer.await.unwrap().unwrap();
    assert_eq!(
        sends.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no progress sends after close"
    );
    tokio::time::advance(Duration::from_secs(20)).await;
    assert!(
        !submission.is_finished(),
        "route close must not cancel submission"
    );
    finish.send(()).unwrap();
    submission.await.unwrap();
    assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 0);
    motor.close().await;
}
