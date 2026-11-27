use super::*;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

#[tokio::test]
async fn full_spool_with_buffered_heartbeat_replays_when_vm_becomes_ready() {
    let (runtime, fake, storage) = fixture();
    hold_boot(&runtime, &fake, "runner");
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let mut frames = vec![hello(), run(JOB)];
    let mut spooled: u64 = frames
        .iter()
        .map(|f| serde_json::to_vec(f).unwrap().len() as u64 + 4)
        .sum();
    let threshold = SPOOL_LIMIT - FRAME_LIMIT as u64 - 4;
    while spooled <= threshold {
        let chunk =
            json!({"type":"bundle_chunk","index":0,"offset":0,"data":"YWFh".repeat(175_000)});
        spooled += serde_json::to_vec(&chunk).unwrap().len() as u64 + 4;
        frames.push(chunk);
        if spooled > threshold {
            // CutClient reads 8192 bytes at a time. Ensure the next heartbeat
            // is already buffered when the last spool append closes admission.
            assert!(spooled % 8192 < 8100);
            frames.push(json!({"type":"heartbeat"}));
            break;
        }
        let heartbeat = json!({"type":"heartbeat"});
        spooled += serde_json::to_vec(&heartbeat).unwrap().len() as u64 + 4;
        frames.push(heartbeat);
    }
    let mut client = CutClient::new(&frames, Cut::Write("output"));
    let r = runtime.clone();
    let mut task = tokio::spawn(async move { serve(r, &c, id, &mut client).await });
    // Spooling 64 MiB through a debug build takes several seconds on a small
    // CI machine; these bounds only catch a stall, which never completes.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if std::fs::metadata(storage.spool(JOB)).is_ok_and(|m| m.len() > threshold) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    let completed = tokio::time::timeout(Duration::from_secs(30), &mut task).await;
    if completed.is_err() {
        task.abort();
    }
    let delivered = runtime.host.lock().unwrap().snapshot.leases[JOB].delivered;
    runtime.stop_job(JOB, true, true).unwrap();
    assert!(
        completed.is_ok(),
        "a buffered heartbeat must not pin a full spool after boot"
    );
    completed.unwrap().unwrap().unwrap();
    assert!(delivered, "the queued Run must reach the guest");
}

/// A first job in a new workspace uploads the repository's whole history,
/// often more than the pre-boot spool holds. The host stops reading the caller
/// once the spool is full; after the VM boots, every byte of the upload must
/// still reach the guest runner, which gives up on an upload that makes no
/// progress for 60 seconds.
#[tokio::test]
async fn upload_larger_than_spool_reaches_the_guest_after_boot() {
    let logs = crate::test_logs::Capture::start();
    let (runtime, fake, storage) = fixture();
    hold_boot(&runtime, &fake, "runner");
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    // Each chunk carries 525,000 bytes ("YWFh" decodes to "aaa").
    let chunk_bytes: u64 = 525_000;
    let chunks: u64 = 140;
    let length = chunk_bytes * chunks;
    assert!(length > SPOOL_LIMIT);
    let mut request = run(JOB);
    request["bundles"] = json!([{
        "index": 0,
        "repository": request["repository_root"],
        "length": length,
        "sha256": "0".repeat(64),
    }]);
    request["siblings"] = json!([]);
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let writer = tokio::spawn(async move {
        for (i, frame) in [hello(), request]
            .into_iter()
            .chain((0..chunks).map(|i| {
                json!({"type":"bundle_chunk","index":0,"offset":i * chunk_bytes,"data":"YWFh".repeat(175_000)})
            }))
            .enumerate()
        {
            let body = serde_json::to_vec(&frame).unwrap();
            client_write
                .write_all(&(body.len() as u32).to_be_bytes())
                .await
                .map_err(|e| format!("frame {i}: {e}"))?;
            client_write
                .write_all(&body)
                .await
                .map_err(|e| format!("frame {i}: {e}"))?;
        }
        Ok::<_, String>(client_write)
    });
    let reader = tokio::spawn(async move {
        let mut sink = vec![0; 65536];
        while client_read.read(&mut sink).await.is_ok_and(|n| n > 0) {}
    });
    let r = runtime.clone();
    let session = tokio::spawn(async move { serve(r, &c, id, &mut server).await });
    let threshold = SPOOL_LIMIT - FRAME_LIMIT as u64 - 4;
    tokio::time::timeout(Duration::from_secs(60), async {
        while !std::fs::metadata(storage.spool(JOB)).is_ok_and(|m| m.len() > threshold) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the spool fills while the VM boot is held");
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    let received = || -> u64 {
        fake.inputs
            .lock()
            .unwrap()
            .iter()
            .map(|bytes| crate::replay::decode(bytes).unwrap())
            .filter(|frame| frame["type"] == "bundle_chunk")
            .map(|frame| frame["data"].as_str().unwrap().len() as u64 / 4 * 3)
            .sum()
    };
    // Spooling and replaying this upload through a debug build takes several
    // seconds on a small machine; the bound only catches a stall.
    let delivered = tokio::time::timeout(Duration::from_secs(60), async {
        while received() < length {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let got = received();
    // The caller then closes its input, as after a finished upload.
    let closed = async {
        let mut input = writer.await.unwrap().unwrap();
        input.shutdown().await.unwrap();
        session.await.unwrap().unwrap();
    };
    let closed = tokio::time::timeout(Duration::from_secs(30), closed).await;
    reader.abort();
    runtime.stop_job(JOB, true, true).unwrap();
    assert!(
        delivered.is_ok(),
        "the guest received {got} of {length} upload bytes"
    );
    closed.expect("the session ends once the caller closes its input");
    let lines = logs.lines();
    let line = |message: &str| {
        lines
            .iter()
            .find(|line| line.contains(&format!("message={message} ")))
            .unwrap_or_else(|| panic!("no {message} line in {lines:#?}"))
            .clone()
    };
    assert!(line("job_upload_replay_started").contains("spooled_bytes="));
    let finished = line("job_upload_replay_finished");
    assert!(
        finished.contains(&format!("expected_bytes={length} ")),
        "{finished}"
    );
    assert!(!finished.contains("received_bytes=0 "), "{finished}");
    assert!(
        !finished.contains(&format!("received_bytes={length} ")),
        "{finished}"
    );
    assert!(line("job_upload_first_live_chunk").contains(&format!("expected_bytes={length} ")));
    assert!(line("job_upload_forwarded").contains(&format!("bytes={length} ")));
    assert!(line("job_caller_input_closed").contains("clean=true "));
}

#[tokio::test]
async fn live_upload_continues_while_caller_heartbeat_write_is_blocked() {
    let (runtime, fake, _) = fixture();
    hold_boot(&runtime, &fake, "runner");
    let (host, guest) = UnixStream::pair().unwrap();
    fake.scripted.lock().unwrap().push_back(host);
    guest.set_nonblocking(true).unwrap();
    let mut guest = tokio::net::UnixStream::from_std(guest).unwrap();

    const BUFFER: usize = 4096;
    const CHUNK_BYTES: usize = 65_535;
    const CHUNKS: usize = 32;
    let first_chunk = |i| json!({"type":"bundle_chunk","index":0,"offset":i * CHUNK_BYTES,"data":"YWFh".repeat(CHUNK_BYTES / 3)});
    let (mut client, mut server) = tokio::io::duplex(BUFFER);
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let r = runtime.clone();
    let session = tokio::spawn(async move { serve(r, &c, id, &mut server).await });
    let setup = tokio::time::timeout(Duration::from_secs(15), async {
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        let mut request = run(JOB);
        request["bundles"] = json!([{
            "index": 0, "repository": request["repository_root"],
            "length": CHUNKS * CHUNK_BYTES, "sha256": "0".repeat(64),
        }]);
        write_frame(&mut client, &request).await.unwrap();
        next_type(&mut client, "accepted").await;
        write_frame(&mut client, &first_chunk(0)).await.unwrap();
        // The withdrawal reply confirms the preceding chunk is in the spool
        // without waiting on filesystem timing or withdrawing an accepted job.
        write_frame(&mut client, &json!({"type":"withdraw","job_id":JOB}))
            .await
            .unwrap();
        next_type(&mut client, "withdraw_reply").await;
        *fake.gate.0.lock().unwrap() = true;
        fake.gate.1.notify_all();
        assert_eq!(
            read_frame(&mut guest).await.unwrap()["type"],
            "verified_lease"
        );
        assert_eq!(read_frame(&mut guest).await.unwrap()["type"], "hello");
        write_frame(&mut guest, &hello()).await.unwrap();
        assert_eq!(read_frame(&mut guest).await.unwrap(), request);
        assert_eq!(read_frame(&mut guest).await.unwrap(), first_chunk(0));
        assert_eq!(read_frame(&mut guest).await.unwrap()["type"], "withdraw");

        // Use a runner heartbeat too large for the caller's receive buffer.
        // After reading its prefix, stop draining that direction so the host
        // cannot finish the heartbeat until the live upload has arrived.
        let beat = json!({"type":"heartbeat","extension":"a".repeat(2 * BUFFER)});
        let body = serde_json::to_vec(&beat).unwrap();
        write_frame(&mut guest, &beat).await.unwrap();
        loop {
            let len = client.read_u32().await.unwrap() as usize;
            if len == body.len() {
                break;
            }
            let mut earlier = vec![0; len];
            client.read_exact(&mut earlier).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&earlier).unwrap()["type"],
                "heartbeat"
            );
        }
        body
    })
    .await
    .expect("spool replay and the blocked heartbeat start");
    let delivered = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            async {
                for i in 1..CHUNKS {
                    write_frame(&mut client, &first_chunk(i)).await.unwrap();
                }
            },
            async {
                for i in 1..CHUNKS {
                    loop {
                        let frame = read_frame(&mut guest).await.unwrap();
                        if frame["type"] != "heartbeat" {
                            assert_eq!(frame, first_chunk(i));
                            break;
                        }
                    }
                }
            }
        );
        let mut body = vec![0; setup.len()];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(body, setup, "the partially written heartbeat stays intact");
    })
    .await;
    session.abort();
    drop(guest);
    runtime.stop_job(JOB, true, true).unwrap();
    delivered.expect("all live upload bytes must reach the guest while caller output is blocked");
}

/// Uploads `chunks` bundle chunks of 65,535 bytes through the per-job host
/// relay to the real guest runner's tenant channel. The VM boot is held until
/// the host's pre-boot spool holds `release_at` bytes, so the runner gets the
/// spooled part replayed and the rest live. Returns the caller's terminal.
#[cfg(target_os = "linux")]
async fn real_guest_upload(chunks: u64, release_at: impl Fn(u64) -> u64) -> Value {
    use sha2::{Digest, Sha256};
    let (runtime, fake, storage) = fixture();
    hold_boot(&runtime, &fake, "runner");
    let root = storage.root.path().join("guest");
    let mut config = motor_runner::Config::new(root.join("disk"), "runner");
    config.tenant_vm = true;
    config.per_job = true;
    config.development_allow_host_backend = true;
    config.free_space_floor_bytes = 0;
    config.max_weight = 1;
    config.reserve_daemon_core = false;
    config.workspace_roots = vec![root.join("remote/worktrees")];
    config.owned_workspace_roots = vec![root.join("remote")];
    *storage.pinned_hello.lock().unwrap() =
        Some(serde_json::to_value(motor_runner::runner_hello(&config, true, true, true)).unwrap());
    let runner = motor_runner::Runner::open(config).unwrap();
    let (host_end, guest_end) = UnixStream::pair().unwrap();
    guest_end.set_nonblocking(true).unwrap();
    let guest_end = tokio::net::UnixStream::from_std(guest_end).unwrap();
    let guest = tokio::spawn(motor_runner::serve_tenant_connection(runner, guest_end));
    fake.scripted.lock().unwrap().push_back(host_end);

    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let chunk_bytes: u64 = 65_535;
    let length = chunk_bytes * chunks;
    let mut digest = Sha256::new();
    for _ in 0..chunks {
        digest.update([b'a'; 65_535]);
    }
    let workspace = root.join("remote/worktrees/job");
    let repository = root.join("remote/source/main");
    let request = json!({"type":"run","job_id":JOB,"workspace_key":workspace,"repository_root":repository,
        "cwd":workspace,"command":"true","env":{},"weight_hint":1,"timeout":30,"queue_wait_limit_s":1800,
        "snapshot_commit":"1".repeat(40),"tree_hash":"3".repeat(40),"integration_tip":"2".repeat(40),"siblings":[],
        "bundles":[{"index":0,"repository":repository,"length":length,"sha256":format!("{:x}", digest.finalize())}]});
    let client_hello = json!({"type":"hello","protocol_version":1,"started":true,"return_paths":true,
        "return_chunked":true,"sibling_paths":true,"gate_jobs":true});
    let frames: Vec<Vec<u8>> = [client_hello, request]
        .into_iter()
        .chain((0..chunks).map(|i| {
            json!({"type":"bundle_chunk","index":0,"offset":i * chunk_bytes,"data":"YWFh".repeat(21_845)})
        }))
        .map(|frame| {
            let body = serde_json::to_vec(&frame).unwrap();
            [(body.len() as u32).to_be_bytes().as_slice(), &body].concat()
        })
        .collect();
    let total: u64 = frames.iter().map(|f| f.len() as u64).sum();
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let writer = tokio::spawn(async move {
        for frame in frames {
            client_write.write_all(&frame).await?;
        }
        // Like ck-motor, stay connected after the upload to follow the job.
        std::future::pending::<()>().await;
        Ok::<_, std::io::Error>(())
    });
    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
    let reader = tokio::spawn(async move {
        loop {
            let Ok(frame) = crate::server::read_frame(&mut client_read).await else {
                return;
            };
            if frame["type"] == "terminal" {
                let _ = terminal_tx.send(frame);
                return;
            }
        }
    });
    let r = runtime.clone();
    let session = tokio::spawn(async move { serve(r, &c, id, &mut server).await });
    let release = release_at(total);
    tokio::time::timeout(Duration::from_secs(60), async {
        while !std::fs::metadata(storage.spool(JOB)).is_ok_and(|m| m.len() >= release) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the spool reaches the release size while the VM boot is held");
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    // The runner refuses an upload with no progress after 60 seconds, so a
    // stall shows up as transfer_interrupted within this bound.
    let terminal = tokio::time::timeout(Duration::from_secs(120), terminal_rx)
        .await
        .expect("the caller gets a terminal")
        .expect("terminal reader");
    session.abort();
    writer.abort();
    reader.abort();
    guest.abort();
    let _ = runtime.stop_job(JOB, true, true);
    terminal
}

/// The test bundle is not a git bundle, so a runner that received and verified
/// every byte refuses it at fetch as bundle_rejected. A stalled upload ends as
/// transfer_interrupted instead.
#[cfg(target_os = "linux")]
fn assert_upload_reached_runner(terminal: &Value) {
    assert_eq!(
        terminal["outcome"]["reason"], "bundle_rejected",
        "the runner never got the whole upload: {terminal}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_runner_gets_an_upload_spooled_whole_before_attach() {
    let terminal = real_guest_upload(160, |total| total).await;
    assert_upload_reached_runner(&terminal);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_runner_gets_an_upload_partly_spooled_at_attach() {
    let terminal = real_guest_upload(400, |total| total / 2).await;
    assert_upload_reached_runner(&terminal);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_runner_gets_an_upload_larger_than_the_spool() {
    let terminal = real_guest_upload(1200, |_| SPOOL_LIMIT - FRAME_LIMIT as u64 - 4).await;
    assert_upload_reached_runner(&terminal);
}

#[derive(Clone, Copy)]
enum Cut {
    Read(ErrorKind),
    AbruptEof,
    Write(&'static str),
}

// Keep reads pending for a write cut so only the selected reply can fail.
// A read cut instead allows every reply to succeed, including heartbeats.
struct CutClient {
    input: std::io::Cursor<Vec<u8>>,
    output: Vec<u8>,
    sent: Vec<Value>,
    cut: Cut,
    accepted: bool,
    hit: bool,
}
impl CutClient {
    fn new(frames: &[Value], cut: Cut) -> Self {
        let mut input = Vec::new();
        for frame in frames {
            let bytes = serde_json::to_vec(frame).unwrap();
            input.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            input.extend_from_slice(&bytes);
        }
        Self {
            input: std::io::Cursor::new(input),
            output: Vec::new(),
            sent: Vec::new(),
            cut,
            accepted: false,
            hit: false,
        }
    }
}
impl AsyncRead for CutClient {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let position = self.input.position() as usize;
        if position < self.input.get_ref().len() {
            let n = buf.remaining().min(self.input.get_ref().len() - position);
            buf.put_slice(&self.input.get_ref()[position..position + n]);
            self.input.set_position((position + n) as u64);
            return Poll::Ready(Ok(()));
        }
        match self.cut {
            Cut::AbruptEof => Poll::Ready(Err(ErrorKind::UnexpectedEof.into())),
            Cut::Read(kind) => {
                self.hit = true;
                if kind == ErrorKind::UnexpectedEof {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(std::io::Error::new(kind, "forced caller read cut")))
                }
            }
            Cut::Write(_) => Poll::Pending,
        }
    }
}
impl AsyncWrite for CutClient {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.hit && matches!(self.cut, Cut::Write(_)) {
            return Poll::Ready(Err(ErrorKind::BrokenPipe.into()));
        }
        self.output.extend_from_slice(bytes);
        if self.output.len() >= 4
            && self.output.len()
                == 4 + u32::from_be_bytes(self.output[..4].try_into().unwrap()) as usize
        {
            let frame = crate::replay::decode(&self.output).unwrap();
            self.output.clear();
            if frame["type"] == "accepted" {
                self.accepted = true;
            }
            if let Cut::Write(kind) = self.cut {
                if frame["type"] == kind && (kind != "heartbeat" || self.accepted) {
                    self.hit = true;
                    return Poll::Ready(Err(ErrorKind::BrokenPipe.into()));
                }
            }
            self.sent.push(frame);
        }
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn interrupted(cut: Cut, frames: &[Value]) {
    let (runtime, fake, storage) = fixture();
    let first = claims(JOB);
    claim(&runtime, &first);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    // A busy workspace holds the upload in the host spool without a runner or
    // a timing-dependent boot future competing with the chosen transport cut.
    let c = claims(JOB2);
    let id = claim(&runtime, &c);
    let mut client = CutClient::new(frames, cut);
    let _ = serve(runtime.clone(), &c, id, &mut client).await;
    assert!(client.hit, "the selected transport cut must be reached");
    let snapshot = runtime.host.lock().unwrap().snapshot.clone();
    let terminal = snapshot.billing[JOB2]
        .terminal
        .as_ref()
        .expect("durable no-start terminal");
    assert_eq!(
        terminal["outcome"],
        json!({"type":"refused_before_start","reason":"transfer_interrupted"})
    );
    assert_eq!(terminal["ran"], "none");
    assert_eq!(terminal["wall_ms"], 0);
    assert_eq!(terminal["queue_wait_ms"], 0);
    assert_eq!(terminal["bundle_bytes"], 0);
    assert!(!snapshot.leases[JOB2].delivered);
    assert_eq!(
        snapshot.zero_usage[JOB2].started_at_ms,
        snapshot.zero_usage[JOB2].ended_at_ms
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(!storage.spool(JOB2).exists());
    let mut reattached = CutClient::new(
        &[hello(), json!({"type":"attach","job_id":JOB2,"from_seq":0})],
        Cut::Read(ErrorKind::UnexpectedEof),
    );
    live(&runtime, &c, &mut reattached).await.unwrap();
    assert_eq!(
        reattached
            .sent
            .iter()
            .find(|frame| frame["type"] == "terminal"),
        Some(terminal)
    );
    assert!(reattached
        .sent
        .iter()
        .any(|frame| frame["type"] == "hosted_terminal"));
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_read_before_hello_records_no_start() {
    interrupted(Cut::Read(ErrorKind::UnexpectedEof), &[]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_read_before_run_records_no_start() {
    interrupted(Cut::Read(ErrorKind::UnexpectedEof), &[hello()]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_reset_after_acceptance_records_no_start() {
    interrupted(Cut::Read(ErrorKind::ConnectionReset), &[hello(), run(JOB2)]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_eof_after_acceptance_records_no_start() {
    interrupted(Cut::Read(ErrorKind::UnexpectedEof), &[hello(), run(JOB2)]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_lease_reply_records_no_start() {
    interrupted(Cut::Write("lease_ok"), &[]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_hello_reply_records_no_start() {
    interrupted(Cut::Write("hello"), &[hello()]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_have_reply_records_no_start() {
    interrupted(
        Cut::Write("have_reply"),
        &[
            hello(),
            json!({"type":"have","repositories":[],"git_dependencies":[]}),
        ],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_accepted_reply_records_no_start() {
    interrupted(Cut::Write("accepted"), &[hello(), run(JOB2)]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_withdraw_reply_records_no_start() {
    let mut request = run(JOB2);
    request["queue_wait_limit_s"] = json!(30);
    interrupted(
        Cut::Write("withdraw_reply"),
        &[hello(), request, json!({"type":"withdraw","job_id":JOB2})],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_heartbeat_after_acceptance_records_no_start() {
    interrupted(
        Cut::Write("heartbeat"),
        &[
            hello(),
            run(JOB2),
            json!({"type":"bundle_chunk","index":0,"offset":0,"data":"YQ=="}),
        ],
    )
    .await;
}

#[tokio::test]
async fn replay_disconnect_before_delivery_is_returned_to_session_settlement() {
    let (runtime, _, _) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    // Before the replay worker authorizes Run, an owner disconnect must still
    // reach serve's no-start settlement, rather than look like a live detach.
    let mut client = CutClient::new(&[], Cut::Read(ErrorKind::UnexpectedEof));
    let error = stream::serve(&runtime, &c, &mut client, false, Reader::default(), true)
        .await
        .expect_err("an upload owner disconnect is returned for settlement");
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
}

async fn interrupted_before_boot() -> (Arc<Runtime>, Arc<Fake>, Arc<Storage>, Claims) {
    interrupted_before_boot_with(Cut::Write("heartbeat")).await
}

async fn interrupted_before_boot_with(cut: Cut) -> (Arc<Runtime>, Arc<Fake>, Arc<Storage>, Claims) {
    let (runtime, fake, storage) = fixture();
    claim(&runtime, &claims(JOB));
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    let c = claims(JOB2);
    let id = claim(&runtime, &c);
    let mut client = CutClient::new(&[hello(), run(JOB2)], cut);
    assert!(serve(runtime.clone(), &c, id, &mut client).await.is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        while !runtime.host.lock().unwrap().snapshot.per_job.executions[JOB2].ended {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB2].boot_started);
    (runtime, fake, storage, c)
}

async fn cancel_and_attach(runtime: &Arc<Runtime>, c: &Claims) -> Value {
    let mut client = CutClient::new(
        &[
            hello(),
            json!({"type":"cancel","job_id":JOB2}),
            json!({"type":"attach","job_id":JOB2,"from_seq":0}),
        ],
        Cut::Read(ErrorKind::UnexpectedEof),
    );
    live(runtime, c, &mut client).await.unwrap();
    assert!(client
        .sent
        .iter()
        .any(|frame| frame["type"] == "cancel_reply"));
    client
        .sent
        .into_iter()
        .find(|frame| frame["type"] == "terminal")
        .unwrap()
}

#[tokio::test]
async fn interrupted_upload_cancel_before_replay_is_cancelled_and_never_boots() {
    let (runtime, fake, _storage, c) = interrupted_before_boot().await;
    let terminal = cancel_and_attach(&runtime, &c).await;
    assert_eq!(terminal["outcome"], json!({"type":"cancelled"}));
    assert_eq!(terminal["ran"], "none");
    runtime.stop_job(JOB, true, false).unwrap();
    let mut host = runtime.host.lock().unwrap();
    assert!(host.begin_job_boot(JOB2).unwrap().is_none());
    assert!(host.snapshot.per_job.executions[JOB2].ended);
    assert!(!host.snapshot.per_job.executions[JOB2].boot_started);
    assert!(!host.snapshot.leases[JOB2].delivered);
    assert!(fake.job_starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn interrupted_upload_cancel_preserves_already_delivered_terminal() {
    let (runtime, _fake, _storage, c) = interrupted_before_boot().await;
    let mut client = CutClient::new(
        &[hello(), json!({"type":"attach","job_id":JOB2,"from_seq":0})],
        Cut::Read(ErrorKind::UnexpectedEof),
    );
    live(&runtime, &c, &mut client).await.unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB2].delivered);
    let original = runtime.host.lock().unwrap().snapshot.billing[JOB2]
        .terminal
        .clone()
        .unwrap();
    assert_eq!(cancel_and_attach(&runtime, &c).await, original);
}

#[tokio::test]
async fn interrupted_upload_cancel_preserves_usage_already_reported() {
    let (runtime, _fake, _storage, c) = interrupted_before_boot().await;
    {
        let mut host = runtime.host.lock().unwrap();
        let mut next = host.snapshot.clone();
        next.billing.get_mut(JOB2).unwrap().enqueued = true;
        host.save(next).unwrap();
    }
    let original = runtime.host.lock().unwrap().snapshot.billing[JOB2]
        .terminal
        .clone()
        .unwrap();
    assert_eq!(cancel_and_attach(&runtime, &c).await, original);
}

#[tokio::test]
async fn interrupted_upload_cancel_reconciles_journal_before_replay_commit() {
    let (runtime, _fake, _storage, c) = interrupted_before_boot().await;
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .cancel_interrupted_upload(JOB2)
        .unwrap());
    // Reopen the old replay index after cancellation was saved in host state,
    // as on recovery from a crash before the replacement replay record committed.
    runtime.job_streams.lock().unwrap().remove(JOB2);
    let mut client = CutClient::new(
        &[hello(), json!({"type":"attach","job_id":JOB2,"from_seq":0})],
        Cut::Read(ErrorKind::UnexpectedEof),
    );
    live(&runtime, &c, &mut client).await.unwrap();
    let terminal = client
        .sent
        .iter()
        .find(|frame| frame["type"] == "terminal")
        .unwrap();
    assert_eq!(terminal["outcome"], json!({"type":"cancelled"}));
    assert_eq!(terminal["ran"], "none");
}

#[tokio::test]
async fn interrupted_upload_abrupt_eof_keeps_result_unobserved_until_attach() {
    let (runtime, _fake, _storage, c) = interrupted_before_boot_with(Cut::AbruptEof).await;
    assert!(!runtime.host.lock().unwrap().snapshot.billing[JOB2].delivered);
    let mut client = CutClient::new(
        &[hello(), json!({"type":"attach","job_id":JOB2,"from_seq":0})],
        Cut::Read(ErrorKind::UnexpectedEof),
    );
    live(&runtime, &c, &mut client).await.unwrap();
    let terminal = client
        .sent
        .iter()
        .find(|frame| frame["type"] == "terminal")
        .unwrap();
    assert_eq!(
        terminal["outcome"],
        json!({"type":"refused_before_start","reason":"transfer_interrupted"})
    );
    assert_eq!(terminal["ran"], "none");
    assert!(client
        .sent
        .iter()
        .any(|frame| frame["type"] == "hosted_terminal"));
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB2].delivered);
}

#[tokio::test]
async fn upload_cut_after_boot_replays_no_start_to_early_and_late_attach() {
    let (runtime, fake, _storage) = fixture();
    hold_boot(&runtime, &fake, "unused");
    let (host_end, runner_end) = UnixStream::pair().unwrap();
    fake.scripted.lock().unwrap().push_back(host_end);
    runner_end.set_nonblocking(true).unwrap();
    let mut runner = tokio::net::UnixStream::from_std(runner_end).unwrap();

    let run_claims = claims(JOB);
    let run_id = claim(&runtime, &run_claims);
    let (mut caller, original) = start(runtime.clone(), run_claims.clone(), run_id).await;
    let mut attach = run_claims;
    attach.purpose = Purpose::Attach;
    let attach_frame = json!({"type":"attach","job_id":JOB,"from_seq":0});
    let mut request = run(JOB);
    request["bundles"] = json!([{
        "index":0, "repository":request["repository_root"], "length":6, "sha256":"0".repeat(64)
    }]);
    request["siblings"] = json!([]);
    request["git_dependencies"] = json!([]);
    let chunk = json!({"type":"bundle_chunk","index":0,"offset":0,"data":"YWJj"});
    let expected = json!({"type":"refused_before_start","reason":"transfer_interrupted"});

    tokio::time::timeout(Duration::from_secs(15), async {
        next_type(&mut caller, "lease_ok").await;
        write_frame(&mut caller, &hello()).await.unwrap();
        next_type(&mut caller, "hello").await;
        write_frame(&mut caller, &request).await.unwrap();
        next_type(&mut caller, "accepted").await;

        next_type(&mut runner, "verified_lease").await;
        next_type(&mut runner, "hello").await;
        // Boot has reached the runner, but the host has not authorized Run delivery yet.
        let early_id = claim(&runtime, &attach);
        let (mut early, early_task) = start(runtime.clone(), attach.clone(), early_id).await;
        next_type(&mut early, "lease_ok").await;
        // If the host already refused this attach, read the refusal even if
        // the hello write fails because the peer closed.
        let _ = write_frame(&mut early, &hello()).await;
        let early_hello = read_frame(&mut early).await.unwrap();
        if early_hello["type"] == "hello" {
            write_frame(&mut early, &attach_frame).await.unwrap();
        }

        write_frame(&mut runner, &hello()).await.unwrap();
        assert_eq!(next_type(&mut runner, "run").await, request);
        write_frame(&mut caller, &chunk).await.unwrap();
        assert_eq!(next_type(&mut runner, "bundle_chunk").await, chunk);
        // Cut the connection inside the next frame. The runner loses the
        // upload without sending its own terminal, as on a relay failure.
        caller.write_all(&8u32.to_be_bytes()).await.unwrap();
        caller.write_all(b"{").await.unwrap();
        drop(caller);
        original.await.unwrap().unwrap();
        runtime.host.lock().unwrap().close(run_id).unwrap();
        let eof = loop {
            match read_frame(&mut runner).await {
                Ok(frame) => assert_eq!(frame["type"], "heartbeat"),
                Err(e) => break e,
            }
        };
        assert_eq!(
            eof.downcast_ref::<std::io::Error>().unwrap().kind(),
            ErrorKind::UnexpectedEof
        );
        drop(runner);

        let early_terminal = if early_hello["type"] == "hello" {
            Some(next_type(&mut early, "terminal").await)
        } else {
            None
        };
        early_task.await.unwrap().unwrap();
        runtime.host.lock().unwrap().close(early_id).unwrap();

        while !runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let late_id = claim(&runtime, &attach);
        let (mut late, late_task) = start(runtime.clone(), attach, late_id).await;
        next_type(&mut late, "lease_ok").await;
        write_frame(&mut late, &hello()).await.unwrap();
        next_type(&mut late, "hello").await;
        write_frame(&mut late, &attach_frame).await.unwrap();
        let terminal = next_type(&mut late, "terminal").await;
        assert_eq!(terminal["outcome"], expected);
        assert_eq!(terminal["ran"], "none");
        assert_eq!(early_hello["type"], "hello", "{early_hello}");
        assert_eq!(early_terminal.unwrap(), terminal);
        late_task.await.unwrap().unwrap();
        runtime.host.lock().unwrap().close(late_id).unwrap();
    })
    .await
    .expect("the interrupted upload settles for both attachments");
}
