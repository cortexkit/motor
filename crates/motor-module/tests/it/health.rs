use super::recovery::{Cut, Fixture};
use motor_protocol::*;
use std::{sync::atomic::Ordering, time::Duration};
use subc_client_rs::HandlerOutcome;

#[tokio::test]
async fn runner_health_op_returns_live_runner_values() {
    let f = Fixture::with_runner_config(Cut::None, 28, None, |config| {
        config.free_space_probe = std::sync::Arc::new(|_| Ok(987_654_321));
    })
    .await;
    let motor = f.motor();
    let mut stream = motor.run(f.request("sleep 30")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                stream.records.recv().await.unwrap().unwrap(),
                StreamRecord::Started(_)
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    // Fetch queues a separate warm build. Let that maintenance finish before
    // draining so the exact refusal count below measures only the caller's
    // rejected request, not a warm job refused while still queued.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = f.runner.status();
            if status.queue_depth == 0
                && status
                    .running_jobs
                    .iter()
                    .all(|job| job.job_id == stream.job_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("warm maintenance must finish before inspecting caller refusals");
    f.runner.begin_drain(Duration::from_secs(10), false);
    let mut refused = motor.run(f.request("true")).unwrap();
    while let Some(record) = refused.records.recv().await {
        if let StreamRecord::Terminal(terminal) = record.unwrap() {
            assert!(matches!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::RunnerDraining
                }
            ));
            break;
        }
    }
    let mut expected = f.runner.runner_status().health.unwrap();
    assert_eq!(expected.free_bytes, Some(987_654_321));
    assert_eq!(expected.queue_depth, 0);
    assert_eq!(expected.since_ms, f.clock.load(Ordering::SeqCst));
    assert_eq!(expected.window_minutes, 60);
    assert_eq!(expected.runner_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(expected.counts.accepted, 1);
    assert_eq!(expected.counts.refusals.get("runner_draining"), Some(&1));
    assert_eq!(
        expected.running,
        vec![HealthRunningJob {
            job_id: stream.job_id,
            weight: 2
        }]
    );
    let HandlerOutcome::Response(bytes) = crate::module::runner_health_reply(&motor).await.unwrap()
    else {
        panic!("runner.health must return one response");
    };
    let report: RunnerHealth = serde_json::from_slice(&bytes).unwrap();
    // Host load and memory can change between the two independent probes.
    expected.load_1m = report.load_1m.clone();
    expected.threads = report.threads;
    expected.mem_available_bytes = report.mem_available_bytes;
    assert_eq!(report, expected);
    motor
        .cancel(CancelRequest::new(stream.job_id))
        .await
        .unwrap();
    while let Some(record) = stream.records.recv().await {
        if matches!(record.unwrap(), StreamRecord::Terminal(_)) {
            break;
        }
    }
}

#[tokio::test]
async fn runner_health_op_reports_unreachable_cause_not_zero_counts() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    f.proxy.unavailable.store(true, Ordering::SeqCst);
    let HandlerOutcome::Error { code, message } =
        crate::module::runner_health_reply(&motor).await.unwrap()
    else {
        panic!("offline runner must return a structured error, not a response");
    };
    assert_eq!(code, "runner_health_unavailable");
    assert!(message.contains("connect to runner"), "{message}");
    assert!(message.contains("offline"), "{message}");
}
