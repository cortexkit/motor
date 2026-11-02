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
        assert!(runner.deleting(&inner, paths::host_str(&run.workspace_key), &[]));
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
    // Preserve the real unit ceiling, but do not read the jobs_min_bytes
    // declaration in /etc/motor-host/capacity.json for a source-protection test.
    config.slice_budget_probe = Some(Arc::new(|| {
        Ok((crate::systemd::read_job_memory_budget()?.max, None))
    }));
    config
}

fn flags(path: &Path) -> libc::c_long {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path).unwrap();
    let mut flags: libc::c_long = 0;
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
        "pkg/returned",
    ] {
        let path = f.main.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "original\n").unwrap();
    }
    commit(&f.main, "first\n");
    let runner = Runner::open(isolated_config(&f)).unwrap();
    let (mut run, bytes) = f.run(
        r#"
        if sed -i 's/original/edited/' sed-file; then exit 11; fi
        if echo edited > write-file; then exit 12; fi
        if rm rm-file; then exit 13; fi
        echo replacement > replacement
        if mv -f replacement rename-file; then exit 14; fi
        echo new > untracked
        echo output > target/output
        echo returned > pkg/returned
        mv dir target/moved
        mkdir dir
        echo replacement > dir/source
        echo ready > ready
        while [ ! -f release ]; do sleep 0.01; done
    "#,
    );
    run.return_paths = vec![Path::new(&f.key)
        .join("pkg/returned")
        .to_string_lossy()
        .into_owned()];
    f.submit(&runner, &run, &bytes);
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
    assert_ne!(
        flags(&Path::new(&f.key).join("target/moved/source")) & 0x10,
        0
    );
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
        "target/moved/source",
    ] {
        let path = Path::new(&f.key).join(name);
        assert_eq!(fs::read_to_string(&path).unwrap(), "original\n");
        assert_clear(&path);
    }
    for (name, contents) in [
        ("untracked", "new\n"),
        ("target/output", "output\n"),
        ("pkg/returned", "returned\n"),
    ] {
        assert_eq!(
            fs::read_to_string(Path::new(&f.key).join(name)).unwrap(),
            contents
        );
    }
    assert!(!crate::source_seal::record_path(&runner.config().state_dir(), run.job_id).exists());
    let (second, bytes) = f.run("test \"$(cat source)\" = first && echo reused > target/second");
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        super::terminal(&runner, second.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(
        fs::read_to_string(Path::new(&f.key).join("target/second")).unwrap(),
        "reused\n"
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
