use super::*;
use crate::shadow::Capture;

impl Motor {
    pub async fn shutdown_shadows(&self) {
        self.shadows.shutdown().await;
    }
    pub(crate) fn shadow_health(&self) -> serde_json::Value {
        self.shadows
            .health()
            .unwrap_or_else(|error| serde_json::json!({"error": error.to_string()}))
    }
    pub(crate) fn shadow_report(&self, since: u64) -> io::Result<Vec<crate::shadow::Record>> {
        self.shadows.report(since)
    }
    pub(super) fn start_shadow(
        &self,
        id: Uuid,
        request: RunRequest,
        mut run: Run,
        snapshots: Arc<Snapshots>,
    ) {
        let Some((permit, primary, mut stop)) = self.shadows.begin(id, &self.config) else {
            return;
        };
        let motor = self.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let shadow_id = Uuid::now_v7();
            let mut route = crate::HostedJobState::new(&motor.config);
            route.avoid_host = motor.config.spill_avoid_host.clone();
            let client = motor.hosted_client(route);
            let mut booked = None;
            let mut capture = Capture::default();
            let mut channel = None;
            let mut run_sent = false;
            // The deadline bounds lease acquisition, upload and the command, not
            // just execution. Shadows never reattach or resubmit after a loss.
            let deadline = Duration::from_secs(request.timeout.unwrap_or(0).saturating_add(120))
                + crate::hosted::SHADOW_PROVISIONING_BUDGET;
            #[cfg(test)]
            let deadline = motor.shadow_deadline.unwrap_or(deadline);
            let execution = motor.execute_shadow(
                shadow_id,
                &request,
                &mut run,
                snapshots,
                &client,
                &mut booked,
                &mut channel,
                &mut capture,
                &mut run_sent,
            );
            let result = tokio::select! {
                _ = stop.changed() => Err(io::Error::other("motor shutdown")),
                result = tokio::time::timeout(deadline, execution) => result.unwrap_or_else(|_| {
                    tracing::warn!(job_id = %id, %shadow_id, request_timeout_s = request.timeout,
                        deadline_s = deadline.as_secs(), "shadow deadline exceeded");
                    Err(io::Error::other("shadow deadline exceeded"))
                }),
            };
            let (terminal, failure) = match result {
                Ok(terminal) => {
                    let failure = matches!(
                        terminal.outcome,
                        Outcome::RefusedBeforeStart { .. } | Outcome::OutcomeUnknown
                    )
                    .then(|| "shadow runner refused or lost outcome".to_owned());
                    (terminal, failure)
                }
                Err(error) => {
                    tracing::warn!(job_id = %id, %shadow_id, cause = %error, "shadow failed; caller unaffected");
                    if let Some(channel) = channel.as_ref() {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            channel.send(Frame::Cancel(CancelRequest::new(shadow_id))),
                        )
                        .await;
                    }
                    (
                        TerminalRecord::new(
                            shadow_id,
                            if run_sent {
                                Outcome::OutcomeUnknown
                            } else {
                                Outcome::RefusedBeforeStart {
                                    reason: RefusalReason::Unreachable,
                                }
                            },
                            0,
                            0,
                            0,
                        ),
                        Some(error.to_string()),
                    )
                }
            };
            client
                .cleanup_shadow(
                    shadow_id,
                    Some(&terminal),
                    booked.as_ref().map_or(0, |l| l.max_s),
                    booked.is_some(),
                )
                .await;
            drop(channel);
            let side = capture.finish(&terminal);
            if *stop.borrow() {
                motor.shadows.forget(id);
                return;
            }
            let primary = tokio::select! {
                _ = stop.changed() => None,
                result = tokio::time::timeout(deadline, primary) => result.ok().and_then(Result::ok),
            };
            motor.shadows.forget(id);
            if let Some(primary) = primary {
                let record = crate::shadow::Record::compare(id, &request, primary, side, failure);
                let manager = motor.shadows.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(error) = manager.append(&record) {
                        tracing::warn!(%error, "shadow log append failed");
                    }
                })
                .await;
            }
        });
        self.shadows.track(task);
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_shadow(
        &self,
        id: Uuid,
        request: &RunRequest,
        run: &mut Run,
        snapshots: Arc<Snapshots>,
        client: &crate::hosted::Client,
        booked: &mut Option<crate::hosted::placement::Lease>,
        channel: &mut Option<Channel>,
        capture: &mut Capture,
        run_sent: &mut bool,
    ) -> io::Result<TerminalRecord> {
        let salt = self.storage.hosted_salt()?;
        *channel = Some(
            client
                .shadow_run(id, request, &salt, booked)
                .await
                .map_err(|e| {
                    io::Error::other(match client.broker_code() {
                        Some(code) => format!("{}: {code}", e.code()),
                        None => e.code().to_owned(),
                    })
                })?,
        );
        let c = channel.as_mut().unwrap();
        let held = self
            .have(
                c,
                snapshots.candidates(&Default::default()),
                snapshots.git_dependencies.keys().cloned().collect(),
                id,
                UnreachableStep::HaveExchange,
            )
            .await?;
        let spool_root = self.spool_root.clone();
        // Separate bundles avoid racing the primary upload, while the captured
        // commit/tree and dependency list remain exactly the primary snapshot.
        let transfer = tokio::task::spawn_blocking(move || {
            let copy = Snapshots {
                main: snapshots.main.clone(),
                siblings: snapshots.siblings.clone(),
                git_dependencies: snapshots.git_dependencies.clone(),
                spool: snapshot::Spool::new(&spool_root, id)?,
            };
            let bundles = copy.transfer_bundles(id, &held.0, held.1.as_deref())?;
            Ok::<_, io::Error>((copy, bundles))
        })
        .await
        .map_err(io::Error::other)??;
        let (snapshots, (bundles, dependencies, upload)) = transfer;
        run.job_id = id;
        run.bundles = bundles;
        run.git_dependencies = dependencies;
        run.gate_job = false;
        run.integration_tip.clear();
        // The hosted runner admits a Run only when it matches the lease exactly,
        // so normalise it the same way the spill path does: an unset vCPU count
        // is the default 2, and the time limit is the lease's own.
        run.weight_hint = Some(run.weight_hint.unwrap_or(2));
        if let Some(lease) = booked.as_ref() {
            run.timeout = Some(lease.max_s);
        }
        *run_sent = true;
        c.send(Frame::Run(run.clone())).await?;
        match c.next().await? {
            Incoming::Frame(Frame::Accepted(_)) => {}
            Incoming::Frame(Frame::Refusal(refusal)) => {
                let mut terminal = TerminalRecord::new(
                    id,
                    Outcome::RefusedBeforeStart {
                        reason: refusal.reason,
                    },
                    0,
                    0,
                    0,
                );
                terminal.refusal_detail = refusal.detail;
                return Ok(terminal);
            }
            _ => return Err(io::Error::other("shadow admission lost")),
        }
        self.upload(c, &snapshots, &upload, None).await?;
        loop {
            match c.next_job().await? {
                Incoming::Frame(Frame::Output(output)) => capture.output(&output),
                Incoming::Frame(Frame::Terminal(terminal)) if terminal.job_id == id => {
                    return Ok(terminal)
                }
                Incoming::Frame(Frame::Heartbeat { .. } | Frame::Started { .. })
                | Incoming::Unknown(_) => {}
                _ => return Err(io::Error::other("unexpected shadow frame")),
            }
        }
    }
}

#[cfg(test)]
mod tests;
