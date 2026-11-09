//! A caller negotiates while its runner boots or waits for a busy workspace.
//! The host answers from the assigned image's capabilities and spools the input,
//! then replays it once both the runner Hello and a validated Run are present.
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
    fn launch_phase(&self, job: &str, starts_vm: bool) -> Result<Arc<Launch>> {
        let operations = self
            .job_stops
            .lock()
            .expect("job stops")
            .entry(job.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _operation = operations.lock().expect("job stop");
        ensure!(
            self.host.lock().expect("host lock").snapshot.leases[job].state != LeaseState::Ended,
            "job cancelled before launch"
        );
        let launch = self.job_launches.lock().expect("job launches")[job].clone();
        if starts_vm {
            launch.started.store(true, Ordering::Release);
        }
        launch.in_flight.store(true, Ordering::SeqCst);
        Ok(launch)
    }

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
            let delivered = self.host.lock().expect("host lock").snapshot.leases[job].delivered;
            if failed && !delivered {
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
                self.host
                    .lock()
                    .expect("host lock")
                    .job_stop_intent(job, unclean)?;
                self.control.stop_job(job)?;
                self.collect_job_exits(unix_ms())?;
                // A successful stop call alone is not the durable exit receipt.
                // Retain reservations until receipt/ack, or the evidence bound.
                return Ok(());
            }
            let mut host = self.host.lock().expect("host lock");
            host.job_stopped_at(job, unclean, unix_ms())?;
        }
        self.job_resources.release(job)
    }

    pub(super) fn recover_jobs(self: &Arc<Self>) -> Result<()> {
        {
            let mut host = self.host.lock().expect("host lock");
            let mut next = host.snapshot.clone();
            next.per_job.tenant_rollback_pending = true;
            host.save(next)?;
        }
        self.collect_job_exits(unix_ms())?;
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
                if execution.final_replay_reservation_bytes.is_some() {
                    if let Ok(stream) = self.job_stream(job) {
                        if let Err(error) = stream::publish_settlement(self, job, &stream) {
                            tracing::warn!(jti = job, %error, "job_final_replay_unavailable");
                        }
                    }
                }
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
                let stored_terminal = cancelled
                    && self.job_stream(job).is_ok_and(|stream| {
                        stream
                            .with_store(|store| Ok(store.ended()))
                            .unwrap_or(false)
                    });
                if !running {
                    self.host
                        .lock()
                        .expect("host lock")
                        .job_vmm_gone(job, unix_ms())?;
                } else if (cancelled && !stored_terminal)
                    || (!execution.runner_hello
                        && self.boot_remaining(&execution, unix_ms()).is_zero())
                {
                    self.stop_job(job, true, running)?;
                } else {
                    self.start_job_events(job.clone());
                    if execution.runner_hello
                        && self.host.lock().expect("host lock").snapshot.leases[job].delivered
                    {
                        if let Err(error) = self.resume_job_stream(&execution) {
                            tracing::error!(jti = job, %error, "job_replay_resume_failed");
                        }
                    }
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
                        if frame["type"] == "memory_sample" {
                            let sample: motor_protocol::MemorySample =
                                serde_json::from_value(frame)?;
                            ensure!(sample.jti.to_string() == job, "foreign job memory sample");
                            runtime
                                .host
                                .lock()
                                .expect("host lock")
                                .job_memory_sample(sample)?;
                            continue;
                        }
                        if observation.boundary(&frame)? {
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
        if !self.per_job() {
            return Ok(());
        }
        self.probe_storage();
        let mut failure = self.collect_job_exits(now_ms).err();
        if let Err(error) = self.host.lock().expect("host lock").clean_fill_sources() {
            failure.get_or_insert(error);
        }
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
            if let Err(error) = self.stop_job(&job, true, true) {
                failure.get_or_insert(error);
            }
        }
        let active: Vec<_> = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions
            .values()
            .filter(|e| {
                e.boot_started && !e.ended && e.evidence.as_ref().is_none_or(|e| e.exit.is_none())
            })
            .cloned()
            .collect();
        for execution in active {
            let job = &execution.claims.jti;
            let gone = if execution
                .evidence
                .as_ref()
                .and_then(|e| e.gone_at_ms)
                .is_some()
            {
                true
            } else {
                match self.control.adopt_job(job) {
                    Ok(running) => !running,
                    Err(error) => {
                        failure.get_or_insert(error);
                        false
                    }
                }
            };
            if !gone
                && execution
                    .evidence
                    .as_ref()
                    .is_some_and(|e| e.stop.is_some())
            {
                if let Err(error) = self.stop_job(job, false, true) {
                    failure.get_or_insert(error);
                }
            }
            if gone {
                self.host
                    .lock()
                    .expect("host lock")
                    .job_vmm_gone(job, now_ms)?;
                let expired = {
                    let host = self.host.lock().expect("host lock");
                    let crate::config::HostSettings::PerJob(s) = &host.settings else {
                        unreachable!()
                    };
                    host.snapshot.per_job.executions[job]
                        .evidence
                        .as_ref()
                        .and_then(|e| e.gone_at_ms)
                        .is_some_and(|first| {
                            now_ms >= first.saturating_add(s.exit_evidence_bound_ms)
                        })
                };
                if expired {
                    self.host
                        .lock()
                        .expect("host lock")
                        .job_stopped_at(job, true, now_ms)?;
                }
            }
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
            let expire = {
                let host = self.host.lock().expect("host lock");
                host.snapshot
                    .leases
                    .get(&job)
                    .and_then(|lease| lease.ended_at_ms)
                    .is_some_and(|end| {
                        now_ms.saturating_sub(end) >= crate::billing::RECORD_RETENTION_MS
                    })
                    && host.snapshot.per_job.executions[&job].replay_completion
                        == Some(crate::state::jobs::storage::ReplayCompletion::Terminal)
            };
            if expire {
                self.job_stream(&job)?.expire_chunks(now_ms)?;
            }
        }
        self.host.lock().expect("host lock").billing_tick(now_ms)?;
        let streams: Vec<_> = self
            .job_streams
            .lock()
            .expect("job streams")
            .values()
            .cloned()
            .collect();
        for stream in streams {
            stream.expire_chunks(now_ms)?;
        }
        self.host
            .lock()
            .expect("host lock")
            .retention_tick(now_ms)?;
        if let Err(error) = self.tick_recoveries(now_ms) {
            failure.get_or_insert(error);
        }
        if let Err(error) = self.job_fills.lock().expect("fill scheduler").tick(
            self.host.clone(),
            self.control.clone(),
            self.job_resources.clone(),
            now_ms,
        ) {
            failure.get_or_insert(error);
        }
        self.tick_balloon(now_ms);
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    fn collect_job_exits(&self, now_ms: u64) -> Result<()> {
        let _operation = self.job_exits.lock().expect("job exit collection");
        for report in self.control.job_exits()? {
            let accepted = self
                .host
                .lock()
                .expect("host lock")
                .job_exit_received(&report, now_ms)?;
            if !accepted {
                continue;
            }
            self.finish_job_exit(&report)?;
        }
        // A crash after helper acknowledgement can remove the report from its
        // next poll. The host's durable receipt remains enough to finish release.
        let receipts: Vec<_> = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions
            .values()
            .filter(|e| !e.ended)
            .filter_map(|e| e.evidence.as_ref()?.exit.as_ref().map(|r| r.report()))
            .collect();
        for report in receipts {
            self.finish_job_exit(&report)?;
        }
        Ok(())
    }

    fn finish_job_exit(&self, report: &motor_vm_helper::job::ExitReport) -> Result<()> {
        let acknowledged = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions[&report.job_id]
            .evidence
            .as_ref()
            .is_some_and(|e| e.acknowledged);
        if !acknowledged {
            self.control.ack_job_exit(report)?;
            self.host
                .lock()
                .expect("host lock")
                .job_exit_acknowledged(report)?;
        }
        {
            let mut host = self.host.lock().expect("host lock");
            let unclean = host.snapshot.per_job.executions[&report.job_id]
                .evidence
                .as_ref()
                .and_then(|e| e.unclean_stop)
                .unwrap_or(true);
            host.job_stopped_at(&report.job_id, unclean, unix_ms())?;
        }
        self.job_resources.release(&report.job_id)
    }

    /// Record pressure before the helper can remove any VM evidence.
    pub fn stop_job_pressure(&self, job: &str, now_ms: u64) -> Result<()> {
        self.host.lock().expect("host lock").job_host_stop(
            job,
            crate::state::settlement::StopCause::HostPressure,
            now_ms,
        )?;
        self.stop_job(job, false, true)
    }

    fn observe_job_frame(&self, job: &str, frame: &Value) -> Result<Option<Value>> {
        let mut host = self.host.lock().expect("host lock");
        match frame["type"].as_str() {
            Some("started") => host.job_started(job, unix_ms())?,
            Some("refusal")
                if matches!(
                    frame["reason"].as_str(),
                    Some("transfer_interrupted" | "bundle_rejected")
                ) =>
            {
                host.job_transfer_refused(job)?;
            }
            Some("terminal") => {
                host.receive_terminal(job, frame.clone(), unix_ms())?;
                // Relay the recorded winner, including removal of unauthenticated
                // host-stop claims, rather than a later guest terminal.
                return Ok(host.snapshot.billing[job].terminal.clone());
            }
            _ => {}
        }
        Ok(None)
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

#[derive(Clone)]
struct Raw {
    bytes: Vec<u8>,
    value: Value,
}

#[derive(Debug)]
enum SessionRefusal {
    CapabilitiesChanged,
    TransferInterrupted,
    SpoolExceeded,
}
impl SessionRefusal {
    fn reason(&self) -> &'static str {
        match self {
            Self::CapabilitiesChanged => "request_rejected",
            Self::TransferInterrupted | Self::SpoolExceeded => "transfer_interrupted",
        }
    }
}
impl std::fmt::Display for SessionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CapabilitiesChanged => "assigned image capabilities changed",
            Self::TransferInterrupted => "queued upload interrupted",
            Self::SpoolExceeded => "queued upload exceeds reserved spool",
        })
    }
}
impl std::error::Error for SessionRefusal {}

fn client_io(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<std::io::Error>().is_some() {
        error.context(SessionRefusal::TransferInterrupted)
    } else {
        error
    }
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
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "client closed before delivery",
                )
                .into());
            }
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
    let stopped = tokio::task::spawn_blocking(move || r.stop_job(&job, true, true)).await?;
    if let Err(error) = stopped {
        let recorded = runtime
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .executions[&claims.jti]
            .evidence
            .as_ref()
            .is_some_and(|e| e.winner.is_some());
        if !recorded {
            return Err(error);
        }
        tracing::warn!(jti = claims.jti, %error, "job_stop_retry_pending");
    }
    // A channel failure after delivery is not proof the command did not run.
    // Wait for the helper receipt (or the first-gone evidence deadline).
    loop {
        let pending = {
            let host = runtime.host.lock().expect("host lock");
            host.snapshot.leases[&claims.jti].delivered
                && host.snapshot.per_job.executions[&claims.jti]
                    .evidence
                    .as_ref()
                    .is_none_or(|e| e.winner.is_none())
        };
        if !pending {
            break;
        }
        let r = runtime.clone();
        tokio::task::spawn_blocking(move || r.tick_jobs(unix_ms())).await??;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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

pub(crate) fn host_available_mib(meminfo: &str) -> Result<u64> {
    let mut samples = meminfo
        .lines()
        .filter_map(|line| line.strip_prefix("MemAvailable:"));
    let value = samples.next().context("host MemAvailable missing")?;
    ensure!(samples.next().is_none(), "duplicate host MemAvailable");
    let mut fields = value.split_whitespace();
    let kib: u64 = fields
        .next()
        .context("host MemAvailable value missing")?
        .parse()?;
    ensure!(
        fields.next() == Some("kB") && fields.next().is_none(),
        "invalid host MemAvailable units"
    );
    Ok(kib / 1024)
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
    let launch = runtime.launch_phase(&execution.claims.jti, false)?;
    let r = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<()> {
            let cap = r.host.lock().expect("host lock").snapshot.per_job.disks[&e.disk_id]
                .storage
                .as_ref()
                .context("workspace cap missing")?
                .cap_bytes;
            if let Err(error) = r.control.ensure_workspace(&e.claims.sub, &e.disk_id, cap) {
                r.host
                    .lock()
                    .expect("host lock")
                    .quarantine_workspace(&e.claims.jti)?;
                return Err(error);
            }
            resources.prepare(&e)
        })();
        launch.in_flight.store(false, Ordering::SeqCst);
        if launch.cancelled.load(Ordering::SeqCst) {
            r.stop_job(&e.claims.jti, true, false)?;
        }
        result
    })
    .await??;
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
    let launch = runtime.launch_phase(&execution.claims.jti, true)?;
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
    if initialized["ok"] != true && initialized["error"] == "job_vm_dax_required" {
        runtime
            .host
            .lock()
            .expect("host lock")
            .set_dax_failed(true)?;
        tracing::error!(
            jti = %execution.claims.jti,
            reason = "job_vm_dax_required",
            image_generation = %assignment.image_generation_id,
            "job_boot_dax_failure"
        );
    }
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
    let mut lease = verified_lease(&execution.claims, true);
    lease["mirror_reports"] = json!(true);
    write_frame(&mut stream, &lease).await?;
    stream.write_all(&hello.bytes).await?;
    let actual = loop {
        let actual = read_frame(&mut stream).await?;
        if actual["type"] != "heartbeat" {
            break actual;
        }
    };
    ensure!(
        crate::jobs::hello_covers(&actual, &promised_hello(assignment, &hello.value)),
        SessionRefusal::CapabilitiesChanged
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
    let acknowledged = write_frame(client, &json!({"type":"lease_ok"})).await;
    let execution = runtime
        .host
        .lock()
        .expect("host lock")
        .snapshot
        .per_job
        .executions[&claims.jti]
        .clone();
    if execution.boot_started && !execution.ended {
        acknowledged?;
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
        acknowledged?;
        return live(&runtime, claims, client).await;
    }
    let result = match acknowledged {
        Ok(()) => session(&runtime, claims, id, client).await,
        Err(error) => Err(client_io(error)),
    };
    if let Err(error) = result {
        if runtime.host.lock().expect("host lock").snapshot.leases[&claims.jti].delivered {
            tracing::debug!(jti = claims.jti, %error, "job_caller_detached");
            return Ok(());
        }
        tracing::warn!(jti = claims.jti, %error, "job_boot_or_delivery_failed");
        if let Some(refused) = error.downcast_ref::<SessionRefusal>() {
            let reason = refused.reason();
            let frame = refusal(&claims.jti, reason, &format!("{error:#}"));
            {
                let mut host = runtime.host.lock().expect("host lock");
                // The independent spool replay may deliver Run while the caller
                // disconnects. Refusal and delivery must race under the same lock.
                if host.snapshot.leases[&claims.jti].delivered {
                    return Ok(());
                }
                host.job_refused(&claims.jti, frame.clone(), unix_ms())?;
            }
            let r = runtime.clone();
            let job = claims.jti.clone();
            tokio::task::spawn_blocking(move || r.stop_job(&job, true, false)).await??;
            if reason == "transfer_interrupted" {
                return stream::serve(&runtime, claims, client, false, Reader::default(), true)
                    .await;
            }
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
    stream::serve(runtime, claims, client, true, Reader::default(), false).await
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
    let hello = tokio::time::timeout_at(expires, reader.next(client))
        .await?
        .map_err(client_io)?;
    ensure!(hello.value["type"] == "hello", "expected hello");
    spool.append(&hello).await?;
    write_frame(client, &promised_hello(&assignment, &hello.value))
        .await
        .map_err(client_io)?;
    let mut have_replies = 0usize;
    let mut withdraw_replies = 0usize;
    let mut accepted = false;
    let mut queue_clock = QueueClock::new();
    let mut queue_limit_ms = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    type Starting =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<tokio::net::UnixStream>> + Send>>;
    let mut starting: Option<Starting> = None;
    let mut ready = None;
    loop {
        if accepted && ready.is_some() && !buffered_frame_ready(&reader.buffered) {
            return replay(
                runtime,
                claims,
                id,
                client,
                ready.take().unwrap(),
                Replay {
                    default_branch_tip: hello.value["default_branch_tip"] == true,
                    spool,
                    reader,
                    have_replies,
                    withdraw_replies,
                    hello_len: hello.bytes.len() as u64,
                },
            )
            .await;
        }
        let launch = if starting.is_none() && ready.is_none() {
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
            let r = runtime.clone();
            let a = assignment.clone();
            let h = hello.clone();
            starting = Some(Box::pin(async move {
                tokio::time::timeout_at(deadline, boot(&r, &execution, &a, id, &h))
                    .await
                    .map_err(|_| anyhow::anyhow!("job boot deadline exceeded"))?
            }));
        }
        tokio::select! {
            stream = async { starting.as_mut().expect("boot in progress").as_mut().await }, if starting.is_some() => {
                ready = Some(stream?);
                starting = None;
            },
            _ = tokio::time::sleep_until(expires), if !accepted => anyhow::bail!("queued lease expired"),
            frame = reader.next(client) => {
                let frame = frame.map_err(client_io)?;
                if spool.append(&frame).await.is_err() {
                    anyhow::bail!(SessionRefusal::SpoolExceeded);
                }
                if cancel(runtime, claims, client, &frame.value, queue_clock.observe(false), queue_limit_ms).await? { return Ok(()); }
                if frame.value["type"] == "withdraw" { withdraw_replies += 1; }
                match frame.value["type"].as_str() {
                    Some("have") if !accepted => {
                        let motor_protocol::Frame::Have { repositories, git_dependencies } = serde_json::from_value(frame.value)? else { unreachable!() };
                        let reply = runtime.host.lock().expect("host lock").job_have(&claims.jti, repositories, git_dependencies);
                        write_frame(client, &serde_json::to_value(reply)?).await.map_err(client_io)?;
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
                        write_frame(client, &serde_json::to_value(motor_protocol::Frame::Accepted(reply))?).await.map_err(client_io)?;
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
                write_frame(client, &json!({"type":"heartbeat","queue_wait_ms":wait})).await.map_err(client_io)?;
            },
        }
    }
}

struct Replay {
    default_branch_tip: bool,
    spool: Spool,
    reader: Reader,
    have_replies: usize,
    withdraw_replies: usize,
    hello_len: u64,
}

async fn replay<S: AsyncRead + AsyncWrite + Unpin + Send>(
    runtime: &Arc<Runtime>,
    claims: &Claims,
    id: u64,
    client: &mut S,
    guest: tokio::net::UnixStream,
    prelude: Replay,
) -> Result<()> {
    let reader = Reader {
        buffered: prelude.reader.buffered.clone(),
    };
    runtime.launch_job_stream(claims.clone(), guest.into_std()?, Some((id, prelude)))?;
    stream::serve(runtime, claims, client, false, reader, true).await
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
        .await
        .map_err(client_io)?;
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

pub(super) mod balloon;
pub(super) mod recovery;
pub(super) mod stream;
#[cfg(test)]
mod tests;
