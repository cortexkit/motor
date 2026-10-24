//! Remote publication state: which fills published their entries to the
//! remote store, which failed to, and whether a fill of a descriptor may run
//! at all.
//!
//! Every trusted fill runs in recache mode while the cache is publishing, so a
//! fill recompiles and rewrites everything it produces. A fill that publishes
//! records itself; a fill whose writes failed is remembered, so that it is not
//! retried on every job arrival. Repeated build failures eventually hold the
//! descriptor like any other failing build; PUT-only failures remain retryable.
//!
//! The same policy decides whether a recorded fill may be reused. Without the
//! remote store a fill record is exactly the reuse rule that existed before,
//! so every function here returns "nothing special" while the cache is not
//! publishing.
use crate::storage::StoreIdentity;
use crate::storage::{Catalog, PublicationRecord, PublishFailure, PublishedKey, Repository};

/// How often the runner looks for repositories whose publication expired and
/// queues a recache fill for them. The period is fixed; [`PublishPolicy::refresh_period`]
/// is what bounds how old a publication may be.
pub(crate) const REFRESH_TICK_MS: u64 = 60 * 60 * 1000;

/// After this many fills of one descriptor in a row failed as builds, the
/// descriptor is held until it changes or the runner restarts, exactly as a
/// fill that fails as a build is held.
pub(crate) const PUBLISH_RETRY_ATTEMPTS: u32 = 3;

/// A fill's own view of whether it published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Every entry the fill wrote reached the store, and the fill of every tool
    /// its plan ran published.
    Published { keys: Vec<PublishedKey> },
    /// The fill did not publish. The reason is logged and shown in the
    /// repository's failure detail; only the marker and its count persist.
    Failed { reason: String },
}

/// Whether a fill of a descriptor may be queued now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Queue it, as any other fill is queued.
    Queue,
    /// A failed publication of it is younger than the retry interval.
    Wait,
    /// Its builds failed [`PUBLISH_RETRY_ATTEMPTS`] times in a row, so
    /// nothing more is queued until the descriptor changes or the runner
    /// restarts.
    Held,
}

pub(crate) struct PublishPolicy {
    /// The runner switch. Recache mode, publication records and this policy
    /// apply only while it is on; with it off, fills and descriptors behave
    /// exactly as they did before the remote store existed.
    pub switch: bool,
    /// How long a published fill stays fresh.
    pub refresh_period: std::time::Duration,
    /// How long a failed publication waits before it is retried.
    pub retry_interval: std::time::Duration,
    /// The store identity the active generation addresses, once the cache can
    /// actually be used; `None` while the cache is unavailable or its tenant
    /// has it switched off.
    pub identity: Option<StoreIdentity>,
    /// Only the breaker's open window blocks a fill; an elapsed window must
    /// admit a fill so its first write can probe the store.
    pub write_breaker_open: bool,
}

impl PublishPolicy {
    /// The policy that applies when nothing publishes.
    pub(crate) fn idle() -> PublishPolicy {
        PublishPolicy {
            switch: false,
            refresh_period: default_refresh_period(),
            retry_interval: default_publish_retry_interval(),
            identity: None,
            write_breaker_open: false,
        }
    }

    /// Whether the cache is publishing: the runner switch is on and a usable
    /// generation addresses the store. Recache mode, publication records,
    /// expiry, the publication gate and the refresh timer all apply only while
    /// this holds.
    pub(crate) fn publishing(&self) -> bool {
        self.switch && self.identity.is_some()
    }

    fn refresh_ms(&self) -> u64 {
        ms(self.refresh_period)
    }

    fn retry_ms(&self) -> u64 {
        ms(self.retry_interval)
    }
}

pub(crate) fn default_refresh_period() -> std::time::Duration {
    std::time::Duration::from_secs(14 * 24 * 60 * 60)
}

pub(crate) fn default_publish_retry_interval() -> std::time::Duration {
    std::time::Duration::from_secs(6 * 60 * 60)
}

/// How long a refresh fill may take on top of its period before it republishes
/// the entries it rewrites: its queue wait and its build time.
pub(crate) fn refill_latency_bound() -> std::time::Duration {
    std::time::Duration::from_secs(48 * 60 * 60)
}

/// The store's lifecycle rule: objects nobody rewrites are deleted after this.
fn store_lifecycle() -> std::time::Duration {
    std::time::Duration::from_secs(30 * 24 * 60 * 60)
}

fn ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Rejects a refresh period or retry interval under which one failed refresh
/// and its retry could both miss before the store's lifecycle deletes the
/// entries. Each message names the field it is about.
pub(crate) fn validate_bounds(
    refresh_period: std::time::Duration,
    publish_retry_interval: std::time::Duration,
) -> std::io::Result<()> {
    let invalid = |message: String| {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        ))
    };
    if refresh_period.is_zero() {
        return invalid("remote_cache.refresh_period must be above zero".into());
    }
    if publish_retry_interval.is_zero() {
        return invalid("remote_cache.publish_retry_interval must be above zero".into());
    }
    if publish_retry_interval >= refresh_period {
        return invalid(format!(
            "remote_cache.publish_retry_interval ({}s) must be below remote_cache.refresh_period ({}s)",
            publish_retry_interval.as_secs(),
            refresh_period.as_secs()
        ));
    }
    if refresh_period + refill_latency_bound() + publish_retry_interval >= store_lifecycle() {
        return invalid(format!(
            "remote_cache.refresh_period ({}s) plus the 48 h refill bound plus remote_cache.publish_retry_interval ({}s) must stay below the store's 30-day lifecycle",
            refresh_period.as_secs(),
            publish_retry_interval.as_secs()
        ));
    }
    Ok(())
}

/// Whether a recorded fill of `descriptor` may stand in for a new one.
///
/// With the cache not publishing, a record is enough, exactly as before the
/// remote store existed. While publishing, the record must also be younger
/// than the refresh period, belong to the store the active generation
/// addresses, and nothing may have failed to publish since it started: a
/// retained record therefore never makes a failed publication reusable.
pub(crate) fn reusable(
    policy: &PublishPolicy,
    repository: &Repository,
    descriptor: &str,
    now: u64,
) -> bool {
    if !repository.filled_descriptors.contains_key(descriptor) {
        return false;
    }
    if !policy.publishing() {
        return true;
    }
    let Some(record) = repository.publications.get(descriptor) else {
        // Nothing of this descriptor is known to be in this store.
        return false;
    };
    if policy.identity.as_ref() != Some(&record.identity) {
        return false;
    }
    if now.saturating_sub(record.started_ms) >= policy.refresh_ms() {
        return false;
    }
    !repository
        .publish_failures
        .get(descriptor)
        .is_some_and(|failure| failure.started_ms > record.started_ms)
}

/// Whether `descriptor` needs a refresh fill now: it has a local fill record
/// but no reusable publication, including a first fill whose PUTs failed.
/// A dependency-free shortcut has nothing to publish and never needs refresh.
pub(crate) fn expired(
    policy: &PublishPolicy,
    repository: &Repository,
    descriptor: &str,
    now: u64,
) -> bool {
    if repository.recorded_without_writer.contains(descriptor) {
        return false;
    }
    policy.publishing()
        && repository.filled_descriptors.contains_key(descriptor)
        && !reusable(policy, repository, descriptor, now)
}

/// The publication gate for one descriptor: whether a fill of it may start
/// now. A descriptor with no failure marker is queued at once. A failure
/// younger than the retry interval waits for the next attempt; once
/// [`PUBLISH_RETRY_ATTEMPTS`] builds in a row have failed, the descriptor is
/// held until it changes or the runner restarts. Only the write breaker's open
/// window blocks a fill: after it elapses the fill's first write probes the store.
pub(crate) fn gate(
    policy: &PublishPolicy,
    repository: &Repository,
    descriptor: &str,
    now: u64,
) -> Gate {
    if !policy.publishing() {
        return Gate::Queue;
    }
    if policy.write_breaker_open {
        return Gate::Wait;
    }
    let Some(failure) = repository.publish_failures.get(descriptor) else {
        return Gate::Queue;
    };
    if failure.attempts >= PUBLISH_RETRY_ATTEMPTS {
        return Gate::Held;
    }
    if now.saturating_sub(failure.started_ms) < policy.retry_ms() {
        return Gate::Wait;
    }
    Gate::Queue
}

/// Records a publication of `descriptor`, which clears its failure marker, or
/// a failed publication, whose marker counts consecutive failed builds. A
/// successful build resets that count even if its PUTs failed. Without a known store identity it is not
/// recorded: nothing says which store it went to.
pub(crate) fn record(
    repository: &mut Repository,
    descriptor: &str,
    started_ms: u64,
    outcome: &Outcome,
    identity: Option<&StoreIdentity>,
    fill_succeeded: bool,
) {
    match outcome {
        Outcome::Published { keys } => {
            let Some(identity) = identity else {
                return;
            };
            repository.publish_failures.remove(descriptor);
            repository.publications.insert(
                descriptor.to_string(),
                PublicationRecord {
                    identity: identity.clone(),
                    started_ms,
                    keys: keys.clone(),
                },
            );
        }
        Outcome::Failed { .. } => {
            let attempts = if fill_succeeded {
                0
            } else {
                repository
                    .publish_failures
                    .get(descriptor)
                    .map_or(1, |failure| failure.attempts.saturating_add(1))
            };
            repository.publish_failures.insert(
                descriptor.to_string(),
                PublishFailure {
                    started_ms,
                    attempts,
                },
            );
        }
    }
}

/// Forgets every publication record and publish-failure marker: the switch
/// went off and on again, so nothing remembered from the previous publishing
/// run says what the store holds now. The local fill records stay, but none of
/// them counts as published until a fill publishes again.
pub(crate) fn forget(catalog: &mut Catalog) {
    for repository in catalog.repositories.values_mut() {
        repository.publications.clear();
        repository.publish_failures.clear();
    }
}

/// Forgets the records and markers of every store other than `identity`, and
/// keeps those of `identity` itself, so a key rotation forgets nothing. A
/// marker remembers a failed fill of one store; once the store changed, what
/// it warned about no longer exists.
pub(crate) fn forget_other_stores(catalog: &mut Catalog, identity: &StoreIdentity) {
    for repository in catalog.repositories.values_mut() {
        repository
            .publications
            .retain(|_, record| &record.identity == identity);
        repository.publish_failures.clear();
    }
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
