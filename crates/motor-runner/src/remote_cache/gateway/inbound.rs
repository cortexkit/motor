//! The client side of a gateway connection: one HTTP/1.1 request at a time
//! from a tool, read with bounded buffers.
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Duration, Instant};

/// Request heads larger than this are refused; a bare key is bounded by
/// `max_key_bytes` anyway.
const MAX_REQUEST_HEAD: usize = 16 * 1024;
const MAX_REQUEST_HEADERS: usize = 64;

/// A parsed request head. Header names are lowercase.
#[derive(Debug)]
pub(super) struct RequestHead {
    pub(super) method: String,
    pub(super) target: String,
    /// The minor HTTP/1 version.
    pub(super) version: u8,
    pub(super) headers: Vec<(String, Vec<u8>)>,
}

impl RequestHead {
    pub(super) fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.headers
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_slice())
    }

    pub(super) fn has(&self, name: &str) -> bool {
        self.values(name).next().is_some()
    }

    /// The client asked to close after this request.
    pub(super) fn close_requested(&self) -> bool {
        self.version == 0
            || self.values("connection").any(|value| {
                value
                    .split(|b| *b == b',')
                    .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"close"))
            })
    }

    /// The request certainly carries no body, so the connection can serve
    /// another request after this one is answered without reading it.
    pub(super) fn body_free(&self) -> bool {
        !self.has("transfer-encoding")
            && self
                .values("content-length")
                .all(|value| value.trim_ascii() == b"0")
    }
}

pub(super) struct Inbound<S> {
    stream: S,
    buf: Vec<u8>,
    pos: usize,
}

/// Why no request head could be read.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HeadError {
    /// The client closed or broke the connection.
    Gone,
    /// The bytes are not an HTTP/1 request head, or it is too large.
    Malformed,
    /// No request began within the idle limit, or a begun head did not
    /// complete within the head limit.
    TimedOut,
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "client write deadline passed")
}

impl<S: AsyncRead + AsyncWrite + Unpin> Inbound<S> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// Reads the next request head. The first byte must arrive within `idle`
    /// of the call, and once one has, the whole head within `head_limit`, so
    /// neither an idle nor a dribbling connection is held open for ever.
    pub(super) async fn read_head(
        &mut self,
        idle: Duration,
        head_limit: Duration,
    ) -> Result<RequestHead, HeadError> {
        let waiting_since = Instant::now();
        // Bytes left over from the previous request already begin this head.
        let mut head_by = (self.pos < self.buf.len()).then(|| waiting_since + head_limit);
        loop {
            if self.pos < self.buf.len() {
                let mut headers = [httparse::EMPTY_HEADER; MAX_REQUEST_HEADERS];
                let mut request = httparse::Request::new(&mut headers);
                match request.parse(&self.buf[self.pos..]) {
                    Ok(httparse::Status::Complete(length)) if length > MAX_REQUEST_HEAD => {
                        return Err(HeadError::Malformed);
                    }
                    Ok(httparse::Status::Complete(length)) => {
                        let head = RequestHead {
                            method: request.method.unwrap_or_default().to_string(),
                            target: request.path.unwrap_or_default().to_string(),
                            version: request.version.unwrap_or(1),
                            headers: request
                                .headers
                                .iter()
                                .map(|header| {
                                    (header.name.to_ascii_lowercase(), header.value.to_vec())
                                })
                                .collect(),
                        };
                        self.pos += length;
                        return Ok(head);
                    }
                    Ok(httparse::Status::Partial) => {
                        if self.buf.len() - self.pos > MAX_REQUEST_HEAD {
                            return Err(HeadError::Malformed);
                        }
                    }
                    Err(_) => return Err(HeadError::Malformed),
                }
            }
            // Drop what earlier requests consumed, so a client that keeps a
            // few bytes of the next request in every read cannot make the
            // buffer grow.
            self.buf.drain(..self.pos);
            self.pos = 0;
            let mut chunk = [0u8; 4 * 1024];
            let by = head_by.unwrap_or(waiting_since + idle);
            match tokio::time::timeout_at(by, self.stream.read(&mut chunk)).await {
                Err(_) => return Err(HeadError::TimedOut),
                Ok(Ok(0)) | Ok(Err(_)) => return Err(HeadError::Gone),
                Ok(Ok(n)) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    head_by.get_or_insert_with(|| Instant::now() + head_limit);
                }
            }
        }
    }

    /// Reads request body bytes, first from what is already buffered.
    pub(super) async fn read_body(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.buf.len() {
            let n = out.len().min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.stream.read(out).await
    }

    /// Bytes held in the read buffer, consumed or not.
    #[cfg(test)]
    pub(super) fn retained(&self) -> usize {
        self.buf.len()
    }

    /// Writes all of `bytes`, failing with `TimedOut` if the client has not
    /// taken them by `by`.
    pub(super) async fn write_all(&mut self, bytes: &[u8], by: Instant) -> io::Result<()> {
        tokio::time::timeout_at(by, async {
            self.stream.write_all(bytes).await?;
            self.stream.flush().await
        })
        .await
        .unwrap_or_else(|_| Err(timed_out()))
    }

    /// Writes a response head. `content_length` is the length of the body
    /// that follows, or for a HEAD request the object's length.
    pub(super) async fn respond(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        content_length: u64,
        close: bool,
        by: Instant,
    ) -> io::Result<()> {
        let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("content-length: {content_length}\r\n"));
        if close {
            head.push_str("connection: close\r\n");
        }
        head.push_str("\r\n");
        self.write_all(head.as_bytes(), by).await
    }

    /// Shuts the write side down, giving up after a second on a client that
    /// does not drain it.
    pub(super) async fn shutdown(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(1), self.stream.shutdown()).await;
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
