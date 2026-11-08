use super::*;

#[test]
fn writer_job_reports_source_protection_was_not_applied() {
    let f = Fixture::new();
    let (run, _) = f.run("true");
    let mut job = Job::new(run.job_id, Some(run), 0, 0, 1);
    job.base = true;
    job.started_ms = Some(1);
    job.terminal(Outcome::Exit { code: 0 }, 2);
    assert_eq!(job.terminal.unwrap().source_read_only(), Some(false));
}

#[test]
fn acknowledged_protection_survives_into_terminal_result() {
    let f = Fixture::new();
    let (run, _) = f.run("true");
    let mut job = Job::new(run.job_id, Some(run), 0, 0, 1);
    job.source_read_only = true;
    job.started_ms = Some(1);
    job.terminal(Outcome::Exit { code: 0 }, 2);
    assert_eq!(job.terminal.unwrap().source_read_only(), Some(true));
}

#[test]
fn refused_before_start_omits_source_protection_even_after_sealing() {
    let f = Fixture::new();
    let (run, _) = f.run("true");
    for started_ms in [None, Some(1)] {
        let mut job = Job::new(run.job_id, Some(run.clone()), 0, 0, 1);
        job.source_read_only = true;
        job.started_ms = started_ms;
        job.terminal(
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::WorkspaceSetupFailed,
            },
            2,
        );
        let terminal = job.terminal.unwrap();
        assert_eq!(terminal.source_read_only(), None);
        assert!(serde_json::to_value(terminal)
            .unwrap()
            .get("source_read_only")
            .is_none());
    }
}

#[test]
fn pending_seal_fences_reuse_discard_and_record_expiry() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (run, _) = f.run("true");
    assert_run_accepted(&runner, &run);
    fs::create_dir_all(&run.workspace_key).unwrap();
    let path = crate::source_seal::record_path(&runner.config().state_dir(), run.job_id);
    // An empty handle list exercises durable ownership without changing any
    // inode flags or substituting the privileged kernel operations.
    fs::write(&path, serde_json::to_vec(&serde_json::json!({
        "plan": { "unit": Uuid::now_v7(), "workspace": run.workspace_key, "caller_workspace": run.workspace_key, "files": [] },
        "handles": []
    })).unwrap()).unwrap();
    runner
        .config()
        .source_seals
        .load(&runner.config().state_dir())
        .unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .jobs
            .get_mut(&run.job_id)
            .unwrap()
            .terminal(Outcome::Exit { code: 0 }, runner.now());
        assert!(Runner::occupied(&inner, &run.workspace_key));
        assert!(!runner.deleting(&inner, paths::host_str(&run.workspace_key), &[]));
    }
    assert!(runner.discard(Path::new(&run.workspace_key)).is_err());
    assert!(runner
        .discard(Path::new(&run.workspace_key).parent().unwrap())
        .is_err());
    f.clock.fetch_add(RETENTION_MS + 1, Ordering::SeqCst);
    runner.maintain_state(&mut Vec::new()).unwrap();
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .contains_key(&run.job_id));
    assert!(path.exists());
    f.clock.fetch_sub(RETENTION_MS + 1, Ordering::SeqCst);
    let (same, _) = f.run("true");
    assert!(matches!(
        runner.accept(same.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let now = runner.now();
        let job = inner.jobs.get_mut(&same.job_id).unwrap();
        job.state = JobState::Queued;
        job.queued_ms = Some(now);
        let index = DispatchIndex::build(&inner);
        assert!(index.occupied(&same.workspace_key));
        runner.update_queue_wait_clocks(&inner, now, &index);
        assert_eq!(inner.jobs[&same.job_id].queue_wait_ms(now + 5_000), 0);
        inner.sibling_fences = crate::sibling_snapshots::SiblingFences::Legacy;
        let siblings = vec![SiblingSnapshot {
            canonical_path: run.workspace_key.clone(),
            commit: run.snapshot_commit.clone(),
            tree_hash: run.tree_hash.clone(),
            base_commit: None,
        }];
        assert!(!Runner::siblings_blocked(&inner, &siblings));
        assert!(!DispatchIndex::build(&inner)
            .siblings_blocked(&siblings, &inner.catalog.sibling_commits));
    }
    let (mut snapshot_user, _) = f.run("true");
    snapshot_user.workspace_key = format!("{}-snapshot-user", f.key);
    snapshot_user.cwd = snapshot_user.workspace_key.clone();
    snapshot_user.repository_root = Path::new(&f.key)
        .with_file_name("snapshot-repository")
        .to_string_lossy()
        .into_owned();
    snapshot_user.bundles[0].repository = snapshot_user.repository_root.clone();
    snapshot_user.siblings[0].canonical_path = run.workspace_key.clone();
    assert_run_accepted(&runner, &snapshot_user);
}

fn isolated_config(f: &Fixture) -> Config {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "operator test requires root with CAP_LINUX_IMMUTABLE"
    );
    let mut config = f.config();
    config.isolate_jobs = true;
    config.disable_warm_builds = true;
    config
}

fn flags(path: &Path) -> libc::c_int {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path).unwrap();
    let mut flags: libc::c_int = 0;
    assert_eq!(
        unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) },
        0
    );
    flags
}

fn assert_clear(path: &Path) {
    assert_eq!(
        flags(path) & 0x10,
        0,
        "immutable flag remains on {}",
        path.display()
    );
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Bun installs a `file:` dependency by hard-linking its tracked files into
/// `node_modules` after the checkout was transferred. Sealing must give the
/// tracked path its own inode rather than refuse the job or freeze the alias.
#[tokio::test]
#[ignore = "requires root with CAP_LINUX_IMMUTABLE, file handles and a real motor-job system unit"]
async fn isolated_source_seal_survives_bun_file_dependency_hard_links() {
    let f = Fixture::new();
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("test-fixtures/javascript/bunworkspace"),
        &f.main,
    );
    commit(&f.main, "first\n");
    let mut config = isolated_config(&f);
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let source = "packages/member/vendor/memberdep/index.js";
    let (run, bytes) = f.run(&format!(
        r#"
        set -e
        test "$(stat -c %h {source})" = 1
        if echo edited >> {source}; then exit 11; fi
        find . -path '*node_modules*' -name index.js -path '*memberdep*' > target/aliases
        while read -r alias; do echo edited >> "$alias"; done < target/aliases
    "#
    ));
    f.submit(&runner, &run, &bytes);
    runner.tick().unwrap();
    let terminal = terminal(&runner, run.job_id).await;
    assert_eq!(
        terminal.outcome,
        Outcome::Exit { code: 0 },
        "{:?}; {}",
        terminal.refusal_detail,
        job_output(&runner, run.job_id)
    );
    assert_eq!(terminal.source_read_only(), Some(true));
    let aliases = fs::read_to_string(Path::new(&f.key).join("target/aliases")).unwrap();
    assert!(
        !aliases.trim().is_empty(),
        "preparation installed no node_modules copy of memberdep, so this test proves nothing"
    );
    assert_clear(&Path::new(&f.key).join(source));
}

#[tokio::test]
#[ignore = "requires root with CAP_LINUX_IMMUTABLE, file handles and a real motor-job system unit"]
async fn isolated_source_seal_refuses_edits_and_workspace_is_reusable() {
    let f = Fixture::new();
    for name in [
        "sed-file",
        "write-file",
        "rm-file",
        "rename-file",
        "dir/source",
        "cross/source",
        "pkg/returned",
    ] {
        let path = f.main.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "original\n").unwrap();
    }
    commit(&f.main, "first\n");
    let mut config = isolated_config(&f);
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (mut run, bytes) = f.run(
        r#"
        set -e
        if sed -i 's/original/edited/' sed-file; then exit 11; fi
        if echo edited > write-file; then exit 12; fi
        if rm rm-file; then exit 13; fi
        echo replacement > replacement
        if mv -f replacement rename-file; then exit 14; fi
        echo new > untracked
        echo output > target/output
        echo returned > pkg/returned
        mv dir dir-moved
        mkdir dir
        echo replacement > dir/source
        if mv cross target/copied; then exit 15; fi
        echo ready > ready
        while [ ! -f release ]; do sleep 0.01; done
    "#,
    );
    run.return_paths = vec![Path::new(&f.key)
        .join("pkg/returned")
        .to_string_lossy()
        .into_owned()];
    f.submit(&runner, &run, &bytes);
    runner.tick().unwrap();
    until_with_diagnostics(
        || {
            Path::new(&f.key).join("ready").exists()
                || runner.core.inner.lock().unwrap().jobs[&run.job_id]
                    .terminal
                    .is_some()
        },
        || runner_diagnostics(&runner),
    )
    .await;
    let early_terminal = runner.core.inner.lock().unwrap().jobs[&run.job_id]
        .terminal
        .clone();
    assert!(
        early_terminal.is_none(),
        "command ended before edit checks finished (exit 11 means sed changed source): {early_terminal:?}; {}",
        job_output(&runner, run.job_id)
    );
    let unit = runner.core.inner.lock().unwrap().jobs[&run.job_id]
        .process
        .unwrap()
        .unit
        .unwrap();
    assert_eq!(runner.config().source_seals.unit(run.job_id), Some(unit));
    runner.maintenance().unwrap();
    assert!(crate::source_seal::record_path(&runner.config().state_dir(), run.job_id).exists());
    assert_ne!(flags(&Path::new(&f.key).join("sed-file")) & 0x10, 0);
    assert_ne!(flags(&Path::new(&f.key).join("dir-moved/source")) & 0x10, 0);
    // target is a separate bind mount. mv copies across that boundary, but
    // cannot remove the sealed original, and its new copy has no seal.
    assert_ne!(flags(&Path::new(&f.key).join("cross/source")) & 0x10, 0);
    assert_clear(&Path::new(&f.key).join("target/copied/source"));
    let (queued, queued_bytes) =
        f.run("test \"$(cat source)\" = first && echo queued > target/queued");
    f.submit(&runner, &queued, &queued_bytes);
    assert_eq!(runner.job_status(queued.job_id).unwrap().state, "queued");
    f.clock.fetch_add(5_000, Ordering::SeqCst);
    runner.tick().unwrap();
    assert_eq!(
        runner.job_status(queued.job_id).unwrap().queue_wait_ms,
        Some(0)
    );
    assert!(!Path::new(&f.key).join("target/queued").exists());
    fs::write(Path::new(&f.key).join("release"), b"release").unwrap();
    let terminal = terminal(&runner, run.job_id).await;
    assert_eq!(
        terminal.outcome,
        Outcome::Exit { code: 0 },
        "{}",
        job_output(&runner, run.job_id)
    );
    assert!(runner.core.inner.lock().unwrap().jobs[&run.job_id].source_read_only);
    assert_eq!(terminal.source_read_only(), Some(true));
    for name in [
        "sed-file",
        "write-file",
        "rm-file",
        "rename-file",
        "dir-moved/source",
        "cross/source",
        "target/copied/source",
    ] {
        let path = Path::new(&f.key).join(name);
        assert_eq!(fs::read_to_string(&path).unwrap(), "original\n");
        assert_clear(&path);
    }
    for (name, contents) in [
        ("untracked", "new\n"),
        ("target/output", "output\n"),
        ("pkg/returned", "returned\n"),
        ("dir/source", "replacement\n"),
    ] {
        assert_eq!(
            fs::read_to_string(Path::new(&f.key).join(name)).unwrap(),
            contents
        );
    }
    assert!(!crate::source_seal::record_path(&runner.config().state_dir(), run.job_id).exists());
    runner.tick().unwrap();
    assert_eq!(
        super::terminal(&runner, queued.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    fs::hard_link(
        Path::new(&f.key).join("source"),
        Path::new(&f.key).join("target/alias"),
    )
    .unwrap();
    let (second, bytes) = f.run("test \"$(cat source)\" = first && echo alias > target/alias && test \"$(cat source)\" = first && echo reused > target/second");
    f.submit(&runner, &second, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        super::terminal(&runner, second.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        fs::read_to_string(Path::new(&f.key).join("target/second")).unwrap(),
        "reused\n"
    );
    use std::os::unix::fs::MetadataExt;
    assert_ne!(
        fs::metadata(Path::new(&f.key).join("source"))
            .unwrap()
            .ino(),
        fs::metadata(Path::new(&f.key).join("target/alias"))
            .unwrap()
            .ino()
    );
}

#[tokio::test]
#[ignore = "requires root with CAP_LINUX_IMMUTABLE, file handles and a real motor-job system unit"]
async fn isolated_startup_unseals_leftover_record_for_dead_unit() {
    let f = Fixture::new();
    let mut config = isolated_config(&f);
    config.manual_tick = true;
    let runner = Runner::open(config.clone()).unwrap();
    let (run, bytes) = f.run("true");
    f.submit(&runner, &run, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, run.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        platform::unit_alive(runner.core.inner.lock().unwrap().jobs[&run.job_id].process).unwrap(),
        Some(false)
    );
    let tree = workspace::snapshot_tree(
        &runner.core.trees.mirror(&run.repository_root),
        &run.tree_hash,
    )
    .unwrap();
    let plan = crate::source_seal::plan(&config, &run, &[], &tree).unwrap();
    config
        .source_seals
        .seal(&config.state_dir(), run.job_id, plan)
        .unwrap();
    assert!(crate::source_seal::record_path(&config.state_dir(), run.job_id).exists());
    // Simulate a daemon crash after persisting a seal for an already dead unit.
    // The next daemon must recover from disk, not from the old runtime map.
    drop(runner);
    let mut fresh = isolated_config(&f);
    fresh.manual_tick = true;
    let reopened = reopen(fresh);
    assert!(!crate::source_seal::record_path(&reopened.config().state_dir(), run.job_id).exists());
    assert_clear(&Path::new(&f.key).join("source"));
    assert!(reopened.config().source_seals.roots().is_empty());
}

#[test]
#[ignore = "requires root and the motor-job account; probes a supported root then an unsupported filesystem"]
fn isolated_startup_refuses_unsupported_source_filesystem() {
    let f = Fixture::new();
    let mut config = isolated_config(&f);
    config.owned_workspace_roots.push("/proc".into());
    let error = Runner::open(config)
        .err()
        .expect("startup must refuse /proc source sealing");
    let text = error.to_string();
    assert!(
        text.contains("/proc") && text.contains("source read-only startup probe"),
        "{text}"
    );
}
