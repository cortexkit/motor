//! A busy workspace has no second runner to answer its caller. The host answers
//! the queued prelude using the assigned image's capabilities, then replays the
//! reserved spool to the one-job runner once the disk is detached.
use super::*;
use crate::jobs::{Assignment, SPOOL_LIMIT};
use crate::state::jobs::Execution;
use tokio::io::AsyncSeekExt;

#[derive(Default)]
pub(super) struct Launch {
    started: AtomicBool,
    in_flight: AtomicBool,
    cancelled: AtomicBool,
}

impl Runtime {
    pub(super) fn per_job(&self) -> bool {
        matches!(
            self.host.lock().expect("host lock").settings,
            crate::config::HostSettings::PerJob(_)
        )
    }

    pub(super) fn stop_job(&self, job: &str, failed: bool, unclean: bool) -> Result<()> {
        let operations = self
            .job_stops
            .lock()
            .expect("job stops")
            .entry(job.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _operation = operations.lock().expect("job stop");
        let execution = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions
            .get(job)
            .cloned();
        let Some(execution) = execution else {
            return Ok(());
        };
        if !execution.ended {
            if failed {
                // End the lease before answering the caller. Host::delivered
                // cannot authorize any Run once this durable record is ended.
                self.host
                    .lock()
                    .expect("host lock")
                    .job_boot_failed(job, unix_ms())?;
            }
            let launch = self
                .job_launches
                .lock()
                .expect("job launches")
                .get(job)
                .cloned();
            if let Some(launch) = &launch {
                launch.cancelled.store(true, Ordering::SeqCst);
                if launch.in_flight.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
            if execution.boot_started
                && launch
                    .as_ref()
                    .is_none_or(|l| l.started.load(Ordering::Acquire))
            {
                self.control.stop_job(job)?;
            }
            let mut host = self.host.lock().expect("host lock");
            host.job_stopped(job, unclean)?;
        }
        self.job_resources.release(job)
    }

    pub(super) fn recover_jobs(self: &Arc<Self>) -> Result<()> {
        let executions: Vec<_> = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions
            .values()
            .cloned()
            .collect();
        for execution in executions {
            let job = &execution.claims.jti;
            if execution.ended {
                self.job_resources.release(job)?;
                continue;
            }
            let assignment = self.job_resources.acquire(&execution)?;
            remove_spool(&assignment.spool)?;
            if !execution.boot_started {
                self.stop_job(job, true, false)?;
            } else {
                // Never issue start_job for a boot-marked execution, including
                // one whose host died before the helper's reply arrived.
                let running = self.control.adopt_job(job)?;
                let cancelled = self
                    .host
                    .lock()
                    .expect("host lock")
                    .snapshot
                    .leases
                    .get(job)
                    .is_some_and(|lease| lease.state == LeaseState::Ended);
                if !running
                    || cancelled
                    || (!execution.runner_hello
                        && self.boot_remaining(&execution, unix_ms()).is_zero())
                {
                    self.stop_job(job, true, running)?;
                } else {
                    self.start_job_events(job.clone());
                }
            }
        }
        Ok(())
    }

    fn start_job_events(self: &Arc<Self>, job: String) {
        if !self
            .event_workers
            .lock()
            .expect("event workers")
            .insert(job.clone())
        {
            return;
        }
        let runtime = self.clone();
        std::thread::spawn(move || {
            while runtime
                .host
                .lock()
                .expect("host lock")
                .snapshot
                .per_job
                .executions
                .get(&job)
                .is_some_and(|e| !e.ended)
            {
                let result = (|| -> Result<()> {
                    let mut stream = runtime.control.job_relay(&job, 53)?;
                    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                    let mut observation = LifecycleStream::default();
                    loop {
                        let frame = read_json(&mut stream)?;
                        if observation.boundary(&frame)? || frame["type"] == "memory_sample" {
                            continue;
                        }
                        ensure!(frame["type"] == "event", "invalid job lifecycle frame");
                        let event: motor_protocol::TenantEvent = serde_json::from_value(frame)?;
                        ensure!(
                            event.jti.is_none_or(|id| id.to_string() == job),
                            "foreign job lifecycle frame"
                        );
                        runtime.host.lock().expect("host lock").event_observed(
                            &job,
                            &event,
                            unix_ms(),
                            observation.is_live(event.event_seq),
                        )?;
                        write_json(
                            &mut stream,
                            &json!({"type":"ack","event_seq":event.event_seq}),
                        )?;
                    }
                })();
                if let Err(error) = result {
                    tracing::debug!(jti = job, %error, "job_lifecycle_detached");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            runtime
                .event_workers
                .lock()
                .expect("event workers")
                .remove(&job);
        });
    }

    fn boot_remaining(&self, execution: &Execution, now_ms: u64) -> Duration {
        let host = self.host.lock().expect("host lock");
        let crate::config::HostSettings::PerJob(settings) = &host.settings else {
            return Duration::ZERO;
        };
        Duration::from_millis(execution.boot_started_at_ms.map_or(0, |at| {
            at.saturating_add(settings.boot_deadline_ms)
                .saturating_sub(now_ms)
        }))
    }

    pub(super) fn tick_jobs(&self, now_ms: u64) -> Result<()> {
        let due: Vec<_> = {
            let host = self.host.lock().expect("host lock");
            let crate::config::HostSettings::PerJob(settings) = &host.settings else {
                return Ok(());
            };
            host.snapshot
                .per_job
                .executions
                .values()
                .filter(|e| {
                    !e.ended
                        && ((e.boot_started
                            && !e.runner_hello
                            && e.boot_started_at_ms.is_none_or(|at| {
                                now_ms >= at.saturating_add(settings.boot_deadline_ms)
                            }))
                            || (!e.boot_started && now_ms >= e.claims.exp.saturating_mul(1000)))
                })
                .map(|e| e.claims.jti.clone())
                .collect()
        };
        for job in due {
            self.stop_job(&job, true, true)?;
        }
        let ended: Vec<_> = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions
            .iter()
            .filter(|(_, e)| e.ended)
            .map(|(job, _)| job.clone())
            .collect();
        // Retry a manager failure after the VM's stopped record was committed.
        // Release is durable and idempotent, including across host restarts.
        for job in ended {
            self.job_resources.release(&job)?;
        }
        self.host.lock().expect("host lock").billing_tick(now_ms)?;
        Ok(())
    }
}

fn remove_spool(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

struct Spool {
    path: std::path::PathBuf,
    file: tokio::fs::File,
    bytes: u64,
}
impl Spool {
    fn create(path: &std::path::Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            path: path.to_owned(),
            file: tokio::fs::File::from_std(file),
            bytes: 0,
        })
    }
    async fn append(&mut self, frame: &Raw) -> Result<()> {
        let bytes = self.bytes.saturating_add(frame.bytes.len() as u64);
        ensure!(bytes <= SPOOL_LIMIT, "queued upload exceeds reserved spool");
        self.file.write_all(&frame.bytes).await?;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = remove_spool(&self.path);
    }
}

struct Raw {
    bytes: Vec<u8>,
    value: Value,
}
#[derive(Default)]
struct Reader {
    buffered: Vec<u8>,
}
impl Reader {
    // `read` is cancellation-safe; already consumed bytes stay here when a
    // heartbeat or the boot future wins a select while a frame is fragmented.
    async fn next<S: AsyncRead + Unpin>(&mut self, stream: &mut S) -> Result<Raw> {
        loop {
            if self.buffered.len() >= 4 {
                let len = u32::from_be_bytes(self.buffered[..4].try_into().unwrap()) as usize;
                ensure!(len > 0 && len <= FRAME_LIMIT, "invalid frame length");
                if self.buffered.len() >= len + 4 {
                    let bytes: Vec<_> = self.buffered.drain(..len + 4).collect();
                    let value = serde_json::from_slice(&bytes[4..])?;
                    return Ok(Raw { bytes, value });
                }
            }
            let mut buffer = [0; 8192];
            let n = stream.read(&mut buffer).await?;
            ensure!(n > 0, "client closed before delivery");
            self.buffered.extend_from_slice(&buffer[..n]);
        }
    }
}
async fn raw<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Raw> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    ensure!(len > 0 && len <= FRAME_LIMIT, "invalid frame length");
    let mut bytes = vec![0; len + 4];
    bytes[..4].copy_from_slice(&header);
    stream.read_exact(&mut bytes[4..]).await?;
    let value = serde_json::from_slice(&bytes[4..])?;
    Ok(Raw { bytes, value })
}

fn refusal(job: &str, reason: &str, detail: &str) -> Value {
    json!({"type":"terminal","job_id":job,"outcome":{"type":"refused_before_start","reason":reason},
        "wall_ms":0,"queue_wait_ms":0,"bundle_bytes":0,"ran":"none","tree_hash":null,
        "workspace_changes":null,"refusal_detail":detail})
}

struct QueueClock {
    at: tokio::time::Instant,
    elapsed_ms: u64,
}
impl QueueClock {
    fn new() -> Self {
        Self {
            at: tokio::time::Instant::now(),
            elapsed_ms: 0,
        }
    }
    fn observe(&mut self, capacity_wait: bool) -> u64 {
        let now = tokio::time::Instant::now();
        if capacity_wait {
            self.elapsed_ms = self
                .elapsed_ms
                .saturating_add(now.duration_since(self.at).as_millis() as u64);
        }
        self.at = now;
        self.elapsed_ms
    }
}

async fn failed<S: AsyncWrite + Unpin>(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    client: &mut S,
) -> Result<()> {
    let job = claims.jti.clone();
    let r = runtime.clone();
    tokio::task::spawn_blocking(move || r.stop_job(&job, true, true)).await??;
    let frames = runtime
        .host
        .lock()
        .expect("host lock")
        .host_settled_frames(&claims.jti, 0);
    if let Some(frames) = frames {
        for frame in frames {
            write_frame(client, &frame).await?;
        }
    } else {
        write_frame(
            client,
            &json!({"type":"lease_refused","reason":"internal_failure"}),
        )
        .await?;
    }
    Ok(())
}

async fn boot(
    runtime: &Arc<Runtime>,
    execution: &Execution,
    assignment: &Assignment,
    id: u64,
    hello: &Raw,
) -> Result<tokio::net::UnixStream> {
    let resources = runtime.job_resources.clone();
    let e = execution.clone();
    tokio::task::spawn_blocking(move || resources.prepare(&e)).await??;
    ensure!(
        runtime.host.lock().expect("host lock").snapshot.leases[&execution.claims.jti].state
            != LeaseState::Ended,
        "job cancelled before launch"
    );
    let start = motor_vm_helper::job::Start {
        kind: motor_vm_helper::job::VmKind::Job,
        job_id: execution.claims.jti.clone(),
        account: execution.claims.sub.clone(),
        workspace_disk_id: execution.disk_id.clone(),
        image_generation_id: assignment.image_generation_id.clone(),
        vcpus: execution.claims.vcpus,
        memory_mib: execution.guest_ram_mib,
        burst_mib: execution.claims.mem_burst_mib,
    };
    let control = runtime.control.clone();
    let launch = runtime.job_launches.lock().expect("job launches")[&execution.claims.jti].clone();
    launch.started.store(true, Ordering::Release);
    launch.in_flight.store(true, Ordering::SeqCst);
    let r = runtime.clone();
    let reply = tokio::task::spawn_blocking(move || {
        let result = control.start_job(&start);
        launch.in_flight.store(false, Ordering::SeqCst);
        if launch.cancelled.load(Ordering::SeqCst) {
            tracing::warn!(jti = start.job_id, "cancelled_job_start_completed_late");
            r.stop_job(&start.job_id, true, true)?;
        }
        result
    })
    .await??;
    ensure!(
        runtime.host.lock().expect("host lock").snapshot.leases[&execution.claims.jti].state
            != LeaseState::Ended,
        "job cancelled during launch"
    );
    let tap = reply["tap_index"]
        .as_u64()
        .filter(|n| (1..=254).contains(n))
        .ok_or_else(|| anyhow::anyhow!("missing job network allocation"))?;
    let mut request = assignment.guest_init.clone();
    request["op"] = json!("attach_tenant");
    request["generation"] = json!(1);
    request["unix_ms"] = json!(unix_ms());
    request["seed"] = json!(motor_lease::base64url(&rand::random::<[u8; 32]>()));
    request["address"] = json!(format!("10.200.{tap}.2/30"));
    request["gateway"] = json!(format!("10.200.{tap}.1"));
    // Recovery never belongs to the job boot budget or a job's guest attach.
    ensure!(
        request.get("recovery_required").is_none() && request.get("recovery_budget_ms").is_none(),
        "job boot cannot recover disks"
    );
    let control = runtime.control.clone();
    let job = execution.claims.jti.clone();
    let initialized =
        tokio::task::spawn_blocking(move || control.job_admin(&job, &request)).await??;
    ensure!(
        initialized["ok"] == true,
        "job runner initialization refused"
    );
    runtime.start_job_events(execution.claims.jti.clone());
    let control = runtime.control.clone();
    let job = execution.claims.jti.clone();
    let stream = tokio::task::spawn_blocking(move || control.job_relay(&job, 52)).await??;
    stream.set_nonblocking(true)?;
    let mut stream = tokio::net::UnixStream::from_std(stream)?;
    write_frame(&mut stream, &verified_lease(&execution.claims, true)).await?;
    stream.write_all(&hello.bytes).await?;
    let actual = loop {
        let actual = read_frame(&mut stream).await?;
        if actual["type"] != "heartbeat" {
            break actual;
        }
    };
    ensure!(
        crate::jobs::hello_covers(&actual, &promised_hello(assignment, &hello.value)),
        "assigned image capabilities changed"
    );
    runtime
        .host
        .lock()
        .expect("host lock")
        .job_runner_hello(&execution.claims.jti)?;
    ensure!(
        runtime.host.lock().expect("host lock").connection_live(id),
        "connection closed before delivery"
    );
    Ok(stream)
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin + Send>(
    runtime: Arc<Runtime>,
    claims: &Claims,
    id: u64,
    client: &mut S,
) -> Result<()> {
    write_frame(client, &json!({"type":"lease_ok"})).await?;
    let execution = runtime
        .host
        .lock()
        .expect("host lock")
        .snapshot
        .per_job
        .executions[&claims.jti]
        .clone();
    if execution.boot_started && !execution.ended {
        let delivered =
            runtime.host.lock().expect("host lock").snapshot.leases[&claims.jti].delivered;
        let cancelled = runtime.host.lock().expect("host lock").snapshot.leases[&claims.jti].state
            == LeaseState::Ended;
        if cancelled || !execution.runner_hello || !delivered {
            write_frame(client, &json!({"type":"lease_refused","reason":"auth"})).await?;
            return Ok(());
        }
        // A reconnect owns no launch authority. Use the existing VM by job id;
        // failure to attach must not stop a job another caller may be observing.
        return live(&runtime, claims, client).await;
    }
    if claims.purpose == Purpose::Attach {
        let first = read_frame(client).await?;
        let terminal = AtomicBool::new(false);
        return serve_stored(&runtime, &claims.jti, &first, client, &terminal).await;
    }
    let result = session(&runtime, claims, id, client).await;
    if let Err(error) = result {
        tracing::warn!(jti = claims.jti, %error, "job_boot_or_delivery_failed");
        if matches!(
            error.to_string().as_str(),
            "assigned image capabilities changed" | "queued upload exceeds reserved spool"
        ) {
            let reason = if error.to_string() == "assigned image capabilities changed" {
                "request_rejected"
            } else {
                "transfer_interrupted"
            };
            let frame = refusal(&claims.jti, reason, &error.to_string());
            runtime.host.lock().expect("host lock").job_refused(
                &claims.jti,
                frame.clone(),
                unix_ms(),
            )?;
            let r = runtime.clone();
            let job = claims.jti.clone();
            tokio::task::spawn_blocking(move || r.stop_job(&job, true, false)).await??;
            write_frame(client, &frame).await?;
            return Ok(());
        }
        failed(&runtime, claims, client).await?;
    }
    Ok(())
}

async fn live<S: AsyncRead + AsyncWrite + Unpin>(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    client: &mut S,
) -> Result<()> {
    let first = raw(client).await?;
    ensure!(first.value["type"] == "hello", "expected reconnect hello");
    let control = runtime.control.clone();
    let job = claims.jti.clone();
    let stream = tokio::task::spawn_blocking(move || control.job_relay(&job, 52)).await??;
    stream.set_nonblocking(true)?;
    let mut guest = tokio::net::UnixStream::from_std(stream)?;
    write_frame(&mut guest, &verified_lease(claims, true)).await?;
    guest.write_all(&first.bytes).await?;
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut gr, mut gw) = guest.split();
    tokio::select! {
        result = tokio::io::copy(&mut cr,&mut gw) => { result?; },
        result = tokio::io::copy(&mut gr,&mut cw) => { result?; },
    }
    Ok(())
}

async fn session<S: AsyncRead + AsyncWrite + Unpin + Send>(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    id: u64,
    client: &mut S,
) -> Result<()> {
    let execution = runtime
        .host
        .lock()
        .expect("host lock")
        .snapshot
        .per_job
        .executions[&claims.jti]
        .clone();
    ensure!(
        !execution.boot_started && !execution.ended,
        "execution already launched"
    );
    let assignment = runtime.job_resources.acquire(&execution)?;
    let mut spool = Spool::create(&assignment.spool)?;
    let expires = tokio::time::Instant::now()
        + Duration::from_millis(claims.exp.saturating_mul(1000).saturating_sub(unix_ms()));
    let mut reader = Reader::default();
    let hello = tokio::time::timeout_at(expires, reader.next(client)).await??;
    ensure!(hello.value["type"] == "hello", "expected hello");
    spool.append(&hello).await?;
    write_frame(client, &promised_hello(&assignment, &hello.value)).await?;
    let mut have_replies = 0usize;
    let mut withdraw_replies = 0usize;
    let mut accepted = false;
    let mut queue_clock = QueueClock::new();
    let mut queue_limit_ms = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    loop {
        let launch = if accepted {
            runtime
                .host
                .lock()
                .expect("host lock")
                .begin_job_boot_at(&claims.jti, unix_ms())?
        } else {
            None
        };
        if let Some(execution) = launch {
            runtime
                .job_launches
                .lock()
                .expect("job launches")
                .insert(claims.jti.clone(), Arc::new(Launch::default()));
            let deadline =
                tokio::time::Instant::now() + runtime.boot_remaining(&execution, unix_ms());
            let starting = boot(runtime, &execution, &assignment, id, &hello);
            tokio::pin!(starting);
            let mut guest = loop {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => anyhow::bail!("job boot deadline exceeded"),
                    result = &mut starting => break result?,
                    frame = reader.next(client) => {
                        let frame = frame?;
                        spool.append(&frame).await?;
                        if cancel(runtime, claims, client, &frame.value, queue_clock.observe(false), queue_limit_ms).await? { return Ok(()); }
                        if frame.value["type"] == "withdraw" { withdraw_replies += 1; }
                    },
                    _ = heartbeat.tick() => write_frame(client, &json!({"type":"heartbeat","queue_wait_ms":queue_clock.observe(false)})).await?,
                }
            };
            spool.file.flush().await?;
            spool
                .file
                .seek(std::io::SeekFrom::Start(hello.bytes.len() as u64))
                .await?;
            let (mut input, mut output) = guest.split();
            // Split the client before replay so runner output cannot deadlock a
            // large queued upload against a full guest socket.
            let (mut cr, mut cw) = tokio::io::split(client);
            let replay = async {
                let mut remaining = spool.bytes.saturating_sub(hello.bytes.len() as u64);
                while remaining > 0 {
                    let frame = raw(&mut spool.file).await?;
                    remaining = remaining
                        .checked_sub(frame.bytes.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("corrupt queued spool"))?;
                    if frame.value["type"] == "run" {
                        // The guest has no job before Run. Record delivery
                        // before the first byte that could authorize execution.
                        runtime.host.lock().expect("host lock").delivered(id)?;
                    }
                    output.write_all(&frame.bytes).await?;
                }
                remove_spool(&spool.path)?;
                output.write_all(&reader.buffered).await?;
                tokio::io::copy(&mut cr, &mut output).await?;
                Ok::<(), anyhow::Error>(())
            };
            let receive = async {
                let mut suppress_accepted = true;
                loop {
                    let frame = raw(&mut input).await?;
                    match frame.value["type"].as_str() {
                        Some("have_reply") if have_replies > 0 => {
                            have_replies -= 1;
                            continue;
                        }
                        Some("withdraw_reply") if withdraw_replies > 0 => {
                            withdraw_replies -= 1;
                            continue;
                        }
                        Some("accepted") if suppress_accepted => {
                            suppress_accepted = false;
                            continue;
                        }
                        Some("terminal") => {
                            // Retain the terminal in the existing billing record.
                            // Do not acknowledge power-off here: this relay does
                            // not durably store the preceding output frames.
                            let mut host = runtime.host.lock().expect("host lock");
                            host.receive_terminal(&claims.jti, frame.value.clone(), unix_ms())?;
                        }
                        _ => {}
                    }
                    cw.write_all(&frame.bytes).await?;
                    if frame.value["type"] == "terminal" {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
            };
            tokio::select! { r = replay => r?, r = receive => r? }
            return Ok(());
        }
        tokio::select! {
            _ = tokio::time::sleep_until(expires), if !accepted => anyhow::bail!("queued lease expired"),
            frame = reader.next(client) => {
                let frame = frame?;
                if spool.append(&frame).await.is_err() {
                    anyhow::bail!("queued upload exceeds reserved spool");
                }
                if cancel(runtime, claims, client, &frame.value, queue_clock.observe(false), queue_limit_ms).await? { return Ok(()); }
                if frame.value["type"] == "withdraw" { withdraw_replies += 1; }
                match frame.value["type"].as_str() {
                    Some("have") if !accepted => {
                        let mut repositories = frame.value["repositories"].clone();
                        if let Some(repositories) = repositories.as_array_mut() { for repository in repositories { repository["commits"] = json!([]); } }
                        let dependencies: Vec<_> = frame.value["git_dependencies"].as_array().into_iter().flatten().map(|url| json!({"url":url,"commits":[]})).collect();
                        write_frame(client, &json!({"type":"have_reply","repositories":repositories,"git_dependencies":dependencies})).await?;
                        have_replies += 1;
                    },
                    Some("run") if !accepted => {
                        let motor_protocol::Frame::Run(run) = serde_json::from_value(frame.value)? else { unreachable!() };
                        ensure!(run.job_id.to_string() == claims.jti
                            && run.weight_hint == Some(claims.vcpus)
                            && run.network.as_ref().is_some_and(|n| n == &motor_protocol::Network::Outbound) == claims.net
                            && run.platform.as_ref().is_none_or(|p| p == &motor_protocol::Platform::Linux)
                            && run.timeout.is_some_and(|s| s <= claims.max_s), "queued Run does not match lease");
                        queue_clock = QueueClock::new();
                        queue_limit_ms = Some(run.queue_wait_limit_s.saturating_mul(1000));
                        let position = {
                            let host = runtime.host.lock().expect("host lock");
                            host.snapshot.per_job.disks[&execution.disk_id].queue.iter().position(|j| j == &claims.jti).unwrap_or(0)
                        };
                        let mut reply = motor_protocol::Accepted::new(run.job_id, position as u32).with_platform(motor_protocol::Platform::Linux);
                        if let Some(network) = run.network { reply = reply.with_network(network); }
                        write_frame(client, &serde_json::to_value(motor_protocol::Frame::Accepted(reply))?).await?;
                        accepted = true;
                    },
                    Some("bundle_chunk") if accepted => {},
                    Some("heartbeat") => {},
                    Some("withdraw") => {},
                    _ => anyhow::bail!("unexpected queued frame"),
                }
            },
            _ = heartbeat.tick() => {
                // Same-workspace ownership and VM setup are not capacity wait.
                // This host refuses capacity at claim rather than queueing it.
                let wait = accepted.then(|| queue_clock.observe(false));
                write_frame(client, &json!({"type":"heartbeat","queue_wait_ms":wait})).await?;
            },
        }
    }
}

fn promised_hello(assignment: &Assignment, client: &Value) -> Value {
    let mut hello = assignment.hello.clone();
    for capability in ["started", "default_branch_tip", "gate_jobs"] {
        hello[capability] = json!(hello[capability] == true && client[capability] == true);
    }
    hello
}

async fn cancel<S: AsyncWrite + Unpin>(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    client: &mut S,
    frame: &Value,
    elapsed_ms: u64,
    queue_limit_ms: Option<u64>,
) -> Result<bool> {
    if !matches!(frame["type"].as_str(), Some("withdraw" | "cancel")) {
        return Ok(false);
    }
    ensure!(frame["job_id"] == claims.jti, "wrong queued job");
    if frame["type"] == "withdraw" && !queue_limit_ms.is_some_and(|limit| elapsed_ms >= limit) {
        write_frame(
            client,
            &json!({"type":"withdraw_reply","outcome":"started"}),
        )
        .await?;
        return Ok(false);
    }
    let mut terminal = refusal(
        &claims.jti,
        "queue_wait_exceeded",
        "workspace queue withdrawn before start",
    );
    if frame["type"] == "cancel" {
        terminal["outcome"] = json!({"type":"cancelled"});
    }
    if frame["spill"] == true {
        terminal["refusal_detail"] = json!(format!(
            "{}workspace queue withdrawn before start",
            motor_protocol::SPILL_WITHDRAWAL_DETAIL_PREFIX
        ));
    }
    runtime.host.lock().expect("host lock").job_refused(
        &claims.jti,
        terminal.clone(),
        unix_ms(),
    )?;
    let r = runtime.clone();
    let job = claims.jti.clone();
    tokio::task::spawn_blocking(move || r.stop_job(&job, true, false)).await??;
    if frame["type"] == "withdraw" {
        write_frame(
            client,
            &json!({"type":"withdraw_reply","outcome":"withdrawn"}),
        )
        .await?;
    } else {
        write_frame(
            client,
            &json!({"type":"cancel_reply","job_id":claims.jti,"state":"cancelled"}),
        )
        .await?;
    }
    write_frame(client, &terminal).await?;
    let hosted = runtime
        .host
        .lock()
        .expect("host lock")
        .snapshot
        .billing
        .get(&claims.jti)
        .and_then(|b| b.hosted_terminal.clone());
    if let Some(hosted) = hosted {
        write_frame(client, &hosted).await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
