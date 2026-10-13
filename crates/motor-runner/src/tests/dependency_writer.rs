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
    let first = descriptor(&f.main, &tip, "rustc A", "cargo build").unwrap();
    assert_ne!(
        first,
        descriptor(&f.main, &tip, "rustc B", "cargo build").unwrap()
    );
    assert_ne!(
        first,
        descriptor(&f.main, &tip, "rustc A", "cargo test").unwrap()
    );
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
async fn writer_is_idle_only_singleton_and_not_age_promoted_on_large_and_small_hosts() {
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
        f.submit(&runner, &first, &bytes);
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
        assert_eq!(
            terminal(&runner, job.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
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
    fs::write(f.main.join("hostile.sh"), "#!/bin/bash\nif touch /motor-cache/wrapper-poison 2>/dev/null; then exit 77; fi\nexec \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(f.main.join("hostile.sh"), fs::Permissions::from_mode(0o755)).unwrap();
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
    assert_eq!(
        terminal(&runner, branch.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
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
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
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
    until(|| writer_count(&runner) == count + 1).await;
    base_done(&runner).await;
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
    let descriptor = crate::writer::fill::descriptor(
        &runner.core.trees.mirror(&f.root),
        &r.integration_tip,
        &runner.core.rustc_version,
        &runner.config().base_commands.join(" && "),
    )
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

#[tokio::test]
async fn queued_foreground_blocks_writer_even_when_writer_fits_idle_capacity() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 16;
    let runner = Runner::open(config).unwrap();
    let (r, bytes) = f.run("true");
    let upload = f.spool(&runner, &r, &bytes);
    let mut prepare = prepare_for(&r);
    prepare.bundles = r.bundles.clone();
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    let id = {
        let mut inner = runner.core.inner.lock().unwrap();
        let mut held = Job::new(Uuid::now_v7(), Some(r.clone()), runner.now(), 1, 12);
        held.state = JobState::Running;
        inner.jobs.insert(held.id, held);
        let mut foreground = Job::new(Uuid::now_v7(), Some(r.clone()), runner.now(), 2, 16);
        foreground.state = JobState::Queued;
        inner.jobs.insert(foreground.id, foreground);
        runner
            .queue_base(&mut inner, &f.root, &r.integration_tip, &f.key, &[], &[])
            .unwrap();
        inner.jobs.values().find(|j| j.base).unwrap().id
    };
    settle().await;
    let started = runner.core.inner.lock().unwrap().jobs[&id].setup_ms;
    runner.begin_drain(Duration::from_secs(1), false);
    assert_eq!(
        started, None,
        "writer used idle capacity while foreground work was queued"
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
