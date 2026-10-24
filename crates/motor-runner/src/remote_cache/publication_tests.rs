use super::*;
use crate::remote_cache::provisioning::{Status, Unavailable};
use crate::remote_cache::publishing::{Changed, Seen, State};
use crate::storage::{Catalog, PublishedKey, Repository, StoreIdentity};
use std::collections::BTreeMap;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

fn store(bucket: &str) -> StoreIdentity {
    StoreIdentity {
        endpoint: "https://s3.example.invalid".into(),
        bucket: bucket.into(),
        prefix: "tenant/acct-01KXJJHK4V6DB42W1YAN2XD9QN/".into(),
    }
}

fn policy(identity: Option<StoreIdentity>) -> PublishPolicy {
    PublishPolicy {
        switch: true,
        refresh_period: std::time::Duration::from_secs(14 * 24 * 60 * 60),
        retry_interval: std::time::Duration::from_secs(6 * 60 * 60),
        identity,
        write_breaker_open: false,
    }
}

fn filled(descriptor: &str) -> Repository {
    let mut repository = Repository::default();
    repository.filled_descriptors.insert(descriptor.into(), 0);
    repository.filled_descriptor = Some(descriptor.into());
    repository
}

fn published(repository: &mut Repository, descriptor: &str, identity: &StoreIdentity, at: u64) {
    record(
        repository,
        descriptor,
        at,
        &Outcome::Published {
            keys: vec![PublishedKey {
                tool: "sccache".into(),
                key: "abc/Rust/x".into(),
            }],
        },
        Some(identity),
        true,
    );
}

#[test]
fn the_store_identity_decides_whether_a_record_counts() {
    let mut f = filled("d1");
    published(&mut f, "d1", &store("fleet"), HOUR);
    assert!(reusable(&policy(Some(store("fleet"))), &f, "d1", 2 * HOUR));
    assert!(
        !reusable(&policy(Some(store("other"))), &f, "d1", 2 * HOUR),
        "a record of another bucket's objects says nothing about this store"
    );
    assert_eq!(f.publications["d1"].keys.len(), 1);
}

#[test]
fn a_recorded_fill_is_reused_as_before_while_the_cache_is_not_publishing() {
    let mut f = filled("d1");
    assert!(reusable(&PublishPolicy::idle(), &f, "d1", 1_000));
    // Even an old record with no publication: with the store off, nothing
    // changed about today's reuse rule.
    assert!(reusable(&policy(None), &f, "d1", 1_000 * 365 * DAY));
    assert!(!reusable(&PublishPolicy::idle(), &f, "d2", 0));
    // A publication record does not make a descriptor with no local fill
    // reusable.
    published(&mut f, "d2", &store("fleet"), HOUR);
    assert!(!reusable(&policy(Some(store("fleet"))), &f, "d2", 2 * HOUR));
}

#[test]
fn while_publishing_a_record_is_reusable_only_young_and_unfailed() {
    let mut f = filled("d1");
    let p = policy(Some(store("fleet")));
    assert!(
        !reusable(&p, &f, "d1", 0),
        "a local record with no publication counts as expired"
    );
    published(&mut f, "d1", &store("fleet"), DAY);
    assert!(reusable(&p, &f, "d1", 2 * DAY));
    assert!(
        !reusable(&p, &f, "d1", DAY + 14 * DAY),
        "a publication older than the refresh period is expired"
    );
    // A failure after that record makes it unusable even while it is young.
    record(
        &mut f,
        "d1",
        DAY + HOUR,
        &Outcome::Failed {
            reason: "store refused it".into(),
        },
        None,
        false,
    );
    assert!(
        !reusable(&p, &f, "d1", 2 * DAY),
        "a retained record never makes a failed publication reusable"
    );
    assert_eq!(f.publish_failures["d1"].attempts, 1);
    // A later publication clears the marker.
    published(&mut f, "d1", &store("fleet"), 3 * DAY);
    assert!(!f.publish_failures.contains_key("d1"));
    assert!(reusable(&p, &f, "d1", 4 * DAY));
}

#[test]
fn the_gate_waits_out_the_retry_interval_and_then_holds_the_descriptor() {
    let mut f = filled("d1");
    let p = policy(Some(store("fleet")));
    assert_eq!(gate(&p, &f, "d1", 0), Gate::Queue);
    record(
        &mut f,
        "d1",
        HOUR,
        &Outcome::Failed {
            reason: "put refused".into(),
        },
        None,
        false,
    );
    assert_eq!(
        gate(&p, &f, "d1", HOUR + 5 * HOUR),
        Gate::Wait,
        "a failure younger than the retry interval queues nothing"
    );
    assert_eq!(gate(&p, &f, "d1", HOUR + 6 * HOUR), Gate::Queue);
    // Retries keep failing: the count rises and, at the cap, the descriptor is
    // held rather than retried forever.
    record(
        &mut f,
        "d1",
        HOUR + 6 * HOUR,
        &Outcome::Failed {
            reason: "put refused".into(),
        },
        None,
        false,
    );
    record(
        &mut f,
        "d1",
        HOUR + 12 * HOUR,
        &Outcome::Failed {
            reason: "put refused".into(),
        },
        None,
        false,
    );
    assert_eq!(f.publish_failures["d1"].attempts, PUBLISH_RETRY_ATTEMPTS);
    assert_eq!(gate(&p, &f, "d1", HOUR + 30 * DAY), Gate::Held);
    // A different descriptor is not held by this one's failures.
    assert_eq!(gate(&p, &f, "d2", HOUR), Gate::Queue);
    // Nor does the cache's switch-off gate anything.
    assert_eq!(gate(&PublishPolicy::idle(), &f, "d1", HOUR), Gate::Queue);
}

#[test]
fn the_refresh_timer_targets_expired_publications_but_never_a_shortcut_record() {
    let mut f = filled("d1");
    let p = policy(Some(store("fleet")));
    published(&mut f, "d1", &store("fleet"), DAY);
    assert!(!expired(&p, &f, "d1", 2 * DAY));
    assert!(expired(&p, &f, "d1", DAY + 14 * DAY));
    // A dependency-free shortcut has nothing to publish.
    let mut shortcut = filled("d2");
    shortcut.recorded_without_writer.insert("d2".into());
    assert!(!expired(&p, &shortcut, "d2", 100 * DAY));
    // The timer also retries a local fill that never published, without a job arrival.
    let local = filled("d3");
    assert!(expired(&p, &local, "d3", 100 * DAY));
    // With the cache not publishing the timer queues nothing at all.
    assert!(!expired(&PublishPolicy::idle(), &f, "d1", 100 * DAY));
}

#[test]
fn a_publication_without_a_store_identity_is_not_recorded() {
    let mut f = filled("d1");
    record(
        &mut f,
        "d1",
        HOUR,
        &Outcome::Published { keys: Vec::new() },
        None,
        true,
    );
    assert!(!f.publications.contains_key("d1"));
}

#[test]
fn forgetting_clears_records_and_markers_but_keeps_local_fills() {
    let mut catalog = Catalog::default();
    let mut repository = filled("d1");
    published(&mut repository, "d1", &store("fleet"), HOUR);
    record(
        &mut repository,
        "d1",
        HOUR,
        &Outcome::Failed { reason: "x".into() },
        None,
        false,
    );
    catalog.repositories.insert("/r".into(), repository);
    forget(&mut catalog);
    let repository = &catalog.repositories["/r"];
    assert!(repository.publications.is_empty());
    assert!(repository.publish_failures.is_empty());
    assert!(repository.filled_descriptors.contains_key("d1"));
}

#[test]
fn a_store_change_forgets_the_old_stores_records_and_a_rotation_forgets_nothing() {
    let mut catalog = Catalog::default();
    let mut repository = filled("d1");
    published(&mut repository, "d1", &store("fleet"), HOUR);
    repository.filled_descriptors.insert("d2".into(), 0);
    published(&mut repository, "d2", &store("old"), HOUR);
    catalog.repositories.insert("/r".into(), repository);

    // A key rotation addresses the same store, which is no change at all.
    forget_other_stores(&mut catalog, &store("fleet"));
    assert!(catalog.repositories["/r"].publications.contains_key("d1"));

    forget_other_stores(&mut catalog, &store("fleet"));
    let repository = &catalog.repositories["/r"];
    assert!(repository.publications.contains_key("d1"));
    assert!(
        !repository.publications.contains_key("d2"),
        "a record of the store the runner left does not survive"
    );
    assert!(repository.filled_descriptors.contains_key("d2"));
}

#[test]
fn the_configured_periods_are_validated_at_load() {
    use std::time::Duration;
    let ok = |refresh: u64, retry: u64| {
        validate_bounds(Duration::from_secs(refresh), Duration::from_secs(retry))
    };
    assert!(ok(14 * 24 * 3600, 6 * 3600).is_ok());
    // Not below the refresh period.
    let error = ok(24 * 3600, 24 * 3600).unwrap_err().to_string();
    assert!(
        error.contains("remote_cache.publish_retry_interval"),
        "{error}"
    );
    // Too long on its own, so the window is past the 30-day lifecycle.
    let error = ok(14 * 24 * 3600, 31 * 24 * 3600).unwrap_err().to_string();
    assert!(
        error.contains("remote_cache.publish_retry_interval"),
        "{error}"
    );
    // The sum of refresh period, the 48 h refill bound and the retry is what
    // must stay below the lifecycle: 29 days plus 6 h plus 48 h is too much.
    let error = ok(29 * 24 * 3600, 6 * 3600).unwrap_err().to_string();
    assert!(error.contains("remote_cache.refresh_period"), "{error}");
    let error = ok(0, 3600).unwrap_err().to_string();
    assert!(error.contains("remote_cache.refresh_period"), "{error}");
    let error = ok(24 * 3600, 0).unwrap_err().to_string();
    assert!(
        error.contains("remote_cache.publish_retry_interval"),
        "{error}"
    );
}

#[test]
fn the_publishing_state_follows_the_switch_tenant_and_generation() {
    let mut state = State::new(true);
    assert!(!state.publishing(), "nothing has been read yet");
    // A complete generation whose tenant has the cache enabled publishes.
    state = publishing_in("gen-1");
    assert!(state.publishing());
    // The same generation with `enabled` off does not.
    state.enabled = Some(false);
    assert!(!state.publishing());
    // An unavailable store still counts as configured, but nothing is
    // published into it until a re-read makes it usable again.
    state.enabled = Some(true);
    state.status = Status::Unavailable(Unavailable::NoCompleteGeneration {
        detail: "gone".into(),
    });
    assert!(state.publishing());
    assert!(state.identity().is_none());
    // A role that may not honour the switch never publishes.
    state = publishing_in("gen-1");
    state.switch = false;
    assert!(!state.publishing());
}

fn publishing_in(name: &str) -> State {
    let mut state = State::new(true);
    state.enabled = Some(true);
    state.status = Status::Available;
    state.identity = Some(store(name));
    state
}

#[test]
fn switching_off_and_on_forgets_records_and_a_key_rotation_does_not() {
    let mut catalog = Catalog {
        repositories: BTreeMap::from([("/r".to_string(), filled("d1"))]),
        ..Catalog::default()
    };
    let mut repository = filled("d1");
    published(&mut repository, "d1", &store("gen-1"), HOUR);
    catalog.repositories.insert("/r".into(), repository);

    // A re-read that changes nothing keeps the record and asks for no fill.
    let on = publishing_in("gen-1");
    assert_eq!(on.reconcile(&on, &mut catalog), Changed::default());
    assert!(catalog.repositories["/r"].publications.contains_key("d1"));

    // Off: nothing is forgotten yet, and nothing is queued.
    let mut off = on.clone();
    off.enabled = Some(false);
    assert_eq!(off.reconcile(&on, &mut catalog), Changed::default());
    assert!(catalog.repositories["/r"].publications.contains_key("d1"));

    // Back on with the same generation: records are forgotten and the
    // repositories are refilled, so the store is rewritten from scratch.
    let changed = on.reconcile(&off, &mut catalog);
    assert!(changed.refill);
    assert!(catalog.repositories["/r"].publications.is_empty());
    assert!(catalog.repositories["/r"]
        .filled_descriptors
        .contains_key("d1"));

    // A prefix moved to a second tenant is another identity: the records of
    // the old store go, and a fill is queued.
    let mut moved = on.clone();
    moved.identity = Some(store("gen-2"));
    let mut fresh = catalog_cache();
    let changed = moved.reconcile(&on, &mut fresh);
    assert!(changed.refill);
    assert!(fresh.repositories["/r"].publications.is_empty());
}

fn catalog_cache() -> Catalog {
    let mut repository = filled("d1");
    published(&mut repository, "d1", &store("gen-1"), HOUR);
    Catalog {
        repositories: BTreeMap::from([("/r".to_string(), repository)]),
        ..Catalog::default()
    }
}

#[test]
fn a_transient_unavailability_with_the_same_identity_forgets_nothing() {
    let mut catalog = catalog_cache();
    let on = publishing_in("gen-1");
    let mut briefly = on.clone();
    briefly.status = Status::Unavailable(Unavailable::NoCompleteGeneration {
        detail: "bad generation".into(),
    });
    let changed = briefly.reconcile(&on, &mut catalog);
    assert_eq!(changed, Changed::default());
}

#[test]
fn a_change_made_while_stopped_is_still_a_transition() {
    // The previous run was publishing and the switch is on again.
    let seen = Seen {
        switch: true,
        publishing: true,
        identity: Some(store("gen-1")),
    };
    let mut state = State::new(true);
    State::restore(&mut state, true, seen.clone());
    // It remembers the switch was on and the cache enabled, so an unchanged
    // re-read keeps its records. The store is not known to be usable until
    // that re-read arrives, which is the decision the restore does not make.
    assert!(state.switch);
    assert_eq!(state.enabled, Some(true));
    assert!(state.identity().is_none());
    let config = crate::Config::new(std::path::PathBuf::from("/tmp/catalog"), "restore");
    assert!(!state.policy(&config).publishing());
    State::restore(&mut state, true, seen.clone());
    let mut reread = crate::remote_cache::provisioning::Reread {
        activated: true,
        previous_identity: None,
        identity: Some(store("gen-1").provisioned()),
        previous_enabled: Some(true),
        enabled: Some(true),
        status: Status::Available,
    };
    state.observe(reread.clone());
    assert!(state.publishing());
    assert_eq!(state.seen(), seen);
    reread.status = Status::Unavailable(Unavailable::NoCompleteGeneration {
        detail: "gone".into(),
    });
    state.observe(reread);
    // A runner that never saw the switch on starts from nothing.
    let mut fresh = State::new(true);
    State::restore(&mut fresh, true, Seen::new(true));
    assert!(!fresh.publishing());
    assert!(fresh.identity().is_none());
    // A role that may not honour the switch restores nothing.
    let mut tenant = State::new(false);
    State::restore(&mut tenant, false, seen);
    assert!(!tenant.publishing());
}

#[test]
fn the_switch_state_round_trips_through_the_catalog() {
    let mut catalog = Catalog::default();
    assert!(
        serde_json::to_value(&catalog)
            .unwrap()
            .get("publishing_seen")
            .is_none(),
        "a catalog that never published carries no switch field"
    );
    catalog.publishing_seen = Seen {
        switch: true,
        publishing: true,
        identity: Some(store("fleet")),
    };
    let json = serde_json::to_value(&catalog).unwrap();
    let back: Catalog = serde_json::from_value(json).unwrap();
    assert_eq!(back.publishing_seen, catalog.publishing_seen);
}
