use super::*;

async fn prepare_workspace(f: &Fixture, runner: &Runner, workspace: &Path) {
    let (mut request, bytes) = f.run("true");
    request.workspace_key = text(workspace).to_owned();
    request.cwd = request.workspace_key.clone();
    f.submit(runner, &request, &bytes);

    let result = terminal(runner, request.job_id).await;
    assert!(matches!(result.outcome, Outcome::Exit { code: 0 }));
    assert!(workspace.is_dir());
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&request.workspace_key));
}

async fn refusal_detail(runner: &Runner, request: &Run) -> String {
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    let terminal = terminal(runner, request.job_id).await;
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    ));
    serde_json::to_string(&terminal).unwrap()
}

fn nested_repository_request(f: &Fixture, outer: &Path) -> (Run, Vec<Vec<u8>>) {
    let (mut request, mut bytes) = f.run("true");
    let repository = outer.join("nested-repository");
    request.workspace_key = text(&f.scratch.join("remote/worktrees/second")).to_owned();
    request.cwd = request.workspace_key.clone();
    request.repository_root = text(&repository).to_owned();
    request.bundles[0].repository = request.repository_root.clone();
    request.siblings.clear();
    request.bundles.truncate(1);
    bytes.truncate(1);
    (request, bytes)
}

#[tokio::test]
async fn nested_workspace_is_refused_before_any_workspace_is_created() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let outer = f.scratch.join("remote/worktrees/outer");
    prepare_workspace(&f, &runner, &outer).await;

    let nested = outer.join("nested");
    let (mut request, _) = f.run("true");
    request.workspace_key = text(&nested).to_owned();
    request.cwd = request.workspace_key.clone();

    let detail = refusal_detail(&runner, &request).await;
    // Refusal details pass through the runner's sanitizer, which renames runner
    // state paths; scratch folders can live there, so compare after sanitizing.
    assert!(detail.contains(&runner.safe_refusal_detail(format!(
        "workspace {} is inside runner workspace {}; nested workspaces aren't supported, so move it outside any remote workspace",
        nested.display(),
        outer.display()
    ), std::iter::empty::<String>())), "{detail}");
    assert!(!nested.exists());
}

#[tokio::test]
async fn workspace_with_only_a_string_prefix_of_a_known_workspace_is_admitted() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let outer = f.scratch.join("remote/worktrees/a/b");
    prepare_workspace(&f, &runner, &outer).await;

    let prefix = f.scratch.join("remote/worktrees/a/bc");
    let (mut request, _) = f.run("true");
    request.workspace_key = text(&prefix).to_owned();
    request.cwd = request.workspace_key.clone();

    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(_)
    ));
}

#[tokio::test]
async fn sibling_inside_a_known_workspace_is_refused_before_setup() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let outer = f.scratch.join("remote/worktrees/outer");
    prepare_workspace(&f, &runner, &outer).await;

    let mut sibling = f.run("true").0;
    let sibling_path = outer.join("sibling");
    sibling.workspace_key = text(&f.scratch.join("remote/worktrees/second")).to_owned();
    sibling.cwd = sibling.workspace_key.clone();
    sibling.repository_root = text(&outer.join("repository")).to_owned();
    sibling.siblings[0].canonical_path = text(&sibling_path).to_owned();

    let detail = refusal_detail(&runner, &sibling).await;
    assert!(detail.contains(&runner.safe_refusal_detail(format!(
        "sibling {} is inside runner workspace {}; nested workspaces aren't supported, so move it outside any remote workspace",
        sibling_path.display(),
        outer.display()
    ), std::iter::empty::<String>())), "{detail}");
    assert!(!Path::new(&sibling.workspace_key).exists());
    assert!(!sibling_path.exists());
}

#[tokio::test]
async fn nested_repository_is_refused_before_cache_catalog_registration() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let outer = f.scratch.join("remote/worktrees/outer");
    prepare_workspace(&f, &runner, &outer).await;
    let (request, bytes) = nested_repository_request(&f, &outer);

    let admission = runner.accept(request.clone()).unwrap();
    if matches!(&admission, Admission::Accepted(_)) {
        let upload = f.spool(&runner, &request, &bytes);
        runner.fetch_run(&request, &upload.directory, true).unwrap();
    }
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .contains_key(&request.repository_root));
    assert!(matches!(
        admission,
        Admission::Refused(RefusalReason::WorkspaceSetupFailed)
    ));
    assert!(!Path::new(&request.workspace_key).exists());
}

#[tokio::test]
async fn nested_prepare_is_refused_before_workspace_or_repository_registration() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let outer = f.scratch.join("remote/worktrees/outer");
    prepare_workspace(&f, &runner, &outer).await;
    let (request, _) = nested_repository_request(&f, &outer);
    let prepare = prepare_for(&request);

    let refusal = match runner.admit_prepare(&prepare).unwrap() {
        Err(refusal) => refusal,
        Ok(_guard) => panic!("a repository inside a prepared workspace must be refused"),
    };
    assert!(matches!(
        &refusal.reason,
        RefusalReason::WorkspaceSetupFailed
    ));
    let detail = refusal.detail.unwrap();
    assert!(
        detail.contains(
            &runner.safe_refusal_detail(&request.repository_root, std::iter::empty::<String>())
        ),
        "{detail}"
    );
    assert!(!Path::new(&request.workspace_key).exists());
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .contains_key(&request.repository_root));
}

#[test]
fn maintenance_removes_failed_missing_repository_but_keeps_work_in_progress() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let stale = f.scratch.join("not-created/stale");
    let busy = f.scratch.join("not-created/busy");
    let building = f.scratch.join("not-created/building");
    let waiting = f.scratch.join("not-created/waiting");
    let existing = f.main.clone();
    let filled = f.scratch.join("not-created/filled");

    let stale_record: Repository = serde_json::from_value(serde_json::json!({
        "failure_detail": "checkout workspace: Permission denied (os error 13)",
        "filled_descriptor": null,
        "filled_descriptors": {},
        "recorded_without_writer": [],
        "miss_refills": [],
        "failed_descriptor": "050bbf…",
        "requested_descriptor": "050bbf…",
        "commit": null,
        "finished_ms": null,
        "result": "failed",
        "desired": {
            "tip": "0cdc379…",
            "workspace": text(&stale),
            "siblings": [],
            "git_dependencies": [],
            "command": "cargo nextest run"
        }
    }))
    .unwrap();
    let filled_record = Repository {
        filled_descriptor: Some("completed-fill".into()),
        filled_descriptors: BTreeMap::from([("completed-fill".into(), 1)]),
        ..Repository::default()
    };
    let building_record = Repository {
        requested_descriptor: Some("in-progress".into()),
        result: Some(RebuildResult::Building),
        ..Repository::default()
    };
    let waiting_record = Repository {
        failed_descriptor: Some("previous-fill".into()),
        requested_descriptor: Some("next-fill".into()),
        ..Repository::default()
    };

    let mut active_request = f.run("true").0;
    active_request.repository_root = text(&busy).to_owned();
    let mut active_job = Job::new(
        active_request.job_id,
        Some(active_request),
        f.clock.load(Ordering::SeqCst),
        1,
        2,
    );
    active_job.base = true;

    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .repositories
            .insert(text(&stale).to_owned(), stale_record);
        inner
            .catalog
            .repositories
            .insert(text(&busy).to_owned(), Repository::default());
        inner
            .catalog
            .repositories
            .insert(text(&building).to_owned(), building_record);
        inner
            .catalog
            .repositories
            .insert(text(&waiting).to_owned(), waiting_record);
        inner
            .catalog
            .repositories
            .insert(text(&existing).to_owned(), Repository::default());
        inner
            .catalog
            .repositories
            .insert(text(&filled).to_owned(), filled_record);
        inner.jobs.insert(active_job.id, active_job);
    }

    runner.maintenance().unwrap();

    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.catalog.repositories.contains_key(text(&stale)));
    for repository in [&busy, &building, &waiting, &existing, &filled] {
        assert!(inner.catalog.repositories.contains_key(text(repository)));
    }
}
