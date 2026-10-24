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

/// What the runner last decided about the remote cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    /// The runner switch, as far as this role may honour it. It is false on
    /// every host that never writes the cache, and without job isolation.
    switch: bool,
    /// The tenant's `enabled` from the latest config that parsed. `None` until
    /// one has, so a first read never looks like a switch-off.
    enabled: Option<bool>,
    status: Status,
    identity: Option<StoreIdentity>,
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
        }
    }

    /// Applies one re-read of the provisioning files.
    pub(crate) fn observe(&mut self, reread: Reread) {
        self.enabled = reread.enabled;
        self.status = reread.status;
        self.identity = reread.identity.as_ref().map(encode_identity);
    }

    /// The cache is publishing: the runner switch is on, its tenant has it
    /// enabled and a complete generation is active.
    pub(crate) fn publishing(&self) -> bool {
        self.switch && self.enabled == Some(true) && self.identity.is_some()
    }

    /// The store identity a request admitted now would be signed for, if any.
    pub(crate) fn identity(&self) -> Option<StoreIdentity> {
        if self.publishing() {
            self.identity.clone()
        } else {
            None
        }
    }

    /// The policy every fill, reuse decision and publication record reads.
    pub(crate) fn policy(&self, config: &crate::Config) -> PublishPolicy {
        PublishPolicy {
            switch: self.switch,
            refresh_period: config.refresh_period,
            retry_interval: config.publish_retry_interval,
            identity: self.identity.clone(),
        }
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
        let entered = !was.publishing();
        if entered {
            publication::forget(catalog);
            changed.refill = true;
        } else if was.identity.as_ref() != Some(identity) {
            publication::forget_other_stores(catalog, identity);
            changed.refill = true;
        }
        // Whatever was already publishing in this store keeps its records, but
        // a repository whose record is this store's and not yet published is
        // still queued by the ordinary rules.
        changed
    }
}

/// What acting on a state change did, for the caller that saves the catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Changed {
    /// Records were forgotten, so repositories with local fill records need a
    /// fill even if nothing changed about their descriptor.
    pub(crate) refill: bool,
}

/// The initial state of a runner that never publishes, for the hosts and
/// configurations that must behave exactly as before the remote store existed.
pub(crate) fn idle() -> State {
    State::new(false)
}

#[cfg(test)]
impl State {
    /// A state that is publishing in `identity`, with the switch on, for tests
    /// that drive the rest of the runner through the publication rules.
    pub(crate) fn publishing_in(identity: StoreIdentity) -> Self {
        Self {
            switch: true,
            enabled: Some(true),
            status: Status::Available,
            identity: Some(identity),
        }
    }
}
