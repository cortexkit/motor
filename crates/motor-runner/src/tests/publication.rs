//! Remote publication as the daemon sees it: the switch transitions that
//! forget records, the refresh timer's candidate selection, the retry cap and
//! the load-time bounds.
//!
//! What a fill records about its own PUTs is covered by the publication
//! module's tests. These tests drive a real runner, with its publishing state
//! set directly to what a re-read of a complete provisioning generation
//! decides, and a repository record put where a finished fill would have left
//! it.
use super::*;
use crate::remote_cache::provisioning::{Reread, Status};
use crate::remote_cache::publishing::State;
use crate::storage::{PublicationRecord, PublishedKey, StoreIdentity};

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

fn store(name: &str) -> StoreIdentity {
    StoreIdentity {
        endpoint: "https://s3.example.invalid".into(),
        bucket: "fleet-cache".into(),
        prefix: format!("tenant/acct-{name}/"),
    }
}

fn fill_config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.development_allow_host_backend = true;
    config
}

/// A runner whose cache publishes into `identity`. The fixture does not
/// isolate jobs, so its own switch is off and the test decides the state.
fn publishing_runner(f: &Fixture, identity: StoreIdentity) -> Runner {
    let runner = Runner::open(fill_config(f)).unwrap();
    set_publishing(&runner, State::publishing_in(identity));
    runner
}

fn set_publishing(runner: &Runner, state: State) {
    *runner.core.publishing.lock().unwrap() = state;
}

fn repository(f: &Fixture, runner: &Runner) -> Repository {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .get(&f.root)
        .cloned()
        .unwrap_or_default()
}

/// Puts a record where a finished fill leaves it: a local fill record, a
/// publication of `descriptor` into `identity`, and the tip desired.
fn record_fill(f: &Fixture, runner: &Runner, descriptor: &str, identity: StoreIdentity, at: u64) {
    let mut record = repository(f, runner);
    record.filled_descriptors.insert(descriptor.into(), at);
    record.filled_descriptor = Some(descriptor.into());
    record.requested_descriptor = Some(descriptor.into());
    record.desired = Some(crate::storage::BaseRequest {
        tip: record
            .desired
            .as_ref()
            .map(|d| d.tip.clone())
            .unwrap_or_else(|| f.key.clone()),
        workspace: f.key.clone(),
        siblings: vec![],
        git_dependencies: vec![],
        command: Some("true".into()),
    });
    record.result = Some(RebuildResult::Ok);
    record.publications.insert(
        descriptor.into(),
        PublicationRecord {
            identity,
            started_ms: at,
            keys: vec![PublishedKey {
                tool: "sccache".into(),
                key: "abc/Rust/unit".into(),
            }],
        },
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .insert(f.root.clone(), record);
}

/// The fixture's current integration tip, which is what a fill is keyed on.
fn tip(f: &Fixture) -> String {
    git(&f.main, &["rev-parse", "HEAD"])
}

/// The descriptor the fixture's tip has under the runner's own rules, which is
/// what a fill of it is recorded under.
fn descriptor(f: &Fixture, runner: &Runner) -> String {
    let tip = tip(f);
    let (steps, _) = crate::workspace::warm_steps(&f.main, &tip);
    runner
        .fill_identity(&f.main, &tip, &steps.join(" && "), false)
        .0
        .expect("the fixture tip has a descriptor")
}

fn count_fills(runner: &Runner) -> usize {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|j| j.base)
        .count()
}

fn reread(identity: StoreIdentity, enabled: bool) -> Reread {
    Reread {
        activated: true,
        previous_identity: Some(identity.provisioned()),
        identity: Some(identity.provisioned()),
        previous_enabled: Some(!enabled),
        enabled: Some(enabled),
        status: Status::Available,
    }
}

#[tokio::test]
async fn switching_off_and_on_again_forgets_the_publication_records() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let at = runner.now() - HOUR;
    record_fill(&f, &runner, "descriptor-a", store("fleet"), at);
    assert!(repository(&f, &runner)
        .publications
        .contains_key("descriptor-a"));

    // Switching the tenant's cache off keeps the records and queues nothing.
    runner
        .observe_publishing(reread(store("fleet"), false))
        .unwrap();
    assert!(repository(&f, &runner)
        .publications
        .contains_key("descriptor-a"));

    // Back on: the records are forgotten, so the descriptor counts as
    // unpublished again and the ordinary rules queue its fill.
    runner
        .observe_publishing(reread(store("fleet"), true))
        .unwrap();
    let after = repository(&f, &runner);
    assert!(
        after.publications.is_empty(),
        "a switch-on did not forget the publication records"
    );
    assert!(after.publish_failures.is_empty());
}

#[tokio::test]
async fn a_key_rotation_keeps_the_records_and_a_new_prefix_forgets_them() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    record_fill(&f, &runner, "descriptor-a", store("fleet"), runner.now());

    // A new generation that addresses the same store is a key rotation.
    runner
        .observe_publishing(reread(store("fleet"), true))
        .unwrap();
    assert!(repository(&f, &runner)
        .publications
        .contains_key("descriptor-a"));

    // A prefix moved to another cache tenant is another store.
    runner
        .observe_publishing(reread(store("second"), true))
        .unwrap();
    assert!(repository(&f, &runner).publications.is_empty());
}

#[tokio::test]
async fn a_transient_unavailability_with_the_same_store_forgets_nothing() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    record_fill(&f, &runner, "descriptor-a", store("fleet"), runner.now());
    let mut unavailable = reread(store("fleet"), true);
    unavailable.status = Status::Unavailable(
        crate::remote_cache::provisioning::Unavailable::NoCompleteGeneration {
            detail: "a mixed set".into(),
        },
    );
    runner.observe_publishing(unavailable).unwrap();
    assert!(repository(&f, &runner)
        .publications
        .contains_key("descriptor-a"));
}

#[tokio::test]
async fn the_refresh_timer_skips_a_shortcut_record() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let at = runner.now() - 100 * DAY;
    record_fill(&f, &runner, "shortcut", store("fleet"), at);
    // A descriptor recorded without running a fill has nothing to publish, so
    // the timer never queues it even when it looks far out of date.
    let mut record = repository(&f, &runner);
    record.recorded_without_writer.insert("shortcut".into());
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .insert(f.root.clone(), record);
    runner.tick().unwrap();
    runner.tick().unwrap();
    assert_eq!(count_fills(&runner), 0);
}

#[test]
fn a_bad_refresh_or_retry_period_is_rejected_at_load() {
    let f = Fixture::new();
    let mut config = fill_config(&f);
    config.publish_retry_interval = std::time::Duration::from_secs(31 * 24 * 3600);
    let error = match Runner::open(config) {
        Ok(_) => panic!("a retry interval of 31 days was accepted"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("remote_cache"), "{error}");

    let mut config = fill_config(&f);
    config.refresh_period = std::time::Duration::from_secs(24 * 3600);
    config.publish_retry_interval = std::time::Duration::from_secs(24 * 3600);
    let error = match Runner::open(config) {
        Ok(_) => panic!("a retry interval not below the refresh period was accepted"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("remote_cache.publish_retry_interval"),
        "the error does not name the field: {error}"
    );
}

#[test]
fn the_switch_state_is_saved_with_the_catalog() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    // A re-read that changes nothing still records what this run saw, so the
    // next start compares against it.
    runner
        .observe_publishing(reread(store("fleet"), true))
        .unwrap();
    let seen = runner.core.inner.lock().unwrap().publishing_seen;
    assert!(seen.switch, "the honouring runner switch was not recorded");
    assert!(seen.publishing, "a publishing runner was not recorded");
    // The next start restores it, so an unchanged configuration is the same
    // publishing run rather than a switch-off followed by a switch-on. The
    // store is not known until the first re-read, so the restore only carries
    // the switch and the tenant's `enabled` state.
    let mut restored = State::new(true);
    State::restore(&mut restored, true, seen);
    assert!(restored.switch);
    assert_eq!(restored.enabled, Some(true));
}
