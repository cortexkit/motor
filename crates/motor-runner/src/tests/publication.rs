//! Real runner fills with an injected publishing state and a test gateway
//! recording cache writes. The tests supply the write ledgers and statistics
//! that isolated transport and installed cache tools normally provide.
use super::*;
use crate::remote_cache::gateway::tests::{generation, Harness, BUCKET, TENANT};
use crate::remote_cache::gateway::{Adapter, Binding, Tool};
use crate::remote_cache::provisioning::{Reread, Status};
use crate::remote_cache::publication as policy;
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
    let mut inner = runner.core.inner.lock().unwrap();
    inner.publishing_seen = state.seen();
    inner.catalog.publishing_seen = state.seen();
    *runner.core.publishing.lock().unwrap() = state;
}

fn restore_publishing(runner: &Runner) {
    let seen = runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .publishing_seen
        .clone();
    let mut state = State::new(true);
    State::restore(&mut state, true, seen);
    *runner.core.publishing.lock().unwrap() = state;
    runner
        .observe_publishing(reread(generation("1", TENANT).identity().recorded(), true))
        .unwrap();
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

fn declare(f: &Fixture, command: &str) {
    fs::create_dir_all(f.main.join(".motor")).unwrap();
    fs::write(
        f.main.join(".motor/cache.toml"),
        format!(
            "[[fill]]\ntool = \"ccache\"\ncommands = [{}]\ndescriptor-inputs = [\"source\"]\n",
            serde_json::to_string(command).unwrap()
        ),
    )
    .unwrap();
    commit(&f.main, "declared fill\n");
}

fn cargo_command(f: &Fixture, command: &str) {
    fs::write(f.main.join("Cargo.toml"), format!("[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[workspace.metadata.motor]\nwarm-commands = [{}]\n", serde_json::to_string(command).unwrap())).unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "").unwrap();
    commit(&f.main, "Cargo warm command\n");
}

fn declared_runner(f: &Fixture) -> Runner {
    let mut config = fill_config(f);
    config.base_commands.clear();
    let runner = Runner::open(config).unwrap();
    set_publishing(
        &runner,
        State::publishing_in(generation("1", TENANT).identity().recorded()),
    );
    runner
}

fn release_path(f: &Fixture) -> PathBuf {
    f.scratch.join("release-fill")
}

fn controlled_command(f: &Fixture) -> String {
    format!("while [ ! -e '{}' ]; do sleep 0.01; done; if [ -e '{}' ]; then exit 1; fi; printf filled > command-marker", release_path(f).display(), f.scratch.join("fail-fill").display())
}

fn controlled_runner(f: &Fixture) -> Runner {
    controlled_runner_with(f, |_| {})
}

fn controlled_runner_with(f: &Fixture, configure: impl FnOnce(&mut Config)) -> Runner {
    let mut config = fill_config(f);
    config.base_commands.clear();
    config.manual_tick = true;
    configure(&mut config);
    let runner = Runner::open(config).unwrap();
    set_publishing(
        &runner,
        State::publishing_in(generation("1", TENANT).identity().recorded()),
    );
    runner
}

async fn drive(runner: &Runner, condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        runner.tick().unwrap();
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("runner condition timed out: {}", runner_diagnostics(runner));
}

async fn submit_reader(f: &Fixture, runner: &Runner) {
    let (request, bytes) = f.run("true");
    f.submit(runner, &request, &bytes);
    drive(runner, || {
        runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .terminal
            .is_some()
    })
    .await;
}

async fn running_fill(runner: &Runner, _previous: usize) -> Job {
    drive(runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner
            .jobs
            .values()
            .any(|j| j.base && j.state == JobState::Running && j.terminal.is_none())
    })
    .await;
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .find(|j| j.base && j.terminal.is_none())
        .unwrap()
        .clone()
}

fn attach(
    runner: &Runner,
    job: &Job,
    h: &Harness,
    tool: Tool,
    stats: Option<&str>,
) -> Arc<Binding> {
    let adapter = match tool {
        Tool::Sccache => Adapter::S3,
        Tool::Ccache => Adapter::Http,
    };
    let binding = h.writer_for(tool, adapter);
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .fill_ledgers
        .insert(job.id, vec![binding.ledger().unwrap().clone()]);
    let output = runner
        .core
        .trees
        .state
        .join("sandboxes")
        .join(job.id.to_string())
        .join("output");
    fs::create_dir_all(&output).unwrap();
    if let Some(stats) = stats {
        fs::write(
            output.join(match tool {
                Tool::Sccache => "cache-stats",
                Tool::Ccache => "ccache-stats",
            }),
            stats,
        )
        .unwrap();
    }
    // Deliberately invalid statistics from a tool absent from the plan must
    // neither prevent publication nor contribute a remote failure.
    fs::write(
        output.join(match tool {
            Tool::Sccache => "ccache-stats",
            Tool::Ccache => "cache-stats",
        }),
        "inactive tool: not statistics",
    )
    .unwrap();
    binding
}

const CCACHE_OK: &str = "remote_storage_error 0\nremote_storage_timeout 0\n";
const SCCACHE_OK: &str =
    r#"{"stats":{"cache_misses":{"counts":{}},"cache_write_errors":0,"cache_errors":9}}"#;

async fn settle(f: &Fixture, runner: &Runner, job: &Job) {
    fs::write(release_path(f), "").unwrap();
    until(|| {
        let inner = runner.core.inner.lock().unwrap();
        inner.jobs[&job.id].terminal.is_some()
            && inner.writer.is_none()
            && !inner.finalizing.contains(&job.id)
    })
    .await;
    fs::remove_file(release_path(f)).unwrap();
}

#[tokio::test]
async fn only_the_tools_invoked_by_the_fill_count() {
    for tool in [Tool::Ccache, Tool::Sccache] {
        let f = Fixture::new();
        let command = controlled_command(&f);
        if tool == Tool::Ccache {
            declare(&f, &command);
        } else {
            cargo_command(&f, &command);
        }
        let runner = controlled_runner(&f);
        let h = Harness::new();
        submit_reader(&f, &runner).await;
        let job = running_fill(&runner, 0).await;
        let binding = attach(
            &runner,
            &job,
            &h,
            tool,
            Some(if tool == Tool::Ccache {
                CCACHE_OK
            } else {
                SCCACHE_OK
            }),
        );
        let path = if tool == Tool::Ccache {
            "/unit".into()
        } else {
            format!("/{BUCKET}/unit")
        };
        assert_eq!(
            h.get(
                &binding,
                &if tool == Tool::Ccache {
                    "/.sccache_check".into()
                } else {
                    format!("/{BUCKET}/.sccache_check")
                }
            )
            .await
            .status,
            404
        );
        assert_eq!(h.put(&binding, &path, b"compiled").await.status, 200);
        settle(&f, &runner, &job).await;
        let record = repository(&f, &runner);
        let publication = &record.publications[job.writer_descriptor.as_ref().unwrap()];
        assert_eq!(publication.started_ms, job.started_ms.unwrap());
        assert_eq!(
            publication.keys,
            vec![PublishedKey {
                tool: tool.id().into(),
                key: "unit".into()
            }]
        );
        assert!(record.publish_failures.is_empty());
        assert_eq!(
            fs::read(Path::new(&job.request.unwrap().cwd).join("command-marker")).unwrap(),
            b"filled"
        );
    }
}

#[tokio::test]
async fn active_tool_statistics_are_required_and_write_errors_prevent_publication() {
    for (tool, stats) in [
        (Tool::Ccache, None),
        (Tool::Ccache, Some("not statistics")),
        (
            Tool::Ccache,
            Some("remote_storage_error 1\nremote_storage_timeout 0\n"),
        ),
        (
            Tool::Ccache,
            Some("remote_storage_error 0\nremote_storage_timeout 1\n"),
        ),
        (Tool::Sccache, None),
        (Tool::Sccache, Some("not statistics")),
        (
            Tool::Sccache,
            Some(r#"{"stats":{"cache_misses":{"counts":{}},"cache_write_errors":1}}"#),
        ),
    ] {
        let f = Fixture::new();
        if tool == Tool::Ccache {
            declare(&f, &controlled_command(&f));
        } else {
            cargo_command(&f, &controlled_command(&f));
        }
        let runner = controlled_runner(&f);
        let h = Harness::new();
        submit_reader(&f, &runner).await;
        let job = running_fill(&runner, 0).await;
        attach(&runner, &job, &h, tool, stats);
        settle(&f, &runner, &job).await;
        let record = repository(&f, &runner);
        assert!(record.publications.is_empty());
        assert!(record
            .filled_descriptors
            .contains_key(job.writer_descriptor.as_ref().unwrap()));
        assert_eq!(
            record.publish_failures[job.writer_descriptor.as_ref().unwrap()].attempts,
            0
        );
    }
}

#[tokio::test]
async fn reuse_requires_a_young_publication_without_a_later_marker() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    submit_reader(&f, &runner).await;
    let job = running_fill(&runner, 0).await;
    settle(&f, &runner, &job).await;
    submit_reader(&f, &runner).await;
    assert_eq!(count_fills(&runner), 1, "young publication was not reused");
    let descriptor = job.writer_descriptor.clone().unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .repositories
            .get_mut(&f.root)
            .unwrap()
            .publish_failures
            .insert(
                descriptor.clone(),
                crate::storage::PublishFailure {
                    started_ms: runner.now() + 1,
                    attempts: 0,
                },
            );
    }
    f.clock.fetch_add(6 * HOUR + 1, Ordering::SeqCst);
    submit_reader(&f, &runner).await;
    assert_eq!(
        count_fills(&runner),
        2,
        "later failure marker allowed reuse"
    );
    let job = running_fill(&runner, 1).await;
    settle(&f, &runner, &job).await;
    f.clock.fetch_add(14 * DAY, Ordering::SeqCst);
    submit_reader(&f, &runner).await;
    assert_eq!(
        count_fills(&runner),
        1,
        "expired publication did not queue a fill after terminal retention"
    );
    let job = running_fill(&runner, 2).await;
    settle(&f, &runner, &job).await;
}

#[tokio::test]
async fn the_timer_republishes_without_jobs_across_periods_restart_and_empty_prefix() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let mut runner = controlled_runner(&f);
    let h = Harness::new();
    submit_reader(&f, &runner).await;
    for attempt in 0..4 {
        let job = running_fill(&runner, attempt).await;
        let binding = attach(&runner, &job, &h, Tool::Ccache, Some(CCACHE_OK));
        assert_eq!(h.put(&binding, "/current", b"compiled").await.status, 200);
        settle(&f, &runner, &job).await;
        let record = repository(&f, &runner);
        assert_eq!(
            record.publications[job.writer_descriptor.as_ref().unwrap()].started_ms,
            runner.now()
        );
        assert_eq!(
            record.filled_descriptors.len(),
            1,
            "refresh changed the descriptor"
        );
        if attempt == 1 {
            let config = runner.config().clone();
            drop(runner);
            runner = reopen(config);
            restore_publishing(&runner);
            assert_eq!(repository(&f, &runner).publications, record.publications);
        }
        h.store.clear();
        assert_eq!(h.get(&binding, "/current").await.status, 404);
        f.clock.fetch_add(14 * DAY, Ordering::SeqCst);
        if attempt < 3 {
            runner.tick().unwrap();
            assert_eq!(
                count_fills(&runner),
                1,
                "timer did not queue an expired local record after terminal retention"
            );
        }
    }
}

#[tokio::test]
async fn failed_puts_retry_only_after_the_interval_and_do_not_consume_the_build_failure_cap() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    let h = Harness::new();
    submit_reader(&f, &runner).await;
    for attempt in 0..5 {
        let job = running_fill(&runner, attempt).await;
        let binding = attach(&runner, &job, &h, Tool::Ccache, Some(CCACHE_OK));
        h.store.status(None);
        assert_eq!(h.put(&binding, "/first", b"compiled").await.status, 200);
        h.store.status(if attempt < 4 { Some(500) } else { None });
        assert_eq!(
            h.put(&binding, "/second", b"compiled").await.status,
            if attempt < 4 { 503 } else { 200 }
        );
        settle(&f, &runner, &job).await;
        if attempt == 4 {
            assert!(repository(&f, &runner).publish_failures.is_empty());
            assert_eq!(h.get(&binding, "/second").await.status, 200);
            break;
        }
        let record = repository(&f, &runner);
        assert!(record.publications.is_empty());
        assert_eq!(
            record.publish_failures[job.writer_descriptor.as_ref().unwrap()].attempts,
            0
        );
        for _ in 0..5 {
            f.clock.fetch_add(HOUR, Ordering::SeqCst);
            submit_reader(&f, &runner).await;
            runner.tick().unwrap();
            assert_eq!(
                count_fills(&runner),
                attempt + 1,
                "publication retry gate admitted an early caller"
            );
        }
        f.clock.fetch_add(HOUR, Ordering::SeqCst);
        runner.tick().unwrap();
        assert_eq!(
            count_fills(&runner),
            attempt + 2,
            "timer did not retry an unpublished local fill"
        );
    }
}

#[tokio::test]
async fn three_failed_build_retries_hold_a_published_descriptor_until_restart() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    submit_reader(&f, &runner).await;
    let first = running_fill(&runner, 0).await;
    settle(&f, &runner, &first).await;
    fs::write(f.scratch.join("fail-fill"), "").unwrap();
    f.clock.fetch_add(14 * DAY, Ordering::SeqCst);
    for attempt in 1..=3 {
        let job = running_fill(&runner, attempt).await;
        settle(&f, &runner, &job).await;
        let record = repository(&f, &runner);
        assert_eq!(
            record.publications[first.writer_descriptor.as_ref().unwrap()].started_ms,
            first.started_ms.unwrap()
        );
        assert_eq!(
            record.publish_failures[first.writer_descriptor.as_ref().unwrap()].attempts,
            attempt as u32
        );
        f.clock.fetch_add(6 * HOUR, Ordering::SeqCst);
    }
    for _ in 0..4 {
        runner.tick().unwrap();
    }
    // The original success expired from the 24-hour job log, but the three
    // failed retries remain: no fourth failed build was started.
    assert_eq!(count_fills(&runner), 3);
    assert_eq!(
        repository(&f, &runner).failed_descriptor,
        first.writer_descriptor
    );
    let config = runner.config().clone();
    drop(runner);
    let runner = reopen(config);
    restore_publishing(&runner);
    fs::remove_file(f.scratch.join("fail-fill")).unwrap();
    let job = running_fill(&runner, 4).await;
    settle(&f, &runner, &job).await;
    assert!(repository(&f, &runner).publish_failures.is_empty());
}

#[tokio::test]
async fn a_retained_publication_survives_put_failure_restart_build_failure_and_success() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    let h = Harness::new();
    submit_reader(&f, &runner).await;
    let first = running_fill(&runner, 0).await;
    settle(&f, &runner, &first).await;
    let descriptor = first.writer_descriptor.clone().unwrap();
    f.clock.fetch_add(HOUR, Ordering::SeqCst);
    // Force a reader-miss refill of the same descriptor while its old
    // publication is young, then fail only the remote write.
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .get_mut(&f.root)
        .unwrap()
        .filled_descriptors
        .remove(&descriptor);
    submit_reader(&f, &runner).await;
    let second = running_fill(&runner, 1).await;
    let binding = attach(&runner, &second, &h, Tool::Ccache, Some(CCACHE_OK));
    h.store.status(Some(500));
    assert_eq!(h.put(&binding, "/failed", b"compiled").await.status, 503);
    settle(&f, &runner, &second).await;
    let before = repository(&f, &runner);
    let config = runner.config().clone();
    drop(runner);
    let runner = reopen(config);
    restore_publishing(&runner);
    assert_eq!(repository(&f, &runner).publications, before.publications);
    assert_eq!(
        repository(&f, &runner).publish_failures,
        before.publish_failures
    );
    assert!(!policy::reusable(
        &runner.publishing_policy(),
        &before,
        &descriptor,
        runner.now()
    ));
    for hour in 1..=6 {
        f.clock.fetch_add(HOUR, Ordering::SeqCst);
        if hour < 6 {
            runner.tick().unwrap();
            assert_eq!(count_fills(&runner), 2);
        }
    }
    fs::write(f.scratch.join("fail-fill"), "").unwrap();
    let third = running_fill(&runner, 2).await;
    settle(&f, &runner, &third).await;
    let after = repository(&f, &runner);
    assert_eq!(after.publications, before.publications);
    assert_eq!(after.publish_failures[&descriptor].attempts, 1);
    assert!(!policy::reusable(
        &runner.publishing_policy(),
        &after,
        &descriptor,
        runner.now()
    ));
    f.clock.fetch_add(6 * HOUR, Ordering::SeqCst);
    fs::remove_file(f.scratch.join("fail-fill")).unwrap();
    let fourth = running_fill(&runner, 3).await;
    settle(&f, &runner, &fourth).await;
    assert!(repository(&f, &runner).publish_failures.is_empty());
    assert!(policy::reusable(
        &runner.publishing_policy(),
        &repository(&f, &runner),
        &descriptor,
        runner.now()
    ));
}

#[tokio::test]
async fn an_initial_build_failure_keeps_the_legacy_hold() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    fs::write(f.scratch.join("fail-fill"), "").unwrap();
    submit_reader(&f, &runner).await;
    let job = running_fill(&runner, 0).await;
    settle(&f, &runner, &job).await;
    f.clock.fetch_add(20 * DAY, Ordering::SeqCst);
    submit_reader(&f, &runner).await;
    assert_eq!(count_fills(&runner), 0);
    assert_eq!(
        repository(&f, &runner).failed_descriptor,
        job.writer_descriptor
    );
}

#[tokio::test]
async fn switch_on_after_restart_and_prefix_changes_queue_real_fills_at_the_reread() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    let mut off = State::publishing_in(generation("1", TENANT).identity().recorded());
    off.enabled = Some(false);
    set_publishing(&runner, off);
    submit_reader(&f, &runner).await;
    let first = running_fill(&runner, 0).await;
    settle(&f, &runner, &first).await;
    assert!(repository(&f, &runner).publications.is_empty());
    let config = runner.config().clone();
    drop(runner);
    let runner = reopen(config);
    restore_publishing(&runner);
    assert_eq!(
        count_fills(&runner),
        2,
        "switch-on waited for a timer or job arrival"
    );
    let second = running_fill(&runner, 1).await;
    let h = Harness::new();
    let binding = attach(&runner, &second, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/unit", b"compiled").await.status, 200);
    settle(&f, &runner, &second).await;
    let identity = generation("1", TENANT).identity().recorded();
    let mut unavailable = reread(identity.clone(), true);
    unavailable.status = Status::Unavailable(
        crate::remote_cache::provisioning::Unavailable::NoCompleteGeneration {
            detail: "unparsable config".into(),
        },
    );
    runner.observe_publishing(unavailable).unwrap();
    runner
        .observe_publishing(reread(identity.clone(), true))
        .unwrap();
    assert_eq!(
        count_fills(&runner),
        2,
        "transient unavailability or key rotation queued a fill"
    );
    runner
        .observe_publishing(reread(identity.clone(), false))
        .unwrap();
    runner.observe_publishing(reread(identity, true)).unwrap();
    assert_eq!(
        count_fills(&runner),
        3,
        "recent local fill blocked switch-on"
    );
    let third = running_fill(&runner, 2).await;
    settle(&f, &runner, &third).await;
    let other = generation("2", "acct:other");
    runner
        .observe_publishing(reread(other.identity().recorded(), true))
        .unwrap();
    assert_eq!(count_fills(&runner), 4, "new prefix waited for a timer");
    h.generations.set(Some(other));
    let fourth = running_fill(&runner, 3).await;
    let binding = attach(&runner, &fourth, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/unit", b"compiled").await.status, 200);
    settle(&f, &runner, &fourth).await;
    assert_eq!(
        repository(&f, &runner).publications[fourth.writer_descriptor.as_ref().unwrap()]
            .identity
            .prefix,
        "tenant/acct-other/"
    );
}

#[tokio::test]
async fn a_store_change_during_a_fill_does_not_publish_or_mark_the_new_store() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    let h = Harness::new();
    submit_reader(&f, &runner).await;
    let job = running_fill(&runner, 0).await;
    let binding = attach(&runner, &job, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/first", b"compiled").await.status, 200);
    let other = generation("2", "acct:other");
    h.generations.set(Some(other.clone()));
    runner
        .observe_publishing(reread(other.identity().recorded(), true))
        .unwrap();
    assert_eq!(h.put(&binding, "/second", b"compiled").await.status, 503);
    settle(&f, &runner, &job).await;
    let record = repository(&f, &runner);
    assert!(record.publications.is_empty());
    assert!(record.publish_failures.is_empty());
    let next = running_fill(&runner, 1).await;
    let binding = attach(&runner, &next, &h, Tool::Ccache, Some(CCACHE_OK));
    for key in ["/first", "/second"] {
        assert_eq!(h.put(&binding, key, b"compiled").await.status, 200);
    }
    settle(&f, &runner, &next).await;
    assert_eq!(
        repository(&f, &runner).publications[next.writer_descriptor.as_ref().unwrap()]
            .keys
            .len(),
        2
    );
}

#[tokio::test]
async fn a_descriptor_change_bypasses_old_markers_and_only_refreshes_current_keys() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    let h = Harness::new();
    submit_reader(&f, &runner).await;
    let first = running_fill(&runner, 0).await;
    let binding = attach(&runner, &first, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/old", b"compiled").await.status, 200);
    settle(&f, &runner, &first).await;
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .get_mut(&f.root)
        .unwrap()
        .publish_failures
        .insert(
            first.writer_descriptor.clone().unwrap(),
            crate::storage::PublishFailure {
                started_ms: runner.now(),
                attempts: 3,
            },
        );
    f.clock.fetch_add(HOUR, Ordering::SeqCst);
    commit(&f.main, "changed declared input\n");
    submit_reader(&f, &runner).await;
    let second = running_fill(&runner, 1).await;
    assert_ne!(first.writer_descriptor, second.writer_descriptor);
    let binding = attach(&runner, &second, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/current", b"compiled").await.status, 200);
    settle(&f, &runner, &second).await;
    f.clock.fetch_add(14 * DAY, Ordering::SeqCst);
    let refresh = running_fill(&runner, 2).await;
    assert_eq!(refresh.writer_descriptor, second.writer_descriptor);
    let binding = attach(&runner, &refresh, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/current", b"compiled").await.status, 200);
    settle(&f, &runner, &refresh).await;
    let writes = h.store.seen();
    assert_eq!(
        writes
            .iter()
            .filter(|request| request.method == "PUT" && request.path.ends_with("/old"))
            .count(),
        1
    );
    assert_eq!(
        writes
            .iter()
            .filter(|request| request.method == "PUT" && request.path.ends_with("/current"))
            .count(),
        2
    );
}

#[tokio::test]
async fn the_gate_waits_for_the_write_breaker_window_but_allows_its_probe_fill() {
    let f = Fixture::new();
    declare(&f, &controlled_command(&f));
    let runner = controlled_runner(&f);
    let h = Harness::new();
    submit_reader(&f, &runner).await;
    let first = running_fill(&runner, 0).await;
    let binding = attach(&runner, &first, &h, Tool::Ccache, Some(CCACHE_OK));
    assert_eq!(h.put(&binding, "/unit", b"compiled").await.status, 200);
    settle(&f, &runner, &first).await;
    for _ in 0..5 {
        h.store.status(Some(500));
        assert_eq!(h.put(&binding, "/outage", b"compiled").await.status, 503);
    }
    runner
        .core
        .publishing
        .lock()
        .unwrap()
        .write_breaker_open_until = h.gateway.write_breaker_open_until();
    assert!(runner.publishing_policy().write_breaker_open);
    f.clock.fetch_add(14 * DAY, Ordering::SeqCst);
    runner.tick().unwrap();
    assert_eq!(count_fills(&runner), 0, "open breaker queued a fill");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(!runner.publishing_policy().write_breaker_open);
    tokio::time::resume();
    f.clock.fetch_add(HOUR, Ordering::SeqCst);
    let job = running_fill(&runner, 1).await;
    let binding = attach(&runner, &job, &h, Tool::Ccache, Some(CCACHE_OK));
    h.store.status(None);
    let (first, second) = tokio::join!(
        h.put(&binding, "/probe", b"compiled"),
        h.put(&binding, "/concurrent", b"compiled")
    );
    assert_eq!((first.status, second.status), (200, 200));
    settle(&f, &runner, &job).await;
    assert!(repository(&f, &runner).publish_failures.is_empty());
    assert_eq!(
        repository(&f, &runner).publications[job.writer_descriptor.as_ref().unwrap()]
            .keys
            .len(),
        2
    );
}

#[test]
fn write_socket_drain_precedes_publication_without_holding_engine_locks() {
    bounded(|| {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let f = Fixture::new();
            declare(&f, &controlled_command(&f));
            let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let signal = entered.clone();
            let runner = controlled_runner_with(&f, |config| {
                config.long_work = Some(Arc::new(move |label| {
                    if label == "publication_drain" {
                        signal.store(true, Ordering::SeqCst);
                    }
                }));
            });
            let h = Harness::new();
            h.store.delay(Some(Duration::from_secs(1)));
            submit_reader(&f, &runner).await;
            let job = running_fill(&runner, 0).await;
            let binding = attach(&runner, &job, &h, Tool::Ccache, Some(CCACHE_OK));
            let client = {
                let h = h.clone();
                let binding = binding.clone();
                tokio::spawn(async move { h.put(&binding, "/delayed", b"compiled").await.status })
            };
            until(|| !h.store.seen().is_empty()).await;
            assert_eq!(binding.ledger().unwrap().summary().in_flight, 1);
            fs::write(release_path(&f), "").unwrap();
            until(|| entered.load(Ordering::SeqCst)).await;
            let log = runner.job_log(job.id);
            assert!(log.write.try_lock().is_ok(), "drain holds job log lock");
            assert!(
                log.signalling.try_lock().is_ok(),
                "drain holds signalling lock"
            );
            {
                let inner = runner
                    .core
                    .inner
                    .try_lock()
                    .expect("drain holds state lock");
                assert!(
                    inner.jobs[&job.id].terminal.is_none(),
                    "terminal committed before drain"
                );
                assert_eq!(inner.writer, Some(job.id));
                assert!(inner.catalog.repositories[&f.root].publications.is_empty());
            }
            assert_eq!(client.await.unwrap(), 200);
            base_done(&runner).await;
            assert_eq!(binding.ledger().unwrap().summary().in_flight, 0);
            assert_eq!(
                repository(&f, &runner).publications[job.writer_descriptor.as_ref().unwrap()].keys
                    [0]
                .key,
                "delayed"
            );
        })
    });
}

// A hung fill finalization must fail this test, not stall the test executable.
// The spawned thread owns the fixture, so the timeout cannot drop the fixture
// while another thread holds the runner's state mutex.
fn bounded(test: impl FnOnce() + Send + 'static) {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        let _ = send.send(result);
    });
    match receive
        .recv_timeout(Duration::from_secs(20))
        .expect("publication fill timed out (possible engine lock deadlock)")
    {
        Ok(()) => {}
        Err(error) => std::panic::resume_unwind(error),
    }
}

#[test]
fn a_real_fill_records_its_publication() {
    bounded(|| {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let f = Fixture::new();
            declare(&f, "printf filled > command-marker");
            let runner = declared_runner(&f);
            warm(&f, &runner).await;
            let record = repository(&f, &runner);
            assert_eq!(count_fills(&runner), 1);
            assert!(record
                .publications
                .contains_key(record.filled_descriptor.as_ref().unwrap()));
        })
    });
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
    let seen = runner.core.inner.lock().unwrap().publishing_seen.clone();
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
