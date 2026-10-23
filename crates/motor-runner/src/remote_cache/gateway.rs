//! The runner-owned gateway between build tools and the remote cache store.
//!
//! It runs inside the runner process and is the only part of the runner that
//! holds a cache key. Tools reach it through Unix sockets the runner binds into
//! their sandbox: a read socket per job and tool, signing with the read-only
//! key and allowing GET and HEAD, and a write socket per trusted fill and
//! tool, signing with the read-write key and also allowing PUT. Each socket is
//! bound to exactly one adapter (wire format) and one tool id; the tool id is
//! never taken from a request.
//!
//! Tools send bare keys; the gateway prepends `<prefix><tool>/` from the
//! active provisioning generation, signs the request with that generation
//! (the one active when the request was admitted) and streams bodies through
//! a bounded buffer. Failures answer so that tools carry on uncached: the read
//! endpoint answers a failure before response headers with 404 (a miss) and
//! resets the connection after them; the write endpoint answers 503; method,
//! query, encoding and path refusals get 403 on both.
mod breaker;
mod inbound;
mod metrics;
mod sigv4;
mod upstream;

pub(crate) use metrics::ToolStats;
pub(crate) use upstream::{Connector, Target, TcpConnector};

use super::provisioning::{ActiveGeneration, Credential, StoreIdentity, Watch};
use breaker::{Admission, Breaker, Outcome, Ticket, WhileProbing};
use inbound::{HeadError, Inbound, RequestHead};
use metrics::{bump, ToolCounters};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};
use upstream::{Conn, Failure, Pool, ResponseHead};

/// The read sccache makes at startup to check its storage. It is counted as a
/// probe, never as a hit or a miss.
pub(crate) const SCCACHE_CHECK_KEY: &str = ".sccache_check";
/// A read slower than the throughput floor counts against the read breaker
/// only after this much measured time.
pub(crate) const SLOW_WINDOW: Duration = Duration::from_secs(10);
/// How often a body transfer re-checks its throughput while the store sends
/// nothing.
const SLOW_TICK: Duration = Duration::from_secs(1);
/// Keys written by a fill that its write ledger keeps as a sample.
pub(crate) const KEY_SAMPLE: usize = 32;

/// A build tool the gateway serves. Its id names the directory under the
/// tenant prefix that holds its objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Tool {
    Sccache,
    Ccache,
}

impl Tool {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Sccache => "sccache",
            Self::Ccache => "ccache",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Sccache => 0,
            Self::Ccache => 1,
        }
    }
}

/// The wire format a socket speaks. An adapter only translates the request
/// path into a bare key; every trust decision is the gateway's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adapter {
    /// Path-style S3, `/<bucket>/<key>`, the single-object GET/HEAD/PUT
    /// pattern sccache uses. The bucket segment must be the configured bucket.
    S3,
    /// Plain HTTP by key, `/<key>`, as ccache's HTTP storage uses.
    Http,
}

impl Adapter {
    /// The bare key a request path names, or `None` when the path is refused:
    /// it contains `%` (nothing is decoded), a character outside
    /// `[A-Za-z0-9._/-]`, a `..` segment, or a key that is empty or starts
    /// with `/`.
    fn bare_key<'a>(self, path: &'a str, bucket: &str) -> Option<&'a str> {
        if path.contains('%') {
            return None;
        }
        let rest = path.strip_prefix('/')?;
        let key = match self {
            Self::Http => rest,
            Self::S3 => {
                let (first, key) = rest.split_once('/')?;
                if first != bucket {
                    return None;
                }
                key
            }
        };
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-');
        if key.is_empty()
            || key.starts_with('/')
            || !key.bytes().all(allowed)
            || key.split('/').any(|segment| segment == "..")
        {
            return None;
        }
        Some(key)
    }
}

/// The two endpoints. Each has its own breaker, shared by every socket of
/// that endpoint on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Read,
    Write,
}

impl Endpoint {
    /// The answer to a failure before response headers: a miss for readers,
    /// an error for fills.
    fn failure_status(self) -> u16 {
        match self {
            Self::Read => 404,
            Self::Write => 503,
        }
    }
}

/// Gateway limits; the defaults are the configured defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Largest object a PUT may send or a response may carry.
    pub(crate) max_object_bytes: u64,
    /// Longest full upstream key, prefix included.
    pub(crate) max_key_bytes: usize,
    /// The per-request buffer bodies stream through.
    pub(crate) stream_buffer_bytes: usize,
    /// In-flight read requests one job may hold.
    pub(crate) max_job_requests: usize,
    /// In-flight read requests across the gateway.
    pub(crate) max_total_requests: usize,
    /// Time to the first response byte, connecting included.
    pub(crate) first_byte_timeout: Duration,
    /// From receipt of a request to its last byte, retries included.
    pub(crate) deadline: Duration,
    /// Retries, made only when the store refuses or resets the connection
    /// before any response byte.
    pub(crate) max_retries: u32,
    /// A GET body arriving slower than this counts against the read breaker.
    pub(crate) min_upstream_bytes_per_s: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_object_bytes: 1 << 30,
            max_key_bytes: 1024,
            stream_buffer_bytes: 1 << 20,
            max_job_requests: 16,
            max_total_requests: 128,
            first_byte_timeout: Duration::from_secs(5),
            deadline: Duration::from_secs(120),
            max_retries: 1,
            min_upstream_bytes_per_s: 1 << 20,
        }
    }
}

/// Where the gateway gets the generation a request signs with.
pub(crate) trait Generations: Send + Sync + 'static {
    /// The generation for a request admitted now, or `None` when the cache
    /// is unavailable or switched off.
    fn admit(&self) -> Option<Arc<ActiveGeneration>>;
}

impl Generations for Watch {
    fn admit(&self) -> Option<Arc<ActiveGeneration>> {
        Watch::admit(self)
    }
}

/// What one socket serves.
#[derive(Debug)]
pub(crate) struct Binding {
    endpoint: Endpoint,
    tool: Tool,
    adapter: Adapter,
    /// The job's in-flight read count, shared by all its read sockets.
    job: Option<Arc<AtomicUsize>>,
    ledger: Option<Arc<WriteLedger>>,
}

impl Binding {
    pub(crate) fn endpoint(&self) -> Endpoint {
        self.endpoint
    }
    pub(crate) fn tool(&self) -> Tool {
        self.tool
    }
    /// The PUT record of a write socket.
    pub(crate) fn ledger(&self) -> Option<&Arc<WriteLedger>> {
        self.ledger.as_ref()
    }
}

/// What a fill's write socket saw, for deciding whether the fill published.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LedgerSummary {
    /// Requests on the socket not yet answered.
    pub(crate) in_flight: usize,
    /// PUTs the store answered 2xx after receiving the whole body.
    pub(crate) puts_stored: u64,
    /// Every other PUT: refused, answered by a breaker or limit, given a
    /// non-2xx status, or ended before the store's answer.
    pub(crate) puts_failed: u64,
    /// The store identity of every generation a PUT was signed under, in
    /// first-seen order.
    pub(crate) identities: Vec<StoreIdentity>,
    /// Up to [`KEY_SAMPLE`] bare keys of stored PUTs, in order.
    pub(crate) key_sample: Vec<String>,
}

/// The PUT record of one write socket.
#[derive(Debug)]
pub(crate) struct WriteLedger {
    tool: Tool,
    state: Mutex<LedgerSummary>,
    idle: Notify,
}

impl WriteLedger {
    pub(crate) fn tool(&self) -> Tool {
        self.tool
    }
    pub(crate) fn summary(&self) -> LedgerSummary {
        self.state.lock().unwrap().clone()
    }

    /// Waits until no request on the socket is in flight.
    pub(crate) async fn idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().unwrap().in_flight == 0 {
                return;
            }
            notified.await;
        }
    }

    fn begin(self: &Arc<Self>) -> Flight {
        self.state.lock().unwrap().in_flight += 1;
        Flight(self.clone())
    }
}

struct Flight(Arc<WriteLedger>);

impl Drop for Flight {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.in_flight -= 1;
        if state.in_flight == 0 {
            self.0.idle.notify_waiters();
        }
    }
}

/// One PUT on a write socket; it is recorded as failed unless marked stored.
struct PutRecord {
    ledger: Arc<WriteLedger>,
    identity: Option<StoreIdentity>,
    key: Option<String>,
    stored: bool,
}

impl Drop for PutRecord {
    fn drop(&mut self) {
        let mut state = self.ledger.state.lock().unwrap();
        if self.stored {
            state.puts_stored += 1;
            if state.key_sample.len() < KEY_SAMPLE {
                if let Some(key) = self.key.take() {
                    state.key_sample.push(key);
                }
            }
        } else {
            state.puts_failed += 1;
        }
        if let Some(identity) = self.identity.take() {
            if !state.identities.contains(&identity) {
                state.identities.push(identity);
            }
        }
    }
}

/// A read request's place under the admission caps.
struct ReadSlot {
    total: Arc<AtomicUsize>,
    job: Arc<AtomicUsize>,
}

impl Drop for ReadSlot {
    fn drop(&mut self) {
        self.job.fetch_sub(1, Ordering::AcqRel);
        self.total.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Shared {
    limits: Limits,
    generations: Arc<dyn Generations>,
    connector: Arc<dyn Connector>,
    pool: Pool,
    read_breaker: Arc<Breaker>,
    write_breaker: Arc<Breaker>,
    total_reads: Arc<AtomicUsize>,
    jobs: Mutex<HashMap<String, Weak<AtomicUsize>>>,
    counters: [ToolCounters; 2],
    origin: Instant,
}

/// The gateway. Cloning shares it.
#[derive(Clone)]
pub(crate) struct Gateway {
    shared: Arc<Shared>,
}

/// A bound socket, served until dropped; dropping removes the socket file.
/// Connections already accepted finish their current requests.
pub(crate) struct Listener {
    path: PathBuf,
    binding: Arc<Binding>,
    task: tokio::task::JoinHandle<()>,
}

impl Listener {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn binding(&self) -> &Arc<Binding> {
        &self.binding
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether a connection can carry another request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    KeepAlive,
    Close,
}

/// A request that passed the method, query and encoding checks.
struct Request {
    method: Method,
    path: String,
    content_length: u64,
    range: Option<String>,
    expect_continue: bool,
    close_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Get,
    Head,
    Put,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Put => "PUT",
        }
    }
}

/// Checks method, query and encoding; `None` is a refusal.
fn parse_request(head: &RequestHead, endpoint: Endpoint) -> Option<Request> {
    let method = match (head.method.as_str(), endpoint) {
        ("GET", _) => Method::Get,
        ("HEAD", _) => Method::Head,
        ("PUT", Endpoint::Write) => Method::Put,
        _ => return None,
    };
    // Any query string is refused, which also refuses ListObjectsV2.
    if head.target.contains('?') || head.has("transfer-encoding") {
        return None;
    }
    let mut content_length = None;
    for value in head.values("content-length") {
        let text = std::str::from_utf8(value).ok()?.trim();
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let parsed: u64 = text.parse().ok()?;
        if content_length.is_some_and(|known| known != parsed) {
            return None;
        }
        content_length = Some(parsed);
    }
    let content_length = content_length.unwrap_or(0);
    if method != Method::Put && content_length != 0 {
        return None;
    }
    let mut range = None;
    for value in head.values("range") {
        let text = std::str::from_utf8(value).ok()?.trim();
        let spec = text.strip_prefix("bytes=")?;
        if range.is_some()
            || spec.is_empty()
            || !spec
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b',')
        {
            return None;
        }
        range = Some(text.to_string());
    }
    let expect_continue = head
        .values("expect")
        .any(|value| value.trim_ascii().eq_ignore_ascii_case(b"100-continue"));
    Some(Request {
        method,
        path: head.target.clone(),
        content_length,
        range,
        expect_continue,
        close_requested: head.close_requested(),
    })
}

/// Response headers forwarded to tools; everything else, and every error
/// body, is dropped (an S3 error body can echo the access key id).
const FORWARDED: [&str; 4] = ["etag", "last-modified", "content-type", "content-range"];

fn forwarded(head: &ResponseHead) -> Vec<(&str, &str)> {
    head.headers
        .iter()
        .filter(|(name, _)| FORWARDED.contains(&name.as_str()))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Where the store endpoint of a generation is, and the path its bucket is
/// addressed under.
struct Upstream {
    target: Target,
    host_header: String,
    base_path: String,
}

fn upstream_of(generation: &ActiveGeneration) -> Option<Upstream> {
    let url = url::Url::parse(&generation.endpoint).ok()?;
    let tls = match url.scheme() {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let host_str = url.host_str()?;
    let host = host_str
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host_str)
        .to_string();
    let host_header = match url.port() {
        Some(port) => format!("{host_str}:{port}"),
        None => host_str.to_string(),
    };
    Some(Upstream {
        target: Target {
            tls,
            host,
            port: url.port_or_known_default()?,
        },
        host_header,
        base_path: url.path().trim_end_matches('/').to_string(),
    })
}

/// Everything one admitted request needs to talk to the store.
struct Exchange<'a> {
    request: &'a Request,
    upstream: &'a Upstream,
    generation: &'a ActiveGeneration,
    credential: &'a Credential,
    canonical_uri: String,
    storage_class: Option<&'a str>,
    deadline: Instant,
}

/// How an attempt to get a response head failed.
enum AttemptError {
    Upstream(Failure),
    /// The tool went away, or stopped sending its body, during a PUT.
    Client,
}

impl Gateway {
    /// A gateway connecting to the store over TCP and TLS.
    pub(crate) fn new(limits: Limits, generations: Arc<dyn Generations>) -> io::Result<Self> {
        Ok(Self::with_connector(
            limits,
            generations,
            Arc::new(TcpConnector::new()?),
        ))
    }

    pub(crate) fn with_connector(
        limits: Limits,
        generations: Arc<dyn Generations>,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                limits,
                generations,
                connector,
                pool: Pool::default(),
                read_breaker: Breaker::new(WhileProbing::Answer),
                write_breaker: Breaker::new(WhileProbing::Hold),
                total_reads: Arc::new(AtomicUsize::new(0)),
                jobs: Mutex::new(HashMap::new()),
                counters: Default::default(),
                origin: Instant::now(),
            }),
        }
    }

    /// A read socket's binding for one job. Every read binding of a job
    /// shares its admission cap.
    pub(crate) fn read_binding(&self, job: &str, tool: Tool, adapter: Adapter) -> Arc<Binding> {
        let mut jobs = self.shared.jobs.lock().unwrap();
        jobs.retain(|_, count| count.strong_count() > 0);
        let count = match jobs.get(job).and_then(Weak::upgrade) {
            Some(count) => count,
            None => {
                let count = Arc::new(AtomicUsize::new(0));
                jobs.insert(job.to_string(), Arc::downgrade(&count));
                count
            }
        };
        Arc::new(Binding {
            endpoint: Endpoint::Read,
            tool,
            adapter,
            job: Some(count),
            ledger: None,
        })
    }

    /// A write socket's binding for one fill, with a fresh PUT ledger.
    pub(crate) fn write_binding(&self, tool: Tool, adapter: Adapter) -> Arc<Binding> {
        Arc::new(Binding {
            endpoint: Endpoint::Write,
            tool,
            adapter,
            job: None,
            ledger: Some(Arc::new(WriteLedger {
                tool,
                state: Mutex::new(LedgerSummary::default()),
                idle: Notify::new(),
            })),
        })
    }

    /// Binds a Unix socket at `path` and serves `binding` on it. Who may
    /// connect is decided by the socket file's mode and by which sandbox the
    /// runner binds it into.
    pub(crate) fn listen(&self, path: &Path, binding: Arc<Binding>) -> io::Result<Listener> {
        let listener = tokio::net::UnixListener::bind(path)?;
        let gateway = self.clone();
        let served = binding.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let gateway = gateway.clone();
                let binding = served.clone();
                tokio::spawn(async move { gateway.serve(binding, stream).await });
            }
        });
        Ok(Listener {
            path: path.to_path_buf(),
            binding,
            task,
        })
    }

    /// Serves requests on one tool connection until it closes.
    pub(crate) async fn serve<S>(&self, binding: Arc<Binding>, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut client = Inbound::new(stream);
        loop {
            let head = match client.read_head().await {
                Ok(head) => head,
                Err(HeadError::Gone) => return,
                Err(HeadError::Malformed) => {
                    self.refused(&binding);
                    let _ = client.respond(403, &[], 0, true).await;
                    client.shutdown().await;
                    return;
                }
            };
            let flight = binding.ledger.as_ref().map(|ledger| ledger.begin());
            let next = self.handle(&binding, &mut client, head).await;
            drop(flight);
            if next == Next::Close {
                client.shutdown().await;
                return;
            }
        }
    }

    /// One tool's transport counts.
    pub(crate) fn stats(&self, tool: Tool) -> ToolStats {
        self.shared.counters[tool.index()].snapshot(self.shared.origin)
    }

    /// The end of the write breaker's open window while it is open.
    pub(crate) fn write_breaker_open_until(&self) -> Option<Instant> {
        self.shared.write_breaker.open_until()
    }

    /// The end of the read breaker's open window while it is open.
    pub(crate) fn read_breaker_open_until(&self) -> Option<Instant> {
        self.shared.read_breaker.open_until()
    }

    fn counters(&self, tool: Tool) -> &ToolCounters {
        &self.shared.counters[tool.index()]
    }

    fn refused(&self, binding: &Binding) {
        let counters = self.counters(binding.tool);
        bump(match binding.endpoint {
            Endpoint::Read => &counters.read_refusals,
            Endpoint::Write => &counters.write_refusals,
        });
    }

    fn admit_read(&self, binding: &Binding) -> Option<ReadSlot> {
        let job = binding.job.clone()?;
        let total = self.shared.total_reads.clone();
        let limits = &self.shared.limits;
        if total.fetch_add(1, Ordering::AcqRel) >= limits.max_total_requests {
            total.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        if job.fetch_add(1, Ordering::AcqRel) >= limits.max_job_requests {
            job.fetch_sub(1, Ordering::AcqRel);
            total.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ReadSlot { total, job })
    }

    /// Records a counted failure on the ticket, once per request.
    fn fail(&self, ticket: &mut Ticket, tool: Tool) {
        if !ticket.reported() {
            bump(&self.counters(tool).errors);
            ticket.report(Outcome::Failure);
        }
    }

    /// Answers without having read a request body, closing the connection
    /// when one may follow.
    async fn answer<S>(
        &self,
        client: &mut Inbound<S>,
        status: u16,
        body_free: bool,
        close_requested: bool,
    ) -> Next
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let close = !body_free || close_requested;
        match client.respond(status, &[], 0, close).await {
            Ok(()) if !close => Next::KeepAlive,
            _ => Next::Close,
        }
    }

    async fn handle<S>(&self, binding: &Binding, client: &mut Inbound<S>, head: RequestHead) -> Next
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let received = Instant::now();
        let limits = &self.shared.limits;
        let deadline = received + limits.deadline;
        let endpoint = binding.endpoint;
        let failure = endpoint.failure_status();
        let body_free = head.body_free();
        let mut put = (head.method == "PUT")
            .then(|| binding.ledger.clone())
            .flatten()
            .map(|ledger| PutRecord {
                ledger,
                identity: None,
                key: None,
                stored: false,
            });
        let Some(request) = parse_request(&head, endpoint) else {
            self.refused(binding);
            return self
                .answer(client, 403, body_free, head.close_requested())
                .await;
        };
        let close_requested = request.close_requested;
        let Some(generation) = self.shared.generations.admit() else {
            return self
                .answer(client, failure, body_free, close_requested)
                .await;
        };
        let Some(bare) = binding.adapter.bare_key(&request.path, &generation.bucket) else {
            self.refused(binding);
            return self.answer(client, 403, body_free, close_requested).await;
        };
        let key = format!("{}{}/{}", generation.prefix, binding.tool.id(), bare);
        if key.len() > limits.max_key_bytes {
            self.refused(binding);
            return self.answer(client, 403, body_free, close_requested).await;
        }
        if request.content_length > limits.max_object_bytes {
            self.refused(binding);
            return self
                .answer(client, failure, body_free, close_requested)
                .await;
        }
        let _slot = match endpoint {
            Endpoint::Read => match self.admit_read(binding) {
                Some(slot) => Some(slot),
                None => {
                    self.refused(binding);
                    return self
                        .answer(client, failure, body_free, close_requested)
                        .await;
                }
            },
            Endpoint::Write => None,
        };
        let credential = match endpoint {
            Endpoint::Read => &generation.read_key,
            Endpoint::Write => match &generation.write_key {
                Some(key) => key,
                // Only the build server holds a write key; without one the
                // write endpoint cannot sign anything.
                None => {
                    return self
                        .answer(client, failure, body_free, close_requested)
                        .await
                }
            },
        };
        let Some(upstream) = upstream_of(&generation) else {
            return self
                .answer(client, failure, body_free, close_requested)
                .await;
        };
        let breaker = match endpoint {
            Endpoint::Read => &self.shared.read_breaker,
            Endpoint::Write => &self.shared.write_breaker,
        };
        let mut ticket = match breaker.admit(deadline).await {
            Admission::Admitted(ticket) => ticket,
            Admission::Refused => {
                bump(&self.counters(binding.tool).breaker_open);
                return self
                    .answer(client, failure, body_free, close_requested)
                    .await;
            }
        };
        if let Some(put) = &mut put {
            // Every PUT is tagged with the store identity of the generation
            // that signs it, so a fill can tell whether all of its PUTs went
            // to the store it started with.
            put.identity = Some(generation.identity());
            put.key = Some(bare.to_string());
        }
        let exchange = Exchange {
            request: &request,
            upstream: &upstream,
            generation: &generation,
            credential,
            canonical_uri: format!(
                "{}/{}/{}",
                upstream.base_path,
                sigv4::uri_encode(&generation.bucket, false),
                sigv4::uri_encode(&key, true)
            ),
            storage_class: (request.method == Method::Put)
                .then_some(generation.storage_class.as_str()),
            deadline,
        };
        let probe = binding.tool == Tool::Sccache && bare == SCCACHE_CHECK_KEY;
        let next = self
            .exchange(binding, client, &exchange, &mut ticket, put.as_mut(), probe)
            .await;
        self.counters(binding.tool)
            .record_latency(self.shared.origin, received.elapsed());
        next
    }

    /// The request head sent to the store, signed for this attempt.
    fn upstream_head(&self, exchange: &Exchange<'_>) -> Vec<u8> {
        let amz_date = sigv4::amz_date(std::time::SystemTime::now());
        let mut headers = vec![
            ("host".to_string(), exchange.upstream.host_header.clone()),
            (
                "x-amz-content-sha256".to_string(),
                sigv4::UNSIGNED_PAYLOAD.to_string(),
            ),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(range) = &exchange.request.range {
            headers.push(("range".to_string(), range.clone()));
        }
        if let Some(class) = exchange.storage_class {
            headers.push(("x-amz-storage-class".to_string(), class.to_string()));
        }
        let method = exchange.request.method.as_str();
        let authorization = sigv4::authorization(
            method,
            &exchange.canonical_uri,
            &headers,
            sigv4::UNSIGNED_PAYLOAD,
            exchange.credential,
            &exchange.generation.region,
            &amz_date,
        );
        let mut text = format!("{method} {} HTTP/1.1\r\n", exchange.canonical_uri);
        for (name, value) in &headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str(&format!("authorization: {authorization}\r\n"));
        if exchange.request.method == Method::Put {
            text.push_str(&format!(
                "content-length: {}\r\n",
                exchange.request.content_length
            ));
        }
        text.push_str("\r\n");
        text.into_bytes()
    }

    /// One attempt at getting a response head from the store. For a PUT it
    /// also streams the tool's body; `body_started` is set once any of it
    /// has been read, after which the request cannot be retried.
    async fn attempt<S>(
        &self,
        client: &mut Inbound<S>,
        exchange: &Exchange<'_>,
        body_started: &mut bool,
        continue_sent: &mut bool,
    ) -> Result<(Conn, ResponseHead), AttemptError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let limits = &self.shared.limits;
        let deadline = exchange.deadline;
        let first_byte_by = Instant::now() + limits.first_byte_timeout;
        let waiting_limit = |first_byte_by: Instant| {
            if first_byte_by <= deadline {
                Failure::FirstByteTimeout
            } else {
                Failure::Deadline
            }
        };
        let target = &exchange.upstream.target;
        let is_put = exchange.request.method == Method::Put;
        // A PUT always gets a fresh connection: its body cannot be replayed,
        // so it must not land on an idle connection the store has dropped.
        let pooled = if is_put {
            None
        } else {
            self.shared.pool.take(target)
        };
        let io = match pooled {
            Some(io) => io,
            None => match tokio::time::timeout_at(
                first_byte_by.min(deadline),
                self.shared.connector.connect(target),
            )
            .await
            {
                Err(_) => return Err(AttemptError::Upstream(waiting_limit(first_byte_by))),
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    return Err(AttemptError::Upstream(Failure::BeforeResponse))
                }
                Ok(Err(_)) => return Err(AttemptError::Upstream(Failure::Transport)),
                Ok(Ok(io)) => io,
            },
        };
        let mut conn = Conn::new(io);
        let head = self.upstream_head(exchange);
        let sent = tokio::time::timeout_at(first_byte_by.min(deadline), async {
            conn.io.write_all(&head).await?;
            conn.io.flush().await
        })
        .await;
        match sent {
            Err(_) => return Err(AttemptError::Upstream(waiting_limit(first_byte_by))),
            Ok(Err(_)) => return Err(AttemptError::Upstream(Failure::BeforeResponse)),
            Ok(Ok(())) => {}
        }
        let mut first_byte_by = first_byte_by;
        if is_put {
            let mut left = exchange.request.content_length;
            if left > 0 && exchange.request.expect_continue && !*continue_sent {
                *continue_sent = true;
                if client
                    .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                    .await
                    .is_err()
                {
                    return Err(AttemptError::Client);
                }
            }
            let size = (limits.stream_buffer_bytes as u64).min(left).max(1) as usize;
            let mut buf = vec![0u8; size];
            while left > 0 {
                let want = (buf.len() as u64).min(left) as usize;
                let n = match tokio::time::timeout_at(deadline, client.read_body(&mut buf[..want]))
                    .await
                {
                    Ok(Ok(n)) if n > 0 => n,
                    _ => return Err(AttemptError::Client),
                };
                *body_started = true;
                match tokio::time::timeout_at(deadline, conn.io.write_all(&buf[..n])).await {
                    Err(_) => return Err(AttemptError::Upstream(Failure::Deadline)),
                    Ok(Err(_)) => return Err(AttemptError::Upstream(Failure::Transport)),
                    Ok(Ok(())) => {}
                }
                left -= n as u64;
            }
            match tokio::time::timeout_at(deadline, conn.io.flush()).await {
                Err(_) => return Err(AttemptError::Upstream(Failure::Deadline)),
                Ok(Err(_)) => return Err(AttemptError::Upstream(Failure::Transport)),
                Ok(Ok(())) => {}
            }
            // The first-byte limit restarts once the whole body is sent: a
            // large upload is bounded by the overall deadline instead.
            first_byte_by = Instant::now() + limits.first_byte_timeout;
        }
        match conn.read_head(first_byte_by, deadline).await {
            // Bytes of the body already went to the store, so a reset is no
            // longer one a retry may repeat.
            Err(Failure::BeforeResponse) if *body_started => {
                Err(AttemptError::Upstream(Failure::Transport))
            }
            Err(failure) => Err(AttemptError::Upstream(failure)),
            Ok(head) => Ok((conn, head)),
        }
    }

    async fn exchange<S>(
        &self,
        binding: &Binding,
        client: &mut Inbound<S>,
        exchange: &Exchange<'_>,
        ticket: &mut Ticket,
        mut put: Option<&mut PutRecord>,
        probe: bool,
    ) -> Next
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let limits = &self.shared.limits;
        let request = exchange.request;
        let deadline = exchange.deadline;
        let endpoint = binding.endpoint;
        let failure = endpoint.failure_status();
        let counters = self.counters(binding.tool);
        let mut body_started = false;
        let mut continue_sent = false;
        let mut retries = 0;
        let (mut conn, head) = loop {
            match self
                .attempt(client, exchange, &mut body_started, &mut continue_sent)
                .await
            {
                Ok(response) => break response,
                Err(AttemptError::Upstream(Failure::BeforeResponse))
                    if retries < limits.max_retries
                        && !body_started
                        && Instant::now() < deadline =>
                {
                    retries += 1;
                }
                Err(AttemptError::Upstream(_)) => {
                    self.fail(ticket, binding.tool);
                    // A PUT body may be partly unread, so the connection
                    // cannot carry another request.
                    let body_free = request.method != Method::Put || request.content_length == 0;
                    return self
                        .answer(client, failure, body_free, request.close_requested)
                        .await;
                }
                Err(AttemptError::Client) => {
                    ticket.report(Outcome::Neutral);
                    return Next::Close;
                }
            }
        };
        let close_requested = request.close_requested;
        let status = head.status;
        if status >= 500 {
            self.fail(ticket, binding.tool);
            return self.answer(client, failure, true, close_requested).await;
        }
        // The store answered; whatever follows, it is reachable.
        if request.method == Method::Put {
            ticket.report(Outcome::Success);
            let length = head.content_length.unwrap_or(0);
            let reusable = !head.close && conn.discard(length, deadline).await;
            if !(200..300).contains(&status) {
                return self.answer(client, failure, true, close_requested).await;
            }
            if let Some(put) = put.as_mut() {
                put.stored = true;
            }
            if reusable {
                if let Some(io) = conn.into_io() {
                    self.shared.pool.put(&exchange.upstream.target, io);
                }
            }
            return match client
                .respond(status, &forwarded(&head), 0, close_requested)
                .await
            {
                Ok(()) if !close_requested => Next::KeepAlive,
                _ => Next::Close,
            };
        }
        if probe {
            bump(&counters.probes);
        }
        let reuse = |conn: Conn, gateway: &Self| {
            if !head.close {
                if let Some(io) = conn.into_io() {
                    gateway.shared.pool.put(&exchange.upstream.target, io);
                }
            }
        };
        match status {
            200 | 206 => {}
            304 => {
                ticket.report(Outcome::Success);
                reuse(conn, self);
                return match client
                    .respond(304, &forwarded(&head), 0, close_requested)
                    .await
                {
                    Ok(()) if !close_requested => Next::KeepAlive,
                    _ => Next::Close,
                };
            }
            _ => {
                ticket.report(Outcome::Success);
                if status == 404 && !probe {
                    bump(&counters.misses);
                }
                let length = head.content_length.unwrap_or(0);
                if request.method == Method::Head
                    || (!head.close && conn.discard(length, deadline).await)
                {
                    reuse(conn, self);
                }
                // A 404 is a miss on both endpoints; anything else the store
                // refuses gets the endpoint's failure answer.
                let answer = if status == 404 { 404 } else { failure };
                return self.answer(client, answer, true, close_requested).await;
            }
        }
        // 200 or 206: forward it when its length is known and within limits.
        let Some(length) = head.content_length else {
            ticket.report(Outcome::Success);
            return self.answer(client, failure, true, close_requested).await;
        };
        if length > limits.max_object_bytes {
            ticket.report(Outcome::Success);
            bump(&counters.oversize);
            return self.answer(client, failure, true, close_requested).await;
        }
        if request.method == Method::Get && !probe {
            bump(&counters.hits);
        }
        if client
            .respond(status, &forwarded(&head), length, close_requested)
            .await
            .is_err()
        {
            ticket.report(Outcome::Neutral);
            return Next::Close;
        }
        if request.method == Method::Head {
            ticket.report(Outcome::Success);
            reuse(conn, self);
            return if close_requested {
                Next::Close
            } else {
                Next::KeepAlive
            };
        }
        let streamed = self
            .stream_body(
                binding,
                client,
                &mut conn,
                length,
                ticket,
                endpoint == Endpoint::Read,
                deadline,
            )
            .await;
        if !streamed {
            // Headers are out, so the only failure answer left is to end the
            // connection; the tool sees a short body and compiles uncached.
            return Next::Close;
        }
        ticket.report(Outcome::Success);
        reuse(conn, self);
        if close_requested {
            Next::Close
        } else {
            Next::KeepAlive
        }
    }

    /// Streams `length` body bytes from the store to the tool through the
    /// bounded buffer. Returns whether the whole body was delivered.
    ///
    /// When `slow_counts` is set, a body arriving below the throughput floor
    /// counts once against the breaker. Throughput is measured from the first
    /// body byte and only over time spent waiting on the store, so time the
    /// buffer sits full behind a slow tool never counts, and only after
    /// [`SLOW_WINDOW`] of measured time while the transfer is unfinished.
    #[allow(clippy::too_many_arguments)]
    async fn stream_body<S>(
        &self,
        binding: &Binding,
        client: &mut Inbound<S>,
        conn: &mut Conn,
        length: u64,
        ticket: &mut Ticket,
        slow_counts: bool,
        deadline: Instant,
    ) -> bool
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let limits = &self.shared.limits;
        let size = (limits.stream_buffer_bytes as u64).min(length).max(1) as usize;
        let mut buf = vec![0u8; size];
        let mut left = length;
        let mut started = false;
        let mut measured = Duration::ZERO;
        let mut measured_bytes = 0u64;
        while left > 0 {
            let want = (buf.len() as u64).min(left) as usize;
            let waiting_since = Instant::now();
            let read = tokio::time::timeout_at(
                (waiting_since + SLOW_TICK).min(deadline),
                conn.read_body(&mut buf[..want]),
            )
            .await;
            if started {
                measured += waiting_since.elapsed();
            }
            match read {
                Err(_) => {
                    if Instant::now() >= deadline {
                        self.fail(ticket, binding.tool);
                        return false;
                    }
                }
                Ok(Ok(0)) | Ok(Err(_)) => {
                    self.fail(ticket, binding.tool);
                    return false;
                }
                Ok(Ok(n)) => {
                    if started {
                        measured_bytes += n as u64;
                    }
                    started = true;
                    left -= n as u64;
                    match tokio::time::timeout_at(deadline, client.write_all(&buf[..n])).await {
                        Ok(Ok(())) => {}
                        // A tool that stops reading or goes away says nothing
                        // about the store.
                        _ => {
                            ticket.report(Outcome::Neutral);
                            return false;
                        }
                    }
                }
            }
            if slow_counts
                && started
                && left > 0
                && !ticket.reported()
                && measured >= SLOW_WINDOW
                && (measured_bytes as f64)
                    < limits.min_upstream_bytes_per_s as f64 * measured.as_secs_f64()
            {
                self.fail(ticket, binding.tool);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests;
