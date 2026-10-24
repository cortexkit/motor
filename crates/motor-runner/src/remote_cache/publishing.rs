//! The runner's view of the remote cache's publishing state.
//!
//! A fill publishes while the runner switch is on, the tenant's `enabled`
//! state is on, job isolation is on (without it a job runs as the runner's own
//! account, so the switch is ignored there) and a complete provisioning
//! generation addresses a usable store. Everything that depends on the remote
//! store — recache mode, publication records, the publication gate, expiry and
//! the refresh timer — reads this one value, and nothing at all while it does
//! not hold.
//!
//! The value moves when a re-read of the provisioning files changes it, and
//! the change is acted on at that re-read rather than at the next timer tick.
//! That is what makes a configuration flip made while the runner was stopped,
//! or a prefix moved to a second tenant, observable at the first re-read that
//! sees it.
use crate::remote_cache::provisioning::{Reread, Status, Unavailable};
use crate::remote_cache::publication::{self, PublishPolicy};
use crate::storage::encode_identity;
use crate::storage::{Catalog, StoreIdentity};
use serde::{Deserialize, Serialize};

/// What the runner last decided about the remote cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    /// The runner switch, as far as this role may honour it. It is false on
    /// every host that never writes the cache, and without job isolation.
    pub(crate) switch: bool,
    /// The tenant's `enabled` from the latest config that parsed. `None` until
    /// one has, so a first read never looks like a switch-off.
    pub(crate) enabled: Option<bool>,
    pub(crate) status: Status,
    pub(crate) identity: Option<StoreIdentity>,
    /// The gateway's write-breaker deadline, refreshed by its owner before
    /// queueing fills. A half-open probe does not extend this window.
    pub(crate) write_breaker_open_until: Option<tokio::time::Instant>,
}

impl State {
    /// The state before anything has been read, for a runner whose switch is
    /// `switch`.
    pub(crate) fn new(switch: bool) -> Self {
        Self {
            switch,
            enabled: None,
            status: Status::Unavailable(Unavailable::NoCompleteGeneration {
                detail: "not read yet".to_string(),
            }),
            identity: None,
            write_breaker_open_until: None,
        }
    }

    /// Applies one re-read of the provisioning files.
    pub(crate) fn observe(&mut self, reread: Reread) {
        self.enabled = reread.enabled;
        self.status = reread.status;
        self.identity = reread.identity.as_ref().map(encode_identity);
    }

    /// The cache is publishing: the runner switch is on, its tenant has the
    /// cache enabled and a complete generation is active.
    pub(crate) fn publishing(&self) -> bool {
        self.switch && self.enabled == Some(true) && self.identity.is_some()
    }

    /// Whether the store itself is usable, as opposed to merely configured:
    /// a `cache_unavailable` decision (a reused generation, a prefix that does
    /// not match its tenant) stops the cache even while `enabled` is on.
    /// Publication reads this together with [`State::publishing`].
    fn store_usable(&self) -> bool {
        self.status == Status::Available
    }

    /// The store identity a request admitted now would be signed for, if any.
    pub(crate) fn identity(&self) -> Option<StoreIdentity> {
        if self.publishing() && self.store_usable() {
            self.identity.clone()
        } else {
            None
        }
    }

    /// The policy every fill, reuse decision and publication record reads. It
    /// publishes only while the cache is on and its store is usable.
    pub(crate) fn policy(&self, config: &crate::Config) -> PublishPolicy {
        PublishPolicy {
            switch: self.publishing() && self.store_usable(),
            refresh_period: config.refresh_period,
            retry_interval: config.publish_retry_interval,
            identity: self.identity(),
            write_breaker_open: self
                .write_breaker_open_until
                .is_some_and(|until| tokio::time::Instant::now() < until),
        }
    }

    /// The transition the next re-read is compared against, once this state
    /// has been acted on and saved.
    pub(crate) fn seen(&self) -> Seen {
        Seen {
            switch: self.switch,
            publishing: self.publishing(),
            identity: self.identity.clone(),
        }
    }

    /// Folds the switch state this runner acted on before it stopped into
    /// `state`, so a configuration flipped while it was stopped is a
    /// transition at the first re-read rather than a first read.
    ///
    /// A runner that never saw the switch on starts from nothing, exactly as
    /// it did before the switch existed; one that had it on remembers being
    /// publishing, so an unchanged configuration at startup keeps its records
    /// and a flip while stopped arrives as the transition it was.
    pub(crate) fn restore(state: &mut State, is_switch: bool, seen: Seen) {
        if !is_switch {
            return;
        }
        state.switch = true;
        if seen.publishing {
            state.enabled = Some(true);
        }
        state.identity = seen.identity;
    }

    /// Brings the catalog's publication records in line with this state, given
    /// the state before the change, and reports whether repositories need a
    /// fill because of it.
    ///
    /// A switch that went off and on again forgets every publication record:
    /// what the runner remembers describes fills of an earlier publishing run,
    /// and nothing says the store still holds their entries. A change of store
    /// identity forgets the records of the store it left, and keeps the ones of
    /// the store it now addresses. A key rotation changes no identity, and a
    /// transient unavailability keeps the identity, so neither forgets
    /// anything.
    pub(crate) fn reconcile(&self, was: &State, catalog: &mut Catalog) -> Changed {
        let mut changed = Changed::default();
        if !self.publishing() {
            return changed;
        }
        let Some(identity) = &self.identity else {
            return changed;
        };
        let entered = !was.switch || was.enabled != Some(true);
        if entered {
            publication::forget(catalog);
            changed.refill = true;
        } else if was
            .identity
            .as_ref()
            .is_some_and(|previous| previous != identity)
            || catalog.repositories.values().any(|repository| {
                repository
                    .publications
                    .values()
                    .any(|record| &record.identity != identity)
            })
        {
            publication::forget_other_stores(catalog, identity);
            changed.refill = true;
        }
        // Whatever was already publishing in this store keeps its records, but
        // a repository whose record is this store's and not yet published is
        // still queued by the ordinary rules.
        changed
    }
}

/// The switch state the runner last acted on, so a change made while it was
/// stopped is still an off-to-on transition at the first re-read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seen {
    /// Whether the runner switch was on when this state was last saved.
    pub(crate) switch: bool,
    /// Whether the cache was publishing then.
    pub(crate) publishing: bool,
    /// Retained across restart so a re-read can distinguish key rotation from
    /// a store change even when the previous fill never published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identity: Option<StoreIdentity>,
}

impl Seen {
    /// Keeps a catalog that never published from carrying the field, so an
    /// older runner reads the same bytes it wrote.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
    pub(crate) fn new(switch: bool) -> Self {
        Self {
            switch,
            publishing: false,
            identity: None,
        }
    }
}

/// What acting on a state change did, for the caller that saves the catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Changed {
    /// Records were forgotten, so repositories with local fill records need a
    /// fill even if nothing changed about their descriptor.
    pub(crate) refill: bool,
}

#[cfg(test)]
impl State {
    /// A state that is publishing into `identity`, with the switch on, for
    /// tests that drive the rest of the runner through the publication rules.
    pub(crate) fn publishing_in(identity: StoreIdentity) -> Self {
        Self {
            switch: true,
            enabled: Some(true),
            status: Status::Available,
            identity: Some(identity),
            write_breaker_open_until: None,
        }
    }
}
