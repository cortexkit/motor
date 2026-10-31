use super::*;
use crate::transport::Duplex;
use cortexkit_test_support::ScratchDir;
use std::{future::Future, pin::Pin, sync::Mutex};

struct ReplayPeer {
    sessions: Mutex<Vec<(u64, Vec<Frame>)>>,
}
impl Connector for ReplayPeer {
    fn connect(
        &self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let (cursor, frames) = self.sessions.lock().unwrap().remove(0);
        Box::pin(async move {
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        return_paths: false,
                        return_chunked_max_bytes: None,
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
                loop {
                    match read_frame_async(&mut peer).await.unwrap() {
                        Some(Frame::Heartbeat { .. }) => continue,
                        Some(Frame::Attach(a)) => {
                            assert_eq!(a.from_seq, cursor);
                            break;
                        }
                        other => panic!("expected attach: {other:?}"),
                    }
                }
                for frame in frames {
                    write_frame_async(&mut peer, &frame).await.unwrap();
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

async fn replay(from: u64, sessions: Vec<(u64, Vec<Frame>)>, id: Uuid) -> Vec<StreamRecord> {
    let scratch = ScratchDir::new("motor-started-replay");
    let motor = Motor::with_connector(
        Config::default(),
        &scratch,
        Arc::new(ReplayPeer {
            sessions: Mutex::new(sessions),
        }),
    )
    .unwrap();
    let mut stream = motor.attach(AttachRequest::new(id, from)).unwrap();
    let mut records = Vec::new();
    while let Some(record) = tokio::time::timeout(Duration::from_secs(10), stream.records.recv())
        .await
        .unwrap()
    {
        records.push(record.unwrap());
    }
    records
}

fn started(id: Uuid) -> Frame {
    Frame::Started {
        job_id: id,
        seq: 0,
        queue_wait_ms: 125,
        started_at_ms: 1_730_000_000_125,
        path_mapping: None,
    }
}
fn output(seq: u64) -> Frame {
    Frame::Output(Output::new(
        seq,
        OutputStream::Stdout,
        BytePayload(b"output".to_vec()),
    ))
}
fn terminal(id: Uuid) -> Frame {
    Frame::Terminal(
        TerminalRecord::new(id, Outcome::Exit { code: 0 }, 1, 125, 0).with_ran(Ran::Remote),
    )
}

#[tokio::test]
async fn started_advances_reconnect_cursor_and_duplicates_are_ignored() {
    let id = Uuid::now_v7();
    let records = replay(
        0,
        vec![
            (0, vec![started(id), started(id)]),
            (1, vec![started(id), output(1), terminal(id)]),
        ],
        id,
    )
    .await;
    assert_eq!(records.len(), 3);
    assert_eq!(
        records[0],
        StreamRecord::Started(Started::new(0, 125, 1_730_000_000_125))
    );
    assert!(matches!(&records[1], StreamRecord::Output(o) if o.seq == 1));
    assert!(matches!(&records[2], StreamRecord::Terminal(_)));
}

#[tokio::test]
async fn attach_after_started_does_not_repeat_it() {
    let id = Uuid::now_v7();
    let records = replay(1, vec![(1, vec![started(id), output(1), terminal(id)])], id).await;
    assert_eq!(records.len(), 2);
    assert!(matches!(&records[0], StreamRecord::Output(o) if o.seq == 1));
    assert!(matches!(&records[1], StreamRecord::Terminal(_)));
}

#[tokio::test]
async fn windows_mapping_and_full_width_outcome_forward_unchanged() {
    let id = Uuid::now_v7();
    let mut frame = started(id);
    let Frame::Started { path_mapping, .. } = &mut frame else {
        unreachable!()
    };
    *path_mapping = Some(PathMapping::new("/", r"C:\m"));
    let terminal = TerminalRecord::new(id, Outcome::WindowsExit { code: 0xC000_0005 }, 1, 125, 0)
        .with_ran(Ran::Remote);
    let records = replay(
        0,
        vec![(0, vec![frame, Frame::Terminal(terminal.clone())])],
        id,
    )
    .await;
    assert_eq!(
        records,
        vec![
            StreamRecord::Started(
                Started::new(0, 125, 1_730_000_000_125)
                    .with_path_mapping(PathMapping::new("/", r"C:\m"))
            ),
            StreamRecord::Terminal(terminal),
        ]
    );
}

#[tokio::test]
async fn new_ck_motor_old_runner_does_not_guess_started_from_heartbeats() {
    let id = Uuid::now_v7();
    let records = replay(
        0,
        vec![(
            0,
            vec![
                Frame::Heartbeat {
                    queue_wait_ms: Some(125),
                },
                Frame::Heartbeat {
                    queue_wait_ms: None,
                },
                output(0),
                terminal(id),
            ],
        )],
        id,
    )
    .await;
    assert_eq!(records.len(), 2);
    assert!(matches!(&records[0], StreamRecord::Output(o) if o.seq == 0));
    assert!(matches!(&records[1], StreamRecord::Terminal(_)));
}
