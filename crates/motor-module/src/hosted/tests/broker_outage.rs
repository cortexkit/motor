use super::*;
use crate::{hosted::boat_tests::committed_repo, Motor};
use tokio::{net::TcpSocket, task::JoinSet};

/// Reserve the broker's port without listening, then forward to the real fake
/// broker after recovery. Connections fail at TCP connect, not as HTTP refusals.
struct Outage {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Outage {
    fn new(target: &str, recovery: Option<Duration>) -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        let target = target.strip_prefix("http://").unwrap().to_owned();
        let task = tokio::spawn(async move {
            match recovery {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending::<()>().await,
            }
            let listener = socket.listen(16).unwrap();
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut incoming, _) = accepted.unwrap();
                        let target = target.clone();
                        connections.spawn(async move {
                            let mut outgoing = tokio::net::TcpStream::connect(target).await.unwrap();
                            let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                        });
                    }
                    Some(result) = connections.join_next() => { result.unwrap(); }
                }
            }
        });
        Self { url, task }
    }
}

impl Drop for Outage {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn terminal_and_progress(mut stream: crate::JobStream) -> (TerminalRecord, Vec<Progress>) {
    let mut progress = Vec::new();
    while let Some(record) = stream.records.recv().await {
        match record.unwrap() {
            StreamRecord::Progress(record) => progress.push(record),
            StreamRecord::Terminal(record) => return (record, progress),
            _ => {}
        }
    }
    panic!("stream ended without a terminal");
}

#[tokio::test]
async fn broker_connect_outage_recovers_and_runs_with_placement_progress() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let outage = Outage::new(&h.route.broker_url, Some(Duration::from_secs(3)));
    let (scratch, repo) = committed_repo();
    let mut config = h.config();
    config.broker_url = Some(outage.url.clone());
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let started = tokio::time::Instant::now();
    let stream = motor
        .run(Harness::request(&repo).with_progress(true))
        .unwrap();
    let id = stream.job_id;
    let (terminal, progress) =
        tokio::time::timeout(Duration::from_secs(20), terminal_and_progress(stream))
            .await
            .expect("placement must recover when the broker becomes reachable");
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert!(progress
        .iter()
        .any(|p| { p.phase() == &ProgressPhase::PlacementWait && p.phase_elapsed_ms() >= 2_000 }));
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.placement_attempts, 1);
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0],
        serde_json::to_vec(&state.lease_request.unwrap()).unwrap()
    );
}

async fn exhaust_outage(queue_wait_s: u64, expected_wait_s: u64) {
    let h = Harness::new(Behavior::CompleteRun).await;
    let outage = Outage::new(&h.route.broker_url, None);
    let (scratch, repo) = committed_repo();
    let mut config = h.config();
    config.broker_url = Some(outage.url.clone());
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let mut request = Harness::request(&repo);
    request.queue_wait_limit_s = Some(queue_wait_s);
    let started = tokio::time::Instant::now();
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let (terminal, _) = tokio::time::timeout(
        Duration::from_secs(expected_wait_s + 10),
        terminal_and_progress(stream),
    )
    .await
    .expect("an unreachable broker must not outlive the retry bound");
    assert!(started.elapsed() >= Duration::from_secs(expected_wait_s));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable
        }
    );
    assert!(terminal
        .refusal_detail
        .unwrap()
        .contains("hosted_broker_unreachable"));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(motor.hosted_state(id).unwrap().placement_attempts, 1);
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn broker_connect_outage_exhausts_sixty_second_bound() {
    exhaust_outage(120, 60).await;
}

#[tokio::test]
async fn broker_connect_outage_is_capped_by_queue_budget() {
    exhaust_outage(3, 3).await;
}

#[tokio::test]
async fn broker_connect_outage_cancel_interrupts_backoff() {
    let logs = crate::logging::Captured::default();
    let _logs = crate::logging::scoped(logs.clone());
    let h = Harness::new(Behavior::CompleteRun).await;
    let outage = Outage::new(&h.route.broker_url, None);
    let (scratch, repo) = committed_repo();
    let mut config = h.config();
    config.broker_url = Some(outage.url.clone());
    let motor = Motor::with_connector(
        config,
        &scratch.0.join("state"),
        Arc::new(NoSsh(AtomicUsize::new(0))),
    )
    .unwrap();
    let stream = motor.run(Harness::request(&repo)).unwrap();
    let id = stream.job_id;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !logs.text().contains("hosted broker transport retry wait") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the request must enter transport backoff");
    let started = tokio::time::Instant::now();
    let (terminal, _) = tokio::time::timeout(Duration::from_millis(500), async {
        motor.cancel(CancelRequest::new(id)).await.unwrap();
        terminal_and_progress(stream).await
    })
    .await
    .expect("cancellation must interrupt the one-second backoff");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(terminal.outcome, Outcome::Cancelled);
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

fn short_http_timeout(mut client: Client) -> Client {
    client.http = reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    client
}

#[tokio::test]
async fn lease_timeout_retries_the_exact_stored_body() {
    let h = Harness::new(Behavior::Normal).await;
    h.unanswered_lease.store(1, Ordering::SeqCst);
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let storage = stored(&scratch, id, h.route.clone());
    let client = short_http_timeout(Client::new(h.route.clone()));
    let (_, lease) = client
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    assert_eq!(lease.job_id, id.to_string());
    let bodies = h.lease_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[0], bodies[1],
        "a timeout must not change the lease hash"
    );
    let state = storage.job(id).unwrap().hosted.unwrap();
    assert_eq!(
        bodies[0],
        serde_json::to_vec(&state.lease_request.unwrap()).unwrap()
    );
    assert_eq!(state.placement_attempts, 1);
}

#[tokio::test]
async fn replacement_timeout_retries_without_rerequesting_the_lease() {
    let first = Harness::with_host(Behavior::Refuse("host_full"), "first-host").await;
    let second = Harness::with_host(Behavior::Normal, "second-host").await;
    first.unanswered_replace.store(1, Ordering::SeqCst);
    first
        .replacement_hosts
        .lock()
        .unwrap()
        .push_back(second.broker.clone());
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let route = HostedJobState {
        fleet_routed: true,
        ..first.route.clone()
    };
    let storage = stored(&scratch, id, route.clone());
    let client = short_http_timeout(Client::new(route));
    let (_, lease) = client
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    assert_eq!(lease.placement.host, "second-host");
    assert_eq!(first.lease_bodies.lock().unwrap().len(), 1);
    assert_eq!(
        *first.requests.lock().unwrap(),
        vec![
            "/v1/leases".to_owned(),
            format!("/v1/leases/{id}/replace"),
            format!("/v1/leases/{id}/replace"),
        ]
    );
}

#[tokio::test]
async fn broker_http_refusals_do_not_enter_transport_backoff() {
    for (status, code, expected) in [
        (400, "bad_request", Error::RequestInvalid(None)),
        (401, "unauthorized", Error::Auth),
        (403, "account_closed", Error::AccountClosed),
        (409, "job_conflict", Error::Rejected),
        (422, "no_host_fits", Error::NoHostFits(None, None)),
        (429, "too_many_requests", Error::Broker),
    ] {
        let http = super::super::test_http::Http::new(move |_, _| {
            (status, serde_json::json!({"error":code}))
        })
        .await;
        let config = Config {
            broker_url: Some(http.url.clone()),
            broker_token_source: BrokerTokenSource::Fake,
            ..Config::default()
        };
        let client = Client::new(HostedJobState::new(&config));
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            client.post_ready_authorized::<_, serde_json::Value>(
                "/v1/leases",
                &serde_json::json!({}),
                "token".into(),
                tokio::time::Instant::now() + Duration::from_secs(60),
                Error::Broker,
            ),
        )
        .await
        .expect("HTTP refusals must not wait for transport recovery");
        assert_eq!(result, Err(expected));
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }
}
