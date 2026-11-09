//! One host-owned runner connection feeds durable replay, independently of any
//! caller socket. Callers read committed frames and submit input to that owner.
use super::*;
use crate::replay::Store;
use tokio::sync::mpsc;

pub(crate) struct Stream {
    store: Mutex<std::result::Result<Store, String>>,
    input: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    running: AtomicBool,
    upload_detached: AtomicBool,
    #[cfg(test)]
    crash: AtomicBool,
}
impl Stream {
    pub(super) fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let mut store = self.store.lock().expect("replay store");
        let result = match &mut *store {
            Ok(store) => f(store),
            Err(error) => anyhow::bail!("quarantined job replay: {error}"),
        };
        if let Err(error) = &result {
            *store = Err(error.to_string());
        }
        result
    }
    #[cfg(test)]
    pub(super) fn crash_for_test(&self) {
        self.crash.store(true, Ordering::Release);
    }
    #[cfg(test)]
    pub(super) fn running_for_test(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

impl Runtime {
    pub(super) fn job_stream(&self, job: &str) -> Result<Arc<Stream>> {
        let mut streams = self.job_streams.lock().expect("job streams");
        if let Some(stream) = streams.get(job) {
            return Ok(stream.clone());
        }
        let directory = self
            .host
            .lock()
            .expect("host lock")
            .journal_directory()
            .context("job replay requires a journal directory")?;
        let store = Store::open(&directory, job)
            .and_then(|store| {
                let host = self.host.lock().expect("host lock");
                ensure!(
                    store.ended()
                        || !host.snapshot.per_job.executions[job].ended
                        || host.host_settled_frames(job, 0).is_some(),
                    "finished job is missing its replay terminal"
                );
                drop(host);
                // A file commit can outlive its following state transaction. Restore
                // observations and the watermark before exposing any committed bytes.
                for position in 0..store.len() {
                    let frame =
                        crate::replay::decode(&store.frame(position)?.context("replay frame")?)?;
                    if matches!(frame["type"].as_str(), Some("started" | "terminal")) {
                        self.observe_job_frame(job, &frame)?;
                    }
                }
                if let Some(seq) = store.last_seq() {
                    self.host.lock().expect("host lock").watermark(job, seq)?;
                }
                Ok(store)
            })
            .map_err(|error| {
                tracing::error!(jti = job, %error, "job_replay_quarantined");
                error.to_string()
            });
        let stream = Arc::new(Stream {
            store: Mutex::new(store),
            input: Mutex::new(None),
            running: AtomicBool::new(false),
            upload_detached: AtomicBool::new(false),
            #[cfg(test)]
            crash: AtomicBool::new(false),
        });
        streams.insert(job.to_owned(), stream.clone());
        Ok(stream)
    }

    pub(super) fn resume_job_stream(self: &Arc<Self>, execution: &Execution) -> Result<()> {
        let stream = self.job_stream(&execution.claims.jti)?;
        // An unreadable replay belongs to this job, not to host startup. Never
        // restart its command or authorize a second VM to repair missing output.
        if stream.with_store(|s| Ok(s.ended())).is_err() {
            return Ok(());
        }
        self.launch_job_stream_inner(execution.claims.clone(), None, None)
    }

    pub(super) fn launch_job_stream(
        self: &Arc<Self>,
        claims: Claims,
        guest: std::os::unix::net::UnixStream,
        prelude: Option<(u64, Replay)>,
    ) -> Result<()> {
        self.launch_job_stream_inner(claims, Some(guest), prelude)
    }

    fn launch_job_stream_inner(
        self: &Arc<Self>,
        claims: Claims,
        guest: Option<std::os::unix::net::UnixStream>,
        prelude: Option<(u64, Replay)>,
    ) -> Result<()> {
        let stream = self.job_stream(&claims.jti)?;
        if stream.running.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let (tx, rx) = mpsc::channel(1);
        *stream.input.lock().expect("job input") = Some(tx);
        let runtime = self.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(collect(&runtime, &claims, &stream, guest, prelude, rx))
            })();
            if let Err(error) = result {
                tracing::warn!(jti = claims.jti, %error, "job_runner_channel_failed");
                let timeout = match &runtime.host.lock().expect("host lock").settings {
                    crate::config::HostSettings::PerJob(s) => s.teardown_timeout_ms,
                    _ => unreachable!(),
                };
                std::thread::sleep(Duration::from_millis(timeout));
                if let Err(error) = runtime.stop_job(&claims.jti, true, true) {
                    tracing::error!(jti = claims.jti, %error, "job_teardown_failed");
                }
                if let Err(error) = publish_settlement(&runtime, &claims.jti, &stream) {
                    tracing::error!(jti = claims.jti, %error, "job_settlement_replay_failed");
                }
            }
            stream.running.store(false, Ordering::Release);
            *stream.input.lock().expect("job input") = None;
        });
        Ok(())
    }
}

async fn collect(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    store: &Arc<Stream>,
    guest: Option<std::os::unix::net::UnixStream>,
    mut prelude: Option<(u64, Replay)>,
    mut commands: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let resume = guest.is_none();
    let guest = match guest {
        Some(guest) => guest,
        None => runtime.control.job_relay(&claims.jti, 52)?,
    };
    guest.set_nonblocking(true)?;
    let mut guest = tokio::net::UnixStream::from_std(guest)?;
    if resume {
        let mut lease = verified_lease(claims, true);
        lease["mirror_reports"] = json!(true);
        write_frame(&mut guest, &lease).await?;
        write_frame(&mut guest, &json!({"type":"hello","protocol_version":1,"started":true,"return_paths":true,"return_chunked":true})).await?;
        loop {
            if read_frame(&mut guest).await?["type"] == "hello" {
                break;
            }
        }
        // Resume inclusively from the last stored sequence so duplicate removal
        // also covers a crash after storage but before the watermark transaction.
        let from = store.with_store(|s| Ok(s.last_seq().unwrap_or(0)))?;
        write_frame(
            &mut guest,
            &json!({"type":"attach","job_id":claims.jti,"from_seq":from}),
        )
        .await?;
    }
    let (mut read, write) = guest.split();
    let write = tokio::sync::Mutex::new(write);
    let mut have_replies = prelude.as_ref().map_or(0, |(_, p)| p.have_replies);
    let mut withdraw_replies = prelude.as_ref().map_or(0, |(_, p)| p.withdraw_replies);
    let upload = async {
        let mut upload = Upload::default();
        if let Some((id, p)) = &mut prelude {
            p.spool.file.flush().await?;
            p.spool
                .file
                .seek(std::io::SeekFrom::Start(p.hello_len))
                .await?;
            let mut remaining = p.spool.bytes.saturating_sub(p.hello_len);
            while remaining > 0 {
                let frame = raw(&mut p.spool.file).await?;
                remaining = remaining
                    .checked_sub(frame.bytes.len() as u64)
                    .context("corrupt queued spool")?;
                if frame.value["type"] == "run" {
                    runtime.host.lock().expect("host lock").delivered(*id)?;
                }
                upload.observe(&frame.value)?;
                write.lock().await.write_all(&frame.bytes).await?;
            }
            remove_spool(&p.spool.path)?;
        }
        loop {
            if store.upload_detached.load(Ordering::Acquire)
                && !upload.complete()
                && commands.is_empty()
            {
                // EOF still means transfer_interrupted for an unfinished input
                // upload. A fully uploaded command keeps its independent channel.
                write.lock().await.shutdown().await?;
                return std::future::pending::<Result<()>>().await;
            }
            tokio::select! {
                biased;
                Some(bytes) = commands.recv() => {
                    upload.observe(&crate::replay::decode(&bytes)?)?;
                    write.lock().await.write_all(&bytes).await?;
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    };
    let receive = async {
        loop {
            let frame = raw(&mut read).await?;
            match frame.value["type"].as_str() {
                Some("have_reply") if have_replies > 0 => {
                    have_replies -= 1;
                    continue;
                }
                Some("withdraw_reply") if withdraw_replies > 0 => {
                    withdraw_replies -= 1;
                    continue;
                }
                Some("accepted" | "hello") => continue,
                _ => {}
            }
            let terminal = frame.value["type"] == "terminal";
            let s = store.clone();
            let r = runtime.clone();
            let job = claims.jti.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let seq = frame.value["seq"].as_u64();
                s.with_store(|s| {
                    let delivery = if frame.value["type"] == "terminal" {
                        r.observe_job_frame(&job, &frame.value)?
                            .filter(|terminal| *terminal != frame.value)
                            .map(|terminal| -> Result<Vec<u8>> {
                                let data = serde_json::to_vec(&terminal)?;
                                Ok([(data.len() as u32).to_be_bytes().as_slice(), &data].concat())
                            })
                            .transpose()?
                    } else {
                        None
                    };
                    let appended = s.append_delivery(&frame.bytes, delivery.as_deref())?;
                    let mut host = r.host.lock().expect("host lock");
                    if let Some(seq) = seq {
                        host.watermark(&job, seq)?;
                    }
                    drop(host);
                    if appended && frame.value["type"] != "terminal" {
                        r.observe_job_frame(&job, &frame.value)?;
                    }
                    Ok(())
                })?;
                Ok(())
            })
            .await??;
            if terminal {
                write_frame(
                    &mut *write.lock().await,
                    &json!({"type":"terminal_stored","jti":claims.jti}),
                )
                .await?;
                return Ok::<(), anyhow::Error>(());
            }
        }
    };
    #[cfg(test)]
    let crashed = async {
        while !store.crash.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    #[cfg(not(test))]
    let crashed = std::future::pending::<()>();
    tokio::select! { result = upload => result, result = receive => result, _ = crashed => Ok(()) }
}

fn publish_settlement(runtime: &Runtime, job: &str, stream: &Stream) -> Result<()> {
    stream.with_store(|store| {
        if store.ended() {
            return Ok(());
        }
        let frames = runtime
            .host
            .lock()
            .expect("host lock")
            .host_settled_frames(job, store.last_seq().unwrap_or(0).saturating_add(1));
        if let Some(frames) = frames {
            for frame in frames {
                if frame["type"] == "hosted_terminal" {
                    continue;
                }
                let data = serde_json::to_vec(&frame)?;
                let bytes = [(data.len() as u32).to_be_bytes().as_slice(), &data].concat();
                store.append(&bytes)?;
                if let Some(seq) = frame["seq"].as_u64() {
                    runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .watermark(job, seq)?;
                }
            }
        }
        Ok(())
    })
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    client: &mut S,
    hello: bool,
    mut reader: Reader,
    mut attached: bool,
) -> Result<()> {
    let stream = runtime.job_stream(&claims.jti)?;
    let _owner = (!hello).then(|| UploadOwner(stream.clone()));
    let _ = publish_settlement(runtime, &claims.jti, &stream);
    let mut position = if attached {
        0
    } else {
        stream.with_store(|s| Ok(s.len())).unwrap_or(0)
    };
    let mut minimum = 0;
    if hello {
        ensure!(
            reader.next(client).await?.value["type"] == "hello",
            "expected reconnect hello"
        );
        write_frame(
            client,
            &json!({"type":"hello","protocol_version":1,"started":true}),
        )
        .await?;
    }
    let (mut read, mut write) = tokio::io::split(client);
    loop {
        let s = stream.clone();
        let frame =
            tokio::task::spawn_blocking(move || s.with_store(|s| s.frame(position))).await?;
        let frame = match frame {
            Ok(frame) => frame,
            Err(_) if !attached => None,
            Err(_) => {
                write_frame(
                    &mut write,
                    &json!({"type":"terminal","job_id":claims.jti,"outcome":{"type":"outcome_unknown"},"wall_ms":0,"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null}),
                )
                .await?;
                return Ok(());
            }
        };
        if let Some(bytes) = frame {
            let value = crate::replay::decode(&bytes)?;
            position += 1;
            let reply = matches!(
                value["type"].as_str(),
                Some("cancel_reply" | "withdraw_reply" | "job_status_reply")
            );
            if (attached || reply) && value["seq"].as_u64().is_none_or(|seq| seq >= minimum) {
                write.write_all(&bytes).await?;
                if value["type"] == "terminal" {
                    return Ok(());
                }
            }
            continue;
        }
        tokio::select! {
            request = reader.next(&mut read) => {
                let request = match request {
                    Ok(request) => request,
                    Err(error) if error.to_string() == "client closed before delivery" => return Ok(()),
                    Err(error) => return Err(error),
                };
                if request.value["type"] == "heartbeat" { continue; }
                ensure!(request.value["job_id"] == claims.jti, "foreign job request");
                match request.value["type"].as_str() {
                    Some("attach") => {
                        minimum = request.value["from_seq"].as_u64().context("attach cursor")?;
                        position = stream.with_store(|s| Ok(s.cursor_for_seq(minimum))).unwrap_or(0);
                        attached = true;
                    }
                    Some("cancel" | "withdraw" | "job_status") => {
                        let ended = runtime.host.lock().expect("host lock").snapshot.billing.get(&claims.jti).and_then(|b| b.terminal.clone());
                        if let Some(mut terminal) = ended {
                            let reply = match request.value["type"].as_str().unwrap() {
                                "cancel" => json!({"type":"cancel_reply","job_id":claims.jti}),
                                "withdraw" => json!({"type":"withdraw_reply","outcome":if terminal["ran"] == "none" { "withdrawn" } else { "started" }}),
                                _ => {
                                    terminal.as_object_mut().context("stored terminal")?.remove("type");
                                    json!({"type":"job_status_reply","job_id":claims.jti,"state":"ended","terminal":terminal,"unit":null,"unit_alive":false})
                                }
                            };
                            write_frame(&mut write, &reply).await?;
                        } else {
                            let tx = stream.input.lock().expect("job input").clone().context("runner disconnected")?;
                            tx.send(request.bytes).await?;
                        }
                    }
                    Some("bundle_chunk") if attached => {
                        let tx = stream.input.lock().expect("job input").clone().context("runner disconnected")?;
                        tx.send(request.bytes).await?;
                    }
                    _ => anyhow::bail!("operation_refused"),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

struct UploadOwner(Arc<Stream>);
impl Drop for UploadOwner {
    fn drop(&mut self) {
        self.0.upload_detached.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct Upload {
    expected: Vec<u64>,
    received: Vec<u64>,
}
impl Upload {
    fn complete(&self) -> bool {
        self.expected == self.received
    }
    fn observe(&mut self, frame: &Value) -> Result<()> {
        match frame["type"].as_str() {
            Some("run") => {
                let motor_protocol::Frame::Run(run) = serde_json::from_value(frame.clone())? else {
                    unreachable!()
                };
                self.expected = run
                    .bundles
                    .iter()
                    .chain(
                        run.git_dependencies
                            .iter()
                            .filter_map(|dependency| dependency.bundle.as_ref()),
                    )
                    .map(|bundle| bundle.length)
                    .collect();
                self.received = vec![0; self.expected.len()];
            }
            Some("bundle_chunk") => {
                let motor_protocol::Frame::BundleChunk {
                    index,
                    offset,
                    data,
                } = serde_json::from_value(frame.clone())?
                else {
                    unreachable!()
                };
                if let Some(received) = self.received.get_mut(index as usize) {
                    if *received == offset {
                        *received = received.saturating_add(data.0.len() as u64);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
