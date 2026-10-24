//! Remote publication as the daemon sees it: which fills run in recache mode,
//! what a fill records, when a recorded fill is reused, the retry cap and the
//! refresh timer.
//!
//! The store is never reached here. A test puts the runner's publishing state
//! into "publishing into this identity" directly, which is what a re-read of
//! a complete provisioning generation would decide, and moves the clock the
//! fills, the gate and the timer read.
use super::*;
use crate::remote_cache::provisioning::{Reread, Status};
use crate::remote_cache::publication::{self, record, Outcome};
use crate::remote_cache::publishing::State;
use crate::storage::StoreIdentity;

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;

fn store(bucket: &str) -> StoreIdentity {
    StoreIdentity {
        endpoint: "https://s3.example.invalid".into(),
        bucket: bucket.into(),
        prefix: "tenant/acct-01KXJJHK4V6DB42W1YAN2XD9QN/".into(),
    }
}

/// A runner whose cache is publishing into `identity` from the start.
fn publishing_runner(f: &Fixture, identity: StoreIdentity) -> Runner {
    let mut config = fill_config(f);
    let runner = Runner::open(config).unwrap();
    set_publishing(&runner, State::publishing_in(identity));
    runner
}

/// The fixture's configuration for the fills these tests run: its own warm
/// command, clearing the fixture default, and the host backend a macOS
/// development host needs, as the other runner tests do.
fn fill_config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.base_commands.clear();
    config.development_allow_host_backend = true;
    config
}

fn set_publishing(runner: &Runner, state: State) {
    *runner.core.publishing.lock().unwrap() = state;
}

fn repository(f: &Fixture, runner: &Runner) -> Repository {
    runner.core.inner.lock().unwrap().catalog.repositories[&f.root].clone()
}

fn writer_count(runner: &Runner) -> usize {
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

fn put_repository(runner: &Runner, root: &str, repository: Repository) {
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .insert(root.to_string(), repository);
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

/// Hands the runner a fresh prepare of the repository's tip and lets any fill
/// it queues settle. A tip whose descriptor is already recorded queues nothing,
/// because reuse is what these tests are about.
async fn refill(f: &Fixture, runner: &Runner) {
    let (request, bytes) = f.run("true");
    let spool = f.spool(runner, &request, &bytes);
    let mut prepare = prepare_for(&request);
    prepare.bundles = request.bundles.clone();
    runner.fetch_prepare(&prepare, &spool.directory).unwrap();
    runner.prepare(&prepare, true).unwrap();
    base_done(runner).await;
}

/// The first fill of the repository: prepares its tip, then queues the fill
/// the way a repository's first warm build is queued.
async fn fill(f: &Fixture, runner: &Runner) -> Repository {
    let (request, bytes) = f.run("true");
    let spool = f.spool(runner, &request, &bytes);
    let mut prepare = prepare_for(&request);
    prepare.bundles = request.bundles.clone();
    runner.fetch_prepare(&prepare, &spool.directory).unwrap();
    runner.prepare(&prepare, true).unwrap();
    eprintln!(
        "DEBUG after prepare writers={} desired={}",
        writer_count(runner),
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .repositories
            .get(&f.root)
            .and_then(|r| r.desired.clone())
            .is_some()
    );
    if writer_count(runner) == 0 {
        let mut inner = runner.core.inner.lock().unwrap();
        runner
            .queue_base(
                &mut inner,
                &f.root,
                &request.integration_tip,
                &f.key,
                &[],
                &[],
            )
            .unwrap();
        runner.catalog(&mut inner).unwrap();
    }
    base_done(runner).await;
    let r = repository(f, runner);
    std::fs::write("/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/28df7d0e2339799b/pool-117/target/scratch/probe2", b"ok").ok();
    eprintln!(
        "DEBUG fill result desired={} requested={:?} filled={:?} pub={:?} detail={:?}",
        r.desired.is_some(),
        r.requested_descriptor,
        r.filled_descriptors,
        r.publications.keys().collect::<Vec<_>>(),
        r.failure_detail
    );
    r
}

#[tokio::test]
async fn a_published_fill_is_recorded_and_reused_while_it_is_fresh() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let first = fill(&f, &runner).await;
    let d = first.requested_descriptor.clone().unwrap();
    let record = first.publications[&d].clone();
    assert_eq!(record.identity, store("fleet"));
    assert_eq!(record.started_ms, first.filled_descriptors[&d]);
    assert!(record.keys.is_empty(), "no write socket wrote anything");

    // A tip whose descriptor did not change reuses the record.
    commit(&f.main, "source-only change\n");
    let count = writer_count(&runner);
    refill(&f, &runner).await;
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Ok));
    assert_eq!(writer_count(&runner), count, "a fresh record filled again");
    assert_eq!(repository(&f, &runner).publications[&d], record);
}

#[tokio::test]
async fn a_record_older_than_the_refresh_period_is_refilled_by_the_timer() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let first = fill(&f, &runner).await;
    let d = first.requested_descriptor.clone().unwrap();
    let count = writer_count(&runner);

    // Within the period the record stands, with no job arriving.
    f.clock.fetch_add(13 * DAY, Ordering::SeqCst);
    runner.tick().unwrap();
    assert_eq!(writer_count(&runner), count);
    assert!(repository(&f, &runner).publications.contains_key(&d));

    // Past it the refresh timer queues the fill, and its publication is new.
    f.clock.fetch_add(2 * DAY, Ordering::SeqCst);
    runner.tick().unwrap();
    until(|| writer_count(&runner) == count + 1).await;
    base_done(&runner).await;
    let refreshed = repository(&f, &runner);
    assert!(
        refreshed.publications[&d].started_ms > first.publications[&d].started_ms,
        "the refresh did not record a new publication"
    );
}

#[tokio::test]
async fn with_the_switch_off_an_old_record_is_reused_exactly_as_before() {
    let f = Fixture::new();
    let runner = Runner::open(fill_config(&f)).unwrap();
    let first = fill(&f, &runner).await;
    assert!(first.publications.is_empty(), "nothing publishes while off");
    let count = writer_count(&runner);
    f.clock.fetch_add(365 * DAY, Ordering::SeqCst);
    commit(&f.main, "source-only change\n");
    refill(&f, &runner).await;
    assert_eq!(
        writer_count(&runner),
        count,
        "with the switch off an old record is reused"
    );
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Ok));
}

#[tokio::test]
async fn a_failed_publication_holds_the_descriptor_for_the_retry_interval() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let first = fill(&f, &runner).await;
    let d = first.requested_descriptor.clone().unwrap();

    // A publication failure the daemon records, as a refused PUT would leave.
    let mut failed = first.clone();
    record(
        &mut failed,
        &d,
        f.clock.load(Ordering::SeqCst),
        &Outcome::Failed {
            reason: "a PUT was refused".into(),
        },
        None,
    );
    put_repository(&runner, &f.root, failed);
    let count = writer_count(&runner);

    // Within the retry interval the gate queues nothing, even for a new tip.
    commit(&f.main, "source-only change\n");
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count);
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Failed));

    // Past it, a fill is queued and its publication clears the marker.
    f.clock.fetch_add(7 * HOUR, Ordering::SeqCst);
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count + 1);
    let after = repository(&f, &runner);
    assert!(!after.publish_failures.contains_key(&d));
    assert_eq!(after.result, Some(RebuildResult::Ok));
    assert!(after.publications.contains_key(&d));
}

#[tokio::test]
async fn after_the_retry_cap_the_descriptor_is_held_like_a_failed_fill() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let first = fill(&f, &runner).await;
    let d = first.requested_descriptor.clone().unwrap();
    let mut failed = first.clone();
    for attempt in 0..publication::PUBLISH_RETRY_ATTEMPTS {
        record(
            &mut failed,
            &d,
            f.clock.load(Ordering::SeqCst) + u64::from(attempt),
            &Outcome::Failed {
                reason: "a PUT was refused".into(),
            },
            None,
        );
    }
    put_repository(&runner, &f.root, failed);
    let count = writer_count(&runner);
    // Far past both the retry interval and the refresh period, nothing is
    // queued: the descriptor is held until it changes or the runner restarts.
    f.clock.fetch_add(30 * DAY, Ordering::SeqCst);
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count);
    let held = repository(&f, &runner);
    assert_eq!(held.failed_descriptor.as_deref(), Some(d.as_str()));
    assert!(
        held.failure_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("failed 3 times in a row")),
        "{:?}",
        held.failure_detail
    );

    // A restart holds it just the same, as a fill that failed as a build is.
    let config = fill_config(&f);
    drop(runner);
    let runner = reopen(config);
    set_publishing(&runner, State::publishing_in(store("fleet")));
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count);

    // A descriptor change queues at once.
    fs::write(f.main.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    commit(&f.main, "manifest change\n");
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count + 1);
}

#[tokio::test]
async fn a_shortcut_repository_is_never_refreshed_by_the_timer() {
    let f = Fixture::new();
    // A graph of only the repository's own packages takes the shortcut:
    // nothing is compiled into a cache, so there is nothing to publish.
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname = \"shortcut\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "").unwrap();
    commit(&f.main, "local-only package\n");
    let runner = publishing_runner(&f, store("fleet"));
    let filled = fill(&f, &runner).await;
    let d = filled.requested_descriptor.clone().unwrap();
    assert!(
        filled.recorded_without_writer.contains(&d),
        "the fixture did not take the dependency-free shortcut"
    );
    let count = writer_count(&runner);
    f.clock.fetch_add(100 * DAY, Ordering::SeqCst);
    runner.tick().unwrap();
    runner.tick().unwrap();
    assert_eq!(writer_count(&runner), count);
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Ok));
    // Its job arrivals reuse the shortcut record too.
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count);
}

#[tokio::test]
async fn switching_off_and_on_again_forgets_records_and_refills() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let first = fill(&f, &runner).await;
    let d = first.requested_descriptor.clone().unwrap();
    assert!(first.publications.contains_key(&d));

    // Switching the tenant's cache off keeps the records and queues nothing.
    runner
        .observe_publishing(reread(store("fleet"), false))
        .unwrap();
    assert!(repository(&f, &runner).publications.contains_key(&d));

    // Back on: the records are forgotten and a fill is queued at once.
    let count = writer_count(&runner);
    runner
        .observe_publishing(reread(store("fleet"), true))
        .unwrap();
    assert!(
        repository(&f, &runner).publications.is_empty(),
        "a switch-on did not forget the publication records"
    );
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count + 1);
    assert!(repository(&f, &runner).publications.contains_key(&d));
}

#[tokio::test]
async fn a_key_rotation_queues_nothing_and_a_new_prefix_forgets_and_refills() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let first = fill(&f, &runner).await;
    let d = first.requested_descriptor.clone().unwrap();
    let count = writer_count(&runner);

    // A new generation that addresses the same store is a key rotation.
    runner
        .observe_publishing(reread(store("fleet"), true))
        .unwrap();
    f.clock.fetch_add(HOUR, Ordering::SeqCst);
    runner.tick().unwrap();
    assert_eq!(writer_count(&runner), count);
    assert!(repository(&f, &runner).publications.contains_key(&d));

    // A prefix moved to a second tenant is another store.
    let mut moved = store("fleet");
    moved.prefix = "tenant/acct-02SECOND000000000000000000/".into();
    runner.observe_publishing(reread(moved, true)).unwrap();
    assert!(repository(&f, &runner).publications.is_empty());
    refill(&f, &runner).await;
    assert_eq!(writer_count(&runner), count + 1);
}

#[tokio::test]
async fn a_foreground_job_does_not_disturb_a_publication_record() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let filled = fill(&f, &runner).await;
    let d = filled.requested_descriptor.clone().unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        motor_protocol::Outcome::Exit { code: 0 }
    );
    let after = repository(&f, &runner);
    assert_eq!(after.publications[&d], filled.publications[&d]);
}

#[tokio::test]
async fn an_unchanged_reread_keeps_the_record_and_saves_nothing() {
    let f = Fixture::new();
    let runner = publishing_runner(&f, store("fleet"));
    let filled = fill(&f, &runner).await;
    let d = filled.requested_descriptor.clone().unwrap();
    assert!(!runner
        .observe_publishing(reread(store("fleet"), true))
        .unwrap());
    assert!(repository(&f, &runner).publications.contains_key(&d));
}

#[test]
fn a_bad_refresh_or_retry_period_is_rejected_at_load() {
    let f = Fixture::new();
    let mut config = fill_config(&f);
    config.publish_retry_interval = std::time::Duration::from_secs(31 * 24 * 3600);
    let error = match Runner::open(config) {
        Ok(_) => panic!("a configuration past the retry bounds was accepted"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("remote_cache"),
        "the error does not name the field: {error}"
    );

    let mut config = fill_config(&f);
    config.refresh_period = std::time::Duration::from_secs(24 * 3600);
    config.publish_retry_interval = std::time::Duration::from_secs(24 * 3600);
    let error = match Runner::open(config) {
        Ok(_) => panic!("a configuration past the retry bounds was accepted"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("remote_cache.publish_retry_interval"),
        "the error does not name the field: {error}"
    );
}
