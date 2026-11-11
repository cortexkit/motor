use super::*;

fn operation() -> Frame {
    Frame::RemoteCacheCreateOnlyProbe {
        account: "acct:test".into(),
    }
}

#[tokio::test]
async fn store_probe_is_refused_on_unleased_and_leased_job_channels() {
    for per_job in [false, true] {
        for purpose in [LeasePurpose::Run, LeasePurpose::Attach] {
            let f = Fixture::new();
            let mut config = f.config();
            config.manual_tick = true;
            config.tenant_vm = true;
            config.per_job = per_job;
            let runner = Runner::open(config).unwrap();
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn(crate::server::tenant_channel(runner.clone(), server));
            motor_protocol::write_frame_async(&mut client, &operation())
                .await
                .unwrap();
            assert!(frame(&mut client).await.is_none());
            task.await.unwrap().unwrap();

            let id = Uuid::now_v7();
            let (mut client, task) = connect(&runner, lease(id, purpose)).await;
            motor_protocol::write_frame_async(&mut client, &operation())
                .await
                .unwrap();
            assert!(frame(&mut client).await.is_none());
            task.await.unwrap().unwrap();
            assert!(!runner.has_job_record(id));
        }
    }
}

#[tokio::test]
async fn store_probe_is_refused_on_guest_event_channel() {
    let f = Fixture::new();
    let runner = open(&f);
    let (mut client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move { crate::tenant::event_session(&runner, server).await });
    motor_protocol::write_frame_async(&mut client, &operation())
        .await
        .unwrap();
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn owner_admin_gets_a_nonzero_report_when_cache_is_unavailable() {
    let f = Fixture::new();
    let runner = open(&f);
    let (path, task) = super::super::listener(&f, &runner);
    let mut client = tokio::net::UnixStream::connect(&*path).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), client.readable())
        .await
        .unwrap()
        .unwrap();
    motor_protocol::write_frame_async(&mut client, &hello())
        .await
        .unwrap();
    assert!(matches!(
        frame(&mut client).await,
        Some(Frame::Hello { .. })
    ));
    motor_protocol::write_frame_async(&mut client, &operation())
        .await
        .unwrap();
    assert_eq!(
        frame(&mut client).await,
        Some(Frame::RemoteCacheCreateOnlyProbeReply(
            motor_protocol::CreateOnlyProbeReport::refused(
                motor_protocol::CreateOnlyProbeDetail::RemoteCacheUnavailable
            )
        ))
    );
    task.abort();
    let _ = task.await;
}
