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

pub(crate) const SHADOW_CLEANUP_BOUND: Duration = Duration::from_secs(120);
const SHADOW_RETRY_DELAYS: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

impl Client {
    /// Retry settlement within a fixed budget without persisting shadow state.
    /// A lost lease response can hide a booked sandbox. Probe release by job ID
    /// even without a response, and finish if the broker says it was admitted.
    pub(crate) async fn cleanup_shadow(
        &self,
        id: Uuid,
        terminal: Option<&TerminalRecord>,
        max_s: u64,
        booked: bool,
    ) {
        if tokio::time::timeout(
            SHADOW_CLEANUP_BOUND,
            self.cleanup_shadow_with_delays(id, terminal, max_s, booked, &SHADOW_RETRY_DELAYS),
        )
        .await
        .is_err()
        {
            tracing::warn!(job_id = %id, cause = "120-second cleanup deadline exceeded", "shadow lease cleanup exhausted budget; broker backstop must reclaim it");
        }
    }

    async fn cleanup_shadow_with_delays(
        &self,
        id: Uuid,
        terminal: Option<&TerminalRecord>,
        max_s: u64,
        booked: bool,
        delays: &[Duration],
    ) {
        let body = terminal
            .map(|t| Finish::from_terminal(t, max_s))
            .unwrap_or(Finish {
                outcome: "lost".into(),
                run_ms: 0,
            });
        let mut finish = booked;
        for (attempt, delay) in delays.iter().enumerate() {
            tokio::time::sleep(*delay).await;
            tracing::info!(job_id = %id, attempt = attempt + 1, finish, outcome = %body.outcome,
                run_ms = body.run_ms, max_s, observed_run_ms = terminal.map(|t| t.wall_ms), "shadow lease cleanup attempt");
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                if finish {
                    match self.finish_once(id, &body).await? {
                        Reply::Settled(_) | Reply::Lost => return Ok(()),
                        Reply::Refused => {}
                    }
                }
                match self.release(id).await {
                    Ok(()) | Err(Error::NotGranted | Error::Released | Error::Finished) => Ok(()),
                    Err(Error::Admitted) => {
                        finish = true;
                        match self.finish_once(id, &body).await? {
                            Reply::Settled(_) | Reply::Lost => Ok(()),
                            Reply::Refused => Err(Error::Rejected),
                        }
                    }
                    Err(error) => Err(error),
                }
            })
            .await;
            let cause = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!(
                    "{}: {}",
                    error.code(),
                    self.broker_failure
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or("no broker response")
                ),
                Err(_) => format!(
                    "cleanup attempt timed out after 10 seconds: {}",
                    self.broker_failure
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or("no broker response")
                ),
            };
            tracing::warn!(job_id = %id, attempt = attempt + 1, %cause, "shadow lease cleanup failed; retrying within cleanup budget");
        }
        tracing::warn!(job_id = %id, attempts = delays.len(), "shadow lease cleanup exhausted retries; broker backstop must reclaim it");
    }
    pub(crate) async fn finish(
        &self,
        id: Uuid,
        terminal: Option<&TerminalRecord>,
        storage: &Storage,
    ) -> std::io::Result<bool> {
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
        let mut body = None;
        storage.hosted_update(id, |state| {
            if !state.finish_done {
                if state.terminal.is_none() {
                    state.terminal = terminal.cloned();
                }
                if state.pending_finish.is_none() {
                    state.pending_finish = terminal.map(|terminal| {
                        Finish::from_terminal(terminal, state.max_runtime_s.unwrap_or(0))
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
            match self.finish_once(id, &body).await {
                Ok(Reply::Settled(settled)) => {
                    storage.hosted_update(id, |s| {
                        s.pending_finish = None;
                        s.finish_done = true;
                        s.billed_vcpu_ms = Some(settled);
                        s.billing = super::BillingStatus::Available;
                    })?;
                    return Ok(false);
                }
                Ok(result @ (Reply::Refused | Reply::Lost)) => {
                    let lost = matches!(result, Reply::Lost);
                    storage.hosted_update(id, |s| {
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
        tracing::info!(job_id = %id, step = "finish", cause = "no Run frame sent", "Boat never-ran cleanup requested");
        storage.hosted_update(id, |state| {
            if !state.finish_done && state.pending_finish.is_none() {
                state.pending_finish = Some(Finish {
                    outcome: "lost".into(),
                    run_ms: 0,
                });
            }
        })?;
        self.finish(id, None, storage).await.map(|_| ())
    }
    async fn finish_once(&self, id: Uuid, body: &Finish) -> Result<Reply, Error> {
        *self.broker_failure.lock().unwrap() = Some("finish bearer acquisition pending".into());
        let bearer = self.bearer().await.inspect_err(|error| {
            *self.broker_failure.lock().unwrap() = Some(format!(
                "finish bearer acquisition failed: {}",
                error.code()
            ));
        })?;
        *self.broker_failure.lock().unwrap() = Some("finish HTTP request pending".into());
        let reply = self
            .http
            .post(format!(
                "{}/v1/leases/{id}/finish",
                self.route.broker_url.trim_end_matches('/')
            ))
            .bearer_auth(bearer)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                *self.broker_failure.lock().unwrap() = Some(format!("transport: {error}"));
                Error::Broker
            })?;
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
                        .map(crate::shadow::detail_tail)
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
