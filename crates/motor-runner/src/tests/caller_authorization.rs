use super::*;

fn mapped_config(f: &Fixture) -> Config {
    let (mut config, _) = super::sibling_snapshots::snapshot_config(f);
    let host = f.scratch.join("callers/home");
    fs::create_dir_all(&host).unwrap();
    config.caller_roots = vec![crate::CallerRoot {
        caller_prefix: "/home".into(),
        host_root: host,
    }];
    config.test_allow_unisolated_caller_roots = true;
    config.manual_tick = true;
    config.disable_warm_builds = true;
    config
}

fn run(f: &Fixture, path: &str) -> Run {
    let (mut request, _) = f.run("true");
    request.workspace_key = path.into();
    request.repository_root = path.into();
    request.cwd = path.into();
    request.siblings.clear();
    request.bundles.clear();
    request
}

fn prepare(request: &Run) -> Prepare {
    Prepare {
        transfer_id: Uuid::now_v7(),
        workspace_key: request.workspace_key.clone(),
        workspace_kind: None,
        repository_root: request.repository_root.clone(),
        base_commit: request.snapshot_commit.clone(),
        integration_tip: request.integration_tip.clone(),
        siblings: request.siblings.clone(),
        bundles: vec![],
        git_dependencies: vec![],
    }
}

fn admission(runner: &Runner, request: &Run) -> Option<(RefusalReason, String)> {
    runner.run_refusal(
        &runner.core.inner.lock().unwrap(),
        request,
        &Default::default(),
    )
}

#[test]
fn mapped_nested_workspaces_fence_catalog_running_starting_admission_and_prepare_in_both_orders() {
    for holder in [
        "catalog",
        "running",
        "starting",
        "admission",
        "prepare",
        "catalog-sibling",
    ] {
        for (first, second) in [
            ("/home/u/proj", "/home/u/proj/.worktrees/x"),
            ("/home/u/proj/.worktrees/x", "/home/u/proj"),
        ] {
            let f = Fixture::new();
            let runner = Runner::open(mapped_config(&f)).unwrap();
            let first_run = run(&f, first);
            {
                let mut inner = runner.core.inner.lock().unwrap();
                match holder {
                    "catalog" => workspace::record_workspace_entry(
                        &mut inner.catalog,
                        first,
                        first,
                        "tip",
                        &[],
                        0,
                    ),
                    "catalog-sibling" => {
                        let sibling = SiblingSnapshot {
                            canonical_path: first.into(),
                            commit: "tip".into(),
                            tree_hash: "tree".into(),
                            base_commit: None,
                        };
                        let identity = f.run("true").0.workspace_key;
                        workspace::record_workspace_entry(
                            &mut inner.catalog,
                            &identity,
                            &identity,
                            "tip",
                            &[sibling],
                            0,
                        );
                    }
                    "running" | "starting" => {
                        let mut job = Job::new(first_run.job_id, Some(first_run.clone()), 0, 0, 2);
                        job.state = JobState::Running;
                        if holder == "starting" {
                            job.terminal(Outcome::Cancelled, 1);
                            inner.starting.insert(job.id);
                        }
                        inner.jobs.insert(job.id, job);
                    }
                    "admission" => {
                        inner.admitting.insert(first_run.job_id, first_run.clone());
                    }
                    "prepare" => {
                        let request = prepare(&first_run);
                        inner.preparing.insert(request.transfer_id, request);
                    }
                    _ => unreachable!(),
                }
            }
            let second_run = run(&f, second);
            let (reason, detail) = admission(&runner, &second_run).unwrap();
            assert_eq!(
                reason,
                RefusalReason::WorkspaceSetupFailed,
                "{holder}: {detail}"
            );
            assert!(
                detail.contains(first) && detail.contains(second),
                "{detail}"
            );
            let refusal = runner
                .admit_prepare(&prepare(&second_run))
                .unwrap()
                .err()
                .unwrap();
            assert_eq!(refusal.reason, RefusalReason::WorkspaceSetupFailed);
            assert!(refusal
                .detail
                .unwrap()
                .contains("nested workspaces aren't supported"));
            assert!(admission(&runner, &run(&f, "/home/u/proj2")).is_none());
            let host = &runner.config().caller_roots[0].host_root;
            assert!(!host.join("u").exists());
        }
    }
}

#[test]
fn mapped_fill_checkouts_join_the_nested_known_set() {
    let fill = format!(
        "/home/u/.motor-base-test-{}",
        workspace::name("/home/u/repo")
    );
    for holder in ["desired", "running-fill", "starting-fill"] {
        for candidate in [format!("{fill}/sub"), "/home/u".into()] {
            let f = Fixture::new();
            let mut config = mapped_config(&f);
            config.instance = "test".into();
            let runner = Runner::open(config).unwrap();
            {
                let mut inner = runner.core.inner.lock().unwrap();
                if holder == "desired" {
                    inner
                        .catalog
                        .repositories
                        .entry("/home/u/repo".into())
                        .or_default()
                        .desired = Some(BaseRequest {
                        workspace: "/home/u/repo".into(),
                        tip: "tip".into(),
                        siblings: vec![],
                        git_dependencies: vec![],
                        command: None,
                    });
                } else {
                    let mut request = run(&f, "/home/u/repo");
                    request.workspace_key = "base:/home/u/repo".into();
                    request.cwd = fill.clone();
                    let mut job = Job::new(request.job_id, Some(request), 0, 0, 2);
                    job.base = true;
                    if holder == "starting-fill" {
                        job.terminal(Outcome::Cancelled, 1);
                        inner.starting.insert(job.id);
                    }
                    inner.jobs.insert(job.id, job);
                }
            }
            let request = run(&f, &candidate);
            let (reason, detail) = admission(&runner, &request).unwrap();
            assert_eq!(reason, RefusalReason::WorkspaceSetupFailed);
            assert!(
                detail.contains(&fill) && detail.contains(&candidate),
                "{detail}"
            );
            assert_eq!(
                runner
                    .admit_prepare(&prepare(&request))
                    .unwrap()
                    .err()
                    .unwrap()
                    .reason,
                reason
            );
        }
    }
}

#[test]
fn queue_base_skips_a_mapped_fill_that_nests_a_workspace_and_retries() {
    let f = Fixture::new();
    let mut config = mapped_config(&f);
    config.instance = "test".into();
    let runner = Runner::open(config).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    let fill = format!(
        "/home/u/.motor-base-test-{}",
        workspace::name("/home/u/repo")
    );
    let blocker = format!("{fill}/sub");
    workspace::record_workspace_entry(&mut inner.catalog, &blocker, &blocker, "tip", &[], 0);
    let plan = || BasePlan {
        tip: "tip".into(),
        command: "true".into(),
        explicit_command: true,
        descriptor: Ok("descriptor".into()),
        dependency_free: false,
        fill: Some(FillInputs {
            git_dependencies: Ok(vec![]),
            tree: Some(Ok("tree".into())),
        }),
    };
    assert!(runner
        .queue_base_planned(&mut inner, "/home/u/repo", "/home/u/repo", plan())
        .unwrap());
    assert!(inner.jobs.is_empty());
    assert!(inner.catalog.repositories["/home/u/repo"].desired.is_some());
    inner.catalog.workspaces.remove(&blocker);
    assert!(runner
        .queue_base_planned(&mut inner, "/home/u/repo", "/home/u/repo", plan())
        .unwrap());
    let request = inner
        .jobs
        .values()
        .next()
        .unwrap()
        .request
        .as_ref()
        .unwrap();
    assert_eq!(request.workspace_key, "base:/home/u/repo");
    assert_eq!(request.cwd, fill);
    assert!(!runner.config().caller_roots[0].host_root.join("u").exists());
}

#[tokio::test]
async fn reserved_bind_and_runner_name_refusals_precede_mutation_for_each_operation() {
    let f = Fixture::new();
    let runner = Runner::open(mapped_config(&f)).unwrap();
    let host = &runner.config().caller_roots[0].host_root;
    let trash = host.join(".motor-trash-test");
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join("secret"), "retired secret").unwrap();
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| {
        binds.borrow_mut().extend([
            (
                f.scratch.join("tool"),
                PathBuf::from("/home/ubuntu/.rustup"),
            ),
            (
                f.scratch.join("bun"),
                PathBuf::from("/home/ubuntu/.bun/bin"),
            ),
        ]);
    });
    for path in [
        "/home/ubuntu",
        "/home/ubuntu/.rustup",
        "/home/ubuntu/.rustup/x",
        "/home/ubuntu/.bun",
        "/home/.motor-trash-test",
        "/home/.motor-trash-e2e/f",
        "/home/.motor-owner",
        "/home/.motor-base-x",
    ] {
        let request = run(&f, path);
        let (reason, detail) = admission(&runner, &request).unwrap();
        assert_eq!(reason, RefusalReason::WorkspaceKeyRejected);
        assert!(detail.contains(path), "{detail}");
        assert_eq!(
            runner
                .admit_prepare(&prepare(&request))
                .unwrap()
                .err()
                .unwrap()
                .reason,
            reason
        );
        assert!(runner.drop_workspace(path).await.is_err());
        let mut inner = runner.core.inner.lock().unwrap();
        let plan = BasePlan {
            tip: "tip".into(),
            command: "true".into(),
            explicit_command: true,
            descriptor: Ok("descriptor".into()),
            dependency_free: false,
            fill: None,
        };
        assert!(!runner
            .queue_base_planned(&mut inner, path, path, plan)
            .unwrap());
        assert!(inner.catalog.repositories.is_empty());
        drop(inner);
        let mut returned = request;
        returned.return_paths = vec![format!("{path}/secret")];
        let result = crate::return_paths::collect_authorized(runner.config(), &returned, 1);
        assert_eq!(
            result.return_paths_failed.unwrap().cause,
            motor_protocol::ReturnFailureCause::Escape
        );
    }
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| binds.borrow_mut().clear());
    assert_eq!(
        fs::read_to_string(trash.join("secret")).unwrap(),
        "retired secret"
    );
    assert!(!host.join("ubuntu").exists());
}

#[test]
fn test_only_mapped_siblings_are_refused_before_mutation() {
    let f = Fixture::new();
    let runner = Runner::open(mapped_config(&f)).unwrap();
    let identity_workspace = f.run("true").0.workspace_key;
    for (workspace, sibling) in [
        (identity_workspace.as_str(), "/home/u/sibling"),
        ("/home/u/workspace", text(&f.main)),
    ] {
        let mut request = run(&f, workspace);
        request.siblings = vec![SiblingSnapshot {
            canonical_path: sibling.into(),
            commit: "tip".into(),
            tree_hash: "tree".into(),
            base_commit: None,
        }];
        let (reason, detail) = admission(&runner, &request).unwrap();
        assert_eq!(reason, RefusalReason::WorkspaceKeyRejected);
        assert!(
            detail.contains("mapped siblings require isolation"),
            "{detail}"
        );
        assert_eq!(
            runner
                .admit_prepare(&prepare(&request))
                .unwrap()
                .err()
                .unwrap()
                .reason,
            reason
        );
    }
    assert!(!runner.config().caller_roots[0].host_root.join("u").exists());
}

#[test]
fn mapped_return_reads_use_host_files_and_refuse_escapes() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let config = mapped_config(&f);
    let host = config.caller_roots[0].host_root.join("u/proj");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("out"), b"mapped output").unwrap();
    symlink("out", host.join("relative")).unwrap();
    symlink("/home/u/proj/out", host.join("absolute")).unwrap();
    symlink("/home/other/secret", host.join("outside")).unwrap();
    symlink(host.join("out"), host.join("host-target")).unwrap();
    let job_directory = f.scratch.join("return-job");
    fs::create_dir_all(&job_directory).unwrap();
    for name in [
        "out",
        "relative",
        "absolute",
        "outside",
        "host-target",
        "../secret",
    ] {
        let mut request = run(&f, "/home/u/proj");
        request.return_paths = vec![format!("/home/u/proj/{name}")];
        let result = crate::return_paths::collect_authorized(&config, &request, 1);
        let chunks =
            crate::return_paths::collect_chunked_authorized(&config, &request, &job_directory);
        if ["out", "relative", "absolute"].contains(&name) {
            let file = &result.returned_files.unwrap()[0];
            assert_eq!(file.path, request.return_paths[0]);
            assert_eq!(file.data, "bWFwcGVkIG91dHB1dA==");
            assert!(matches!(
                chunks.files[0].outcome,
                motor_protocol::ReturnManifestOutcome::Returned { .. }
            ));
            assert_eq!(chunks.total_bytes, 13);
        } else {
            assert_eq!(
                result.return_paths_failed.unwrap().cause,
                motor_protocol::ReturnFailureCause::Escape
            );
            assert!(matches!(
                chunks.files[0].outcome,
                motor_protocol::ReturnManifestOutcome::Failed {
                    cause: motor_protocol::ReturnPathsFailureCause::Escape,
                    ..
                }
            ));
        }
    }
    fs::rename(&host, host.with_extension("saved")).unwrap();
    symlink(host.with_extension("saved"), &host).unwrap();
    let mut request = run(&f, "/home/u/proj");
    request.return_paths = vec!["/home/u/proj/out".into()];
    assert_eq!(
        crate::return_paths::collect_authorized(&config, &request, 1)
            .return_paths_failed
            .unwrap()
            .cause,
        motor_protocol::ReturnFailureCause::Escape
    );
}

#[test]
fn collision_table_accepts_admission_and_fill_beside_runner_tools() {
    let f = Fixture::new();
    let runner = Runner::open(mapped_config(&f)).unwrap();
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| {
        binds.borrow_mut().extend([
            (
                f.scratch.join("rustup"),
                PathBuf::from("/home/ubuntu/.rustup"),
            ),
            (
                f.scratch.join("bun"),
                PathBuf::from("/home/ubuntu/.bun/bin"),
            ),
        ])
    });
    for path in [
        "/home/ubuntu/project",
        "/home/ubuntu/.rustup2",
        "/home/ubuntu/.bun/lib",
    ] {
        let request = run(&f, path);
        assert!(admission(&runner, &request).is_none(), "{path}");
        let guard = runner.admit_prepare(&prepare(&request)).unwrap().unwrap();
        drop(guard);
        let mut inner = runner.core.inner.lock().unwrap();
        let before = inner.jobs.len();
        let plan = BasePlan {
            tip: "tip".into(),
            command: "true".into(),
            explicit_command: true,
            descriptor: Ok(format!("descriptor:{path}")),
            dependency_free: false,
            fill: Some(FillInputs {
                git_dependencies: Ok(vec![]),
                tree: Some(Ok("tree".into())),
            }),
        };
        assert!(runner
            .queue_base_planned(&mut inner, path, path, plan)
            .unwrap());
        assert_eq!(inner.jobs.len(), before + 1, "{path}");
    }
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| binds.borrow_mut().clear());
}

#[tokio::test]
async fn mapped_workspace_drop_cancels_then_waits_for_setup_ownership() {
    let f = Fixture::new();
    let mut config = mapped_config(&f);
    let hold = Hold::new("drop-owner");
    config.long_work = Some(hold.hook());
    let _release = Release(hold.clone());
    let runner = Runner::open(config).unwrap();
    let request = run(&f, "/home/u/proj");
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.insert(
            request.job_id,
            Job::new(request.job_id, Some(request.clone()), 0, 0, 2),
        );
        inner.starting.insert(request.job_id);
        inner.reserved.insert(
            request.workspace_key.clone(),
            Reservation {
                repository: request.repository_root.clone(),
                commit: request.snapshot_commit.clone(),
                siblings: vec![],
                since_ms: 0,
            },
        );
    }
    hold.arm();
    let drop = tokio::spawn({
        let runner = runner.clone();
        async move { runner.drop_workspace("/home/u/proj").await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !hold.held() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!drop.is_finished());
    {
        let mut inner = runner.core.inner.lock().unwrap();
        assert_eq!(
            inner.jobs[&request.job_id]
                .terminal
                .as_ref()
                .unwrap()
                .outcome,
            Outcome::Cancelled
        );
        inner.reserved.remove(&request.workspace_key);
        inner.starting.remove(&request.job_id);
    }
    hold.release();
    let reply = tokio::time::timeout(Duration::from_secs(5), drop)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(serde_json::to_string(&reply)
        .unwrap()
        .contains(&request.job_id.to_string()));
    assert!(!runner.config().caller_roots[0].host_root.join("u").exists());
}

#[test]
fn mapped_writer_and_catalog_checks_accept_only_authorized_host_paths() {
    let f = Fixture::new();
    let runner = Runner::open(mapped_config(&f)).unwrap();
    let request = run(&f, "/home/u/proj");
    let plan = crate::caller_path::authorize_request(
        runner.config(),
        crate::caller_path::request(&request),
        Some(crate::caller_path::run(&request).caller_cwd),
        &[],
    )
    .unwrap();
    fs::create_dir_all(&plan.host_workspace).unwrap();
    let host = crate::caller_path::host(&plan.host_workspace);
    runner
        .validate_writer_paths_with_catalog(
            &runner.core.inner.lock().unwrap(),
            host,
            Some(host),
            &[],
        )
        .unwrap();
    runner.validate_destinations(host, Some(host), &[]).unwrap();
    assert!(runner
        .validate_destinations(crate::caller_path::host_str("/home/u/proj"), None, &[])
        .is_err());
}

#[test]
fn mapped_prepare_materializes_and_records_the_host_checkout() {
    let f = Fixture::new();
    let runner = Runner::open(mapped_config(&f)).unwrap();
    let (mut request, mut bytes) = f.run("true");
    request.workspace_key = "/home/u/proj".into();
    request.repository_root = request.workspace_key.clone();
    request.cwd = request.workspace_key.clone();
    request.siblings.clear();
    request.bundles.truncate(1);
    request.bundles[0].repository = request.repository_root.clone();
    bytes.truncate(1);
    f.submit(&runner, &request, &bytes);
    let prepare = prepare(&request);
    let _guard = runner.admit_prepare(&prepare).unwrap().unwrap();
    runner.prepare(&prepare, false).unwrap();
    let host = runner.config().caller_roots[0].host_root.join("u/proj");
    assert_eq!(fs::read_to_string(host.join("source")).unwrap(), "first\n");
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key("/home/u/proj"));
}

#[tokio::test]
async fn mapped_writer_refusal_details_name_the_caller_not_host_storage() {
    let f = Fixture::new();
    let runner = Runner::open(mapped_config(&f)).unwrap();
    let host = runner.config().caller_roots[0].host_root.join("u/proj");
    fs::create_dir_all(host.join(format!(".motor-base-{}-retired", runner.config().instance)))
        .unwrap();
    let request = run(&f, "/home/u/proj");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceKeyRejected)
    ));
    let terminal = terminal(&runner, request.job_id).await;
    let json = serde_json::to_string(&terminal).unwrap();
    assert!(json.contains("/home/u/proj"), "{json}");
    assert!(
        !json.contains(runner.config().caller_roots[0].host_root.to_str().unwrap()),
        "{json}"
    );
}
