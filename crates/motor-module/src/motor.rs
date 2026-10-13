use crate::{
    allowed_env,
    diagnostics::{record_unreachable, UnreachableStep},
    snapshot::{self, Snapshots},
    storage::{JobAttribution, Storage},
    transport::{Channel, Incoming, InstanceConnector},
    Config, Connector, SshConnector,
};
use motor_protocol::*;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, watch},
};

pub struct JobStream {
    pub job_id: Uuid,
    pub records: mpsc::Receiver<io::Result<StreamRecord>>,
    storage: Arc<Storage>,
    wake: watch::Sender<bool>,
}
impl JobStream {
    /// Persist before waking the attached task. Dropping a stream is a detach,
    /// not a cancel; only this explicit operation records cancellation intent.
    pub fn cancel(&self) -> io::Result<()> {
        self.storage.cancel(self.job_id, true)?;
        let _ = self.wake.send(true);
        Ok(())
    }
}

#[derive(Clone)]
pub struct Motor {
    config: Config,
    storage: Arc<Storage>,
    connector: Arc<dyn Connector>,
    spool_root: PathBuf,
}
impl Motor {
    pub(crate) fn set_connector(&mut self, connector: Arc<dyn Connector>) {
        self.connector = connector;
    }
    pub fn open(config: Config, storage_root: &Path) -> io::Result<Self> {
        let connector = Arc::new(SshConnector::new(config.clone()));
        Self::with_connector(config, storage_root, connector)
    }
    pub fn with_connector(
        config: Config,
        storage_root: &Path,
        connector: Arc<dyn Connector>,
    ) -> io::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            storage: Arc::new(Storage::open(storage_root)?),
            connector,
            spool_root: storage_root.join("spools"),
        })
    }
    pub fn run(&self, request: RunRequest) -> io::Result<JobStream> {
        self.run_attributed(request, None)
    }
    pub(crate) fn run_attributed(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
    ) -> io::Result<JobStream> {
        self.run_classified(request, attribution, false)
    }
    pub(crate) fn run_direct(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
    ) -> io::Result<JobStream> {
        self.run_classified(request, attribution, true)
    }
    fn run_classified(
        &self,
        mut request: RunRequest,
        attribution: Option<JobAttribution>,
        gate_job: bool,
    ) -> io::Result<JobStream> {
        let mut env_not_forwarded: Vec<_> = request
            .env
            .keys()
            .filter(|name| !allowed_env(name))
            .cloned()
            .collect();
        env_not_forwarded.sort();
        env_not_forwarded.dedup();
        // Filter before the request can enter storage, logging, or a wire frame.
        request.env.retain(|name, _| allowed_env(name));
        let id = Uuid::now_v7();
        let wait = if gate_job {
            30 * 60
        } else {
            request
                .queue_wait_limit_s
                .unwrap_or(self.config.queue_wait_limit_s)
        };
        let instance = self
            .config
            .target(request.platform.as_ref())
            .ok()
            .map(|t| t.instance);
        self.storage
            .create(id, wait, attribution, instance, gate_job)?;
        let (stream, tx, rx) = self.stream(id);
        let motor = self.clone();
        tokio::spawn(async move {
            if let Err(e) = motor
                .run_job(id, request, env_not_forwarded, wait, &tx, rx)
                .await
            {
                let _ = tx
                    .send(Err(io::Error::other(format!(
                        "job_id={id}: {e}; query exec.status or exec.attach by job_id"
                    ))))
                    .await;
            }
        });
        Ok(stream)
    }
    pub fn attach(&self, request: AttachRequest) -> io::Result<JobStream> {
        let id = request.job_id;
        self.connector_for_job(id)?;
        let (stream, tx, rx) = self.stream(id);
        let motor = self.clone();
        tokio::spawn(async move {
            let result = if let Some(t) = motor.storage.job(id).and_then(|j| j.local_terminal) {
                emit(&tx, StreamRecord::Terminal(t)).await
            } else {
                motor.follow(id, request.from_seq, None, &tx, rx).await
            };
            if let Err(e) = result {
                let _ = tx
                    .send(Err(io::Error::other(format!(
                        "job_id={id}: {e}; query exec.status or exec.attach by job_id"
                    ))))
                    .await;
            }
        });
        Ok(stream)
    }
    pub(crate) fn refuse_capacity(&self) -> io::Result<JobStream> {
        self.refuse_before_start(RefusalReason::RunnerFull, None)
    }
    /// A stream whose only record is a before-start refusal under a fresh job
    /// ID. Nothing is snapshotted, uploaded or sent to the runner.
    pub(crate) fn refuse_before_start(
        &self,
        reason: RefusalReason,
        detail: Option<String>,
    ) -> io::Result<JobStream> {
        let id = Uuid::now_v7();
        let (stream, tx, _) = self.stream(id);
        let motor = self.clone();
        tokio::spawn(async move {
            if let Err(error) = motor.refused(id, reason, None, detail, &tx).await {
                let _ = tx.send(Err(error)).await;
            }
        });
        Ok(stream)
    }
    fn stream(
        &self,
        id: Uuid,
    ) -> (
        JobStream,
        mpsc::Sender<io::Result<StreamRecord>>,
        watch::Receiver<bool>,
    ) {
        let (tx, records) = mpsc::channel(32);
        let (wake, rx) = watch::channel(false);
        (
            JobStream {
                job_id: id,
                records,
                storage: self.storage.clone(),
                wake,
            },
            tx,
            rx,
        )
    }
    async fn refused(
        &self,
        id: Uuid,
        reason: RefusalReason,
        hash: Option<String>,
        detail: Option<String>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) -> io::Result<()> {
        let mut terminal = TerminalRecord::new(id, Outcome::RefusedBeforeStart { reason }, 0, 0, 0)
            .with_ran(Ran::None);
        if let Some(hash) = hash {
            terminal = terminal.with_tree_hash(hash);
        }
        if let Some(detail) = detail {
            terminal = terminal.with_refusal_detail(bounded_detail(detail));
        }
        self.storage.local_terminal(terminal.clone())?;
        emit(tx, StreamRecord::Terminal(terminal)).await
    }
    async fn run_job(
        &self,
        id: Uuid,
        request: RunRequest,
        env_not_forwarded: Vec<String>,
        wait: u64,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
        wake: watch::Receiver<bool>,
    ) -> io::Result<()> {
        if let Err(error) = self.config.target(request.platform.as_ref()) {
            return self
                .refused(
                    id,
                    RefusalReason::PlatformUnsupported,
                    None,
                    Some(error.to_string()),
                    tx,
                )
                .await;
        }
        let windows = request.platform == Some(Platform::Windows);
        let connector = self.connector_for_job(id)?;
        if request.network.is_some() && !self.config.network_routing {
            return self
                .refused(
                    id,
                    RefusalReason::NetworkUnsupported,
                    None,
                    Some(
                        "outbound network for remote jobs is not available yet; run without net"
                            .into(),
                    ),
                    tx,
                )
                .await;
        }
        if let Some(reason) = connector.refusal() {
            return self
                .refused(id, RefusalReason::Unknown(reason.into()), None, None, tx)
                .await;
        }
        let config = self.config.clone();
        let r = request.clone();
        let valid = tokio::task::spawn_blocking(move || {
            snapshot::validate(&config, &r.workspace_key, &r.repository_root, &r.cwd)
        })
        .await
        .map_err(io::Error::other)?;
        if let Err(error) = valid {
            tracing::warn!(job_id = %id, check = %error, "workspace key refused");
            return self
                .refused(
                    id,
                    RefusalReason::WorkspaceKeyRejected,
                    None,
                    Some(error.to_string()),
                    tx,
                )
                .await;
        }
        let config = self.config.clone();
        let r = request.clone();
        let parent = self.spool_root.clone();
        let snapshots = tokio::task::spawn_blocking(move || {
            Snapshots::build(
                &config,
                &r.workspace_key,
                &r.repository_root,
                &r.siblings,
                &parent,
                id,
                None,
            )
        })
        .await
        .map_err(io::Error::other)?;
        let snapshots = match snapshots {
            Ok(s) => s,
            Err(f) => {
                tracing::warn!(job_id = %id, check = f.check, "snapshot refused");
                return self
                    .refused(
                        id,
                        RefusalReason::SnapshotFailed,
                        f.tree_hash,
                        Some(f.check.to_string()),
                        tx,
                    )
                    .await;
            }
        };
        let mut channel = match Channel::open(connector.as_ref(), id).await {
            Ok(c) => c,
            Err(reason) => {
                return self
                    .refused(id, reason, Some(snapshots.main.tree_hash.clone()), None, tx)
                    .await
            }
        };
        if windows
            && !channel
                .platforms
                .iter()
                .any(|platform| platform == "windows")
        {
            return self
                .refused(
                    id,
                    RefusalReason::PlatformUnsupported,
                    Some(snapshots.main.tree_hash.clone()),
                    Some("requested platform windows is not listed in the runner hello".into()),
                    tx,
                )
                .await;
        }
        let held = match self
            .have(
                &mut channel,
                snapshots.candidates(&self.storage.previous(&request.workspace_key)),
                snapshots.git_dependencies.keys().cloned().collect(),
                id,
                UnreachableStep::HaveExchange,
            )
            .await
        {
            Ok(h) => h,
            Err(_) => {
                return self
                    .refused(
                        id,
                        RefusalReason::Unreachable,
                        Some(snapshots.main.tree_hash.clone()),
                        None,
                        tx,
                    )
                    .await
            }
        };
        let hash = snapshots.main.tree_hash.clone();
        let (snapshots, bundles) = tokio::task::spawn_blocking(move || {
            let bundles = snapshots.transfer_bundles(id, &held.0, held.1.as_deref());
            (snapshots, bundles)
        })
        .await
        .map_err(io::Error::other)?;
        let (bundles, git_dependencies, upload) = match bundles {
            Ok(b) => b,
            Err(_) => {
                return self
                    .refused(id, RefusalReason::SnapshotFailed, Some(hash), None, tx)
                    .await
            }
        };
        let run = Run {
            gate_job: self.storage.job(id).is_some_and(|job| job.gate_job) && channel.gate_jobs,
            network: request.network.clone(),
            platform: if windows {
                Some(Platform::Windows)
            } else {
                None
            },
            job_id: id,
            workspace_key: request.workspace_key.clone(),
            workspace_kind: Some(snapshot::workspace_kind(
                &self.config,
                &request.workspace_key,
            )?),
            repository_root: request.repository_root,
            cwd: request.cwd,
            command: request.command,
            env: request.env,
            weight_hint: request.weight_hint,
            timeout: request.timeout,
            queue_wait_limit_s: wait,
            snapshot_commit: snapshots.main.commit.clone(),
            tree_hash: if self.config.inject_tree_hash_mismatch {
                "0".repeat(40)
            } else {
                hash
            },
            integration_tip: snapshots.main.tip.clone().unwrap_or_default(),
            siblings: snapshots.sibling_frames(),
            bundles: bundles.clone(),
            git_dependencies,
            base_commit: Some(snapshots.main.head.clone()),
        };
        // From this point on even a failed write might have reached the runner.
        // Never resubmit or invent a before-start outcome after attempting run.
        if channel.send(Frame::Run(run)).await.is_err() {
            drop(channel);
            drop(snapshots);
            return self.follow(id, 0, None, tx, wake).await;
        }
        let admission = channel.next().await;
        match admission {
            Ok(Incoming::Frame(Frame::Accepted(mut accepted))) => {
                // Older runners may accept the job but ignore its network request.
                // Cancel before uploading, rather than silently running it offline.
                if request.network.is_some() && accepted.network != Some(Network::Outbound) {
                    self.cancel(CancelRequest::new(id)).await?;
                    return self
                        .refused(
                            id,
                            RefusalReason::NetworkUnsupported,
                            Some(snapshots.main.tree_hash.clone()),
                            Some(
                                "runner accepted the job without granting outbound network".into(),
                            ),
                            tx,
                        )
                        .await;
                }
                let wrong_platform = windows && accepted.platform != Some(Platform::Windows);
                self.storage
                    .accepted(request.workspace_key, snapshots.commits())?;
                // A runner cannot grant network access the caller did not request.
                if request.network.is_none() {
                    accepted.network = None;
                }
                let accepted = accepted.with_env_not_forwarded(env_not_forwarded);
                if wrong_platform {
                    self.cancel(CancelRequest::new(id)).await?;
                }
                emit(tx, StreamRecord::Accepted(accepted)).await?;
                if wrong_platform {
                    drop(channel);
                    drop(snapshots);
                    return self.follow(id, 0, None, tx, wake).await;
                }
            }
            Ok(Incoming::Frame(Frame::Refusal(refusal))) => {
                log_refusal(id, &refusal);
                let (reason, detail) = runner_refusal_fields(&refusal);
                let mut terminal =
                    TerminalRecord::new(id, Outcome::RefusedBeforeStart { reason }, 0, 0, 0)
                        .with_ran(Ran::None)
                        .with_tree_hash(snapshots.main.tree_hash.clone());
                terminal.refusal_detail = detail;
                terminal.retry_after_ms = refusal.retry_after_ms;
                self.storage.local_terminal(terminal.clone())?;
                return emit(tx, StreamRecord::Terminal(terminal)).await;
            }
            // A lost acknowledgement may hide acceptance. Inspect the submitted
            // job ID rather than resubmitting a command that might already run.
            _ => {
                drop(channel);
                drop(snapshots);
                return self.follow(id, 0, None, tx, wake).await;
            }
        };
        let uploaded = self.upload(&channel, &snapshots, &upload, Some(id)).await;
        drop(snapshots);
        if uploaded.is_err() {
            drop(channel);
            return self.follow(id, 0, None, tx, wake).await;
        }
        self.follow(id, 0, Some(channel), tx, wake).await
    }
    async fn have(
        &self,
        channel: &mut Channel,
        repositories: Vec<RepositoryCandidates>,
        git_dependencies: Vec<String>,
        id: Uuid,
        step: UnreachableStep,
    ) -> io::Result<(
        Vec<RepositoryCandidates>,
        Option<Vec<GitDependencyCandidates>>,
    )> {
        let result = async {
            channel
                .send(Frame::Have {
                    repositories,
                    git_dependencies,
                })
                .await?;
            match channel.next().await? {
                Incoming::Frame(Frame::HaveReply {
                    repositories,
                    git_dependencies,
                }) => Ok((repositories, git_dependencies)),
                _ => Err(io::Error::other("expected have_reply")),
            }
        }
        .await;
        if let Err(error) = &result {
            record_unreachable(id, step, error);
        }
        result
    }
    async fn upload(
        &self,
        channel: &Channel,
        snapshots: &Snapshots,
        bundles: &[BundleManifest],
        job: Option<Uuid>,
    ) -> io::Result<()> {
        for bundle in bundles {
            let mut file =
                tokio::fs::File::open(snapshots.spool.0.join(format!("bundle-{}", bundle.index)))
                    .await?;
            let mut offset = 0;
            let mut bytes = vec![0; 65536];
            loop {
                // Keep an incomplete upload alive until cancel is acknowledged.
                // Closing first would let the runner retain transfer_interrupted
                // instead of the cancellation tombstone.
                if let Some(id) = job.filter(|id| self.pending(*id)) {
                    let mut delay = 1;
                    while self.pending(id) {
                        if self.send_cancel(id).await.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                        delay = (delay * 2).min(30);
                    }
                    return Err(io::Error::other("cancel upload"));
                }
                let n = file.read(&mut bytes).await?;
                if n == 0 {
                    break;
                }
                channel
                    .send(Frame::BundleChunk {
                        index: bundle.index,
                        offset,
                        data: BytePayload(bytes[..n].to_vec()),
                    })
                    .await?;
                offset += n as u64;
            }
        }
        Ok(())
    }
    /// Whether a cancel for this job is recorded and not yet acknowledged.
    pub(crate) fn pending(&self, id: Uuid) -> bool {
        self.storage.job(id).is_some_and(|j| j.pending_cancel)
    }
    async fn send_cancel(&self, id: Uuid) -> io::Result<()> {
        let connector = self.connector_for_job(id)?;
        let mut channel = Channel::open(connector.as_ref(), id)
            .await
            .map_err(|r| io::Error::other(format!("{r:?}")))?;
        channel.send(Frame::Cancel(CancelRequest::new(id))).await?;
        match channel.next().await? {
            Incoming::Frame(Frame::CancelReply(reply)) if reply.job_id == id => {
                self.storage.cancel(id, false)
            }
            _ => Err(io::Error::other("expected cancel_reply")),
        }
    }
    async fn follow(
        &self,
        id: Uuid,
        mut cursor: u64,
        mut channel: Option<Channel>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
        mut wake: watch::Receiver<bool>,
    ) -> io::Result<()> {
        let connector = self.connector_for_job(id)?;
        let wait = self
            .storage
            .job(id)
            .map(|j| j.queue_wait_limit_s)
            .unwrap_or(self.config.queue_wait_limit_s);
        let mut backoff = 1;
        let mut withdraw_sent = false;
        let mut queue_deadline = None;
        let mut wake_open = true;
        loop {
            if tx.is_closed() {
                return Err(io::Error::other("caller detached"));
            }
            if self.pending(id) {
                let cancel = tokio::select! {
                    result = self.send_cancel(id) => result,
                    _ = tx.closed() => return Err(io::Error::other("caller detached")),
                };
                if cancel.is_err() {
                    self.pause(backoff, &mut wake, &mut wake_open).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
                drop(channel.take());
            }
            if channel.is_none() {
                queue_deadline = None;
                let opened = tokio::select! {
                    result = Channel::open(connector.as_ref(), id) => result,
                    _ = tx.closed() => return Err(io::Error::other("caller detached")),
                };
                if let Ok(c) = opened {
                    // A persisted cancellation may arrive while the connection opens.
                    if self.pending(id) {
                        drop(c);
                        continue;
                    }
                    // Attach by the submitted ID, never resubmit Run after a
                    // broken connection. The runner returns output and the
                    // terminal outcome it retained for that ID.
                    if c.send(Frame::Attach(AttachRequest::new(id, cursor)))
                        .await
                        .is_ok()
                    {
                        channel = Some(c);
                    }
                }
                if channel.is_none() {
                    self.pause(backoff, &mut wake, &mut wake_open).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
            }
            let c = channel.as_mut().unwrap();
            let incoming = tokio::select! {
                biased;
                frame = c.next_job() => Some(frame),
                changed = wake.changed(), if wake_open => { if changed.is_err() { wake_open = false; } None },
                _ = async { if let Some(deadline) = queue_deadline { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } }, if !withdraw_sent => {
                    withdraw_sent = true;
                    queue_deadline = None;
                    if c.send(Frame::Withdraw { job_id: id, reason: None }).await.is_err() { channel = None; }
                    None
                },
            };
            let Some(incoming) = incoming else {
                continue;
            };
            match incoming {
                Ok(Incoming::Frame(Frame::Heartbeat { queue_wait_ms })) => {
                    queue_deadline = queue_wait_ms.and_then(|elapsed| {
                        tokio::time::Instant::now().checked_add(Duration::from_millis(
                            wait.saturating_mul(1000).saturating_sub(elapsed),
                        ))
                    });
                }
                Ok(Incoming::Frame(Frame::Started {
                    job_id,
                    seq,
                    queue_wait_ms,
                    started_at_ms,
                    path_mapping,
                })) if job_id == id => {
                    queue_deadline = None;
                    if seq < cursor {
                        continue;
                    }
                    cursor = seq.saturating_add(1);
                    let mut started = Started::new(seq, queue_wait_ms, started_at_ms);
                    started.path_mapping = path_mapping;
                    emit(tx, StreamRecord::Started(started)).await?;
                    backoff = 1;
                }
                Ok(Incoming::Frame(Frame::Output(output))) => {
                    if output.seq < cursor {
                        continue;
                    }
                    cursor = output.seq.saturating_add(1);
                    // Unknown streams still advance the replay cursor; they are
                    // not terminal and cannot authorize a local fallback.
                    if matches!(output.stream, OutputStream::Stdout | OutputStream::Stderr) {
                        emit(tx, StreamRecord::Output(output)).await?;
                    }
                    backoff = 1;
                }
                Ok(Incoming::Unknown(seq)) => {
                    if let Some(seq) = seq {
                        cursor = cursor.max(seq.saturating_add(1));
                    }
                }
                Ok(Incoming::Frame(Frame::Terminal(mut terminal))) => {
                    // A retry is safe only when the runner proves the command never started.
                    if !matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. }) {
                        terminal.retry_after_ms = None;
                    }
                    terminal.refusal_detail = terminal.refusal_detail.map(bounded_detail);
                    return emit(tx, StreamRecord::Terminal(terminal)).await;
                }
                Ok(Incoming::Frame(Frame::WithdrawReply { .. })) => {}
                _ => {
                    channel = None;
                    // EOF and write failure wake immediately; only failed
                    // reconnects back off, not the first status query.
                }
            }
        }
    }
    async fn pause(&self, seconds: u64, wake: &mut watch::Receiver<bool>, open: &mut bool) {
        tokio::select! { _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}, result = wake.changed(), if *open => { if result.is_err() { *open = false; } } }
    }
    /// Idempotent acknowledgement means intent is durable locally. If detached,
    /// a retry task sends it without depending on an attach caller staying alive.
    pub async fn cancel(&self, request: CancelRequest) -> io::Result<CancelReply> {
        let id = request.job_id;
        self.connector_for_job(id)?;
        self.storage.cancel(id, true)?;
        if self.send_cancel(id).await.is_err() {
            let motor = self.clone();
            tokio::spawn(async move {
                let mut delay = 1;
                while motor.pending(id) {
                    if motor.send_cancel(id).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    delay = (delay * 2).min(30);
                }
            });
        }
        Ok(CancelReply::new(id))
    }
    pub async fn status(&self) -> StatusReply {
        self.disk_status().await.status
    }
    pub async fn disk_status(&self) -> RunnerStatusReply {
        self.runner_status()
            .await
            .unwrap_or_else(|_| RunnerStatusReply {
                status: StatusReply::new(0, Vec::new(), false, Vec::new(), String::new()),
                disk: None,
                health: None,
            })
    }
    async fn runner_status(&self) -> io::Result<RunnerStatusReply> {
        self.runner_status_for(None).await
    }
    pub(crate) async fn runner_status_for(
        &self,
        platform: Option<&Platform>,
    ) -> io::Result<RunnerStatusReply> {
        let connector = self.connector_for_target(self.config.target(platform)?);
        let mut channel = Channel::open_with_cause(connector.as_ref(), Uuid::now_v7())
            .await
            .map_err(|(reason, cause)| {
                io::Error::other(format!("connect to runner: {reason:?}: {cause}"))
            })?;
        channel
            .send(Frame::Status)
            .await
            .map_err(|error| io::Error::other(format!("send runner status: {error}")))?;
        match channel
            .next()
            .await
            .map_err(|error| io::Error::other(format!("receive runner status: {error}")))?
        {
            Incoming::Frame(Frame::StatusReply(reply)) => Ok(reply),
            _ => Err(io::Error::other("expected runner status_reply")),
        }
    }
    pub async fn runner_health(&self) -> io::Result<RunnerHealth> {
        self.runner_health_for(None).await
    }
    pub(crate) async fn runner_health_for(
        &self,
        platform: Option<&Platform>,
    ) -> io::Result<RunnerHealth> {
        self.runner_status_for(platform)
            .await?
            .health
            .ok_or_else(|| io::Error::other("runner does not report health (upgrade required)"))
    }
    pub async fn job_status(&self, id: Uuid) -> io::Result<JobStatusReply> {
        let connector = self.connector_for_job(id)?;
        let mut channel = Channel::open(connector.as_ref(), id)
            .await
            .map_err(|reason| io::Error::other(format!("job_id={id}: {reason:?}")))?;
        channel.send(Frame::JobStatus { job_id: id }).await?;
        match channel.next().await? {
            Incoming::Frame(Frame::JobStatusReply(reply)) if reply.job_id == id => Ok(reply),
            _ => Err(io::Error::other(format!(
                "job_id={id}: expected job_status_reply"
            ))),
        }
    }
    pub async fn drop_workspace(&self, request: DropRequest) -> io::Result<DropReply> {
        let mut channel = Channel::open(self.connector.as_ref(), Uuid::now_v7())
            .await
            .map_err(|r| io::Error::other(format!("{r:?}")))?;
        channel.send(Frame::Drop(request)).await?;
        match channel.next().await? {
            Incoming::Frame(Frame::DropReply(reply)) => Ok(reply),
            _ => Err(io::Error::other("expected drop_reply")),
        }
    }
    pub async fn prepare(&self, request: PrepareRequest) -> io::Result<PrepareReply> {
        let id = Uuid::now_v7();
        let config = self.config.clone();
        let r = request.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || {
            snapshot::validate(
                &config,
                &r.workspace_key,
                &r.repository_root,
                &r.workspace_key,
            )
        })
        .await
        .map_err(io::Error::other)?
        {
            tracing::warn!(transfer_id = %id, check = %error, "workspace key refused");
            return Ok(prepare_refused(id, RefusalReason::WorkspaceKeyRejected)
                .with_refusal_detail(bounded_detail(error.to_string())));
        }
        let config = self.config.clone();
        let r = request.clone();
        let parent = self.spool_root.clone();
        let snapshots = tokio::task::spawn_blocking(move || {
            Snapshots::build(
                &config,
                &r.workspace_key,
                &r.repository_root,
                &r.siblings,
                &parent,
                id,
                Some(&r.base_commit),
            )
        })
        .await
        .map_err(io::Error::other)?;
        let snapshots = match snapshots {
            Ok(s) => s,
            Err(f) => {
                tracing::warn!(transfer_id = %id, check = f.check, "snapshot refused");
                return Ok(prepare_refused(id, RefusalReason::SnapshotFailed)
                    .with_refusal_detail(bounded_detail(f.check.to_string())));
            }
        };
        let mut channel = match Channel::open(self.connector.as_ref(), id).await {
            Ok(c) => c,
            Err(r) => return Ok(prepare_refused(id, r)),
        };
        let held = match self
            .have(
                &mut channel,
                snapshots.candidates(&self.storage.previous(&request.workspace_key)),
                snapshots.git_dependencies.keys().cloned().collect(),
                id,
                UnreachableStep::Prepare,
            )
            .await
        {
            Ok(h) => h,
            Err(_) => return Ok(prepare_refused(id, RefusalReason::Unreachable)),
        };
        let (snapshots, bundles) = tokio::task::spawn_blocking(move || {
            let b = snapshots.transfer_bundles(id, &held.0, held.1.as_deref());
            (snapshots, b)
        })
        .await
        .map_err(io::Error::other)?;
        let (bundles, git_dependencies, upload) = match bundles {
            Ok(b) => b,
            Err(_) => return Ok(prepare_refused(id, RefusalReason::SnapshotFailed)),
        };
        let frame = Frame::Prepare(Prepare {
            transfer_id: id,
            workspace_kind: Some(snapshot::workspace_kind(
                &self.config,
                &request.workspace_key,
            )?),
            workspace_key: request.workspace_key,
            repository_root: request.repository_root,
            base_commit: snapshots.main.commit.clone(),
            integration_tip: snapshots.main.tip.clone().unwrap_or_default(),
            siblings: snapshots.sibling_frames(),
            bundles: bundles.clone(),
            git_dependencies,
        });
        let result = async {
            channel.send(frame).await?;
            match channel.next().await? {
                Incoming::Frame(Frame::PrepareAccepted { .. }) => {}
                Incoming::Frame(Frame::Refusal(refusal)) => {
                    log_refusal(id, &refusal);
                    return Ok(prepare_runner_refused(id, &refusal));
                }
                _ => return Err(io::Error::other("expected prepare_accepted")),
            }
            self.upload(&channel, &snapshots, &upload, None).await?;
            match channel.next().await? {
                Incoming::Frame(Frame::Prepared { .. }) => {
                    Ok(PrepareReply::new(id, PrepareOutcome::Prepared))
                }
                Incoming::Frame(Frame::Refusal(refusal)) => {
                    log_refusal(id, &refusal);
                    Ok(prepare_runner_refused(id, &refusal))
                }
                _ => Err(io::Error::other("prepare detached")),
            }
        }
        .await;
        Ok(result.unwrap_or_else(|_| prepare_refused(id, RefusalReason::TransferInterrupted)))
    }
    fn connector_for_target(&self, target: crate::config::RunnerTarget) -> Arc<dyn Connector> {
        Arc::new(InstanceConnector {
            connector: self.connector.clone(),
            target,
        })
    }
    fn connector_for_job(&self, id: Uuid) -> io::Result<Arc<dyn Connector>> {
        let job = self.storage.job(id);
        // Each job record names the runner instance it was submitted to, so attach,
        // status and cancel reach the same instance after a restart. Records
        // written before that field existed were all submitted to the Linux runner.
        let target = self
            .config
            .stored_target(job.as_ref().and_then(|j| j.runner_instance.as_deref()))?;
        Ok(self.connector_for_target(target))
    }
}
fn prepare_refused(id: Uuid, reason: RefusalReason) -> PrepareReply {
    PrepareReply::new(id, PrepareOutcome::RefusedBeforeStart { reason })
}
fn bounded_detail(mut detail: String) -> String {
    let mut end = detail.len().min(REFUSAL_DETAIL_MAX_BYTES);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    detail
}
fn runner_refusal_fields(refusal: &Refusal) -> (RefusalReason, Option<String>) {
    if let Some(full) = &refusal.disk_full {
        (RefusalReason::RunnerDiskFull, Some(full.diagnostic()))
    } else if refusal.reason == RefusalReason::RunnerDraining {
        (
            RefusalReason::RunnerDraining,
            Some("runner_draining".into()),
        )
    } else {
        (
            refusal.reason.clone(),
            refusal.detail.clone().map(bounded_detail),
        )
    }
}
fn prepare_runner_refused(id: Uuid, refusal: &Refusal) -> PrepareReply {
    let (reason, detail) = runner_refusal_fields(refusal);
    let mut reply = prepare_refused(id, reason);
    reply.refusal_detail = detail;
    reply.retry_after_ms = refusal.retry_after_ms;
    reply
}
async fn emit(tx: &mpsc::Sender<io::Result<StreamRecord>>, record: StreamRecord) -> io::Result<()> {
    // Caller disconnection is ordinary detach. The runner retains history; the
    // module must not keep a full output stream in memory or cancel the job.
    tx.send(Ok(record))
        .await
        .map_err(|_| io::Error::other("caller detached"))
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod queue_tests;

#[cfg(test)]
#[path = "started_tests.rs"]
mod started_tests;

fn log_refusal(id: Uuid, refusal: &Refusal) {
    tracing::info!(job_id = %id, reason = ?refusal.reason,
        retry_after_ms = refusal.retry_after_ms, "runner admission refused");
}
