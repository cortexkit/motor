use super::*;

fn runner(f: &Fixture, per_job: bool, opted_in: bool) -> Runner {
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.per_job = per_job;
    let runner = Runner::open(config).unwrap();
    runner
        .tenant_events()
        .unwrap()
        .enable_reports(per_job, opted_in)
        .unwrap();
    runner
}

fn with_dependency(f: &Fixture, request: &mut Run, bytes: &mut Vec<Vec<u8>>) {
    let commit = &request.snapshot_commit;
    let reference = format!("refs/ck-motor/dep/{commit}");
    git(&f.main, &["update-ref", &reference, commit]);
    let path = f.scratch.join("dependency.bundle");
    git(&f.main, &["bundle", "create", text(&path), &reference]);
    git(&f.main, &["update-ref", "-d", &reference]);
    let data = fs::read(path).unwrap();
    request.git_dependencies.push(GitDependency {
        url: "https://example.com/dependency".into(),
        commits: vec![commit.clone()],
        bundle: Some(BundleManifest {
            index: bytes.len() as u32,
            repository: "https://example.com/dependency".into(),
            length: data.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&data)),
        }),
    });
    bytes.push(data);
}

#[tokio::test]
async fn verified_import_reports_each_repository_dependency_pin_and_pruned_ref_durably() {
    let f = Fixture::new();
    let runner = runner(&f, true, true);
    let (mut request, mut bytes) = f.run("true");
    with_dependency(&f, &mut request, &mut bytes);
    f.submit(&runner, &request, &bytes);
    let events = pending(&runner);
    let imported: Vec<_> = events
        .iter()
        .filter_map(|event| event.mirror_refs.as_ref())
        .collect();
    assert_eq!(imported.len(), 3);
    let refs: Vec<_> = imported
        .iter()
        .flat_map(|report| {
            assert!(report.imported);
            report.refs.iter().cloned()
        })
        .collect();
    assert_eq!(refs.len(), 4);
    assert_eq!(
        refs.iter()
            .filter(|r| r.repository.as_deref() == Some(&f.root))
            .count(),
        2
    );
    assert_eq!(
        refs.iter()
            .filter(|r| r.repository.as_deref() == Some(&f.canonical))
            .count(),
        1
    );
    assert_eq!(
        refs.iter()
            .filter(|r| r.dependency.as_deref() == Some("https://example.com/dependency"))
            .count(),
        1
    );
    for reference in &refs {
        let mirror = match (&reference.repository, &reference.dependency) {
            (Some(repository), None) => runner.core.trees.mirror(repository),
            (None, Some(dependency)) => runner.core.trees.dependency_mirror(dependency),
            _ => panic!("report must identify exactly one mirror"),
        };
        assert_eq!(
            git(&mirror, &["rev-parse", &reference.reference]),
            reference.commit
        );
        assert_eq!(
            git(&mirror, &["cat-file", "-t", &reference.commit]),
            "commit"
        );
    }
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.clear();
        inner.catalog.workspaces.clear();
        inner.catalog.transferred.clear();
        inner.catalog.repositories.clear();
    }
    runner.prune_refs().unwrap();
    let log = crate::tenant::EventLog::open_for_mode(&runner.config().state_dir(), true).unwrap();
    let mut retired = Vec::new();
    while let Some(event) = log.first() {
        if let Some(report) = event.mirror_refs {
            assert!(!report.imported);
            retired.extend(report.refs);
        }
        log.ack(event.event_seq).unwrap();
    }
    assert_eq!(retired.len(), 3);
    for reference in retired {
        assert!(refs.contains(&reference));
        assert!(workspace::git(
            &runner
                .core
                .trees
                .mirror(reference.repository.as_ref().unwrap()),
            &["rev-parse", "--verify", &reference.reference]
        )
        .is_err());
    }
}

#[tokio::test]
async fn legacy_leases_and_tenant_mode_never_persist_new_report_event_kinds() {
    for (per_job, opted_in) in [(false, false), (false, true), (true, false)] {
        let f = Fixture::new();
        let runner = runner(&f, per_job, opted_in);
        let (request, bytes) = f.run("true");
        f.submit(&runner, &request, &bytes);
        assert!(pending(&runner).iter().all(|e| e.mirror_refs.is_none()));
        runner.tick().unwrap();
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        let stored: serde_json::Value =
            crate::storage::read_json(&runner.config().state_dir().join("tenant-events.json"))
                .unwrap();
        if !per_job {
            assert!(stored.get("mirrors").is_none());
        }
    }
}

#[tokio::test]
async fn missing_snapshot_tip_sibling_or_dependency_without_bundle_is_transfer_interrupted() {
    for missing in ["snapshot", "tip", "sibling", "dependency"] {
        let f = Fixture::new();
        let runner = runner(&f, true, true);
        let (mut request, bytes) = f.run("touch marker");
        let spool = f.spool(&runner, &request, &bytes);
        runner
            .core
            .trees
            .fetch(
                &spool.directory,
                &request.bundles,
                &f.root,
                &request.snapshot_commit,
                &request.integration_tip,
                &request.siblings,
            )
            .unwrap();
        request.bundles.clear();
        match missing {
            "snapshot" => request.snapshot_commit = "a".repeat(40),
            "tip" => request.integration_tip = "b".repeat(40),
            "sibling" => request.siblings[0].commit = "c".repeat(40),
            "dependency" => {
                request.git_dependencies = vec![GitDependency {
                    url: "https://example.com/missing".into(),
                    commits: vec!["d".repeat(40)],
                    bundle: None,
                }]
            }
            _ => unreachable!(),
        }
        f.submit(&runner, &request, &[]);
        runner.tick().unwrap();
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::TransferInterrupted,
            },
            "{missing}"
        );
        assert!(runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .started_ms
            .is_none());
        assert!(!Path::new(&f.key).join("marker").exists());
        assert!(pending(&runner).iter().all(|e| e.mirror_refs.is_none()));
    }
}

#[tokio::test]
async fn missing_thin_bundle_prerequisite_is_transfer_interrupted_without_start_or_import_report() {
    let f = Fixture::new();
    let runner = runner(&f, true, true);
    let prerequisite = git(&f.main, &["rev-parse", "HEAD"]);
    commit(&f.main, "next\n");
    let (mut request, mut bytes) = f.run("touch marker");
    let reference = "refs/ck-motor/thin/snapshot";
    git(&f.main, &["update-ref", reference, "HEAD"]);
    let path = f.scratch.join("thin.bundle");
    git(
        &f.main,
        &[
            "bundle",
            "create",
            text(&path),
            reference,
            &format!("^{prerequisite}"),
        ],
    );
    git(&f.main, &["update-ref", "-d", reference]);
    bytes[0] = fs::read(path).unwrap();
    request.bundles[0].length = bytes[0].len() as u64;
    request.bundles[0].sha256 = format!("{:x}", Sha256::digest(&bytes[0]));
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TransferInterrupted,
        }
    );
    assert!(!Path::new(&f.key).join("marker").exists());
    assert!(pending(&runner).iter().all(|e| e.mirror_refs.is_none()));
}

#[tokio::test]
async fn next_boot_retirement_before_any_run_reports_local_identity_or_explicit_unknown() {
    for known in [true, false] {
        let f = Fixture::new();
        let first = runner(&f, true, true);
        let (request, bytes) = f.run("true");
        f.submit(&first, &request, &bytes);
        first.tick().unwrap();
        assert_eq!(
            terminal(&first, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        pending(&first);
        let mirror = first.core.trees.mirror(&f.root);
        let state = first.config().state_dir();
        drop(first);
        if !known {
            git(
                &mirror,
                &[
                    "config",
                    "--file",
                    text(&mirror.join("config")),
                    "--unset",
                    "motor.repository",
                ],
            );
        }
        fs::remove_dir_all(state.join("jobs")).unwrap();
        fs::remove_file(state.join("tenant-events.json")).unwrap();
        let second = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut config = f.config();
                config.manual_tick = true;
                config.tenant_vm = true;
                config.per_job = true;
                match Runner::open(config) {
                    Ok(runner) => break runner,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        // Terminal persistence can precede the last setup-task
                        // clone releasing the runner's process lock.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("second boot failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        second.discard(&mirror).unwrap();
        assert!(pending(&second).iter().all(|e| e.mirror_refs.is_none()));
        second
            .tenant_events()
            .unwrap()
            .enable_reports(true, true)
            .unwrap();
        let reports = pending(&second)
            .into_iter()
            .filter_map(|e| e.mirror_refs)
            .collect::<Vec<_>>();
        let refs = reports
            .into_iter()
            .flat_map(|r| {
                assert!(!r.imported);
                r.refs
            })
            .collect::<Vec<_>>();
        assert!(refs.iter().any(|r| r.commit == request.snapshot_commit));
        assert!(refs
            .iter()
            .filter(|r| r.commit == request.snapshot_commit)
            .all(
                |r| r.repository.as_deref() == known.then_some(f.root.as_str())
                    && r.dependency.is_none()
            ));
        assert!(!second.has_job_record(request.job_id));
    }
}
