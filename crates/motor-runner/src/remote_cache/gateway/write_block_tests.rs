use super::*;

async fn put_event(store: &FakeStore, path: &str, interrupted: bool) {
    loop {
        let changed = store.upload_changed.notified();
        let events = if interrupted {
            &store.interrupted_puts
        } else {
            &store.put_heads
        };
        if events.lock().unwrap().iter().any(|value| value == path) {
            return;
        }
        changed.await;
    }
}

#[tokio::test(start_paused = true)]
async fn block_between_keepalive_puts_denies_checks_and_clearing_restores_writes() {
    let h = Harness::with_limits(Limits {
        max_job_put_objects: 2,
        ..Limits::default()
    });
    let job = h.gateway.job_create_binding("job", TENANT);
    let mut client = h.open(&job);
    client
        .write_all(format!("PUT {} HTTP/1.1\r\ncontent-length: 0\r\n\r\n", s3("one")).as_bytes())
        .await
        .unwrap();
    assert_eq!(read_reply(&mut client, false).await.unwrap().status, 200);
    h.gateway.set_job_write_block(true).await;
    let connects = h.store.connects();
    for key in ["two", SCCACHE_CHECK_KEY] {
        client
            .write_all(format!("PUT {} HTTP/1.1\r\ncontent-length: 0\r\n\r\n", s3(key)).as_bytes())
            .await
            .unwrap();
        assert_eq!(read_reply(&mut client, false).await.unwrap().status, 503);
        assert_eq!(h.store.connects(), connects);
    }
    assert_eq!(h.gateway.job_put_totals("job").refused, 2);
    assert_eq!(h.gateway.stats(Tool::Sccache).write_refusals, 2);
    assert_eq!(h.get(&job, &s3("one")).await.status, 200);
    assert_eq!(h.put(&h.writer(), &s3("fill"), b"fill").await.status, 200);
    h.gateway.set_job_write_block(false).await;
    assert_eq!(h.put(&job, &s3("two"), b"job").await.status, 200);
    assert_eq!(
        h.put(&h.reader("readonly"), &s3("never"), b"v")
            .await
            .status,
        403
    );
}

#[tokio::test(start_paused = true)]
async fn acknowledgement_closes_partial_upload_before_last_byte_and_never_publishes() {
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
    put_event(&h.store, &stored(Tool::Sccache, "partial"), false).await;
    h.gateway.set_job_write_block(true).await;
    h.gateway.set_job_write_block(false).await;
    assert_eq!(read_reply(&mut client, false).await.unwrap().status, 503);
    put_event(&h.store, &stored(Tool::Sccache, "partial"), true).await;
    assert_eq!(h.head(&job, &s3("partial")).await.status, 404);
    assert_eq!(h.gateway.job_put_totals("job").refused, 1);
    assert_eq!(h.gateway.job_put_totals("job").failed, 0);
    assert_eq!(h.put(&job, &s3("partial"), b"full").await.status, 200);
    assert_eq!(h.get(&job, &s3("partial")).await.body, b"full");
}

#[tokio::test(start_paused = true)]
async fn fully_sent_put_waiting_for_store_response_completes_while_blocked() {
    let h = Harness::new();
    h.store.delay(Some(Duration::from_secs(2)));
    let job = h.gateway.job_create_binding("job", TENANT);
    let mut client = h.open(&job);
    client
        .write_all(
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 4\r\n\r\nfull",
                s3("complete")
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    while h.store.seen_count() == 0 {
        tokio::task::yield_now().await;
    }
    h.gateway.set_job_write_block(true).await;
    assert_eq!(read_reply(&mut client, false).await.unwrap().status, 200);
    assert_eq!(h.gateway.job_put_totals("job").accepted, 1);
    h.store.delay(None);
    assert_eq!(h.get(&job, &s3("complete")).await.body, b"full");
}
