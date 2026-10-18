//! One sender orders heartbeats and usage requests under one durable counter.
use crate::{
    billing::{sign_request, Request},
    server::{unix_ms, Runtime},
};
use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use motor_lease::Heartbeat;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub struct Sender {
    runtime: Arc<Runtime>,
    key: SigningKey,
    client: reqwest::blocking::Client,
    endpoint: String,
    in_flight: Option<Request>,
    #[cfg(test)]
    heartbeats: Heartbeats,
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
            runtime,
            key,
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            endpoint: motor_lease::ISSUER.into(),
            in_flight: None,
        })
    }
    #[cfg(test)]
    pub fn step(&mut self, now_ms: u64) -> Result<()> {
        self.heartbeats.step(now_ms)?;
        self.send_next(now_ms)
    }
    pub fn send_next(&mut self, now_ms: u64) -> Result<()> {
        {
            let mut host = self.runtime.host.lock().expect("host lock");
            host.expire_outbox(now_ms)?;
            if self.in_flight.as_ref().is_some_and(|r| {
                now_ms.saturating_sub(r.created_ms) >= crate::billing::OUTBOX_MAX_AGE_MS
            }) {
                self.in_flight = None;
            }
            if self.in_flight.is_none() {
                self.in_flight = host.snapshot.outbox.first().cloned();
            }
        }
        let Some(request) = self.in_flight.as_ref() else {
            return Ok(());
        };
        let headers = {
            let mut host = self.runtime.host.lock().expect("host lock");
            sign_request(&mut host, &self.runtime.host_id, request, now_ms, &self.key)?
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
            .send()?;
        let status = response.status().as_u16();
        let body: Value = response.json().unwrap_or(Value::Null);
        if status == 401 {
            tracing::error!("host_auth_failed_retire_key_if_clock_reversed");
        }
        if self
            .runtime
            .host
            .lock()
            .expect("host lock")
            .finish_request(request, status, &body)?
        {
            self.in_flight = None;
        }
        Ok(())
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
        let memory = mem_available_mib().context("host MemAvailable")?;
        let changed = {
            let host = self.runtime.host.lock().expect("host lock");
            self.last_state.as_ref()
                != Some(&lifecycle_state(host.heartbeat(now_ms, memory, healthy)))
        };
        if due || changed {
            healthy = true;
            for tenant in tenants {
                let ready = self
                    .runtime
                    .host
                    .lock()
                    .expect("host lock")
                    .is_ready(&tenant);
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
        let mut host = self.runtime.host.lock().expect("host lock");
        let heartbeat = host.heartbeat(now_ms, memory, healthy);
        let state = lifecycle_state(heartbeat.clone());
        if due || self.last_state.as_ref() != Some(&state) {
            host.queue_heartbeat(&self.runtime.host_id, &heartbeat, now_ms)?;
            self.last_heartbeat = now_ms;
            self.last_state = Some(state);
        }
        Ok(())
    }
}
fn mem_available_mib() -> Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    let kb: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .context("MemAvailable missing")?
        .split_whitespace()
        .next()
        .context("MemAvailable value")?
        .parse()?;
    Ok(kb / 1024)
}
