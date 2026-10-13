use super::*;

#[tokio::test]
async fn head_workspace_survives_idle_expiry_and_task_defaults_expire() {
    for kind in [Some(WorkspaceKind::Head), Some(WorkspaceKind::Task), None] {
        let f = Fixture::new();
        let runner = Runner::open(f.config()).unwrap();
        let (mut r, bytes) = f.run("echo preserved > marker");
        r.workspace_kind = kind;
        r.workspace_key = f.root.clone();
        r.cwd = r.workspace_key.clone();
        f.submit(&runner, &r, &bytes);
        assert_eq!(
            terminal(&runner, r.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        base_done(&runner).await;
        let used = runner.core.inner.lock().unwrap().catalog.workspaces[&r.workspace_key].used_ms;
        f.clock.store(used + RETENTION_MS + 1, Ordering::SeqCst);
        runner.maintenance().unwrap();
        assert_eq!(
            Path::new(&r.workspace_key).exists(),
            kind == Some(WorkspaceKind::Head)
        );
        assert!(runner.core.trees.mirror(&f.root).exists());
        assert!(
            runner.core.inner.lock().unwrap().catalog.repositories[&f.root]
                .filled_descriptor
                .is_some()
        );
        if kind == Some(WorkspaceKind::Head) {
            assert_eq!(
                fs::read_to_string(Path::new(&r.workspace_key).join("marker")).unwrap(),
                "preserved\n"
            );
            assert!(
                runner
                    .drop_workspace(&r.workspace_key)
                    .await
                    .unwrap()
                    .dropped
            );
            assert!(runner.core.trees.mirror(&f.root).exists());
        }
    }
}

#[test]
fn head_workspace_is_lru_evictable_outside_task_roots() {
    let f = Fixture::new();
    let head = PathBuf::from(&f.root);
    let task = f.scratch.join("remote/worktrees/newer");
    let outside_task = f.scratch.join("remote/source/outside-task");
    let reclaim = head.clone();
    let mut config = f.config();
    config.free_space_floor_bytes = 100;
    config.free_space_probe = Arc::new(move |_| Ok(if reclaim.exists() { 10 } else { 200 }));
    let runner = Runner::open(config).unwrap();
    for (path, kind, used) in [
        (&head, WorkspaceKind::Head, 1),
        (&task, WorkspaceKind::Task, 2),
        (&outside_task, WorkspaceKind::Task, 0),
    ] {
        fs::create_dir_all(path.join("target")).unwrap();
        fs::write(path.join("target/artifact"), [1; 8192]).unwrap();
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.workspaces.insert(
            text(path).into(),
            Workspace {
                kind,
                used_ms: used,
                ..Workspace::default()
            },
        );
    }
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .repositories
            .insert(f.root.clone(), Repository::default());
        inner
            .catalog
            .sibling_commits
            .insert(f.root.clone(), "old".into());
    }
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!head.exists());
    assert!(task.exists());
    assert!(outside_task.exists());
    assert_eq!(
        runner
            .runner_status()
            .disk
            .unwrap()
            .last_eviction
            .unwrap()
            .path,
        f.root
    );
}

#[test]
fn runner_still_rejects_strict_workspace_ancestors_and_invalid_siblings() {
    assert!(workspace::validate("/source/repo", "/source/repo", &[]).is_ok());
    assert!(workspace::validate("/source", "/source/repo", &[]).is_err());
    let sibling = SiblingSnapshot {
        canonical_path: "/source/repo".into(),
        commit: "a".repeat(40),
        tree_hash: "b".repeat(40),
        base_commit: None,
    };
    assert!(workspace::validate("/source/repo", "/source/repo", &[sibling]).is_err());
}

#[tokio::test]
async fn workspace_sibling_fences_queue_both_directions_for_jobs_and_reservations() {
    for reserved in [false, true] {
        for workspace_first in [false, true] {
            let f = Fixture::new();
            let (runner, hold, _release) = held_runner(&f);
            hold.arm();
            let (mut workspace_owner, _) = f.run("true");
            workspace_owner.workspace_key = f.canonical.clone();
            workspace_owner.repository_root = f.canonical.clone();
            workspace_owner.cwd = f.canonical.clone();
            workspace_owner.workspace_kind = Some(WorkspaceKind::Head);
            workspace_owner.siblings.clear();
            let (sibling_owner, _) = f.run("true");
            let (active, later) = if workspace_first {
                (workspace_owner, sibling_owner)
            } else {
                (sibling_owner, workspace_owner)
            };
            {
                let mut inner = runner.core.inner.lock().unwrap();
                let mut owner = Job::new(active.job_id, Some(active.clone()), runner.now(), 1, 1);
                if reserved {
                    owner.state = JobState::Queued;
                    inner.starting.insert(owner.id);
                    inner.reserved.insert(
                        active.workspace_key.clone(),
                        Reservation {
                            repository: active.repository_root.clone(),
                            commit: active.snapshot_commit.clone(),
                            siblings: active.siblings.clone(),
                            since_ms: runner.now(),
                        },
                    );
                } else {
                    owner.state = JobState::Running;
                }
                inner.jobs.insert(owner.id, owner);
                let mut queued = Job::new(later.job_id, Some(later.clone()), runner.now(), 2, 1);
                queued.state = JobState::Queued;
                inner.jobs.insert(queued.id, queued);
            }
            runner.tick().unwrap();
            {
                let inner = runner.core.inner.lock().unwrap();
                assert!(
                    !inner.starting.contains(&later.job_id),
                    "cross-role path owner must queue the later setup"
                );
                assert_eq!(inner.jobs[&later.job_id].state, JobState::Queued);
                assert!(runner
                    .queue_wait_detail(&inner, later.job_id)
                    .contains(&active.job_id.to_string()));
            }
            assert_eq!(
                runner
                    .prepare(&prepare_for(&later), true)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            {
                let mut inner = runner.core.inner.lock().unwrap();
                inner.jobs.remove(&active.job_id);
                inner.starting.remove(&active.job_id);
                inner.reserved.remove(&active.workspace_key);
            }
            runner.tick().unwrap();
            assert!(
                runner
                    .core
                    .inner
                    .lock()
                    .unwrap()
                    .starting
                    .contains(&later.job_id),
                "the later job must proceed after the owner releases its path"
            );
            hold.release();
            let _ = terminal(&runner, later.job_id).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_drop_waits_for_a_sibling_owner_before_deleting() {
    let f = Fixture::new();
    let hold = Hold::new("drop-owner");
    let mut config = f.config();
    config.long_work = Some(hold.hook());
    let runner = Runner::open(config).unwrap();
    let (active, _) = f.run("true");
    let path = PathBuf::from(&f.canonical);
    fs::create_dir_all(&path).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.workspaces.insert(
            f.canonical.clone(),
            Workspace {
                kind: WorkspaceKind::Head,
                ..Workspace::default()
            },
        );
        let mut job = Job::new(active.job_id, Some(active.clone()), runner.now(), 1, 1);
        job.state = JobState::Running;
        inner.jobs.insert(job.id, job);
    }
    hold.arm();
    let dropping = tokio::spawn({
        let runner = runner.clone();
        let key = f.canonical.clone();
        async move { runner.drop_workspace(&key).await }
    });
    reached(&hold).await;
    assert!(path.exists());
    assert!(!dropping.is_finished());
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .remove(&active.job_id);
    hold.release();
    assert!(dropping.await.unwrap().unwrap().dropped);
    assert!(!path.exists());
}
