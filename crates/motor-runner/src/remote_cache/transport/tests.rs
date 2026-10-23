//! Transport tests: real Unix sockets served by a real gateway against the
//! gateway tests' in-memory store, reached through the real forwarder binary
//! over loopback TCP.
use super::*;
use crate::remote_cache::gateway::tests::{
    generation, stored, FakeConnector, FakeStore, Switchable, BUCKET, TENANT,
};
use crate::remote_cache::gateway::{Endpoint, Limits};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// A gateway on its own multi-threaded runtime, so tests without an async
/// context of their own (the sandbox tests) can serve sockets with it.
pub(crate) struct FakeCache {
    pub(crate) store: Arc<FakeStore>,
    pub(crate) gateway: Gateway,
    runtime: tokio::runtime::Runtime,
}

impl FakeCache {
    pub(crate) fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let store = Arc::new(FakeStore::default());
        let generations = Arc::new(Switchable::default());
        generations.set(Some(generation("1", TENANT)));
        let gateway = Gateway::with_connector(
            Limits::default(),
            generations,
            Arc::new(FakeConnector(store.clone())),
        );
        Self {
            store,
            gateway,
            runtime,
        }
    }

    pub(crate) fn transport(&self, tools: &[(Tool, u16)]) -> Transport {
        Transport::new(
            self.gateway.clone(),
            self.runtime.handle().clone(),
            TENANT,
            tools,
        )
        .unwrap()
    }
}

/// The store path a bare sccache key lands at.
pub(crate) fn stored_sccache(key: &str) -> String {
    stored(Tool::Sccache, key)
}

/// The S3 request path sccache sends for a bare key.
pub(crate) fn s3_path(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

/// A loopback port nothing listens on right now.
pub(crate) fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills a process group when dropped. A started forwarder serves from the
/// process group of whatever started it, and outside a unit's control group
/// nothing else would end it.
pub(crate) struct ProcessGroup(pub(crate) i32);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe { libc::kill(-self.0, libc::SIGKILL) };
    }
}

/// Starts a forwarder from `directory`, so `socket` may be a short relative
/// path whatever the directory's length. Returns its exit status and the
/// group its background copy serves from.
fn start_forwarder(
    binary: &Path,
    directory: &Path,
    port: u16,
    socket: &str,
) -> (std::process::ExitStatus, ProcessGroup) {
    let mut child = Command::new(binary)
        .arg(format!("127.0.0.1:{port}"))
        .arg(socket)
        .current_dir(directory)
        .process_group(0)
        .spawn()
        .unwrap();
    let group = ProcessGroup(child.id() as i32);
    (child.wait().unwrap(), group)
}

/// One HTTP/1.1 exchange on a new loopback connection, closed after the
/// answer: the status and body, or the error that ended it before a status
/// line arrived.
pub(crate) fn http(port: u16, method: &str, path: &str, body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let length = if method == "PUT" {
        format!("content-length: {}\r\n", body.len())
    } else {
        String::new()
    };
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n{length}\r\n")
            .into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    let end = reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no response head"))?;
    let head = String::from_utf8_lossy(&reply[..end]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("bad status line: {head}")))?;
    Ok((status, reply[end + 4..].to_vec()))
}

fn forwarder_binary(state: &Path) -> PathBuf {
    crate::isolation::prepare_cache_forwarder(state).unwrap()
}

#[test]
fn tools_and_ports_must_be_distinct_and_ports_nonzero() {
    let cache = FakeCache::new();
    let make = |tools: &[(Tool, u16)]| {
        Transport::new(
            cache.gateway.clone(),
            cache.runtime.handle().clone(),
            TENANT,
            tools,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    };
    assert_eq!(make(&[(Tool::Sccache, 4300), (Tool::Ccache, 4301)]), Ok(()));
    assert_eq!(
        make(&[(Tool::Sccache, 0)]),
        Err("forwarder port for sccache must not be 0".into())
    );
    assert_eq!(
        make(&[(Tool::Sccache, 4300), (Tool::Sccache, 4301)]),
        Err("sccache is listed twice in the cache transport".into())
    );
    assert_eq!(
        make(&[(Tool::Sccache, 4300), (Tool::Ccache, 4300)]),
        Err("forwarder port 4300 is used by more than one tool".into())
    );
}

#[test]
fn a_fill_puts_through_its_forwarder_and_write_socket() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let scratch = cortexkit_test_support::ScratchDir::new("cache-transport");
    let cache = FakeCache::new();
    let directory = scratch.join("gateway");
    let port = free_port();
    let ccache_port = free_port();
    let sockets = cache
        .transport(&[(Tool::Sccache, port), (Tool::Ccache, ccache_port)])
        .open(&directory, Role::Fill)
        .unwrap();
    assert_eq!(
        sockets.paths(),
        [
            directory.join("sccache.sock"),
            directory.join("ccache.sock")
        ]
    );
    for path in sockets.paths() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600, "{}", path.display());
    }
    assert_eq!(std::fs::metadata(&directory).unwrap().mode() & 0o777, 0o755);
    for listener in sockets.listeners() {
        assert_eq!(listener.binding().endpoint(), Endpoint::Write);
        assert!(listener.binding().ledger().is_some());
    }
    let forwarders = sockets.forwarders();
    assert_eq!(
        forwarders
            .iter()
            .map(|f| (f.tool(), f.port(), f.socket(), f.url()))
            .collect::<Vec<_>>(),
        [
            (
                Tool::Sccache,
                port,
                "/motor-gateway/sccache.sock".to_string(),
                format!("http://127.0.0.1:{port}")
            ),
            (
                Tool::Ccache,
                ccache_port,
                "/motor-gateway/ccache.sock".to_string(),
                format!("http://127.0.0.1:{ccache_port}")
            ),
        ]
    );
    let binary = forwarder_binary(scratch.path());
    let (status, _group) = start_forwarder(&binary, &directory, port, "sccache.sock");
    assert!(status.success());
    // The forwarder's start returned, so its port already accepts.
    assert_eq!(http(port, "PUT", &s3_path("obj"), b"hello").unwrap().0, 200);
    assert_eq!(
        http(port, "GET", &s3_path("obj"), b"").unwrap(),
        (200, b"hello".to_vec())
    );
    let put = cache
        .store
        .seen()
        .into_iter()
        .find(|seen| seen.method == "PUT")
        .unwrap();
    assert_eq!(
        (put.path.as_str(), put.body.as_slice()),
        (stored_sccache("obj").as_str(), &b"hello"[..])
    );
    let ledger = sockets.listeners()[0].binding().ledger().unwrap().summary();
    assert_eq!((ledger.puts_stored, ledger.puts_failed), (1, 0));
    assert_eq!(ledger.key_sample, ["obj"]);
}

#[test]
fn a_jobs_read_socket_serves_gets_and_refuses_puts() {
    let scratch = cortexkit_test_support::ScratchDir::new("cache-transport");
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"cached");
    let directory = scratch.join("gateway");
    let port = free_port();
    let sockets = cache
        .transport(&[(Tool::Sccache, port)])
        .open(&directory, Role::Job("job-a"))
        .unwrap();
    let binding = sockets.listeners()[0].binding();
    assert_eq!(binding.endpoint(), Endpoint::Read);
    assert!(binding.ledger().is_none());
    let binary = forwarder_binary(scratch.path());
    let (status, _group) = start_forwarder(&binary, &directory, port, "sccache.sock");
    assert!(status.success());
    assert_eq!(
        http(port, "GET", &s3_path("k"), b"").unwrap(),
        (200, b"cached".to_vec())
    );
    assert_eq!(http(port, "PUT", &s3_path("k2"), b"x").unwrap().0, 403);
    assert!(cache.store.seen().iter().all(|seen| seen.method != "PUT"));
}

#[test]
fn a_forwarder_whose_socket_is_gone_closes_the_tools_connection() {
    let scratch = cortexkit_test_support::ScratchDir::new("cache-transport");
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"cached");
    let directory = scratch.join("gateway");
    let port = free_port();
    let sockets = cache
        .transport(&[(Tool::Sccache, port)])
        .open(&directory, Role::Job("job-a"))
        .unwrap();
    let binary = forwarder_binary(scratch.path());
    let (status, _group) = start_forwarder(&binary, &directory, port, "sccache.sock");
    assert!(status.success());
    assert_eq!(http(port, "GET", &s3_path("k"), b"").unwrap().0, 200);
    drop(sockets);
    assert!(!directory.join("sccache.sock").exists());
    // The tool gets no answer at all, which it treats as an unreachable
    // cache, and the forwarder keeps running for later connections.
    // The forwarder closes the connection without reading the request, so
    // the close may arrive as a reset.
    let error = http(port, "GET", &s3_path("k"), b"").unwrap_err();
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
        ),
        "{error}"
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
}

#[test]
fn a_forwarder_that_cannot_bind_its_port_exits_nonzero() {
    let scratch = cortexkit_test_support::ScratchDir::new("cache-transport");
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let binary = forwarder_binary(scratch.path());
    let (status, _group) = start_forwarder(&binary, scratch.path(), port, "sccache.sock");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn sockets_bind_under_a_directory_longer_than_a_socket_address() {
    let scratch = cortexkit_test_support::ScratchDir::new("cache-transport");
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"far away");
    let mut parent = scratch.path().to_path_buf();
    while parent.as_os_str().len() < 120 {
        parent.push("a-rather-long-directory-name");
    }
    std::fs::create_dir_all(&parent).unwrap();
    let directory = parent.join("gateway");
    let port = free_port();
    let sockets = cache
        .transport(&[(Tool::Sccache, port)])
        .open(&directory, Role::Job("job-a"))
        .unwrap();
    let path = directory.join("sccache.sock");
    assert!(path.as_os_str().len() > 108);
    assert_eq!(sockets.paths(), [path.clone()]);
    let binary = forwarder_binary(scratch.path());
    let (status, _group) = start_forwarder(&binary, &directory, port, "sccache.sock");
    assert!(status.success());
    assert_eq!(
        http(port, "GET", &s3_path("k"), b"").unwrap(),
        (200, b"far away".to_vec())
    );
    drop(sockets);
    assert!(!path.exists());
}
