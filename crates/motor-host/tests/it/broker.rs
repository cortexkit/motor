use super::billing::{end, started, terminal};
use super::*;
use anyhow::{bail, Result};
use motor_host::{broker::Sender, helper::Control, server::Runtime};
use motor_lease::{HostAuthenticator, HostHeaders};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};

struct NoVm;
impl Control for NoVm {
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        bail!("unexpected wake")
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, _: u32) -> Result<std::os::unix::net::UnixStream> {
        bail!("unexpected relay")
    }
    fn admin(&self, _: &str, _: &Value) -> Result<Value> {
        bail!("unexpected admin")
    }
    fn suspend(&self, _: &str) -> Result<()> {
        bail!("unexpected suspend")
    }
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
}

#[test]
fn signed_sender_heartbeats_at_fifteen_seconds_and_on_change_and_resigns_unchanged_usage_after_lost_reply(
) {
    let ms = NOW * 1000;
    let runtime = Runtime::new("host".into(), host(), Arc::new(NoVm));
    runtime.host.lock().unwrap().start_billing(ms).unwrap();
    let key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
    let public = key.verifying_key();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let host = runtime.clone();
    let worker = std::thread::spawn(move || {
        let mut auth = HostAuthenticator::default();
        auth.register("host".into(), public);
        let mut usage_body = None;
        let mut usage_counter = 0;
        let mut usage_timestamp = 0;
        let mut heartbeat_counters = Vec::new();
        for index in 0..6 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap().to_owned();
            let mut headers = BTreeMap::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                let (key, value) = line.trim().split_once(':').unwrap();
                headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
            }
            let mut body = vec![0; headers["content-length"].parse::<usize>().unwrap()];
            reader.read_exact(&mut body).unwrap();
            let signed = HostHeaders {
                host: headers["ck-host"].clone(),
                timestamp: headers["ck-timestamp"].parse().unwrap(),
                counter: headers["ck-counter"].parse().unwrap(),
                signature: headers["ck-signature"].clone(),
            };
            auth.verify("POST", &path, &body, &signed, signed.timestamp)
                .unwrap();
            // A broker response may wait on network I/O without locking host state.
            assert!(host.host.try_lock().is_ok());
            let usage = path.ends_with("/usage");
            assert_eq!(usage, matches!(index, 3 | 4));
            if index == 3 {
                usage_body = Some(body);
                usage_counter = signed.counter;
                usage_timestamp = signed.timestamp;
                // The broker accepted the body, but its response is truncated.
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 50\r\nConnection: close\r\n\r\n{{"
                )
                .unwrap();
                continue;
            }
            if index == 4 {
                assert_eq!(usage_body.as_ref().unwrap(), &body);
                assert!(signed.counter > usage_counter);
                assert!(signed.timestamp > usage_timestamp + 60_000);
            } else {
                heartbeat_counters.push(signed.counter);
                let hb: motor_lease::Heartbeat = serde_json::from_slice(&body).unwrap();
                if index == 2 {
                    assert_eq!(
                        (hb.running.len(), hb.running[0].state),
                        (1, motor_lease::JobState::Started)
                    );
                }
                if index == 5 {
                    assert!(hb.running.is_empty());
                    assert!(signed.counter > usage_counter);
                }
            }
            let reply = if usage {
                json!({"job_id":JOB,"settled_vcpu_ms":18000,"released_vcpu_ms":12000})
            } else {
                json!({"ignored":[]})
            }
            .to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",reply.len(),reply).unwrap();
        }
        assert_eq!(heartbeat_counters.len(), 4);
    });
    let mut sender = Sender::new(runtime.clone(), key).unwrap();
    sender.test_endpoint(endpoint);
    sender.step(ms).unwrap();
    sender.step(ms + 14_999).unwrap();
    sender.step(ms + 15_000).unwrap();
    started(&mut runtime.host.lock().unwrap(), JOB, 1);
    sender.step(ms + 15_001).unwrap();
    {
        let mut h = runtime.host.lock().unwrap();
        end(&mut h, JOB, 3, ms + 18_000, terminal(JOB, "exit"));
        h.terminal_delivered(JOB, ms + 18_000).unwrap();
    }
    sender.step(ms + 18_000).unwrap();
    sender.step(ms + 90_000).unwrap();
    sender.step(ms + 90_001).unwrap();
    worker.join().unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.outbox.is_empty());
}

#[test]
fn heartbeat_publication_continues_while_a_broker_response_is_blocked() {
    use motor_host::broker::Heartbeats;
    let ms = NOW * 1000;
    let runtime = Runtime::new("host".into(), host(), Arc::new(NoVm));
    runtime.host.lock().unwrap().start_billing(ms).unwrap();
    let mut publisher = Heartbeats::new(runtime.clone());
    publisher.step(ms).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let broker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        reader.read_exact(&mut vec![0; length]).unwrap();
        ready_tx.send(()).unwrap();
        release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let body = "{\"ignored\":[]}";
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let host = runtime.clone();
    let sender = std::thread::spawn(move || {
        let mut sender =
            Sender::new(host, ed25519_dalek::SigningKey::from_bytes(&[8; 32])).unwrap();
        sender.test_endpoint(endpoint);
        sender.send_next(ms).unwrap();
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    publisher.step(ms + 15_000).unwrap();
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.last_alive,
        ms + 15_000
    );
    release_tx.send(()).unwrap();
    broker.join().unwrap();
    sender.join().unwrap();
}

/// One broker request: its route and the CK-Timestamp it was signed at.
fn read_request(stream: &std::net::TcpStream) -> (String, u64) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let path = line.split_whitespace().nth(1).unwrap().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.trim().split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    let mut body = vec![0; headers["content-length"].parse::<usize>().unwrap()];
    reader.read_exact(&mut body).unwrap();
    (path, headers["ck-timestamp"].parse().unwrap())
}

/// The sender used to retry a heartbeat the broker refused with 400 about
/// once a second, forever, and a usage report queued behind it never went out.
/// A 400 now sets the body aside, the next attempt waits at least the
/// heartbeat interval, and the usage report is sent next.
#[test]
fn a_refused_heartbeat_is_set_aside_and_never_holds_back_the_usage_report() {
    let ms = NOW * 1000;
    let runtime = Runtime::new("host".into(), host(), Arc::new(NoVm));
    runtime.host.lock().unwrap().start_billing(ms).unwrap();
    started(&mut runtime.host.lock().unwrap(), JOB, 1);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stop.clone();
    let broker = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            let (path, timestamp) = read_request(&stream);
            let (status, reply) = if path.ends_with("/usage") {
                (
                    "200 OK",
                    json!({"job_id":JOB,"settled_vcpu_ms":0,"released_vcpu_ms":0}),
                )
            } else {
                ("400 Bad Request", json!({"error":"bad_request"}))
            };
            let reply = reply.to_string();
            write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).unwrap();
            seen.push((path, timestamp));
        }
        seen
    });
    let mut sender = Sender::new(
        runtime.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[9; 32]),
    )
    .unwrap();
    sender.test_endpoint(endpoint);
    sender.step(ms).unwrap();
    {
        let mut h = runtime.host.lock().unwrap();
        end(&mut h, JOB, 3, ms + 1_000, terminal(JOB, "exit"));
        h.terminal_delivered(JOB, ms + 1_000).unwrap();
    }
    // The sender loop runs every 250 ms.
    for now in (ms + 1_000..=ms + 20_000).step_by(250) {
        sender.step(now).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen = broker.join().unwrap();
    let routes: Vec<_> = seen
        .iter()
        .map(|(path, _)| path.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(routes, ["heartbeat", "usage", "heartbeat"], "{seen:?}");
    // Nothing goes out again until the heartbeat interval has passed.
    assert!(seen[1].1 >= ms + 15_000, "{seen:?}");
    let h = runtime.host.lock().unwrap();
    // Only a newer heartbeat, queued at its interval, may still be waiting.
    assert!(
        h.snapshot.outbox.iter().all(|r| r.job.is_none()),
        "{:?}",
        h.snapshot.outbox
    );
    assert_eq!(h.snapshot.rejected_outbox.len(), 2);
    assert!(h
        .snapshot
        .rejected_outbox
        .iter()
        .all(|r| r.path.ends_with("/heartbeat") && r.job.is_none()));
}

/// A body that breaks the contract grammar is never signed or sent: the
/// sender sets it aside and sends what follows it.
#[test]
fn an_outbox_entry_breaking_the_contract_grammar_is_set_aside_unsent() {
    let ms = NOW * 1000;
    let runtime = Runtime::new("host".into(), host(), Arc::new(NoVm));
    let invalid = motor_host::billing::Request {
        path: "/v1/hosts/host/heartbeat".into(),
        body: br#"{"free_vcpus":8,"free_mem_mib":87846,"queued":0,"warm":[{"tenant":"tenant-1","workspace_key_hash":"","vm":"vm-1"}],"running":[],"healthy":true}"#.to_vec(),
        created_ms: ms,
        job: None,
    };
    {
        let mut h = runtime.host.lock().unwrap();
        let mut next = h.snapshot.clone();
        next.outbox.push(invalid.clone());
        h.save(next).unwrap();
    }
    // No broker listens here: a send attempt would fail with a transport error.
    let mut sender = Sender::new(
        runtime.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[9; 32]),
    )
    .unwrap();
    sender.test_endpoint("http://127.0.0.1:9".into());
    sender.send_next(ms).unwrap();
    let h = runtime.host.lock().unwrap();
    assert!(h.snapshot.outbox.is_empty());
    assert_eq!(h.snapshot.rejected_outbox.len(), 1);
    assert_eq!(h.snapshot.rejected_outbox[0].body, invalid.body);
}

/// The broker lists closed accounts in its heartbeat reply. Malformed replies
/// change nothing and fail no heartbeat; a well-formed one closes only the
/// tenant bound to a listed subject.
#[test]
fn heartbeat_replies_close_bound_accounts_and_malformed_replies_are_harmless() {
    let ms = NOW * 1000;
    let runtime = Runtime::new("host".into(), host(), Arc::new(NoVm));
    runtime.host.lock().unwrap().start_billing(ms).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let sub = claims(JOB).sub;
    let replies = [
        "not json".to_owned(),
        json!({"closed_subjects": 5}).to_string(),
        json!({"closed_subjects": [{"tenant": "tenant"}, "tenant"]}).to_string(),
        json!({"closed_subjects": ["acct:01BX5ZZKBKACTAV9WEVGEMMVRZ", sub]}).to_string(),
    ];
    let broker = std::thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let (path, _) = read_request(&stream);
            assert!(path.ends_with("/heartbeat"), "{path}");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
        }
    });
    let mut sender = Sender::new(
        runtime.clone(),
        ed25519_dalek::SigningKey::from_bytes(&[9; 32]),
    )
    .unwrap();
    sender.test_endpoint(endpoint);
    for n in 0..4 {
        sender.step(ms + n * 15_000).unwrap();
        let h = runtime.host.lock().unwrap();
        assert!(h.snapshot.outbox.is_empty(), "heartbeat {n} not confirmed");
        assert!(h.snapshot.rejected_outbox.is_empty());
        assert_eq!(h.is_closed("tenant"), n == 3, "after heartbeat {n}");
    }
    broker.join().unwrap();
    let h = runtime.host.lock().unwrap();
    assert_eq!(h.snapshot.closed.len(), 1);
    assert_eq!(h.snapshot.closed["tenant"].sub, sub);
}
