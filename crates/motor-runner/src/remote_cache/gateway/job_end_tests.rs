use super::*;

async fn upload_started(store: &FakeStore) {
    loop {
        let changed = store.upload_changed.notified();
        if !store.put_heads.lock().unwrap().is_empty() {
            return;
        }
        changed.await;
    }
}

#[tokio::test]
async fn job_end_removes_totals_and_closes_every_socket_before_reporting() {
    let scratch = cortexkit_test_support::ScratchDir::new("job-cache-end");
    let h = Harness::new();
    let job = h.gateway.job_create_binding("job", TENANT);
    let reader = h.reader("job");
    let write_path = scratch.join("write.sock");
    let read_path = scratch.join("read.sock");
    let _writer = h.gateway.listen(&write_path, job.clone()).unwrap();
    let _reader = h.gateway.listen(&read_path, reader.clone()).unwrap();
    let mut client = tokio::net::UnixStream::connect(&write_path).await.unwrap();
    client
        .write_all(
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 4\r\n\r\nfull",
                s3("one")
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(read_reply(&mut client, false).await.unwrap().status, 200);
    let other = h.gateway.job_create_binding("other", TENANT);
    assert_eq!(h.put(&other, &s3("two"), b"other").await.status, 200);

    assert_eq!(
        h.gateway.finish_job("job").await,
        JobPutTotals {
            accepted: 1,
            accepted_bytes: 4,
            ..Default::default()
        }
    );
    assert!(!write_path.exists());
    assert!(!read_path.exists());
    assert!(tokio::net::UnixStream::connect(&write_path).await.is_err());
    let _ = client
        .write_all(
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 4\r\n\r\nlate",
                s3("late")
            )
            .as_bytes(),
        )
        .await;
    assert!(read_reply(&mut client, false).await.is_none());
    assert!(h.try_open(&job).is_none());
    assert!(h.try_open(&reader).is_none());
    let rebound = h.gateway.job_create_binding("job", TENANT);
    assert!(h.try_open(&rebound).is_none());
    assert!(!h
        .gateway
        .shared
        .job_puts
        .lock()
        .unwrap()
        .contains_key("job"));
    assert_eq!(h.gateway.job_put_totals("other").accepted, 1);
    assert_eq!(h.get(&other, &s3("one")).await.body, b"full");
}

#[tokio::test(start_paused = true)]
async fn job_end_cancels_a_put_that_never_finishes_and_reports_its_guard() {
    let h = Harness::new();
    let job = h.gateway.job_create_binding("job", TENANT);
    let mut client = h.open(&job);
    client
        .write_all(
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 4\r\n\r\nabc",
                s3("partial")
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    upload_started(&h.store).await;
    let began = Instant::now();
    assert_eq!(h.gateway.finish_job("job").await.failed, 1);
    assert!(began.elapsed() < Duration::from_secs(3));
    assert!(read_reply(&mut client, false).await.is_none());
    assert!(!h
        .gateway
        .shared
        .job_puts
        .lock()
        .unwrap()
        .contains_key("job"));
}

#[tokio::test(start_paused = true)]
async fn job_end_bounds_a_stalled_put_guard_and_late_accounting_cannot_recreate_entry() {
    let h = Harness::new();
    let job = h.gateway.job_create_binding("job", TENANT);
    let state = job.job_puts.as_ref().unwrap().clone();
    let (mut client, server) = tokio::io::duplex(4096);
    let connection = h.gateway.admit_connection(&job, server).unwrap();
    tokio::pin!(connection);
    client
        .write_all(
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 4\r\n\r\nabc",
                s3("partial")
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::select! {
        _ = &mut connection => panic!("partial upload unexpectedly finished"),
        _ = upload_started(&h.store) => {},
    }
    // Leave the connection future unpolled to model a stalled executor that
    // cannot yet process cancellation or drop its in-flight PUT guard.
    let began = Instant::now();
    assert_eq!(h.gateway.finish_job("job").await, JobPutTotals::default());
    assert_eq!(began.elapsed(), Duration::from_secs(3));
    assert!(!h
        .gateway
        .shared
        .job_puts
        .lock()
        .unwrap()
        .contains_key("job"));
    connection.await;
    assert_eq!(state.lock().unwrap().totals.failed, 1);
    assert!(h.try_open(&job).is_none());
    assert!(!h
        .gateway
        .shared
        .job_puts
        .lock()
        .unwrap()
        .contains_key("job"));
}
