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
        bytes(format!(r#"{{ "type":"terminal","job_id":"{JOB}","outcome":{{"type":"exit","code":0}},"wall_ms":100,"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null,"last_seq":4 }}"#).as_bytes()),
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
/// The next frame the host sends the guest, skipping the host's keep-alive
/// heartbeats as the guest runner does.
async fn guest_frame(guest: &mut tokio::net::UnixStream) -> Value {
    loop {
        let frame = read_frame(guest).await.unwrap();
        if frame["type"] != "heartbeat" {
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
    launched_with_returns(runtime, fake, true).await
}
async fn launched_with_returns(
    runtime: &Arc<Runtime>,
    fake: &Fake,
    returns: bool,
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
    if returns {
        request["return_paths"] = json!(["/out"]);
        request["return_chunked"] = json!(true);
    }
    write_frame(&mut client, &request).await.unwrap();
    next_type(&mut client, "accepted").await;
    handshake(&mut guest).await;
    assert_eq!(guest_frame(&mut guest).await["type"], "run");
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].replay_reservation_bytes,
        Some(crate::state::jobs::storage::REPLAY_RESERVATION_BYTES)
    );
    write_frame(&mut guest, &json!({"type":"accepted","job_id":JOB}))
        .await
        .unwrap();
    (client, task, guest)
}

async fn observer(
    runtime: &Arc<Runtime>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Result<()>>,
    u64,
) {
    let mut c = claims(JOB);
    c.purpose = Purpose::Attach;
    let id = claim(runtime, &c);
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
    (client, task, id)
}

async fn runner_heartbeat(client: &mut tokio::io::DuplexStream, wait: u64) -> Raw {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = raw(client).await.unwrap();
            assert_eq!(frame.value["type"], "heartbeat");
            if frame.value["queue_wait_ms"] == wait {
                return frame;
            }
        }
    })
    .await
    .expect("runner queue heartbeat did not reach the caller before any sequenced record")
}

#[tokio::test]
async fn runner_queue_heartbeat_reaches_caller_before_records_without_entering_replay() {
    let (runtime, fake, _) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    let beat = bytes(br#"{ "type":"heartbeat", "queue_wait_ms":12345, "extension":"raw" }"#);
    guest.write_all(&beat).await.unwrap();
    assert_eq!(runner_heartbeat(&mut client, 12345).await.bytes, beat);
    let stream = runtime.job_stream(JOB).unwrap();
    assert_eq!(stream.with_store(|s| Ok(s.len())).unwrap(), 0);
    assert_eq!(stream.with_store(|s| Ok(s.last_seq())).unwrap(), None);

    for frame in records() {
        guest.write_all(&frame).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        for frame in records() {
            assert_eq!(next_record(&mut client).await.bytes, frame);
        }
    })
    .await
    .expect("sequenced replay records did not follow the live heartbeat");
    ack(&runtime, &mut guest).await;
    assert_eq!(stream.with_store(|s| Ok(s.len())).unwrap(), records().len());
    task.await.unwrap().unwrap();
    power_off(&runtime, &fake);
    assert_eq!(replayed(&runtime).await, records());
}

#[tokio::test]
async fn reattach_waits_for_a_new_runner_heartbeat_instead_of_replaying_the_old_one() {
    let (runtime, fake, _) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    write_frame(
        &mut guest,
        &json!({"type":"heartbeat","queue_wait_ms":12345}),
    )
    .await
    .unwrap();
    runner_heartbeat(&mut client, 12345).await;
    drop(client);
    task.await.unwrap().unwrap();
    let (mut client, task, id) = observer(&runtime).await;
    let beat = tokio::time::timeout(
        super::super::stream::CLIENT_HEARTBEAT + Duration::from_secs(3),
        read_frame(&mut client),
    )
    .await
    .expect("quiet attachment did not receive a keepalive")
    .unwrap();
    assert_eq!(beat, json!({"type":"heartbeat","queue_wait_ms":0}));
    write_frame(
        &mut guest,
        &json!({"type":"heartbeat","queue_wait_ms":54321}),
    )
    .await
    .unwrap();
    runner_heartbeat(&mut client, 54321).await;
    drop(client);
    task.await.unwrap().unwrap();
    runtime.host.lock().unwrap().close(id).unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn quiet_prestart_attachment_repeats_only_its_live_runner_queue_sample() {
    let (runtime, fake, _) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    let beat = bytes(br#"{ "type":"heartbeat", "queue_wait_ms":12345 }"#);
    guest.write_all(&beat).await.unwrap();
    runner_heartbeat(&mut client, 12345).await;
    let repeated = tokio::time::timeout(
        super::super::stream::CLIENT_HEARTBEAT + Duration::from_secs(3),
        raw(&mut client),
    )
    .await
    .expect("quiet preparation did not receive a keepalive")
    .unwrap();
    assert_eq!(repeated.bytes, beat);
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn busy_disk_observer_keepalive_pauses_instead_of_clearing_the_queue_budget() {
    let (runtime, _, _) = fixture();
    claim(&runtime, &claims(JOB));
    {
        let mut host = runtime.host.lock().unwrap();
        let disk = host.snapshot.per_job.executions[JOB].disk_id.clone();
        host.snapshot
            .per_job
            .disks
            .get_mut(&disk)
            .unwrap()
            .attached_job = Some("holder".into());
    }
    let (mut client, task, id) = observer(&runtime).await;
    let beat = tokio::time::timeout(
        super::super::stream::CLIENT_HEARTBEAT + Duration::from_secs(3),
        read_frame(&mut client),
    )
    .await
    .expect("busy-disk observer did not receive a keepalive")
    .unwrap();
    assert_eq!(beat, json!({"type":"heartbeat","queue_wait_ms":0}));
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].boot_started);
    drop(client);
    task.await.unwrap().unwrap();
    runtime.host.lock().unwrap().close(id).unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}
async fn ack(runtime: &Arc<Runtime>, guest: &mut tokio::net::UnixStream) {
    ack_at(runtime, guest, 4).await;
}
async fn ack_at(runtime: &Arc<Runtime>, guest: &mut tokio::net::UnixStream, highest_seq: u64) {
    assert_eq!(
        read_frame(guest).await.unwrap(),
        json!({"type":"terminal_stored","jti":JOB})
    );
    let store = runtime.job_stream(JOB).unwrap();
    assert!(store.with_store(|s| Ok(s.ended())).unwrap());
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].replay_completion,
        Some(crate::state::jobs::storage::ReplayCompletion::Terminal)
    );
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.leases[JOB].highest_seq,
        highest_seq
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
    replayed_at(runtime, unix_ms()).await
}
async fn replayed_at(runtime: &Arc<Runtime>, now_ms: u64) -> Vec<Vec<u8>> {
    let mut c = claims(JOB);
    c.purpose = Purpose::Attach;
    c.exp = now_ms / 1000 + 300;
    let id = runtime
        .host
        .lock()
        .unwrap()
        .claim(c.clone(), now_ms)
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

#[test]
fn restart_releases_final_record_hold_when_terminal_already_committed() {
    use crate::state::jobs::storage::{ReplayCompletion, REPLAY_RECORD_BYTES};
    let (runtime, fake, storage) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    runtime
        .host
        .lock()
        .unwrap()
        .job_stopped(JOB, false)
        .unwrap();
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
            .final_replay_reservation_bytes,
        Some(REPLAY_RECORD_BYTES)
    );
    // Reproduce a crash after the file commit but before the host state save.
    let mut store = crate::replay::Store::open(storage.root.path(), JOB).unwrap();
    store.append(records().last().unwrap()).unwrap();
    drop(store);
    let restarted = reopen(&runtime, fake, storage.clone());
    restarted.recover().unwrap();
    let execution = &restarted.host.lock().unwrap().snapshot.per_job.executions[JOB];
    assert_eq!(execution.final_replay_reservation_bytes, None);
    assert_eq!(
        execution.replay_completion,
        Some(ReplayCompletion::Terminal)
    );
    let (_, saved) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
    assert_eq!(
        saved.per_job.executions[JOB].final_replay_reservation_bytes,
        None
    );
}

#[test]
fn replay_quarantine_releases_final_hold_and_survives_restart() {
    use crate::state::jobs::storage::{ReplayCompletion, REPLAY_RECORD_BYTES};
    let (runtime, fake, storage) = fixture();
    claim(&runtime, &claims(JOB));
    runtime
        .host
        .lock()
        .unwrap()
        .job_stopped(JOB, false)
        .unwrap();
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
            .final_replay_reservation_bytes,
        Some(REPLAY_RECORD_BYTES)
    );
    let mut store = crate::replay::Store::open(storage.root.path(), JOB).unwrap();
    store.append(&records()[1]).unwrap();
    drop(store);
    let index = storage
        .root
        .path()
        .join("replay")
        .join(JOB)
        .join("index.json");
    std::fs::write(&index, b"damaged index").unwrap();
    let restarted = reopen(&runtime, fake.clone(), storage.clone());
    restarted.recover().unwrap();
    assert_eq!(
        restarted.host.lock().unwrap().snapshot.per_job.executions[JOB]
            .final_replay_reservation_bytes,
        None
    );
    assert_eq!(
        restarted.host.lock().unwrap().snapshot.per_job.executions[JOB].replay_completion,
        Some(ReplayCompletion::Quarantined)
    );
    // Replacing the damaged file cannot resurrect a quarantined writer.
    std::fs::write(
        index,
        br#"{"version":1,"entries":[],"last_seq":null,"terminal":false}"#,
    )
    .unwrap();
    let again = reopen(&restarted, fake, storage);
    assert!(again
        .job_stream(JOB)
        .unwrap()
        .with_store(|_| Ok(()))
        .is_err());
    assert_eq!(
        again.host.lock().unwrap().snapshot.per_job.executions[JOB].final_replay_reservation_bytes,
        None
    );
    // Chunk expiry past retention skips a quarantined replay instead of
    // failing every host tick.
    again
        .host
        .lock()
        .unwrap()
        .snapshot
        .leases
        .get_mut(JOB)
        .unwrap()
        .ended_at_ms = Some(0);
    again
        .job_stream(JOB)
        .unwrap()
        .expire_chunks(u64::MAX)
        .unwrap();
}

#[tokio::test]
async fn detached_caller_retains_raw_output_returns_terminal_and_one_usage_across_poweroff_and_restart(
) {
    let (runtime, fake, storage) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    let frames = records();
    for frame in &frames[..5] {
        guest.write_all(frame).await.unwrap();
    }
    for frame in &frames[..5] {
        assert_eq!(next_record(&mut client).await.bytes, *frame);
    }
    drop(client);
    task.await.unwrap().unwrap();
    for frame in &frames[5..] {
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
        assert_eq!(guest_frame(&mut guest).await, request);
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
async fn delayed_exit_after_delivered_channel_failure_commits_terminal_for_live_and_attach() {
    let (runtime, fake, _) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    // No Started frame is observed, but the delivered Run may still have run.
    guest.write_all(&records()[1]).await.unwrap();
    assert_eq!(next_record(&mut client).await.value["seq"], 1);
    fake.fail_poll.store(true, Ordering::SeqCst);
    drop(guest);
    let stream = runtime.job_stream(JOB).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while stream.running_for_test() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!stream.with_store(|s| Ok(s.ended())).unwrap());
    fake.fail_poll.store(false, Ordering::SeqCst);
    runtime.collect_job_exits(unix_ms()).unwrap();
    assert!(stream.with_store(|s| Ok(s.ended())).unwrap());
    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = read_frame(&mut client).await.unwrap();
            if frame["type"] == "terminal" {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(terminal["outcome"]["type"], "outcome_unknown");
    assert_eq!(terminal["ran"], "remote");
    assert_eq!(terminal["last_seq"], 2);
    task.await.unwrap().unwrap();
    let attached = replayed(&runtime).await;
    assert_eq!(
        crate::replay::decode(attached.last().unwrap()).unwrap(),
        terminal
    );
}

#[tokio::test]
async fn execution_exit_preserves_an_already_stored_runner_terminal() {
    let (runtime, fake, _) = fixture();
    let id = claim(&runtime, &claims(JOB));
    runtime.host.lock().unwrap().delivered(id).unwrap();
    let stored = runtime.job_stream(JOB).unwrap();
    stored
        .with_store(|s| {
            for frame in records() {
                s.append(&frame)?;
            }
            Ok(())
        })
        .unwrap();
    {
        let terminal = crate::replay::decode(records().last().unwrap()).unwrap();
        runtime.observe_job_frame(JOB, &terminal).unwrap();
    }
    power_off(&runtime, &fake);
    assert_eq!(
        stored.with_store(|s| s.frame(s.len() - 1)).unwrap(),
        Some(records().last().unwrap().clone())
    );
    assert_eq!(replayed(&runtime).await.last(), records().last());
}

#[tokio::test]
async fn restart_recovers_a_missing_terminal_after_receipt_or_evidence_expiry() {
    use crate::state::jobs::storage::ReplayCompletion;
    for receipt in [false, true] {
        let (runtime, fake, storage) = fixture();
        let id = claim(&runtime, &claims(JOB));
        {
            let mut host = runtime.host.lock().unwrap();
            host.delivered(id).unwrap();
            if receipt {
                host.job_exit_received(
                    &motor_vm_helper::job::ExitReport {
                        job_id: JOB.into(),
                        oom_kill: 0,
                        egress_bytes: None,
                    },
                    unix_ms(),
                )
                .unwrap();
            }
            host.job_stopped(JOB, true).unwrap();
            // Older snapshots can have exit evidence without a replayable billing terminal.
            let mut next = host.snapshot.clone();
            next.billing.entry(JOB.into()).or_default().terminal = None;
            host.save(next).unwrap();
        }
        let mut store = crate::replay::Store::open(storage.root.path(), JOB).unwrap();
        store.append(&records()[1]).unwrap();
        drop(store);
        let restarted = reopen(&runtime, fake, storage);
        restarted.recover().unwrap();
        let stream = restarted.job_stream(JOB).unwrap();
        assert!(stream.with_store(|s| Ok(s.ended())).unwrap());
        assert_eq!(
            restarted.host.lock().unwrap().snapshot.per_job.executions[JOB].replay_completion,
            Some(ReplayCompletion::Terminal)
        );
        let attached = replayed(&restarted).await;
        let terminal = crate::replay::decode(attached.last().unwrap()).unwrap();
        assert_eq!(terminal["outcome"]["type"], "outcome_unknown");
        assert_eq!(terminal["ran"], "remote");
        assert_eq!(terminal["last_seq"], 1);
    }
}

#[test]
fn missing_terminal_without_durable_execution_end_is_not_repaired() {
    for ended_without_evidence in [false, true] {
        let (runtime, fake, storage) = fixture();
        claim(&runtime, &claims(JOB));
        if ended_without_evidence {
            let mut host = runtime.host.lock().unwrap();
            let mut next = host.snapshot.clone();
            next.per_job.executions.get_mut(JOB).unwrap().ended = true;
            host.save(next).unwrap();
        }
        let restarted = reopen(&runtime, fake, storage);
        let stream = restarted.job_stream(JOB).unwrap();
        let ended = stream.with_store(|s| Ok(s.ended()));
        if ended_without_evidence {
            assert!(ended.is_err());
        } else {
            assert!(!ended.unwrap());
        }
    }
}

#[tokio::test]
async fn authoritative_terminals_are_byte_identical_live_and_on_replay_after_restart() {
    for host_stop in [false, true] {
        let (runtime, fake, storage) = fixture();
        let (mut client, task, mut guest) = launched(&runtime, &fake).await;
        let frames = records();
        for frame in &frames[..5] {
            guest.write_all(frame).await.unwrap();
        }
        for frame in &frames[..5] {
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
            let tail =
                bytes(br#"{ "type":"output", "seq":5, "stream":"stdout", "bytes":"dGFpbA==" }"#);
            guest.write_all(&tail).await.unwrap();
            assert_eq!(next_record(&mut client).await.bytes, tail);
        }
        let mut forged = crate::replay::decode(frames.last().unwrap()).unwrap();
        forged["oom_kill"] = json!({"cause":"host_pressure"});
        if host_stop {
            forged["last_seq"] = json!(123);
        }
        let raw_terminal = bytes(&serde_json::to_vec(&forged).unwrap());
        guest.write_all(&raw_terminal).await.unwrap();
        let delivered = next_record(&mut client).await;
        assert_eq!(delivered.value["type"], "terminal");
        assert_eq!(delivered.value["last_seq"], if host_stop { 5 } else { 4 });
        if host_stop {
            assert_eq!(delivered.value["oom_kill"]["cause"], "host_pressure");
            assert_eq!(delivered.value["outcome"]["type"], "signal");
        } else {
            assert!(delivered.value.get("oom_kill").is_none());
            assert_eq!(delivered.value["outcome"]["type"], "exit");
        }
        ack_at(&runtime, &mut guest, if host_stop { 5 } else { 4 }).await;
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
        assert_eq!(guest_frame(&mut guest).await["type"], "job_status");
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

#[tokio::test]
async fn undelivered_upload_eof_keeps_the_no_start_terminal_and_hosted_receipt_live() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    {
        let mut host = runtime.host.lock().unwrap();
        let disk = host.snapshot.per_job.executions[JOB].disk_id.clone();
        host.snapshot
            .per_job
            .disks
            .get_mut(&disk)
            .unwrap()
            .attached_job = Some("holder".into());
    }
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    write_frame(
        &mut client,
        &json!({"type":"bundle_chunk","index":0,"offset":0,"data":"YWJj"}),
    )
    .await
    .unwrap();
    client.shutdown().await.unwrap();
    let terminal = next_type(&mut client, "terminal").await;
    assert_eq!(terminal["outcome"]["type"], "refused_before_start");
    assert_eq!(terminal["outcome"]["reason"], "transfer_interrupted");
    assert_eq!(terminal["ran"], "none");
    next_type(&mut client, "hosted_terminal").await;
    task.await.unwrap().unwrap();
    let stream = runtime.job_stream(JOB).unwrap();
    assert_eq!(
        stream
            .with_store(|store| store.frame(0))
            .unwrap()
            .map(|frame| crate::replay::decode(&frame).unwrap()),
        Some(terminal)
    );
    let host = runtime.host.lock().unwrap();
    assert!(!host.snapshot.leases[JOB].delivered);
    assert_eq!(
        host.snapshot.zero_usage[JOB].started_at_ms,
        host.snapshot.zero_usage[JOB].ended_at_ms
    );
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn relayed_run_alone_sets_return_reservation_and_runner_hints_cannot_change_it() {
    use crate::state::jobs::storage::{REPLAY_RESERVATION_BYTES, RETURN_BYTES};
    for returns in [false, true] {
        let (runtime, fake, _storage) = fixture();
        let (mut client, task, mut guest) = launched_with_returns(&runtime, &fake, returns).await;
        write_frame(
            &mut guest,
            &json!({"type":"output","seq":0,"stream":"stdout","bytes":"eA=="}),
        )
        .await
        .unwrap();
        next_type(&mut client, "output").await;
        let expected = REPLAY_RESERVATION_BYTES - if returns { 0 } else { RETURN_BYTES };
        assert_eq!(
            runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].replay_reservation_bytes,
            Some(expected)
        );
        assert_eq!(
            runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].relayed_return_paths,
            Some(returns)
        );
        for frame in [
            json!({"type":"hello","return_paths":!returns,"return_reservation_bytes":if returns {0} else {RETURN_BYTES}}),
            json!({"type":"heartbeat","return_reservation_bytes":if returns {0} else {RETURN_BYTES}}),
            json!({"type":"accepted","return_paths":!returns}),
        ] {
            write_frame(&mut guest, &frame).await.unwrap();
        }
        write_frame(
            &mut guest,
            &json!({"type":"output","seq":1,"stream":"stdout","bytes":"eA=="}),
        )
        .await
        .unwrap();
        next_type(&mut client, "output").await;
        assert_eq!(
            runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].replay_reservation_bytes,
            Some(expected)
        );
        guest.write_all(records().last().unwrap()).await.unwrap();
        next_type(&mut client, "terminal").await;
        assert_eq!(guest_frame(&mut guest).await["type"], "terminal_stored");
        drop(client);
        task.await.unwrap().unwrap();
        power_off(&runtime, &fake);
    }
}

#[tokio::test]
async fn fake_clock_chunk_expiry_and_delayed_usage_acceptance_survive_restart_without_rebilling_attach(
) {
    use crate::billing::{RECORD_RETENTION_MS, REPLAY_RETENTION_MS};
    use motor_protocol::{ReturnManifestEntry, ReturnManifestOutcome, ReturnPathsFailureCause};
    let (runtime, fake, storage) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    let mut manifest = json!({"type":"return_manifest","seq":3,"job_id":JOB,"cap_bytes":134217728,"total_bytes":3});
    manifest["files"] = serde_json::to_value(vec![ReturnManifestEntry::returned(
        "/out",
        3,
        "0".repeat(64),
    )
    .with_chunks(1)])
    .unwrap();
    for value in [
        json!({"type":"started","seq":0,"job_id":JOB}),
        json!({"type":"output","seq":1,"stream":"stdout","bytes":"eA=="}),
        json!({"type":"return_chunk","seq":2,"job_id":JOB,"path_index":0,"offset":0,"data":"YWJj"}),
        manifest,
    ] {
        write_frame(&mut guest, &value).await.unwrap();
    }
    guest.write_all(records().last().unwrap()).await.unwrap();
    while next_record(&mut client).await.value["type"] != "terminal" {}
    assert_eq!(guest_frame(&mut guest).await["type"], "terminal_stored");
    drop(client);
    task.await.unwrap().unwrap();
    power_off(&runtime, &fake);
    let end = runtime.host.lock().unwrap().snapshot.leases[JOB]
        .ended_at_ms
        .unwrap();
    let queued = runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .outbox
        .iter()
        .find(|r| r.job.as_deref() == Some(JOB))
        .unwrap()
        .clone();
    let body = queued.body.clone();
    runtime
        .job_stream(JOB)
        .unwrap()
        .expire_chunks(end + RECORD_RETENTION_MS - 1)
        .unwrap();
    let before = replayed_at(&runtime, end + RECORD_RETENTION_MS - 1).await;
    assert!(before
        .iter()
        .any(|f| crate::replay::decode(f).unwrap()["type"] == "return_chunk"));
    runtime
        .job_stream(JOB)
        .unwrap()
        .expire_chunks(end + RECORD_RETENTION_MS)
        .unwrap();
    let after = replayed_at(&runtime, end + RECORD_RETENTION_MS).await;
    assert!(!after
        .iter()
        .any(|f| crate::replay::decode(f).unwrap()["type"] == "return_chunk"));
    let manifest: motor_protocol::ReturnManifest = after
        .iter()
        .map(|f| crate::replay::decode(f).unwrap())
        .find(|f| f["type"] == "return_manifest")
        .map(|f| serde_json::from_value(f).unwrap())
        .unwrap();
    assert!(matches!(
        manifest.files[0].outcome,
        ReturnManifestOutcome::Failed {
            cause: ReturnPathsFailureCause::Expired,
            size: Some(3)
        }
    ));
    assert_eq!(manifest.total_bytes, 0);
    let paths = vec!["/out".to_owned()];
    motor_protocol::ReturnAssembler::new(&paths, 134217728)
        .finish_expired_replay(&manifest, 1)
        .unwrap();
    let accepted = end + 2 * 24 * 60 * 60 * 1000;
    assert_eq!(
        runtime
            .host
            .lock()
            .unwrap()
            .prune_retention(accepted)
            .unwrap(),
        0
    );
    assert!(!replayed_at(&runtime, accepted).await.is_empty());
    assert_eq!(
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .outbox
            .iter()
            .filter(|r| r.job.as_deref() == Some(JOB))
            .map(|r| &r.body)
            .collect::<Vec<_>>(),
        vec![&body]
    );
    let reply = json!({"job_id":JOB,"settled_vcpu_ms":100,"released_vcpu_ms":0});
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .finish_request_at(&queued, 200, &reply, accepted)
        .unwrap());
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .finish_request_at(&queued, 200, &reply, accepted + 1000)
        .unwrap());
    assert_eq!(queued.body, body);
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].usage_accepted_at_ms,
        Some(accepted)
    );
    let restarted = reopen(&runtime, fake, storage.clone());
    let boundary = accepted + REPLAY_RETENTION_MS;
    assert_eq!(
        restarted
            .host
            .lock()
            .unwrap()
            .prune_retention(boundary)
            .unwrap(),
        0
    );
    let frames = replayed_at(&restarted, boundary).await;
    assert_eq!(
        frames
            .iter()
            .filter(|f| crate::replay::decode(f).unwrap()["type"] == "terminal")
            .count(),
        1
    );
    assert!(restarted
        .host
        .lock()
        .unwrap()
        .snapshot
        .outbox
        .iter()
        .all(|r| r.job.as_deref() != Some(JOB)));
    assert_eq!(
        restarted
            .host
            .lock()
            .unwrap()
            .prune_retention(boundary + 1)
            .unwrap(),
        1
    );
    let mut attach = claims(JOB);
    attach.purpose = Purpose::Attach;
    attach.exp = (boundary + 1) / 1000 + 300;
    assert!(restarted
        .host
        .lock()
        .unwrap()
        .claim(attach, boundary + 1)
        .is_err());
    assert_eq!(
        crate::replay::Store::open(storage.root.path(), JOB)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn old_execution_snapshot_has_conservative_replay_defaults() {
    let (runtime, _, _) = fixture();
    claim(&runtime, &claims(JOB));
    let mut snapshot = serde_json::to_value(&runtime.host.lock().unwrap().snapshot).unwrap();
    let execution = snapshot["per_job"]["executions"][JOB]
        .as_object_mut()
        .unwrap();
    execution.remove("relayed_return_paths");
    execution.remove("usage_accepted_at_ms");
    let snapshot: crate::state::Snapshot = serde_json::from_value(snapshot).unwrap();
    assert_eq!(snapshot.per_job.executions[JOB].relayed_return_paths, None);
    assert_eq!(snapshot.per_job.executions[JOB].usage_accepted_at_ms, None);
    assert_eq!(
        snapshot.per_job.executions[JOB].replay_reservation_bytes,
        Some(crate::state::jobs::storage::REPLAY_RESERVATION_BYTES)
    );
}

#[tokio::test]
async fn slow_live_caller_and_restarted_attach_receive_truncation_then_all_retained_frames() {
    let (runtime, fake, storage) = fixture();
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    write_frame(&mut guest, &json!({"type":"started","seq":0,"job_id":JOB}))
        .await
        .unwrap();
    for seq in 1..=110 {
        write_frame(
            &mut guest,
            &json!({"type":"output","seq":seq,"stream":"stdout","bytes":"e".repeat(700_000)}),
        )
        .await
        .unwrap();
    }
    guest.write_all(records().last().unwrap()).await.unwrap();
    assert_eq!(guest_frame(&mut guest).await["type"], "terminal_stored");
    let mut seen = Vec::new();
    let mut terminal_count = 0;
    loop {
        let value = next_record(&mut client).await.value;
        if value["type"] == "output" {
            seen.push(value);
        } else if value["type"] == "terminal" {
            terminal_count += 1;
            break;
        }
    }
    assert_eq!(terminal_count, 1);
    let beginning = seen
        .iter()
        .position(|v| v["truncated_before_seq"].is_u64())
        .expect("slow caller must cross an evicted segment");
    let earliest = seen[beginning]["seq"].as_u64().unwrap();
    assert_eq!(seen[beginning]["truncated_before_seq"], earliest);
    assert_eq!(
        seen[beginning..]
            .iter()
            .map(|f| f["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (earliest..=110).collect::<Vec<_>>()
    );
    drop(client);
    task.await.unwrap().unwrap();
    power_off(&runtime, &fake);
    let restarted = reopen(&runtime, fake, storage);
    let replay = replayed(&restarted).await;
    let output: Vec<_> = replay
        .iter()
        .map(|f| crate::replay::decode(f).unwrap())
        .filter(|f| f["type"] == "output")
        .collect();
    assert_eq!(output[0]["truncated_before_seq"], earliest);
    assert_eq!(
        output
            .iter()
            .map(|f| f["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (earliest..=110).collect::<Vec<_>>()
    );
    assert_eq!(
        replay
            .iter()
            .filter(|f| crate::replay::decode(f).unwrap()["type"] == "terminal")
            .count(),
        1
    );
}
#[tokio::test]
async fn failed_export_starts_after_caller_terminal_and_does_not_extend_billing_or_change_replay() {
    let (runtime, fake, _) = fixture();
    let (export_host, mut export_guest) = UnixStream::pair().unwrap();
    *fake.fill_export.lock().unwrap() = Some(export_host);
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    // The fixture's legacy Hello does not attest a default-branch tip. Install
    // the same host-owned request that an attested Run records for this test.
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .export_tips
        .insert(
            JOB.into(),
            crate::jobs::fill_source::Request {
                account: claims(JOB).sub,
                repository_root: "/home/source".into(),
                integration_tip: "a".repeat(40),
                mem_mib: 100,
                observed_ms: unix_ms(),
            },
        );
    let frames = records();
    for frame in &frames {
        guest.write_all(frame).await.unwrap();
    }
    for frame in &frames {
        assert_eq!(next_record(&mut client).await.bytes, *frame);
    }
    task.await.unwrap().unwrap();
    let end = runtime.host.lock().unwrap().snapshot.leases[JOB]
        .ended_at_ms
        .unwrap();
    let usage_before = runtime.host.lock().unwrap().snapshot.billing[JOB].clone();
    let at_export = runtime.clone();
    let request = tokio::task::spawn_blocking(move || {
        let request = crate::helper::read_json(&mut export_guest).unwrap();
        assert_eq!(request["op"], "export_tip");
        {
            let mut host = at_export.host.lock().unwrap();
            assert_eq!(host.snapshot.leases[JOB].ended_at_ms, Some(end));
            assert_eq!(host.snapshot.billing[JOB].terminal_received_ms, Some(end));
            assert_eq!(host.snapshot.billing[JOB].ended_observed_ms, Some(end));
            host.billing_tick(end + 300_000).unwrap();
            assert_eq!(host.snapshot.leases[JOB].ended_at_ms, Some(end));
        }
        crate::helper::write_json(
            &mut export_guest,
            &json!({"ok":false,"error":"export failure"}),
        )
        .unwrap();
        request
    })
    .await
    .unwrap();
    assert_eq!(request["job_id"], JOB);
    ack(&runtime, &mut guest).await;
    runtime
        .host
        .lock()
        .unwrap()
        .billing_tick(end + 300_000)
        .unwrap();
    power_off(&runtime, &fake);
    {
        let host = runtime.host.lock().unwrap();
        assert_eq!(host.snapshot.leases[JOB].ended_at_ms, Some(end));
        assert_eq!(host.snapshot.billing[JOB].terminal, usage_before.terminal);
        assert_eq!(
            host.snapshot.billing[JOB].ended_observed_ms,
            usage_before.ended_observed_ms
        );
        assert!(host.snapshot.per_job.fill_sources.is_empty());
        let billing = &host.snapshot.billing[JOB];
        let usage = billing.usage.as_ref().unwrap();
        assert_eq!(usage.ended_at_ms, end);
        assert_eq!(
            billing.hosted_terminal.as_ref().unwrap()["billed_vcpu_ms"],
            u64::from(claims(JOB).vcpus) * (end - usage.started_at_ms)
        );
    }
    assert_eq!(replayed(&runtime).await, frames);
}
#[tokio::test]
async fn timed_out_export_stops_the_vm_without_changing_the_terminal_or_billed_end() {
    let (runtime, fake, _) = fixture();
    let (export_host, _stuck_guest) = UnixStream::pair().unwrap();
    *fake.fill_export.lock().unwrap() = Some(export_host);
    let (mut client, task, mut guest) = launched(&runtime, &fake).await;
    runtime
        .job_stream(JOB)
        .unwrap()
        .set_export_timeout(Duration::from_millis(50));
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .export_tips
        .insert(
            JOB.into(),
            crate::jobs::fill_source::Request {
                account: claims(JOB).sub,
                repository_root: "/home/source".into(),
                integration_tip: "a".repeat(40),
                mem_mib: 100,
                observed_ms: unix_ms(),
            },
        );
    let frames = records();
    for frame in &frames {
        guest.write_all(frame).await.unwrap();
    }
    for frame in &frames {
        assert_eq!(next_record(&mut client).await.bytes, *frame);
    }
    task.await.unwrap().unwrap();
    let end = runtime.host.lock().unwrap().snapshot.leases[JOB]
        .ended_at_ms
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("export deadline must stop the VM");
    {
        let host = runtime.host.lock().unwrap();
        assert_eq!(host.snapshot.leases[JOB].ended_at_ms, Some(end));
        assert!(host.snapshot.per_job.export_tips.is_empty());
        assert!(host.snapshot.per_job.fill_sources.is_empty());
        assert_eq!(
            host.snapshot.billing[JOB].terminal.as_ref().unwrap()["outcome"],
            json!({"type":"exit","code":0})
        );
    }
    assert_eq!(replayed(&runtime).await, frames);
}
