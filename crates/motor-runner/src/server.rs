#[path = "server_activity.rs"]
mod activity;
use crate::{
    engine::{
        transfer_refusal, transfer_refusal_detail, workspace_setup_detail, Admission, QueueProgress,
    },
    spool::Upload,
    Runner,
};
use activity::{Activity, ActivityRead};
use motor_protocol::*;
use std::{io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{UnixListener, UnixStream},
    sync::{mpsc, watch},
};
use tracing::Instrument;

fn unix_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct TenantState {
    admitted: bool,
    closed: bool,
    created_record: bool,
    cause: RefusedCause,
}

struct TenantConnection {
    runner: Runner,
    lease: VerifiedLease,
    state: tokio::sync::Mutex<TenantState>,
    changed: tokio::sync::Notify,
    stop_write: tokio::sync::Notify,
}

/// The wire `type` tag of a frame, for logs. Only refused frames reach this,
/// and the connection closes right after, so serializing the frame is cheap
/// enough here.
fn frame_type(frame: &Frame) -> String {
    serde_json::to_value(frame)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

impl TenantConnection {
    async fn admitted(&self) -> bool {
        self.state.lock().await.admitted
    }
    fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(self.lease.exp.saturating_sub(unix_s()))
    }
    async fn expire(&self) -> bool {
        let mut state = self.state.lock().await;
        if state.admitted {
            return false;
        }
        state.closed = true;
        state.cause = RefusedCause::Expired;
        self.changed.notify_one();
        true
    }
    /// Logs why this tenant connection is being closed. `frame` names the
    /// refused frame's type (or why no frame could be decoded), never its
    /// contents, so a refusal names itself without copying peer data.
    fn operation_refused(&self, frame: &str, reason: &'static str) {
        tracing::warn!(jti = %self.lease.jti, frame, reason, "operation_refused");
    }
    fn allowed(&self, frame: &Frame, hello: &mut bool, ran: &mut bool) -> bool {
        if !*hello {
            *hello = matches!(frame, Frame::Hello { .. });
            return *hello;
        }
        let run = self.lease.purpose == LeasePurpose::Run;
        match frame {
            // ck-motor's hosted channel writes a heartbeat every second after
            // hello on run and attach connections alike. It carries no
            // authority and the session skips it, so it must not close the
            // connection.
            Frame::Heartbeat { .. } => true,
            Frame::Have { .. } | Frame::BundleChunk { .. } => run,
            Frame::Run(request) if run && !*ran && request.job_id == self.lease.jti => {
                if self.runner.has_job_record(self.lease.jti) {
                    return false;
                }
                *ran = true;
                true
            }
            Frame::Attach(request) => request.job_id == self.lease.jti,
            Frame::Cancel(request) => request.job_id == self.lease.jti,
            Frame::Withdraw { job_id, .. } | Frame::JobStatus { job_id } => {
                *job_id == self.lease.jti
            }
            _ => false,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) async fn tenant_channel<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    runner: Runner,
    mut socket: S,
) -> io::Result<()> {
    let accepted = std::time::Instant::now();
    // Do not use read_frame_async here: it skips unknown records for SSH's
    // additive compatibility, whereas this boundary must fail closed.
    let lease =
        match tokio::time::timeout(Duration::from_secs(60), read_record_async(&mut socket)).await {
            Ok(Ok(Some(DecodedFrame::Frame(Frame::VerifiedLease(lease))))) => lease,
            _ => return Ok(()),
        };
    let ctx = Arc::new(TenantConnection {
        runner: runner.clone(),
        lease,
        state: tokio::sync::Mutex::new(TenantState {
            admitted: false,
            closed: false,
            created_record: false,
            cause: RefusedCause::Disconnect,
        }),
        changed: tokio::sync::Notify::new(),
        stop_write: tokio::sync::Notify::new(),
    });
    channel_stream(runner, socket, accepted, Some(ctx)).await
}

/// The daemon owns no TCP listener. Connections use the pinned framed protocol;
/// heartbeats are transport-only and never enter retained job history.
pub async fn serve(runner: Runner, listener: UnixListener) -> io::Result<()> {
    let mut channels = tokio::task::JoinSet::new();
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    loop {
        let (socket, _) = tokio::select! {
            socket = listener.accept() => socket?,
            Some(_) = channels.join_next(), if !channels.is_empty() => continue,
            _ = poll.tick() => {
                if runner.is_draining() && runner.drain_should_exit() { break; }
                continue;
            }
        };
        let accepted = std::time::Instant::now();
        let connection_id = Uuid::now_v7();
        let span = tracing::info_span!("runner connection", %connection_id);
        tracing::info!(%connection_id, "runner connection accepted");
        let runner = runner.clone();
        channels.spawn(
            async move {
                if let Err(error) = channel(runner, socket, accepted).await {
                    tracing::warn!(%error, "runner channel detached");
                }
            }
            .instrument(span),
        );
    }
    // Let terminal and drain acknowledgements flush before closing connections.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while channels.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

async fn reader<R: AsyncRead + Unpin>(
    read: R,
    tx: mpsc::Sender<Frame>,
    activity: watch::Sender<Activity>,
    tenant: Option<Arc<TenantConnection>>,
) {
    let mut read = ActivityRead {
        read,
        activity,
        idle: Box::pin(tokio::time::sleep(Duration::from_secs(60))),
    };
    let mut hello = false;
    let mut ran = false;
    loop {
        let result = if let Some(ctx) = &tenant {
            tokio::select! {
                result = read_record_async(&mut read) => match result {
                    Ok(Some(DecodedFrame::Frame(frame))) => Ok(Some(frame)),
                    Ok(None) => Ok(None),
                    Ok(Some(DecodedFrame::Unknown(_))) => { ctx.operation_refused("unknown", "unknown frame type"); break; }
                    Err(error) => {
                        // The decode error text can quote peer bytes; log its kind only.
                        tracing::warn!(jti = %ctx.lease.jti, kind = ?error.kind(), "tenant frame read failed");
                        ctx.operation_refused("undecodable", "frame read failed");
                        break;
                    }
                },
                _ = tokio::time::sleep_until(ctx.deadline()), if ctx.lease.purpose == LeasePurpose::Run && !ctx.admitted().await => {
                    if ctx.expire().await { break; }
                    continue;
                }
            }
        } else {
            read_frame_async(&mut read).await
        };
        let frame = match result {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "runner frame read/decode failed");
                break;
            }
        };
        if let Some(ctx) = &tenant {
            if !ctx.allowed(&frame, &mut hello, &mut ran) {
                ctx.operation_refused(&frame_type(&frame), "not allowed on this tenant connection");
                break;
            }
        }
        read.activity.send_modify(|p| {
            p.partial_at = None;
            if matches!(&frame, Frame::BundleChunk { data, .. } if !data.0.is_empty()) {
                p.chunk_at = Some(tokio::time::Instant::now());
            }
        });
        if tx.send(frame).await.is_err() {
            break;
        }
    }
    if let Some(ctx) = tenant {
        ctx.state.lock().await.closed = true;
        ctx.changed.notify_one();
        ctx.stop_write.notify_one();
    }
}

async fn writer<W: AsyncWrite + Unpin>(
    mut write: W,
    mut rx: mpsc::Receiver<Frame>,
    accepted: std::time::Instant,
    job: watch::Receiver<Option<QueueProgress>>,
    tenant: Option<Arc<TenantConnection>>,
) {
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    loop {
        let frame = tokio::select! {
            frame = rx.recv() => match frame { Some(f) => f, None => break },
            _ = heartbeat.tick() => Frame::Heartbeat {
                queue_wait_ms: job.borrow().as_ref().and_then(QueueProgress::elapsed_ms),
            },
        };
        if let Some(ctx) = &tenant {
            if ctx.state.lock().await.closed {
                break;
            }
        }
        let result = if let (Some(ctx), Frame::Accepted(_)) = (&tenant, &frame) {
            let mut state = ctx.state.lock().await;
            if state.closed || unix_s() >= ctx.lease.exp {
                state.cause = RefusedCause::Expired;
                state.closed = true;
                ctx.changed.notify_one();
                break;
            }
            let result = match tokio::time::timeout_at(
                ctx.deadline(),
                write_frame_async(&mut write, &frame),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    state.cause = RefusedCause::Expired;
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "tenant admission expired during accepted write",
                    ))
                }
            };
            if result.is_ok() {
                // Upload and execution wait for this write and the durable event,
                // so detached work cannot outrun its admission record.
                state.admitted = true;
                if let Err(error) = ctx.runner.tenant_admitted(ctx.lease.jti) {
                    tracing::error!(%error, "cannot persist tenant admission");
                    state.closed = true;
                    ctx.changed.notify_one();
                    break;
                }
            } else {
                state.closed = true;
            }
            ctx.changed.notify_one();
            result
        } else if let Some(ctx) = &tenant {
            tokio::select! {
                result = write_frame_async(&mut write, &frame) => result,
                _ = ctx.stop_write.notified() => break,
            }
        } else {
            write_frame_async(&mut write, &frame).await
        };
        if let Err(error) = result {
            let frame_type = match &frame {
                Frame::Hello { .. } => "hello",
                Frame::Started { .. } => "started",
                Frame::Output(_) => "output",
                Frame::Terminal(_) => "terminal",
                _ => "control",
            };
            tracing::warn!(%error, frame_type, "runner frame write failed");
            break;
        }
        if matches!(frame, Frame::Hello { .. }) {
            tracing::info!(
                elapsed_ms = accepted.elapsed().as_millis() as u64,
                "runner hello written"
            );
        }
    }
}

async fn send(tx: &mpsc::Sender<Frame>, frame: Frame) -> io::Result<()> {
    tx.send(frame)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "channel closed"))
}

/// Run a `Runner` call that takes the runner's state lock on the blocking pool.
/// Long filesystem and git work (materializing or deleting a workspace,
/// reflinking a warm target, fetching a bundle) runs outside that lock, but it
/// is still a blocking mutex that persists state with fsync. Waiting for it on
/// an async worker parks the worker; with every worker parked the runtime
/// stops serving other connections, and a new channel's Hello then goes
/// unanswered until ck-motor gives up after its 15-second handshake bound.
async fn locked<T: Send + 'static>(
    runner: &Runner,
    call: impl FnOnce(Runner) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let runner = runner.clone();
    tokio::task::spawn_blocking(move || call(runner))
        .await
        .map_err(io::Error::other)?
}

async fn next(rx: &mut mpsc::Receiver<Frame>) -> Option<Frame> {
    while let Some(frame) = rx.recv().await {
        if !matches!(frame, Frame::Heartbeat { .. }) {
            return Some(frame);
        }
    }
    None
}

async fn channel(
    runner: Runner,
    socket: UnixStream,
    accepted: std::time::Instant,
) -> io::Result<()> {
    channel_stream(runner, socket, accepted, None).await
}

async fn channel_stream<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    runner: Runner,
    socket: S,
    accepted: std::time::Instant,
    tenant: Option<Arc<TenantConnection>>,
) -> io::Result<()> {
    let (read, write) = tokio::io::split(socket);
    let (in_tx, mut rx) = mpsc::channel(8);
    let (tx, out_rx) = mpsc::channel(16);
    let mut tasks = tokio::task::JoinSet::new();
    let (activity, mut progress) = watch::channel(Activity::default());
    let reading = tasks.spawn(reader(read, in_tx, activity, tenant.clone()));
    let (live_job, job) = watch::channel(None);
    tasks.spawn(writer(write, out_rx, accepted, job, tenant.clone()).in_current_span());
    let result = session(
        runner,
        &mut rx,
        &tx,
        accepted,
        &mut progress,
        &live_job,
        tenant.as_ref(),
    )
    .await;
    reading.abort();
    drop(tx);
    while tasks.join_next().await.is_some() {}
    if let Some(ctx) = tenant {
        let state = ctx.state.lock().await;
        if !state.admitted && ctx.lease.purpose == LeasePurpose::Run {
            if state.created_record {
                ctx.runner.forget_unadmitted(ctx.lease.jti)?;
            }
            ctx.runner
                .tenant_events()
                .ok_or_else(|| io::Error::other("tenant event log is not configured"))?
                .refused(ctx.lease.jti, state.cause)?;
        }
    }
    result
}

async fn session(
    runner: Runner,
    rx: &mut mpsc::Receiver<Frame>,
    tx: &mpsc::Sender<Frame>,
    accepted: std::time::Instant,
    progress: &mut watch::Receiver<Activity>,
    live_job: &watch::Sender<Option<QueueProgress>>,
    tenant: Option<&Arc<TenantConnection>>,
) -> io::Result<()> {
    let Some(Frame::Hello {
        protocol_version,
        started: supports_started,
        default_branch_tip: supports_default_branch_tip,
        gate_jobs: supports_gate_jobs,
        ..
    }) = next(rx).await
    else {
        return Ok(());
    };
    tracing::info!(
        elapsed_ms = accepted.elapsed().as_millis() as u64,
        protocol_version,
        supports_started,
        supports_default_branch_tip,
        "runner hello received"
    );
    send(
        tx,
        Frame::Hello {
            return_paths: !runner.config().windows_instance(),
            protocol_version: PROTOCOL_VERSION,
            started: supports_started,
            default_branch_tip: supports_default_branch_tip,
            gate_jobs: supports_gate_jobs,
            platforms: match runner.config().served_platform() {
                Platform::Windows => vec!["windows".into()],
                _ => Vec::new(),
            },
        },
    )
    .await?;
    if protocol_version != PROTOCOL_VERSION {
        return Ok(());
    }
    while let Some(frame) = next(rx).await {
        if let Some(ctx) = tenant {
            if ctx.state.lock().await.closed {
                return Ok(());
            }
        }
        match frame {
            Frame::Have {
                repositories,
                git_dependencies,
            } => {
                let r = runner.clone();
                let (held, dependencies) = tokio::task::spawn_blocking(move || {
                    Ok::<_, io::Error>((
                        r.have(repositories),
                        r.have_dependencies(git_dependencies)?,
                    ))
                })
                .await
                .map_err(io::Error::other)??;
                send(
                    tx,
                    Frame::HaveReply {
                        repositories: held,
                        git_dependencies: Some(dependencies),
                    },
                )
                .await?;
            }
            Frame::Run(mut request) => {
                if let Some(ctx) = tenant {
                    if !crate::tenant::matches(&ctx.lease, &request) {
                        ctx.state.lock().await.cause = RefusedCause::Mismatch;
                        send(
                            tx,
                            Frame::Refusal(Refusal {
                                job_id: Some(request.job_id),
                                transfer_id: None,
                                reason: RefusalReason::RequestRejected,
                                disk_full: None,
                                retry_after_ms: None,
                                detail: None,
                            }),
                        )
                        .await?;
                        return Ok(());
                    }
                }
                request.gate_job &= supports_gate_jobs;
                let admitted = request.clone();
                match locked(&runner, move |r| r.accept(admitted)).await? {
                    Admission::Duplicate => {
                        if let Some(ctx) = tenant {
                            ctx.operation_refused("run", "job already has a record");
                        }
                        return Ok(());
                    }
                    Admission::Refused(reason) => {
                        if let Some(ctx) = tenant {
                            ctx.state.lock().await.cause = RefusedCause::Rejected;
                        }
                        let id = request.job_id;
                        // The detail only helps the caller: if the record can't
                        // be read, the refusal still goes out without it.
                        let detail = locked(&runner, move |r| {
                            Ok(r.job_status(id).ok().and_then(|status| {
                                status.terminal.and_then(|terminal| {
                                    terminal.refusal_detail().map(str::to_owned)
                                })
                            }))
                        })
                        .await?;
                        send(
                            tx,
                            Frame::Refusal(Refusal {
                                job_id: Some(request.job_id),
                                transfer_id: None,
                                reason,
                                disk_full: None,
                                retry_after_ms: runner.drain_retry_after_ms(),
                                detail,
                            }),
                        )
                        .await?;
                        return Ok(());
                    }
                    Admission::Accepted(queue_position) => {
                        if let Some(ctx) = tenant {
                            ctx.state.lock().await.created_record = true;
                        }
                        let id = request.job_id;
                        let progress = locked(&runner, move |r| Ok(r.queue_progress(id))).await?;
                        live_job.send_replace(Some(progress));
                        // A lost acknowledgement is still an interrupted upload,
                        // not an unrecorded run which could later be replayed.
                        let mut accepted = Accepted::new(request.job_id, queue_position)
                            .with_platform(runner.config().served_platform());
                        if let Some(network) = &request.network {
                            accepted = accepted.with_network(network.clone());
                        }
                        if let Err(e) = send(tx, Frame::Accepted(accepted)).await {
                            let id = request.job_id;
                            let error = e.to_string();
                            locked(&runner, move |r| {
                                r.refuse(id, RefusalReason::TransferInterrupted, error)
                            })
                            .await?;
                            return Err(e);
                        }
                        if let Some(ctx) = tenant {
                            loop {
                                let notified = ctx.changed.notified();
                                let state = ctx.state.lock().await;
                                if state.closed {
                                    return Ok(());
                                }
                                if state.admitted {
                                    break;
                                }
                                drop(state);
                                notified.await;
                            }
                        }
                    }
                    Admission::DiskFull(full) => {
                        if let Some(ctx) = tenant {
                            ctx.state.lock().await.cause = RefusedCause::Rejected;
                        }
                        send(
                            tx,
                            Frame::Refusal(Refusal {
                                job_id: Some(request.job_id),
                                transfer_id: None,
                                reason: RefusalReason::RunnerDiskFull,
                                disk_full: Some(full.clone()),
                                retry_after_ms: None,
                                detail: Some(full.diagnostic()),
                            }),
                        )
                        .await?;
                        return Ok(());
                    }
                }
                let _transfer = runner.transfer(request.job_id);
                let started = std::time::Instant::now();
                let manifests = all_bundles(&request.bundles, &request.git_dependencies);
                match upload(&runner, request.job_id, &manifests, rx, Some(progress)).await {
                    Ok(upload) => {
                        tracing::info!(
                            job_id = %request.job_id,
                            bytes = manifests.iter().map(|b| b.length).sum::<u64>(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "bundle upload complete"
                        );
                        let r = runner.clone();
                        let req = request.clone();
                        // The task is not tied to the socket writer. Once all
                        // digests pass, EOF cannot invalidate this job's proof.
                        tokio::task::spawn_blocking(move || {
                            let result =
                                r.fetch_run(&req, &upload.directory, supports_default_branch_tip);
                            drop(upload);
                            result
                        })
                        .await
                        .map_err(io::Error::other)??;
                    }
                    Err((reason, error)) => {
                        let id = request.job_id;
                        locked(&runner, move |r| r.refuse(id, reason, error)).await?;
                    }
                }
                return attach(
                    &runner,
                    request.job_id,
                    0,
                    supports_started,
                    rx,
                    tx,
                    tenant.is_some(),
                )
                .await;
            }
            Frame::Prepare(request) => {
                let req = request.clone();
                let _transfer = match locked(&runner, move |r| r.admit_prepare(&req)).await? {
                    Ok(guard) => guard,
                    Err(refusal) => {
                        tracing::warn!(
                            transfer_id = %request.transfer_id,
                            reason = ?refusal.reason,
                            detail = ?refusal.detail,
                            "prepare refused"
                        );
                        send(tx, Frame::Refusal(refusal)).await?;
                        return Ok(());
                    }
                };
                send(
                    tx,
                    Frame::PrepareAccepted {
                        transfer_id: request.transfer_id,
                    },
                )
                .await?;
                let started = std::time::Instant::now();
                let manifests = all_bundles(&request.bundles, &request.git_dependencies);
                let result = match upload(
                    &runner,
                    request.transfer_id,
                    &manifests,
                    rx,
                    Some(progress),
                )
                .await
                {
                    Ok(upload) => {
                        tracing::info!(
                            transfer_id = %request.transfer_id,
                            workspace = %request.workspace_key,
                            bytes = manifests.iter().map(|b| b.length).sum::<u64>(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "prepare upload complete"
                        );
                        let r = runner.clone();
                        let req = request.clone();
                        tokio::task::spawn_blocking(move || {
                            let result = r.fetch_prepare(&req, &upload.directory);
                            drop(upload);
                            result
                        })
                        .await
                        .map_err(io::Error::other)?
                        .map_err(|error| {
                            let detail = runner.safe_refusal_detail(
                                transfer_refusal_detail(&error),
                                std::iter::empty(),
                            );
                            (transfer_refusal(&error), detail)
                        })
                    }
                    Err((reason, error)) => {
                        let detail = runner.safe_refusal_detail(&error, std::iter::empty());
                        tracing::warn!(transfer_id = %request.transfer_id, ?reason, %error, detail, "prepare upload failed");
                        Err((reason, detail))
                    }
                };
                let result = if result.is_ok() {
                    loop {
                        let r = runner.clone();
                        let req = request.clone();
                        match tokio::task::spawn_blocking(move || {
                            r.prepare(&req, supports_default_branch_tip)
                        })
                        .await
                        .map_err(io::Error::other)?
                        {
                            Ok(()) => break Ok(()),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                tokio::time::sleep(Duration::from_millis(25)).await
                            }
                            Err(error) => {
                                let detail = runner.safe_refusal_detail(
                                    workspace_setup_detail("prepare workspace", &error),
                                    std::iter::empty(),
                                );
                                break Err((RefusalReason::WorkspaceSetupFailed, detail));
                            }
                        }
                    }
                } else {
                    result
                };
                let frame = match result {
                    Ok(()) => Frame::Prepared {
                        transfer_id: request.transfer_id,
                    },
                    Err((reason, detail)) => {
                        tracing::warn!(transfer_id = %request.transfer_id, ?reason, detail, "prepare refused");
                        Frame::Refusal(Refusal {
                            job_id: None,
                            transfer_id: Some(request.transfer_id),
                            reason,
                            disk_full: None,
                            retry_after_ms: None,
                            detail: Some(detail),
                        })
                    }
                };
                send(tx, frame).await?;
                return Ok(());
            }
            Frame::Attach(request) => {
                let id = request.job_id;
                let progress = locked(&runner, move |r| Ok(r.queue_progress(id))).await?;
                live_job.send_replace(Some(progress));
                return attach(
                    &runner,
                    request.job_id,
                    request.from_seq,
                    supports_started,
                    rx,
                    tx,
                    tenant.is_some(),
                )
                .await;
            }
            Frame::Cancel(request) => {
                let id = request.job_id;
                locked(&runner, move |r| r.cancel(id)).await?;
                send(tx, Frame::CancelReply(CancelReply::new(request.job_id))).await?;
                if tenant.is_some() {
                    continue;
                }
                return Ok(());
            }
            Frame::Withdraw {
                job_id,
                reason,
                spill,
            } => {
                let outcome =
                    locked(&runner, move |r| r.withdraw_for(job_id, reason, spill)).await?;
                send(tx, Frame::WithdrawReply { outcome }).await?;
                if tenant.is_some() {
                    continue;
                }
                return Ok(());
            }
            Frame::Status => {
                let status = locked(&runner, |r| Ok(r.runner_status())).await?;
                send(tx, Frame::StatusReply(status)).await?;
                return Ok(());
            }
            Frame::JobStatus { job_id } => {
                let status = locked(&runner, move |r| r.job_status(job_id)).await?;
                send(tx, Frame::JobStatusReply(status)).await?;
                if tenant.is_some() {
                    continue;
                }
                return Ok(());
            }
            Frame::Resume => {
                locked(&runner, |r| {
                    r.resume();
                    Ok(())
                })
                .await?;
                send(tx, Frame::ResumeReply { resumed: true }).await?;
                return Ok(());
            }
            Frame::Drain {
                deadline_s,
                exit_after,
            } => {
                let waiter = locked(&runner, move |r| {
                    Ok(r.begin_drain_request(
                        Duration::from_secs(deadline_s.min(86400)),
                        exit_after,
                    ))
                })
                .await?;
                loop {
                    let waiter = waiter.clone();
                    if let Some((timed_out, jobs)) =
                        locked(&runner, move |r| Ok(r.drain_reply_for(&waiter))).await?
                    {
                        send(
                            tx,
                            Frame::DrainReply {
                                timed_out,
                                restart_recoverable_jobs: Some(jobs),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            Frame::Drop(request) => {
                send(
                    tx,
                    Frame::DropReply(runner.drop_workspace(&request.workspace_key).await?),
                )
                .await?;
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected frame",
                ))
            }
        }
    }
    Ok(())
}

async fn upload(
    runner: &Runner,
    id: Uuid,
    manifest: &[BundleManifest],
    rx: &mut mpsc::Receiver<Frame>,
    mut wire_progress: Option<&mut watch::Receiver<Activity>>,
) -> Result<Upload, (RefusalReason, io::Error)> {
    let mut upload = Upload::new(runner.spool_dir(id), manifest)
        .map_err(|error| (RefusalReason::BundleRejected, error))?;
    let mut progress_at = tokio::time::Instant::now();
    while !upload.complete() {
        let active_at = wire_progress
            .as_ref()
            .map(|rx| *rx.borrow())
            .map(|p| {
                p.chunk_at
                    .into_iter()
                    .chain(p.partial_at)
                    .fold(progress_at, std::cmp::max)
            })
            .unwrap_or(progress_at);
        let frame = tokio::select! {
            frame = tokio::time::timeout_at(active_at + Duration::from_secs(60), next(rx)) =>
                frame.map_err(|error| (RefusalReason::TransferInterrupted, io::Error::new(io::ErrorKind::TimedOut, format!("transfer_stalled: {error}"))))?,
            _ = async { if let Some(rx) = wire_progress.as_mut() { let _ = rx.changed().await; } else { std::future::pending::<()>().await; } } => continue,
        };
        let Some(frame) = frame else {
            return Err((
                RefusalReason::TransferInterrupted,
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    if tokio::time::Instant::now().duration_since(active_at)
                        >= Duration::from_secs(60)
                    {
                        "transfer_stalled: bundle upload stream ended before completion"
                    } else {
                        "bundle upload stream ended before completion"
                    },
                ),
            ));
        };
        let advanced = matches!(&frame, Frame::BundleChunk { data, .. } if !data.0.is_empty());
        upload
            .chunk(frame)
            .map_err(|error| (RefusalReason::BundleRejected, error))?;
        if advanced {
            progress_at = tokio::time::Instant::now();
        }
    }
    Ok(upload)
}

async fn attach(
    runner: &Runner,
    id: Uuid,
    mut from: u64,
    started: bool,
    rx: &mut mpsc::Receiver<Frame>,
    tx: &mpsc::Sender<Frame>,
    tenant: bool,
) -> io::Result<()> {
    tracing::info!(job_id = %id, from_seq = from, supports_started = started, "runner job stream attached");
    loop {
        let r = runner.clone();
        let frames = tokio::task::spawn_blocking(move || {
            if started {
                r.replay(id, from)
            } else {
                r.replay_legacy(id, from)
            }
        })
        .await
        .map_err(io::Error::other)??;
        for frame in frames {
            match &frame {
                Frame::Output(output) => from = output.seq + 1,
                Frame::Started { seq, .. } => from = seq + 1,
                Frame::ReturnResult(result) => from = result.seq + 1,
                _ => {}
            }
            let terminal = matches!(frame, Frame::Terminal(_));
            send(tx, frame).await?;
            if terminal {
                return Ok(());
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            frame = next(rx) => match frame {
                None => return Ok(()),
                Some(Frame::Cancel(_)) => {
                    locked(runner, move |r| r.cancel(id)).await?;
                    if tenant { send(tx, Frame::CancelReply(CancelReply::new(id))).await?; }
                },
                Some(Frame::Attach(request)) if tenant => from = request.from_seq,
                Some(Frame::JobStatus { .. }) if tenant => {
                    let status = locked(runner, move |r| r.job_status(id)).await?;
                    send(tx, Frame::JobStatusReply(status)).await?;
                },
                Some(Frame::Withdraw { reason, spill, .. }) => {
                    let outcome = locked(runner, move |r| r.withdraw_for(id, reason, spill)).await?;
                    send(tx, Frame::WithdrawReply { outcome }).await?;
                },
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected attach frame")),
            }
        }
    }
}

fn all_bundles(
    workspace: &[BundleManifest],
    dependencies: &[GitDependency],
) -> Vec<BundleManifest> {
    workspace
        .iter()
        .chain(dependencies.iter().filter_map(|d| d.bundle.as_ref()))
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "server_transfer_tests.rs"]
mod transfer_tests;
