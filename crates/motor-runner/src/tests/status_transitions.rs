use super::*;

fn status(runner: &Runner) -> RepositoryStatus {
    runner.status().repositories.into_iter().next().unwrap()
}

fn assert_status(runner: &Runner, tip: Option<&str>, result: RebuildResult, age: Option<u64>) {
    let status = status(runner);
    assert_eq!(status.published_commit.as_deref(), tip);
    assert_eq!(status.last_rebuild_result, Some(result));
    assert_eq!(status.warm_target_age_s, age);
}

fn manifest(command: &str) -> String {
    format!(
        "[workspace]\n[workspace.metadata.motor]\nwarm-commands = [{}]\n",
        serde_json::to_string(command).unwrap()
    )
}

fn prepare_tip(f: &Fixture, runner: &Runner) -> String {
    let (run, bytes) = f.run("true");
    let upload = f.spool(runner, &run, &bytes);
    let mut prepare = prepare_for(&run);
    prepare.bundles = run.bundles;
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    runner.prepare(&prepare, true).unwrap();
    prepare.integration_tip
}

#[tokio::test]
async fn status_transition_table_preserves_success_age_reuses_source_and_resets_after_eviction() {
    let f = Fixture::new();
    let hold = f.scratch.join("hold-fill");
    let fail = f.scratch.join("fail-fill");
    let command = format!("while [ -f '{}' ]; do sleep 0.01; done; if [ -f '{}' ]; then exit 1; fi; printf warm > \"$CARGO_TARGET_DIR/artifact\"", hold.display(), fail.display());
    let a_manifest = manifest(&command);
    let a = declare(&f, &a_manifest);
    let pressure = Arc::new(AtomicBool::new(false));
    let mut config = f.config();
    config.base_commands.clear();
    config.workspace_roots.clear();
    config.free_space_floor_bytes = 10;
    let cache_entry = config.sccache_dir().join("entry");
    config.free_space_probe = Arc::new({
        let pressure = pressure.clone();
        let cache_entry = cache_entry.clone();
        move |_| {
            Ok(if pressure.load(Ordering::SeqCst) && cache_entry.exists() {
                7
            } else {
                200
            })
        }
    });
    let runner = Runner::open(config).unwrap();
    assert_eq!(prepare_tip(&f, &runner), a);
    base_done(&runner).await;
    assert_status(&runner, Some(&a), RebuildResult::Ok, Some(0));
    let finished_a = runner.core.inner.lock().unwrap().catalog.repositories[&f.root].finished_ms;
    f.clock.fetch_add(10_000, Ordering::SeqCst);
    fs::write(&hold, "hold").unwrap();
    fs::write(&fail, "fail").unwrap();
    let b = declare(&f, &format!("{a_manifest}\n# new descriptor\n"));
    assert_eq!(prepare_tip(&f, &runner), b);
    assert_status(&runner, Some(&a), RebuildResult::Building, Some(10));
    fs::remove_file(&hold).unwrap();
    base_done(&runner).await;
    assert_status(&runner, Some(&a), RebuildResult::Failed, Some(10));
    assert_eq!(
        runner.core.inner.lock().unwrap().catalog.repositories[&f.root].finished_ms,
        finished_a
    );
    fs::remove_file(&fail).unwrap();
    f.clock.fetch_add(5_000, Ordering::SeqCst);
    fs::write(f.main.join("Cargo.toml"), &a_manifest).unwrap();
    let c = commit(&f.main, "source-only reuse\n").0;
    let writers = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| job.base)
        .count();
    assert_eq!(prepare_tip(&f, &runner), c);
    assert_status(&runner, Some(&c), RebuildResult::Ok, Some(15));
    assert_eq!(
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|job| job.base)
            .count(),
        writers
    );
    fs::create_dir_all(cache_entry.parent().unwrap()).unwrap();
    fs::write(&cache_entry, "evicted cache entry").unwrap();
    fs::write(&hold, "hold").unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    until(|| status(&runner).last_rebuild_result == Some(RebuildResult::Building)).await;
    assert_status(&runner, Some(&c), RebuildResult::Building, Some(15));
    f.clock.fetch_add(7_000, Ordering::SeqCst);
    fs::remove_file(&hold).unwrap();
    base_done(&runner).await;
    assert_status(&runner, Some(&c), RebuildResult::Ok, Some(0));
    f.clock.fetch_add(3_000, Ordering::SeqCst);
    fs::write(&cache_entry, "evict again").unwrap();
    fs::write(&hold, "hold").unwrap();
    fs::write(&fail, "fail").unwrap();
    assert!(runner.ensure_disk_space().unwrap().is_none());
    until(|| status(&runner).last_rebuild_result == Some(RebuildResult::Building)).await;
    assert_status(&runner, Some(&c), RebuildResult::Building, Some(3));
    fs::remove_file(&hold).unwrap();
    base_done(&runner).await;
    assert_status(&runner, Some(&c), RebuildResult::Failed, Some(3));
    assert!(!runner.config().state_dir().join("generations").exists());
    let target = Path::new(&f.key).join("target");
    fs::write(target.join("retained-output"), "keep").unwrap();
    prepare_tip(&f, &runner);
    assert_eq!(fs::read(target.join("retained-output")).unwrap(), b"keep");
    let (run, _) = f.run("true");
    let mut fresh = prepare_for(&run);
    fresh.workspace_key = text(&f.scratch.join("remote/worktrees/fresh-target")).into();
    runner.prepare(&fresh, true).unwrap();
    assert_eq!(
        fs::read_dir(Path::new(&fresh.workspace_key).join("target"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn status_transition_table_no_dependencies_and_skipped_fill() {
    let f = Fixture::new();
    let mut config = f.config();
    config.base_commands.clear();
    let runner = Runner::open(config).unwrap();
    let tip = prepare_tip(&f, &runner);
    assert_status(&runner, Some(&tip), RebuildResult::Ok, Some(0));
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .any(|job| job.base));
    let empty_tip = declare(&f, "[package]\nname='no-dependencies'\nversion='0.1.0'\n");
    assert!(
        crate::writer::fill::dependency_free(&f.main, &empty_tip, None).unwrap(),
        "fixture graph has dependencies"
    );
    assert_eq!(prepare_tip(&f, &runner), empty_tip);
    assert!(
        crate::writer::fill::dependency_free(&runner.core.trees.mirror(&f.root), &empty_tip, None,)
            .unwrap(),
        "mirror graph has dependencies"
    );
    assert_status(&runner, Some(&empty_tip), RebuildResult::Ok, Some(0));
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .any(|job| job.base));
    fs::create_dir_all(f.main.join("local-dep")).unwrap();
    fs::write(
        f.main.join("local-dep/Cargo.toml"),
        "[package]\nname='local-dep'\nversion='0.1.0'\n",
    )
    .unwrap();
    let local_tip = declare(&f, "[package]\nname='no-dependencies'\nversion='0.1.0'\n[dependencies]\nlocal-dep = { path = 'local-dep' }\n");
    assert_eq!(prepare_tip(&f, &runner), local_tip);
    assert_status(&runner, Some(&local_tip), RebuildResult::Ok, Some(0));
    let tip = local_tip;
    let unsupported_tip = declare(&f, "[package]\nname='unsupported'\nversion='0.1.0'\n[dependencies]\nlocal-dep = { path = '../outside' }\n");
    assert!(!crate::writer::fill::dependency_free(&f.main, &unsupported_tip, None).unwrap());
    f.clock.fetch_add(12_000, Ordering::SeqCst);
    let mut inner = runner.core.inner.lock().unwrap();
    runner
        .queue_base(&mut inner, &f.root, &"0".repeat(40), &f.key, &[], &[])
        .unwrap();
    drop(inner);
    assert_status(&runner, Some(&tip), RebuildResult::Failed, Some(12));
    fs::write(f.main.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(f.main.join("Cargo.lock"), format!("version=4\n[[package]]\nname='private'\nversion='0.1.0'\nsource='git+https://private.invalid/repo?rev={}#{}'\n", "1".repeat(40), "1".repeat(40))).unwrap();
    commit(&f.main, "unavailable dependency\n");
    prepare_tip(&f, &runner);
    assert_status(&runner, Some(&tip), RebuildResult::Failed, Some(12));
}
