//! A caller cancel can land while a spilled job's lease request is still
//! pending. Whatever the broker answers afterwards, every booking it grants
//! must be settled with exactly one finish or release, and the caller keeps
//! the result it was already given.
use super::queue_tests::{fixture_repo, ResubmitPeer};
use super::*;
use crate::hosted::test_http::Requests;
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::Ordering;

fn spill_config(broker_url: &str) -> Config {
    std::env::set_var("MOTOR_FAKE_BROKER_TOKEN", "hosted-test-bearer");
    Config {
        runner_instance: "test".into(),
        broker_token_source: crate::BrokerTokenSource::Fake,
        broker_url: Some(broker_url.into()),
        spill_to_hosted: true,
        spill_after_s: 0,
        ..Config::default()
    }
}

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn lease_requests(requests: &Requests) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/leases")
        .count()
}

/// What the broker was asked to settle for any lease: the finish bodies and
/// the job IDs whose release it acknowledged.
fn settlements(requests: &Requests) -> (Vec<crate::hosted::placement::Finish>, Vec<Uuid>) {
    let requests = requests.lock().unwrap();
    let finishes = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/finish"))
        .map(|(_, body)| serde_json::from_value(body.clone()).unwrap())
        .collect();
    let released = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/release"))
        .map(|(path, _)| path.split('/').nth(3).unwrap().parse().unwrap())
        .collect();
    (finishes, released)
}

/// Reads a job stream until the caller goes away, which is how a caller
/// detaches: it returns the last terminal it saw and how many it saw.
async fn detach_after(mut stream: JobStream) -> (Option<TerminalRecord>, usize) {
    let mut last = None;
    let mut terminals = 0;
    while let Some(record) = stream.records.recv().await {
        if let Ok(StreamRecord::Terminal(terminal)) = record {
            last = Some(terminal);
            terminals += 1;
        }
    }
    (last, terminals)
}

async fn start_spill(motor: &Motor, repo: &Path, wait: u64) -> JobStream {
    let id = Uuid::now_v7();
    motor
        .storage
        .create(id, wait, None, Some("test".into()), false)
        .unwrap();
    let path = repo.to_str().unwrap();
    let request = RunRequest::new(path, path, path, "sleep 7200").with_timeout(7_200);
    let (stream, tx, wake) = motor.stream(id);
    let job = motor.clone();
    let submission = motor.submission(id);
    tokio::spawn(async move {
        let _submission = submission;
        if let Err(error) = job
            .run_job(
                id,
                (request, crate::ReturnOptions::default()),
                Vec::new(),
                (wait, true),
                &tx,
                wake,
            )
            .await
        {
            let _ = tx.send(Err(error)).await;
        }
    });
    stream
}

/// The caller cancels while the lease request is pending and leaves. The
/// broker never answers that request with a booking. The booking that never
/// was is still closed once, as a job that never ran, so nothing stays held:
/// a Boat booking is settled with finish after its release is refused.
#[tokio::test]
async fn cancel_without_a_granted_lease_settles_the_never_ran_booking_once() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    boat.hold_provisioning.store(true, Ordering::SeqCst);
    let scratch = ScratchDir::new("cancel-no-lease");
    let repo = fixture_repo(&scratch);
    let motor = Motor::with_connector(
        spill_config(boat.config.broker_url.as_ref().unwrap()),
        &scratch.join("state"),
        Arc::new(ResubmitPeer::new()),
    )
    .unwrap();
    let stream = start_spill(&motor, &repo, 30).await;
    let id = stream.job_id;
    wait_until("the lease request", || lease_requests(&boat.requests) == 1).await;
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    let (reported, terminals) = detach_after(stream).await;
    assert_eq!(terminals, 1, "one terminal");
    let Some(reported) = reported else {
        panic!("the caller was told no outcome at all");
    };
    assert!(
        matches!(
            reported.outcome,
            Outcome::OutcomeUnknown | Outcome::Cancelled
        ),
        "{reported:?}"
    );

    // The lease request is answered last, after the caller is gone. Nothing
    // was booked, and nothing may stay booked.
    boat.hold_provisioning.store(false, Ordering::SeqCst);
    wait_until("the booking to be closed", || {
        let (finishes, released) = settlements(&boat.requests);
        !finishes.is_empty() || !released.is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (finishes, released) = settlements(&boat.requests);
    assert_eq!(
        released,
        [id],
        "the lease request that was sent must be released"
    );
    assert_eq!(
        finishes.len(),
        1,
        "finish is the only way to close a Boat booking: {finishes:?}"
    );
    assert_eq!(
        finishes[0].outcome, "lost",
        "a booking nothing ever ran on closes as never having run"
    );
}

