//! HTTP test control, deliberately separate from the I/O-free lease library.
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::SigningKey;
use motor_lease::fake::{BrokerReply, BrokerRequest, FakeBroker, FakeConfig};
use motor_lease::{HostHeaders, JwksKey, Placement, RequesterJwk};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    worker_bearer: String,
    subject: String,
    placement: Placement,
    endpoint: String,
    endpoint_spki_sha256: String,
    kid: String,
    signing_seed: String,
    #[serde(default)]
    announced_keys: Vec<JwksKey>,
    registered_hosts: Vec<RegisteredHost>,
}

#[derive(Deserialize)]
struct RegisteredHost {
    host: String,
    public_key: RequesterJwk,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

fn configure(
    args: &[String],
) -> Result<(FakeBroker, std::net::SocketAddr), Box<dyn std::error::Error>> {
    if !args.len().is_multiple_of(2) {
        return Err("usage: motor-fake-broker --runner-instance NAME --config FILE [--listen IP:PORT] [--refuse CODE] [--max-s SECONDS]".into());
    }
    let mut options = BTreeMap::new();
    for pair in args.as_chunks::<2>().0 {
        if ![
            "--runner-instance",
            "--config",
            "--listen",
            "--refuse",
            "--max-s",
        ]
        .contains(&pair[0].as_str())
            || options.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err("unknown or repeated option".into());
        }
    }
    let instance = options
        .get("--runner-instance")
        .copied()
        .ok_or("--runner-instance is required")?;
    if instance == "main" || instance.is_empty() {
        return Err("fake broker is forbidden for the main runner instance".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(
        options.get("--config").ok_or("--config is required")?,
    )?)?;
    let seed: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&config.signing_seed)?
        .try_into()
        .map_err(|_| "signing_seed must be 32 bytes")?;
    let mut broker = FakeBroker::new(
        instance,
        FakeConfig {
            worker_bearer: config.worker_bearer,
            subject: config.subject,
            placement: config.placement,
            endpoint: config.endpoint,
            endpoint_spki_sha256: config.endpoint_spki_sha256,
            kid: config.kid,
            signing_key: SigningKey::from_bytes(&seed),
            announced_keys: config.announced_keys,
        },
    )?;
    for host in config.registered_hosts {
        broker.register_host(host.host, host.public_key.verifying_key()?);
    }
    if let Some(code) = options.get("--refuse") {
        broker.knobs.refusal = Some((403, (*code).into()));
    }
    if let Some(max_s) = options.get("--max-s") {
        let max_s = max_s.parse::<u64>()?;
        if max_s == 0 {
            return Err("--max-s must be positive".into());
        }
        broker.knobs.max_s = Some(max_s);
    }
    let listen = options.get("--listen").copied().unwrap_or("127.0.0.1:0");
    let address: std::net::SocketAddr = listen.parse()?;
    if !address.ip().is_loopback() {
        return Err("fake broker must listen on loopback".into());
    }
    Ok((broker, address))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let (mut broker, address) = configure(&args)?;
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    println!("listening {}", listener.local_addr()?);
    io::stdout().flush()?;
    loop {
        broker.settle_host_lost(now_ms());
        match listener.accept() {
            Ok((stream, _)) => {
                // A client can disappear after acceptance; settlement remains in the model.
                let _ = serve_connection(stream, &mut broker);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn serve_connection(mut stream: TcpStream, broker: &mut FakeBroker) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let reply = read_request(&mut stream, broker)
        .unwrap_or_else(|_| BrokerReply::error(400, "invalid_request"));
    send_reply(&mut stream, reply)
}

fn invalid_request() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP request")
}

fn read_request(stream: &mut TcpStream, broker: &mut FakeBroker) -> io::Result<BrokerReply> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.by_ref().take(16_385).read_line(&mut line)?;
    let mut used = line.len();
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err(invalid_request());
    }
    let (method, path) = (parts[0].to_owned(), parts[1].to_owned());
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader
            .by_ref()
            .take(16_385_u64.saturating_sub(used as u64))
            .read_line(&mut line)?;
        used += line.len();
        if used > 16_384 || line.is_empty() {
            return Err(invalid_request());
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(invalid_request)?;
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err(invalid_request());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(invalid_request());
    }
    let len = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| invalid_request())?
        .unwrap_or(0);
    if len > 1_048_576 {
        return Err(invalid_request());
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    let host_headers = (|| {
        Some(HostHeaders {
            host: headers.get("ck-host")?.clone(),
            timestamp: headers.get("ck-timestamp")?.parse().ok()?,
            counter: headers.get("ck-counter")?.parse().ok()?,
            signature: headers.get("ck-signature")?.clone(),
        })
    })();
    Ok(broker.handle(
        BrokerRequest {
            method: &method,
            path: &path,
            bearer: headers
                .get("authorization")
                .and_then(|v| v.strip_prefix("Bearer ")),
            host_headers: host_headers.as_ref(),
            body: &body,
        },
        now_ms(),
    ))
}

fn send_reply(stream: &mut TcpStream, reply: BrokerReply) -> io::Result<()> {
    let body = serde_json::to_vec(&reply.body)?;
    write!(stream, "HTTP/1.1 {} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.status, body.len())?;
    stream.write_all(&body)
}

#[cfg(test)]
#[path = "../../tests/it/http.rs"]
mod tests;
