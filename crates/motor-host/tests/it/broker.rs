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
