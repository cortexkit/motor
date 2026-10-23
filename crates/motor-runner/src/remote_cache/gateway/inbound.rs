//! The client side of a gateway connection: one HTTP/1.1 request at a time
//! from a tool, read with bounded buffers.
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
}

impl<S: AsyncRead + AsyncWrite + Unpin> Inbound<S> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            pos: 0,
        }
    }

    pub(super) async fn read_head(&mut self) -> Result<RequestHead, HeadError> {
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
            if self.pos == self.buf.len() {
                self.buf.clear();
                self.pos = 0;
            }
            let mut chunk = [0u8; 4 * 1024];
            match self.stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(HeadError::Gone),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
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

    pub(super) async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await
    }

    /// Writes a response head. `content_length` is the length of the body
    /// that follows, or for a HEAD request the object's length.
    pub(super) async fn respond(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        content_length: u64,
        close: bool,
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
        self.write_all(head.as_bytes()).await
    }

    pub(super) async fn shutdown(&mut self) {
        let _ = self.stream.shutdown().await;
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
