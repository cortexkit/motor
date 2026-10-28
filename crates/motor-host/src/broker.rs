//! One sender orders heartbeats and usage requests under one durable counter.
use crate::{
    billing::{sign_request, Request},
    server::{unix_ms, Runtime},
};
use anyhow::Result;
use ed25519_dalek::SigningKey;
use motor_lease::Heartbeat;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// An explicit operator configuration can redirect broker transport, not token
/// identity. Never expose fake-broker admission on a public listener.
pub fn fake_broker_endpoint(url: &str, listen: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        parsed.scheme() == "http"
            && matches!(parsed.host_str(), Some("127.0.0.1" | "[::1]"))
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.path() == "/"
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "fake broker must be an http loopback origin"
    );
    let listen: std::net::SocketAddr = listen.parse()?;
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "fake broker requires loopback listen"
    );
    Ok(parsed.as_str().trim_end_matches('/').into())
}

pub fn trusted_override_config(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(path.is_absolute(), "override config must be absolute");
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink()
                && metadata.uid() == 0
                && metadata.mode() & 0o022 == 0,
            "override config and ancestors must be root-owned and not writable by others"
        );
    }
    Ok(())
}

pub struct Sender {
    runtime: Arc<Runtime>,
    key: SigningKey,
    client: reqwest::blocking::Client,
    endpoint: String,
    in_flight: Option<Request>,
    /// Consecutive failed attempts, which set how long the next one waits.
    failures: u32,
    /// No request is sent before this time after a failure.
    retry_at_ms: u64,
    #[cfg(test)]
    heartbeats: Heartbeats,
}

/// The heartbeat interval. After a failure the sender waits at least this
/// long, doubling up to `MAX_RETRY_MS`, so a broker that keeps refusing a
/// request never sees a tight retry loop.
const RETRY_MS: u64 = 15_000;
const MAX_RETRY_MS: u64 = 120_000;

fn retry_delay_ms(failures: u32) -> u64 {
    RETRY_MS
        .saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(MAX_RETRY_MS)
}

/// Names an outbox entry in logs by its route and job, never its body.
fn kind(request: &Request) -> &'static str {
    if request.path.ends_with("/usage") {
        "usage"
    } else if request.path.ends_with("/heartbeat") {
        "heartbeat"
    } else {
        "other"
    }
}
impl Sender {
    #[cfg(test)]
    pub fn test_endpoint(&mut self, endpoint: String) {
        self.endpoint = endpoint;
    }
    pub fn new(runtime: Arc<Runtime>, key: SigningKey) -> Result<Self> {
        Ok(Self {
            #[cfg(test)]
            heartbeats: Heartbeats::new(runtime.clone()),
            endpoint: runtime.broker_endpoint.clone(),
            runtime,
            key,
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            in_flight: None,
            failures: 0,
            retry_at_ms: 0,
        })
    }
    #[cfg(test)]
    pub fn step(&mut self, now_ms: u64) -> Result<()> {
        self.heartbeats.step(now_ms)?;
        self.send_next(now_ms)
    }
    pub fn send_next(&mut self, now_ms: u64) -> Result<()> {
        if now_ms < self.retry_at_ms {
            return Ok(());
        }
        {
            let mut host = self.runtime.host.lock().expect("host lock");
            host.expire_outbox(now_ms)?;
            if self.in_flight.as_ref().is_some_and(|r| {
                now_ms.saturating_sub(r.created_ms) >= crate::billing::OUTBOX_MAX_AGE_MS
            }) {
                self.in_flight = None;
            }
            // Queuing a heartbeat replaces the unsent one, including one this
            // sender is retrying. The newer body carries the current state,
            // so retrying the replaced one would only hold up what follows.
            if self.in_flight.as_ref().is_some_and(|r| {
                r.job.is_none()
                    && !host
                        .snapshot
                        .outbox
                        .iter()
                        .any(|q| q.path == r.path && q.body == r.body)
            }) {
                self.in_flight = None;
            }
            if self.in_flight.is_none() {
                self.in_flight = host.snapshot.outbox.first().cloned();
            }
            if let Some(request) = self.in_flight.as_ref() {
                if let Some(check) = crate::wire::request_violation(&request.path, &request.body) {
                    tracing::error!(
                        request = kind(request),
                        job_id = request.job,
                        check,
                        "host_request_invalid"
                    );
                    host.reject_request(request)?;
                    self.in_flight = None;
                    return Ok(());
                }
            }
        }
        let Some(request) = self.in_flight.clone() else {
            return Ok(());
        };
        let headers = {
            let mut host = self.runtime.host.lock().expect("host lock");
            sign_request(
                &mut host,
                &self.runtime.host_id,
                &request,
                now_ms,
                &self.key,
            )?
        };
        let response = self
            .client
            .post(format!("{}{}", self.endpoint, request.path))
            .header("CK-Host", headers.host)
            .header("CK-Timestamp", headers.timestamp.to_string())
            .header("CK-Counter", headers.counter.to_string())
            .header("CK-Signature", headers.signature)
            .header("Content-Type", "application/json")
            .body(request.body.clone())
            .send();
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.failed(now_ms);
                return Err(error.into());
            }
        };
        let status = response.status().as_u16();
        let body: Value = response.json().unwrap_or(Value::Null);
        // CLOUD's error bodies are exactly {"error":"<code>"}; the code is the
        // only part of a reply worth logging.
        let code = body["error"].as_str().unwrap_or("");
        if status == 401 {
            tracing::error!("host_auth_failed_retire_key_if_clock_reversed");
        }
        if status == 400 {
            // The broker refused this body's shape. Resending it cannot
            // succeed, so set it aside and move on to the next request.
            tracing::error!(
                request = kind(&request),
                job_id = request.job,
                status,
                code,
                "host_request_rejected"
            );
            self.runtime
                .host
                .lock()
                .expect("host lock")
                .reject_request(&request)?;
            self.in_flight = None;
            self.failed(now_ms);
            return Ok(());
        }
        if self
            .runtime
            .host
            .lock()
            .expect("host lock")
            .finish_request(&request, status, &body)?
        {
            if request.path.ends_with("/heartbeat") && (200..300).contains(&status) {
                self.closed_accounts(&body, now_ms);
            }
            self.in_flight = None;
            self.failures = 0;
            self.retry_at_ms = 0;
        } else {
            self.failed(now_ms);
            tracing::warn!(
                request = kind(&request),
                job_id = request.job,
                status,
                code,
                retry_in_ms = self.retry_at_ms.saturating_sub(now_ms),
                "host_request_unconfirmed"
            );
        }
        Ok(())
    }
    /// Records the closed accounts a heartbeat reply lists. The heartbeat
    /// already succeeded: a journal failure here is logged, and the broker
    /// lists the same subjects again in its next reply.
    fn closed_accounts(&self, reply: &Value, now_ms: u64) {
        let subjects = crate::closure::closed_subjects(reply);
        if subjects.is_empty() {
            return;
        }
        let result = self
            .runtime
            .host
            .lock()
            .expect("host lock")
            .close_subjects(&subjects, now_ms);
        if let Err(error) = result {
            tracing::error!(%error, "closed_accounts_not_recorded");
        }
    }
    fn failed(&mut self, now_ms: u64) {
        self.failures = self.failures.saturating_add(1);
        self.retry_at_ms = now_ms.saturating_add(retry_delay_ms(self.failures));
    }
}

fn lifecycle_state(mut heartbeat: Heartbeat) -> Heartbeat {
    // Elapsed time and free RAM change continuously, not lifecycle state.
    heartbeat.free_mem_mib = 0;
    for job in &mut heartbeat.running {
        job.elapsed_ms = 0;
    }
    heartbeat
}

pub fn run(runtime: Arc<Runtime>, key: SigningKey) -> Result<()> {
    let mut heartbeats = Heartbeats::new(runtime.clone());
    // Publishing cannot wait for an in-flight broker response: admission changes
    // and observation timestamps must reach the durable queue during an outage.
    std::thread::spawn(move || loop {
        if heartbeats.step(unix_ms()).is_err() {
            tracing::warn!("heartbeat_publish_failed");
        }
        std::thread::sleep(Duration::from_millis(250));
    });
    let mut sender = Sender::new(runtime, key)?;
    loop {
        if sender.send_next(unix_ms()).is_err() {
            tracing::warn!("host_request_retry");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub struct Heartbeats {
    runtime: Arc<Runtime>,
    last_heartbeat: u64,
    last_state: Option<Heartbeat>,
}
impl Heartbeats {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            last_heartbeat: 0,
            last_state: None,
        }
    }
    pub fn step(&mut self, now_ms: u64) -> Result<()> {
        let tenants: Vec<_> = self
            .runtime
            .host
            .lock()
            .expect("host lock")
            .tenants
            .values()
            .map(|t| t.tenant.clone())
            .collect();
        let mut healthy = self.last_state.as_ref().is_none_or(|h| h.healthy);
        let due = now_ms.saturating_sub(self.last_heartbeat) >= 15_000;
        // A failed probe latches the storage refusal; the heartbeat is still
        // sent, advertising no free vCPUs.
        self.runtime.probe_storage();
        let changed = {
            let host = self.runtime.host.lock().expect("host lock");
            // Free memory is not lifecycle state, so any value compares equal.
            self.last_state.as_ref() != Some(&lifecycle_state(host.heartbeat(now_ms, 0, healthy)))
        };
        if due || changed {
            let previous_health = healthy;
            healthy = true;
            // The memory probe below needs the list again.
            for tenant in tenants.clone() {
                // Snapshot I/O can exceed an ordinary helper RPC deadline.
                // Publish lifecycle state without queueing a stale adopt behind
                // the same tenant's suspend/wake/administrative operation.
                let operations = self.runtime.tenant_operations(&tenant)?;
                let _operation = match operations.try_lock() {
                    Ok(operation) => operation,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        healthy &= previous_health;
                        continue;
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        anyhow::bail!("tenant operations lock poisoned")
                    }
                };
                let ready = {
                    let host = self.runtime.host.lock().expect("host lock");
                    if host.is_suspending(&tenant) {
                        healthy &= previous_health;
                        continue;
                    }
                    host.is_ready(&tenant)
                };
                match self.runtime.control.adopt(&tenant) {
                    Ok(false) if ready => self
                        .runtime
                        .host
                        .lock()
                        .expect("host lock")
                        .lose_tenant(&tenant, now_ms)?,
                    Ok(true) if ready => match self.runtime.control.metrics(&tenant) {
                        Ok(sample) => self
                            .runtime
                            .host
                            .lock()
                            .expect("host lock")
                            .sample(&tenant, sample)?,
                        Err(_) => {
                            healthy = false;
                            tracing::warn!(tenant, "usage_metrics_unavailable");
                        }
                    },
                    Err(_) => healthy = false,
                    _ => {}
                }
            }
        }
        if due || changed {
            self.runtime.host.lock().expect("host lock").clear_warm();
        }
        self.probe_memory(&tenants, now_ms, due || changed)?;
        let mut host = self.runtime.host.lock().expect("host lock");
        let memory = host.free_mem_mib(now_ms);
        let heartbeat = host.heartbeat(now_ms, memory, healthy);
        let state = lifecycle_state(heartbeat.clone());
        if due || self.last_state.as_ref() != Some(&state) {
            // A body the broker would refuse with 400 is never queued. It is
            // checked again at the next interval or state change.
            if let Some(check) = crate::wire::heartbeat_violation(&heartbeat) {
                tracing::error!(check, "heartbeat_invalid");
            } else {
                host.queue_heartbeat(&self.runtime.host_id, &heartbeat, now_ms)?;
            }
            self.last_heartbeat = now_ms;
            self.last_state = Some(state);
        }
        Ok(())
    }
}
impl Heartbeats {
    /// Asks each running tenant's runner for its guest's available memory
    /// when its last sample is due for renewal. A tenant busy with a wake,
    /// suspend or other operation is skipped; its sample ages, and a sample
    /// older than the freshness bound advertises no free memory.
    fn probe_memory(&self, tenants: &[String], now_ms: u64, heartbeat: bool) -> Result<()> {
        for tenant in tenants {
            if !self
                .runtime
                .host
                .lock()
                .expect("host lock")
                .memory_probe_due(tenant, now_ms, heartbeat)
            {
                continue;
            }
            let operations = self.runtime.tenant_operations(tenant)?;
            let _operation = match operations.try_lock() {
                Ok(operation) => operation,
                Err(std::sync::TryLockError::WouldBlock) => continue,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    anyhow::bail!("tenant operations lock poisoned")
                }
            };
            if !self
                .runtime
                .host
                .lock()
                .expect("host lock")
                .memory_probe_due(tenant, now_ms, heartbeat)
            {
                continue;
            }
            let sample = self
                .runtime
                .control
                .runner_sample(tenant)
                .map_err(|error| format!("{error:#}"));
            let mut host = self.runtime.host.lock().expect("host lock");
            host.record_memory(
                tenant,
                now_ms,
                sample
                    .as_ref()
                    .map(|s| s.mem_available_bytes)
                    .map_err(Clone::clone),
            );
            // A failed probe leaves the previous fill sample to go stale, so
            // it stops counting rather than lingering.
            if let Ok(sample) = sample {
                host.record_fill(tenant, now_ms, sample.fill_permits);
                if heartbeat {
                    host.record_warm(tenant, now_ms, sample.held_workspaces);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod qualification_tests {
    use super::*;
    #[test]
    fn fake_broker_override_refuses_nonloopback_origins() {
        for url in [
            "http://example.com",
            "https://127.0.0.1",
            "http://127.0.0.2",
            "http://localhost",
            "http://127.0.0.1/x",
            "http://127.0.0.1?x",
            "http://u@127.0.0.1",
        ] {
            assert!(
                fake_broker_endpoint(url, "127.0.0.1:1234").is_err(),
                "{url}"
            );
        }
        assert_eq!(
            fake_broker_endpoint("http://127.0.0.1:1234", "127.0.0.1:4321").unwrap(),
            "http://127.0.0.1:1234"
        );
        assert_eq!(
            fake_broker_endpoint("http://[::1]:1234", "[::1]:4321").unwrap(),
            "http://[::1]:1234"
        );
    }
    #[test]
    fn fake_broker_override_refuses_public_listen() {
        for listen in ["0.0.0.0:1234", "[::]:1234", "192.0.2.1:1234"] {
            assert!(
                fake_broker_endpoint("http://127.0.0.1:1234", listen).is_err(),
                "{listen}"
            );
        }
    }
}
