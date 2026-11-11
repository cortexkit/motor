use super::*;

fn creator(h: &Harness, job: &str) -> Arc<Binding> {
    h.gateway.job_create_binding(job, TENANT)
}

const CONDITIONS: [&str; 5] = [
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
    "if-range",
];

#[tokio::test(start_paused = true)]
async fn puts_sign_the_gateways_create_condition_and_ignore_the_jobs_conditions() {
    let h = Harness::new();
    let binding = creator(&h, "job-a");
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    for method in ["GET", "HEAD", "PUT"] {
        let conditions = CONDITIONS
            .iter()
            .map(|name| format!("{name}: job-chosen-value\r\n"))
            .collect::<String>();
        let request = format!(
            "{method} {} HTTP/1.1\r\nhost: x\r\n{conditions}content-length: 0\r\n\r\n",
            s3("k")
        );
        assert_eq!(
            h.raw(&binding, request.as_bytes(), method == "HEAD")
                .await
                .status,
            200
        );
    }
    assert_eq!(h.put(&binding, &s3("body"), b"bytes").await.status, 200);
    // A retry before any body byte was sent must carry the condition too.
    h.store.set(|k| k.reset_next = 1);
    assert_eq!(h.put(&binding, &s3("retry"), b"").await.status, 200);
    let seen = h.store.seen();
    assert_eq!(seen.len(), 6);
    for request in &seen {
        let is_put = request.method == "PUT";
        assert_eq!(
            request.access_key_id(),
            if is_put { "WRITE-1" } else { "READ-1" }
        );
        for name in CONDITIONS {
            let values: Vec<_> = request
                .headers
                .iter()
                .filter(|(key, _)| key == name)
                .collect();
            if name == "if-none-match" && is_put {
                assert_eq!(values.len(), 1);
                assert_eq!(values[0].1, "*");
            } else {
                assert!(values.is_empty(), "{name}: {request:?}");
            }
        }
        let authorization = request.header("authorization").unwrap();
        let signed = authorization
            .split("SignedHeaders=")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap();
        assert_eq!(
            signed.split(';').any(|name| name == "if-none-match"),
            is_put
        );
    }
    assert_eq!(seen[3].body, b"bytes");
    assert_eq!(h.put(&h.writer(), &s3("fill"), b"fill").await.status, 200);
    let fill = h.store.seen().pop().unwrap();
    for name in CONDITIONS {
        assert_eq!(fill.header(name), None);
    }
    assert!(!fill
        .header("authorization")
        .unwrap()
        .contains("if-none-match"));
}

#[tokio::test(start_paused = true)]
async fn reads_finish_while_a_fill_write_probe_is_held() {
    let h = Harness::new();
    let fill = h.writer();
    let binding = creator(&h, "job-a");
    h.store.put_object(&stored(Tool::Sccache, "k"), b"cached");
    open_write_breaker(&h, &fill).await;
    tokio::time::sleep(OPEN_WINDOW).await;
    h.store.set(|k| {
        k.status = None;
        k.respond_after = Some(Duration::from_secs(3));
    });
    let probe = {
        let h = h.clone();
        tokio::spawn(async move { h.put(&fill, &s3("probe"), b"v").await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!probe.is_finished());
    h.store.set(|k| k.respond_after = None);
    let started = Instant::now();
    let got = h.get(&binding, &s3("k")).await;
    assert_eq!((got.status, got.body), (200, b"cached".to_vec()));
    assert_eq!(h.head(&binding, &s3("k")).await.status, 200);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(!probe.is_finished());
    assert_eq!(probe.await.unwrap(), 200);
}

#[tokio::test(start_paused = true)]
async fn reads_share_job_and_gateway_caps_with_read_only_sockets() {
    let h = Harness::with_limits(Limits {
        max_job_requests: 1,
        max_total_requests: 2,
        ..Limits::default()
    });
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store.delay(Some(Duration::from_secs(4)));
    let held = {
        let (h, binding) = (h.clone(), creator(&h, "job-a"));
        tokio::spawn(async move { h.get(&binding, &s3("k")).await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let started = Instant::now();
    assert_eq!(h.head(&h.reader("job-a"), &s3("k")).await.status, 404);
    assert_eq!(h.get(&creator(&h, "job-a"), &s3("k")).await.status, 404);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(h.store.seen_count(), 1);
    let other = {
        let (h, binding) = (h.clone(), h.reader("job-b"));
        tokio::spawn(async move { h.get(&binding, &s3("k")).await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.head(&creator(&h, "job-c"), &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), 2);
    assert_eq!(h.gateway.stats(Tool::Sccache).read_refusals, 3);
    assert_eq!(held.await.unwrap(), 200);
    assert_eq!(other.await.unwrap(), 200);
    assert_eq!(h.get(&creator(&h, "job-a"), &s3("k")).await.status, 200);
}

#[tokio::test(start_paused = true)]
async fn job_puts_share_the_jobs_connection_cap() {
    let h = Harness::with_limits(Limits {
        max_job_connections: 1,
        ..Limits::default()
    });
    h.store.delay(Some(Duration::from_secs(4)));
    let binding = creator(&h, "job-a");
    let held = {
        let (h, binding) = (h.clone(), binding.clone());
        tokio::spawn(async move { h.put(&binding, &s3("k"), b"v").await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.store.seen_count(), 1);
    assert!(h.try_open(&creator(&h, "job-a")).is_none());
    let ccache = h
        .gateway
        .read_binding("job-a", TENANT, Tool::Ccache, Adapter::Http);
    assert!(h.try_open(&ccache).is_none());
    assert!(h.try_open(&creator(&h, "job-b")).is_some());
    assert_eq!(held.await.unwrap(), 200);
    assert!(h.try_open(&binding).is_some());
}

#[tokio::test(start_paused = true)]
async fn job_write_failures_do_not_open_fill_or_read_breakers_and_rotation_preserves_reads() {
    let h = Harness::new();
    let binding = creator(&h, "job-a");
    h.store.put_object(&stored(Tool::Sccache, "k"), b"cached");
    h.store.status(Some(500));
    for _ in 0..FAILURES_TO_OPEN {
        assert_eq!(h.put(&binding, &s3("bad"), b"v").await.status, 503);
    }
    assert!(h.gateway.shared.job_write_breaker.open_until().is_some());
    assert_eq!(h.gateway.write_breaker_open_until(), None);
    assert_eq!(h.gateway.read_breaker_open_until(), None);
    h.store.status(None);
    let seen = h.store.seen_count();
    assert_eq!(h.put(&binding, &s3("open"), b"v").await.status, 503);
    assert_eq!(h.store.seen_count(), seen);
    assert_eq!(h.get(&binding, &s3("k")).await.body, b"cached");
    assert_eq!(h.head(&binding, &s3("k")).await.status, 200);
    assert_eq!(h.put(&h.writer(), &s3("fill"), b"v").await.status, 200);
    let mut rotated = generation("2", TENANT);
    rotated.write_key = None;
    h.generations.set(Some(rotated));
    assert_eq!(h.get(&binding, &s3("k")).await.status, 200);
    assert_eq!(h.head(&binding, &s3("k")).await.status, 200);
    assert_eq!(h.store.seen().last().unwrap().access_key_id(), "READ-2");
    let seen = h.store.seen_count();
    assert_eq!(h.put(&binding, &s3("no-key"), b"v").await.status, 503);
    assert_eq!(h.store.seen_count(), seen);
}

#[tokio::test(start_paused = true)]
async fn read_failures_are_misses_and_use_the_read_breaker() {
    let h = Harness::new();
    let binding = creator(&h, "job-a");
    h.store.status(Some(500));
    for _ in 0..FAILURES_TO_OPEN {
        assert_eq!(h.head(&binding, &s3("k")).await.status, 404);
    }
    assert!(h.gateway.read_breaker_open_until().is_some());
    assert_eq!(h.gateway.write_breaker_open_until(), None);
    assert_eq!(h.gateway.shared.job_write_breaker.open_until(), None);
    h.store.status(None);
    let seen = h.store.seen_count();
    assert_eq!(h.get(&binding, &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), seen);
    assert_eq!(h.put(&binding, &s3("k"), b"v").await.status, 200);
}

#[tokio::test(start_paused = true)]
async fn job_create_refuses_delete_list_and_prefix_escapes_without_contacting_the_store() {
    let h = Harness::new();
    let binding = creator(&h, "job-a");
    for (method, path) in [
        ("DELETE", s3("k")),
        ("GET", format!("/{BUCKET}?list-type=2")),
        ("PUT", s3("k?uploadId=x")),
        ("PUT", "/another-bucket/key".into()),
        ("GET", s3("../ccache/k")),
        ("PUT", s3("../ccache/k")),
        ("PUT", s3("%2e%2e/ccache/k")),
        ("PUT", "/ccache/key".into()),
    ] {
        let request = format!("{method} {path} HTTP/1.1\r\nhost: x\r\ncontent-length: 0\r\n\r\n");
        assert_eq!(
            h.raw(&binding, request.as_bytes(), false).await.status,
            403,
            "{method} {path}"
        );
    }
    assert_eq!(h.store.connects(), 0);
    h.generations.set(Some(generation("2", "other-tenant")));
    assert_eq!(h.get(&binding, &s3("k")).await.status, 404);
    assert_eq!(h.put(&binding, &s3("k"), b"v").await.status, 503);
    assert_eq!(h.store.connects(), 0);
}
