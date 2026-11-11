use super::*;

fn creator(h: &Harness, job: &str) -> Arc<Binding> {
    h.gateway.job_create_binding(job, TENANT)
}

fn assert_empty_duplicate(reply: &Reply) {
    assert_eq!(reply.status, 200);
    assert!(reply.complete);
    assert!(reply.body.is_empty());
    assert_eq!(reply.header("content-length"), Some("0"));
    assert!(reply
        .names()
        .iter()
        .all(|name| ["content-length", "connection"].contains(name)));
}

#[tokio::test(start_paused = true)]
async fn fake_store_conditionally_creates_without_overwriting() {
    let store = Arc::new(FakeStore::default());
    let (mut client, server) = tokio::io::duplex(4096);
    tokio::spawn(store.clone().serve(server));
    for (condition, body, expected) in [
        ("if-none-match: *\r\n", "one", 200),
        ("if-none-match: *\r\n", "two", 412),
        ("", "new", 200),
    ] {
        let request = format!("PUT /key HTTP/1.1\r\n{condition}content-length: 3\r\n\r\n{body}");
        client.write_all(request.as_bytes()).await.unwrap();
        assert_eq!(
            read_reply(&mut client, false).await.unwrap().status,
            expected
        );
        assert_eq!(
            store.objects.lock().unwrap().get("/key").unwrap(),
            if expected == 412 {
                b"one"
            } else {
                body.as_bytes()
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn full_body_duplicate_is_empty_and_preserves_bytes_and_client_reuse() {
    let h = Harness::new();
    let binding = creator(&h, "job-a");
    assert_eq!(h.put(&binding, &s3("key"), b"original").await.status, 200);
    let mut client = h.open(&binding);
    client
        .write_all(
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 9\r\n\r\ndifferent",
                s3("key")
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let reply = read_reply(&mut client, false).await.unwrap();
    assert_empty_duplicate(&reply);
    assert_ne!(reply.header("connection"), Some("close"));
    client
        .write_all(format!("GET {} HTTP/1.1\r\n\r\n", s3("key")).as_bytes())
        .await
        .unwrap();
    assert_eq!(
        read_reply(&mut client, false).await.unwrap().body,
        b"original"
    );
    assert_eq!(
        h.gateway.job_put_totals("job-a"),
        JobPutTotals {
            accepted: 1,
            accepted_bytes: 8,
            duplicate: 1,
            ..JobPutTotals::default()
        }
    );
    assert_eq!(h.store.seen_count(), 3);
    assert!(h.store.seen()[..2]
        .iter()
        .all(|request| request.header("if-none-match") == Some("*")));
}

#[tokio::test(start_paused = true)]
async fn duplicate_is_a_breaker_success_after_full_or_early_body() {
    for early in [false, true] {
        let h = Harness::new();
        let binding = creator(&h, "job-a");
        h.store.status(Some(500));
        for _ in 0..FAILURES_TO_OPEN - 1 {
            assert_eq!(h.put(&binding, &s3("failed"), b"x").await.status, 503);
        }
        h.store.set(|knobs| {
            knobs.status = Some(412);
            knobs.early_status = early.then_some(412);
            knobs.reject_body_writes = early;
        });
        assert_empty_duplicate(&h.put(&binding, &s3("duplicate"), b"x").await);
        h.store.set(|knobs| {
            knobs.status = Some(500);
            knobs.early_status = None;
            knobs.reject_body_writes = false;
        });
        assert_eq!(h.put(&binding, &s3("failed"), b"x").await.status, 503);
        assert_eq!(h.gateway.shared.job_write_breaker.open_until(), None);
        assert_eq!(h.gateway.job_put_totals("job-a").duplicate, 1);
        assert_eq!(h.gateway.job_put_totals("job-a").accepted_bytes, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn non_success_answers_fail_once_without_replaying_the_body() {
    for status in [301, 400, 403, 404, 409, 429, 500, 503] {
        let h = Harness::new();
        let binding = creator(&h, "job-a");
        h.store.status(Some(status));
        let reply = h.put(&binding, &s3("key"), b"finished-output").await;
        assert_eq!(reply.status, 503, "store status {status}");
        assert!(reply.body.is_empty());
        assert_eq!(h.store.connects(), 1);
        let seen = h.store.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].body, b"finished-output");
        assert_eq!(seen[0].header("if-none-match"), Some("*"));
        assert_eq!(h.gateway.stats(Tool::Sccache).errors, 1);
        assert_eq!(
            h.gateway.job_put_totals("job-a"),
            JobPutTotals {
                failed: 1,
                ..JobPutTotals::default()
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn early_answers_close_instead_of_reusing_unread_body_bytes() {
    for status in [200, 409, 412, 500] {
        let h = Harness::with_limits(Limits {
            stream_buffer_bytes: 16 * 1024,
            ..Limits::default()
        });
        let binding = creator(&h, "job-a");
        h.store
            .put_object(&stored(Tool::Sccache, "key"), b"original");
        h.store.set(|knobs| knobs.early_status = Some(status));
        let mut client = h.try_open_sized(&binding, 256 * 1024).unwrap();
        let mut request = format!(
            "PUT {} HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            s3("key"),
            4 * MIB
        )
        .into_bytes();
        request.extend(vec![b'x'; 128 * 1024]);
        // A request-looking suffix is still part of the announced PUT body.
        request.extend_from_slice(format!("GET {} HTTP/1.1\r\n\r\n", s3("key")).as_bytes());
        client.write_all(&request).await.unwrap();
        let reply = read_reply(&mut client, false).await.unwrap();
        assert_eq!(reply.header("connection"), Some("close"));
        if status == 412 {
            assert_empty_duplicate(&reply);
        } else {
            assert_eq!(reply.status, if status == 200 { 200 } else { 503 });
        }
        assert!(read_to_close(&mut client).await.is_empty());
        assert_eq!(h.store.connects(), 1);
        let seen = h.store.seen();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].body.is_empty());
        assert_eq!(seen[0].header("if-none-match"), Some("*"));
        assert_eq!(
            h.store.objects.lock().unwrap()[&stored(Tool::Sccache, "key")],
            b"original"
        );
        let totals = h.gateway.job_put_totals("job-a");
        assert_eq!(totals.duplicate, u64::from(status == 412));
        assert_eq!(totals.failed, u64::from(status == 409 || status == 500));
    }
}

#[tokio::test(start_paused = true)]
async fn write_error_still_waits_for_an_answer_but_only_until_first_byte_timeout() {
    for delay in [Duration::from_secs(1), Duration::from_secs(6)] {
        let h = Harness::new();
        let binding = creator(&h, "job-a");
        h.store.set(|knobs| {
            knobs.early_status = Some(412);
            knobs.reject_body_writes = true;
            knobs.respond_after = Some(delay);
        });
        let start = Instant::now();
        let reply = h.put(&binding, &s3("key"), b"body").await;
        let elapsed = start.elapsed();
        if delay < h.gateway.shared.limits.first_byte_timeout {
            assert_empty_duplicate(&reply);
            assert!(around(elapsed, delay));
            assert_eq!(h.gateway.job_put_totals("job-a").duplicate, 1);
        } else {
            assert_eq!(reply.status, 503);
            assert!(around(elapsed, h.gateway.shared.limits.first_byte_timeout));
            assert_eq!(h.gateway.job_put_totals("job-a").failed, 1);
        }
        assert_eq!(reply.header("connection"), Some("close"));
        assert_eq!(h.store.connects(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn job_totals_include_refusals_and_survive_new_bindings_and_key_rotation() {
    let h = Harness::new();
    let binding = creator(&h, "job-a");
    for status in [201, 204, 299] {
        h.store.set(|knobs| {
            knobs.script = Some(vec![(
                format!("HTTP/1.1 {status} Stored\r\netag: accepted\r\ncontent-length: 0\r\n\r\n")
                    .into_bytes(),
                Duration::ZERO,
            )]);
        });
        let reply = h.put(&binding, &s3("accepted"), b"bytes").await;
        assert_eq!(reply.status, status);
        assert_eq!(reply.header("etag"), Some("accepted"));
    }
    assert_eq!(
        h.put(&binding, "/wrong-prefix", b"refused").await.status,
        403
    );
    h.store.status(Some(409));
    assert_eq!(h.put(&binding, &s3("failed"), b"x").await.status, 503);
    h.store.status(None);
    assert_eq!(
        h.put(&creator(&h, "job-b"), &s3("other"), b"other")
            .await
            .status,
        200
    );
    h.generations.set(Some(generation("2", TENANT)));
    drop(binding);
    let binding = creator(&h, "job-a");
    assert_eq!(
        h.put(&binding, &s3("accepted-again"), b"new").await.status,
        200
    );
    assert_eq!(h.store.seen().last().unwrap().access_key_id(), "WRITE-2");
    assert_eq!(
        h.gateway.job_put_totals("job-a"),
        JobPutTotals {
            accepted: 4,
            accepted_bytes: 18,
            failed: 1,
            refused: 1,
            duplicate: 0,
        }
    );
    assert_eq!(h.gateway.job_put_totals("job-b").accepted, 1);
    assert_eq!(h.gateway.job_put_totals("unknown"), JobPutTotals::default());
    assert_eq!(h.store.seen_count(), 6);
}

#[tokio::test(start_paused = true)]
async fn fill_duplicates_stay_failures_and_keep_the_write_ledger_unchanged() {
    let h = Harness::new();
    let writer = h.writer();
    assert_eq!(h.put(&writer, &s3("key"), b"original").await.status, 200);
    assert_eq!(h.put(&writer, &s3("key"), b"replacement").await.status, 200);
    h.store.status(Some(412));
    assert_eq!(h.put(&writer, &s3("key"), b"failed").await.status, 503);
    h.store.status(None);
    assert_eq!(h.get(&writer, &s3("key")).await.body, b"replacement");
    let ledger = writer.ledger().unwrap().summary();
    assert_eq!(ledger.puts_stored, 2);
    assert_eq!(ledger.puts_failed, 1);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 0);
    assert!(h
        .store
        .seen()
        .iter()
        .all(|request| request.header("if-none-match").is_none()));
    assert!(h.gateway.shared.job_puts.lock().unwrap().is_empty());
}
