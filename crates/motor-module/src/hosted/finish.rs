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
                Err(_) => {}
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
        let reply = self
            .http
            .post(format!(
                "{}/v1/leases/{id}/finish",
                self.route.broker_url.trim_end_matches('/')
            ))
            .bearer_auth(self.bearer().await?)
            .json(body)
            .send()
            .await
            .map_err(|_| Error::Broker)?;
        let status = reply.status().as_u16();
        tracing::info!(job_id = %id, outcome = %body.outcome, run_ms = body.run_ms,
            http_status = status, "Boat finish replied");
        if matches!(status, 400 | 404 | 409) {
            tracing::warn!(job_id = %id, http_status = status, "Boat finish refused without retry");
            if status == 409
                && reply
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .as_ref()
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.as_str())
                    == Some("sandbox_lost")
            {
                return Ok(Reply::Lost);
            }
            return Ok(Reply::Refused);
        }
        if status != 200 {
            return Err(Error::Broker);
        }
        let reply: FinishReply = reply.json().await.map_err(|_| Error::Broker)?;
        if reply.job_id != id.to_string()
            || reply
                .settled_vcpu_ms
                .checked_add(reply.released_vcpu_ms)
                .is_none()
        {
            return Err(Error::Broker);
        }
        Ok(Reply::Settled(reply.settled_vcpu_ms))
    }
}

#[cfg(test)]
mod tests;
