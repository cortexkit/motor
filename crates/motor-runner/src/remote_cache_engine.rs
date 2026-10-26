use super::*;
use motor_protocol::{
    RemoteCacheHealth, RemoteCacheRepositoryHealth, RemoteCacheState, RemoteCacheToolHealth,
};
use sha2::{Digest, Sha256};

impl Runner {
    #[cfg(unix)]
    pub(super) fn start_remote_cache(&self) {
        let Some(runtime) = &self.core.config.remote_cache_runtime else {
            return;
        };
        let weak = Arc::downgrade(&self.core);
        let mut last_reason = None;
        let mut report = move |reread: crate::remote_cache::provisioning::Reread| {
            let reason = crate::remote_cache::runtime::reason(&reread.status);
            if reason != last_reason {
                if let Some(reason) = &reason {
                    tracing::warn!(%reason, "remote cache unavailable; serving uncached");
                }
                last_reason = reason;
            }
            if let Some(core) = weak.upgrade() {
                if let Err(error) = (Runner { core }).observe_publishing(reread) {
                    tracing::warn!(kind = ?error.kind(), "remote cache publication state could not be saved");
                }
            }
        };
        report(runtime.watch.reread());
        let watch = runtime.watch.clone();
        runtime
            .handle
            .spawn(watch.run(Duration::from_secs(1), Duration::from_secs(10), report));
    }

    pub(super) fn remote_cache_health(&self, now: u64) -> RemoteCacheHealth {
        snapshot(
            &self.core.config,
            &self.core.inner.lock().unwrap().catalog,
            now,
        )
    }
}

pub(super) fn snapshot(config: &Config, catalog: &Catalog, now: u64) -> RemoteCacheHealth {
    let mut result = RemoteCacheHealth {
        state: RemoteCacheState::Off,
        reason: None,
        sccache: RemoteCacheToolHealth::default(),
        ccache: RemoteCacheToolHealth::default(),
        repositories: BTreeMap::new(),
        repositories_omitted: 0,
    };
    if config.remote_cache {
        result.state = RemoteCacheState::CacheUnavailable;
        result.reason = config
            .remote_cache_error
            .clone()
            .or(Some("unsupported runner backend".into()));
        #[cfg(unix)]
        if let Some(runtime) = &config.remote_cache_runtime {
            use crate::remote_cache::gateway::Tool;
            result.sccache = tool_health(runtime.gateway.stats(Tool::Sccache));
            result.ccache = tool_health(runtime.gateway.stats(Tool::Ccache));
            let status = runtime.watch.status();
            result.reason = crate::remote_cache::runtime::reason(&status);
            if runtime.watch.enabled_setting() == Some(false) {
                result.state = RemoteCacheState::Off;
                result.reason = None;
            } else if result.reason.is_none() {
                result.reason = runtime.fault.lock().unwrap().clone();
                let open = [
                    runtime.gateway.read_breaker_open_until(),
                    runtime.gateway.write_breaker_open_until(),
                ]
                .into_iter()
                .flatten()
                .any(|until| tokio::time::Instant::now() < until);
                if open {
                    result.reason = Some("upstream breaker open".into());
                }
                if result.reason.is_none() {
                    result.state = RemoteCacheState::Available;
                }
            }
        }
    }
    // Bound the wire block independently of repository history and URLs.
    // Operators can read the full publication records from catalog.json.
    for (root, record) in crate::caller_path::entries(&catalog.repositories)
        .map(|(path, record)| (path.caller.text(), record))
    {
        if record.filled_descriptor.is_none() && record.failed_descriptor.is_none() {
            continue;
        }
        if result.repositories.len() == 16 {
            result.repositories_omitted += 1;
            continue;
        }
        let current = record
            .requested_descriptor
            .as_ref()
            .or(record.filled_descriptor.as_ref());
        let latest = record
            .publications
            .values()
            .map(|record| record.started_ms)
            .max();
        result.repositories.insert(
            format!("{:x}", Sha256::digest(root.as_bytes())),
            RemoteCacheRepositoryHealth {
                publication_age_ms: latest.map(|at| now.saturating_sub(at)),
                shortcut: current.is_some_and(|d| record.recorded_without_writer.contains(d)),
                held_descriptor: record
                    .failed_descriptor
                    .as_ref()
                    .map(|d| d.chars().take(128).collect()),
            },
        );
    }
    result
}

#[cfg(unix)]
fn tool_health(stats: crate::remote_cache::gateway::ToolStats) -> RemoteCacheToolHealth {
    RemoteCacheToolHealth {
        hits: stats.hits,
        probes: stats.probes,
        misses: stats.misses,
        breaker_open: stats.breaker_open,
        errors: stats.errors,
        refusals: motor_protocol::RemoteCacheRefusals {
            read: stats.read_refusals,
            write: stats.write_refusals,
        },
        p95_ms: stats.p95_ms,
    }
}
