use super::*;

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

async fn leased_job_is_not_delayed_by_fill(tenant_vm: bool, boat_profile: bool) {
    let f = Fixture::new();
    let release = f.scratch.join("release-fill");
    let mut config = f.config();
    config.tenant_vm = tenant_vm;
    config.boat_profile = boat_profile;
    // The tenant has no write key. Boat is given one, which changes nothing:
    // Boat never fills.
    config.tenant_write_key = Some(boat_profile);
    config.boat_memory_max_bytes = boat_profile.then_some(2 << 30);
    if boat_profile {
        config.free_space_probe = Arc::new(|_| Ok(63 << 30));
        #[cfg(target_os = "linux")]
        {
            config.boat_storage_trim = Arc::new(|| Ok(()));
        }
    }
    config.max_weight = 4;
    config.base_commands = vec![format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    )];
    let runner = if boat_profile {
        Runner::open_with_backend(
            config,
            Box::new(super::boat_disk::SchedulerFixtureBackend::default()),
        )
        .unwrap()
    } else {
        Runner::open(config).unwrap()
    };
    let (request, bytes) = f.run("true");
    let upload = f.spool(&runner, &request, &bytes);
    let mut prepare = prepare_for(&request);
    prepare.bundles = request.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    runner.prepare(&prepare, true).unwrap();
    runner.tick().unwrap();
    let fill = runner.core.inner.lock().unwrap().writer;
    if let Some(id) = fill {
        until(|| {
            runner.core.inner.lock().unwrap().jobs[&id]
                .started_ms
                .is_some()
        })
        .await;
    }
    // The old scheduler starts a four-permit fill while idle and cannot admit
    // this two-permit job until that fill finishes. A tenant without a write
    // key and Boat must not fill at all.
    let (mut job, bytes) = job_at(&f, "leased-job", "true");
    job.weight_hint = Some(2);
    f.submit(&runner, &job, &bytes);
    let completed =
        tokio::time::timeout(Duration::from_secs(5), terminal(&runner, job.job_id)).await;
    fs::write(release, "").unwrap();
    let final_result = terminal(&runner, job.job_id).await;
    base_done(&runner).await;
    assert!(completed.is_ok(), "a background fill delayed a leased job");
    assert_eq!(final_result.outcome, Outcome::Exit { code: 0 });
    assert_eq!(writer_count(&runner), 0);
}

#[tokio::test]
async fn keyless_tenant_vm_fill_cannot_delay_a_leased_job() {
    leased_job_is_not_delayed_by_fill(true, false).await;
}

#[tokio::test]
async fn boat_fill_cannot_delay_a_leased_job_even_with_a_write_key() {
    leased_job_is_not_delayed_by_fill(false, true).await;
}

/// A tenant VM whose provisioning holds a write key fills its own cache from
/// the default-branch tip, as the build server does, and reports the fill's
/// permits in health so its host counts them as claimed while it runs.
#[tokio::test]
async fn keyed_tenant_vm_schedules_a_fill_and_reports_its_permits() {
    let f = Fixture::new();
    let release = f.scratch.join("release-fill");
    let mut config = f.config();
    config.tenant_vm = true;
    config.tenant_write_key = Some(true);
    config.base_commands = vec![format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    )];
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    let upload = f.spool(&runner, &request, &bytes);
    let mut prepare = prepare_for(&request);
    prepare.bundles = request.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    runner.prepare(&prepare, true).unwrap();
    runner.tick().unwrap();
    until(|| runner.core.inner.lock().unwrap().writer.is_some()).await;
    let fill = runner.core.inner.lock().unwrap().writer.unwrap();
    until(|| {
        runner.core.inner.lock().unwrap().jobs[&fill]
            .started_ms
            .is_some()
    })
    .await;
    let weight = runner.core.inner.lock().unwrap().jobs[&fill].weight;
    assert!(weight > 0);
    assert_eq!(
        runner.runner_status().health.unwrap().fill_permits,
        Some(weight)
    );
    fs::write(&release, "").unwrap();
    base_done(&runner).await;
    assert_eq!(writer_count(&runner), 1);
    assert_eq!(runner.runner_status().health.unwrap().fill_permits, Some(0));
}

#[derive(Clone, Default)]
struct LogBytes(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A tenant VM without a write key runs no fill and says why once, however
/// often a fill is considered; its jobs are unaffected.
#[tokio::test]
async fn keyless_tenant_vm_logs_once_and_runs_no_fill() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.tenant_write_key = Some(false);
    let bytes = LogBytes::default();
    let runner = tracing::subscriber::with_default(
        motor_protocol::logging::LineSubscriber::new("[runner]", bytes.clone()),
        || {
            let runner = Runner::open(config).unwrap();
            let (request, bytes) = f.run("true");
            let upload = f.spool(&runner, &request, &bytes);
            let mut prepare = prepare_for(&request);
            prepare.bundles = request.bundles.clone();
            runner.fetch_prepare(&prepare, &upload.directory).unwrap();
            for _ in 0..2 {
                runner.prepare(&prepare, true).unwrap();
                runner.tick().unwrap();
            }
            runner
        },
    );
    let log = String::from_utf8(bytes.0.lock().unwrap().clone()).unwrap();
    assert_eq!(
        log.matches("no remote cache write key is provisioned")
            .count(),
        1,
        "{log}"
    );
    assert!(runner.core.inner.lock().unwrap().writer.is_none());
    assert_eq!(writer_count(&runner), 0);
    assert_eq!(runner.runner_status().health.unwrap().fill_permits, Some(0));
}

#[tokio::test]
async fn leased_modes_do_not_requeue_persisted_fills() {
    for (tenant_vm, boat_profile) in [(true, false), (false, true)] {
        let f = Fixture::new();
        let mut config = f.config();
        config.manual_tick = true;
        let runner = Runner::open(config.clone()).unwrap();
        let (request, bytes) = f.run("true");
        let upload = f.spool(&runner, &request, &bytes);
        let mut prepare = prepare_for(&request);
        prepare.bundles = request.bundles.clone();
        runner.fetch_prepare(&prepare, &upload.directory).unwrap();
        runner.prepare(&prepare, true).unwrap();
        assert_eq!(writer_count(&runner), 1);
        drop(upload);
        drop(runner);
        config.tenant_vm = tenant_vm;
        config.boat_profile = boat_profile;
        // Neither mode has a write key, so neither may fill.
        config.tenant_write_key = Some(false);
        config.boat_memory_max_bytes = boat_profile.then_some(2 << 30);
        let runner = reopen(config);
        runner.tick().unwrap();
        let inner = runner.core.inner.lock().unwrap();
        assert!(inner.writer.is_none());
        assert!(inner
            .jobs
            .values()
            .filter(|job| job.base)
            .all(|job| job.terminal.is_some()));
        assert_eq!(inner.jobs.values().filter(|job| job.base).count(), 1);
    }
}

#[tokio::test]
async fn descriptor_reuses_source_only_tips_and_refills_manifest_and_config_changes() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let first = repository(&f, &runner);
    assert!(first.filled_descriptor.is_some());
    let count = writer_count(&runner);
    commit(&f.main, "source-only\n");
    warm(&f, &runner).await;
    let reused = repository(&f, &runner);
    assert_eq!(writer_count(&runner), count);
    assert_eq!(first.filled_descriptor, reused.filled_descriptor);
    assert_eq!(first.finished_ms, reused.finished_ms);
    assert_eq!(reused.commit, Some(git(&f.main, &["rev-parse", "HEAD"])));
    for (path, contents) in [
        ("Cargo.toml", "[workspace]\nmembers=[]\n"),
        (
            "nested/Cargo.toml",
            "[package]\nname='nested'\nversion='0.1.0'\n",
        ),
        (".cargo/config.toml", "[build]\njobs=2\n"),
        ("nested/.cargo/config", "[build]\njobs=1\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel='stable'\n"),
    ] {
        let file = f.main.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, contents).unwrap();
        commit(&f.main, path);
        let previous = writer_count(&runner);
        warm(&f, &runner).await;
        assert_eq!(writer_count(&runner), previous + 1, "{path} did not refill");
    }
    assert!(!runner.config().state_dir().join("generations").exists());
    assert_eq!(
        fs::read_dir(runner.config().writer_scratch_dir())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn descriptor_changes_for_toolchain_and_command_without_lockfile_changes() {
    let f = Fixture::new();
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    let descriptor = crate::writer::fill::descriptor;
    let compilers = Compilers::default();
    let of = |toolchain: &str, command: &str| {
        descriptor(&f.main, &tip, toolchain, command, None, &compilers).unwrap()
    };
    let first = of("rustc A", "cargo build");
    assert_ne!(first, of("rustc B", "cargo build"));
    assert_ne!(first, of("rustc A", "cargo test"));
}

#[tokio::test]
async fn failed_descriptor_is_suppressed_until_change_or_restart() {
    let f = Fixture::new();
    let mut config = f.config();
    config.base_commands = vec!["false".into()];
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
    let first = repository(&f, &runner);
    assert_eq!(first.result, Some(RebuildResult::Failed));
    let count = writer_count(&runner);
    commit(&f.main, "source-only after failure\n");
    warm(&f, &runner).await;
    assert_eq!(writer_count(&runner), count);
    drop(runner);
    let runner = reopen(config);
    until(|| writer_count(&runner) == count + 1).await;
    base_done(&runner).await;
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Failed));
    fs::write(f.main.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    commit(&f.main, "manifest change\n");
    warm(&f, &runner).await;
    assert_eq!(writer_count(&runner), count + 2);
}

#[tokio::test]
async fn writer_uses_only_integration_tip_not_branch_lockfile_or_siblings() {
    let f = Fixture::new();
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    fs::write(f.main.join("Cargo.lock"), "branch lockfile\n").unwrap();
    fs::create_dir_all(f.main.join(".cargo")).unwrap();
    fs::write(f.main.join(".cargo/config.toml"), "hostile branch config\n").unwrap();
    commit(&f.main, "branch only\n");
    let runner = Runner::open(f.config()).unwrap();
    let (mut r, bytes) = f.run("true");
    r.integration_tip = tip.clone();
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    let inner = runner.core.inner.lock().unwrap();
    let writer = inner
        .jobs
        .values()
        .find(|j| j.base)
        .unwrap()
        .request
        .as_ref()
        .unwrap();
    assert_eq!(writer.snapshot_commit, tip);
    assert!(writer.siblings.is_empty());
    assert!(!Path::new(&writer.cwd).join("Cargo.lock").exists());
    assert!(!Path::new(&writer.cwd).join(".cargo/config.toml").exists());
}

#[tokio::test]
async fn missing_tip_and_pinned_git_dependency_skip_without_building() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let count = writer_count(&runner);
    {
        let mut inner = runner.core.inner.lock().unwrap();
        runner
            .queue_base(&mut inner, &f.root, &"0".repeat(40), &f.key, &[], &[])
            .unwrap();
        assert_eq!(
            inner.catalog.repositories[&f.root].result,
            Some(RebuildResult::Failed)
        );
    }
    assert_eq!(writer_count(&runner), count);
    fs::write(f.main.join("Cargo.lock"), format!("version=4\n[[package]]\nname='private'\nversion='0.1.0'\nsource='git+https://private.invalid/repo?rev={}#{}'\n", "1".repeat(40), "1".repeat(40))).unwrap();
    commit(&f.main, "missing pinned dependency\n");
    warm(&f, &runner).await;
    assert_eq!(writer_count(&runner), count);
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Failed));
}

#[tokio::test]
async fn writer_lockfile_mutation_fails_and_scratch_is_removed() {
    let f = Fixture::new();
    fs::write(f.main.join("Cargo.lock"), "version=4\n\n").unwrap();
    commit(&f.main, "lockfile\n");
    let mut config = f.config();
    config.base_commands = vec!["printf 'version=4\\n' > Cargo.lock".into()];
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Failed));
    assert!(repository(&f, &runner).commit.is_none());
    assert_eq!(
        fs::read_dir(runner.config().writer_scratch_dir())
            .unwrap()
            .count(),
        0
    );
    assert!(!runner.config().state_dir().join("generations").exists());
}

#[tokio::test]
async fn writer_is_a_singleton_that_waits_for_free_permits_on_large_and_small_hosts() {
    for capacity in [16, 2] {
        let f = Fixture::new();
        let writer_release = f.scratch.join("writer-release");
        let foreground_release = f.scratch.join("foreground-release");
        let mut config = f.config();
        let topology = CpuTopology::new(
            (0..capacity + u32::from(capacity >= 4))
                .map(|cpu| vec![cpu])
                .collect(),
        )
        .unwrap();
        let capacity = topology.capacity();
        config.max_weight = capacity;
        config.cpu_topology = Some(topology);
        config.base_commands = vec![format!(
            "while test ! -e '{}'; do sleep .02; done",
            writer_release.display()
        )];
        let runner = Runner::open(config).unwrap();
        let (mut first, bytes) = job_at(
            &f,
            "saturated",
            &format!(
                "while test ! -e '{}'; do sleep .02; done",
                foreground_release.display()
            ),
        );
        first.weight_hint = Some(capacity);
        // Submitted as a branch tip, so that it queues no fill: a fill queued
        // with it would take its permits first.
        assert_run_accepted(&runner, &first);
        let spool = f.spool(&runner, &first, &bytes);
        runner.fetch_run(&first, &spool.directory, false).unwrap();
        until(|| {
            runner
                .status()
                .running_jobs
                .iter()
                .any(|j| j.job_id == first.job_id)
        })
        .await;
        let (mut next, bytes) = job_at(&f, "queued-foreground", "true");
        next.weight_hint = Some(1);
        f.submit(&runner, &next, &bytes);
        f.clock.fetch_add(BACKFILL_GRACE_MS + 1, Ordering::SeqCst);
        settle().await;
        assert!(runner.core.inner.lock().unwrap().writer.is_none());
        fs::write(foreground_release, "").unwrap();
        assert_eq!(
            terminal(&runner, first.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        assert_eq!(
            terminal(&runner, next.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        until(|| {
            runner
                .status()
                .running_jobs
                .iter()
                .any(|j| j.workspace_key.starts_with("base:"))
        })
        .await;
        {
            let inner = runner.core.inner.lock().unwrap();
            let id = inner.writer.unwrap();
            assert_eq!(inner.jobs[&id].weight, 4.min(capacity));
            assert_eq!(
                inner
                    .jobs
                    .values()
                    .filter(|j| j.base && j.started_ms.is_some() && j.terminal.is_none())
                    .count(),
                1
            );
        }
        let (mut behind, bytes) = job_at(&f, "behind-writer", "true");
        behind.weight_hint = Some(capacity);
        f.submit(&runner, &behind, &bytes);
        settle().await;
        assert!(!begun(&runner, behind.job_id));
        fs::write(writer_release, "").unwrap();
        assert_eq!(
            terminal(&runner, behind.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        base_done(&runner).await;
    }
}

/// Waits for job `id` to end and asserts that it exited 0. A failure prints
/// the whole terminal record, including any refusal detail, the job's
/// output and the writer diagnostics, so an intermittent failure explains
/// itself.
#[cfg(target_os = "linux")]
async fn assert_exited(f: &Fixture, runner: &Runner, id: Uuid) {
    let terminal = terminal(runner, id).await;
    assert_eq!(
        terminal.outcome,
        Outcome::Exit { code: 0 },
        "terminal: {terminal:?}\nrefusal detail: {:?}\noutput:\n{}\n{}",
        terminal.refusal_detail(),
        replay_text(runner, id),
        writer_diagnostics(f, runner)
    );
}

#[cfg(target_os = "linux")]
pub(super) async fn isolated_graph_fixture() {
    let f = Fixture::new();
    let source = f.scratch.join("git-dependencies");
    fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    for name in ["normal", "dev", "host_build", "host_macro", "derive"] {
        let root = source.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        let extra = if name == "normal" {
            "[features]\nextra=[]\n"
        } else if name == "derive" {
            "[lib]\nproc-macro=true\n[dependencies]\nhost_macro={path='../host_macro'}\n"
        } else {
            ""
        };
        fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n{extra}"),
        )
        .unwrap();
        let code = if name == "derive" {
            "extern crate proc_macro; #[proc_macro] pub fn answer(_: proc_macro::TokenStream) -> proc_macro::TokenStream { assert_eq!(host_macro::value(), 7); \"7u32\".parse().unwrap() }\n"
        } else if name == "normal" {
            "#[cfg(not(feature=\"extra\"))] compile_error!(\"extra feature required\"); pub fn value() -> u32 { 7 }\n"
        } else {
            "pub fn value() -> u32 { 7 }\n"
        };
        fs::write(root.join("src/lib.rs"), code).unwrap();
    }
    let (oid, _) = commit(&source, "bundled dependency graph\n");
    let url = format!("https://writer-{}.invalid/private.git", Uuid::now_v7());
    let pin = format!("git+{url}?rev={oid}#{oid}");
    let mut manifest = String::from("[package]\nname='writer-graph'\nversion='0.1.0'\nedition='2021'\n[features]\nextra=['normal/extra']\n[dependencies]\n");
    for name in ["normal", "derive"] {
        manifest.push_str(&format!("{name}={{git='{url}',rev='{oid}'}}\n"));
    }
    manifest.push_str(&format!("[dev-dependencies]\ndev={{git='{url}',rev='{oid}'}}\n[build-dependencies]\nhost_build={{git='{url}',rev='{oid}'}}\n"));
    fs::write(f.main.join("Cargo.toml"), manifest).unwrap();
    let mut lock = String::from("version=4\n");
    for name in ["normal", "dev", "host_build", "host_macro", "derive"] {
        lock.push_str(&format!(
            "[[package]]\nname='{name}'\nversion='0.1.0'\nsource='{pin}'\n"
        ));
        if name == "derive" {
            lock.push_str("dependencies=['host_macro']\n");
        }
    }
    lock.push_str("[[package]]\nname='writer-graph'\nversion='0.1.0'\ndependencies=['normal','dev','host_build','derive']\n");
    fs::write(f.main.join("Cargo.lock"), lock).unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() -> u32 { normal::value() + derive::answer!() } #[test] fn dev_graph() { assert_eq!(dev::value(), 7); }\n").unwrap();
    fs::write(f.main.join("build.rs"), "fn main() { assert_eq!(host_build::value(), 7); assert_eq!(std::env::var(\"CARGO_INCREMENTAL\").unwrap_or_default(), \"1\"); }\n").unwrap();
    let (tip, _) = commit(&f.main, "approved integration tip\n");
    let reference = format!("refs/ck-motor/dep/{oid}");
    git(&source, &["update-ref", &reference, &oid]);
    let path = f.scratch.join("dependency.bundle");
    git(&source, &["bundle", "create", text(&path), &reference]);
    let dep_bytes = fs::read(path).unwrap();
    let declaration = GitDependency {
        url: url.clone(),
        commits: vec![oid],
        bundle: Some(BundleManifest {
            index: 2,
            repository: url,
            length: dep_bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&dep_bytes)),
        }),
    };
    let mut config = f.config();
    config.isolate_jobs = true;
    config.base_commands = vec!["cargo test --no-run --workspace --features extra --locked && cargo build --release --features extra --locked && test \"$CARGO_INCREMENTAL\" = 1 && test -n \"$(ls -A /run/motor/target/release/incremental)\" && sccache --show-stats".into()];
    let unavailable = Command::new("cargo")
        .args(["metadata", "--locked", "--offline", "--format-version", "1"])
        .current_dir(&f.main)
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .output()
        .unwrap();
    assert!(
        !unavailable.status.success(),
        "unique tip-only git source was already fetched"
    );
    assert!(
        String::from_utf8_lossy(&unavailable.stderr).contains("offline"),
        "{}",
        String::from_utf8_lossy(&unavailable.stderr)
    );
    // The first request prepares a branch with no dependencies. Its bundle also
    // carries the approved git pin, but branch preparation cannot fetch that source.
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='writer-graph'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version=4\n[[package]]\nname='writer-graph'\nversion='0.1.0'\n",
    )
    .unwrap();
    commit(&f.main, "branch-first lockfile without tip dependencies\n");
    let runner = Runner::open(config).unwrap();
    let (mut r, mut bytes) = f.run("true");
    r.integration_tip = tip.clone();
    assert_ne!(r.snapshot_commit, r.integration_tip);
    r.git_dependencies = vec![declaration.clone()];
    bytes.push(dep_bytes.clone());
    f.submit(&runner, &r, &bytes);
    assert_exited(&f, &runner, r.job_id).await;
    base_done(&runner).await;
    assert_eq!(
        repository(&f, &runner).result,
        Some(RebuildResult::Ok),
        "{}",
        writer_diagnostics(&f, &runner)
    );
    assert_eq!(repository(&f, &runner).commit, Some(tip.clone()));
    assert_eq!(
        fs::read_to_string(Path::new(&r.workspace_key).join("Cargo.lock")).unwrap(),
        "version=4\n[[package]]\nname='writer-graph'\nversion='0.1.0'\n"
    );
    git(&f.main, &["checkout", &tip, "--", "."]);
    commit(&f.main, "restore approved dependency graph for readers\n");
    for name in ["first-reader", "second-reader"] {
        let (mut job, mut bytes) = job_at(&f, name, "export CARGO_INCREMENTAL=1; cargo test --no-run --workspace --features extra --locked && sccache --show-stats && test -n \"$(ls -A /run/motor/target/debug/incremental)\"");
        job.git_dependencies = vec![declaration.clone()];
        bytes.push(dep_bytes.clone());
        f.submit(&runner, &job, &bytes);
        assert_exited(&f, &runner, job.job_id).await;
        let output = replay_text(&runner, job.job_id);
        assert!(
            counter(&output, "Cache hits") >= 4,
            "normal, dev and both host libraries must hit: {output}"
        );
        assert_eq!(counter(&output, "Cache misses"), 0, "{output}");
        assert!(
            output.contains("Compiling derive"),
            "proc macro artifact must compile: {output}"
        );
        assert!(
            output.contains("Compiling writer-graph"),
            "incremental workspace must compile: {output}"
        );
    }
    fs::write(
        f.main.join("build.rs"),
        "fn main() { assert!(std::fs::write(\"/motor-cache/poison\", \"branch\").is_err()); }\n",
    )
    .unwrap();
    let mut branch_lock = fs::read_to_string(f.main.join("Cargo.lock")).unwrap();
    branch_lock.push_str("\n# branch-only lockfile change\n");
    fs::write(f.main.join("Cargo.lock"), branch_lock).unwrap();
    fs::create_dir_all(f.main.join(".cargo")).unwrap();
    fs::write(
        f.main.join(".cargo/config.toml"),
        "[build]\nrustc-wrapper='./hostile.sh'\n",
    )
    .unwrap();
    crate::test_helpers::write_test_executable(
        &f.main.join("hostile.sh"),
        b"#!/bin/bash\nif touch /motor-cache/wrapper-poison 2>/dev/null; then exit 77; fi\nexec \"$@\"\n",
    )
    .unwrap();
    commit(&f.main, "unmerged hostile branch\n");
    let count = writer_count(&runner);
    let (mut branch, mut bytes) = job_at(
        &f,
        "hostile-reader",
        "cargo test --no-run --workspace --features extra --locked",
    );
    branch.integration_tip = tip;
    branch.git_dependencies = vec![declaration];
    bytes.push(dep_bytes);
    f.submit(&runner, &branch, &bytes);
    assert_exited(&f, &runner, branch.job_id).await;
    base_done(&runner).await;
    assert_eq!(writer_count(&runner), count);
    assert!(!runner.config().sccache_dir().join("poison").exists());
    assert!(!runner
        .config()
        .sccache_dir()
        .join("wrapper-poison")
        .exists());
    runner.begin_drain(Duration::from_secs(10), false);
    until(|| runner.drain_result().is_some()).await;
    crate::isolation::restore_runner_access(f.scratch.path()).unwrap();
}

#[cfg(target_os = "linux")]
fn replay_text(runner: &Runner, id: Uuid) -> String {
    String::from_utf8(
        runner
            .replay(id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|f| match f {
                Frame::Output(o) => Some(o.bytes.0),
                _ => None,
            })
            .flatten()
            .collect(),
    )
    .unwrap()
}

#[cfg(target_os = "linux")]
fn counter(output: &str, name: &str) -> u64 {
    output
        .lines()
        .find(|l| l.starts_with(&format!("{name} ")))
        .unwrap_or_else(|| panic!("missing {name}: {output}"))
        .split_whitespace()
        .last()
        .unwrap()
        .parse()
        .unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn restart_stops_writer_before_scratch_cleanup_and_requeues_descriptor() {
    use std::os::unix::process::ExitStatusExt;

    async fn drive_idle(runner: &Runner) {
        until_with_diagnostics(
            || {
                runner.tick().unwrap();
                let inner = runner.core.inner.lock().unwrap();
                // A terminal writer can still be publishing its catalog result
                // or settling scratch cleanup. Wait for those owners as well.
                inner.jobs.values().all(|job| job.terminal.is_some())
                    && inner.writer.is_none()
                    && inner.finalizing.is_empty()
                    && inner.base_finishes.is_empty()
                    && inner.base_settling.is_empty()
            },
            || runner_diagnostics(runner),
        )
        .await;
    }

    let f = Fixture::new();
    let mut config = f.config();
    // The interrupted record below exists only on disk. A background tick must
    // not consume the unfilled descriptor before the simulated restart.
    config.manual_tick = true;
    let runner = Runner::open(config.clone()).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    drive_idle(&runner).await;
    assert_exited(&f, &runner, request.job_id).await;
    let mut writer = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .find(|j| j.base)
        .unwrap()
        .clone();
    let count = writer_count(&runner);
    writer.terminal = None;
    writer.state = JobState::Running;
    writer.process = Some(platform::ProcessIdentity {
        pid: 1,
        start: 1,
        unit: Some(Uuid::now_v7()),
    });
    runner.save(&writer).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let g = inner.catalog.repositories.get_mut(&f.root).unwrap();
        g.filled_descriptor = None;
        g.filled_descriptors.clear();
        g.result = Some(RebuildResult::Building);
        runner.catalog(&inner).unwrap();
    }
    let scratch = config.writer_scratch_dir().join(writer.id.to_string());
    fs::create_dir_all(&scratch).unwrap();
    fs::write(scratch.join("live"), "unit still owns this").unwrap();
    assert_eq!(writer_count(&runner), count);
    drop(runner);
    let stopped = Arc::new(AtomicBool::new(false));
    let observed = stopped.clone();
    let live_scratch = scratch.clone();
    let mut config = config;
    config.systemd_command = Some(Arc::new(move |args| {
        let stdout = if args[0] == "stop" {
            assert!(
                live_scratch.join("live").exists(),
                "scratch deleted beneath live writer"
            );
            observed.store(true, Ordering::SeqCst);
            ""
        } else if args[0] == "show" && args.contains(&"--property=ActiveState") {
            if observed.load(Ordering::SeqCst) {
                "ActiveState=inactive\n"
            } else {
                "ActiveState=active\n"
            }
        } else {
            ""
        };
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        })
    }));
    let runner = reopen(config);
    assert!(stopped.load(Ordering::SeqCst));
    assert!(!scratch.exists());
    assert_eq!(writer_count(&runner), count);
    runner.tick().unwrap();
    assert_eq!(writer_count(&runner), count + 1);
    drive_idle(&runner).await;
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Ok));
    assert!(runner.core.inner.lock().unwrap().writer.is_none());
}

#[tokio::test]
async fn cacheable_reader_misses_refill_the_successful_descriptor() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let count = writer_count(&runner);
    miss_reader(&f, &runner, "reader-after-eviction", None).await;
    until(|| writer_count(&runner) == count + 1).await;
    base_done(&runner).await;
    assert_eq!(repository(&f, &runner).result, Some(RebuildResult::Ok));
    miss_reader(&f, &runner, "reader-still-missing", None).await;
    settle().await;
    assert_eq!(
        writer_count(&runner),
        count + 1,
        "same units repeatedly missed after the bounded refill"
    );
}

#[test]
fn offline_preflight_rejects_sibling_paths_and_missing_registry_sources_without_compiling() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::create_dir_all(f.sibling.join("src")).unwrap();
    fs::write(
        f.sibling.join("Cargo.toml"),
        "[package]\nname='sibling'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(f.sibling.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(f.main.join("Cargo.toml"), "[package]\nname='preflight'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nsibling={path='../commons'}\n").unwrap();
    fs::write(f.main.join("Cargo.lock"), "version=4\n[[package]]\nname='preflight'\nversion='0.1.0'\ndependencies=['sibling']\n[[package]]\nname='sibling'\nversion='0.1.0'\n").unwrap();
    let run = || {
        Command::new("python3")
            .args(["-c", crate::writer::fill::PREFLIGHT])
            .current_dir(&f.main)
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_HOME", f.scratch.join("empty-cargo"))
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .output()
            .unwrap()
    };
    let rejected = run();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("path dependencies outside the repository"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!f.main.join("target").exists());
    fs::write(f.main.join("Cargo.toml"), "[package]\nname='preflight'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nmissing-writer-source='=0.0.0'\n").unwrap();
    assert!(!run().status.success());
    assert!(!f.main.join("target").exists());
}

#[cfg(target_os = "linux")]
const RESTART_COMMAND: &str = "printf 'writer-live\\n'; while true; do sleep 1; done";

#[cfg(target_os = "linux")]
#[test]
fn writer_restart_child() {
    let Some(path) = std::env::var_os("MOTOR_WRITER_RESTART_FIXTURE") else {
        return;
    };
    let path = PathBuf::from(path);
    let r: Run = read_json(&path.join("request.json")).unwrap();
    let mut config = Config::new(path.join("state"), "e2e");
    config.free_space_floor_bytes = 0;
    config.workspace_roots = vec![path.join("remote/worktrees")];
    config.owned_workspace_roots = vec![path.clone()];
    config.isolate_jobs = true;
    config.base_commands = vec![RESTART_COMMAND.into()];
    let runner = Runner::open(config).unwrap();
    runner.accept(r.clone()).unwrap();
    runner.fetch_run(&r, &path.join("bundles"), true).unwrap();
    assert!(
        poll(Duration::from_secs(90), || {
            let inner = runner.core.inner.lock().unwrap();
            inner
                .jobs
                .values()
                .find(|j| j.base && j.started_ms.is_some())
                .is_some_and(|j| {
                    fs::read_to_string(
                        runner
                            .core
                            .trees
                            .state
                            .join("sandboxes")
                            .join(j.id.to_string())
                            .join("output/stdout"),
                    )
                    .is_ok_and(|o| o.contains("writer-live"))
                })
        }),
        "writer did not reach its live unit"
    );
    unsafe { libc::_exit(87) }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires motor-cache, motor-job, sudo, ACLs and systemd"]
async fn isolated_restart_reaps_live_writer_unit_before_deleting_scratch() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='restart-writer'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version=4\n[[package]]\nname='restart-writer'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    commit(&f.main, "restart fixture\n");
    let (mut r, bytes) = f.run("true");
    r.siblings.clear();
    r.bundles.truncate(1);
    atomic_json(&f.scratch.join("request.json"), &r).unwrap();
    fs::create_dir_all(f.scratch.join("bundles")).unwrap();
    fs::write(f.scratch.join("bundles/0"), &bytes[0]).unwrap();
    let status = crate::test_process::ChildGuard::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::tests::dependency_writer::writer_restart_child",
                "--nocapture",
            ])
            .env("MOTOR_WRITER_RESTART_FIXTURE", f.scratch.path())
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0"),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(status.code(), Some(87));
    let mut config = f.config();
    config.isolate_jobs = true;
    config.base_commands = vec![RESTART_COMMAND.into()];
    let jobs: Vec<Job> = fs::read_dir(config.state_dir().join("jobs"))
        .unwrap()
        .map(|e| read_json(&e.unwrap().path().join("record.json")).unwrap())
        .collect();
    let previous = jobs
        .iter()
        .find(|j| j.base && j.terminal.is_none())
        .unwrap();
    let process = previous.process.unwrap();
    f.cleanup.0.lock().unwrap().register(process);
    assert!(crate::isolation::unit_alive(process.unit.unwrap()).unwrap());
    let old_scratch = config.writer_scratch_dir().join(previous.id.to_string());
    assert!(old_scratch.exists());
    let runner = reopen(config);
    assert!(!crate::isolation::unit_alive(process.unit.unwrap()).unwrap());
    assert!(!old_scratch.exists());
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if runner
                .status()
                .running_jobs
                .iter()
                .any(|j| j.workspace_key.starts_with("base:"))
            {
                break;
            }
            assert_ne!(
                repository(&f, &runner).result,
                Some(RebuildResult::Failed),
                "replacement writer failed before running: {}",
                writer_diagnostics(&f, &runner)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "replacement writer never reached Running: {}",
            writer_diagnostics(&f, &runner)
        )
    });
    let id = {
        let inner = runner.core.inner.lock().unwrap();
        assert_eq!(
            inner
                .jobs
                .values()
                .filter(|j| j.base && j.terminal.is_none())
                .count(),
            1
        );
        inner.writer.unwrap()
    };
    runner.cancel(id).unwrap();
    base_done(&runner).await;
    assert!(runner.core.inner.lock().unwrap().writer.is_none());
    crate::isolation::restore_runner_access(f.scratch.path()).unwrap();
}

#[test]
fn failed_descriptor_request_queues_nothing() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 1;
    let runner = Runner::open(config).unwrap();
    let (r, bytes) = f.run("true");
    let upload = f.spool(&runner, &r, &bytes);
    let mut prepare = prepare_for(&r);
    prepare.bundles = r.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    let descriptor = runner
        .fill_identity(
            &runner.core.trees.mirror(&f.root),
            &r.integration_tip,
            &runner.config().base_commands.join(" && "),
            true,
        )
        .0
        .unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    inner
        .catalog
        .repositories
        .entry(f.root.clone())
        .or_default()
        .failed_descriptor = Some(descriptor);
    runner
        .queue_base(&mut inner, &f.root, &r.integration_tip, &f.key, &[], &[])
        .unwrap();
    assert!(inner.jobs.values().all(|j| !j.base));
    assert_eq!(
        inner.catalog.repositories[&f.root].result,
        Some(RebuildResult::Failed)
    );
}

/// Under steady load the foreground queue never empties, and the oldest
/// queued job usually waits for more permits than are free. A cache fill
/// must still get through: it is tried before that job and takes only free
/// permits, the repository whose jobs arrived last fills first, and the
/// single writer slot keeps the next fill waiting until this one ends.
#[tokio::test]
async fn one_fill_reaches_free_permits_past_a_queued_job_that_does_not_fit() {
    let f = Fixture::new();
    let release = f.scratch.join("fill-release");
    let mut config = f.config();
    config.max_weight = 16;
    config.base_commands = vec![format!(
        "while test ! -e '{}'; do sleep .02; done",
        release.display()
    )];
    let runner = Runner::open(config).unwrap();
    // Two repositories with mirrors: the fixture's main one, and commons.
    let (main, bytes) = f.run("true");
    let upload = f.spool(&runner, &main, &bytes);
    let mut prepare = prepare_for(&main);
    prepare.bundles = main.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    let (mut commons, _) = job_at(&f, "commons-job", "true");
    let (manifest, bundle) = f.bundle(&f.sibling, &f.canonical, 0);
    let tip = git(&f.sibling, &["rev-parse", "HEAD"]);
    commons.repository_root = f.canonical.clone();
    commons.bundles = vec![manifest];
    commons.siblings = vec![];
    commons.snapshot_commit = tip.clone();
    commons.integration_tip = tip.clone();
    commons.base_commit = Some(tip.clone());
    commons.tree_hash = git(&f.sibling, &["rev-parse", "HEAD^{tree}"]);
    let upload = f.spool(&runner, &commons, &[bundle]);
    let mut prepare = prepare_for(&commons);
    prepare.bundles = commons.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    let (main_fill, commons_fill, aged) = {
        let mut inner = runner.core.inner.lock().unwrap();
        let now = runner.now();
        // A main job holds 12 of 16 permits; a commons job, accepted later
        // and past its backfill grace, waits for all 16.
        inner.order += 1;
        let mut held = Job::new(Uuid::now_v7(), Some(main.clone()), now, inner.order, 12);
        held.state = JobState::Running;
        inner.jobs.insert(held.id, held);
        inner.order += 1;
        let mut waiting = Job::new(Uuid::now_v7(), Some(commons.clone()), now, inner.order, 16);
        waiting.state = JobState::Queued;
        waiting.queued_ms = Some(now - BACKFILL_GRACE_MS - 1);
        let aged = waiting.id;
        inner.jobs.insert(aged, waiting);
        // The main fill is queued first, so queue order alone would pick it.
        let fill = |inner: &mut Inner, root: &str, tip: &str, key: &str| {
            runner.queue_base(inner, root, tip, key, &[], &[]).unwrap();
            inner
                .jobs
                .values()
                .filter(|j| j.base && j.request.as_ref().unwrap().repository_root == root)
                .map(|j| j.id)
                .max()
                .unwrap()
        };
        let main_fill = fill(&mut inner, &f.root, &main.integration_tip, &f.key);
        let commons_fill = fill(&mut inner, &f.canonical, &tip, &commons.workspace_key);
        (main_fill, commons_fill, aged)
    };
    until_with_diagnostics(
        || runner.core.inner.lock().unwrap().writer.is_some(),
        || {
            format!(
                "no fill started past the queued job; {}",
                runner_diagnostics(&runner)
            )
        },
    )
    .await;
    settle().await;
    // Read under the state lock, assert after releasing it: a failed
    // assertion while holding it would poison it for the runner's threads.
    let (writer, weight, main_setup, aged_setup) = {
        let inner = runner.core.inner.lock().unwrap();
        (
            inner.writer,
            inner.jobs[&commons_fill].weight,
            inner.jobs[&main_fill].setup_ms,
            inner.jobs[&aged].setup_ms,
        )
    };
    assert_eq!(
        writer,
        Some(commons_fill),
        "the fill for the repository with the newest job must go first"
    );
    assert_eq!(weight, 4);
    assert_eq!(
        main_setup, None,
        "a second fill started while the first was running"
    );
    assert_eq!(aged_setup, None);
    fs::write(&release, "").unwrap();
    terminal(&runner, commons_fill).await;
    // The queued job still waits, and the next fill starts all the same.
    assert_eq!(
        terminal(&runner, main_fill).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let aged_setup = runner.core.inner.lock().unwrap().jobs[&aged].setup_ms;
    assert!(aged_setup.is_none());
    runner.begin_drain(Duration::from_secs(1), false);
}

/// A fill that does not fit yet keeps the permits that come free for itself:
/// otherwise small jobs arriving behind it would take each one, and under
/// steady load the fill would never start.
#[tokio::test]
async fn a_waiting_fill_keeps_free_permits_from_later_jobs() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 16;
    let runner = Runner::open(config).unwrap();
    let (main, bytes) = f.run("true");
    let upload = f.spool(&runner, &main, &bytes);
    let mut prepare = prepare_for(&main);
    prepare.bundles = main.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    let (small, _) = job_at(&f, "small", "true");
    let (fill, small) = {
        let mut inner = runner.core.inner.lock().unwrap();
        let now = runner.now();
        // 13 of 16 permits are held: the 4-permit fill does not fit, a
        // 1-permit job would.
        inner.order += 1;
        let mut held = Job::new(Uuid::now_v7(), Some(main.clone()), now, inner.order, 13);
        held.state = JobState::Running;
        inner.jobs.insert(held.id, held);
        runner
            .queue_base(&mut inner, &f.root, &main.integration_tip, &f.key, &[], &[])
            .unwrap();
        let fill = inner.jobs.values().find(|j| j.base).unwrap().id;
        inner.order += 1;
        let mut waiting = Job::new(small.job_id, Some(small.clone()), now, inner.order, 1);
        waiting.state = JobState::Queued;
        waiting.queued_ms = Some(now);
        inner.jobs.insert(waiting.id, waiting);
        (fill, small.job_id)
    };
    settle().await;
    let (writer, small_setup) = {
        let inner = runner.core.inner.lock().unwrap();
        (inner.writer, inner.jobs[&small].setup_ms)
    };
    runner.begin_drain(Duration::from_secs(1), false);
    assert_eq!(writer, None);
    assert_eq!(
        small_setup, None,
        "a later job took the permits fill {fill} is waiting for"
    );
}

async fn miss_reader(f: &Fixture, runner: &Runner, name: &str, approved_tip: Option<&str>) {
    let (mut reader, bytes) = job_at(f, name, "true");
    if let Some(tip) = approved_tip {
        reader.integration_tip = tip.into();
    }
    let stats = runner
        .core
        .trees
        .state
        .join("sandboxes")
        .join(reader.job_id.to_string())
        .join("output/cache-stats");
    fs::create_dir_all(stats.parent().unwrap()).unwrap();
    fs::write(stats, r#"{"stats":{"cache_misses":{"counts":{"Rust":1}}}}"#).unwrap();
    assert!(crate::writer::fill::reader_missed(
        &runner.core.trees.state,
        reader.job_id
    ));
    f.submit(runner, &reader, &bytes);
    assert_eq!(
        terminal(runner, reader.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
}

#[tokio::test]
async fn branch_only_dependency_misses_never_queue_a_trusted_refill() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let tip = repository(&f, &runner).commit.unwrap();
    let count = writer_count(&runner);
    fs::write(f.main.join("Cargo.toml"), "[package]\nname='branch-only'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nitoa='=1.0.18'\n").unwrap();
    commit(&f.main, "branch-only dependency\n");
    for name in ["branch-miss-first", "branch-miss-again"] {
        miss_reader(&f, &runner, name, Some(&tip)).await;
        settle().await;
        assert_eq!(
            writer_count(&runner),
            count,
            "branch-specific units cannot be filled from the approved tip"
        );
    }
    assert!(repository(&f, &runner).miss_refills.is_empty());
}

#[tokio::test]
async fn miss_refill_bound_survives_runner_restart() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
    let count = writer_count(&runner);
    miss_reader(&f, &runner, "first-miss", None).await;
    until(|| writer_count(&runner) == count + 1).await;
    base_done(&runner).await;
    let markers = repository(&f, &runner).miss_refills;
    assert_eq!(markers.len(), 1);
    drop(runner);
    let runner = reopen(config);
    assert_eq!(repository(&f, &runner).miss_refills, markers);
    miss_reader(&f, &runner, "miss-after-restart", None).await;
    settle().await;
    assert_eq!(
        writer_count(&runner),
        count + 1,
        "restart reset the miss-triggered refill bound"
    );
}

#[tokio::test]
async fn new_trusted_tip_gets_its_own_miss_refill_budget() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let count = writer_count(&runner);
    miss_reader(&f, &runner, "first-tip-miss", None).await;
    until(|| writer_count(&runner) == count + 1).await;
    base_done(&runner).await;
    commit(&f.main, "new default-branch source\n");
    warm(&f, &runner).await;
    assert_eq!(
        writer_count(&runner),
        count + 1,
        "source-only tip should reuse the descriptor"
    );
    let current = repository(&f, &runner);
    let key = format!(
        "{}:{}",
        current.commit.unwrap(),
        current.filled_descriptor.unwrap()
    );
    miss_reader(&f, &runner, "new-tip-miss", None).await;
    assert!(
        repository(&f, &runner).miss_refills.contains(&key),
        "new approved tip did not receive a refill marker"
    );
    until(|| writer_count(&runner) == count + 2).await;
    base_done(&runner).await;
    assert_eq!(repository(&f, &runner).miss_refills.len(), 2);
}

#[tokio::test]
async fn repository_without_rust_graph_records_tip_without_a_writer() {
    let f = Fixture::new();
    let mut config = f.config();
    config.base_commands.clear();
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    let first = repository(&f, &runner);
    assert_eq!(first.result, Some(RebuildResult::Ok));
    assert_eq!(writer_count(&runner), 0);
    assert!(!runner.config().writer_scratch_dir().exists());
    f.clock.fetch_add(1000, Ordering::SeqCst);
    let (next, _) = commit(&f.main, "source-only default branch\n");
    warm(&f, &runner).await;
    let reused = repository(&f, &runner);
    assert_eq!(reused.commit, Some(next));
    assert_eq!(reused.finished_ms, first.finished_ms);
    assert_eq!(writer_count(&runner), 0);
}

// Only the Linux writer tests call `writer_diagnostics`.
#[cfg(target_os = "linux")]
fn writer_diagnostics(f: &Fixture, runner: &Runner) -> String {
    let (catalog, jobs) = {
        let inner = runner.core.inner.lock().unwrap();
        (
            serde_json::to_string_pretty(&inner.catalog.repositories.get(&f.root)).unwrap(),
            inner
                .jobs
                .values()
                .filter(|j| j.base)
                .map(|j| (j.id, serde_json::to_string(j).unwrap()))
                .collect::<Vec<_>>(),
        )
    };
    let mut diagnostic = format!("writer catalog: {catalog}");
    for (id, record) in jobs {
        let bytes: Vec<u8> = runner
            .replay(id, 0)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| match f {
                Frame::Output(o) => Some(o.bytes.0),
                _ => None,
            })
            .flatten()
            .collect();
        diagnostic.push_str(&format!(
            "\nwriter {id}: {record}\n{}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    diagnostic
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn empty_and_invalid_writer_statistics_fail_with_a_persisted_reason() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    warm(&f, &runner).await;
    let original = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .find(|j| j.base)
        .unwrap()
        .clone();
    for (contents, reason) in [
        ("", "empty statistics file"),
        ("not-json", "invalid statistics JSON"),
        ("{}", "cache_misses counter"),
    ] {
        let mut job = original.clone();
        job.id = Uuid::now_v7();
        job.request.as_mut().unwrap().job_id = job.id;
        job.terminal = None;
        job.state = JobState::Running;
        job.durable_output = true;
        let stats = runner
            .core
            .trees
            .state
            .join("sandboxes")
            .join(job.id.to_string())
            .join("output/cache-stats");
        fs::create_dir_all(stats.parent().unwrap()).unwrap();
        fs::write(stats, contents).unwrap();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let g = inner.catalog.repositories.get_mut(&f.root).unwrap();
            g.filled_descriptor = None;
            g.filled_descriptors.clear();
            g.failed_descriptor = None;
            inner.jobs.insert(job.id, job.clone());
        }
        runner
            .finish_execution(
                job.id,
                Ok(backend::ExecutionOutcome::Unix(
                    std::process::ExitStatus::from_raw(0),
                )),
                true,
            )
            .unwrap();
        let g = repository(&f, &runner);
        assert_eq!(g.result, Some(RebuildResult::Failed));
        assert!(
            g.failure_detail
                .as_deref()
                .is_some_and(|d| d.contains("writer statistics:") && d.contains(reason)),
            "missing failure reason: {}",
            writer_diagnostics(&f, &runner)
        );
        let saved: Catalog = read_json(&runner.core.trees.state.join("catalog.json")).unwrap();
        assert_eq!(saved.repositories[&f.root].failure_detail, g.failure_detail);
    }
}

#[tokio::test]
async fn restart_does_not_authorize_legacy_catalog_tips_without_descriptors() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
    let count = writer_count(&runner);
    let path = runner.core.trees.state.join("catalog.json");
    fs::write(f.main.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    let (unapproved, _) = commit(&f.main, "legacy branch tip\n");
    let (request, bytes) = f.run("true");
    runner.accept(request.clone()).unwrap();
    let upload = f.spool(&runner, &request, &bytes);
    runner
        .fetch_run(&request, &upload.directory, false)
        .unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    drop(runner);
    let mut legacy: serde_json::Value = read_json(&path).unwrap();
    let g = legacy["repositories"][&f.root].as_object_mut().unwrap();
    g.get_mut("desired").unwrap()["tip"] = unapproved.into();
    for field in [
        "requested_descriptor",
        "filled_descriptor",
        "filled_descriptors",
        "failed_descriptor",
        "miss_refills",
    ] {
        g.remove(field);
    }
    atomic_json(&path, &legacy).unwrap();
    let runner = reopen(config);
    settle().await;
    assert_eq!(
        writer_count(&runner),
        count,
        "legacy desired tip was replayed without default-branch approval"
    );
    assert!(repository(&f, &runner).desired.is_none());
    warm(&f, &runner).await;
    assert_eq!(
        writer_count(&runner),
        count + 1,
        "new capable request did not authorize a fill"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires trusted operator, motor-cache, motor-job, sccache, ACLs, sudo and systemd"]
async fn isolated_state_acl_stays_private_after_startup_and_cache_fill() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    fs::write(f.main.join("source.rs"), "pub fn value() -> u32 { 7 }\n").unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='acl-cache'\nversion='0.1.0'\nedition='2021'\n[lib]\npath='source.rs'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version=4\n[[package]]\nname='acl-cache'\nversion='0.1.0'\n",
    )
    .unwrap();
    commit(&f.main, "cache ACL fixture\n");
    let mut config = f.config();
    config.isolate_jobs = true;
    config.base_commands = vec!["sccache rustc --crate-type=rlib --crate-name acl_cache --emit=link source.rs --out-dir \"$CARGO_TARGET_DIR\" && sccache --show-stats".into()];
    let state = config.state_dir();
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    // chmod restricts an extended ACL's mask, not its owning-group entry.
    // Simulate a prior installation with group access hidden behind that mask.
    let seeded = Command::new("/usr/bin/setfacl")
        .args([
            "-m",
            "u:motor-job:--x,u:motor-cache:--x,g::rwx,m::rwx",
            "--",
        ])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    let assert_private = || {
        let output = Command::new("/usr/bin/getfacl")
            .args(["-cp", "--"])
            .arg(&state)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let acl = String::from_utf8(output.stdout).unwrap();
        for entry in [
            "user::rwx",
            "user:motor-job:--x",
            "user:motor-cache:--x",
            "group::---",
            "mask::--x",
            "other::---",
        ] {
            assert!(
                acl.lines().any(|line| line == entry),
                "missing {entry}:\n{acl}"
            );
        }
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o710
        );
    };
    let runner = Runner::open(config).unwrap();
    assert!(
        runner.config().writer_account.is_some(),
        "cache writer must be enabled"
    );
    assert_private();
    fn cache_files(path: &Path) -> usize {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    cache_files(&entry.path())
                } else {
                    usize::from(entry.file_type().unwrap().is_file())
                }
            })
            .sum()
    }
    assert_eq!(cache_files(&runner.config().sccache_dir()), 0);
    let (r, bytes) = f.run("true");
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    assert_eq!(
        repository(&f, &runner).result,
        Some(RebuildResult::Ok),
        "{}",
        writer_diagnostics(&f, &runner)
    );
    let fills = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| job.base)
        .count();
    assert_eq!(fills, 1, "fixture must complete one real cache fill");
    assert!(
        cache_files(&runner.config().sccache_dir()) > 0,
        "fill must publish a real cache entry: {}",
        writer_diagnostics(&f, &runner)
    );
    assert_private();
}

/// Eviction moves the compiler cache aside, puts an empty one in its place
/// and then deletes the old one. If that deletion fails, the cache jobs see
/// is still empty, so the fill records must go and the repository refill.
#[tokio::test]
async fn compiler_cache_emptied_by_a_failed_eviction_is_filled_again() {
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "an undeletable entry requires a non-root runner"
    );
    let f = Fixture::new();
    let mut config = f.config();
    config.free_space_floor_bytes = 100;
    let fleet = config.sccache_dir();
    let shard = fleet.join("0");
    let pressure = Arc::new(AtomicBool::new(false));
    let pressured = pressure.clone();
    let cached = shard.clone();
    // Below the hard floor for as long as the cache holds its entry.
    config.free_space_probe = Arc::new(move |_| {
        Ok(if pressured.load(Ordering::SeqCst) && cached.exists() {
            10
        } else {
            300
        })
    });
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    let filled = repository(&f, &runner).filled_descriptor;
    assert!(filled.is_some());
    let fills = writer_count(&runner);
    // An entry in a read-only shard cannot be deleted by the runner.
    fs::create_dir_all(&shard).unwrap();
    fs::write(shard.join("entry"), "cached").unwrap();
    fs::set_permissions(&shard, fs::Permissions::from_mode(0o500)).unwrap();
    pressure.store(true, Ordering::SeqCst);
    let _ = runner.ensure_disk_space().unwrap();
    pressure.store(false, Ordering::SeqCst);
    // Let the fixture clean up the copy that could not be deleted.
    let trash = runner.config().state_dir().join("trash");
    let mut stranded = false;
    for entry in fs::read_dir(&trash).unwrap() {
        let retired = entry.unwrap().path().join("0");
        if retired.is_dir() {
            fs::set_permissions(&retired, fs::Permissions::from_mode(0o700)).unwrap();
            stranded = true;
        }
    }
    assert!(stranded, "the eviction did not fail after moving the cache");
    assert!(
        fleet.is_dir() && !shard.exists(),
        "the cache was not emptied"
    );
    assert_eq!(
        repository(&f, &runner).filled_descriptor,
        None,
        "a fill is still recorded for an empty compiler cache"
    );
    runner.tick().unwrap();
    base_done(&runner).await;
    assert_eq!(
        writer_count(&runner),
        fills + 1,
        "the cache was not refilled"
    );
    assert_eq!(repository(&f, &runner).filled_descriptor, filled);
}

/// A compiler cache emptied while the runner was down (by hand, or by an
/// eviction that moved it aside and did not live to save the catalog) holds
/// none of the recorded fills; one that still holds entries keeps them.
#[tokio::test]
async fn restart_forgets_fills_of_an_emptied_compiler_cache() {
    let f = Fixture::new();
    let mut config = f.config();
    let fleet = config.sccache_dir();
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
    let filled = repository(&f, &runner).filled_descriptor.unwrap();
    let fills = writer_count(&runner);
    fs::create_dir_all(fleet.join("0/1")).unwrap();
    drop(runner);
    // Ticks are driven by hand, so a refill cannot replace the records the
    // assertions read before it runs.
    config.manual_tick = true;
    let runner = reopen(config.clone());
    assert!(
        repository(&f, &runner)
            .filled_descriptors
            .contains_key(&filled),
        "a restart forgot the fills of a cache that still holds entries"
    );
    drop(runner);
    fs::remove_dir_all(fleet.join("0")).unwrap();
    let runner = reopen(config);
    assert!(
        repository(&f, &runner).filled_descriptors.is_empty(),
        "a fill is still recorded for an empty compiler cache"
    );
    runner.tick().unwrap();
    base_done(&runner).await;
    assert_eq!(
        writer_count(&runner),
        fills + 1,
        "the cache was not refilled"
    );
    assert_eq!(
        repository(&f, &runner).filled_descriptor.as_ref(),
        Some(&filled)
    );
}
