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
    assert_eq!(seen.lock().unwrap().len(), usize::from(opt_in));
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
        [0, 2000, 12000, 22000]
    );
    for record in progress {
        let id = records
            .iter()
            .find_map(|record| match record {
                StreamRecord::Accepted(accepted) => Some(accepted.job_id),
                _ => None,
            })
            .unwrap();
        assert_eq!(record.job_id(), Some(id));
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
async fn timed_progress_carries_the_assigned_job_id() {
    let _clock = HeldClock::new();
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    h.lease_replies.lock().unwrap().push_back((
        503,
        serde_json::json!({"error":"no_capacity"}),
        "30",
    ));
    let (scratch, repo) = committed_repo();
    let motor = Motor::with_connector(
        Config {
            hosted_repositories: vec![repo.to_str().unwrap().into()],
            ..h.config()
        },
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let mut stream = motor.run(request(&repo).with_progress(true)).unwrap();
    let id = stream.job_id;
    until("broker retry before the timed progress", || {
        logs.text().contains("hosted broker retry wait")
    })
    .await;
    tokio::time::advance(Duration::from_secs(2)).await;
    loop {
        if let StreamRecord::Progress(progress) = stream.records.recv().await.unwrap().unwrap() {
            if progress.phase_elapsed_ms() >= 2000 {
                assert_eq!(progress.job_id(), Some(id));
                break;
            }
        }
    }
    stream.cancel().unwrap();
    collect(stream, Arc::new(Mutex::new(Vec::new()))).await;
    motor.close().await;
}

#[tokio::test(start_paused = true)]
async fn fast_hosted_run_reports_id_immediately() {
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
    let seen = seen.lock().unwrap().clone();
    let StreamRecord::Progress(progress) = &seen[0] else {
        panic!("the first record must disclose the job ID: {seen:?}");
    };
    assert_eq!(progress.phase_elapsed_ms(), 0);
    let id = seen
        .iter()
        .find_map(|record| match record {
            StreamRecord::Accepted(accepted) => Some(accepted.job_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(progress.job_id(), Some(id));
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

fn queued(waiting_on: &str, blocker: Option<Uuid>) -> QueueStatus {
    QueueStatus {
        state: "queued".into(),
        waiting_on: Some(waiting_on.into()),
        blocker: blocker.map(|job_id| QueueBlocker {
            job_id,
            started_ms: Some(1_730_000_000_000),
            timeout_s: Some(600),
        }),
    }
}

/// Lets the forwarding task act on everything already sent to it.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn queue_wait(record: StreamRecord) -> Progress {
    match decoded(record) {
        StreamRecord::Progress(progress) if progress.phase() == &ProgressPhase::QueueWait => {
            progress
        }
        other => panic!("expected queue_wait progress: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn queue_wait_reports_each_change_once_between_accepted_and_started() {
    let _clock = HeldClock::new();
    let id = Uuid::now_v7();
    let holder = Uuid::now_v7();
    let reporter = progress::Reporter::new(true, 4).with_job_id(id);
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
    // A heartbeat can precede the Accepted record; it is held until then.
    reporter.queue_status(&queued("workspace owner", Some(holder)));
    settle().await;
    assert!(rx.try_recv().is_err(), "no queue_wait before Accepted");
    events
        .send(Ok(StreamRecord::Accepted(Accepted::new(id, 0))))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap(),
        StreamRecord::Accepted(_)
    ));
    let first = queue_wait(rx.recv().await.unwrap().unwrap());
    assert_eq!(first.waiting_on(), Some(&ProgressWaitingOn::Checkout));
    assert_eq!(first.blocker_job_id(), Some(holder));
    assert_eq!(first.job_id(), Some(id));
    assert_eq!(first.phase_elapsed_ms(), 0);

    // An unchanged wait repeats nothing, however long it lasts.
    tokio::time::advance(Duration::from_secs(61)).await;
    reporter.queue_status(&queued("workspace owner", Some(holder)));
    settle().await;
    assert!(rx.try_recv().is_err(), "an unchanged wait is not repeated");

    reporter.queue_status(&queued("sibling checkout owner", None));
    let second = queue_wait(rx.recv().await.unwrap().unwrap());
    assert_eq!(
        second.waiting_on(),
        Some(&ProgressWaitingOn::SiblingCheckout)
    );
    assert_eq!(second.blocker_job_id(), None);
    assert_eq!(second.phase_elapsed_ms(), 61_000);

    // A wait with no matching reason is still a change, reported without one.
    reporter.queue_status(&queued("queued workspace job", None));
    let third = queue_wait(rx.recv().await.unwrap().unwrap());
    assert_eq!(third.waiting_on(), None);
    assert_eq!(third.blocker_job_id(), None);
    // Another unmatched label maps to the same wait, and a job that is no
    // longer queued is not waiting in the queue.
    reporter.queue_status(&queued("some future label", None));
    reporter.queue_status(&QueueStatus {
        state: "uploading".into(),
        waiting_on: Some("bundle upload/fetch".into()),
        blocker: None,
    });
    settle().await;
    assert!(rx.try_recv().is_err(), "the mapped wait did not change");

    events
        .send(Ok(StreamRecord::Started(Started::new(0, 61_000, 1))))
        .await
        .unwrap();
    assert!(matches!(
        rx.recv().await.unwrap().unwrap(),
        StreamRecord::Started(_)
    ));
    reporter.queue_status(&queued("memory", None));
    tokio::time::advance(Duration::from_secs(120)).await;
    settle().await;
    assert!(rx.try_recv().is_err(), "no queue_wait after Started");
    finish.send(()).unwrap();
    task.await.unwrap();
    assert!(rx.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn queue_wait_ignores_heartbeats_read_after_started() {
    let _clock = HeldClock::new();
    let id = Uuid::now_v7();
    let reporter = progress::Reporter::new(true, 4).with_job_id(id);
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
    // The job stream reader saw Started and then another queued heartbeat before the
    // forwarder caught up with Accepted.
    reporter.queue_status(&queued("permits", None));
    reporter.queue_started();
    reporter.queue_status(&queued("memory", None));
    events
        .send(Ok(StreamRecord::Accepted(Accepted::new(id, 0))))
        .await
        .unwrap();
    settle().await;
    finish.send(()).unwrap();
    task.await.unwrap();
    let mut seen = Vec::new();
    while let Some(record) = rx.recv().await {
        seen.push(record.unwrap());
    }
    assert!(
        matches!(seen.as_slice(), [StreamRecord::Accepted(_)]),
        "{seen:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn queue_wait_is_silent_without_queue_status() {
    let _clock = HeldClock::new();
    let id = Uuid::now_v7();
    let reporter = progress::Reporter::new(true, 4).with_job_id(id);
    let (events, receive) = mpsc::channel(8);
    let (tx, mut rx) = mpsc::channel(8);
    let (finish, done) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
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
    events
        .send(Ok(StreamRecord::Accepted(Accepted::new(id, 0))))
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(120)).await;
    settle().await;
    events
        .send(Ok(StreamRecord::Started(Started::new(0, 120_000, 1))))
        .await
        .unwrap();
    settle().await;
    finish.send(()).unwrap();
    task.await.unwrap();
    let mut seen = Vec::new();
    while let Some(record) = rx.recv().await {
        seen.push(record.unwrap());
    }
    assert!(
        matches!(
            seen.as_slice(),
            [StreamRecord::Accepted(_), StreamRecord::Started(_)]
        ),
        "{seen:?}"
    );
}

#[test]
fn queue_wait_maps_runner_and_host_labels() {
    for (label, expected) in [
        ("workspace owner", Some(ProgressWaitingOn::Checkout)),
        (
            "sibling user of this checkout",
            Some(ProgressWaitingOn::SiblingCheckout),
        ),
        (
            "sibling checkout owner",
            Some(ProgressWaitingOn::SiblingCheckout),
        ),
        ("permits", Some(ProgressWaitingOn::Capacity)),
        ("gate job ahead", Some(ProgressWaitingOn::Capacity)),
        (
            "cache fill ahead needs more free permits",
            Some(ProgressWaitingOn::Capacity),
        ),
        (
            "older job ahead needs more free permits",
            Some(ProgressWaitingOn::Capacity),
        ),
        ("demand exceeds capacity", Some(ProgressWaitingOn::Capacity)),
        (
            "returned bytes budget (needs 10 bytes; 5 of 12 bytes reserved)",
            Some(ProgressWaitingOn::Capacity),
        ),
        ("memory", Some(ProgressWaitingOn::Memory)),
        ("checkout disk", Some(ProgressWaitingOn::CheckoutDisk)),
        ("queue order", None),
        ("queued workspace job", None),
        ("workspace drop", None),
        ("workspace setup", None),
        ("bundle upload/fetch", None),
        ("", None),
        ("a label from a newer runner", None),
    ] {
        assert_eq!(progress::waiting_on(label), expected, "{label}");
    }
    let holder = Uuid::now_v7();
    assert_eq!(
        progress::QueueWait::from_status(&queued("checkout disk", Some(holder))),
        Some(progress::QueueWait {
            waiting_on: Some(ProgressWaitingOn::CheckoutDisk),
            blocker_job_id: Some(holder),
        })
    );
    assert_eq!(
        progress::QueueWait::from_status(&QueueStatus {
            state: "queued".into(),
            waiting_on: None,
            blocker: None,
        }),
        Some(progress::QueueWait {
            waiting_on: None,
            blocker_job_id: None,
        })
    );
    for state in ["uploading", "running", "starting"] {
        let mut status = queued("permits", None);
        status.state = state.into();
        assert_eq!(progress::QueueWait::from_status(&status), None, "{state}");
    }
}

#[tokio::test(start_paused = true)]
async fn hosted_heartbeats_reach_the_caller_as_queue_wait() {
    let _clock = HeldClock::new();
    let h = Harness::new(Behavior::CompleteRun).await;
    let holder = Uuid::now_v7();
    let heartbeat = |status: QueueStatus| Frame::Heartbeat {
        queue_wait_ms: Some(0),
        queue_status: Some(status),
    };
    *h.after_accepted.lock().unwrap() = vec![
        heartbeat(queued("checkout disk", Some(holder))),
        heartbeat(queued("checkout disk", Some(holder))),
    ];
    // Held until the caller has seen the wait, so the two waits cannot merge.
    h.run_gate.send_replace(false);
    *h.after_run_gate.lock().unwrap() = vec![
        Frame::Started {
            // The harness replaces this with the run's ID.
            job_id: Uuid::nil(),
            seq: 0,
            queue_wait_ms: 5,
            started_at_ms: 1_730_000_000_005,
            path_mapping: None,
        },
        heartbeat(queued("memory", None)),
    ];
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
    until("the queue_wait record", || {
        seen.lock().unwrap().iter().any(
            |r| matches!(r, StreamRecord::Progress(p) if p.phase() == &ProgressPhase::QueueWait),
        )
    })
    .await;
    h.run_gate.send_replace(true);
    until("hosted completion", || task.is_finished()).await;
    task.await.unwrap();
    let seen = seen.lock().unwrap().clone();
    let accepted = seen
        .iter()
        .position(|r| matches!(r, StreamRecord::Accepted(_)))
        .unwrap();
    let started = seen
        .iter()
        .position(|r| matches!(r, StreamRecord::Started(_)))
        .unwrap();
    let waits: Vec<_> = seen
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match record {
            StreamRecord::Progress(p) if p.phase() == &ProgressPhase::QueueWait => {
                Some((index, p.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(waits.len(), 1, "{seen:?}");
    let (index, wait) = &waits[0];
    assert!(accepted < *index && *index < started, "{seen:?}");
    assert_eq!(wait.waiting_on(), Some(&ProgressWaitingOn::CheckoutDisk));
    assert_eq!(wait.blocker_job_id(), Some(holder));
    motor.close().await;
}
