use super::{
    placement::{Finish, FinishReply},
    Client, Error,
};
use crate::storage::Storage;
use motor_protocol::{TerminalRecord, Uuid};
use std::time::Duration;

enum Reply {
    Settled(u64),
    Refused,
    Lost,
}

impl Client {
    pub(crate) async fn release_stored(&self, id: Uuid, storage: &Storage) -> Result<(), Error> {
        self.release_stored_inner(id, storage, false).await
    }

    /// Recovery has ended the submission, so successful release can publish a terminal.
    pub(crate) async fn release_unused(&self, id: Uuid, storage: &Storage) -> Result<(), Error> {
        self.release_stored_inner(id, storage, true).await
    }

    async fn release_stored_inner(
        &self,
        id: Uuid,
        storage: &Storage,
        retire_unstarted: bool,
    ) -> Result<(), Error> {
        let _finish = storage.lock_finish(id).await;
        if storage
            .job(id)
            .and_then(|j| j.hosted)
            .is_some_and(|s| s.finish_done)
        {
            return Ok(());
        }
        let result = self.release(id).await;
        if result.is_ok() || matches!(result, Err(Error::Released | Error::Finished)) {
            storage
                .hosted_settlement_update(id, retire_unstarted, |s| {
                    s.finish_done = true;
                })
                .map_err(|_| Error::Broker)?;
        }
        result
    }

    pub(crate) async fn finish(
        &self,
        id: Uuid,
        terminal: Option<&TerminalRecord>,
        storage: &Storage,
    ) -> std::io::Result<bool> {
        self.finish_inner(id, terminal, storage, false).await
    }

    async fn finish_inner(
        &self,
        id: Uuid,
        terminal: Option<&TerminalRecord>,
        storage: &Storage,
        retire_unstarted: bool,
    ) -> std::io::Result<bool> {
        // Normal completion, cancellation recovery and shutdown can meet here.
        // Serialize the request as well as the state updates, not just the body.
        let _finish = storage.lock_finish(id).await;
        let Some(state) = storage.job(id).and_then(|j| j.hosted) else {
            return Ok(false);
        };
        if state.finish_done
            || state.pending_finish.is_none()
                && state
                    .placement
                    .as_ref()
                    .is_none_or(|p| !p.host.starts_with("boat:"))
                && state.hosted_code.as_deref() != Some("hosted_sandbox_lost")
        {
            return Ok(state.hosted_code.as_deref() == Some("hosted_sandbox_lost"));
        }
        // A terminal may arrive while another owner holds an older fallback.
        // Keep the observed result, including a real unknown outcome, under the
        // finish lock instead of replacing it with the caller's stale summary.
        let terminal = state
            .terminal
            .as_ref()
            .filter(|_| state.has_real_terminal())
            .or(terminal
                .filter(|t| !matches!(t.outcome, motor_protocol::Outcome::OutcomeUnknown))
                .or(state.terminal.as_ref().filter(|t| {
                    state.cancel_settlement_ready()
                        || !matches!(t.outcome, motor_protocol::Outcome::OutcomeUnknown)
                }))
                .or(terminal)
                .or(state.terminal.as_ref()));
        let mut body = None;
        storage.hosted_update(id, |state| {
            if !state.finish_done {
                if !state.has_real_terminal() {
                    state.terminal = terminal.cloned();
                }
                if state.pending_finish.is_none() {
                    state.pending_finish = state.terminal.as_ref().map(|terminal| {
                        let mut finish =
                            Finish::from_terminal(terminal, state.max_runtime_s.unwrap_or(0));
                        if state.awaiting_cancel_terminal()
                            && matches!(terminal.outcome, motor_protocol::Outcome::OutcomeUnknown)
                        {
                            // A live stream may still supply real terminal timing.
                            // Its cancel acknowledgment ends the booking even if
                            // that terminal never arrives; it does not prove how
                            // the command ended.
                            finish.outcome = "cancelled".into();
                        }
                        finish
                    });
                }
                body = state.pending_finish.clone();
            }
        })?;
        let Some(body) = body else {
            return Ok(false);
        };
        tracing::info!(job_id = %id, step = "finish", cause = ?terminal.or(state.terminal.as_ref()).map(|terminal| &terminal.outcome),
            run_sent = state.run_sent, outcome = %body.outcome, run_ms = body.run_ms, "Boat finish requested");
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
            match self.finish_once(state.runner_id(id), &body).await {
                Ok(Reply::Settled(settled)) => {
                    storage.hosted_settlement_update(id, retire_unstarted, |s| {
                        s.pending_finish = None;
                        s.finish_done = true;
                        s.billed_vcpu_ms = Some(settled);
                        s.billing = super::BillingStatus::Available;
                    })?;
                    return Ok(false);
                }
                Ok(result @ (Reply::Refused | Reply::Lost)) => {
                    let lost = matches!(result, Reply::Lost);
                    storage.hosted_settlement_update(id, retire_unstarted, |s| {
                        s.pending_finish = None;
                        s.finish_done = true;
                        s.billing = super::BillingStatus::Unavailable;
                        if lost {
                            s.hosted_code = Some("hosted_sandbox_lost".into());
                            s.hosted_no_fallback = true;
                        }
                    })?;
                    return Ok(lost);
                }
                Err(error) => {
                    tracing::warn!(job_id = %id, step = "finish", cause = error.code(), detail = ?self.broker_failure.lock().unwrap(), attempt, "Boat finish failed; retry remains pending");
                    if error == Error::AccountInvalid {
                        break;
                    }
                }
            }
        }
        // The durable body survives this bounded attempt and is retried on restart.
        tracing::warn!(job_id = %id, "Boat finish remains pending");
        Ok(false)
    }
    /// Settle a booked job that never received a Run frame, so the broker
    /// stops holding its sandbox. CLOUD's finish contract has no outcome that
    /// says "never ran"; `lost` with zero run time is the closest it accepts.
    /// The body is stored first, so a restart retries it like any pending finish.
    pub(crate) async fn finish_never_ran(
        &self,
        id: Uuid,
        storage: &Storage,
    ) -> std::io::Result<()> {
        self.finish_never_ran_inner(id, storage, false).await
    }

    /// Unlike placement retries, recovery will not request another booking after finish.
    pub(crate) async fn finish_unused(&self, id: Uuid, storage: &Storage) -> std::io::Result<()> {
        self.finish_never_ran_inner(id, storage, true).await
    }

    async fn finish_never_ran_inner(
        &self,
        id: Uuid,
        storage: &Storage,
        retire_unstarted: bool,
    ) -> std::io::Result<()> {
        tracing::info!(job_id = %id, step = "finish", cause = "no Run frame sent", "Boat never-ran cleanup requested");
        storage.hosted_update(id, |state| {
            if !state.finish_done && state.pending_finish.is_none() {
                state.pending_finish = Some(Finish {
                    outcome: "lost".into(),
                    run_ms: 0,
                });
            }
        })?;
        self.finish_inner(id, None, storage, retire_unstarted)
            .await
            .map(|_| ())
    }
    async fn finish_once(&self, id: Uuid, body: &Finish) -> Result<Reply, Error> {
        *self.broker_failure.lock().unwrap() = Some("finish bearer acquisition pending".into());
        let bearer = self.bearer().await.inspect_err(|error| {
            *self.broker_failure.lock().unwrap() = Some(format!(
                "finish bearer acquisition failed: {}",
                error.code()
            ));
        })?;
        let reply = self
            .post_with_refresh(&format!("/v1/leases/{id}/finish"), body, bearer)
            .await?;
        let status = reply.status().as_u16();
        tracing::info!(job_id = %id, outcome = %body.outcome, run_ms = body.run_ms,
            http_status = status, "Boat finish replied");
        if status != 200 {
            let code = reply
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(|code| code.as_str())
                        .map(crate::diagnostics::detail_tail)
                });
            *self.broker_failure.lock().unwrap() =
                Some(format!("HTTP {status}, broker_code={code:?}"));
            if !matches!(status, 400 | 404 | 409) {
                return Err(Error::Broker);
            }
            tracing::warn!(job_id = %id, step = "finish", cause = "broker refused finish", http_status = status, broker_code = ?code, "Boat finish refused");
            if status == 409 && code.as_deref() == Some("sandbox_lost") {
                return Ok(Reply::Lost);
            }
            return Ok(Reply::Refused);
        }
        let reply: FinishReply = reply.json().await.map_err(|error| {
            *self.broker_failure.lock().unwrap() =
                Some(format!("HTTP {status}, invalid finish response: {error}"));
            Error::Broker
        })?;
        if reply.job_id != id.to_string()
            || reply
                .settled_vcpu_ms
                .checked_add(reply.released_vcpu_ms)
                .is_none()
        {
            *self.broker_failure.lock().unwrap() =
                Some("HTTP 200, finish response job ID or settlement mismatch".into());
            return Err(Error::Broker);
        }
        Ok(Reply::Settled(reply.settled_vcpu_ms))
    }
}

#[cfg(test)]
mod tests;
