//! The gateway's HTTP/1.1 client to the object store: one request at a time
//! per connection, with idle connections kept for reuse.
//!
//! The client is small on purpose: the gateway must know exactly when the
//! first response byte arrives (the first-byte limit and the retry rule both
//! depend on it), and it streams bodies through its own bounded buffer.
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::time::{Duration, Instant};

/// A connection to the store, plain or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}
pub(crate) type BoxedIo = Box<dyn Io>;
pub(crate) type ConnectFuture<'a> = Pin<Box<dyn Future<Output = io::Result<BoxedIo>> + Send + 'a>>;

/// Opens connections to the store. Tests replace it with an in-memory store.
pub(crate) trait Connector: Send + Sync + 'static {
    fn connect<'a>(&'a self, target: &'a Target) -> ConnectFuture<'a>;
}

/// Where a store endpoint is reached.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Target {
    pub(crate) tls: bool,
    /// Host name or IP address, without IPv6 brackets.
    pub(crate) host: String,
    pub(crate) port: u16,
}

/// Connects over TCP, with TLS verified against the public web roots for
/// `https` endpoints.
pub(crate) struct TcpConnector {
    tls: tokio_rustls::TlsConnector,
    /// Set only by a test constructor, so tests can reach a plain local
    /// server; production endpoints are always `https`.
    plaintext: bool,
}

impl TcpConnector {
    pub(crate) fn new() -> io::Result<Self> {
        let provider = rustls::crypto::ring::default_provider();
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            tls: tokio_rustls::TlsConnector::from(Arc::new(config)),
            plaintext: false,
        })
    }

    /// A connector that skips TLS even for `https` targets, for tests
    /// against a plain local server.
    #[cfg(test)]
    pub(crate) fn plaintext_for_tests() -> io::Result<Self> {
        Ok(Self {
            plaintext: true,
            ..Self::new()?
        })
    }
}

impl Connector for TcpConnector {
    fn connect<'a>(&'a self, target: &'a Target) -> ConnectFuture<'a> {
        Box::pin(async move {
            let tcp = tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
            tcp.set_nodelay(true)?;
            if !target.tls || self.plaintext {
                return Ok(Box::new(tcp) as BoxedIo);
            }
            let name = rustls::pki_types::ServerName::try_from(target.host.clone())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let tls = self.tls.connect(name, tcp).await?;
            Ok(Box::new(tls) as BoxedIo)
        })
    }
}

/// How long an idle connection is kept. Stores close idle connections after
/// varying times, so this stays short.
const IDLE_FOR: Duration = Duration::from_secs(15);
const IDLE_PER_TARGET: usize = 32;

#[derive(Default)]
pub(super) struct Pool {
    idle: Mutex<HashMap<Target, Vec<(BoxedIo, Instant)>>>,
}

impl Pool {
    pub(super) fn take(&self, target: &Target) -> Option<BoxedIo> {
        let mut idle = self.idle.lock().unwrap();
        let list = idle.get_mut(target)?;
        while let Some((io, since)) = list.pop() {
            if since.elapsed() < IDLE_FOR {
                return Some(io);
            }
        }
        None
    }
    pub(super) fn put(&self, target: &Target, io: BoxedIo) {
        let mut idle = self.idle.lock().unwrap();
        let list = idle.entry(target.clone()).or_default();
        list.retain(|(_, since)| since.elapsed() < IDLE_FOR);
        if list.len() < IDLE_PER_TARGET {
            list.push((io, Instant::now()));
        }
    }
}

/// Response heads larger than this are a protocol error.
const MAX_RESPONSE_HEAD: usize = 64 * 1024;
const MAX_RESPONSE_HEADERS: usize = 100;

/// Why an exchange with the store failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// The connection was refused, reset or closed before any response
    /// byte; the one case a retry is allowed.
    BeforeResponse,
    /// No response byte within the first-byte limit (connecting included).
    FirstByteTimeout,
    /// The overall deadline passed while waiting on the store.
    Deadline,
    /// Any other transport or protocol failure.
    Transport,
}

/// How a response's body is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Framing {
    /// No body: a HEAD request, or a 204 or 304 answer.
    Empty,
    /// Exactly this many bytes.
    Length(u64),
    /// No length at all, so the body runs to the end of the connection; a
    /// chunked body is treated the same way, since the gateway does not
    /// decode chunks. Either way the connection cannot be reused.
    UntilClose,
}

/// A response's head, with the framing the gateway needs.
#[derive(Debug)]
pub(super) struct ResponseHead {
    pub(super) status: u16,
    /// Lowercase names.
    pub(super) headers: Vec<(String, String)>,
    /// The `content-length` header; for a HEAD request the object's length.
    pub(super) content_length: Option<u64>,
    pub(super) framing: Framing,
    /// The store asked to close, or the body runs to the end of the
    /// connection, so the connection cannot be reused.
    pub(super) close: bool,
}

/// One connection to the store and the bytes read from it but not consumed.
pub(super) struct Conn {
    pub(super) io: BoxedIo,
    buf: Vec<u8>,
    pos: usize,
    /// Response bytes read so far on this connection for the current request.
    pub(super) received: u64,
}

impl Conn {
    pub(super) fn new(io: BoxedIo) -> Self {
        Self {
            io,
            buf: Vec::new(),
            pos: 0,
            received: 0,
        }
    }

    pub(super) fn into_io(self) -> Option<BoxedIo> {
        // Unconsumed bytes would be read as the next response.
        (self.pos == self.buf.len()).then_some(self.io)
    }

    async fn fill(&mut self) -> io::Result<usize> {
        // Drop what is consumed, so the buffer holds at most one head.
        self.buf.drain(..self.pos);
        self.pos = 0;
        let mut chunk = [0u8; 8 * 1024];
        let n = self.io.read(&mut chunk).await?;
        self.buf.extend_from_slice(&chunk[..n]);
        self.received += n as u64;
        Ok(n)
    }

    /// Reads the response head to a request (`head_request` for HEAD),
    /// skipping interim 1xx answers. The first byte must arrive by
    /// `first_byte_by`, the rest by `deadline`.
    pub(super) async fn read_head(
        &mut self,
        first_byte_by: Instant,
        deadline: Instant,
        head_request: bool,
    ) -> Result<ResponseHead, Failure> {
        if self.pos == self.buf.len() {
            match tokio::time::timeout_at(first_byte_by.min(deadline), self.fill()).await {
                Err(_) if first_byte_by <= deadline => return Err(Failure::FirstByteTimeout),
                Err(_) => return Err(Failure::Deadline),
                Ok(Ok(0)) | Ok(Err(_)) => return Err(Failure::BeforeResponse),
                Ok(Ok(_)) => {}
            }
        }
        loop {
            let mut headers = [httparse::EMPTY_HEADER; MAX_RESPONSE_HEADERS];
            let mut response = httparse::Response::new(&mut headers);
            match response.parse(&self.buf[self.pos..]) {
                Ok(httparse::Status::Complete(length)) => {
                    if length > MAX_RESPONSE_HEAD {
                        return Err(Failure::Transport);
                    }
                    let status = response.code.ok_or(Failure::Transport)?;
                    let mut head = ResponseHead {
                        status,
                        headers: Vec::new(),
                        content_length: None,
                        framing: Framing::UntilClose,
                        close: response.version == Some(0),
                    };
                    let mut chunked = false;
                    for header in response.headers.iter() {
                        let name = header.name.to_ascii_lowercase();
                        let value = std::str::from_utf8(header.value)
                            .map_err(|_| Failure::Transport)?
                            .trim()
                            .to_string();
                        match name.as_str() {
                            "content-length" => {
                                let parsed = value.parse().map_err(|_| Failure::Transport)?;
                                if head.content_length.is_some_and(|known| known != parsed) {
                                    return Err(Failure::Transport);
                                }
                                head.content_length = Some(parsed);
                            }
                            "transfer-encoding" => chunked = true,
                            "connection" if value.eq_ignore_ascii_case("close") => {
                                head.close = true
                            }
                            _ => {}
                        }
                        head.headers.push((name, value));
                    }
                    self.pos += length;
                    if (100..200).contains(&status) {
                        // An interim answer (100 Continue and the like)
                        // precedes the real one; a protocol switch is never
                        // acceptable.
                        if status == 101 {
                            return Err(Failure::Transport);
                        }
                        if self.pos == self.buf.len() {
                            match tokio::time::timeout_at(deadline, self.fill()).await {
                                Err(_) => return Err(Failure::Deadline),
                                Ok(Ok(0)) | Ok(Err(_)) => return Err(Failure::Transport),
                                Ok(Ok(_)) => {}
                            }
                        }
                        continue;
                    }
                    // Both framings at once make the body's end ambiguous
                    // (the classic request-smuggling shape): refused.
                    if chunked && head.content_length.is_some() {
                        return Err(Failure::Transport);
                    }
                    head.framing = if head_request || status == 204 || status == 304 {
                        Framing::Empty
                    } else if chunked {
                        Framing::UntilClose
                    } else {
                        match head.content_length {
                            Some(length) => Framing::Length(length),
                            None => Framing::UntilClose,
                        }
                    };
                    if head.framing == Framing::UntilClose {
                        head.close = true;
                    }
                    return Ok(head);
                }
                Ok(httparse::Status::Partial) => {
                    if self.buf.len() - self.pos > MAX_RESPONSE_HEAD {
                        return Err(Failure::Transport);
                    }
                    match tokio::time::timeout_at(deadline, self.fill()).await {
                        Err(_) => return Err(Failure::Deadline),
                        Ok(Ok(0)) | Ok(Err(_)) => return Err(Failure::Transport),
                        Ok(Ok(_)) => {}
                    }
                }
                Err(_) => return Err(Failure::Transport),
            }
        }
    }

    /// Reads body bytes into `out`, first from what is already buffered.
    /// Cancel-safe: dropping the future loses no data.
    pub(super) async fn read_body(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.buf.len() {
            let n = out.len().min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        let n = self.io.read(out).await?;
        self.received += n as u64;
        Ok(n)
    }

    /// Reads and discards the body of `head` when it is small and framed by
    /// length, so the connection can be reused; returns whether it can be.
    pub(super) async fn discard(&mut self, head: &ResponseHead, deadline: Instant) -> bool {
        const MAX_DISCARD: u64 = 64 * 1024;
        if head.close {
            return false;
        }
        let length = match head.framing {
            Framing::Empty => return true,
            Framing::UntilClose => return false,
            Framing::Length(length) if length > MAX_DISCARD => return false,
            Framing::Length(length) => length,
        };
        let mut left = length as usize;
        let mut scratch = [0u8; 8 * 1024];
        while left > 0 {
            let want = left.min(scratch.len());
            match tokio::time::timeout_at(deadline, self.read_body(&mut scratch[..want])).await {
                Ok(Ok(n)) if n > 0 => left -= n,
                _ => return false,
            }
        }
        true
    }
}
