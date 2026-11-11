//! One host-owned runner connection feeds durable replay, independently of any
//! caller socket. Callers read committed frames and submit input to that owner.
use super::*;
use crate::replay::Store;
use crate::state::jobs::storage::ReplayCompletion;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

/// Longest gap between host writes to a job VM's runner channel, well inside
/// the runner's 60-second wire inactivity limit.
pub(super) const GUEST_HEARTBEAT: Duration = Duration::from_secs(5);

/// Longest gap between host writes to an attached caller, inside ck-motor's
/// 60-second read inactivity limit. Heartbeats are live-only, not replay records.
pub(super) const CLIENT_HEARTBEAT: Duration = Duration::from_secs(10);

struct Input {
    bytes: Vec<u8>,
    reply: Option<oneshot::Sender<Vec<u8>>>,
}

pub(crate) struct Stream {
    job: String,
    host: Arc<Mutex<Host>>,
    store: Mutex<std::result::Result<Store, String>>,
    input: Mutex<Option<mpsc::Sender<Input>>>,
    runner_heartbeat: watch::Sender<Option<Vec<u8>>>,
    running: AtomicBool,
    upload_detached: AtomicBool,
    terminal_delivered: AtomicBool,
    #[cfg(test)]
    crash: AtomicBool,
    #[cfg(test)]
    export_limit: Mutex<Option<Duration>>,
}
impl Stream {
    fn export_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(limit) = *self.export_limit.lock().unwrap() {
            return limit;
        }
        Duration::from_secs(motor_protocol::FILL_SOURCE_EXPORT_TIMEOUT_S)
    }
    #[cfg(test)]
    pub(super) fn set_export_timeout(&self, limit: Duration) {
        *self.export_limit.lock().unwrap() = Some(limit);
    }
    pub(super) fn expire_chunks(&self, now_ms: u64) -> Result<()> {
        let end = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .leases
            .get(&self.job)
            .and_then(|lease| lease.ended_at_ms);
        if end.is_some_and(|end| now_ms.saturating_sub(end) >= crate::billing::RECORD_RETENTION_MS)
        {
            // A quarantined replay keeps its files as evidence and has no
            // open store to expire. Reporting that as an error would fail
            // every host tick for as long as the stream stays loaded.
            if self.store.lock().expect("replay store").is_err() {
                return Ok(());
            }
            self.with_store(Store::expire_chunks)?;
        }
        Ok(())
    }
    pub(super) fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let mut store = self.store.lock().expect("replay store");
        let already_quarantined = store.is_err();
        let result = match &mut *store {
            Ok(store) => f(store),
            Err(error) => Err(anyhow::anyhow!("quarantined job replay: {error}")),
        };
        let completion = if let Err(error) = &result {
            if !already_quarantined {
                *store = Err(error.to_string());
            }
            Some(ReplayCompletion::Quarantined)
        } else if store.as_ref().is_ok_and(|store| store.ended()) {
            Some(ReplayCompletion::Terminal)
        } else {
            None
        };
        if let Some(completion) = completion {
            self.host
                .lock()
                .expect("host lock")
                .job_replay_closed(&self.job, completion)?;
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
        let quarantined = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions[job]
            .replay_completion
            == Some(ReplayCompletion::Quarantined);
        let store = (if quarantined {
            Err(anyhow::anyhow!("job replay is durably quarantined"))
        } else {
            Store::open(&directory, job)
        })
        .and_then(|mut store| {
            publish_to_store(self, job, &mut store)?;
            let host = self.host.lock().expect("host lock");
            ensure!(
                store.ended()
                    || !host.snapshot.per_job.executions[job].ended
                    || host.host_settled_frames(job, 0).is_some(),
                "finished job is missing its replay terminal"
            );
            // A Run the host relayed without return paths can never produce
            // returned chunks, so a reopened store accepts none.
            if host.snapshot.per_job.executions[job].relayed_return_paths == Some(false) {
                store.set_return_limit(0)?;
            }
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
        let completion = match &store {
            Err(_) => Some(ReplayCompletion::Quarantined),
            Ok(store) if store.ended() => Some(ReplayCompletion::Terminal),
            _ => None,
        };
        if let Some(completion) = completion {
            self.host
                .lock()
                .expect("host lock")
                .job_replay_closed(job, completion)?;
        }
        let stream = Arc::new(Stream {
            job: job.to_owned(),
            host: self.host.clone(),
            store: Mutex::new(store),
            input: Mutex::new(None),
            runner_heartbeat: watch::channel(None).0,
            running: AtomicBool::new(false),
            upload_detached: AtomicBool::new(false),
            terminal_delivered: AtomicBool::new(false),
            #[cfg(test)]
            crash: AtomicBool::new(false),
            #[cfg(test)]
            export_limit: Mutex::new(None),
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
    mut commands: mpsc::Receiver<Input>,
) -> Result<()> {
    let resume = guest.is_none();
    if resume {
        let runtime = runtime.clone();
        let job = claims.jti.clone();
        tokio::task::spawn_blocking(move || runtime.sync_remote_cache_write_block(&job, true))
            .await?;
    }
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
    let pending = Mutex::new(BTreeMap::<
        String,
        std::collections::VecDeque<oneshot::Sender<Vec<u8>>>,
    >::new());
    let upload = async {
        let default_branch_tip = prelude.as_ref().is_some_and(|(_, p)| p.default_branch_tip);
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
                    let motor_protocol::Frame::Run(run) =
                        serde_json::from_value(frame.value.clone())?
                    else {
                        unreachable!()
                    };
                    let mut host = runtime.host.lock().expect("host lock");
                    host.job_relayed_run(&claims.jti, !run.return_paths.is_empty())?;
                    if let Err(error) = if default_branch_tip {
                        host.note_fill_source(&claims.jti, &run, unix_ms())
                    } else {
                        Ok(())
                    } {
                        tracing::warn!(jti=claims.jti, %error, "fill_export_setup_failed");
                    }
                    host.delivered(*id)?;
                }
                upload.observe(&frame.value)?;
                write.lock().await.write_all(&frame.bytes).await?;
            }
            remove_spool(&p.spool.path)?;
        }
        // The guest runner closes its channel after 60 s without a byte from
        // the host ("wire inactivity"). In tenant mode ck-motor's heartbeats
        // pass through; here the host answers client heartbeats itself and the
        // client may detach, so the host keeps the guest channel alive on its
        // own. A failed heartbeat only stops heartbeats: a guest that closed
        // its input must not fail the stream before its terminal is read.
        let mut last_write = tokio::time::Instant::now();
        let mut heartbeats = true;
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
                Some(input) = commands.recv() => {
                    let frame = crate::replay::decode(&input.bytes)?;
                    if default_branch_tip && frame["type"] == "run" {
                        if let Ok(motor_protocol::Frame::Run(run)) = serde_json::from_value(frame.clone()) {
                            if let Err(error) = runtime.host.lock().expect("host lock").note_fill_source(&claims.jti, &run, unix_ms()) {
                                tracing::warn!(jti=claims.jti, %error, "fill_export_setup_failed");
                            }
                        }
                    }
                    upload.observe(&frame)?;
                    if let Some(reply) = input.reply {
                        let kind = format!("{}_reply", frame["type"].as_str().context("control type")?);
                        pending.lock().expect("pending replies").entry(kind).or_default().push_back(reply);
                    }
                    write.lock().await.write_all(&input.bytes).await?;
                    last_write = tokio::time::Instant::now();
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            if heartbeats && last_write.elapsed() >= GUEST_HEARTBEAT {
                let beat =
                    write_frame(&mut *write.lock().await, &json!({"type":"heartbeat"})).await;
                if let Err(error) = beat {
                    tracing::info!(jti = claims.jti, %error, "guest_heartbeat_stopped");
                    heartbeats = false;
                }
                last_write = tokio::time::Instant::now();
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
                Some("accepted") => {
                    let limit = runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .job_run_accepted(&claims.jti)?;
                    store.with_store(|s| s.set_return_limit(limit))?;
                    continue;
                }
                Some("hello") => continue,
                Some("heartbeat") => {
                    store.runner_heartbeat.send_replace(Some(frame.bytes));
                    continue;
                }
                _ => {}
            }
            let terminal = frame.value["type"] == "terminal";
            let response = is_reply(&frame.value).then(|| frame.clone());
            let s = store.clone();
            let r = runtime.clone();
            let job = claims.jti.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let seq = frame.value["seq"].as_u64();
                s.with_store(|s| {
                    let delivery = if frame.value["type"] == "terminal" {
                        r.observe_job_frame(&job, &frame.value)?
                            .map(|terminal| host_terminal_delivery(terminal, s.last_seq()))
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
            if let Some(response) = response {
                let reply = pending
                    .lock()
                    .expect("pending replies")
                    .get_mut(response.value["type"].as_str().context("reply type")?)
                    .and_then(std::collections::VecDeque::pop_front);
                if let Some(reply) = reply {
                    let _ = reply.send(response.bytes);
                }
            }
            if terminal {
                let host = runtime.host.clone();
                let control = runtime.control.clone();
                let job = claims.jti.clone();
                let limit = store.export_timeout();
                let deadline = std::time::Instant::now() + limit;
                let due = host
                    .lock()
                    .expect("host lock")
                    .snapshot
                    .per_job
                    .export_tips
                    .contains_key(&job);
                // The terminal is already durable and billing has ended. Do not
                // make a connected caller wait for maintenance before seeing it.
                let export = async {
                    while due
                        && !resume
                        && !store.terminal_delivered.load(Ordering::Acquire)
                        && !store.upload_detached.load(Ordering::Acquire)
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    if !due {
                        return Ok(());
                    }
                    tokio::task::spawn_blocking(move || {
                        crate::jobs::fill_source::export(&host, &*control, &job, deadline)
                    })
                    .await?
                };
                let timed_out = match tokio::time::timeout(limit, export).await {
                    Ok(Ok(())) => false,
                    Ok(Err(error)) => {
                        tracing::warn!(jti=claims.jti, %error, "fill_export_failed");
                        false
                    }
                    Err(_) => {
                        tracing::warn!(jti = claims.jti, "fill_export_timed_out");
                        true
                    }
                };
                {
                    let mut host = runtime.host.lock().expect("host lock");
                    let mut next = host.snapshot.clone();
                    next.per_job.export_tips.remove(&claims.jti);
                    if let Err(error) = host.save(next) {
                        tracing::warn!(%error, "fill_export_release_failed");
                    }
                }
                if !timed_out {
                    // Once acknowledged, the guest drains its reports, unmounts
                    // its disks and ends its own VM: a clean stop, so the disk
                    // is reused without a recovery check. Record that before the
                    // guest can exit. A forced stop later (no acknowledgement,
                    // or a guest that never ends) upgrades it to unclean.
                    runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .job_stop_intent(&claims.jti, false)?;
                }
                let acknowledged = if timed_out {
                    false
                } else {
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                        write_frame(
                            &mut *write.lock().await,
                            &json!({"type":"terminal_stored","jti":claims.jti}),
                        )
                        .await
                    })
                    .await
                    .is_ok_and(|result| result.is_ok())
                };
                if !acknowledged {
                    // The guest may be stuck exporting or no longer reading its
                    // job channel. Its already committed terminal still wins;
                    // a hard stop quarantines only the unfinished disk cleanup.
                    let runtime = runtime.clone();
                    let job = claims.jti.clone();
                    tokio::task::spawn_blocking(move || runtime.stop_job(&job, false, true))
                        .await??;
                }
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

// Host settlement can be committed before the collector drains its final
// output. Freeze the delivered watermark only when committing the terminal to
// replay, using the host's own cursor rather than the guest's terminal claims.
fn host_terminal_delivery(mut terminal: Value, last_seq: Option<u64>) -> Value {
    if terminal["outcome"]["type"] == "outcome_unknown"
        || matches!(
            terminal["oom_kill"]["cause"].as_str(),
            Some("host_pressure" | "host_oom")
        )
    {
        let fields = terminal.as_object_mut().expect("terminal object");
        if let Some(seq) = last_seq {
            fields.insert("last_seq".into(), seq.into());
        } else {
            fields.remove("last_seq");
        }
    }
    terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_terminal_watermark_distinguishes_unknown_from_sequence_zero() {
        for cause in ["host_pressure", "host_oom"] {
            let terminal = json!({"type":"terminal","oom_kill":{"cause":cause},"last_seq":123});
            assert!(host_terminal_delivery(terminal.clone(), None)
                .get("last_seq")
                .is_none());
            assert_eq!(host_terminal_delivery(terminal, Some(0))["last_seq"], 0);
        }
        let terminal = json!({"type":"terminal","last_seq":123});
        assert_eq!(host_terminal_delivery(terminal.clone(), Some(5)), terminal);
    }
}

pub(super) fn publish_settlement(runtime: &Runtime, job: &str, stream: &Stream) -> Result<()> {
    stream.with_store(|store| publish_to_store(runtime, job, store))
}

fn publish_to_store(runtime: &Runtime, job: &str, store: &mut Store) -> Result<()> {
    if store.ended() {
        return Ok(());
    }
    let frames = {
        let mut host = runtime.host.lock().expect("host lock");
        if let Some(frames) =
            host.host_settled_frames(job, store.last_seq().unwrap_or(0).saturating_add(1))
        {
            // These frames already encode proven queued-job withdrawals or
            // outcomes selected by the host's settlement evidence.
            frames
        } else {
            let execution = &host.snapshot.per_job.executions[job];
            if !execution.ended
                || !execution
                    .evidence
                    .as_ref()
                    .is_some_and(|e| e.exit.is_some() || e.evidence_expired)
            {
                return Ok(());
            }
            // Queued-job withdrawal ends an undelivered lease with zero usage,
            // so no Run can later be delivered. Missing Started proves nothing.
            let lease = &host.snapshot.leases[job];
            let never_started = !lease.delivered
                && lease.started_at_ms.is_none()
                && host.snapshot.zero_usage.contains_key(job);
            let terminal = if never_started {
                host.snapshot
                    .billing
                    .get(job)
                    .and_then(|b| b.terminal.clone())
                    .unwrap_or_else(|| {
                        refusal(
                            job,
                            "internal_failure",
                            "execution ended before Run delivery",
                        )
                    })
            } else {
                // An ended VM proves no more output can arrive, not that its
                // delivered Run never started. Missing history stays unknown.
                host_terminal_delivery(
                    json!({"type":"terminal","job_id":job,"outcome":{"type":"outcome_unknown"},"wall_ms":0,"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null}),
                    store.last_seq(),
                )
            };
            let mut next = host.snapshot.clone();
            next.billing.entry(job.into()).or_default().terminal = Some(terminal.clone());
            host.save(next)?;
            vec![terminal]
        }
    };
    for frame in frames {
        if frame["type"] == "hosted_terminal" {
            continue;
        }
        let frame = host_terminal_delivery(frame, store.last_seq());
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
    Ok(())
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
    // Subscribing starts after the latest heartbeat: only a new runner frame
    // can describe the queue state of this caller's live attachment.
    let mut heartbeats = stream.runner_heartbeat.subscribe();
    stream.expire_chunks(unix_ms())?;
    let _owner = (!hello).then(|| UploadOwner(stream.clone()));
    let _ = publish_settlement(runtime, &claims.jti, &stream);
    let mut position = if attached {
        0
    } else {
        stream.with_store(|s| Ok(s.next_id())).unwrap_or(0)
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
    let mut last_write = tokio::time::Instant::now();
    let mut last_runner_heartbeat = None;
    // Bare host heartbeats tell ck-motor to stop timing a queue; send those
    // only once the runner has started the command. Every sequenced record
    // (started, output, returned files) comes after that.
    let mut started = false;
    loop {
        let s = stream.clone();
        let frame = tokio::task::spawn_blocking(move || {
            s.with_store(|s| s.cursor_frame(position, minimum))
        })
        .await?;
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
        if let Some((next, bytes)) = frame {
            let value = crate::replay::decode(&bytes)?;
            position = next;
            started |= value["seq"].is_u64();
            if attached
                && !is_reply(&value)
                && value["seq"].as_u64().is_none_or(|seq| seq >= minimum)
            {
                write
                    .write_all(&bytes)
                    .await
                    .map_err(anyhow::Error::from)
                    .map_err(client_io)?;
                last_write = tokio::time::Instant::now();
                if let Some(seq) = value["seq"].as_u64() {
                    minimum = seq.saturating_add(1);
                }
                if value["type"] == "terminal" {
                    stream.terminal_delivered.store(true, Ordering::Release);
                    // Billing acknowledgements are host metadata, not runner
                    // output. Reuse their durable record after the stored
                    // terminal on live streams and reattached streams alike.
                    let hosted = runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .snapshot
                        .billing
                        .get(&claims.jti)
                        .and_then(|billing| billing.hosted_terminal.clone());
                    if let Some(hosted) = hosted {
                        write_frame(&mut write, &hosted).await?;
                    }
                    return Ok(());
                }
            }
            continue;
        }
        tokio::select! {
            request = reader.next(&mut read) => {
                let request = match request {
                    Ok(request) => request,
                    Err(error) if hello && error.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof) => return Ok(()),
                    Err(error) => return Err(client_io(error)),
                };
                if request.value["type"] == "heartbeat" { continue; }
                // Bundle chunks carry no job id: they continue this connection's
                // own upload, which outlasts the VM boot for a large history.
                if request.value["type"] != "bundle_chunk" {
                    ensure!(request.value["job_id"] == claims.jti, "foreign job request");
                }
                match request.value["type"].as_str() {
                    Some("attach") => {
                        minimum = request.value["from_seq"].as_u64().context("attach cursor")?;
                        position = 0;
                        attached = true;
                        heartbeats.mark_unchanged();
                        last_runner_heartbeat = None;
                    }
                    Some("cancel" | "withdraw" | "job_status") => {
                        let ended = runtime.host.lock().expect("host lock").snapshot.billing.get(&claims.jti).and_then(|b| b.terminal.clone());
                        let kind = request.value["type"].as_str().context("control type")?;
                        if let Some(terminal) = ended {
                            let reply = stored_reply(kind, &claims.jti, terminal)?;
                            write_frame(&mut write, &reply).await?;
                        } else {
                            let tx = stream.input.lock().expect("job input").clone().context("runner disconnected")?;
                            let (reply, response) = oneshot::channel();
                            tx.send(Input { bytes: request.bytes, reply: Some(reply) }).await?;
                            match response.await {
                                Ok(bytes) => write.write_all(&bytes).await?,
                                Err(_) => {
                                    let terminal = runtime.host.lock().expect("host lock").snapshot.billing.get(&claims.jti).and_then(|b| b.terminal.clone()).context("runner disconnected")?;
                                    write_frame(&mut write, &stored_reply(kind, &claims.jti, terminal)?).await?;
                                }
                            }
                        }
                    }
                    Some("bundle_chunk") if attached => {
                        let tx = stream.input.lock().expect("job input").clone().context("runner disconnected")?;
                        tx.send(Input { bytes: request.bytes, reply: None }).await?;
                    }
                    _ => anyhow::bail!("operation_refused"),
                }
            }
            _ = heartbeats.changed(), if attached => {
                let bytes = heartbeats.borrow_and_update().clone();
                if let Some(bytes) = bytes {
                    write.write_all(&bytes).await.map_err(anyhow::Error::from).map_err(client_io)?;
                    last_runner_heartbeat = Some(bytes);
                    last_write = tokio::time::Instant::now();
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                if attached && last_write.elapsed() >= CLIENT_HEARTBEAT {
                    if started {
                        write_frame(&mut write, &json!({"type":"heartbeat"})).await.map_err(client_io)?;
                    } else if let Some(bytes) = &last_runner_heartbeat {
                        write.write_all(bytes).await.map_err(anyhow::Error::from).map_err(client_io)?;
                    } else {
                        // No live runner queue sample has reached this attachment.
                        // Keep the caller's capacity-queue budget paused rather
                        // than clearing its deadline with a bare heartbeat.
                        write_frame(&mut write, &json!({"type":"heartbeat","queue_wait_ms":0})).await.map_err(client_io)?;
                    }
                    last_write = tokio::time::Instant::now();
                }
            }
        }
    }
}

fn is_reply(frame: &Value) -> bool {
    matches!(
        frame["type"].as_str(),
        Some("cancel_reply" | "withdraw_reply" | "job_status_reply")
    )
}

fn stored_reply(kind: &str, job: &str, mut terminal: Value) -> Result<Value> {
    Ok(match kind {
        "cancel" => json!({"type":"cancel_reply","job_id":job}),
        "withdraw" => {
            json!({"type":"withdraw_reply","outcome":if terminal["ran"] == "none" { "withdrawn" } else { "started" }})
        }
        _ => {
            terminal
                .as_object_mut()
                .context("stored terminal")?
                .remove("type");
            json!({"type":"job_status_reply","job_id":job,"state":"ended","terminal":terminal,"unit":null,"unit_alive":false})
        }
    })
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
