use super::*;

pub(super) fn next_boot(config: &Config, forked: bool) {
    // Wait for every monitor and teardown owner to release Core before
    // modelling the tenant agent's per-boot history reset.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.state_dir().join("daemon.lock"))
        .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < end => {
                std::thread::park_timeout(Duration::from_millis(5))
            }
            Err(error) => panic!("previous boot did not release daemon.lock: {error}"),
        }
    }
    fs::remove_dir_all(config.state_dir().join("jobs")).unwrap();
    fs::create_dir(config.state_dir().join("jobs")).unwrap();
    let marker = config.fixture_workspace_forked_marker.as_ref().unwrap();
    if forked {
        fs::write(marker, "1").unwrap();
    } else if marker.exists() {
        fs::remove_file(marker).unwrap();
    }
}

fn adoption_request(f: &Fixture) -> Run {
    let (mut request, _) = f.run("true");
    request.siblings.clear();
    request.workspace_key = "/home/new".into();
    request.cwd = request.workspace_key.clone();
    request
}

fn entry(request: &Run) -> Workspace {
    Workspace {
        repository: paths::request(request)
            .caller_repository_root
            .text()
            .clone(),
        snapshot: request.snapshot_commit.clone(),
        used_ms: 123,
        last_job_id: Some(Uuid::now_v7()),
        ..Workspace::default()
    }
}

#[test]
fn adoption_rekeys_whole_entry_persists_and_preserves_snapshot_pins() {
    for (source, destination) in [
        ("/home/r", "/home/r/.worktrees/y"),
        ("/home/r/.worktrees/y", "/home/r"),
    ] {
        let f = Fixture::new();
        let roots = f.scratch.join("roots");
        let mut request = adoption_request(&f);
        request.workspace_key = destination.into();
        let mut config = f.config();
        config.manual_tick = true;
        config
            .owned_workspace_roots
            .extend([PathBuf::from("/home"), PathBuf::from("/Users")]);
        let runner = Runner::open(config).unwrap();
        let checkout = crate::fork_adoption::checkout_path(&roots, source).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("artifact"), b"warm").unwrap();
        let mirror = runner.core.trees.mirror(&f.root);
        fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        git(
            &f.main,
            &["clone", "--mirror", text(&f.main), text(&mirror)],
        );
        runner
            .core
            .trees
            .pin(&f.root, &request.snapshot_commit, "", &[])
            .unwrap();
        let expected = entry(&request);
        {
            let mut inner = runner.core.inner.lock().unwrap();
            inner
                .catalog
                .workspaces
                .insert(source.into(), expected.clone());
            let mut other = expected.clone();
            other.repository = "/other/root".into();
            inner
                .catalog
                .workspaces
                .insert("/home/unrelated".into(), other);
            inner.build_swept_ms.insert(source.into(), 1);
            inner.build_guard_pending.insert(source.into());
            inner.build_retry_after.insert(source.into(), [1, 2, 3]);
            runner.adopt_checkout(&mut inner, &request, &roots).unwrap();
            assert!(!inner.catalog.workspaces.contains_key(source));
            let adopted = &inner.catalog.workspaces[destination];
            assert_eq!(adopted.snapshot, expected.snapshot);
            assert_eq!(adopted.used_ms, expected.used_ms);
            assert_eq!(adopted.repository, expected.repository);
            assert_eq!(adopted.last_job_id, None);
            assert!(inner.catalog.workspaces.contains_key("/home/unrelated"));
            assert!(!inner.build_swept_ms.contains_key(source));
            assert!(!inner.build_guard_pending.contains(source));
            assert!(!inner.build_retry_after.contains_key(source));
        }
        let saved = paths::catalog::Split::read(&runner.core.trees.state)
            .unwrap()
            .loaded();
        assert!(saved.workspaces.contains_key(destination));
        assert!(!saved.workspaces.contains_key(source));
        runner.prune_refs().unwrap();
        assert_eq!(
            git(
                &mirror,
                &[
                    "rev-parse",
                    &format!("refs/snapshots/{}", expected.snapshot)
                ]
            ),
            expected.snapshot
        );
        assert_eq!(
            fs::read(
                crate::fork_adoption::checkout_path(&roots, destination)
                    .unwrap()
                    .join("artifact")
            )
            .unwrap(),
            b"warm"
        );
    }
}

fn forked_config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.manual_tick = true;
    config.per_job = true;
    config.tenant_vm = true;
    let marker = f.scratch.join("forked");
    fs::write(&marker, "1").unwrap();
    config.fixture_workspace_forked_marker = Some(marker);
    config.fixture_adoption_roots = Some(f.scratch.join("remote"));
    config
}

#[tokio::test]
async fn fresh_fork_status_clears_all_donor_ids_and_restart_preserves_new_work() {
    let f = Fixture::new();
    let config = forked_config(&f);
    let (request, bytes) = f.run("true");
    let mut catalog = Catalog::default();
    for key in [
        f.key.clone(),
        text(&f.scratch.join("remote/worktrees/other")).into(),
    ] {
        fs::create_dir_all(&key).unwrap();
        catalog.workspaces.insert(
            key,
            Workspace {
                repository: f.root.clone(),
                used_ms: (config.clock)(),
                last_job_id: Some(Uuid::now_v7()),
                ..Workspace::default()
            },
        );
    }
    atomic_json(&config.state_dir().join("catalog.json"), &catalog).unwrap();
    let runner = Runner::open(config.clone()).unwrap();
    assert!(runner
        .runner_status()
        .health
        .unwrap()
        .held_workspaces
        .is_empty());
    let saved = paths::catalog::Split::read(&config.state_dir())
        .unwrap()
        .loaded();
    assert_eq!(saved.workspaces.len(), 2);
    assert!(saved
        .workspaces
        .values()
        .all(|workspace| workspace.last_job_id.is_none()));
    // Fixture::run includes a sibling repository in the Run, which forbids
    // adoption regardless of the process UID.
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    until(|| runner.core.inner.lock().unwrap().finalizing.is_empty()).await;
    let expected = runner.runner_status().health.unwrap().held_workspaces;
    assert!(expected
        .iter()
        .any(|workspace| workspace.job_id == request.job_id));
    drop(runner);
    let runner = reopen(config);
    assert_eq!(
        runner.runner_status().health.unwrap().held_workspaces,
        expected
    );
}

#[test]
fn donor_authorization_without_fixture_relocation_rejects_bind_and_disk_roots() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    assert!(config.fixture_adoption_roots.is_none());
    let runner = Runner::open(config).unwrap();
    for donor in [
        "/home",
        "/Users",
        "/tenant/roots/home/donor",
        "/home/not-owned-by-this-runner",
    ] {
        assert!(
            runner
                .adoption_path(Path::new("/tenant/roots"), donor)
                .is_err(),
            "{donor}"
        );
    }
    for donor in ["/home", "/Users", "/tenant/roots/home/donor"] {
        assert!(
            crate::fork_adoption::checkout_path(Path::new("/tenant/roots"), donor).is_err(),
            "{donor}"
        );
    }
}

#[tokio::test]
async fn refused_nested_adoptions_retire_inherited_trees_and_run_cold() {
    for donor_inside in [false, true] {
        let f = Fixture::new();
        let config = forked_config(&f);
        let roots = config.fixture_adoption_roots.clone().unwrap();
        let outer = roots.join("worktrees/r");
        let nested = outer.join(".worktrees/y");
        let (source, destination) = if donor_inside {
            (&nested, &outer)
        } else {
            (&outer, &nested)
        };
        fs::create_dir_all(source).unwrap();
        fs::write(source.join("inherited-artifact"), b"warm").unwrap();
        let runner = Runner::open(config).unwrap();
        let (mut request, mut bytes) = f.run("test ! -e inherited-artifact");
        let donor_siblings = request.siblings.clone();
        request.workspace_key = text(destination).into();
        request.cwd = request.workspace_key.clone();
        request.siblings.clear();
        request.bundles.truncate(1);
        bytes.truncate(1);
        {
            let mut inner = runner.core.inner.lock().unwrap();
            let mut donor = entry(&request);
            donor.siblings = donor_siblings;
            inner.catalog.workspaces.insert(text(source).into(), donor);
            let other = roots.join("worktrees/unrelated");
            fs::create_dir_all(&other).unwrap();
            fs::write(other.join("inherited"), b"other-root").unwrap();
            inner.catalog.workspaces.insert(
                text(&other).into(),
                Workspace {
                    repository: "/other/root".into(),
                    ..Workspace::default()
                },
            );
            assert!(crate::fork_adoption::donor(&inner.catalog, &request).is_err());
            runner
                .adopt_or_cold_checkout(&mut inner, &request, &roots)
                .unwrap();
            assert!(inner.catalog.workspaces.is_empty());
            assert!(!other.exists());
            assert!(!destination.exists());
        }
        f.submit(&runner, &request, &bytes);
        runner.tick().unwrap();
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        assert!(!destination.join("inherited-artifact").exists());
        assert!(!runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .contains_key(text(source)));
    }
}

#[test]
fn cold_fallback_rejects_symlinked_destination_before_deleting_any_donor() {
    let f = Fixture::new();
    let config = forked_config(&f);
    let roots = config.fixture_adoption_roots.clone().unwrap();
    let runner = Runner::open(config).unwrap();
    let (mut request, _) = f.run("true");
    let source = roots.join("worktrees/donor");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("artifact"), b"keep").unwrap();
    let destination = roots.join("worktrees/link");
    std::os::unix::fs::symlink(&source, &destination).unwrap();
    request.workspace_key = text(&destination).into();
    request.cwd = request.workspace_key.clone();
    let mut inner = runner.core.inner.lock().unwrap();
    inner
        .catalog
        .workspaces
        .insert(text(&source).into(), entry(&request));
    assert!(runner
        .cold_forked_checkout(&mut inner, &request, &roots)
        .is_err());
    assert!(inner.catalog.workspaces.contains_key(text(&source)));
    assert_eq!(fs::read(source.join("artifact")).unwrap(), b"keep");
    assert!(destination
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn two_matching_checkouts_refuse() {
    let f = Fixture::new();
    let request = adoption_request(&f);
    let mut catalog = Catalog::default();
    catalog.workspaces.insert("/home/a".into(), entry(&request));
    catalog.workspaces.insert("/home/b".into(), entry(&request));
    assert!(crate::fork_adoption::donor(&catalog, &request)
        .unwrap_err()
        .to_string()
        .contains("exactly one"));
}

#[test]
fn another_repository_root_refuses() {
    let f = Fixture::new();
    let request = adoption_request(&f);
    let mut catalog = Catalog::default();
    let mut other = entry(&request);
    other.repository = "/other/root".into();
    catalog.workspaces.insert("/home/a".into(), other);
    assert!(crate::fork_adoption::donor(&catalog, &request).is_err());
}

#[test]
fn donor_or_request_siblings_refuse() {
    let f = Fixture::new();
    let (with_siblings, _) = f.run("true");
    for donor_siblings in [false, true] {
        let mut request = adoption_request(&f);
        let mut workspace = entry(&request);
        if donor_siblings {
            workspace.siblings = with_siblings.siblings.clone();
        } else {
            request.siblings = with_siblings.siblings.clone();
        }
        let mut catalog = Catalog::default();
        catalog.workspaces.insert("/home/a".into(), workspace);
        assert!(crate::fork_adoption::donor(&catalog, &request)
            .unwrap_err()
            .to_string()
            .contains("siblings"));
    }
}

#[test]
fn base_workspace_or_existing_caller_entry_refuses() {
    let f = Fixture::new();
    let request = adoption_request(&f);
    for key in ["base:root", request.workspace_key.as_str()] {
        let mut catalog = Catalog::default();
        catalog.workspaces.insert(key.into(), entry(&request));
        assert!(crate::fork_adoption::donor(&catalog, &request).is_err());
    }
}

#[test]
fn marker_absent_never_adopts_even_with_a_matching_donor() {
    let f = Fixture::new();
    let request = adoption_request(&f);
    let mut config = f.config();
    config.manual_tick = true;
    config.per_job = true;
    config.tenant_vm = true;
    config.fixture_workspace_forked_marker = Some(f.scratch.join("absent-marker"));
    let runner = Runner::open(config).unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .insert("/home/donor".into(), entry(&request));
    let output = capture_runner_logs(|| {
        runner.accept(request.clone()).unwrap();
    });
    let inner = runner.core.inner.lock().unwrap();
    assert_eq!(inner.workspace_forked, Some(false));
    assert!(inner.catalog.workspaces.contains_key("/home/donor"));
    assert!(!inner
        .catalog
        .workspaces
        .contains_key(&request.workspace_key));
    assert!(output.contains("forked marker absent"), "{output}");
}

#[tokio::test]
async fn crash_after_move_before_rekey_next_job_sets_up_complete_destination() {
    let f = Fixture::new();
    let roots = f.scratch.join("remote");
    let source = Path::new(&f.key);
    let destination = roots.join("worktrees/y");
    let mut config = f.config();
    config.manual_tick = true;
    config.per_job = true;
    config.tenant_vm = true;
    config.fixture_workspace_forked_marker = Some(f.scratch.join("forked"));
    config.fixture_adoption_roots = Some(roots.clone());
    let runner = Runner::open(config.clone()).unwrap();
    let (mut request, mut bytes) = f.run("true");
    request.siblings.clear();
    request.bundles.truncate(1);
    bytes.truncate(1);
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    until(|| runner.core.inner.lock().unwrap().finalizing.is_empty()).await;
    drop(runner);
    next_boot(&config, true);
    crate::fork_adoption::move_checkout(&roots, source, &destination, || Ok(()), || Ok(()))
        .unwrap();
    let runner = reopen(config);
    {
        let mut inner = runner.core.inner.lock().unwrap();
        assert!(inner.catalog.workspaces.contains_key(&f.key));
        assert!(runner
            .adopt_checkout(
                &mut inner,
                &Run {
                    workspace_key: text(&destination).into(),
                    ..request.clone()
                },
                &roots
            )
            .is_err());
    }
    request.job_id = Uuid::now_v7();
    request.workspace_key = text(&destination).into();
    request.cwd = request.workspace_key.clone();
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&request.workspace_key));
    runner.prune_refs().unwrap();
    assert!(!source.exists());
}
