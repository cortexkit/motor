use super::*;
use crate::transport::Duplex;
use cortexkit_test_support::ScratchDir;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

/// A build server whose job stream sends what the test asks for, and whose
/// cancel connections hold their reply until the test releases them.
#[derive(Default)]
struct CancelPeer {
    attaches: AtomicUsize,
    cancels: AtomicUsize,
    cancel_reached: Notify,
    cancel_release: Notify,
    next_output: Notify,
    terminal: Notify,
}

struct Peer(Arc<CancelPeer>);

impl Connector for Peer {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let peer = self.0.clone();
        Box::pin(async move {
            let (client, mut stream) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut stream).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut stream,
                    &Frame::Hello {
                        return_paths: false,
                        return_chunked: false,
                        sibling_paths: false,
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
                let request = loop {
                    match read_frame_async(&mut stream).await.unwrap() {
                        Some(Frame::Heartbeat { .. }) => continue,
                        other => break other,
                    }
                };
                match request {
                    Some(Frame::Attach(_)) => {
                        peer.attaches.fetch_add(1, Ordering::SeqCst);
                        for seq in 0..2 {
                            if seq > 0 {
                                peer.next_output.notified().await;
                            }
                            write_frame_async(
                                &mut stream,
                                &Frame::Output(Output::new(
                                    seq,
                                    OutputStream::Stdout,
                                    BytePayload(b"output".to_vec()),
                                )),
                            )
                            .await
                            .unwrap();
                        }
                        peer.terminal.notified().await;
                        write_frame_async(
                            &mut stream,
                            &Frame::Terminal(
                                TerminalRecord::new(id, Outcome::Exit { code: 143 }, 2, 0, 0)
                                    .with_ran(Ran::Remote),
                            ),
                        )
                        .await
                        .unwrap();
                    }
                    Some(Frame::Cancel(request)) => {
                        peer.cancels.fetch_add(1, Ordering::SeqCst);
                        peer.cancel_reached.notify_one();
                        peer.cancel_release.notified().await;
                        write_frame_async(
                            &mut stream,
                            &Frame::CancelReply(CancelReply::new(request.job_id)),
                        )
                        .await
                        .unwrap();
                    }
                    other => panic!("unexpected request {other:?}"),
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

async fn next(stream: &mut JobStream) -> StreamRecord {
    tokio::time::timeout(Duration::from_secs(10), stream.records.recv())
        .await
        .expect("the job stream stalled")
        .unwrap()
        .unwrap()
}

/// While `cancel` waits for the runner to acknowledge a cancel, the job's
/// stream keeps reading on its connection. If it sent a second cancel on
/// another connection, it would wait for that reply and then reattach before
/// it could read the terminal.
#[tokio::test]
async fn job_stream_keeps_its_connection_while_cancel_is_delivered() {
    let scratch = ScratchDir::new("motor-cancel-delivery");
    let peer = Arc::new(CancelPeer::default());
    let motor =
        Motor::with_connector(Config::default(), &scratch, Arc::new(Peer(peer.clone()))).unwrap();
    let id = Uuid::now_v7();
    motor.storage.create(id, 60, None, None, false).unwrap();
    let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
    assert!(matches!(next(&mut stream).await, StreamRecord::Output(_)));

    let cancelling = tokio::spawn({
        let motor = motor.clone();
        async move { motor.cancel(CancelRequest::new(id)).await }
    });
    tokio::time::timeout(Duration::from_secs(10), peer.cancel_reached.notified())
        .await
        .expect("the cancel never reached the runner");
    assert!(motor.pending(id));

    // A frame on the job stream lets its loop see the pending cancel.
    peer.next_output.notify_one();
    assert!(matches!(next(&mut stream).await, StreamRecord::Output(_)));
    peer.terminal.notify_one();
    assert!(
        matches!(next(&mut stream).await, StreamRecord::Terminal(_)),
        "the terminal arrives on the stream's own connection"
    );
    assert_eq!(
        peer.cancels.load(Ordering::SeqCst),
        1,
        "one cancel connection"
    );
    assert_eq!(peer.attaches.load(Ordering::SeqCst), 1, "no reattach");

    peer.cancel_release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), cancelling)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        !motor.pending(id),
        "the acknowledged cancel is no longer pending"
    );
    assert!(motor.cancels_in_flight.lock().unwrap().is_empty());
}
