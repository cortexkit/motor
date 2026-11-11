use super::*;

fn creator(h: &Harness, job: &str) -> Arc<Binding> {
    h.gateway.job_create_binding(job, TENANT)
}

#[test]
fn runner_settings_supply_gateway_write_limits() {
    let mut config = crate::Config::new("unused-write-budget-state", "main");
    assert_eq!(Limits::for_config(&config), Limits::default());
    assert_eq!(config.remote_cache_max_job_put_objects, 5_000);
    assert_eq!(
        config.remote_cache_max_job_put_bytes,
        8 * 1024 * 1024 * 1024
    );
    assert_eq!(config.remote_cache_max_job_puts, 16);
    config.remote_cache_max_job_put_objects = 7;
    config.remote_cache_max_job_put_bytes = 11;
    config.remote_cache_max_job_puts = 2;
    let limits = Limits::for_config(&config);
    assert_eq!(limits.max_job_put_objects, 7);
    assert_eq!(limits.max_job_put_bytes, 11);
    assert_eq!(limits.max_job_puts, 2);
}

#[tokio::test(start_paused = true)]
async fn object_budget_survives_connections_socket_replacement_and_key_rotation() {
    let h = Harness::with_limits(Limits {
        max_job_put_objects: 2,
        ..Limits::default()
    });
    let binding = creator(&h, "job-a");
    assert_eq!(h.put(&binding, &s3("one"), b"one").await.status, 200);
    assert_eq!(h.put(&binding, &s3("two"), b"two").await.status, 200);
    let connects = h.store.connects();
    assert_eq!(h.put(&binding, &s3("three"), b"three").await.status, 503);
    assert_eq!(h.store.connects(), connects);
    drop(binding);
    h.generations.set(Some(generation("2", TENANT)));
    let binding = creator(&h, "job-a");
    let mut conn = h.open(&binding);
    conn.write_all(format!("PUT {} HTTP/1.1\r\ncontent-length: 0\r\n\r\n", s3("four")).as_bytes())
        .await
        .unwrap();
    let reply = read_reply(&mut conn, false).await.unwrap();
    assert_eq!(reply.status, 503);
    assert_ne!(reply.header("connection"), Some("close"));
    assert_eq!(h.store.connects(), connects);
    conn.write_all(format!("GET {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("one")).as_bytes())
        .await
        .unwrap();
    let reply = read_reply(&mut conn, false).await.unwrap();
    assert_eq!((reply.status, reply.body), (200, b"one".to_vec()));
    assert_eq!(h.store.seen().last().unwrap().access_key_id(), "READ-2");
    assert_eq!(h.gateway.job_put_totals("job-a").refused, 2);
    assert_eq!(h.gateway.stats(Tool::Sccache).write_refusals, 2);
    assert_eq!(
        h.put(&creator(&h, "job-b"), &s3("other"), b"v")
            .await
            .status,
        200
    );
}

#[tokio::test(start_paused = true)]
async fn byte_budget_checks_content_length_and_rejected_puts_reserve_nothing() {
    let h = Harness::with_limits(Limits {
        max_job_put_objects: 1,
        max_job_put_bytes: 5,
        ..Limits::default()
    });
    let binding = creator(&h, "job-a");
    // A refused head must not connect, wait for the body, or consume an object.
    let reply = h
        .raw(
            &binding,
            format!(
                "PUT {} HTTP/1.1\r\ncontent-length: 6\r\n\r\n",
                s3("too-big")
            )
            .as_bytes(),
            false,
        )
        .await;
    assert_eq!(reply.status, 503);
    assert_eq!(h.store.connects(), 0);
    assert_eq!(h.put(&binding, &s3("fits"), b"12345").await.status, 200);
    assert_eq!(h.store.connects(), 1);
    assert_eq!(h.gateway.job_put_totals("job-a").accepted_bytes, 5);
    assert_eq!(h.gateway.job_put_totals("job-a").refused, 1);
}

#[tokio::test(start_paused = true)]
async fn duplicates_and_failures_keep_object_and_byte_reservations() {
    for objects in [true, false] {
        let h = Harness::with_limits(Limits {
            max_job_put_objects: if objects { 2 } else { 10 },
            max_job_put_bytes: if objects { 100 } else { 6 },
            ..Limits::default()
        });
        let binding = creator(&h, "job-a");
        h.store
            .put_object(&stored(Tool::Sccache, "duplicate"), b"original");
        assert_eq!(h.put(&binding, &s3("duplicate"), b"123").await.status, 200);
        h.store.status(Some(409));
        assert_eq!(h.put(&binding, &s3("failure"), b"456").await.status, 503);
        h.store.status(None);
        let connects = h.store.connects();
        h.generations.set(Some(generation("2", TENANT)));
        let binding = creator(&h, "job-a");
        assert_eq!(h.put(&binding, &s3("over-budget"), b"7").await.status, 503);
        assert_eq!(h.store.connects(), connects);
        assert_eq!(
            h.gateway.job_put_totals("job-a"),
            JobPutTotals {
                duplicate: 1,
                failed: 1,
                refused: 1,
                ..JobPutTotals::default()
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_puts_never_overshoot_object_or_byte_budgets() {
    for objects in [true, false] {
        let h = Harness::with_limits(Limits {
            max_job_put_objects: if objects { 2 } else { 100 },
            max_job_put_bytes: if objects { 100 } else { 7 },
            ..Limits::default()
        });
        let barrier = Arc::new(tokio::sync::Barrier::new(12));
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..12 {
            let (h, barrier) = (h.clone(), barrier.clone());
            tasks.spawn(async move {
                let binding = creator(&h, "job-a");
                barrier.wait().await;
                let mut conn = h.open(&binding);
                conn.write_all(
                    format!(
                        "PUT {} HTTP/1.1\r\ncontent-length: 3\r\nconnection: close\r\n\r\n123",
                        s3(&format!("key-{i}"))
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
                let reply = read_reply(&mut conn, false).await.unwrap();
                // EOF confirms that request accounting has ended on the
                // serving task before the parent checks the totals.
                assert_eq!(conn.read(&mut [0]).await.unwrap(), 0);
                reply.status
            });
        }
        let mut accepted = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                200 => accepted += 1,
                status => assert_eq!(status, 503),
            }
        }
        assert_eq!(accepted, 2);
        assert_eq!(h.store.connects(), 2);
        assert_eq!(h.store.seen_count(), 2);
        let totals = h.gateway.job_put_totals("job-a");
        assert_eq!(
            (totals.accepted, totals.accepted_bytes, totals.refused),
            (2, 6, 10)
        );
    }
}

async fn hold_upload(h: &Harness, binding: &Arc<Binding>, key: &str) -> DuplexStream {
    let mut conn = h.open(binding);
    conn.write_all(
        format!(
            "PUT {} HTTP/1.1\r\ncontent-length: 1\r\nexpect: 100-continue\r\n\r\n",
            s3(key)
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    assert_eq!(read_reply(&mut conn, false).await.unwrap().status, 100);
    conn
}

#[tokio::test(start_paused = true)]
async fn concurrent_put_cap_spans_sockets_and_rotation_and_releases_on_completion() {
    let h = Harness::with_limits(Limits {
        max_job_puts: 1,
        ..Limits::default()
    });
    let binding = creator(&h, "job-a");
    let mut held = hold_upload(&h, &binding, "held").await;
    h.generations.set(Some(generation("2", TENANT)));
    let another = creator(&h, "job-a");
    for key in ["refused", SCCACHE_CHECK_KEY] {
        let connects = h.store.connects();
        assert_eq!(h.put(&another, &s3(key), b"v").await.status, 503);
        assert_eq!(h.store.connects(), connects);
    }
    assert_eq!(h.put(&h.writer(), &s3("fill"), b"fill").await.status, 200);
    assert_eq!(
        h.put(&creator(&h, "job-b"), &s3("other"), b"v")
            .await
            .status,
        200
    );
    assert_eq!(h.get(&another, &s3("fill")).await.status, 200);
    held.write_all(b"v").await.unwrap();
    assert_eq!(read_reply(&mut held, false).await.unwrap().status, 200);
    assert_eq!(h.put(&another, &s3("held"), b"duplicate").await.status, 200);
    h.store.status(Some(409));
    assert_eq!(h.put(&another, &s3("failed"), b"v").await.status, 503);
    h.store.status(None);
    assert_eq!(h.put(&another, &s3("after"), b"v").await.status, 200);
    assert_eq!(
        h.gateway.job_put_totals("job-a"),
        JobPutTotals {
            accepted: 2,
            accepted_bytes: 2,
            duplicate: 1,
            failed: 1,
            refused: 2,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_upload_releases_concurrency_but_keeps_its_budget() {
    let h = Harness::with_limits(Limits {
        max_job_puts: 1,
        max_job_put_objects: 2,
        max_job_put_bytes: 2,
        ..Limits::default()
    });
    let binding = creator(&h, "job-a");
    drop(hold_upload(&h, &binding, "cancelled").await);
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.gateway.job_put_totals("job-a").failed == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.put(&binding, &s3("after"), b"v").await.status, 200);
    let connects = h.store.connects();
    assert_eq!(h.put(&binding, &s3("exhausted"), b"v").await.status, 503);
    assert_eq!(h.store.connects(), connects);
}

#[tokio::test(start_paused = true)]
async fn sccache_check_is_create_only_but_exempt_from_object_and_byte_budgets() {
    let h = Harness::with_limits(Limits {
        max_job_put_objects: 0,
        max_job_put_bytes: 0,
        ..Limits::default()
    });
    let binding = creator(&h, "job-a");
    for body in [b"original".as_slice(), b"replacement"] {
        assert_eq!(
            h.put(&binding, &s3(SCCACHE_CHECK_KEY), body).await.status,
            200
        );
    }
    let seen = h.store.seen();
    assert_eq!(seen.len(), 2);
    for request in seen {
        assert_eq!(request.header("if-none-match"), Some("*"));
        assert!(request
            .header("authorization")
            .unwrap()
            .contains("if-none-match"));
    }
    let got = h.get(&binding, &s3(SCCACHE_CHECK_KEY)).await;
    assert_eq!((got.status, got.body), (200, b"original".to_vec()));
    let connects = h.store.connects();
    assert_eq!(h.put(&binding, &s3("ordinary"), b"v").await.status, 503);
    assert_eq!(h.store.connects(), connects);
    assert_eq!(h.gateway.job_put_totals("job-a").duplicate, 1);
}

#[tokio::test(start_paused = true)]
async fn fill_puts_ignore_all_job_write_limits() {
    let h = Harness::with_limits(Limits {
        max_job_put_objects: 0,
        max_job_put_bytes: 0,
        max_job_puts: 0,
        ..Limits::default()
    });
    let binding = creator(&h, "job-a");
    assert_eq!(
        h.put(&binding, &s3(SCCACHE_CHECK_KEY), b"v").await.status,
        503
    );
    assert_eq!(h.store.connects(), 0);
    for _ in 0..2 {
        assert_eq!(h.put(&h.writer(), &s3("fill"), b"fill").await.status, 200);
    }
    assert_eq!(h.get(&binding, &s3("fill")).await.status, 200);
    assert_eq!(h.gateway.job_put_totals("job-a").refused, 1);
}
