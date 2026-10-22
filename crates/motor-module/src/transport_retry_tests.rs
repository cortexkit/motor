use super::*;
use crate::diagnostics::{count, UnreachableStep};
use std::sync::atomic::{AtomicUsize, Ordering};

struct HelloConnector {
    connects: AtomicUsize,
    closed_connects: usize,
    protocol_version: u32,
    reply_delay: Duration,
}

impl HelloConnector {
    fn new(closed_connects: usize, protocol_version: u32) -> Self {
        Self {
            connects: AtomicUsize::new(0),
            closed_connects,
            protocol_version,
            reply_delay: Duration::ZERO,
        }
    }

    fn with_reply_delay(mut self, reply_delay: Duration) -> Self {
        self.reply_delay = reply_delay;
        self
    }
}

impl Connector for HelloConnector {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let attempt = self.connects.fetch_add(1, Ordering::SeqCst);
        let closed_connects = self.closed_connects;
        let protocol_version = self.protocol_version;
        let reply_delay = self.reply_delay;
        Box::pin(async move {
            let (client, mut runner) = tokio::io::duplex(4096);
            if attempt >= closed_connects {
                tokio::spawn(async move {
                    let mut header = [0; 4];
                    runner.read_exact(&mut header).await.unwrap();
                    let mut payload = vec![0; u32::from_be_bytes(header) as usize];
                    runner.read_exact(&mut payload).await.unwrap();
                    assert!(matches!(
                        serde_json::from_slice::<Frame>(&payload).unwrap(),
                        Frame::Hello { .. }
                    ));
                    if !reply_delay.is_zero() {
                        assert!(
                            tokio::time::timeout(reply_delay, runner.read_u8())
                                .await
                                .is_err(),
                            "the channel sent a non-hello frame before the hello reply"
                        );
                    }
                    write_active(
                        &mut runner,
                        &Frame::Hello {
                            return_paths: false,
                            return_chunked: false,
                            protocol_version,
                            started: true,
                            default_branch_tip: false,
                            gate_jobs: false,
                            platforms: Vec::new(),
                        },
                    )
                    .await
                    .unwrap();
                });
            } else {
                drop(runner);
            }
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

fn diagnostic_count() -> u64 {
    UnreachableStep::ALL.iter().map(|step| count(*step)).sum()
}

#[tokio::test]
async fn retries_closed_handshakes_until_runner_answers_without_recording_unreachable() {
    let connector = HelloConnector::new(2, PROTOCOL_VERSION);
    let before = diagnostic_count();

    Channel::open_with_cause(&connector, Uuid::now_v7())
        .await
        .unwrap();

    assert_eq!(connector.connects.load(Ordering::SeqCst), 3);
    assert_eq!(diagnostic_count(), before);
}

#[tokio::test(start_paused = true)]
async fn exhausted_closed_handshakes_record_one_unreachable_diagnostic() {
    let connector = HelloConnector::new(usize::MAX, PROTOCOL_VERSION);
    let before = diagnostic_count();
    let started = tokio::time::Instant::now();

    let result = Channel::open_with_cause(&connector, Uuid::now_v7()).await;
    let (reason, detail) = match result {
        Err(error) => error,
        Ok(_) => panic!("the closed runner unexpectedly completed its handshake"),
    };

    assert_eq!(reason, RefusalReason::Unreachable);
    assert!(detail.contains("no runner hello within 15s"));
    assert!(
        detail.contains("still waiting in hello_send")
            || detail.contains("still waiting in hello_recv")
    );
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    assert_eq!(diagnostic_count() - before, 1);
}

#[tokio::test]
async fn version_mismatch_does_not_retry_the_handshake() {
    let connector = HelloConnector::new(0, PROTOCOL_VERSION + 1);

    let result = Channel::open_with_cause(&connector, Uuid::now_v7()).await;
    let (reason, _) = match result {
        Err(error) => error,
        Ok(_) => panic!("the mismatched runner unexpectedly completed its handshake"),
    };

    assert_eq!(reason, RefusalReason::RunnerVersionMismatch);
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn handshake_does_not_send_heartbeats_before_the_hello_reply() {
    let connector =
        HelloConnector::new(0, PROTOCOL_VERSION).with_reply_delay(Duration::from_millis(1500));

    Channel::open_with_cause(&connector, Uuid::now_v7())
        .await
        .unwrap();
}
