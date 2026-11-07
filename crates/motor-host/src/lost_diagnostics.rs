use crate::helper::Control;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const MAX_JOBS: usize = 3;
const FETCH_BUDGET: Duration = Duration::from_secs(30);

/// Capture evidence while the lost job's VM may still be running. Neither the
/// collector nor its blocking guest queries hold a host or tenant-operation lock;
/// stopping the VM never waits for diagnostics, even if a query does not return.
pub(crate) fn spawn(control: Arc<dyn Control>, tenant: String, jobs: Vec<(String, u64)>) {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let log_tenant = tenant.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("lost-diagnostics".into())
        .spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                collect(control, &tenant, jobs, FETCH_BUDGET)
            });
        })
    {
        tracing::warn!(tenant = log_tenant, detail = %line(&error.to_string()), "lost_job_diagnostics_failed");
    }
}

pub(crate) fn collect(
    control: Arc<dyn Control>,
    tenant: &str,
    jobs: Vec<(String, u64)>,
    budget: Duration,
) {
    let deadline = Instant::now() + budget;
    let (send, receive) = std::sync::mpsc::channel();
    let mut pending = BTreeSet::new();
    for (job, since_unix_s) in jobs.into_iter().take(MAX_JOBS) {
        if !pending.insert(job.clone()) {
            continue;
        }
        let control = control.clone();
        let query_tenant = tenant.to_owned();
        let send = send.clone();
        let query_job = job.clone();
        tracing::info!(tenant, job_id = job, "lost_job_diagnostics_started");
        if let Err(error) = std::thread::Builder::new()
            .name("lost-diagnostics-query".into())
            .spawn(move || {
                let result = control.admin(
                    &query_tenant,
                    &json!({
                        "op":"diagnostics", "since_unix_s":since_unix_s, "jti":query_job
                    }),
                );
                let _ = send.send((query_job, result));
            })
        {
            pending.remove(&job);
            tracing::warn!(tenant, job_id = job, detail = %line(&error.to_string()), "lost_job_diagnostics_failed");
        }
    }
    drop(send);
    while !pending.is_empty() {
        match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((job, result)) => {
                pending.remove(&job);
                if let Err(error) = result.and_then(|reply| log_reply(tenant, &job, &reply)) {
                    tracing::warn!(tenant, job_id = job, detail = %line(&format!("{error:#}")), "lost_job_diagnostics_failed");
                }
            }
            Err(_) => {
                for job in pending {
                    tracing::warn!(tenant, job_id = job, "lost_job_diagnostics_timed_out");
                }
                break;
            }
        }
    }
}

fn log_reply(tenant: &str, job_id: &str, reply: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(reply["ok"] == true, "guest diagnostics refused");
    anyhow::ensure!(
        reply["journal"]["unavailable"] != true,
        "guest journal unavailable: {}",
        reply["journal"]["error"]
    );
    let entries = reply["journal"]["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("guest journal entries missing"))?;
    for entry in entries.iter().take(50) {
        if let Some(message) = entry["message"].as_str() {
            tracing::info!(tenant, job_id, unix_us = entry["unix_us"].as_u64(),
                line = %line(message), "lost_job_runner_log");
        }
    }
    tracing::info!(
        tenant,
        job_id,
        entries = entries.len().min(50),
        truncated = entries.len() > 50 || reply["journal"]["truncated"] == true,
        "lost_job_diagnostics_finished"
    );
    Ok(())
}

fn line(value: &str) -> String {
    value
        .chars()
        .take(300)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
