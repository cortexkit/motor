use super::*;
use crate::hosted::tests::harness::{Behavior, Harness, NoSsh, Scratch};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

struct DropPeer {
    drops: Arc<Mutex<Vec<DropRequest>>>,
    reply: DropReply,
}

impl Connector for DropPeer {
    fn connect(
        &self,
        _: Uuid,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + '_>,
    > {
        let drops = self.drops.clone();
        let reply = self.reply.clone();
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
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: true,
                        platforms: vec!["linux".into()],
                        return_paths: true,
                        return_chunked: false,
                        sibling_paths: false,
                    },
                )
                .await
                .unwrap();
                while let Some(frame) = read_frame_async(&mut peer).await.unwrap() {
                    match frame {
                        Frame::Heartbeat { .. } => {}
                        Frame::Drop(request) => {
                            drops.lock().unwrap().push(request);
                            write_frame_async(&mut peer, &Frame::DropReply(reply))
                                .await
                                .unwrap();
                            return;
                        }
                        other => panic!("unexpected direct frame: {other:?}"),
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn crate::Duplex>)
        })
    }
}

#[tokio::test]
async fn hosted_drop_reaches_direct_runner_without_creating_jobs() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    for fleet in [false, true] {
        for dropped in [false, true] {
            let mut config = h.config();
            config.hosted_fleet_routing = fleet;
            let drops = Arc::new(Mutex::new(Vec::new()));
            let cancelled = vec![Uuid::now_v7()];
            let motor = Motor::with_connector(
                config,
                &scratch.0.join(format!("drop-{fleet}-{dropped}")),
                Arc::new(DropPeer {
                    drops: drops.clone(),
                    reply: DropReply::new(dropped, cancelled.clone()),
                }),
            )
            .unwrap();
            let key = "/home/worktrees/project";
            let result = motor.drop_workspace(DropRequest::new(key)).await;
            assert_eq!(motor.storage.job_count(), 0);
            let reply = result.unwrap();
            assert_eq!(reply.dropped, dropped);
            assert_eq!(reply.cancelled_jobs, cancelled);
            let drops = drops.lock().unwrap();
            assert_eq!(drops.len(), 1);
            assert_eq!(drops[0].workspace_key, key);
        }
    }
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hosted_prepare_refuses_without_creating_jobs() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    for fleet in [false, true] {
        let mut config = h.config();
        config.hosted_fleet_routing = fleet;
        let motor = Motor::with_connector(
            config,
            &scratch.0.join(format!("prepare-{fleet}")),
            ssh.clone(),
        )
        .unwrap();
        let reply = motor
            .prepare(PrepareRequest::new(
                "/home/worktrees/project",
                "/home/worktrees/project",
                "HEAD",
            ))
            .await
            .unwrap();
        assert_eq!(
            reply.outcome,
            PrepareOutcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected,
            }
        );
        assert_eq!(
            reply.refusal_detail(),
            Some("hosted_prepare_unsupported: hosted_no_fallback")
        );
        assert!(motor.storage.job(reply.transfer_id).is_none());
        assert_eq!(motor.storage.job_count(), 0);
        assert!(!motor.spool_root.exists());
    }
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn hosted_drop_preserves_direct_connection_error_without_creating_jobs() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let mut errors = Vec::new();
    for (hosted, fleet) in [(false, false), (true, false), (true, true)] {
        let mut config = h.config();
        config.hosted_runners = hosted;
        config.hosted_fleet_routing = fleet;
        let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
        let motor = Motor::with_connector(
            config,
            &scratch.0.join(format!("unreachable-{hosted}-{fleet}")),
            ssh.clone(),
        )
        .unwrap();
        let error = motor
            .drop_workspace(DropRequest::new("/home/worktrees/project"))
            .await
            .unwrap_err();
        let attempts = ssh.0.load(Ordering::SeqCst);
        assert!(attempts > 0);
        assert_eq!(motor.storage.job_count(), 0);
        errors.push((error.kind(), error.to_string(), attempts));
    }
    assert_eq!(errors[0], errors[1]);
    assert_eq!(errors[0], errors[2]);
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
}
