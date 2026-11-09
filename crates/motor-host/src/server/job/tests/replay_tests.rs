use super::*;

fn bytes(text: &[u8]) -> Vec<u8> {
    [(text.len() as u32).to_be_bytes().as_slice(), text].concat()
}
fn records() -> Vec<Vec<u8>> {
    vec![
        bytes(format!(r#"{{ "type":"started", "seq":0,"job_id":"{JOB}" }}"#).as_bytes()),
        bytes(br#"{ "type":"output", "seq":1, "stream":"stdout", "bytes":"aGVsbG8=" }"#),
        bytes(br#"{ "type":"return_result", "seq":2, "files":[], "extension":"raw" }"#),
        bytes(br#"{ "type":"return_chunk", "seq":3, "bytes":"YWJj", "extension":"raw" }"#),
        bytes(br#"{ "type":"return_manifest", "seq":4, "files":[] }"#),
        bytes(format!(r#"{{ "type":"terminal","job_id":"{JOB}","outcome":{{"type":"exit","code":0}},"wall_ms":100,"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null }}"#).as_bytes()),
    ]
}
async fn next_record(client: &mut tokio::io::DuplexStream) -> Raw {
    loop {
        let frame = raw(client).await.unwrap();
        if frame.value["type"] != "heartbeat" {
            return frame;
        }
    }
}
fn peer(fake: &Fake) -> tokio::net::UnixStream {
    let (host, guest) = UnixStream::pair().unwrap();
    fake.scripted.lock().unwrap().push_back(host);
    guest.set_nonblocking(true).unwrap();
    tokio::net::UnixStream::from_std(guest).unwrap()
}
async fn handshake(guest: &mut tokio::net::UnixStream) {
    assert_eq!(read_frame(guest).await.unwrap()["type"], "verified_lease");
    assert_eq!(read_frame(guest).await.unwrap()["type"], "hello");
    write_frame(guest, &hello()).await.unwrap();
}
async fn launched(
    runtime: &Arc<Runtime>,
    fake: &Fake,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Result<()>>,
    tokio::net::UnixStream,
) {
    let mut guest = peer(fake);
    let c = claims(JOB);
    let id = claim(runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    let mut request = run(JOB);
    request["bundles"] = json!([]);
    write_frame(&mut client, &request).await.unwrap();
    next_type(&mut client, "accepted").await;
    handshake(&mut guest).await;
    assert_eq!(read_frame(&mut guest).await.unwrap()["type"], "run");
    write_frame(&mut guest, &json!({"type":"accepted","job_id":JOB}))
        .await
        .unwrap();
    (client, task, guest)
}
async fn ack(runtime: &Arc<Runtime>, guest: &mut tokio::net::UnixStream) {
    assert_eq!(
        read_frame(guest).await.unwrap(),
        json!({"type":"terminal_stored","jti":JOB})
    );
    let store = runtime.job_stream(JOB).unwrap();
    assert!(store.with_store(|s| Ok(s.ended())).unwrap());
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.leases[JOB].highest_seq,
        4
    );
}
fn power_off(runtime: &Arc<Runtime>, fake: &Fake) {
    // The fake virtual machine monitor may exit only after the test observed
    // terminal_stored, which confirms that the terminal frame is durable.
    fake.gone.lock().unwrap().insert(JOB.into());
    fake.exits
        .lock()
        .unwrap()
        .push(motor_vm_helper::job::ExitReport {
            job_id: JOB.into(),
            oom_kill: 0,
            egress_bytes: Some(0),
        });
    runtime.collect_job_exits(unix_ms()).unwrap();
    runtime
        .host
        .lock()
        .unwrap()
        .billing_tick(unix_ms() + 600_000)
        .unwrap();
}
fn reopen(runtime: &Arc<Runtime>, fake: Arc<Fake>, storage: Arc<Storage>) -> Arc<Runtime> {
    let (journal, snapshot) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = runtime.host.lock().unwrap().settings.clone();
    Runtime::with_job_resources("host".into(), host, fake, storage)
}
async fn replayed(runtime: &Arc<Runtime>) -> Vec<Vec<u8>> {
    let mut c = claims(JOB);
    c.purpose = Purpose::Attach;
    let id = runtime
        .host
        .lock()
        .unwrap()
        .claim(c.clone(), unix_ms())
        .unwrap()
        .0;
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    write_frame(
        &mut client,
        &json!({"type":"attach","job_id":JOB,"from_seq":0}),
    )
    .await
    .unwrap();
    let mut result = Vec::new();
    loop {
        let frame = raw(&mut client).await.unwrap();
        let terminal = frame.value["type"] == "terminal";
        result.push(frame.bytes);
        if terminal {
            break;
        }
    }
    drop(client);
    task.await.unwrap().unwrap();
    runtime.host.lock().unwrap().close(id).unwrap();
    result
}
fn usage(runtime: &Arc<Runtime>) -> Vec<u8> {
    serde_json::to_vec(
        runtime.host.lock().unwrap().snapshot.billing[JOB]
            .usage
            .as_ref()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn detached_caller_retains_raw_output_returns_terminal_and_one_usage_across_poweroff_and_restart(
) {
    let (runtime, fake, storage) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    let frames = records();
    for frame in &frames[..4] {
        guest.write_all(frame).await.unwrap();
        assert_eq!(next_record(&mut client).await.bytes, *frame);
    }
    drop(client);
    task.await.unwrap().unwrap();
    for frame in &frames[4..] {
        guest.write_all(frame).await.unwrap();
    }
    ack(&runtime, &mut guest).await;
    power_off(&runtime, &fake);
    let body = usage(&runtime);
    let outbox = runtime.host.lock().unwrap().snapshot.outbox.clone();
    assert_eq!(outbox.len(), 1);
    assert_eq!(replayed(&runtime).await, frames);
    assert_eq!(usage(&runtime), body);
    assert_eq!(runtime.host.lock().unwrap().snapshot.outbox, outbox);
    let restarted = reopen(&runtime, fake.clone(), storage.clone());
    restarted.recover().unwrap();
    assert_eq!(replayed(&restarted).await, frames);
    assert_eq!(usage(&restarted), body);
    assert_eq!(restarted.host.lock().unwrap().snapshot.outbox, outbox);
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("start:"))
            .count(),
        1
    );
    assert!(fake.scripted.lock().unwrap().is_empty());
    let index_path = storage
        .root
        .path()
        .join("replay")
        .join(JOB)
        .join("index.json");
    let mut index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    index["version"] = json!(999);
    std::fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
    let quarantined = reopen(&restarted, fake.clone(), storage);
    quarantined.recover().unwrap();
    let frames = replayed(&quarantined).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        crate::replay::decode(&frames[0]).unwrap()["outcome"]["type"],
        "outcome_unknown"
    );
    assert_eq!(usage(&quarantined), body);
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("start:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn host_down_while_command_finishes_resumes_last_sequence_and_drops_duplicate_output() {
    let (runtime, fake, storage) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    let frames = records();
    for frame in &frames[..2] {
        guest.write_all(frame).await.unwrap();
        next_record(&mut client).await;
    }
    drop(client);
    task.await.unwrap().unwrap();
    let stream = runtime.job_stream(JOB).unwrap();
    stream.crash_for_test();
    tokio::time::timeout(Duration::from_secs(5), async {
        while stream.running_for_test() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("stop:")));
    drop(guest);
    // The guest has finished, but holds its terminal and VM until the resumed
    // connection acknowledges storage. The old host worker is no longer alive.
    let mut resumed = peer(&fake);
    let restarted = reopen(&runtime, fake.clone(), storage);
    restarted.recover().unwrap();
    handshake(&mut resumed).await;
    assert_eq!(
        read_frame(&mut resumed).await.unwrap(),
        json!({"type":"attach","job_id":JOB,"from_seq":1})
    );
    for frame in &frames[1..] {
        resumed.write_all(frame).await.unwrap();
    }
    ack(&restarted, &mut resumed).await;
    power_off(&restarted, &fake);
    assert_eq!(replayed(&restarted).await, frames);
    let body = usage(&restarted);
    assert_eq!(replayed(&restarted).await, frames);
    assert_eq!(usage(&restarted), body);
    assert_eq!(restarted.host.lock().unwrap().snapshot.outbox.len(), 1);
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("start:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn live_controls_use_the_single_runner_connection_and_ended_controls_use_the_store() {
    let (runtime, fake, _) = fixture();
    let (client, task, mut guest) = launched(&runtime, &fake).await;
    drop(client);
    task.await.unwrap().unwrap();
    let mut c = claims(JOB);
    c.purpose = Purpose::Attach;
    let id = runtime
        .host
        .lock()
        .unwrap()
        .claim(c.clone(), unix_ms())
        .unwrap()
        .0;
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    for (kind, response) in [
        (
            "job_status",
            json!({"type":"job_status_reply","job_id":JOB,"state":"running"}),
        ),
        ("cancel", json!({"type":"cancel_reply","job_id":JOB})),
        (
            "withdraw",
            json!({"type":"withdraw_reply","outcome":"started"}),
        ),
    ] {
        let request = json!({"type":kind,"job_id":JOB});
        write_frame(&mut client, &request).await.unwrap();
        assert_eq!(read_frame(&mut guest).await.unwrap(), request);
        write_frame(&mut guest, &response).await.unwrap();
        assert_eq!(read_frame(&mut client).await.unwrap(), response);
    }
    for frame in records() {
        guest.write_all(&frame).await.unwrap();
    }
    ack(&runtime, &mut guest).await;
    for (kind, response) in [
        ("cancel", "cancel_reply"),
        ("withdraw", "withdraw_reply"),
        ("job_status", "job_status_reply"),
    ] {
        write_frame(&mut client, &json!({"type":kind,"job_id":JOB}))
            .await
            .unwrap();
        let reply = read_frame(&mut client).await.unwrap();
        assert_eq!(reply["type"], response);
        if kind == "job_status" {
            assert_eq!(reply["state"], "ended");
        }
        if kind == "withdraw" {
            assert_eq!(reply["outcome"], "started");
        }
    }
    drop(client);
    task.await.unwrap().unwrap();
    power_off(&runtime, &fake);
}

#[tokio::test]
async fn failed_guest_channel_stops_after_teardown_timeout_and_replays_lost_settlement() {
    let (runtime, fake, _) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    for frame in &records()[..2] {
        guest.write_all(frame).await.unwrap();
        next_record(&mut client).await;
    }
    let began = std::time::Instant::now();
    drop(guest);
    let frames = tokio::time::timeout(Duration::from_secs(5), async {
        let mut frames = Vec::new();
        loop {
            let frame = read_frame(&mut client).await.unwrap();
            let ended = frame["type"] == "terminal";
            frames.push(frame);
            if ended {
                break;
            }
        }
        frames
    })
    .await
    .unwrap();
    assert!(began.elapsed() >= Duration::from_millis(1000));
    assert_eq!(frames.last().unwrap()["outcome"]["type"], "outcome_unknown");
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.billing[JOB]
            .usage
            .as_ref()
            .unwrap()
            .outcome,
        motor_lease::UsageOutcome::Lost
    );
    assert!(fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c == &format!("stop:{JOB}")));
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn authoritative_terminals_are_byte_identical_live_and_on_replay_after_restart() {
    for host_stop in [false, true] {
        let (runtime, fake, storage) = fixture();
        let (mut client, task, mut guest) = launched(&runtime, &fake).await;
        let frames = records();
        for frame in &frames[..5] {
            guest.write_all(frame).await.unwrap();
            assert_eq!(next_record(&mut client).await.bytes, *frame);
        }
        if host_stop {
            runtime
                .host
                .lock()
                .unwrap()
                .job_host_stop(
                    JOB,
                    crate::state::settlement::StopCause::HostPressure,
                    unix_ms(),
                )
                .unwrap();
        }
        let mut forged = crate::replay::decode(frames.last().unwrap()).unwrap();
        forged["oom_kill"] = json!({"cause":"host_pressure"});
        let raw_terminal = bytes(&serde_json::to_vec(&forged).unwrap());
        guest.write_all(&raw_terminal).await.unwrap();
        let delivered = next_record(&mut client).await;
        assert_eq!(delivered.value["type"], "terminal");
        if host_stop {
            assert_eq!(delivered.value["oom_kill"]["cause"], "host_pressure");
            assert_eq!(delivered.value["outcome"]["type"], "signal");
        } else {
            assert!(delivered.value.get("oom_kill").is_none());
            assert_eq!(delivered.value["outcome"]["type"], "exit");
        }
        ack(&runtime, &mut guest).await;
        let stored = runtime.job_stream(JOB).unwrap();
        assert_eq!(
            stored.with_store(|s| s.raw_frame(s.len() - 1)).unwrap(),
            Some(raw_terminal)
        );
        drop(client);
        task.await.unwrap().unwrap();
        power_off(&runtime, &fake);
        let body = usage(&runtime);
        assert_eq!(replayed(&runtime).await.last().unwrap(), &delivered.bytes);
        let restarted = reopen(&runtime, fake.clone(), storage);
        restarted.recover().unwrap();
        assert_eq!(replayed(&restarted).await.last().unwrap(), &delivered.bytes);
        assert_eq!(usage(&restarted), body);
    }
}

#[tokio::test]
async fn concurrent_controls_receive_only_their_own_runner_reply() {
    let (runtime, fake, _) = fixture();
    let (client, task, mut guest) = launched(&runtime, &fake).await;
    let mut clients = vec![client];
    let mut tasks = vec![task];
    {
        let mut c = claims(JOB);
        c.purpose = Purpose::Attach;
        let id = runtime
            .host
            .lock()
            .unwrap()
            .claim(c.clone(), unix_ms())
            .unwrap()
            .0;
        let (mut client, task) = start(runtime.clone(), c, id).await;
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        clients.push(client);
        tasks.push(task);
    }
    for client in &mut clients {
        write_frame(client, &json!({"type":"job_status","job_id":JOB}))
            .await
            .unwrap();
        assert_eq!(read_frame(&mut guest).await.unwrap()["type"], "job_status");
    }
    for ordinal in [1, 2] {
        write_frame(
            &mut guest,
            &json!({"type":"job_status_reply","job_id":JOB,"state":"running","ordinal":ordinal}),
        )
        .await
        .unwrap();
    }
    for (index, mut client) in clients.into_iter().enumerate() {
        assert_eq!(next_record(&mut client).await.value["ordinal"], index + 1);
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    runtime.stop_job(JOB, true, false).unwrap();
}
