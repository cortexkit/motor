//! Gateway tests against an in-memory object store. Most run on tokio's paused
//! clock, so first-byte limits, deadlines, throughput windows and breaker
//! windows elapse instantly and exactly.
use super::breaker::{FAILURES_TO_OPEN, OPEN_WINDOW};
use super::upstream::{BoxedIo, ConnectFuture};
use super::*;
use crate::remote_cache::provisioning::{derived_prefix, Generation, HostRole, KeyRole, Layout};
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream};

const TENANT: &str = "acct:01KXJJHK4V6DB42W1YAN2XD9QN";
const BUCKET: &str = "fleet-cache";
const CLASS: &str = "EXPRESS_ONEZONE";
/// What the fake store puts in error bodies; it must never reach a tool.
const LEAK: &str = "LEAKED-ACCESS-ID";
const MIB: usize = 1 << 20;

// ---------------------------------------------------------------------------
// The fake store
// ---------------------------------------------------------------------------

/// How the fake store behaves; tests change it between requests.
#[derive(Clone, Default)]
struct Knobs {
    /// Connections are refused.
    refuse: bool,
    /// Connecting never completes.
    blackhole: bool,
    /// Requests are read but never answered.
    silent: bool,
    /// This many more connections are closed right after their request is
    /// read, before any response byte.
    reset_next: usize,
    /// Every request is answered with this status and an error body.
    status: Option<u16>,
    /// Each answer waits this long first.
    respond_after: Option<Duration>,
    /// GET bodies are sent this many bytes at a time, with this pause after
    /// each piece.
    drip: Option<(usize, Duration)>,
    /// PUT bodies are read this many bytes at a time, with this pause after
    /// each piece.
    slow_put: Option<(usize, Duration)>,
}

/// One request the fake store received.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
    fn access_key_id(&self) -> &str {
        let authorization = self.header("authorization").unwrap();
        let rest = authorization
            .strip_prefix("AWS4-HMAC-SHA256 Credential=")
            .unwrap();
        &rest[..rest.find('/').unwrap()]
    }
}

#[derive(Default)]
struct FakeStore {
    knobs: Mutex<Knobs>,
    objects: Mutex<HashMap<String, Vec<u8>>>,
    /// Objects of zeros, generated as they are sent, so large ones cost no
    /// memory.
    sized: Mutex<HashMap<String, u64>>,
    seen: Mutex<Vec<Seen>>,
    connects: AtomicUsize,
}

impl FakeStore {
    fn set(&self, change: impl FnOnce(&mut Knobs)) {
        change(&mut self.knobs.lock().unwrap());
    }
    fn knobs(&self) -> Knobs {
        self.knobs.lock().unwrap().clone()
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn seen_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
    fn put_object(&self, path: &str, body: &[u8]) {
        self.objects
            .lock()
            .unwrap()
            .insert(path.to_string(), body.to_vec());
    }
    fn put_sized(&self, path: &str, length: u64) {
        self.sized.lock().unwrap().insert(path.to_string(), length);
    }

    async fn serve(self: Arc<Self>, mut io: DuplexStream) {
        let mut buf = Vec::new();
        loop {
            let (length, method, path, headers) = loop {
                let mut parsed = [httparse::EMPTY_HEADER; 64];
                let mut request = httparse::Request::new(&mut parsed);
                if let Ok(httparse::Status::Complete(length)) = request.parse(&buf) {
                    break (
                        length,
                        request.method.unwrap().to_string(),
                        request.path.unwrap().to_string(),
                        request
                            .headers
                            .iter()
                            .map(|h| {
                                (
                                    h.name.to_ascii_lowercase(),
                                    String::from_utf8(h.value.to_vec()).unwrap(),
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                let mut chunk = [0u8; 4096];
                match io.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            buf.drain(..length);
            let content_length: usize = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map(|(_, value)| value.parse().unwrap())
                .unwrap_or(0);
            let knobs = self.knobs();
            let mut body = Vec::new();
            while body.len() < content_length {
                if !buf.is_empty() {
                    let n = buf.len().min(content_length - body.len());
                    body.extend(buf.drain(..n));
                    continue;
                }
                let piece = knobs.slow_put.map_or(64 * 1024, |(piece, _)| piece);
                let mut chunk = vec![0u8; piece.min(content_length - body.len())];
                // A whole piece per pause, however the transport splits it.
                if io.read_exact(&mut chunk).await.is_err() {
                    return;
                }
                body.extend_from_slice(&chunk);
                if let Some((_, pause)) = knobs.slow_put {
                    tokio::time::sleep(pause).await;
                }
            }
            self.seen.lock().unwrap().push(Seen {
                method: method.clone(),
                path: path.clone(),
                headers,
                body: body.clone(),
            });
            if knobs.silent {
                std::future::pending::<()>().await;
            }
            if let Some(delay) = knobs.respond_after {
                tokio::time::sleep(delay).await;
            }
            {
                let mut knobs = self.knobs.lock().unwrap();
                if knobs.reset_next > 0 {
                    knobs.reset_next -= 1;
                    return;
                }
            }
            if let Some(status) = knobs.status {
                let error = format!("<Error><AccessKeyId>{LEAK}</AccessKeyId></Error>");
                let head = format!(
                    "HTTP/1.1 {status} Error\r\ncontent-type: application/xml\r\nx-amz-request-id: R1\r\ncontent-length: {}\r\n\r\n",
                    error.len()
                );
                let mut out = head.into_bytes();
                if method != "HEAD" {
                    out.extend_from_slice(error.as_bytes());
                }
                if io.write_all(&out).await.is_err() {
                    return;
                }
                continue;
            }
            if method == "PUT" {
                self.put_object(&path, &body);
                let head = "HTTP/1.1 200 OK\r\netag: \"put-etag\"\r\nx-amz-request-id: R2\r\nx-amz-id-2: internal\r\ncontent-length: 0\r\n\r\n";
                if io.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                continue;
            }
            let stored = self.objects.lock().unwrap().get(&path).cloned();
            let sized = self.sized.lock().unwrap().get(&path).copied();
            let total = match (&stored, sized) {
                (Some(bytes), _) => bytes.len() as u64,
                (None, Some(length)) => length,
                (None, None) => {
                    let error = format!(
                        "<Error><Code>NoSuchKey</Code><AccessKeyId>{LEAK}</AccessKeyId></Error>"
                    );
                    let mut out = format!(
                        "HTTP/1.1 404 Not Found\r\ncontent-type: application/xml\r\ncontent-length: {}\r\n\r\n",
                        error.len()
                    )
                    .into_bytes();
                    if method != "HEAD" {
                        out.extend_from_slice(error.as_bytes());
                    }
                    if io.write_all(&out).await.is_err() {
                        return;
                    }
                    continue;
                }
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {total}\r\netag: \"abc\"\r\nlast-modified: Wed, 21 Oct 2026 07:28:00 GMT\r\ncontent-type: application/octet-stream\r\nx-amz-request-id: R3\r\nx-amz-storage-class: STANDARD\r\nserver: fake\r\nset-cookie: s=1\r\n\r\n"
            );
            if io.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            if method == "HEAD" {
                continue;
            }
            let (piece, pause) = knobs.drip.unwrap_or((256 * 1024, Duration::ZERO));
            let zeros = vec![0u8; piece];
            let mut sent = 0u64;
            while sent < total {
                let n = (piece as u64).min(total - sent) as usize;
                let chunk = match &stored {
                    Some(bytes) => &bytes[sent as usize..sent as usize + n],
                    None => &zeros[..n],
                };
                if io.write_all(chunk).await.is_err() {
                    return;
                }
                sent += n as u64;
                if !pause.is_zero() && sent < total {
                    tokio::time::sleep(pause).await;
                }
            }
        }
    }
}

struct FakeConnector(Arc<FakeStore>);

impl Connector for FakeConnector {
    fn connect<'a>(&'a self, target: &'a Target) -> ConnectFuture<'a> {
        let store = self.0.clone();
        Box::pin(async move {
            assert_eq!(target.host, "store.test");
            assert!(!target.tls);
            store.connects.fetch_add(1, Ordering::SeqCst);
            let knobs = store.knobs();
            if knobs.refuse {
                return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
            }
            if knobs.blackhole {
                std::future::pending::<()>().await;
            }
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(store.serve(server));
            Ok(Box::new(client) as BoxedIo)
        })
    }
}

// ---------------------------------------------------------------------------
// Generations
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Switchable(Mutex<Option<Arc<ActiveGeneration>>>);

impl Switchable {
    fn set(&self, generation: Option<ActiveGeneration>) {
        *self.0.lock().unwrap() = generation.map(Arc::new);
    }
}

impl Generations for Switchable {
    fn admit(&self) -> Option<Arc<ActiveGeneration>> {
        self.0.lock().unwrap().clone()
    }
}

/// A build-server generation; its key ids name the generation so tests can
/// tell which one signed a request.
fn generation(name: &str, tenant: &str) -> ActiveGeneration {
    ActiveGeneration {
        generation: Generation(name.to_string()),
        tenant: tenant.to_string(),
        endpoint: "http://store.test".to_string(),
        region: "de".to_string(),
        bucket: BUCKET.to_string(),
        prefix: derived_prefix(tenant),
        storage_class: CLASS.to_string(),
        read_key: Credential {
            access_key_id: format!("READ-{name}"),
            secret_access_key: format!("read-secret-{name}"),
        },
        write_key: Some(Credential {
            access_key_id: format!("WRITE-{name}"),
            secret_access_key: format!("write-secret-{name}"),
        }),
    }
}

/// The store path of a bare key under the fleet prefix.
fn stored(tool: Tool, key: &str) -> String {
    format!("/{BUCKET}/{}{}/{key}", derived_prefix(TENANT), tool.id())
}

// ---------------------------------------------------------------------------
// The harness and a minimal tool-side client
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Harness {
    gateway: Gateway,
    store: Arc<FakeStore>,
    generations: Arc<Switchable>,
}

impl Harness {
    fn new() -> Self {
        Self::with_limits(Limits::default())
    }
    fn with_limits(limits: Limits) -> Self {
        let store = Arc::new(FakeStore::default());
        let generations = Arc::new(Switchable::default());
        generations.set(Some(generation("1", TENANT)));
        let gateway = Gateway::with_connector(
            limits,
            generations.clone(),
            Arc::new(FakeConnector(store.clone())),
        );
        Self {
            gateway,
            store,
            generations,
        }
    }
    fn reader(&self, job: &str) -> Arc<Binding> {
        self.gateway.read_binding(job, Tool::Sccache, Adapter::S3)
    }
    fn writer(&self) -> Arc<Binding> {
        self.gateway.write_binding(Tool::Sccache, Adapter::S3)
    }
    fn open(&self, binding: &Arc<Binding>) -> DuplexStream {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let gateway = self.gateway.clone();
        let binding = binding.clone();
        tokio::spawn(async move { gateway.serve(binding, server).await });
        client
    }
    /// Sends one raw request on a new connection and reads the reply.
    async fn raw(&self, binding: &Arc<Binding>, request: &[u8], head: bool) -> Reply {
        let mut conn = self.open(binding);
        conn.write_all(request).await.unwrap();
        read_reply(&mut conn, head).await.expect("a response head")
    }
    async fn get(&self, binding: &Arc<Binding>, path: &str) -> Reply {
        self.raw(
            binding,
            format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes(),
            false,
        )
        .await
    }
    async fn head(&self, binding: &Arc<Binding>, path: &str) -> Reply {
        self.raw(
            binding,
            format!("HEAD {path} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes(),
            true,
        )
        .await
    }
    async fn put(&self, binding: &Arc<Binding>, path: &str, body: &[u8]) -> Reply {
        let mut request = format!(
            "PUT {path} HTTP/1.1\r\nhost: x\r\nauthorization: dummy\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        self.raw(binding, &request, false).await
    }
}

fn s3(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The whole announced body arrived.
    complete: bool,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
    fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        names
    }
}

async fn read_head<R: AsyncRead + Unpin>(
    conn: &mut R,
) -> Option<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::new();
    loop {
        let mut parsed = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut parsed);
        if let Ok(httparse::Status::Complete(length)) = response.parse(&buf) {
            let headers = response
                .headers
                .iter()
                .map(|h| {
                    (
                        h.name.to_ascii_lowercase(),
                        String::from_utf8(h.value.to_vec()).unwrap(),
                    )
                })
                .collect();
            let status = response.code.unwrap();
            return Some((status, headers, buf[length..].to_vec()));
        }
        let mut chunk = [0u8; 4096];
        match conn.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Reads one response; `None` when the connection ends before a head.
async fn read_reply<R: AsyncRead + Unpin>(conn: &mut R, head_request: bool) -> Option<Reply> {
    let (status, headers, mut body) = read_head(conn).await?;
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map(|(_, v)| v.parse().unwrap())
        .unwrap_or(0);
    if head_request {
        return Some(Reply {
            status,
            headers,
            body: Vec::new(),
            complete: true,
        });
    }
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < length {
        match conn.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    Some(Reply {
        status,
        headers,
        complete: body.len() == length,
        body,
    })
}

/// Reads a GET reply's body counting bytes only, `piece` bytes at a time
/// with `pause` between reads; returns the status and the bytes received.
async fn read_counting(conn: &mut DuplexStream, piece: usize, pause: Duration) -> (u16, u64, u64) {
    let (status, headers, early) = read_head(conn).await.expect("a response head");
    let length: u64 = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map(|(_, v)| v.parse().unwrap())
        .unwrap_or(0);
    let mut received = early.len() as u64;
    let mut chunk = vec![0u8; piece];
    while received < length {
        match conn.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => received += n as u64,
        }
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
    }
    (status, length, received)
}

// ---------------------------------------------------------------------------
// Endpoints, adapters and refusals
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn read_endpoint_gets_and_heads_through_the_s3_adapter() {
    let h = Harness::new();
    h.store
        .put_object(&stored(Tool::Sccache, "ab/cd-1.x"), b"object bytes");
    let reader = h.reader("job-a");
    // Two requests on one kept-alive connection.
    let mut conn = h.open(&reader);
    conn.write_all(format!("GET {} HTTP/1.1\r\nhost: x\r\nauthorization: AWS4-HMAC-SHA256 Credential=dummy\r\n\r\n", s3("ab/cd-1.x")).as_bytes())
        .await
        .unwrap();
    let got = read_reply(&mut conn, false).await.unwrap();
    conn.write_all(format!("HEAD {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("ab/cd-1.x")).as_bytes())
        .await
        .unwrap();
    let headed = read_reply(&mut conn, true).await.unwrap();

    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"object bytes");
    assert!(got.complete);
    // Only the cache metadata headers are forwarded.
    assert_eq!(
        got.names(),
        ["content-length", "content-type", "etag", "last-modified"]
    );
    assert_eq!(headed.status, 200);
    assert_eq!(headed.header("content-length"), Some("12"));

    let seen = h.store.seen();
    assert_eq!(seen.len(), 2);
    for request in &seen {
        assert_eq!(request.path, stored(Tool::Sccache, "ab/cd-1.x"));
        // Signed with the read-only key, never with the tool's dummy one.
        assert_eq!(request.access_key_id(), "READ-1");
        assert_eq!(
            request.header("x-amz-content-sha256"),
            Some("UNSIGNED-PAYLOAD")
        );
        assert_eq!(request.header("host"), Some("store.test"));
        assert_eq!(request.header("x-amz-storage-class"), None);
    }
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[1].method, "HEAD");
    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!((stats.hits, stats.misses, stats.errors), (1, 0, 0));
    assert!(stats.p95_ms.is_some());
}

#[tokio::test(start_paused = true)]
async fn write_endpoint_puts_with_the_storage_class_and_the_write_key() {
    let h = Harness::new();
    let writer = h.writer();
    let body = vec![7u8; 3 * MIB + 5];
    let reply = h.put(&writer, &s3("ab/put-1"), &body).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.names(), ["content-length", "etag"]);

    let seen = h.store.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(seen[0].path, stored(Tool::Sccache, "ab/put-1"));
    assert_eq!(seen[0].header("x-amz-storage-class"), Some(CLASS));
    assert_eq!(seen[0].access_key_id(), "WRITE-1");
    assert!(seen[0]
        .header("authorization")
        .unwrap()
        .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-storage-class,"));
    assert_eq!(seen[0].body, body);

    // A GET through the write endpoint also signs with the write key.
    let got = h.get(&writer, &s3("ab/put-1")).await;
    assert_eq!((got.status, got.body.len()), (200, body.len()));
    assert_eq!(h.store.seen()[1].access_key_id(), "WRITE-1");

    assert_eq!(writer.ledger().unwrap().tool(), Tool::Sccache);
    let ledger = writer.ledger().unwrap().summary();
    assert_eq!(
        ledger,
        LedgerSummary {
            in_flight: 0,
            puts_stored: 1,
            puts_failed: 0,
            identities: vec![generation("1", TENANT).identity()],
            key_sample: vec!["ab/put-1".to_string()],
        }
    );
}

#[tokio::test(start_paused = true)]
async fn http_adapter_maps_bare_keys_under_the_ccache_prefix() {
    let h = Harness::new();
    let writer = h.gateway.write_binding(Tool::Ccache, Adapter::Http);
    let reader = h.gateway.read_binding("job-a", Tool::Ccache, Adapter::Http);
    assert_eq!(
        h.put(&writer, "/0f/abcdef", b"ccache entry").await.status,
        200
    );
    let got = h.get(&reader, "/0f/abcdef").await;
    assert_eq!(
        (got.status, got.body.as_slice()),
        (200, &b"ccache entry"[..])
    );
    assert_eq!(h.head(&reader, "/0f/abcdef").await.status, 200);
    assert_eq!(h.get(&reader, "/0f/missing").await.status, 404);

    let seen = h.store.seen();
    assert_eq!(seen[0].path, stored(Tool::Ccache, "0f/abcdef"));
    assert_eq!(seen[0].header("x-amz-storage-class"), Some(CLASS));
    assert_eq!(seen[1].path, stored(Tool::Ccache, "0f/abcdef"));
    assert_eq!(seen[1].access_key_id(), "READ-1");
    // The bucket is the gateway's business: an HTTP path naming it is just a
    // key under the tool's prefix.
    assert_eq!(h.get(&reader, &s3("0f/abcdef")).await.status, 404);
    assert_eq!(
        h.store.seen().last().unwrap().path,
        stored(Tool::Ccache, &format!("{BUCKET}/0f/abcdef"))
    );
    let stats = h.gateway.stats(Tool::Ccache);
    assert_eq!((stats.hits, stats.misses), (1, 2));
    assert_eq!(h.gateway.stats(Tool::Sccache), ToolStats::default());
}

#[tokio::test(start_paused = true)]
async fn both_endpoints_refuse_other_methods_queries_encodings_and_bad_paths() {
    let h = Harness::new();
    h.store.put_object(&stored(Tool::Sccache, "ok"), b"x");
    let reader = h.reader("job-a");
    let writer = h.writer();
    let http = h.gateway.read_binding("job-a", Tool::Ccache, Adapter::Http);
    let request = |line: &str, extra: &str| format!("{line} HTTP/1.1\r\nhost: x\r\n{extra}\r\n");
    let common = [
        request(&format!("DELETE {}", s3("ok")), ""),
        request(&format!("POST {}", s3("ok")), "content-length: 0\r\n"),
        request(&format!("OPTIONS {}", s3("ok")), ""),
        request(&format!("GET /{BUCKET}?list-type=2"), ""),
        request(&format!("GET {}?x-id=GetObject", s3("ok")), ""),
        request(&format!("GET {}?", s3("ok")), ""),
        request(
            &format!("GET {}", s3("ok")),
            "transfer-encoding: chunked\r\n",
        ),
        request(&format!("GET {}", s3("ok")), "content-length: 3\r\n"),
        request(&format!("GET {}", s3("a%2Fb")), ""),
        request(&format!("GET {}", s3("a/../ok")), ""),
        request(&format!("GET {}", s3("..")), ""),
        request(&format!("GET {}", s3("a*b")), ""),
        request(&format!("GET {}", s3("a:b")), ""),
        request(&format!("GET {}", s3("/ok")), ""),
        request(&format!("GET {}", s3("")), ""),
        request(&format!("GET /{BUCKET}"), ""),
        request("GET /other-bucket/ok", ""),
        request(&format!("GET {}", s3("ok")), "range: items=0-1\r\n"),
        "NOT HTTP AT ALL\r\n\r\n".to_string(),
        // A request head over the 16 KiB limit.
        request(
            &format!("GET {}", s3("ok")),
            &format!("x-filler: {}\r\n", "f".repeat(17 * 1024)),
        ),
    ];
    for raw in &common {
        assert_eq!(
            h.raw(&reader, raw.as_bytes(), false).await.status,
            403,
            "{raw}"
        );
    }
    let put = |path: &str, extra: &str| {
        format!("PUT {path} HTTP/1.1\r\nhost: x\r\ncontent-length: 1\r\n{extra}\r\nz")
    };
    let read_only = [put(&s3("ok"), "")];
    for raw in &read_only {
        assert_eq!(
            h.raw(&reader, raw.as_bytes(), false).await.status,
            403,
            "{raw}"
        );
    }
    let write = [
        request(&format!("DELETE {}", s3("ok")), ""),
        request(&format!("POST {}?uploads", s3("ok")), ""),
        put(&format!("{}?partNumber=1", s3("ok")), ""),
        format!(
            "PUT {} HTTP/1.1\r\nhost: x\r\ntransfer-encoding: chunked\r\n\r\n1\r\nz\r\n0\r\n\r\n",
            s3("ok")
        ),
        format!(
            "PUT {} HTTP/1.1\r\nhost: x\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\nzz",
            s3("ok")
        ),
        put(&s3("a/../b"), ""),
        put(&s3("%41"), ""),
    ];
    for raw in &write {
        assert_eq!(
            h.raw(&writer, raw.as_bytes(), false).await.status,
            403,
            "{raw}"
        );
    }
    assert_eq!(h.get(&http, "//leading-slash").await.status, 403);
    assert_eq!(
        h.get(&http, "/a/./ok").await.status,
        404,
        "a '.' segment is a key"
    );

    let sccache = h.gateway.stats(Tool::Sccache);
    assert_eq!(
        sccache.read_refusals,
        (common.len() + read_only.len()) as u64
    );
    assert_eq!(sccache.write_refusals, write.len() as u64);
    assert_eq!(h.gateway.stats(Tool::Ccache).read_refusals, 1);
    // Nothing refused reached the store; only the '.'-segment GET did.
    assert_eq!(h.store.seen_count(), 1);
    // Refused PUTs are failed PUTs of the fill.
    let ledger = writer.ledger().unwrap().summary();
    assert_eq!((ledger.puts_stored, ledger.puts_failed), (0, 5));
}

#[tokio::test(start_paused = true)]
async fn the_full_upstream_key_is_bounded_by_max_key_bytes() {
    let h = Harness::with_limits(Limits {
        max_key_bytes: 100,
        ..Limits::default()
    });
    let reader = h.reader("job-a");
    // The prefix and tool directory count towards the limit.
    let room = 100 - format!("{}sccache/", derived_prefix(TENANT)).len();
    let fits = "k".repeat(room);
    let over = "k".repeat(room + 1);
    assert_eq!(h.get(&reader, &s3(&fits)).await.status, 404);
    assert_eq!(h.get(&reader, &s3(&over)).await.status, 403);
    assert_eq!(h.store.seen_count(), 1);
    assert_eq!(h.gateway.stats(Tool::Sccache).read_refusals, 1);
}

#[tokio::test(start_paused = true)]
async fn object_size_limits_refuse_large_puts_and_abort_large_responses() {
    let h = Harness::with_limits(Limits {
        max_object_bytes: 1000,
        ..Limits::default()
    });
    let reader = h.reader("job-a");
    let writer = h.writer();
    assert_eq!(h.put(&writer, &s3("fits"), &[1; 1000]).await.status, 200);
    assert_eq!(h.put(&writer, &s3("over"), &[1; 1001]).await.status, 503);
    assert_eq!(
        h.store.seen_count(),
        1,
        "an over-limit PUT never reaches the store"
    );
    h.store.put_sized(&stored(Tool::Sccache, "big"), 1001);
    let got = h.get(&reader, &s3("fits")).await;
    assert_eq!((got.status, got.body.len()), (200, 1000));
    let big = h.get(&reader, &s3("big")).await;
    assert_eq!((big.status, big.body.len()), (404, 0));
    assert_eq!(h.head(&reader, &s3("big")).await.status, 404);

    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!(stats.write_refusals, 1);
    assert_eq!(stats.oversize, 2);
    assert_eq!((stats.hits, stats.errors), (1, 0));
    assert_eq!(h.gateway.read_breaker_open_until(), None);
    let ledger = writer.ledger().unwrap().summary();
    assert_eq!((ledger.puts_stored, ledger.puts_failed), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn failure_answers_carry_no_upstream_error_body() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    let writer = h.writer();
    let missing = h.get(&reader, &s3("missing")).await;
    assert_eq!((missing.status, missing.body.len()), (404, 0));
    assert_eq!(missing.names(), ["content-length"]);
    for status in [400, 403, 500, 503] {
        h.store.set(|k| k.status = Some(status));
        let read = h.get(&reader, &s3("k")).await;
        assert_eq!((read.status, read.body.len()), (404, 0), "{status}");
        assert_eq!(read.names(), ["content-length"]);
        let put = h.put(&writer, &s3("k"), b"v").await;
        assert_eq!((put.status, put.body.len()), (503, 0), "{status}");
        // The write endpoint answers a GET the store does not have with a
        // miss, but any other refusal with an error.
        assert_eq!(h.get(&writer, &s3("k")).await.status, 503, "{status}");
    }
    let stats = h.gateway.stats(Tool::Sccache);
    // Only the 5xx answers count against a breaker: two statuses, one
    // read-endpoint request each, two write-endpoint requests each.
    assert_eq!(stats.errors, 6);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 0);
}

#[tokio::test(start_paused = true)]
async fn sccache_check_reads_count_as_probes() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    assert_eq!(h.get(&reader, &s3(".sccache_check")).await.status, 404);
    h.store
        .put_object(&stored(Tool::Sccache, ".sccache_check"), b"ok");
    assert_eq!(h.get(&reader, &s3(".sccache_check")).await.status, 200);
    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!((stats.probes, stats.hits, stats.misses), (2, 0, 0));
    // The same key under ccache is an ordinary object.
    let ccache = h.gateway.read_binding("job-a", Tool::Ccache, Adapter::Http);
    assert_eq!(h.get(&ccache, "/.sccache_check").await.status, 404);
    assert_eq!(h.gateway.stats(Tool::Ccache).misses, 1);
}

#[tokio::test(start_paused = true)]
async fn an_unavailable_cache_answers_without_contacting_the_store() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    let writer = h.writer();
    h.generations.set(None);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.put(&writer, &s3("k"), b"v").await.status, 503);
    // A host without the read-write key (a tenant VM or Boat) cannot write.
    let mut read_only = generation("1", TENANT);
    read_only.write_key = None;
    h.generations.set(Some(read_only));
    assert_eq!(h.put(&writer, &s3("k"), b"v").await.status, 503);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.store.connects(), 1, "only the last GET reached the store");
    let ledger = writer.ledger().unwrap().summary();
    assert_eq!((ledger.puts_stored, ledger.puts_failed), (0, 2));
    assert!(ledger.identities.is_empty());
}

#[tokio::test(start_paused = true)]
async fn expect_continue_is_answered_before_the_body_is_read() {
    let h = Harness::new();
    let writer = h.writer();
    let mut conn = h.open(&writer);
    conn.write_all(
        format!(
            "PUT {} HTTP/1.1\r\nhost: x\r\ncontent-length: 4\r\nexpect: 100-continue\r\n\r\n",
            s3("continued")
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let interim = read_reply(&mut conn, true).await.unwrap();
    assert_eq!(interim.status, 100);
    conn.write_all(b"body").await.unwrap();
    let reply = read_reply(&mut conn, false).await.unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(h.store.seen()[0].body, b"body");
}

// ---------------------------------------------------------------------------
// First byte, retry and deadline
// ---------------------------------------------------------------------------

fn around(elapsed: Duration, expected: Duration) -> bool {
    elapsed >= expected && elapsed < expected + Duration::from_millis(100)
}

#[tokio::test(start_paused = true)]
async fn a_blackholed_store_gives_a_miss_after_one_attempt_within_5_s() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store.set(|k| k.blackhole = true);
    let started = Instant::now();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert!(
        around(started.elapsed(), Duration::from_secs(5)),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(h.store.connects(), 1, "a first-byte timeout is not retried");
    // A store that accepts the connection and never answers is the same.
    h.store.set(|k| {
        k.blackhole = false;
        k.silent = true;
    });
    let started = Instant::now();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert!(around(started.elapsed(), Duration::from_secs(5)));
    assert_eq!(h.store.connects(), 2);
    // On the write endpoint the same failure is a 503.
    let started = Instant::now();
    assert_eq!(h.put(&h.writer(), &s3("k"), b"v").await.status, 503);
    assert!(around(started.elapsed(), Duration::from_secs(5)));
    assert_eq!(h.store.connects(), 3);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 3);
}

#[tokio::test(start_paused = true)]
async fn a_refused_store_is_retried_once_then_missed() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store.set(|k| k.refuse = true);
    let started = Instant::now();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(h.store.connects(), 2);
    assert_eq!(h.put(&h.writer(), &s3("k"), b"v").await.status, 503);
    assert_eq!(h.store.connects(), 4);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 2);
}

#[tokio::test(start_paused = true)]
async fn a_reset_before_any_response_byte_is_retried_once() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    let writer = h.writer();
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store.set(|k| k.reset_next = 1);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(h.store.seen_count(), 2);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 0);
    h.store.set(|k| k.reset_next = 2);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), 4, "one retry only");
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 1);
    // A PUT reset after its body went out cannot be replayed.
    h.store.set(|k| k.reset_next = 1);
    assert_eq!(h.put(&writer, &s3("p"), b"body").await.status, 503);
    assert_eq!(h.store.seen_count(), 5);
    assert_eq!(writer.ledger().unwrap().summary().puts_failed, 1);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_a_slow_drip_by_resetting_the_connection() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store
        .put_sized(&stored(Tool::Sccache, "drip"), 4 * MIB as u64);
    h.store
        .set(|k| k.drip = Some((1024, Duration::from_secs(1))));
    let started = Instant::now();
    let reply = h.get(&reader, &s3("drip")).await;
    // Headers were already out, so the failure is a short body.
    assert_eq!(reply.status, 200);
    assert!(!reply.complete);
    assert!(
        around(started.elapsed(), Duration::from_secs(120)),
        "{:?}",
        started.elapsed()
    );
    // Counted once, as slow after 10 s, not again at the deadline.
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 1);
}

#[tokio::test(start_paused = true)]
async fn the_retry_counts_against_the_deadline() {
    let h = Harness::with_limits(Limits {
        deadline: Duration::from_secs(4),
        ..Limits::default()
    });
    let reader = h.reader("job-a");
    // The first attempt is reset after 3 s; the retry gets only the second
    // that is left, not a fresh first-byte limit.
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store.set(|k| {
        k.reset_next = 1;
        k.respond_after = Some(Duration::from_secs(3));
    });
    let started = Instant::now();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert!(
        around(started.elapsed(), Duration::from_secs(4)),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(h.store.seen_count(), 2);
}

// ---------------------------------------------------------------------------
// The breaker
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn below_floor_reads_trip_the_read_breaker_but_healthy_heads_and_small_gets_do_not() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store
        .put_object(&stored(Tool::Sccache, "small"), &[3; 4096]);
    h.store
        .put_sized(&stored(Tool::Sccache, "large"), 64 * MIB as u64);
    // Healthy traffic, slow to start but within the first-byte limit.
    h.store
        .set(|k| k.respond_after = Some(Duration::from_secs(4)));
    for _ in 0..20 {
        assert_eq!(h.head(&reader, &s3("small")).await.status, 200);
        assert_eq!(h.get(&reader, &s3("small")).await.status, 200);
    }
    // A small body below the floor that completes within the window.
    h.store.set(|k| {
        k.respond_after = None;
        k.drip = Some((512, Duration::from_secs(1)));
    });
    assert!(h.get(&reader, &s3("small")).await.complete);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 0);
    assert_eq!(h.gateway.read_breaker_open_until(), None);

    // Five sustained streams at 64 KiB/s.
    h.store
        .set(|k| k.drip = Some((64 * 1024, Duration::from_secs(1))));
    let slow: Vec<_> = (0..5)
        .map(|_| {
            let (h, reader) = (h.clone(), reader.clone());
            tokio::spawn(async move { h.get(&reader, &s3("large")).await })
        })
        .collect();
    tokio::time::sleep(SLOW_WINDOW - Duration::from_secs(1)).await;
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 0, "not before 10 s");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 5);
    assert!(h.gateway.read_breaker_open_until().is_some());
    let before = h.store.seen_count();
    let started = Instant::now();
    assert_eq!(h.get(&reader, &s3("small")).await.status, 404);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(h.store.seen_count(), before);
    assert_eq!(h.gateway.stats(Tool::Sccache).breaker_open, 1);
    for task in slow {
        let reply = task.await.unwrap();
        assert_eq!(reply.status, 200);
        assert!(!reply.complete);
    }
    assert_eq!(
        h.gateway.stats(Tool::Sccache).errors,
        5,
        "each counted once"
    );
}

#[tokio::test(start_paused = true)]
async fn client_backpressure_never_counts_against_the_breaker() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store
        .put_sized(&stored(Tool::Sccache, "large"), 64 * MIB as u64);
    let mut conn = h.open(&reader);
    conn.write_all(format!("GET {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("large")).as_bytes())
        .await
        .unwrap();
    // The tool reads 16 KiB a second, so the deadline ends the transfer.
    let (status, length, received) =
        read_counting(&mut conn, 16 * 1024, Duration::from_secs(1)).await;
    assert_eq!(status, 200);
    assert!(received < length);
    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!((stats.errors, stats.hits), (0, 1));
    assert_eq!(h.gateway.read_breaker_open_until(), None);
}

#[tokio::test(start_paused = true)]
async fn five_failures_open_the_read_breaker_and_a_failed_probe_reopens_it() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store.set(|k| k.status = Some(500));
    for _ in 0..FAILURES_TO_OPEN - 1 {
        assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    }
    assert_eq!(h.gateway.read_breaker_open_until(), None);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(
        h.gateway.read_breaker_open_until(),
        Some(Instant::now() + OPEN_WINDOW)
    );
    // Open: answered at once, without the store.
    let seen = h.store.seen_count();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), seen);
    assert_eq!(h.gateway.stats(Tool::Sccache).breaker_open, 1);
    // The write endpoint has its own breaker.
    h.store.set(|k| k.status = None);
    assert_eq!(h.put(&h.writer(), &s3("w"), b"v").await.status, 200);
    h.store.set(|k| k.status = Some(500));
    let seen = h.store.seen_count();

    tokio::time::sleep(OPEN_WINDOW - Duration::from_secs(1)).await;
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), seen);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Past the window: the next request probes, fails, and the breaker
    // reopens for a whole window at once.
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), seen + 1);
    assert_eq!(
        h.gateway.read_breaker_open_until(),
        Some(Instant::now() + OPEN_WINDOW)
    );
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(h.store.seen_count(), seen + 1);
    assert_eq!(h.gateway.stats(Tool::Sccache).breaker_open, 3);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 6);

    // A successful probe closes it.
    tokio::time::sleep(OPEN_WINDOW).await;
    h.store.set(|k| k.status = None);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(h.gateway.read_breaker_open_until(), None);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(h.store.seen_count(), seen + 3);
}

/// Opens the write breaker with five failed PUTs.
async fn open_write_breaker(h: &Harness, writer: &Arc<Binding>) {
    h.store.set(|k| {
        k.status = Some(500);
        k.respond_after = None;
    });
    for i in 0..FAILURES_TO_OPEN {
        assert_eq!(
            h.put(writer, &s3(&format!("fail-{i}")), b"v").await.status,
            503
        );
    }
    assert!(h.gateway.write_breaker_open_until().is_some());
}

#[tokio::test(start_paused = true)]
async fn write_requests_wait_for_the_probe_and_follow_its_result() {
    let h = Harness::new();
    let writer = h.writer();
    open_write_breaker(&h, &writer).await;
    let seen = h.store.seen_count();
    assert_eq!(h.put(&writer, &s3("open"), b"v").await.status, 503);
    assert_eq!(
        h.store.seen_count(),
        seen,
        "refused at once inside the window"
    );
    tokio::time::sleep(OPEN_WINDOW).await;
    assert_eq!(
        h.gateway.write_breaker_open_until(),
        None,
        "a breaker past its window does not report open"
    );

    // A probe that succeeds: PUTs arriving meanwhile wait, then go through.
    h.store.set(|k| {
        k.status = None;
        k.respond_after = Some(Duration::from_secs(3));
    });
    let spawn_put = |key: String| {
        let (h, writer) = (h.clone(), writer.clone());
        tokio::spawn(async move { h.put(&writer, &s3(&key), b"v").await.status })
    };
    let probe = spawn_put("probe".into());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let held: Vec<_> = (0..3).map(|i| spawn_put(format!("held-{i}"))).collect();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        h.store.seen_count(),
        seen + 1,
        "only the probe is at the store"
    );
    assert_eq!(probe.await.unwrap(), 200);
    for put in held {
        assert_eq!(put.await.unwrap(), 200);
    }
    assert_eq!(h.store.seen_count(), seen + 4);
    let breaker_open = h.gateway.stats(Tool::Sccache).breaker_open;

    // A probe that fails: the held PUTs get 503 without reaching the store
    // and the breaker reopens for a whole window.
    open_write_breaker(&h, &writer).await;
    tokio::time::sleep(OPEN_WINDOW).await;
    h.store
        .set(|k| k.respond_after = Some(Duration::from_secs(3)));
    let seen = h.store.seen_count();
    let probe = spawn_put("probe-2".into());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let held: Vec<_> = (0..3).map(|i| spawn_put(format!("held-2-{i}"))).collect();
    assert_eq!(probe.await.unwrap(), 503);
    for put in held {
        assert_eq!(put.await.unwrap(), 503);
    }
    assert_eq!(h.store.seen_count(), seen + 1);
    assert_eq!(
        h.gateway.write_breaker_open_until(),
        Some(Instant::now() + OPEN_WINDOW)
    );
    assert_eq!(
        h.gateway.stats(Tool::Sccache).breaker_open,
        breaker_open + 3
    );
}

#[tokio::test(start_paused = true)]
async fn read_requests_during_a_probe_get_an_immediate_miss() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store.set(|k| k.status = Some(500));
    for _ in 0..FAILURES_TO_OPEN {
        h.get(&reader, &s3("k")).await;
    }
    tokio::time::sleep(OPEN_WINDOW).await;
    h.store.set(|k| {
        k.status = None;
        k.respond_after = Some(Duration::from_secs(3));
    });
    let probe = {
        let (h, reader) = (h.clone(), reader.clone());
        tokio::spawn(async move { h.get(&reader, &s3("k")).await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let started = Instant::now();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 404);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(h.gateway.stats(Tool::Sccache).breaker_open, 1);
    assert_eq!(probe.await.unwrap(), 200);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
}

#[tokio::test(start_paused = true)]
async fn slow_but_successful_puts_and_4xx_keys_do_not_trip_the_write_breaker() {
    let h = Harness::new();
    let writer = h.writer();
    // 8 MiB read by the store at 256 KiB/s: 32 s per PUT.
    h.store
        .set(|k| k.slow_put = Some((256 * 1024, Duration::from_secs(1))));
    let body = vec![1u8; 8 * MIB];
    for i in 0..FAILURES_TO_OPEN + 1 {
        let started = Instant::now();
        assert_eq!(
            h.put(&writer, &s3(&format!("slow-{i}")), &body)
                .await
                .status,
            200
        );
        assert!(started.elapsed() >= Duration::from_secs(30));
    }
    h.store.set(|k| {
        k.slow_put = None;
        k.status = Some(403);
    });
    for i in 0..2 * FAILURES_TO_OPEN {
        assert_eq!(
            h.put(&writer, &s3(&format!("denied-{i}")), b"v")
                .await
                .status,
            503
        );
    }
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 0);
    assert_eq!(h.gateway.write_breaker_open_until(), None);
    h.store.set(|k| k.status = None);
    assert_eq!(h.put(&writer, &s3("after"), b"v").await.status, 200);
}

// ---------------------------------------------------------------------------
// Admission caps and memory
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn at_default_caps_a_jobs_excess_reads_are_refused_and_other_jobs_are_unaffected() {
    let h = Harness::new();
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store
        .set(|k| k.respond_after = Some(Duration::from_secs(4)));
    let job_a = h.reader("job-a");
    let held: Vec<_> = (0..Limits::default().max_job_requests)
        .map(|_| {
            let (h, reader) = (h.clone(), job_a.clone());
            tokio::spawn(async move { h.get(&reader, &s3("k")).await.status })
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Another socket of the same job shares its cap.
    let started = Instant::now();
    assert_eq!(h.get(&h.reader("job-a"), &s3("k")).await.status, 404);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(h.gateway.stats(Tool::Sccache).read_refusals, 1);
    // Job B is admitted meanwhile.
    assert_eq!(h.get(&h.reader("job-b"), &s3("k")).await.status, 200);
    for task in held {
        assert_eq!(task.await.unwrap(), 200);
    }
    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!(
        (stats.hits, stats.read_refusals, stats.breaker_open),
        (17, 1, 0)
    );
    // The write endpoint is not under the read caps.
    assert_eq!(h.put(&h.writer(), &s3("w"), b"v").await.status, 200);
}

#[tokio::test(start_paused = true)]
async fn the_gateway_wide_cap_bounds_reads_across_jobs() {
    let h = Harness::with_limits(Limits {
        max_total_requests: 4,
        ..Limits::default()
    });
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store
        .set(|k| k.respond_after = Some(Duration::from_secs(4)));
    let held: Vec<_> = (0..4)
        .map(|i| {
            let (h, reader) = (h.clone(), h.reader(&format!("job-{i}")));
            tokio::spawn(async move { h.get(&reader, &s3("k")).await.status })
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.get(&h.reader("job-x"), &s3("k")).await.status, 404);
    for task in held {
        assert_eq!(task.await.unwrap(), 200);
    }
    assert_eq!(h.get(&h.reader("job-x"), &s3("k")).await.status, 200);
    assert_eq!(h.gateway.stats(Tool::Sccache).read_refusals, 1);
}

/// Job A holds 64 slow-reading GETs of the largest fixture object (1 GiB)
/// while job B reads small objects. Returns job B's statuses.
async fn slow_readers(h: &Harness, during: impl std::future::Future<Output = ()>) -> Vec<u16> {
    h.store
        .put_sized(&stored(Tool::Sccache, "largest"), 1 << 30);
    h.store
        .put_object(&stored(Tool::Sccache, "small"), &[5; 1024]);
    let job_a = h.reader("job-a");
    let readers: Vec<_> = (0..64)
        .map(|_| {
            let mut conn = h.open(&job_a);
            tokio::spawn(async move {
                conn.write_all(
                    format!("GET {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("largest")).as_bytes(),
                )
                .await
                .unwrap();
                read_counting(&mut conn, 64 * 1024, Duration::from_secs(1)).await
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(15)).await;
    let job_b = h.reader("job-b");
    let mut statuses = Vec::new();
    for _ in 0..10 {
        statuses.push(h.get(&job_b, &s3("small")).await.status);
    }
    during.await;
    for reader in readers {
        let (status, length, received) = reader.await.unwrap();
        assert_eq!(status, 200);
        assert!(
            received < length,
            "a slow reader never holds a whole object"
        );
    }
    statuses
}

#[tokio::test(start_paused = true)]
async fn with_the_job_cap_at_64_slow_readers_leave_other_jobs_unaffected() {
    let h = Harness::with_limits(Limits {
        max_job_requests: 64,
        ..Limits::default()
    });
    let statuses = slow_readers(&h, async {}).await;
    assert_eq!(statuses, vec![200; 10]);
    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!((stats.hits, stats.breaker_open, stats.errors), (74, 0, 0));
    assert_eq!(stats.read_refusals, 0);
}

/// Resident set size of this process, from /proc.
#[cfg(target_os = "linux")]
fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64
}

const RSS_CHILD: &str = "MOTOR_GATEWAY_RSS_CHILD";

/// Runs the 64 slow readers in a child process of its own, so other tests
/// running in parallel cannot disturb the memory measurement.
#[cfg(target_os = "linux")]
#[test]
fn slow_readers_grow_resident_memory_less_than_the_buffer_bound() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote_cache::gateway::tests::slow_readers_memory_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(RSS_CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("resident growth"), "{stdout}");
}

/// The measuring half of the test above; a no-op unless started by it.
#[cfg(target_os = "linux")]
#[test]
fn slow_readers_memory_child() {
    if std::env::var_os(RSS_CHILD).is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let limits = Limits {
            max_job_requests: 64,
            ..Limits::default()
        };
        let bound = (limits.max_total_requests * limits.stream_buffer_bytes) as u64;
        let h = Harness::with_limits(limits);
        let baseline = resident_bytes();
        let peak = Arc::new(std::sync::atomic::AtomicU64::new(baseline));
        let sampler = {
            let peak = peak.clone();
            tokio::spawn(async move {
                loop {
                    peak.fetch_max(resident_bytes(), Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
        };
        let statuses = slow_readers(&h, async {
            peak.fetch_max(resident_bytes(), Ordering::Relaxed);
        })
        .await;
        sampler.abort();
        assert_eq!(statuses, vec![200; 10]);
        let growth = peak.load(Ordering::Relaxed) - baseline;
        println!("resident growth {growth} bytes, bound {bound} bytes");
        assert!(growth < bound, "resident growth {growth} >= {bound}");
    });
}

#[tokio::test(start_paused = true)]
async fn a_one_gib_object_at_healthy_throughput_over_30_s_is_not_a_failure() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store.put_sized(&stored(Tool::Sccache, "gib"), 1 << 30);
    // 4 MiB every 118 ms is about 34 MiB/s, so the transfer spans 30 s.
    h.store
        .set(|k| k.drip = Some((4 * MIB, Duration::from_millis(118))));
    let started = Instant::now();
    let mut conn = h.open(&reader);
    conn.write_all(format!("GET {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("gib")).as_bytes())
        .await
        .unwrap();
    let (status, length, received) = read_counting(&mut conn, MIB, Duration::ZERO).await;
    assert_eq!((status, length, received), (200, 1 << 30, 1 << 30));
    assert!(
        started.elapsed() >= Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    let stats = h.gateway.stats(Tool::Sccache);
    assert_eq!((stats.hits, stats.errors), (1, 0));
    assert_eq!(h.gateway.read_breaker_open_until(), None);
}

// ---------------------------------------------------------------------------
// Generations and the write ledger
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn requests_sign_with_the_generation_they_were_admitted_under() {
    let h = Harness::new();
    let reader = h.reader("job-a");
    h.store.put_object(&stored(Tool::Sccache, "k"), b"v");
    h.store
        .set(|k| k.respond_after = Some(Duration::from_secs(2)));
    let early = {
        let (h, reader) = (h.clone(), reader.clone());
        tokio::spawn(async move { h.get(&reader, &s3("k")).await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    // A key rotation: same store identity, new keys.
    h.generations.set(Some(generation("2", TENANT)));
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(early.await.unwrap(), 200);
    let seen = h.store.seen();
    assert_eq!(seen[0].access_key_id(), "READ-1");
    assert_eq!(seen[1].access_key_id(), "READ-2");
}

#[tokio::test(start_paused = true)]
async fn each_put_is_tagged_with_the_store_identity_that_signed_it() {
    let h = Harness::new();
    let writer = h.writer();
    assert_eq!(h.put(&writer, &s3("a"), b"v").await.status, 200);
    // A rotation keeps the identity.
    h.generations.set(Some(generation("2", TENANT)));
    assert_eq!(h.put(&writer, &s3("b"), b"v").await.status, 200);
    // A different cache tenant is a different prefix, so another identity.
    let other = "acct:01KXJJHK4V6DB42W1YAN2XD9QM";
    h.generations.set(Some(generation("3", other)));
    assert_eq!(h.put(&writer, &s3("c"), b"v").await.status, 200);
    let seen = h.store.seen();
    assert_eq!(seen[1].access_key_id(), "WRITE-2");
    assert_eq!(
        seen[2].path,
        format!("/{BUCKET}/{}sccache/c", derived_prefix(other))
    );
    assert_eq!(seen[2].access_key_id(), "WRITE-3");
    let ledger = writer.ledger().unwrap().summary();
    assert_eq!(
        ledger.identities,
        vec![
            generation("1", TENANT).identity(),
            generation("3", other).identity()
        ]
    );
    assert_eq!(ledger.key_sample, ["a", "b", "c"]);
    assert_eq!(ledger.puts_stored, 3);
}

#[tokio::test(start_paused = true)]
async fn the_write_ledger_tracks_in_flight_puts_and_failed_ones() {
    let h = Harness::new();
    let writer = h.writer();
    let ledger = writer.ledger().unwrap().clone();
    h.store
        .set(|k| k.respond_after = Some(Duration::from_secs(3)));
    let put = {
        let (h, writer) = (h.clone(), writer.clone());
        tokio::spawn(async move { h.put(&writer, &s3("slow"), b"v").await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(ledger.summary().in_flight, 1);
    assert_eq!(
        ledger.summary().puts_stored,
        0,
        "not stored before the store answers"
    );
    ledger.idle().await;
    let summary = ledger.summary();
    assert_eq!((summary.in_flight, summary.puts_stored), (0, 1));
    assert_eq!(put.await.unwrap(), 200);

    // A fill that goes away mid-body: the PUT failed, and the breaker learns
    // nothing from it.
    h.store.set(|k| k.respond_after = None);
    let mut conn = h.open(&writer);
    conn.write_all(
        format!(
            "PUT {} HTTP/1.1\r\nhost: x\r\ncontent-length: 1000\r\n\r\npartial",
            s3("cut")
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(conn);
    tokio::time::sleep(Duration::from_secs(1)).await;
    ledger.idle().await;
    let summary = ledger.summary();
    assert_eq!((summary.puts_stored, summary.puts_failed), (1, 1));
    assert_eq!(summary.key_sample, ["slow"]);
    assert_eq!(h.gateway.stats(Tool::Sccache).errors, 0);
}

#[tokio::test]
async fn a_unix_socket_listener_serves_its_binding_until_dropped() {
    let scratch = cortexkit_test_support::ScratchDir::new("gateway");
    let path = scratch.join("read.sock");
    let h = Harness::new();
    h.store
        .put_object(&stored(Tool::Sccache, "k"), b"over a socket");
    let listener = h.gateway.listen(&path, h.reader("job-a")).unwrap();
    assert_eq!(listener.path(), path);
    assert_eq!(listener.binding().endpoint(), Endpoint::Read);
    assert_eq!(listener.binding().tool(), Tool::Sccache);
    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(format!("GET {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("k")).as_bytes())
        .await
        .unwrap();
    let reply = read_reply(&mut stream, false).await.unwrap();
    assert_eq!(
        (reply.status, reply.body.as_slice()),
        (200, &b"over a socket"[..])
    );
    drop(listener);
    assert!(!path.exists());
    assert!(tokio::net::UnixStream::connect(&path).await.is_err());
}

// ---------------------------------------------------------------------------
// With real provisioning files and a real TCP connection
// ---------------------------------------------------------------------------

/// A provisioning directory written the way producers write it: each file
/// to a temporary name, chmodded, then renamed into place.
struct Provisioned {
    _scratch: cortexkit_test_support::ScratchDir,
    layout: Layout,
}

impl Provisioned {
    fn new(role: HostRole) -> Self {
        let scratch = cortexkit_test_support::ScratchDir::new("gateway-provisioning");
        let layout = Layout::new(scratch.join("cache-provisioning"), role);
        layout.prepare().unwrap();
        Self {
            _scratch: scratch,
            layout,
        }
    }
    fn write(&self, name: &str, mode: u32, value: serde_json::Value) {
        use std::os::unix::fs::PermissionsExt;
        let temporary = self.layout.directory().join(format!(".{name}.tmp"));
        std::fs::write(&temporary, value.to_string()).unwrap();
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode)).unwrap();
        std::fs::rename(&temporary, self.layout.directory().join(name)).unwrap();
    }
    fn config(&self, generation: &str) {
        self.write(
            "config.json",
            0o444,
            serde_json::json!({
                "generation": generation,
                "tenant": TENANT,
                "endpoint": "http://store.test",
                "region": "de",
                "bucket": BUCKET,
                "prefix": derived_prefix(TENANT),
                "storage_class": CLASS,
                "enabled": true,
            }),
        );
    }
    fn key(&self, role: KeyRole, generation: &str) {
        let id = match role {
            KeyRole::Read => format!("READ-{generation}"),
            KeyRole::Write => format!("WRITE-{generation}"),
        };
        self.write(
            role.file_name(),
            0o400,
            serde_json::json!({
                "generation": generation,
                "access_key_id": id,
                "secret_access_key": format!("secret-{generation}"),
            }),
        );
    }
}

#[tokio::test(start_paused = true)]
async fn staggered_provisioning_updates_change_the_signing_key_only_when_complete() {
    let files = Provisioned::new(HostRole::BuildServer);
    files.key(KeyRole::Read, "1");
    files.key(KeyRole::Write, "1");
    files.config("1");
    let watch = Watch::new(files.layout.clone());
    watch.reread();
    let store = Arc::new(FakeStore::default());
    store.put_object(&stored(Tool::Sccache, "k"), b"v");
    let gateway = Gateway::with_connector(
        Limits::default(),
        Arc::new(watch.clone()),
        Arc::new(FakeConnector(store.clone())),
    );
    let h = Harness {
        gateway,
        store,
        generations: Arc::new(Switchable::default()),
    };
    let reader = h.reader("job-a");
    let writer = h.writer();
    let signer = |h: &Harness| h.store.seen().last().unwrap().access_key_id().to_string();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(signer(&h), "READ-1");
    // Config first, then the read key, then the write key: until the last
    // file lands the set is mixed and generation 1 stays in use.
    files.config("2");
    watch.reread();
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(signer(&h), "READ-1");
    files.key(KeyRole::Read, "2");
    watch.reread();
    assert_eq!(h.put(&writer, &s3("p"), b"v").await.status, 200);
    assert_eq!(signer(&h), "WRITE-1");
    files.key(KeyRole::Write, "2");
    assert!(watch.reread().activated);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(signer(&h), "READ-2");
    assert_eq!(h.put(&writer, &s3("p"), b"v").await.status, 200);
    assert_eq!(signer(&h), "WRITE-2");
}

#[tokio::test(start_paused = true)]
async fn a_tenant_vm_rotation_is_used_only_by_requests_admitted_after_it() {
    let files = Provisioned::new(HostRole::TenantVm);
    files.key(KeyRole::Read, "1");
    files.config("1");
    let watch = Watch::new(files.layout.clone());
    watch.reread();
    let store = Arc::new(FakeStore::default());
    store.put_object(&stored(Tool::Sccache, "k"), b"v");
    store.set(|k| k.respond_after = Some(Duration::from_secs(2)));
    let h = Harness {
        gateway: Gateway::with_connector(
            Limits::default(),
            Arc::new(watch.clone()),
            Arc::new(FakeConnector(store.clone())),
        ),
        store,
        generations: Arc::new(Switchable::default()),
    };
    let reader = h.reader("job-a");
    let early = {
        let (h, reader) = (h.clone(), reader.clone());
        tokio::spawn(async move { h.get(&reader, &s3("k")).await.status })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    files.key(KeyRole::Read, "2");
    files.config("2");
    assert!(watch.reread().activated);
    assert_eq!(h.get(&reader, &s3("k")).await.status, 200);
    assert_eq!(early.await.unwrap(), 200);
    let seen = h.store.seen();
    assert_eq!(seen[0].access_key_id(), "READ-1");
    assert_eq!(seen[1].access_key_id(), "READ-2");
    // A tenant VM holds no write key, so its write endpoint signs nothing.
    assert_eq!(h.put(&h.writer(), &s3("p"), b"v").await.status, 503);
    assert_eq!(h.store.seen_count(), 2);
}

/// The production connector over plain TCP, against a one-request server.
#[tokio::test]
async fn the_tcp_connector_reaches_an_http_endpoint_with_its_port() {
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    let accepted = tokio::spawn(async move {
        let (mut socket, _) = server.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&chunk[..n]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\netag: \"e\"\r\n\r\nhello")
            .await
            .unwrap();
        String::from_utf8(request).unwrap()
    });
    let generations = Arc::new(Switchable::default());
    let mut active = generation("1", TENANT);
    active.endpoint = format!("http://127.0.0.1:{port}/");
    generations.set(Some(active));
    let gateway = Gateway::new(Limits::default(), generations).unwrap();
    let reader = gateway.read_binding("job-a", Tool::Sccache, Adapter::S3);
    let (mut client, server_half) = tokio::io::duplex(64 * 1024);
    tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.serve(reader, server_half).await }
    });
    client
        .write_all(format!("GET {} HTTP/1.1\r\nhost: x\r\n\r\n", s3("k")).as_bytes())
        .await
        .unwrap();
    let reply = read_reply(&mut client, false).await.unwrap();
    assert_eq!((reply.status, reply.body.as_slice()), (200, &b"hello"[..]));
    let request = accepted.await.unwrap();
    assert!(
        request.starts_with(&format!("GET {} HTTP/1.1\r\n", stored(Tool::Sccache, "k"))),
        "{request}"
    );
    assert!(
        request.contains(&format!("host: 127.0.0.1:{port}\r\n")),
        "{request}"
    );
}
