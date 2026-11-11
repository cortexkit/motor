use super::*;

#[tokio::test]
async fn emergency_control_is_refused_on_unleased_and_leased_job_channels() {
    for per_job in [false, true] {
        for purpose in [LeasePurpose::Run, LeasePurpose::Attach] {
            let f = Fixture::new();
            let mut config = f.config();
            config.manual_tick = true;
            config.tenant_vm = true;
            config.per_job = per_job;
            let runner = Runner::open(config).unwrap();
            for blocked in [true, false] {
                let operation = Frame::SetRemoteCacheWriteBlock { blocked };
                let (mut client, server) = tokio::io::duplex(4096);
                let task = tokio::spawn(crate::server::tenant_channel(runner.clone(), server));
                motor_protocol::write_frame_async(&mut client, &operation)
                    .await
                    .unwrap();
                assert!(frame(&mut client).await.is_none());
                task.await.unwrap().unwrap();

                let id = Uuid::now_v7();
                let (mut client, task) = connect(&runner, lease(id, purpose)).await;
                motor_protocol::write_frame_async(&mut client, &operation)
                    .await
                    .unwrap();
                assert!(frame(&mut client).await.is_none());
                task.await.unwrap().unwrap();
                assert!(!runner.has_job_record(id));
            }
        }
    }
}

#[tokio::test]
async fn emergency_control_is_refused_on_guest_event_channel() {
    let f = Fixture::new();
    let runner = open(&f);
    let (mut client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move { crate::tenant::event_session(&runner, server).await });
    motor_protocol::write_frame_async(
        &mut client,
        &Frame::SetRemoteCacheWriteBlock { blocked: true },
    )
    .await
    .unwrap();
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn emergency_control_is_acknowledged_on_local_owner_admin_channel() {
    let f = Fixture::new();
    let runner = open(&f);
    let (path, task) = super::super::listener(&f, &runner);
    for blocked in [true, false] {
        let mut client = tokio::net::UnixStream::connect(&*path).await.unwrap();
        motor_protocol::write_frame_async(&mut client, &hello())
            .await
            .unwrap();
        assert!(matches!(
            motor_protocol::read_frame_async(&mut client).await.unwrap(),
            Some(Frame::Hello { .. })
        ));
        motor_protocol::write_frame_async(
            &mut client,
            &Frame::SetRemoteCacheWriteBlock { blocked },
        )
        .await
        .unwrap();
        assert_eq!(
            motor_protocol::read_frame_async(&mut client).await.unwrap(),
            Some(Frame::SetRemoteCacheWriteBlockReply { blocked })
        );
    }
    task.abort();
    let _ = task.await;
}
