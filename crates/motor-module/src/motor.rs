use crate::{
    allowed_env,
    diagnostics::{record_unreachable, UnreachableStep},
    snapshot::{self, Snapshots},
    storage::{JobAttribution, JobLease, Storage},
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
    _lease: JobLease,
}
impl JobStream {
    pub(crate) fn storage(&self) -> Arc<Storage> {
        self.storage.clone()
    }
    pub fn hosted_state(&self) -> Option<crate::HostedJobState> {
        self.storage.job(self.job_id).and_then(|j| j.hosted_route())
    }
    pub fn return_result(&self) -> Option<ReturnResult> {
        self.storage.job(self.job_id).and_then(|j| j.return_result)
    }
    /// Record terminal delivery only after the consumer has successfully handled
    /// the terminal record. Merely queueing a record does not count as delivery.
    pub fn acknowledge_terminal(&self) -> io::Result<()> {
        self.storage.terminal_delivered(self.job_id)
    }
    /// Persist before waking the attached task. Dropping a stream is a detach,
    /// not a cancel; only this explicit operation records cancellation intent.
    pub fn cancel(&self) -> io::Result<()> {
        self.storage.cancel(self.job_id, true)?;
        if self.hosted_state().is_some() {
            tracing::info!(job_id = %self.job_id, step = "cancel", cause = "caller requested cancellation", "hosted cancellation requested");
        }
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
    account_tokens: Option<Arc<crate::AccountTokens>>,
}
#[cfg(test)]
#[path = "hosted/account/boundary_tests.rs"]
mod hosted_account_tests;
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
            account_tokens: None,
        })
    }
    pub fn with_account_tokens(mut self, tokens: Arc<crate::AccountTokens>) -> Self {
        self.account_tokens = Some(tokens);
        let motor = self.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                motor.resume_finishes().await;
            });
        }
        self
    }
    async fn resume_finishes(&self) {
        for (id, state) in self.storage.pending_finishes() {
            let terminal = state.terminal.clone();
            let _ = self
                .hosted_client(state)
                .finish(id, terminal.as_ref(), &self.storage)
                .await;
        }
    }
    fn hosted_client(&self, state: crate::HostedJobState) -> crate::hosted::Client {
        crate::hosted::Client::new(state).with_account_tokens(self.account_tokens.clone())
    }
    pub fn run(&self, request: RunRequest) -> io::Result<JobStream> {
        self.run_attributed(request, None)
    }
    pub(crate) fn run_attributed(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
    ) -> io::Result<JobStream> {
        self.run_classified(
            request,
            attribution,
            false,
            false,
            false,
            crate::ReturnOptions::default(),
        )
    }
    #[cfg(test)]
    pub(crate) fn run_direct(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
    ) -> io::Result<JobStream> {
        self.run_classified(
            request,
            attribution,
            true,
            false,
            false,
            crate::ReturnOptions::default(),
        )
    }
    pub fn run_with_returns(
        &self,
        request: RunRequest,
        returns: crate::ReturnOptions,
    ) -> io::Result<JobStream> {
        self.run_classified(request, None, false, false, false, returns)
    }
    pub(crate) fn run_direct_with_spill(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
        returns: crate::ReturnOptions,
        allow_spill: bool,
    ) -> io::Result<JobStream> {
        self.run_classified(request, attribution, true, allow_spill, false, returns)
    }
    /// Send one direct job straight to a hosted lease, to test the hosted
    /// path. It needs neither `hosted_runners` nor spill, and changes neither
    /// for any other job. The lease avoids no host, is not retried for
    /// capacity, and a lease that can't be granted or connected refuses the
    /// job instead of falling back to the build server.
    pub(crate) fn run_direct_hosted(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
        returns: crate::ReturnOptions,
    ) -> io::Result<JobStream> {
        self.run_classified(request, attribution, true, false, true, returns)
    }
    pub(crate) fn set_direct_secret_hash(&self, id: Uuid, hash: [u8; 32]) -> io::Result<()> {
        self.storage.set_direct_secret_hash(id, hash)
    }
    pub(crate) fn direct_authorized(&self, id: Uuid, secret: Option<&str>) -> bool {
        self.storage.direct_authorized(id, secret)
    }
    fn run_classified(
        &self,
        mut request: RunRequest,
        attribution: Option<JobAttribution>,
        gate_job: bool,
        allow_gate_spill: bool,
        hosted_test: bool,
        returns: crate::ReturnOptions,
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
        let spill = !hosted_test
            && crate::spill::eligible(&self.config, &request, gate_job, allow_gate_spill)
            && self.config.spill_after_s <= wait;
        let instance = self
            .config
            .target(request.platform.as_ref())
            .ok()
            .map(|t| t.instance);
        self.storage
            .create(id, wait, attribution, instance, gate_job)?;
        if hosted_test {
            let mut route = crate::HostedJobState::new(&self.config);
            route.hosted_test = true;
            self.storage.hosted_route(id, route)?;
        } else if self.config.hosted_runners && request.platform != Some(Platform::Windows) {
            self.storage
                .hosted_route(id, crate::HostedJobState::new(&self.config))?;
        }
        self.storage
            .retain_job(id, Some(request.workspace_key.clone()))?;
        let (stream, tx, rx) = self.stream(id);
        let motor = self.clone();
        tokio::spawn(async move {
            if let Err(e) = motor
                .run_job(
                    id,
                    (request, returns),
                    env_not_forwarded,
                    (wait, spill),
                    &tx,
                    rx,
                )
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
        self.storage.retain_job(id, None)?;
        if self.hosted_state(id).is_none() {
            self.connector_for_job(id)?;
        }
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
        if self.config.hosted_runners {
            self.storage
                .hosted_route(id, crate::HostedJobState::new(&self.config))?;
        }
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
                _lease: self.storage.lease_job(id),
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
        self.refused_with_retry(id, reason, hash, detail, None, tx)
            .await
    }
    async fn refused_with_retry(
        &self,
        id: Uuid,
        reason: RefusalReason,
        hash: Option<String>,
        detail: Option<String>,
        retry_after_ms: Option<u64>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) -> io::Result<()> {
        if self.hosted_state(id).is_some() {
            tracing::warn!(job_id = %id, step = "refused_before_start", ?reason,
                cause = detail.as_deref().unwrap_or("no refusal detail"), "hosted job stopped");
        }
        let mut terminal = TerminalRecord::new(id, Outcome::RefusedBeforeStart { reason }, 0, 0, 0)
            .with_ran(Ran::None);
        terminal.retry_after_ms = retry_after_ms;
        if let Some(hash) = hash {
            terminal = terminal.with_tree_hash(hash);
        }
        let detail = if self.hosted_state(id).is_some() {
            if let Some(state) = self.hosted_state(id).filter(|s| s.lease_attempted) {
                let boat = state
                    .placement
                    .as_ref()
                    .is_some_and(|p| p.host.starts_with("boat:"));
                // This refuses a hosted job before or after its Run frame. Before
                // the Run frame reaches a runner the job cannot have started, so
                // neither a lost sandbox nor an admitted lease may turn the
                // refusal into an unknown outcome or a reattach.
                if boat
                    && self
                        .hosted_client(state.clone())
                        .finish(id, Some(&terminal), &self.storage)
                        .await?
                    && state.run_sent
                {
                    return self.lost_sandbox(id, tx).await;
                }
                if !boat {
                    let released = self.hosted_client(state.clone()).release(id).await;
                    tracing::info!(job_id = %id, result = released.err().map_or("released", |e| e.code()),
                        run_sent = state.run_sent, "hosted release");
                    if released == Err(crate::hosted::Error::Admitted) {
                        if state.run_sent || state.placement.is_some() {
                            let (_, wake) = watch::channel(false);
                            return self.follow(id, 0, None, tx, wake).await;
                        }
                        // Boat jobs are admitted at booking, so job_admitted
                        // with no recorded placement and no Run frame sent is a
                        // Boat booking whose lease reply ck-motor never
                        // accepted. Nothing ran: settle it and refuse.
                        self.hosted_client(state)
                            .finish_never_ran(id, &self.storage)
                            .await?;
                    }
                }
            }
            let code = detail
                .as_deref()
                .filter(|d| {
                    d.starts_with("hosted_")
                        || d.starts_with("return_paths_")
                        || d.starts_with("lease_mismatch")
                })
                .and_then(|d| d.split(':').next())
                .unwrap_or("hosted_request_rejected")
                .to_string();
            self.storage.hosted_update(id, |state| {
                state.hosted_no_fallback = true;
                state.hosted_code = Some(code.clone());
            })?;
            Some(match detail {
                Some(d) if d.starts_with(&code) => d,
                other => format!("{code}: {}", other.as_deref().unwrap_or("request refused")),
            })
        } else {
            detail
        };
        if let Some(detail) = detail {
            terminal = terminal.with_refusal_detail(bounded_detail(detail));
        }
        self.storage.local_terminal(terminal.clone())?;
        emit(tx, StreamRecord::Terminal(terminal)).await
    }
    async fn run_job(
        &self,
        id: Uuid,
        submission: (RunRequest, crate::ReturnOptions),
        env_not_forwarded: Vec<String>,
        (wait, spill): (u64, bool),
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
        wake: watch::Receiver<bool>,
    ) -> io::Result<()> {
        let (request, returns) = submission;
        let hosted = self.hosted_state(id).is_some();
        if let Err(code) = validate_return_paths(
            &request.workspace_key,
            &returns.return_paths,
            returns.return_max_bytes,
        ) {
            return self
                .refused(
                    id,
                    RefusalReason::RequestRejected,
                    None,
                    Some(code.into()),
                    tx,
                )
                .await;
        }
        if hosted {
            if let Err(error) = crate::hosted::validate(&request) {
                return self
                    .refused(id, error.reason(), None, Some(error.code().into()), tx)
                    .await;
            }
        }
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
        let connector = if hosted {
            None
        } else {
            Some(self.connector_for_job(id)?)
        };
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
        if let Some(code) = connector.as_ref().and_then(|c| c.refusal()) {
            let (reason, detail) = request_refusal_fields(RefusalReason::Unknown(code.into()));
            return self.refused(id, reason, None, detail, tx).await;
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
        let storage = self.storage.clone();
        let snapshots = tokio::task::spawn_blocking(move || {
            Snapshots::build(
                &config,
                &r.workspace_key,
                &r.repository_root,
                &r.siblings,
                &parent,
                id,
                None,
                &storage,
            )
        })
        .await
        .map_err(io::Error::other)?;
        let snapshots = match snapshots {
            Ok(s) => s,
            Err(f) => {
                let detail = snapshot_refusal_detail(
                    f.check,
                    &self.spool_root,
                    request.env.values().cloned(),
                );
                tracing::warn!(job_id = %id, check = detail, "snapshot refused");
                return self
                    .refused(
                        id,
                        RefusalReason::SnapshotFailed,
                        f.tree_hash,
                        Some(detail),
                        tx,
                    )
                    .await;
            }
        };
        let snapshots = Arc::new(snapshots);
        loop {
            if !self
                .run_snapshot(
                    id,
                    (request.clone(), returns.clone()),
                    env_not_forwarded.clone(),
                    wait,
                    spill,
                    snapshots.clone(),
                    tx,
                    wake.clone(),
                )
                .await
                .inspect_err(|error| self.log_hosted_failure(id, "run_snapshot", error))?
            {
                return Ok(());
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_snapshot(
        &self,
        id: Uuid,
        submission: (RunRequest, crate::ReturnOptions),
        env_not_forwarded: Vec<String>,
        wait: u64,
        spill: bool,
        snapshots: Arc<Snapshots>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
        wake: watch::Receiver<bool>,
    ) -> io::Result<bool> {
        let (mut request, returns) = submission;
        let hosted = self.hosted_state(id).is_some();
        let windows = request.platform == Some(Platform::Windows);
        let build_server_id = self.build_server_id(id);
        // A resubmitted spill gets one spill attempt only, and its queue-wait
        // budget starts again: the first wait only proved the build server full.
        let resubmitted = build_server_id != id;
        let spilled = self
            .hosted_state(id)
            .is_some_and(|route| route.spill_wait_ms.is_some());
        let spill_after = (spill && !hosted && !resubmitted).then_some(self.config.spill_after_s);
        let wire_wait = spill_after.map_or(wait, |after| wait.min(after));
        let connector = if hosted {
            None
        } else {
            Some(self.connector_for_job(id)?)
        };
        // Measures how long a spilled job spent on its hosted route.
        let spill_started = tokio::time::Instant::now();
        let (mut channel, held) = loop {
            let opened = if let Some(state) = self.hosted_state(id) {
                let client = self.hosted_client(state);
                match client.run(id, &request, &self.storage).await {
                    Ok((channel, lease)) => {
                        request.timeout = Some(lease.max_s);
                        request.weight_hint = Some(request.weight_hint.unwrap_or(2));
                        Ok(channel)
                    }
                    Err(error) => {
                        // The hosted client never sends a Run frame, so this
                        // failure proves the job did not start anywhere.
                        if self.return_spill(id, error.code(), spill_started).await? {
                            return Ok(true);
                        }
                        match error {
                            crate::hosted::Error::Admitted => {
                                return self.follow_inner(id, 0, None, tx, wake, spill_after).await
                            }
                            crate::hosted::Error::SandboxLost => {
                                return self.lost_sandbox(id, tx).await.map(|_| false)
                            }
                            error => {
                                // A direct hosted test exists to show what the
                                // broker answered, so name its error code too.
                                let detail = if self.hosted_state(id).is_some_and(|s| s.hosted_test)
                                {
                                    error.detail_with_broker_code(client.broker_code().as_deref())
                                } else {
                                    error.detail()
                                };
                                Err((error.reason(), Some(detail), error.retry_after_ms()))
                            }
                        }
                    }
                }
            } else {
                open_for_request(connector.as_ref().unwrap().as_ref(), build_server_id)
                    .await
                    .map_err(|(reason, detail)| (reason, detail, None))
            };
            let mut channel = match opened {
                Ok(c) => c,
                Err((reason, detail, retry_after_ms)) => {
                    return self
                        .refused_with_retry(
                            id,
                            reason,
                            Some(snapshots.main.tree_hash.clone()),
                            detail,
                            retry_after_ms,
                            tx,
                        )
                        .await
                        .map(|_| false)
                }
            };
            if !returns.return_paths.is_empty() && !channel.return_paths {
                if self
                    .return_spill(id, "return_paths_unsupported", spill_started)
                    .await?
                {
                    return Ok(true);
                }
                return self
                    .refused(
                        id,
                        RefusalReason::RequestRejected,
                        Some(snapshots.main.tree_hash.clone()),
                        Some("return_paths_unsupported".into()),
                        tx,
                    )
                    .await
                    .map(|_| false);
            }
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
                    .await
                    .map(|_| false);
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
                // No Run frame was sent on this connection, so a spilled job
                // goes back to the build server rather than reattaching to a
                // runner that never received it.
                Err(_) if self.spill_returnable(id) => {
                    drop(channel);
                    if self
                        .return_spill(id, "hosted_connect_failed", spill_started)
                        .await?
                    {
                        return Ok(true);
                    }
                    return self.follow_inner(id, 0, None, tx, wake, None).await;
                }
                Err(_)
                    if self
                        .hosted_state(id)
                        .and_then(|s| s.placement)
                        .is_some_and(|p| p.host.starts_with("boat:")) =>
                {
                    drop(channel);
                    return self.follow_inner(id, 0, None, tx, wake, None).await;
                }
                Err(_)
                    if hosted
                        && self
                            .hosted_state(id)
                            .is_some_and(|s| s.placement_attempts < 3) =>
                {
                    drop(channel);
                    continue;
                }
                Err(_) => {
                    return self
                        .refused(
                            id,
                            RefusalReason::Unreachable,
                            Some(snapshots.main.tree_hash.clone()),
                            hosted.then(|| "hosted_connect_failed".into()),
                            tx,
                        )
                        .await
                        .map(|_| false)
                }
            };
            break (channel, held);
        };
        let hash = snapshots.main.tree_hash.clone();
        let transfer_snapshots = snapshots.clone();
        let bundles = tokio::task::spawn_blocking(move || {
            transfer_snapshots.transfer_bundles(id, &held.0, held.1.as_deref())
        })
        .await
        .map_err(io::Error::other)
        .inspect_err(|error| self.log_hosted_failure(id, "transfer_bundles", error))?;
        let (bundles, git_dependencies, upload) = match bundles {
            Ok(b) => b,
            Err(error) => {
                self.log_hosted_failure(id, "transfer_bundles", &error);
                return self
                    .refused(id, RefusalReason::SnapshotFailed, Some(hash), None, tx)
                    .await
                    .map(|_| false);
            }
        };
        let run = Run {
            return_paths: returns.return_paths,
            return_max_bytes: returns.return_max_bytes,
            gate_job: self.storage.job(id).is_some_and(|job| job.gate_job) && channel.gate_jobs,
            network: request.network.clone(),
            platform: if windows {
                Some(Platform::Windows)
            } else {
                None
            },
            job_id: build_server_id,
            workspace_key: request.workspace_key.clone(),
            workspace_kind: Some(
                snapshot::workspace_kind(&self.config, &request.workspace_key)
                    .inspect_err(|error| self.log_hosted_failure(id, "workspace_kind", error))?,
            ),
            repository_root: request.repository_root,
            cwd: request.cwd,
            command: request.command,
            env: request.env,
            weight_hint: request.weight_hint,
            timeout: request.timeout,
            queue_wait_limit_s: wire_wait,
            snapshot_commit: snapshots.main.commit.clone(),
            tree_hash: if self.config.inject_tree_hash_mismatch {
                "0".repeat(40)
            } else {
                hash
            },
            // Omit the fill tip so hosted runners cannot queue background cache
            // fills on leased CPUs, even when the snapshot resolved a tip.
            integration_tip: if hosted {
                String::new()
            } else {
                snapshots.main.tip.clone().unwrap_or_default()
            },
            siblings: snapshots.sibling_frames(),
            bundles: bundles.clone(),
            git_dependencies,
            base_commit: Some(snapshots.main.head.clone()),
        };
        // From this point on even a failed write might have reached the runner.
        // Never resubmit or invent a before-start outcome after attempting run.
        // The hosted route records that durably first: it is what stops a
        // spilled job from returning to the build server once it may have run.
        if hosted {
            self.storage
                .hosted_update(id, |state| state.run_sent = true)
                .inspect_err(|error| self.log_hosted_failure(id, "persist_run", error))?;
        }
        if let Err(error) = channel.send(Frame::Run(run)).await {
            self.log_hosted_failure(id, "run_send", &error);
            drop(channel);
            drop(snapshots);
            if self
                .return_spill(id, "hosted_run_unacknowledged", spill_started)
                .await?
            {
                return Ok(true);
            }
            return self.follow_inner(id, 0, None, tx, wake, spill_after).await;
        }
        if hosted {
            tracing::info!(job_id = %id, step = "run_send", cause = "Run frame written; awaiting admission", "hosted Run sent");
        }
        let admission = channel.next().await;
        match admission {
            Ok(Incoming::Frame(Frame::Accepted(mut accepted))) => {
                if hosted {
                    tracing::info!(job_id = %id, step = "admission", cause = "runner accepted Run", "hosted Run accepted");
                }
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
                        .await
                        .map(|_| false);
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
                    self.log_hosted_failure(id, "admission", "accepted platform mismatch");
                    self.cancel(CancelRequest::new(id)).await?;
                }
                // The caller already holds this job's acceptance from its first
                // build-server submission. Neither a spill to a hosted runner
                // nor a resubmission is a second job.
                if !resubmitted && !spilled {
                    emit(tx, StreamRecord::Accepted(accepted))
                        .await
                        .inspect_err(|error| {
                            self.log_hosted_failure(id, "accepted_delivery", error)
                        })?;
                }
                if wrong_platform {
                    drop(channel);
                    drop(snapshots);
                    return self.follow_inner(id, 0, None, tx, wake, spill_after).await;
                }
            }
            Ok(Incoming::Frame(Frame::Refusal(refusal))) => {
                log_refusal(id, &refusal);
                let (reason, detail) = runner_refusal_fields(&refusal);
                if hosted {
                    return self
                        .refused(
                            id,
                            reason,
                            Some(snapshots.main.tree_hash.clone()),
                            Some(format!("lease_mismatch: {}", detail.unwrap_or_default())),
                            tx,
                        )
                        .await
                        .map(|_| false);
                }
                let mut terminal =
                    TerminalRecord::new(id, Outcome::RefusedBeforeStart { reason }, 0, 0, 0)
                        .with_ran(Ran::None)
                        .with_tree_hash(snapshots.main.tree_hash.clone());
                terminal.refusal_detail = detail;
                terminal.retry_after_ms = refusal.retry_after_ms;
                self.storage.local_terminal(terminal.clone())?;
                return emit(tx, StreamRecord::Terminal(terminal))
                    .await
                    .map(|_| false);
            }
            // A lost acknowledgement may hide acceptance. Inspect the submitted
            // job ID rather than resubmitting a command that might already run.
            other => {
                self.log_hosted_failure(id, "admission", incoming_cause(&other));
                drop(channel);
                drop(snapshots);
                if self
                    .return_spill(id, "hosted_run_unacknowledged", spill_started)
                    .await?
                {
                    return Ok(true);
                }
                return self.follow_inner(id, 0, None, tx, wake, spill_after).await;
            }
        };
        let uploaded = self.upload(&channel, &snapshots, &upload, Some(id)).await;
        drop(snapshots);
        if let Err(error) = &uploaded {
            if hosted {
                tracing::warn!(job_id = %id, step = "upload", cause = %error, "hosted job stream closed");
            }
        }
        if uploaded.is_err() {
            drop(channel);
            return self.follow_inner(id, 0, None, tx, wake, spill_after).await;
        }
        self.follow_inner(id, 0, Some(channel), tx, wake, spill_after)
            .await
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
            self.log_hosted_failure(id, "have_exchange", error);
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
        let build_server_id = self.build_server_id(id);
        let mut channel = self
            .open_job_channel(id)
            .await
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        channel
            .send(Frame::Cancel(CancelRequest::new(build_server_id)))
            .await?;
        match channel.next().await? {
            Incoming::Frame(Frame::CancelReply(reply)) if reply.job_id == build_server_id => {
                self.storage.cancel(id, false)
            }
            _ => Err(io::Error::other("expected cancel_reply")),
        }
    }
    pub fn hosted_state(&self, id: Uuid) -> Option<crate::HostedJobState> {
        self.storage.job(id).and_then(|j| j.hosted_route())
    }
    fn log_hosted_failure(&self, id: Uuid, step: &str, cause: impl std::fmt::Display) {
        if self.hosted_state(id).is_some() {
            tracing::warn!(job_id = %id, step, cause = %cause, "hosted job stopped");
        }
    }
    /// The ID the build server knows this job by. It differs from the caller's
    /// job ID only after a spilled job whose hosted route failed before any Run
    /// frame reached a hosted runner was resubmitted to the build server.
    fn build_server_id(&self, id: Uuid) -> Uuid {
        self.storage
            .job(id)
            .and_then(|j| j.build_server_job_id)
            .unwrap_or(id)
    }
    /// Whether a spilled job whose hosted route failed may go back to the build
    /// server. A hosted runner, Boat or first-party, runs a command only from
    /// the Run frame ck-motor sends it, and the route records `run_sent`
    /// before that frame is written. Until then nothing has run anywhere,
    /// whatever the broker booked. Jobs routed hosted by configuration never
    /// fall back: only a spill records `spill_wait_ms`.
    fn spill_returnable(&self, id: Uuid) -> bool {
        self.hosted_state(id)
            .is_some_and(|route| route.spill_wait_ms.is_some() && !route.run_sent)
    }
    /// Send a spilled job that never reached a hosted runner back to the build
    /// server, rather than refusing a job that already gave up its place in
    /// the queue or reattaching to a runner that never received it. Returns
    /// false, changing nothing, when the job may not go back.
    ///
    /// The broker is told first so it stops holding a sandbox for the job: a
    /// booked Boat job is settled with finish (release cannot end it), any
    /// other lease is released. The new build-server ID is stored before the
    /// caller's loop sends its Run frame, so a restart attaches to that ID
    /// instead of losing the job or submitting it twice.
    async fn return_spill(
        &self,
        id: Uuid,
        cause: &str,
        started: tokio::time::Instant,
    ) -> io::Result<bool> {
        if !self.spill_returnable(id) {
            if self
                .hosted_state(id)
                .is_some_and(|route| route.spill_wait_ms.is_some())
            {
                tracing::info!(job_id = %id, cause,
                    "spilled job's Run frame may have reached its hosted runner; reattaching, not resubmitting");
            }
            return Ok(false);
        }
        let route = self
            .hosted_state(id)
            .ok_or_else(|| io::Error::other("missing hosted route"))?;
        let client = self.hosted_client(route.clone());
        let boat = route
            .placement
            .as_ref()
            .is_some_and(|p| p.host.starts_with("boat:"));
        if boat {
            client.finish_never_ran(id, &self.storage).await?;
        } else if route.lease_attempted {
            let released = client.release(id).await;
            tracing::info!(job_id = %id, result = released.err().map_or("released", |e| e.code()),
                "hosted release");
            // Boat jobs are admitted at booking, so job_admitted means a Boat
            // booking whose lease reply ck-motor never accepted. Settle it.
            if released == Err(crate::hosted::Error::Admitted) && route.placement.is_none() {
                client.finish_never_ran(id, &self.storage).await?;
            }
        }
        let build_server_id = Uuid::now_v7();
        self.storage.resubmitted(id, build_server_id, cause)?;
        let spill_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            job_id = %id,
            build_server_job_id = %build_server_id,
            broker_code = cause,
            spill_ms,
            "spill not placed; resubmitted to build server"
        );
        Ok(true)
    }
    pub fn return_result(&self, id: Uuid) -> Option<ReturnResult> {
        self.storage.job(id).and_then(|j| j.return_result)
    }
    pub(crate) fn hosted_health(&self) -> Option<serde_json::Value> {
        if !self.config.hosted_runners {
            return None;
        }
        let last = self.storage.last_hosted();
        let error = last.as_ref().and_then(|s| s.hosted_code.as_deref());
        Some(serde_json::json!({
            "hosted": true,
            "token_source": self.config.broker_token_source,
            "broker_reachable": last.as_ref().and_then(|s| if s.hosted_code.as_deref() == Some("hosted_broker_unreachable") { Some(false) } else if s.placement.is_some() { Some(true) } else { None }),
            "last_lease_error": error,
        }))
    }
    async fn open_job_channel(&self, id: Uuid) -> Result<Channel, crate::hosted::Error> {
        if let Some(state) = self.hosted_state(id) {
            self.hosted_client(state).attach(id, &self.storage).await
        } else {
            let connector = self
                .connector_for_job(id)
                .map_err(|_| crate::hosted::Error::Connect)?;
            Channel::open(connector.as_ref(), self.build_server_id(id))
                .await
                .map_err(|_| crate::hosted::Error::Connect)
        }
    }
    async fn finish_hosted(
        &self,
        id: Uuid,
        unavailable: bool,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) -> io::Result<()> {
        let state = self
            .hosted_state(id)
            .ok_or_else(|| io::Error::other("missing hosted route"))?;
        let mut terminal = state.terminal.clone().unwrap_or_else(|| {
            TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
                .with_ran(Ran::Unknown("maybe".into()))
        });
        if self
            .hosted_client(self.hosted_state(id).unwrap())
            .finish(id, Some(&terminal), &self.storage)
            .await?
        {
            terminal = Self::sandbox_lost_terminal(id, &state);
        }
        if unavailable {
            self.storage.hosted_update(id, |s| {
                if s.billing != crate::BillingStatus::Available {
                    s.billing = crate::BillingStatus::Unavailable;
                }
            })?;
        }
        self.storage
            .hosted_update(id, |s| s.terminal = Some(terminal.clone()))?;
        self.storage.terminal_observed(id)?;
        emit(tx, StreamRecord::Terminal(terminal)).await
    }
    async fn lost_sandbox(
        &self,
        id: Uuid,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
    ) -> io::Result<()> {
        let state = self
            .hosted_state(id)
            .ok_or_else(|| io::Error::other("missing hosted route"))?;
        let terminal = Self::sandbox_lost_terminal(id, &state);
        self.storage.hosted_update(id, |s| {
            s.hosted_code = Some("hosted_sandbox_lost".into());
            s.hosted_no_fallback = true;
        })?;
        self.hosted_client(self.hosted_state(id).unwrap())
            .finish(id, Some(&terminal), &self.storage)
            .await?;
        self.storage
            .hosted_update(id, |s| s.terminal = Some(terminal))?;
        self.finish_hosted(id, true, tx).await
    }
    fn sandbox_lost_terminal(id: Uuid, state: &crate::HostedJobState) -> TerminalRecord {
        let sandbox = state
            .placement
            .as_ref()
            .and_then(|p| p.vm.as_deref())
            .unwrap_or("unknown");
        TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
            .with_ran(Ran::Unknown("maybe".into()))
            .with_refusal_detail(format!("hosted_sandbox_lost: {sandbox}"))
    }
    async fn follow(
        &self,
        id: Uuid,
        cursor: u64,
        channel: Option<Channel>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
        wake: watch::Receiver<bool>,
    ) -> io::Result<()> {
        self.follow_inner(id, cursor, channel, tx, wake, None)
            .await
            .inspect_err(|error| self.log_hosted_failure(id, "follow", error))
            .map(|_| ())
    }
    async fn follow_inner(
        &self,
        id: Uuid,
        mut cursor: u64,
        mut channel: Option<Channel>,
        tx: &mpsc::Sender<io::Result<StreamRecord>>,
        mut wake: watch::Receiver<bool>,
        spill_after: Option<u64>,
    ) -> io::Result<bool> {
        let hosted = self.hosted_state(id).is_some();
        // Frames to and from the build server carry its ID for the job; records
        // and storage keep the caller's.
        let build_server_id = self.build_server_id(id);
        let mut spill_watch = spill_after.map(crate::spill::Watch::new);
        if self
            .hosted_state(id)
            .is_some_and(|s| s.billing != crate::BillingStatus::Pending && s.terminal.is_some())
        {
            return self.finish_hosted(id, false, tx).await.map(|_| false);
        }
        let wait = self
            .storage
            .job(id)
            .map(|j| j.queue_wait_limit_s)
            .unwrap_or(self.config.queue_wait_limit_s);
        let wait = spill_after.map_or(wait, |after| wait.min(after));
        let mut backoff = 1;
        let mut withdraw_sent = false;
        let mut queue_deadline = None;
        let mut wake_open = true;
        let mut reattaches = 0;
        let mut hosted_terminal_deadline = None;
        loop {
            if tx.is_closed() {
                self.log_hosted_failure(id, "follow", "caller detached");
                return Err(io::Error::other("caller detached"));
            }
            if self.pending(id) {
                if hosted && channel.is_some() {
                    if channel
                        .as_ref()
                        .unwrap()
                        .send(Frame::Cancel(CancelRequest::new(id)))
                        .await
                        .inspect_err(|error| self.log_hosted_failure(id, "cancel", error))
                        .is_err()
                    {
                        channel = None;
                    }
                } else {
                    let cancel = tokio::select! {
                        result = self.send_cancel(id) => result,
                        _ = tx.closed() => {
                            self.log_hosted_failure(id, "cancel", "caller detached");
                            return Err(io::Error::other("caller detached"));
                        },
                    };
                    if cancel.is_err() {
                        if hosted {
                            channel = None;
                        } else {
                            self.pause(backoff, &mut wake, &mut wake_open).await;
                            backoff = (backoff * 2).min(30);
                            continue;
                        }
                    }
                    drop(channel.take());
                }
            }
            if channel.is_none() {
                if let Some(watch) = spill_watch.as_mut() {
                    watch.disconnected();
                }
                queue_deadline = None;
                if hosted && reattaches == 3 {
                    self.log_hosted_failure(id, "reattach", "attempt limit reached");
                    return self.finish_hosted(id, false, tx).await.map(|_| false);
                }
                if hosted {
                    if reattaches > 0 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    reattaches += 1;
                }
                let opened = tokio::select! {
                    result = self.open_job_channel(id) => result,
                    _ = tx.closed() => {
                        self.log_hosted_failure(id, "reattach", "caller detached");
                        return Err(io::Error::other("caller detached"));
                    },
                };
                if let Err(error) = &opened {
                    self.log_hosted_failure(id, "reattach", error.code());
                }
                if matches!(opened, Err(crate::hosted::Error::SandboxLost)) {
                    return self.lost_sandbox(id, tx).await.map(|_| false);
                }
                if let Err(error @ crate::hosted::Error::BrokerRefused) = opened.as_ref() {
                    let terminal = TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
                        .with_ran(Ran::Unknown("maybe".into()))
                        .with_refusal_detail(error.code());
                    self.storage.hosted_update(id, |s| {
                        s.hosted_code = Some(error.code().into());
                        s.hosted_no_fallback = true;
                        s.terminal = Some(terminal);
                    })?;
                    return self.finish_hosted(id, true, tx).await.map(|_| false);
                }
                if hosted
                    && matches!(
                        opened,
                        Err(crate::hosted::Error::Finished | crate::hosted::Error::Released)
                    )
                {
                    return self.finish_hosted(id, true, tx).await.map(|_| false);
                }
                if let Ok(c) = opened {
                    // A persisted cancellation may arrive while the connection opens.
                    if self.pending(id) {
                        drop(c);
                        continue;
                    }
                    // Attach by the submitted ID, never resubmit Run after a
                    // broken connection. The runner returns output and the
                    // terminal outcome it retained for that ID.
                    if c.send(Frame::Attach(AttachRequest::new(build_server_id, cursor)))
                        .await
                        .inspect_err(|error| self.log_hosted_failure(id, "attach_send", error))
                        .is_ok()
                    {
                        channel = Some(c);
                        if hosted && self.hosted_state(id).is_some_and(|s| s.terminal.is_some()) {
                            hosted_terminal_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_secs(5));
                        }
                    }
                }
                if channel.is_none() {
                    if hosted {
                        continue;
                    }
                    self.pause(backoff, &mut wake, &mut wake_open).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }
            }
            let c = channel.as_mut().unwrap();
            let incoming = tokio::select! {
                biased;
                frame = c.next_job() => Some(frame),
                _ = async { if let Some(deadline) = hosted_terminal_deadline { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } } => {
                    self.log_hosted_failure(id, "hosted_terminal", "timeout waiting for settlement record");
                    channel = None; hosted_terminal_deadline = None; None
                },
                changed = wake.changed(), if wake_open => { if changed.is_err() { wake_open = false; } None },
                _ = async { if let Some(deadline) = queue_deadline { tokio::time::sleep_until(deadline).await; } else { std::future::pending::<()>().await; } }, if !withdraw_sent => {
                    withdraw_sent = true;
                    queue_deadline = None;
                    let spill = spill_watch.as_ref().is_some_and(|watch| watch.ready());
                    if c.send(Frame::Withdraw { job_id: build_server_id, reason: None, spill }).await.is_err() { channel = None; }
                    None
                },
            };
            let Some(incoming) = incoming else {
                continue;
            };
            match incoming {
                Ok(Incoming::Frame(Frame::Heartbeat { queue_wait_ms })) => {
                    if let Some(watch) = spill_watch.as_mut() {
                        watch.observe(queue_wait_ms);
                    }
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
                })) if job_id == build_server_id => {
                    if let Some(watch) = spill_watch.as_mut() {
                        watch.started();
                    }
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
                    if let Some(watch) = spill_watch.as_mut() {
                        watch.started();
                    }
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
                Ok(Incoming::Frame(Frame::ReturnResult(mut result)))
                    if result.job_id == build_server_id =>
                {
                    if result.seq < cursor {
                        continue;
                    }
                    cursor = result.seq.saturating_add(1);
                    result.job_id = id;
                    self.storage.return_result(result)?;
                    backoff = 1;
                }
                Ok(Incoming::Unknown(seq)) => {
                    if let Some(seq) = seq {
                        cursor = cursor.max(seq.saturating_add(1));
                    }
                }
                Ok(Incoming::Frame(Frame::Terminal(mut terminal))) => {
                    if terminal.job_id != build_server_id {
                        self.log_hosted_failure(id, "terminal", "terminal job ID mismatch");
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "terminal job ID mismatch",
                        ));
                    }
                    terminal.job_id = id;
                    // A retry is safe only when the runner proves the command never started.
                    if !matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. }) {
                        terminal.retry_after_ms = None;
                    }
                    terminal.refusal_detail = terminal.refusal_detail.map(bounded_detail);
                    if !self.pending(id)
                        && spill_watch
                            .as_ref()
                            .is_some_and(|watch| watch.withdrawn(&terminal))
                    {
                        let wait_ms = spill_watch.as_ref().unwrap().wait_ms();
                        tracing::info!(job_id = %id, wait_ms, "capacity permit wait spilled to hosted runner");
                        let mut route = crate::HostedJobState::new(&self.config);
                        route.spill_wait_ms = Some(wait_ms);
                        route.avoid_host = self.config.spill_avoid_host.clone();
                        self.storage.hosted_route(id, route)?;
                        return Ok(true);
                    }
                    let boat = hosted
                        && self
                            .hosted_state(id)
                            .and_then(|s| s.placement)
                            .is_some_and(|p| p.host.starts_with("boat:"));
                    if boat
                        && self
                            .hosted_client(self.hosted_state(id).unwrap())
                            .finish(id, Some(&terminal), &self.storage)
                            .await?
                    {
                        return self.lost_sandbox(id, tx).await.map(|_| false);
                    }
                    self.storage.terminal_observed(id)?;
                    if hosted {
                        self.storage.cancel(id, false)?;
                        if matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. }) {
                            terminal.refusal_detail = Some(format!(
                                "lease_mismatch: {}",
                                terminal.refusal_detail.take().unwrap_or_default()
                            ));
                            self.storage.hosted_update(id, |state| {
                                state.hosted_no_fallback = true;
                                state.hosted_code = Some("lease_mismatch".into());
                            })?;
                        }
                        self.storage.hosted_update(id, |state| {
                            if state
                                .terminal
                                .as_ref()
                                .is_none_or(|t| matches!(t.outcome, Outcome::OutcomeUnknown))
                            {
                                state.terminal = Some(terminal.clone());
                            }
                        })?;
                        if self
                            .hosted_state(id)
                            .and_then(|s| s.placement)
                            .is_some_and(|p| p.host.starts_with("boat:"))
                        {
                            return self.finish_hosted(id, false, tx).await.map(|_| false);
                        }
                        hosted_terminal_deadline =
                            Some(tokio::time::Instant::now() + Duration::from_secs(5));
                        continue;
                    }
                    return emit(tx, StreamRecord::Terminal(terminal))
                        .await
                        .map(|_| false);
                }
                Ok(Incoming::Frame(Frame::HostedTerminal(terminal)))
                    if hosted && terminal.job_id == id =>
                {
                    let state = self.hosted_state(id).unwrap();
                    if state.terminal.is_none()
                        || state.placement.as_ref().is_none_or(|p| {
                            terminal.placement != crate::hosted::placement::terminal_placement(p)
                        })
                        || state.max_runtime_s != Some(terminal.max_runtime_s)
                    {
                        self.log_hosted_failure(
                            id,
                            "hosted_terminal",
                            "terminal does not match stored route",
                        );
                        return Err(io::Error::other(
                            "hosted terminal does not match stored route",
                        ));
                    }
                    self.storage
                        .hosted_update(id, |state| state.settled(terminal))?;
                    return self.finish_hosted(id, false, tx).await.map(|_| false);
                }
                Ok(Incoming::Frame(Frame::WithdrawReply { .. })) => {}
                Ok(Incoming::Frame(Frame::CancelReply(reply))) if hosted && reply.job_id == id => {
                    tracing::info!(job_id = %id, step = "cancel", cause = "runner acknowledged cancellation", "hosted cancellation acknowledged");
                    self.storage.cancel(id, false)?;
                }
                other => {
                    if hosted {
                        // Name the close, so an outcome-unknown job says which
                        // connection ended and how.
                        let cause = incoming_cause(&other);
                        tracing::warn!(
                            job_id = %id,
                            step = if reattaches == 0 { "run" } else { "reattach" },
                            reattach = reattaches,
                            cursor,
                            cause,
                            "hosted job stream closed"
                        );
                    }
                    channel = None;
                    hosted_terminal_deadline = None;
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
        if self.hosted_state(id).is_none() {
            self.connector_for_job(id)?;
        }
        if self.hosted_state(id).is_some_and(|s| s.terminal.is_some()) {
            return Ok(CancelReply::new(id));
        }
        self.storage.cancel(id, true)?;
        if self.hosted_state(id).is_some() {
            tracing::info!(job_id = %id, step = "cancel", cause = "caller requested cancellation", "hosted cancellation requested");
        }
        // The task streaming a hosted job already owns its only allowed attach
        // connection. It checks pending cancellation on each heartbeat and sends
        // it there, rather than opening a conflicting second connection.
        if self.hosted_state(id).is_some() && self.storage.has_live_stream(id) {
            return Ok(CancelReply::new(id));
        }
        if self
            .send_cancel(id)
            .await
            .inspect_err(|error| self.log_hosted_failure(id, "cancel", error))
            .is_err()
        {
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
        if self.config.hosted_runners && platform != Some(&Platform::Windows) {
            return Ok(RunnerStatusReply {
                status: StatusReply::new(0, Vec::new(), false, Vec::new(), "hosted"),
                disk: None,
                health: None,
            });
        }
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
        self.storage.retain_job(id, None)?;
        if let Some(state) = self.hosted_state(id).filter(|s| s.terminal.is_some()) {
            if state.billing == crate::BillingStatus::Pending {
                let (tx, mut rx) = mpsc::channel(32);
                let (_, wake) = watch::channel(false);
                self.follow(id, u64::MAX, None, &tx, wake).await?;
                let _ = rx.try_recv();
            }
            return Ok(JobStatusReply {
                job_id: id,
                state: "terminal".into(),
                waiting_on: None,
                terminal: self.hosted_state(id).and_then(|s| s.terminal),
                unit: None,
                unit_alive: None,
                queue_wait_ms: None,
            });
        }
        let mut channel = match self.open_job_channel(id).await {
            Ok(channel) => channel,
            Err(crate::hosted::Error::Finished | crate::hosted::Error::Released)
                if self.hosted_state(id).is_some() =>
            {
                let (tx, _rx) = mpsc::channel(1);
                self.finish_hosted(id, true, &tx).await?;
                return Ok(JobStatusReply {
                    job_id: id,
                    state: "terminal".into(),
                    waiting_on: None,
                    terminal: self.hosted_state(id).and_then(|s| s.terminal),
                    unit: None,
                    unit_alive: None,
                    queue_wait_ms: None,
                });
            }
            Err(error) => return Err(io::Error::other(format!("job_id={id}: {error:?}"))),
        };
        let build_server_id = self.build_server_id(id);
        channel
            .send(Frame::JobStatus {
                job_id: build_server_id,
            })
            .await?;
        match channel.next().await? {
            Incoming::Frame(Frame::JobStatusReply(mut reply))
                if reply.job_id == build_server_id =>
            {
                reply.job_id = id;
                if let Some(terminal) = reply.terminal.as_mut() {
                    terminal.job_id = id;
                }
                if let Some(terminal) = reply
                    .terminal
                    .as_ref()
                    .filter(|_| self.hosted_state(id).is_some())
                {
                    self.storage
                        .hosted_update(id, |s| s.terminal = Some(terminal.clone()))?;
                    drop(channel);
                    let (tx, _rx) = mpsc::channel(1);
                    let (_, wake) = watch::channel(false);
                    self.follow(id, u64::MAX, None, &tx, wake).await?;
                    reply.terminal = self.hosted_state(id).and_then(|s| s.terminal);
                }
                Ok(reply)
            }
            _ => Err(io::Error::other(format!(
                "job_id={id}: expected job_status_reply"
            ))),
        }
    }
    pub async fn drop_workspace(&self, request: DropRequest) -> io::Result<DropReply> {
        if self.config.hosted_runners {
            let id = Uuid::now_v7();
            self.storage
                .hosted_route(id, crate::HostedJobState::new(&self.config))?;
            self.storage.hosted_update(id, |s| {
                s.hosted_no_fallback = true;
                s.hosted_code = Some("hosted_prepare_unsupported".into());
            })?;
            return Err(io::Error::other(
                "hosted_prepare_unsupported: hosted_no_fallback",
            ));
        }
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
        if self.config.hosted_runners {
            self.storage
                .hosted_route(id, crate::HostedJobState::new(&self.config))?;
            self.storage.hosted_update(id, |s| {
                s.hosted_no_fallback = true;
                s.hosted_code = Some("hosted_prepare_unsupported".into());
            })?;
            return Ok(prepare_refused(id, RefusalReason::RequestRejected)
                .with_refusal_detail("hosted_prepare_unsupported: hosted_no_fallback"));
        }
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
        let storage = self.storage.clone();
        let snapshots = tokio::task::spawn_blocking(move || {
            Snapshots::build(
                &config,
                &r.workspace_key,
                &r.repository_root,
                &r.siblings,
                &parent,
                id,
                Some(&r.base_commit),
                &storage,
            )
        })
        .await
        .map_err(io::Error::other)?;
        let snapshots = match snapshots {
            Ok(s) => s,
            Err(f) => {
                let detail = snapshot_refusal_detail(f.check, &self.spool_root, []);
                tracing::warn!(transfer_id = %id, check = detail, "snapshot refused");
                return Ok(
                    prepare_refused(id, RefusalReason::SnapshotFailed).with_refusal_detail(detail)
                );
            }
        };
        let mut channel = match open_for_request(self.connector.as_ref(), id).await {
            Ok(c) => c,
            Err((reason, detail)) => {
                let mut reply = prepare_refused(id, reason);
                reply.refusal_detail = detail.map(bounded_detail);
                return Ok(reply);
            }
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
fn request_refusal_fields(reason: RefusalReason) -> (RefusalReason, Option<String>) {
    match reason {
        RefusalReason::Unknown(code)
            if matches!(
                code.as_str(),
                "runner_credential_unavailable"
                    | "runner_credential_missing"
                    | "runner_credential_grant_refused"
                    | "runner_agent_start_failed"
                    | "runner_ssh_config_invalid"
                    | "runner_agent_stopped"
                    | "runner_sign_invalid_response"
                    | "runner_sign_missing"
                    | "runner_sign_grant_refused"
                    | "runner_sign_unavailable"
            ) =>
        {
            (RefusalReason::RunnerAuthenticationFailed, Some(code))
        }
        reason => (reason, None),
    }
}

async fn open_for_request(
    connector: &dyn Connector,
    id: Uuid,
) -> Result<Channel, (RefusalReason, Option<String>)> {
    Channel::open(connector, id)
        .await
        .map_err(request_refusal_fields)
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

fn snapshot_refusal_detail(
    mut detail: String,
    spool_root: &Path,
    environment_values: impl IntoIterator<Item = String>,
) -> String {
    // Skip short environment values such as "0" so redaction does not replace
    // unrelated diagnostic text.
    let mut redactions: Vec<_> = environment_values
        .into_iter()
        .chain(std::env::vars_os().filter_map(|(_, value)| value.into_string().ok()))
        .filter(|value| value.len() >= 8)
        .map(|value| (value, "client environment"))
        .collect();
    for path in [Some(spool_root), spool_root.parent()]
        .into_iter()
        .flatten()
    {
        if path != Path::new("/") && !path.as_os_str().is_empty() {
            redactions.push((path.to_string_lossy().into_owned(), "snapshot state"));
        }
    }
    redactions.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    for (value, replacement) in redactions {
        detail = detail.replace(&value, replacement);
    }
    // Redact before taking a line or truncating so a multiline secret cannot
    // leave an exposed prefix in the diagnostic.
    bounded_detail(detail.lines().next().unwrap_or_default().to_owned())
}

#[cfg(test)]
mod snapshot_detail_tests {
    use super::*;

    #[test]
    fn snapshot_refusal_keeps_first_error_line_and_redacts_before_bounding() {
        let secret = "private-first-line\nprivate-second-line";
        let state = Path::new("/snapshot-test-state/spools");
        let detail = snapshot_refusal_detail(
            format!("snapshot capture failed: fatal: {secret} at /snapshot-test-state/spools/index\nsecond stderr line"),
            state,
            [secret.to_owned()],
        );
        assert_eq!(
            detail,
            "snapshot capture failed: fatal: client environment at snapshot state/index"
        );
        let long = snapshot_refusal_detail(
            format!("snapshot capture failed: {}", "é".repeat(1000)),
            state,
            [],
        );
        assert!(long.len() <= REFUSAL_DETAIL_MAX_BYTES);
        assert!(long.len() >= REFUSAL_DETAIL_MAX_BYTES - 1);
    }
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

#[cfg(test)]
#[path = "resubmit_tests.rs"]
mod resubmit_tests;

#[cfg(test)]
#[path = "post_connect_tests.rs"]
mod post_connect_tests;

fn incoming_cause(incoming: &io::Result<Incoming>) -> String {
    match incoming {
        Err(error) => error.to_string(),
        Ok(Incoming::Frame(frame)) => format!(
            "unexpected {} frame",
            serde_json::to_value(frame)
                .ok()
                .and_then(|value| value["type"].as_str().map(str::to_owned))
                .unwrap_or_default()
        ),
        Ok(Incoming::Unknown(_)) => "unknown frame".into(),
    }
}

fn log_refusal(id: Uuid, refusal: &Refusal) {
    tracing::info!(job_id = %id, reason = ?refusal.reason,
        retry_after_ms = refusal.retry_after_ms, "runner admission refused");
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    struct TerminalPeer;
    impl Connector for TerminalPeer {
        fn connect(
            &self,
            id: Uuid,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = io::Result<Box<dyn crate::transport::Duplex>>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                let (client, mut peer) = tokio::io::duplex(65536);
                tokio::spawn(async move {
                    assert!(matches!(
                        read_frame_async(&mut peer).await.unwrap(),
                        Some(Frame::Hello { .. })
                    ));
                    write_frame_async(
                        &mut peer,
                        &Frame::Hello {
                            return_paths: false,
                            protocol_version: PROTOCOL_VERSION,
                            started: false,
                            default_branch_tip: false,
                            gate_jobs: false,
                            platforms: Vec::new(),
                        },
                    )
                    .await
                    .unwrap();
                    loop {
                        match read_frame_async(&mut peer).await.unwrap() {
                            Some(Frame::Heartbeat { .. }) => continue,
                            Some(Frame::Attach(request)) => {
                                assert_eq!(request.job_id, id);
                                break;
                            }
                            other => panic!("expected attach, got {other:?}"),
                        }
                    }
                    write_frame_async(
                        &mut peer,
                        &Frame::Terminal(
                            TerminalRecord::new(id, Outcome::Exit { code: 0 }, 1, 0, 0)
                                .with_ran(Ran::Remote),
                        ),
                    )
                    .await
                    .unwrap();
                });
                Ok(Box::new(client) as Box<dyn crate::transport::Duplex>)
            })
        }
    }

    #[tokio::test]
    async fn remote_terminal_is_observed_before_delivery_is_acknowledged() {
        let scratch = ScratchDir::new("motor-remote-terminal-delivery");
        let motor =
            Motor::with_connector(Config::default(), &scratch, Arc::new(TerminalPeer)).unwrap();
        let id = Uuid::now_v7();
        motor.storage.create(id, 60, None, None, false).unwrap();
        let mut stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
        let record = tokio::time::timeout(Duration::from_secs(5), stream.records.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(record, StreamRecord::Terminal(_)));
        let job = motor.storage.job(id).unwrap();
        assert!(job.terminal_observed_at_ms.is_some());
        assert!(job.terminal_delivered_at_ms.is_none());
        assert!(
            job.local_terminal.is_none(),
            "remote replay must still include the runner's output"
        );
        stream.acknowledge_terminal().unwrap();
        assert!(motor
            .storage
            .job(id)
            .unwrap()
            .terminal_delivered_at_ms
            .is_some());
    }

    #[tokio::test]
    async fn terminal_delivery_is_durable_only_after_successful_forwarding() {
        let scratch = ScratchDir::new("motor-terminal-delivery");
        let motor = Motor::open(Config::default(), &scratch).unwrap();
        let stream = motor.refuse_capacity().unwrap();
        let id = stream.job_id;
        let result = crate::module::forward(std::future::pending(), stream, |_| async {
            Err(io::Error::other("caller disconnected"))
        })
        .await;
        assert!(result.is_err());
        let job = motor.storage.job(id).unwrap();
        assert!(job.terminal_observed_at_ms.is_some());
        assert!(job.terminal_delivered_at_ms.is_none());

        let stream = motor.attach(AttachRequest::new(id, 0)).unwrap();
        crate::module::forward(std::future::pending(), stream, |record| async move {
            assert!(matches!(record, StreamRecord::Terminal(_)));
            Ok(())
        })
        .await
        .unwrap();
        let job = motor.storage.job(id).unwrap();
        assert!(job.terminal_delivered_at_ms.is_some());
        assert!(job.retain_until_ms.unwrap() > job.terminal_delivered_at_ms.unwrap());
        drop(motor);
        let reopened = Motor::open(Config::default(), &scratch).unwrap();
        assert!(reopened
            .storage
            .job(id)
            .unwrap()
            .terminal_delivered_at_ms
            .is_some());
    }
}
