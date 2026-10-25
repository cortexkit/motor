//! Poll the runner's completed eviction receipt, not free-space deltas: deleting
//! a workspace frees XFS extents but does not return sparse host blocks until
//! FITRIM. The independent idle path also reclaims free extents after restore.
use super::*;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_secs(5);
const IDLE_TRIM: u64 = 300;

#[derive(Default)]
struct Policy {
    eviction: Option<Value>,
    idle_since: Option<u64>,
    last_trim: u64,
}

async fn poll<F: Future<Output = io::Result<()>>>(
    policy: &mut Policy,
    status: &Value,
    now: u64,
    trim: impl FnOnce() -> F,
) -> io::Result<()> {
    if policy.due(status, now) {
        trim().await?;
        policy.trimmed(status, now);
    }
    Ok(())
}

impl Policy {
    fn due(&mut self, status: &Value, now: u64) -> bool {
        if status["ok"] != true {
            self.idle_since = None;
            return false;
        }
        if snapshot::busy(status).is_none() {
            self.idle_since.get_or_insert(now);
        } else {
            self.idle_since = None;
        }
        let receipt = &status["disk"]["last_eviction"];
        let evicted = receipt.is_object() && self.eviction.as_ref() != Some(receipt);
        let timer = self
            .idle_since
            .is_some_and(|since| now.saturating_sub(since.max(self.last_trim)) >= IDLE_TRIM);
        evicted || timer
    }

    fn trimmed(&mut self, status: &Value, now: u64) {
        self.eviction = status["disk"]["last_eviction"]
            .as_object()
            .map(|_| status["disk"]["last_eviction"].clone());
        self.last_trim = now;
    }
}

pub(super) async fn watch(lock: Arc<tokio::sync::Mutex<()>>) {
    let start = Instant::now();
    let mut policy = Policy::default();
    let mut timer = tokio::time::interval(POLL);
    loop {
        timer.tick().await;
        let _guard = lock.lock().await;
        if snapshot::frozen() || !Path::new("/run/motor-tenant-generation").exists() {
            continue;
        }
        let status = match super::forward(Frame::Status).await {
            Ok(status) => status,
            Err(error) => {
                policy.idle_since = None;
                tracing::warn!(%error, "tenant trim status unavailable");
                continue;
            }
        };
        let now = start.elapsed().as_secs();
        if let Err(error) = poll(&mut policy, &status, now, || async {
            tokio::task::spawn_blocking(|| command("fstrim", &["/tenant"]))
                .await
                .map_err(io::Error::other)?
        })
        .await
        {
            tracing::warn!(%error, "tenant fstrim failed; will retry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(eviction: Value) -> Value {
        json!({"ok":true,"queue_depth":0,"running_jobs":[],"disk":{"last_eviction":eviction}})
    }

    #[tokio::test]
    async fn tenant_trim_after_completed_eviction_retries_until_success() {
        let mut policy = Policy::default();
        let idle = status(Value::Null);
        assert!(!policy.due(&idle, 0));
        let evicted = status(json!({"path":"/Users/workspace","evicted_ms":10}));
        assert!(policy.due(&evicted, 5));
        // A failed trim must not mark the eviction as handled; the next poll retries.
        assert!(poll(&mut policy, &evicted, 5, || async {
            Err(io::Error::other("injected trim failure"))
        })
        .await
        .is_err());
        let mut calls = 0;
        poll(&mut policy, &evicted, 10, || {
            calls += 1;
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(calls, 1);
        assert!(!policy.due(&evicted, 15));
        assert!(policy.due(
            &status(json!({"path":"/home/workspace","evicted_ms":11})),
            20
        ));
    }

    #[tokio::test]
    async fn tenant_trim_idle_timer_runs_without_any_eviction() {
        let mut policy = Policy::default();
        let idle = status(Value::Null);
        let mut trims = vec![];
        for now in [0, 299, 300, 599, 600] {
            poll(&mut policy, &idle, now, || {
                trims.push(now);
                std::future::ready(Ok(()))
            })
            .await
            .unwrap();
        }
        assert_eq!(trims, [300, 600]);
    }

    #[test]
    fn tenant_trim_timer_requires_continuous_known_idleness() {
        let mut policy = Policy::default();
        let idle = status(Value::Null);
        assert!(!policy.due(&idle, 0));
        let mut busy = idle.clone();
        busy["queue_depth"] = json!(1);
        assert!(!policy.due(&busy, 299));
        assert!(!policy.due(&idle, 300));
        assert!(!policy.due(&idle, 599));
        assert!(policy.due(&idle, 600));
    }
}
