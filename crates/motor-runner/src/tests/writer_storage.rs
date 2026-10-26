use super::*;
use std::os::unix::fs::symlink;

fn prepare_request(run: &Run) -> Prepare {
    Prepare {
        transfer_id: Uuid::now_v7(),
        workspace_key: run.workspace_key.clone(),
        repository_root: run.repository_root.clone(),
        base_commit: run.snapshot_commit.clone(),
        integration_tip: run.integration_tip.clone(),
        siblings: run.siblings.clone(),
        bundles: run.bundles.clone(),
        git_dependencies: run.git_dependencies.clone(),
        workspace_kind: run.workspace_kind,
    }
}

#[test]
fn writer_path_exclusion_blocks_run_and_prepare_before_mutation() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let cache = config.sccache_dir();
    let scratch = config.writer_scratch_dir();
    let runner = Runner::open(config).unwrap();
    let legacy = f.scratch.join("remote/worktrees/.motor-base-e2e-example");
    let container = f.scratch.join("remote/worktrees/container");
    let nested = container.join(format!(
        "nested/.motor-base-e2e-{}",
        workspace::name(&f.root)
    ));
    for path in [&cache, &scratch, &legacy, &nested] {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("sentinel"), "untouched").unwrap();
    }
    let alias = f.scratch.join("remote/worktrees/alias");
    symlink(&cache, &alias).unwrap();
    runner.core.inner.lock().unwrap().catalog.workspaces.insert(
        text(&container.join("nested/workspace")).into(),
        Workspace {
            repository: f.root.clone(),
            ..Workspace::default()
        },
    );
    let (ordinary, _) = f.run("true");
    let mut requests = Vec::new();
    // These workspace paths exercise the same admission check for overlap with
    // writer storage. Symlinks must not conceal the overlap, even when a suffix
    // of the supplied path does not exist yet.
    for path in [
        cache.clone(),
        cache.join("future/child"),
        cache.parent().unwrap().to_path_buf(),
        runner.config().state_dir(),
        scratch.clone(),
        scratch.join("missing"),
        legacy.clone(),
        legacy.join("missing"),
        legacy.parent().unwrap().to_path_buf(),
        container,
        alias.join("future/child"),
    ] {
        let mut request = ordinary.clone();
        request.job_id = Uuid::now_v7();
        request.workspace_key = text(&path).into();
        request.cwd = request.workspace_key.clone();
        request.siblings.clear();
        requests.push(request);
    }
    fs::create_dir_all(&f.key).unwrap();
    symlink(&cache, Path::new(&f.key).join("cwd-alias")).unwrap();
    let mut cwd = ordinary.clone();
    cwd.job_id = Uuid::now_v7();
    cwd.cwd = text(&Path::new(&f.key).join("cwd-alias/missing")).into();
    cwd.siblings.clear();
    requests.push(cwd);
    let sibling_alias = Path::new(&f.canonical);
    fs::create_dir_all(sibling_alias.parent().unwrap()).unwrap();
    symlink(&scratch, sibling_alias).unwrap();
    requests.push(ordinary.clone());

    for request in requests {
        // workspace::validate accepts these absolute workspace/sibling paths,
        // and cwd is within the workspace. Refusal must therefore come from
        // writer-storage exclusion, not the existing lexical path checks.
        workspace::validate(
            &request.workspace_key,
            &request.repository_root,
            &request.siblings,
            false,
        )
        .unwrap();
        assert!(Path::new(&request.cwd).starts_with(&request.workspace_key));
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Refused(RefusalReason::WorkspaceKeyRejected)
        ));
        let prepare = prepare_request(&request);
        // Prepare has no cwd field; the cwd-only case is covered by Run.
        if request.cwd != request.workspace_key {
            continue;
        }
        let refusal = match runner.admit_prepare(&prepare).unwrap() {
            Err(refusal) => refusal,
            Ok(_) => panic!("writer path admitted for Prepare"),
        };
        assert_eq!(refusal.reason, RefusalReason::WorkspaceKeyRejected);
        for error in [
            runner
                .fetch_prepare(&prepare, f.scratch.path())
                .unwrap_err(),
            runner.prepare(&prepare, true).unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("writer-owned storage"),
                "{error}"
            );
        }
    }
    assert!(
        !runner.core.trees.mirror(&f.root).exists(),
        "refusal must precede fetching bundles"
    );
    for path in [&cache, &scratch, &legacy, &nested] {
        assert_eq!(
            fs::read_to_string(path.join("sentinel")).unwrap(),
            "untouched"
        );
    }
}

#[tokio::test]
async fn writer_path_exclusion_leaves_foreground_workspaces_and_other_instance_names_usable() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let runner = Runner::open(config).unwrap();
    let (mut run, bytes) = f.run("printf foreground");
    run.workspace_key = text(&f.scratch.join("remote/worktrees/.motor-base-main-other")).into();
    run.cwd = run.workspace_key.clone();
    f.submit(&runner, &run, &bytes);
    assert_eq!(
        terminal(&runner, run.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .all(|job| !job.base));
}

#[test]
fn terminal_writer_without_checkout_does_not_fence_its_parent_workspace() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, _) = f.run("true");
    request.cwd = text(&Path::new(&f.key).join("nested/.motor-base-e2e-failed")).into();
    let mut job = Job::new(request.job_id, Some(request.clone()), runner.now(), 1, 4);
    job.base = true;
    job.terminal(Outcome::OutcomeUnknown, runner.now());
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .insert(job.id, job.clone());
    assert!(!Path::new(&request.cwd).exists());
    runner
        .validate_writer_paths(
            crate::caller_path::host_str(&f.key),
            Some(&f.key).map(|p| crate::caller_path::host_str(p)),
            &[],
        )
        .unwrap();
    job.terminal = None;
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .insert(job.id, job.clone());
    assert!(runner
        .validate_writer_paths(
            crate::caller_path::host_str(&f.key),
            Some(crate::caller_path::host_str(&f.key)),
            &[]
        )
        .is_err());
    job.terminal(Outcome::OutcomeUnknown, runner.now());
    runner.core.inner.lock().unwrap().jobs.insert(job.id, job);
    fs::create_dir_all(&request.cwd).unwrap();
    assert!(runner
        .validate_writer_paths(
            crate::caller_path::host_str(&f.key),
            Some(crate::caller_path::host_str(&f.key)),
            &[]
        )
        .is_err());
}
