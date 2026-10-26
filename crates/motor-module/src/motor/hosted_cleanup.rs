use super::*;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::task::JoinHandle;

pub(super) struct Cleanup {
    tasks: Mutex<Option<HashMap<Uuid, JoinHandle<()>>>>,
    pub(super) shutdown: watch::Sender<bool>,
    pub(super) booking_open: Arc<Mutex<bool>>,
}

impl Default for Cleanup {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(Some(HashMap::new())),
            shutdown: watch::channel(false).0,
            booking_open: Arc::new(Mutex::new(true)),
        }
    }
}

impl Motor {
    #[cfg(test)]
    pub(super) async fn wait_hosted_cleanup(&self, id: Uuid) {
        let task = self
            .hosted_cleanup
            .tasks
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|tasks| tasks.remove(&id));
        if let Some(task) = task {
            task.await.unwrap();
        }
    }

    /// Own settlement independently of the caller's stream. In particular, an
    /// error delivering a pre-Run cancel must not abandon a granted booking.
    pub(super) fn recover_hosted(&self, id: Uuid) {
        let Some(job) = self.storage.job(id) else {
            return;
        };
        let Some(state) = job.hosted else {
            return;
        };
        // A detached caller has not cancelled its command. Leave an ordinary
        // running job attachable; only cancellation or settlement owns cleanup.
        if state.run_sent
            && !job.pending_cancel
            && !state.cleanup_pending
            && state.terminal.is_none()
            && state.pending_finish.is_none()
            && !*self.hosted_cleanup.shutdown.borrow()
        {
            return;
        }
        if state.finish_done
            || (!state.lease_attempted
                && state.pending_finish.is_none()
                && !state.cleanup_pending
                && !(state.terminal.is_some()
                    && state
                        .placement
                        .as_ref()
                        .is_some_and(|p| p.host.starts_with("boat:"))))
        {
            return;
        }
        if state.run_sent
            && state.pending_finish.is_none()
            && state
                .placement
                .as_ref()
                .is_some_and(|p| !p.host.starts_with("boat:"))
        {
            return; // First-party hosts own settlement after admission.
        }
        let mut tasks = self.hosted_cleanup.tasks.lock().unwrap();
        let Some(tasks) = tasks.as_mut() else {
            return;
        };
        tasks.retain(|_, task| !task.is_finished());
        if tasks.get(&id).is_some_and(|task| !task.is_finished()) {
            return;
        }
        if let Err(error) = self.storage.hosted_update(id, |s| s.cleanup_pending = true) {
            self.log_hosted_failure(id, "persist_settlement_recovery", error);
            return;
        }
        let motor = self.clone();
        tasks.insert(
            id,
            tokio::spawn(async move { motor.cleanup_hosted(id).await }),
        );
    }

    async fn cleanup_hosted(&self, id: Uuid) {
        let mut shutdown = self.hosted_cleanup.shutdown.subscribe();
        let fence = self
            .submissions
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.subscribe());
        // A cancel retry must not release a booking its submission still owns.
        // Shutdown settles even a request whose grant has not arrived yet.
        if let Some(mut fence) = fence {
            while !*shutdown.borrow() {
                let expired = self
                    .storage
                    .job(id)
                    .and_then(|j| j.hosted)
                    .and_then(|s| s.host_deadline_s)
                    .is_some_and(|d| crate::hosted::now_s() >= d);
                if expired {
                    break;
                }
                tokio::select! {
                    _ = fence.changed() => break,
                    _ = shutdown.changed() => {},
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
        }
        let mut backoff = 1;
        loop {
            let stopping = *shutdown.borrow();
            match self.cleanup_hosted_once(id, stopping).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => self.log_hosted_failure(id, "settlement_recovery", error),
            }
            if stopping {
                // The pending body is durable before HTTP; startup retries it.
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(backoff)) => {},
                _ = shutdown.changed() => {},
            }
            backoff = (backoff * 2).min(30);
        }
    }

    pub(super) async fn cleanup_hosted_once(&self, id: Uuid, stopping: bool) -> io::Result<bool> {
        let Some(state) = self.storage.job(id).and_then(|job| job.hosted) else {
            return Ok(true);
        };
        if state.finish_done {
            return Ok(true);
        }
        let boat = state
            .placement
            .as_ref()
            .is_some_and(|p| p.host.starts_with("boat:"));
        if !boat && state.billing == crate::BillingStatus::Available && state.terminal.is_some() {
            return Ok(true); // First-party hosts settle their own usage.
        }
        let client = self.hosted_client(state.clone());
        if !state.run_sent {
            tracing::info!(job_id = %id, step = "settlement_recovery", cause = "submission ended before Run", "settling unused hosted booking");
            if boat || state.pending_finish.is_some() {
                client.finish_never_ran(id, &self.storage).await?;
            } else {
                match client.release_stored(id, &self.storage).await {
                    Ok(())
                    | Err(
                        crate::hosted::Error::Released
                        | crate::hosted::Error::Finished
                        | crate::hosted::Error::NotGranted,
                    ) => {
                        self.storage.hosted_update(id, |s| s.finish_done = true)?;
                    }
                    Err(crate::hosted::Error::Admitted) => {
                        client.finish_never_ran(id, &self.storage).await?;
                    }
                    Err(error) => {
                        return Err(io::Error::other(error.code()));
                    }
                }
            }
        } else {
            let terminal = state.terminal.clone().unwrap_or_else(|| {
                TerminalRecord::new(id, Outcome::OutcomeUnknown, 0, 0, 0)
                    .with_ran(Ran::Unknown("maybe".into()))
            });
            let unknown = matches!(terminal.outcome, Outcome::OutcomeUnknown);
            if unknown && state.pending_finish.is_none() {
                // Old stores lack a deadline. A fresh bounded recovery window
                // is conservative; never use a refreshed attach token's expiry.
                let deadline = state.host_deadline_s.unwrap_or_else(|| {
                    crate::hosted::now_s()
                        .saturating_add(state.max_runtime_s.unwrap_or(0))
                        .saturating_add(crate::hosted::HOST_LOST_GRACE_S)
                });
                if state.host_deadline_s.is_none() {
                    self.storage
                        .hosted_update(id, |s| s.host_deadline_s = Some(deadline))?;
                }
                let expired = crate::hosted::now_s() >= deadline;
                if !stopping && !expired {
                    self.storage.cancel(id, true)?;
                    if let Err(error) = self.send_cancel(id).await {
                        self.log_hosted_failure(id, "settlement_cancel", error);
                        return Ok(false);
                    }
                }
                tracing::info!(job_id = %id, step = "settlement_recovery", cause = if stopping { "shutdown" } else if expired { "host deadline passed" } else { "cancellation acknowledged" }, "settling unknown hosted outcome");
            }
            client.finish(id, Some(&terminal), &self.storage).await?;
        }
        let done = self
            .storage
            .job(id)
            .and_then(|j| j.hosted)
            .is_none_or(|s| s.finish_done);
        if done {
            self.storage
                .hosted_update(id, |s| s.cleanup_pending = false)?;
            // An archived spill may now be cancelling its build-server ID.
            // Settling the old booking must not consume that newer intent.
            if self
                .storage
                .job(id)
                .is_some_and(|j| j.build_server_job_id.is_none())
            {
                self.storage.cancel(id, false)?;
            }
        }
        Ok(done)
    }

    pub(crate) async fn shutdown_hosted(&self) {
        // A lease attempt is recorded under this gate before its HTTP request.
        // Close it before taking the inventory, so shutdown cannot miss a grant.
        *self.hosted_cleanup.booking_open.lock().unwrap() = false;
        self.hosted_cleanup.shutdown.send_replace(true);
        for id in self.storage.unsettled_hosted() {
            self.recover_hosted(id);
        }
        let tasks = self
            .hosted_cleanup
            .tasks
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();
        for (_, task) in tasks {
            if let Err(error) = task.await {
                tracing::warn!(%error, "hosted settlement task failed during shutdown");
            }
        }
    }
}
