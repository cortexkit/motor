use super::*;

fn workspace(runner: &Runner, path: &Path, used_ms: u64) {
    fs::create_dir_all(path.join("target")).unwrap();
    fs::write(path.join("target/artifact"), vec![1; 8192]).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    inner.catalog.workspaces.insert(
        text(path).into(),
        Workspace {
            used_ms,
            ..Workspace::default()
        },
    );
    runner.catalog(&inner).unwrap();
}

fn config(f: &Fixture, probe: impl Fn(&Path) -> io::Result<u64> + Send + Sync + 'static) -> Config {
    let mut c = f.config();
    c.free_space_floor_bytes = 100;
    c.free_space_probe = Arc::new(probe);
    c
}

#[test]
fn disk_eviction_skips_reserved_and_running_workspaces() {
    let f = Fixture::new();
    let root = f.scratch.join("remote/worktrees");
    let reserved = root.join("reserved");
    let running = root.join("running");
    let queued = root.join("queued");
    let oldest_idle = root.join("old-idle");
    let newest_idle = root.join("new-idle");
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let reclaim = oldest_idle.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && reclaim.exists() {
            10
        } else {
            200
        })
    }))
    .unwrap();
    let now = runner.now();
    for (path, age) in [
        (&reserved, 50),
        (&running, 40),
        (&queued, 30),
        (&oldest_idle, 20),
        (&newest_idle, 10),
    ] {
        workspace(&runner, path, now - age);
    }
    for path in [&reserved, &running, &queued] {
        cargo_output(
            path,
            "debug/incremental/protected/state.bin",
            now - 49 * 60 * 60 * 1000,
        );
        cargo_output(
            path,
            "debug/deps/protected-0123456789abcdef",
            now - 49 * 60 * 60 * 1000,
        );
    }
    let (mut r, _) = f.run("true");
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.reserved.insert(
            text(&reserved).into(),
            Reservation {
                repository: f.root.clone(),
                commit: String::new(),
                siblings: vec![],
                since_ms: now,
            },
        );
        for (path, state) in [(&running, JobState::Running), (&queued, JobState::Queued)] {
            r.job_id = Uuid::now_v7();
            r.workspace_key = text(path).into();
            r.cwd = text(path).into();
            let mut job = Job::new(r.job_id, Some(r.clone()), now, 1, 99);
            job.state = state;
            runner.save(&job).unwrap();
            inner.jobs.insert(job.id, job);
        }
    }
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(
        reserved.join("target/artifact").exists(),
        "reserved workspace must survive disk eviction"
    );
    assert!(running.join("target/artifact").exists());
    assert!(queued.join("target/artifact").exists());
    for path in [&reserved, &running, &queued] {
        assert!(path
            .join("target/debug/incremental/protected/state.bin")
            .exists());
        assert!(path
            .join("target/debug/deps/protected-0123456789abcdef")
            .exists());
    }
    assert!(
        !oldest_idle.exists(),
        "the oldest idle workspace and its target must be deleted"
    );
    assert!(newest_idle.exists());
    let status = runner.runner_status();
    let disk = status.disk.unwrap();
    assert_eq!(disk.free_bytes, Some(200));
    assert_eq!(disk.floor_bytes, 100);
    let eviction = disk.last_eviction.unwrap();
    assert_eq!(eviction.path, text(&oldest_idle));
    assert_eq!(eviction.last_used_ms, now - 20);
    assert!(eviction.bytes >= 8192);
    let health = status.health.unwrap();
    assert_eq!(health.counts.evictions, 1);
    assert_eq!(health.counts.eviction_bytes, eviction.bytes);
    assert_eq!(health.free_bytes, Some(200));
    assert_eq!(health.queue_depth, 1);
    assert_eq!(health.running.len(), 1);
    assert_eq!(health.running[0].weight, 99);
}

#[test]
fn tenant_status_reports_current_workspaces_and_drops_evicted_entries() {
    let f = Fixture::new();
    let oldest = f.scratch.join("remote/worktrees/old-warm");
    let newest = f.scratch.join("remote/worktrees/new-warm");
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let reclaim = oldest.clone();
    let mut c = config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && reclaim.exists() {
            10
        } else {
            200
        })
    });
    c.tenant_vm = true;
    let runner = Runner::open(c).unwrap();
    let now = runner.now();
    let old_job = Uuid::now_v7();
    let new_job = Uuid::now_v7();
    workspace(&runner, &f.scratch.join("remote/worktrees/legacy"), now);
    for (path, job_id, used_ms) in [(&oldest, old_job, now - 20), (&newest, new_job, now - 10)] {
        workspace(&runner, path, used_ms);
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .get_mut(text(path))
            .unwrap()
            .last_job_id = Some(job_id);
    }
    let held = runner.runner_status().health.unwrap().held_workspaces;
    assert_eq!(
        held.iter().map(|row| row.job_id).collect::<Vec<_>>(),
        vec![new_job, old_job]
    );
    assert_eq!(held[0].used_ms, now - 10);
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!oldest.exists());
    let held = runner.runner_status().health.unwrap().held_workspaces;
    assert_eq!(
        held,
        vec![HeldWorkspace {
            job_id: new_job,
            used_ms: now - 10
        }]
    );
    let wire = serde_json::to_string(&held).unwrap();
    assert!(
        !wire.contains(text(&newest)),
        "host cache telemetry must not disclose workspace paths"
    );
}

#[test]
fn disk_admission_preserves_incoming_sibling_head() {
    let f = Fixture::new();
    let (request, _) = f.run("true");
    let sibling = PathBuf::from(&request.siblings[0].canonical_path);
    let runner = Runner::open(config(&f, |_| Ok(150))).unwrap();
    workspace(&runner, &sibling, runner.now());
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .get_mut(text(&sibling))
        .unwrap()
        .kind = WorkspaceKind::Head;
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        sibling.join("target/artifact").exists(),
        "admission evicted its canonical sibling"
    );
}

#[test]
fn disk_admission_preserves_incoming_head_workspace_and_sibling() {
    incoming_workspace_survives(WorkspaceKind::Head);
}

#[test]
fn disk_admission_preserves_incoming_ordinary_workspace_and_sibling() {
    incoming_workspace_survives(WorkspaceKind::Task);
}

fn incoming_workspace_survives(kind: WorkspaceKind) {
    let f = Fixture::new();
    let (mut request, _) = f.run("true");
    request.workspace_kind = Some(kind);
    if kind == WorkspaceKind::Head {
        request.workspace_key = f.root.clone();
        request.cwd = f.root.clone();
    }
    let incoming = PathBuf::from(&request.workspace_key);
    let sibling = PathBuf::from(&request.siblings[0].canonical_path);
    let idle = f.scratch.join("remote/worktrees/unrelated");
    let reclaim = idle.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if reclaim.exists() { 150 } else { 200 })
    }))
    .unwrap();
    for (path, age, workspace_kind) in [
        (&incoming, 30, kind),
        (&sibling, 20, WorkspaceKind::Head),
        (&idle, 2 * 60 * 60 * 1000, WorkspaceKind::Task),
    ] {
        workspace(&runner, path, runner.now() - age);
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .get_mut(text(path))
            .unwrap()
            .kind = workspace_kind;
    }
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        incoming.join("target/artifact").exists(),
        "admission evicted its own idle workspace"
    );
    assert!(
        sibling.join("target/artifact").exists(),
        "admission evicted its canonical sibling"
    );
    assert!(
        !idle.exists(),
        "pressure must still evict unrelated idle trees"
    );
    assert_eq!(
        runner
            .runner_status()
            .disk
            .unwrap()
            .last_eviction
            .unwrap()
            .path,
        text(&idle)
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(inner.catalog.workspaces.contains_key(text(&incoming)));
    assert!(inner.catalog.workspaces.contains_key(text(&sibling)));
    assert_eq!(inner.jobs[&request.job_id].state, JobState::AwaitingBundles);
}

#[test]
fn disk_admission_protects_canonical_alias_through_job_insertion() {
    let f = Fixture::new();
    let (mut request, _) = f.run("true");
    let incoming = PathBuf::from(&request.workspace_key);
    fs::create_dir_all(incoming.parent().unwrap()).unwrap();
    let alias = f.scratch.join("alias");
    std::os::unix::fs::symlink(incoming.parent().unwrap(), &alias).unwrap();
    request.workspace_key = text(&alias.join("a")).into();
    request.cwd = request.workspace_key.clone();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let mut c = config(&f, |_| Ok(150));
    c.owned_workspace_roots.push(alias.clone());
    let at_handoff = entered.clone();
    let resume = release.clone();
    c.long_work = Some(Arc::new(move |label| {
        if label == "admission_record" {
            at_handoff.wait();
            resume.wait();
        }
    }));
    let runner = Runner::open(c).unwrap();
    workspace(&runner, &incoming, runner.now());
    let acceptor = runner.clone();
    let admitted = request.clone();
    let thread = std::thread::spawn(move || acceptor.accept(admitted));
    entered.wait();
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .contains_key(&request.job_id));
    assert!(runner.ensure_disk_space().unwrap().is_none());
    let survived_handoff = incoming.join("target/artifact").exists();
    release.wait();
    assert!(matches!(
        thread.join().unwrap().unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        survived_handoff,
        "canonical alias lost protection between eviction and job insertion"
    );
    assert!(runner.core.inner.lock().unwrap().admitting.is_empty());
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(
        incoming.join("target/artifact").exists(),
        "recorded job must take over protection"
    );
}

#[test]
fn disk_admission_releases_protection_after_probe_error() {
    let f = Fixture::new();
    let runner = Runner::open(config(&f, |_| Err(io::Error::other("probe failed")))).unwrap();
    let (request, _) = f.run("true");
    assert_eq!(
        runner.accept(request).err().unwrap().to_string(),
        "probe failed"
    );
    assert!(runner.core.inner.lock().unwrap().admitting.is_empty());
}

#[test]
fn disk_eviction_failure_is_backed_off_across_admissions() {
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "permission reproduction requires a non-root runner"
    );
    let f = Fixture::new();
    let runner = Runner::open(config(&f, |_| Ok(150))).unwrap();
    let idle = f.scratch.join("remote/worktrees/blocked");
    workspace(&runner, &idle, runner.now() - 2 * 60 * 60 * 1000);
    fs::set_permissions(idle.join("target"), fs::Permissions::from_mode(0o500)).unwrap();
    assert!(runner.ensure_disk_space().unwrap().is_none());
    let backed_off = runner
        .core
        .inner
        .lock()
        .unwrap()
        .eviction_retry_after
        .contains_key(&idle);
    fs::set_permissions(idle.join("target"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(backed_off, "unremovable candidate must be backed off");
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .counts
            .eviction_failures,
        1
    );
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(
        idle.exists(),
        "a subsequent admission must not retry the failed tree immediately"
    );
    f.clock.fetch_add(5 * 60 * 1000, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(
        !idle.exists(),
        "expired backoff must allow recovery after permissions are repaired"
    );
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .eviction_retry_after
        .is_empty());
    let counts = runner.runner_status().health.unwrap().counts;
    assert_eq!(counts.eviction_failures, 1);
    assert_eq!(counts.evictions, 1);
}

#[test]
fn disk_admission_evicts_then_admits_and_reprobes() {
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/idle");
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let reclaimed = idle.clone();
    let probes = Arc::new(AtomicU64::new(0));
    let count = probes.clone();
    let runner = Runner::open(config(&f, move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(if active.load(Ordering::SeqCst) && reclaimed.exists() {
            1
        } else {
            200
        })
    }))
    .unwrap();
    workspace(&runner, &idle, runner.now());
    pressure.store(true, Ordering::SeqCst);
    let (r, _) = f.run("true");
    assert!(matches!(
        runner.accept(r.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(!idle.exists());
    assert!(probes.load(Ordering::SeqCst) >= 2);
    let inner = runner.core.inner.lock().unwrap();
    assert_eq!(inner.jobs[&r.job_id].state, JobState::AwaitingBundles);
    assert!(inner.jobs[&r.job_id].started_ms.is_none());
    assert!(!inner.catalog.workspaces.contains_key(text(&idle)));
}

#[test]
fn disk_soft_cleanup_evicts_lru_before_the_hard_floor() {
    let f = Fixture::new();
    let old = f.scratch.join("remote/worktrees/old-idle");
    let new = f.scratch.join("remote/worktrees/new-idle");
    let reclaim = old.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if reclaim.exists() { 150 } else { 200 })
    }))
    .unwrap();
    workspace(&runner, &old, runner.now() - 2 * 60 * 60 * 1000);
    workspace(&runner, &new, runner.now() - 60 * 60 * 1000);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        !old.exists(),
        "soft pressure must reclaim the LRU workspace even above the hard floor"
    );
    assert!(
        new.exists(),
        "eviction must stop when free bytes reach twice the floor"
    );
    assert_eq!(runner.runner_status().disk.unwrap().free_bytes, Some(200));
}

#[test]
fn disk_soft_cleanup_admits_when_reflinks_cannot_reach_soft_level() {
    let f = Fixture::new();
    let runner = Runner::open(config(&f, |_| Ok(100))).unwrap();
    let idle = f.scratch.join("remote/worktrees/idle");
    workspace(&runner, &idle, runner.now() - 2 * 60 * 60 * 1000);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(!idle.exists());
    let disk = runner.runner_status().disk.unwrap();
    assert_eq!(disk.free_bytes, Some(100));
    assert!(disk.last_eviction.is_some());
    assert!(
        runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .terminal
            .is_none(),
        "missing the soft goal must not become a refusal, even exactly at the hard floor"
    );
}

#[test]
fn disk_soft_pressure_evicts_only_candidates_used_over_30_minutes_ago() {
    let f = Fixture::new();
    let old = f.scratch.join("remote/worktrees/old-idle");
    let recent = f.scratch.join("remote/worktrees/recent-idle");
    let runner = Runner::open(config(&f, |_| Ok(150))).unwrap();
    let now = runner.now();
    workspace(&runner, &old, now - 2 * 60 * 60 * 1000);
    workspace(&runner, &recent, now - 5 * 60 * 1000);

    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        !old.exists(),
        "the two-hour-old candidate should be evicted"
    );
    assert!(
        recent.exists(),
        "the five-minute-old candidate should survive"
    );
}

#[test]
fn disk_soft_pressure_removes_fresh_garbage_but_keeps_recent_caches() {
    let f = Fixture::new();
    let recent = f.scratch.join("remote/worktrees/recent-idle");
    let runner = Runner::open(config(&f, |_| Ok(150))).unwrap();
    workspace(&runner, &recent, runner.now() - 5 * 60 * 1000);
    // A dead job's private tree and a pending deletion, both written just now.
    let state = runner.core.trees.state.clone();
    let sandbox = state.join("sandboxes").join(Uuid::now_v7().to_string());
    let trash = state.join("trash").join(Uuid::now_v7().to_string());
    for dir in [&sandbox, &trash] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("leftover"), "x").unwrap();
    }

    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        !sandbox.exists(),
        "an orphaned sandbox is garbage at any age"
    );
    assert!(!trash.exists(), "a pending deletion is garbage at any age");
    assert!(recent.exists(), "a recent cache should still be kept");
}

#[test]
fn disk_soft_pressure_admits_when_all_candidates_are_recent() {
    let f = Fixture::new();
    let recent = f.scratch.join("remote/worktrees/recent-idle");
    let runner = Runner::open(config(&f, |_| Ok(150))).unwrap();
    workspace(&runner, &recent, runner.now() - 5 * 60 * 1000);

    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(recent.exists(), "a recent candidate should not be evicted");
    assert!(runner.core.inner.lock().unwrap().last_eviction.is_none());
    let health = runner.runner_status().health.unwrap();
    assert_eq!(health.counts.soft_goal_unmet_passes, 1);
    assert_eq!(health.counts.skipped_recent, 1);
    assert_eq!(health.counts.evictions, 0);
}

#[test]
fn disk_hard_pressure_evicts_recent_candidates_in_lru_order_until_soft_goal() {
    let f = Fixture::new();
    let oldest = f.scratch.join("remote/worktrees/oldest");
    let middle = f.scratch.join("remote/worktrees/middle");
    let newest = f.scratch.join("remote/worktrees/newest");
    let probe_oldest = oldest.clone();
    let probe_middle = middle.clone();
    let probe_newest = newest.clone();
    let runner = Runner::open(config(&f, move |_| {
        if probe_oldest.exists() {
            assert!(
                probe_middle.exists() && probe_newest.exists(),
                "hard-pressure eviction must begin with the LRU candidate"
            );
            Ok(50)
        } else if probe_middle.exists() {
            assert!(
                probe_newest.exists(),
                "hard-pressure eviction must continue in LRU order"
            );
            Ok(75)
        } else {
            Ok(200)
        }
    }))
    .unwrap();
    let now = runner.now();
    workspace(&runner, &oldest, now - 5 * 60 * 1000);
    workspace(&runner, &middle, now - 4 * 60 * 1000);
    workspace(&runner, &newest, now - 3 * 60 * 1000);

    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(!oldest.exists());
    assert!(!middle.exists());
    assert!(newest.exists(), "eviction should stop at the soft goal");
}

#[test]
fn disk_hard_pressure_switches_to_recent_cache_protection_above_floor() {
    let f = Fixture::new();
    let oldest = f.scratch.join("remote/worktrees/oldest");
    let recent_a = f.scratch.join("remote/worktrees/recent-a");
    let recent_b = f.scratch.join("remote/worktrees/recent-b");
    let probe_oldest = oldest.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if probe_oldest.exists() { 50 } else { 150 })
    }))
    .unwrap();
    let now = runner.now();
    workspace(&runner, &oldest, now - 5 * 60 * 1000);
    workspace(&runner, &recent_a, now - 4 * 60 * 1000);
    workspace(&runner, &recent_b, now - 3 * 60 * 1000);

    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(
        !oldest.exists(),
        "hard pressure should evict its LRU candidate"
    );
    assert!(
        recent_a.exists(),
        "recent candidates survive after free space rises above the floor"
    );
    assert!(
        recent_b.exists(),
        "recent candidates survive after free space rises above the floor"
    );
}

#[test]
fn disk_eviction_ignores_a_candidate_removed_after_selection() {
    let f = Fixture::new();
    let candidate = f.scratch.join("remote/worktrees/disappearing");
    let probe_candidate = candidate.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if probe_candidate.exists() { 50 } else { 150 })
    }))
    .unwrap();
    workspace(&runner, &candidate, runner.now());

    let dependency_lock = runner.core.trees.dependency_lock(&candidate);
    let dependency_guard = dependency_lock.lock().unwrap();
    let disk_runner = runner.clone();
    let eviction = std::thread::spawn(move || disk_runner.ensure_disk_space());
    let candidate_key = text(&candidate).to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if runner
            .core
            .inner
            .lock()
            .unwrap()
            .dropping
            .contains(&candidate_key)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "eviction did not select the candidate"
        );
        std::thread::yield_now();
    }
    fs::remove_dir_all(&candidate).unwrap();
    drop(dependency_guard);

    assert!(eviction.join().unwrap().unwrap().is_none());
    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.eviction_retry_after.contains_key(&candidate));
    assert!(inner.last_eviction.is_none());
}

#[test]
fn disk_soft_cleanup_does_nothing_at_twice_the_floor() {
    let f = Fixture::new();
    let runner = Runner::open(config(&f, |_| Ok(200))).unwrap();
    let idle = f.scratch.join("remote/worktrees/idle");
    workspace(&runner, &idle, runner.now());
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
    assert!(idle.exists());
    assert!(runner.runner_status().disk.unwrap().last_eviction.is_none());
}

#[test]
fn default_disk_budget_admits_at_223_gb_with_no_evictable_workspaces() {
    let f = Fixture::new();
    let mut c = f.config();
    c.free_space_floor_bytes = Config::new("unused", "default").free_space_floor_bytes;
    assert_eq!(c.free_space_floor_bytes, 128 * 1024 * 1024 * 1024);
    c.free_space_probe = Arc::new(|_| Ok(223_000_000_000));
    let runner = Runner::open(c).unwrap();
    let (request, _) = f.run("true");
    assert!(matches!(runner.accept(request).unwrap(), Admission::Accepted(0)), "223 GB is below the soft goal but above the hard floor; no eviction candidate must not mean refusal");
}

#[test]
fn explicit_sandbox_disk_budget_admits_with_sixty_three_gib_free() {
    let f = Fixture::new();
    let defaults = Config::new("unused", "default");
    assert_eq!(defaults.free_space_floor_bytes, 128 * (1 << 30));
    assert!(defaults.free_space_eviction_bytes.is_none());
    let mut c = f.config();
    c.free_space_floor_bytes = 24 * (1 << 30);
    c.free_space_eviction_bytes = Some(32 * (1 << 30));
    c.free_space_probe = Arc::new(|_| Ok(63 * (1 << 30)));
    let runner = Runner::open(c).unwrap();
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Accepted(0)
    ));
}

#[test]
fn explicit_eviction_goal_does_not_silently_use_twice_the_floor() {
    let f = Fixture::new();
    let mut c = f.config();
    c.free_space_floor_bytes = 24 * (1 << 30);
    c.free_space_eviction_bytes = Some(32 * (1 << 30));
    c.free_space_probe = Arc::new(|_| Ok(40 * (1 << 30)));
    c.clock = Arc::new(|| 4_000_000);
    let runner = Runner::open(c).unwrap();
    let idle = f.scratch.join("remote/worktrees/old-idle");
    workspace(&runner, &idle, 0);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(idle.exists());
    assert!(runner.runner_status().disk.unwrap().last_eviction.is_none());
}

#[test]
fn disk_admission_refuses_typed_when_reflinks_reclaim_too_little() {
    let f = Fixture::new();
    let runner = Runner::open(config(&f, |_| Ok(7))).unwrap();
    let idle = f.scratch.join("remote/worktrees/idle");
    workspace(&runner, &idle, runner.now());
    let (r, _) = f.run("touch should-not-run");
    assert!(matches!(
        runner.accept(r.clone()).unwrap(),
        Admission::DiskFull(RunnerDiskFull {
            free_bytes: 7,
            floor_bytes: 100
        })
    ));
    assert!(!idle.exists());
    let terminal = runner.replay(r.job_id, 0).unwrap().pop().unwrap();
    assert!(
        matches!(terminal, Frame::Terminal(t) if t.ran == Some(Ran::None) && t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::RunnerDiskFull } && t.refusal_detail() == Some("runner_disk_full: free_bytes=7 floor_bytes=100"))
    );
    let job: Job = read_json(&runner.record_path(r.job_id)).unwrap();
    assert!(job.started_ms.is_none());
    assert!(job.process.is_none());
    assert!(!Path::new(&r.workspace_key).join("should-not-run").exists());
}

#[test]
fn disk_cleanup_only_removes_stale_owned_staging() {
    let f = Fixture::new();
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) {
            7
        } else {
            200
        })
    }))
    .unwrap();
    let state = runner.config().state_dir();
    let stale = [
        state.join("spools/orphan"),
        state.join("sandboxes/orphan"),
        state.join("trash/orphan"),
    ];
    let mirror = state.join("mirrors/keep");
    let outside = f.scratch.join("not-runner-owned/review");
    let symlink = f.scratch.join("remote/worktrees/link");
    for path in stale.iter().chain([&mirror, &outside]) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("keep"), "bytes").unwrap();
    }
    workspace(&runner, &outside, runner.now() - 100);
    fs::create_dir_all(symlink.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &symlink).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner
            .catalog
            .workspaces
            .insert(text(&symlink).into(), Workspace::default());
    }
    let (r, _) = f.run("true");
    assert!(matches!(
        runner.accept(r.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    let _transfer = runner.transfer(r.job_id);
    let spool = runner.spool_dir(r.job_id);
    let sandbox = state.join("sandboxes").join(r.job_id.to_string());
    fs::create_dir_all(&spool).unwrap();
    fs::create_dir_all(&sandbox).unwrap();
    let prepare_key = f.scratch.join("remote/worktrees/preparing");
    workspace(&runner, &prepare_key, runner.now() - 200);
    let p = Prepare {
        workspace_kind: None,
        transfer_id: Uuid::now_v7(),
        workspace_key: text(&prepare_key).into(),
        repository_root: f.root.clone(),
        base_commit: String::new(),
        integration_tip: String::new(),
        siblings: vec![],
        bundles: vec![],
        git_dependencies: vec![],
    };
    let _prepare = runner.admit_prepare(&p).unwrap().unwrap();
    let prepare_spool = runner.spool_dir(p.transfer_id);
    fs::create_dir_all(&prepare_spool).unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_some());
    for path in &stale {
        assert!(!path.exists(), "stale staging survived: {}", path.display());
    }
    for path in [
        &mirror,
        &outside,
        &symlink,
        &spool,
        &sandbox,
        &prepare_key,
        &prepare_spool,
    ] {
        assert!(path.exists(), "protected path removed: {}", path.display());
    }
}

#[tokio::test]
async fn disk_full_wire_refusal_carries_tag_and_bytes_before_upload() {
    let f = Fixture::new();
    let runner = Runner::open(config(&f, |_| Ok(3))).unwrap();
    let (socket, server) = listener(&f, &runner);
    let mut client = connect(&socket).await;
    let (r, _) = f.run("false");
    write_frame_async(&mut client, &Frame::Run(r.clone()))
        .await
        .unwrap();
    let frame = read_next(&mut client).await;
    assert!(
        matches!(frame, Frame::Refusal(Refusal { job_id: Some(id), reason: RefusalReason::RunnerDiskFull, disk_full: Some(RunnerDiskFull { free_bytes: 3, floor_bytes: 100 }), .. }) if id == r.job_id)
    );
    server.abort();
    let _ = server.await;
}

#[cfg(target_os = "linux")]
fn sticky_sandbox(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for directory in ["home/tmp", "tmp", "output"] {
        fs::create_dir_all(path.join(directory)).unwrap();
        fs::write(path.join(directory).join("foreign-owned"), "private output").unwrap();
    }
    for (relative, owner, mode) in [
        ("home", "motor-job:motor-job", "0700"),
        ("home/tmp", "motor-job:motor-job", "1777"),
        ("output", "root:root", "0755"),
        ("tmp", "root:root", "1777"),
    ] {
        assert!(Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/chown", owner, "--"])
            .arg(path.join(relative))
            .status()
            .unwrap()
            .success());
        assert!(Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/chmod", mode, "--"])
            .arg(path.join(relative))
            .status()
            .unwrap()
            .success());
    }
    // Root-owned files under a job-owned sticky directory also break teardown
    // that reclaims only the top-level /tmp tree.
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/chown", "-R", "-P", "root:root", "--"])
        .arg(path.join("tmp/foreign-owned"))
        .arg(path.join("home/tmp/foreign-owned"))
        .status()
        .unwrap()
        .success());
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires trusted host sudo and motor-job account; see motor-runner README"]
fn isolated_disk_pressure_reclaims_orphaned_sticky_sandbox_after_restart() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let pressure = Arc::new(AtomicBool::new(false));
    let flag = pressure.clone();
    let mut c = config(&f, move |_| {
        Ok(if flag.load(Ordering::SeqCst) {
            150
        } else {
            200
        })
    });
    c.isolate_jobs = true;
    let runner = Runner::open(c.clone()).unwrap();
    let (request, _) = f.run("true");
    runner.accept(request.clone()).unwrap();
    let sandbox = c
        .state_dir()
        .join("sandboxes")
        .join(request.job_id.to_string());
    sticky_sandbox(&sandbox);
    let outside = f.scratch.join("outside-sandbox");
    fs::write(&outside, "untouched").unwrap();
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/chown", "motor-job:motor-job", "--"])
        .arg(&outside)
        .status()
        .unwrap()
        .success());
    let uid = fs::metadata(&outside).unwrap().uid();
    std::os::unix::fs::symlink(&outside, sandbox.join("outside-link")).unwrap();
    drop(runner);
    let runner = reopen(c);
    assert!(runner
        .job_status(request.job_id)
        .unwrap()
        .terminal
        .is_some());
    assert!(
        sandbox.exists(),
        "restart has no RuntimeDir handle for a lost pre-start job"
    );
    crate::isolation::restore_runner_access(&sandbox).unwrap();
    assert_eq!(
        fs::remove_file(sandbox.join("tmp/foreign-owned"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM),
        "ACL restore alone must reproduce sticky unlink failure"
    );
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!sandbox.exists());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    assert_eq!(
        fs::metadata(&outside).unwrap().uid(),
        uid,
        "reclamation must not follow symlinks"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires trusted host sudo and motor-job account; see motor-runner README"]
fn isolated_runtime_cleanup_reclaims_nested_sticky_directories() {
    let f = Fixture::new();
    let state = f.config().state_dir();
    let id = Uuid::now_v7();
    let sandbox = state.join("sandboxes").join(id.to_string());
    sticky_sandbox(&sandbox);
    let runtime = crate::isolation::recovered_runtime(&state, id);
    drop(runtime);
    assert!(
        !sandbox.exists(),
        "normal teardown must reclaim sticky directories outside /tmp too"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires trusted host sudo and motor-job account; see motor-runner README"]
fn isolated_disk_pressure_reclaims_job_owned_target() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/private-target");
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let reclaim = idle.clone();
    let mut c = config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && reclaim.exists() {
            1
        } else {
            200
        })
    });
    c.isolate_jobs = true;
    let runner = Runner::open(c).unwrap();
    workspace(&runner, &idle, runner.now());
    fs::set_permissions(idle.join("target"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(
        idle.join("target/artifact"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(Command::new("sudo")
        .args(["-n", "chown", "-R", "-P", "motor-job:motor-job", "--"])
        .arg(idle.join("target"))
        .status()
        .unwrap()
        .success());
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!idle.exists());
    assert!(
        runner
            .runner_status()
            .disk
            .unwrap()
            .last_eviction
            .unwrap()
            .bytes
            >= 8192
    );
}

#[test]
fn disk_guard_evicts_javascript_cache_by_last_use_and_skips_locked_publications() {
    let f = Fixture::new();
    let pressure = Arc::new(AtomicBool::new(false));
    let mut config = f.config();
    config.free_space_floor_bytes = 100;
    let cache = config.state_dir().join("js-installs");
    let old = cache.join("a".repeat(64));
    let recent = cache.join("b".repeat(64));
    let busy_key = "c".repeat(64);
    let busy = cache.join(format!("{busy_key}.publication.tmp"));
    let probe_old = old.clone();
    let flag = pressure.clone();
    config.free_space_probe = Arc::new(move |_| {
        Ok(if flag.load(Ordering::SeqCst) && probe_old.exists() {
            50
        } else {
            300
        })
    });
    let state = config.state_dir();
    let runner = Runner::open(config).unwrap();
    for (path, seconds) in [(&old, 2), (&recent, 3), (&busy, 1)] {
        fs::create_dir_all(path.join("node_modules")).unwrap();
        fs::write(path.join("node_modules/content"), "owned cache").unwrap();
        File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
    }
    let busy_lock = platform::javascript_cache_lock(&state, &busy_key).unwrap();
    busy_lock.lock_exclusive().unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(
        !old.exists(),
        "oldest idle JavaScript entry should be reclaimed"
    );
    assert!(
        recent.exists(),
        "stop at the soft goal without deleting a newer entry"
    );
    assert!(
        busy.exists(),
        "an in-progress publication is not an idle root"
    );
    assert_eq!(
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .last_eviction
            .as_ref()
            .unwrap()
            .path,
        old.to_string_lossy()
    );
}

#[test]
fn disk_guard_reclaims_interrupted_javascript_publications_after_lock_release() {
    for root in ["js-installs", "js-manager-caches"] {
        let f = Fixture::new();
        let pressure = Arc::new(AtomicBool::new(false));
        let mut config = f.config();
        config.free_space_floor_bytes = 100;
        let key = "d".repeat(64);
        let temporary = config
            .state_dir()
            .join(root)
            .join(format!("{key}.interrupted.tmp"));
        let probe_path = temporary.clone();
        let flag = pressure.clone();
        config.free_space_probe = Arc::new(move |_| {
            Ok(if flag.load(Ordering::SeqCst) && probe_path.exists() {
                50
            } else {
                300
            })
        });
        let state = config.state_dir();
        let runner = Runner::open(config).unwrap();
        fs::create_dir_all(&temporary).unwrap();
        let lock = platform::javascript_cache_lock(&state, &key).unwrap();
        lock.lock_exclusive().unwrap();
        pressure.store(true, Ordering::SeqCst);
        assert!(runner.ensure_disk_space().unwrap().is_some());
        assert!(temporary.exists(), "{root} must survive while locked");
        // A process another thread forks while the lock is held shares the
        // lock's open file until it execs. Releasing must not wait for it.
        #[cfg(unix)]
        let _forked = crate::test_helpers::fork_before_exec().unwrap();
        drop(lock);
        assert!(runner.ensure_disk_space().unwrap().is_none());
        assert!(!temporary.exists(), "{root} must be reclaimed after unlock");
    }
}

#[test]
fn disk_soft_pressure_evicts_old_javascript_entries_but_keeps_recent_ones() {
    let f = Fixture::new();
    let state = f.config().state_dir();
    let runner = Runner::open(config(&f, |_| Ok(150))).unwrap();
    let now = runner.now();
    let mut old = Vec::new();
    let mut recent = Vec::new();
    for root in ["js-installs", "js-manager-caches"] {
        for (key, age, entries) in [
            ("a".repeat(64), 31 * 60 * 1000, &mut old),
            ("b".repeat(64), 5 * 60 * 1000, &mut recent),
        ] {
            let path = state.join(root).join(key);
            fs::create_dir_all(&path).unwrap();
            File::open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_millis(now - age),
                ))
                .unwrap();
            entries.push(path);
        }
    }
    // Eviction checks whether each entry is in use by taking and releasing its
    // lock, as setup does. A process another thread forks while the lock is
    // held shares the lock's open file until it execs; a release must not wait
    // for that, or the entry would look busy and survive the pass.
    #[cfg(unix)]
    let _forked = {
        let lock = platform::javascript_cache_lock(&state, &"a".repeat(64)).unwrap();
        lock.lock_exclusive().unwrap();
        let forked = crate::test_helpers::fork_before_exec().unwrap();
        drop(lock);
        forked
    };
    assert!(runner.ensure_disk_space().unwrap().is_none());
    for path in old {
        assert!(
            !path.exists(),
            "old cache must be evicted: {}",
            path.display()
        );
    }
    for path in recent {
        assert!(
            path.exists(),
            "recent cache must survive: {}",
            path.display()
        );
    }
}

/// Jobs mount the shared compiler cache read-only, so its directory time
/// stays at the writer's last top-level change however many jobs read it.
/// Sets that time to two hours ago on the runner clock, with one entry inside.
fn stale_compiler_cache(runner: &Runner) -> PathBuf {
    let fleet = runner.config().sccache_dir();
    fs::create_dir_all(&fleet).unwrap();
    fs::write(fleet.join("entry"), "cached").unwrap();
    File::open(&fleet)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(
                UNIX_EPOCH + Duration::from_millis(runner.now() - 2 * 60 * 60 * 1000),
            ),
        )
        .unwrap();
    fleet
}

#[tokio::test]
async fn disk_soft_pressure_keeps_the_compiler_cache_a_job_finished_with_recently() {
    let f = Fixture::new();
    // Between the hard floor (100) and the soft goal (200).
    let mut c = config(&f, |_| Ok(150));
    c.disable_warm_builds = true;
    let runner = Runner::open(c).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let fleet = stale_compiler_cache(&runner);
    f.clock.fetch_add(5 * 60 * 1000, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(
        fleet.join("entry").exists(),
        "a cache read by a job five minutes ago was evicted as idle"
    );
    // Once no job has used it for over half an hour it is idle like any cache.
    f.clock.fetch_add(30 * 60 * 1000, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(fleet.is_dir());
    assert!(
        !fleet.join("entry").exists(),
        "an idle cache must still be reclaimed under soft pressure"
    );
}

#[tokio::test]
async fn disk_soft_pressure_keeps_the_compiler_cache_while_a_job_runs() {
    let f = Fixture::new();
    let mut c = config(&f, |_| Ok(150));
    c.disable_warm_builds = true;
    let runner = Runner::open(c).unwrap();
    // No job has finished since the runner started, as after a restart.
    let (request, bytes) = f.run("while [ ! -f release ]; do sleep 0.01; done");
    f.submit(&runner, &request, &bytes);
    wait_for(|| {
        runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .started
            .is_some()
    })
    .await;
    let fleet = stale_compiler_cache(&runner);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    let kept = fleet.join("entry").exists();
    fs::write(Path::new(&request.workspace_key).join("release"), "").unwrap();
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(kept, "a cache a running job reads was evicted as idle");
}

fn cargo_output(workspace: &Path, name: &str, read_ms: u64) -> PathBuf {
    let path = workspace.join("target").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, [5; 8192]).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_accessed(UNIX_EPOCH + Duration::from_millis(read_ms)))
        .unwrap();
    path
}

#[test]
fn disk_size_guard_drops_only_incremental_and_defers_queued_workspace() {
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/guard");
    let mut c = f.config();
    assert_eq!(c.build_folder_cap_bytes, 64 << 30);
    c.build_folder_cap_bytes = 1;
    let runner = Runner::open(c).unwrap();
    workspace(&runner, &idle, runner.now());
    let incremental = cargo_output(
        &idle,
        "debug/incremental/crate-session/state.bin",
        runner.now(),
    );
    let release = cargo_output(
        &idle,
        "release/incremental/crate-session/state.bin",
        runner.now(),
    );
    let dep = cargo_output(
        &idle,
        "debug/deps/libfoo-0123456789abcdef.rlib",
        runner.now(),
    );
    let (mut request, _) = f.run("true");
    request.workspace_key = text(&idle).into();
    request.cwd = text(&idle).into();
    let job = Job::new(request.job_id, Some(request.clone()), runner.now(), 1, 1);
    runner.core.inner.lock().unwrap().jobs.insert(job.id, job);
    runner.guard_build_folder(text(&idle));
    assert!(
        incremental.exists() && release.exists(),
        "queued workspace was trimmed"
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .remove(&request.job_id);
    runner.guard_build_folder(text(&idle));
    assert!(!incremental.exists() && !release.exists());
    assert!(dep.exists() && idle.join("target/artifact").exists());
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(text(&idle)));
}

#[test]
fn disk_size_guard_preserves_incremental_below_cap() {
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/small-target");
    let runner = Runner::open(f.config()).unwrap();
    workspace(&runner, &idle, runner.now());
    let incremental = cargo_output(&idle, "debug/incremental/foo/state.bin", runner.now());
    runner.guard_build_folder(text(&idle));
    assert!(incremental.exists());
}

#[test]
fn disk_stale_unit_maintenance_deletes_whole_units_and_runs_once_daily() {
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/stale-units");
    let mut c = f.config();
    // Only the explicit sweep may observe the day-long clock jump.
    c.manual_tick = true;
    let runner = Runner::open(c).unwrap();
    workspace(&runner, &idle, runner.now());
    let old = runner.now() - 49 * 60 * 60 * 1000;
    let dep = cargo_output(&idle, "debug/deps/libfoo-0123456789abcdef.rlib", old);
    let fingerprint = cargo_output(
        &idle,
        "debug/.fingerprint/foo-0123456789abcdef/lib-foo",
        old,
    );
    let build = cargo_output(&idle, "debug/build/foo-0123456789abcdef/out/generated", old);
    let recent = cargo_output(
        &idle,
        "debug/deps/libfoo-fedcba9876543210.rlib",
        runner.now(),
    );
    let unrelated = cargo_output(&idle, "debug/deps/unknown.txt", old);
    let outside = f.scratch.join("outside-gc");
    fs::write(&outside, "keep").unwrap();
    let link = idle.join("target/debug/deps/linked-1111111111111111");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    runner.maintain_build_folders();
    assert!(!dep.exists() && !fingerprint.exists() && !build.exists());
    assert!(recent.exists() && unrelated.exists() && outside.exists());
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    let late = cargo_output(&idle, "debug/deps/late-0123456789abcdef", old);
    runner.maintain_build_folders();
    assert!(late.exists(), "daily limit was ignored");
    f.clock.fetch_add(24 * 60 * 60 * 1000, Ordering::SeqCst);
    runner.maintain_build_folders();
    assert!(!late.exists());
    assert!(idle.exists());
}

#[test]
fn disk_pressure_stale_pass_reaches_goal_without_dropping_incremental() {
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/stale-first");
    let dep = idle.join("target/debug/deps/libfoo-0123456789abcdef.rlib");
    let probe = dep.clone();
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && probe.exists() {
            150
        } else {
            200
        })
    }))
    .unwrap();
    workspace(&runner, &idle, runner.now() - 60 * 60 * 1000);
    cargo_output(
        &idle,
        "debug/deps/libfoo-0123456789abcdef.rlib",
        runner.now() - 49 * 60 * 60 * 1000,
    );
    let incremental = cargo_output(&idle, "debug/incremental/foo/state.bin", runner.now());
    // A pressure pass bypasses the periodic daily limit.
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .build_swept_ms
        .insert(text(&idle).into(), runner.now());
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!dep.exists());
    assert!(incremental.exists() && idle.exists());
    assert!(runner.runner_status().disk.unwrap().last_eviction.is_none());
}

#[test]
fn disk_pressure_incremental_pass_prefers_largest_and_retains_workspaces() {
    let f = Fixture::new();
    let small = f.scratch.join("remote/worktrees/small-incremental");
    let large = f.scratch.join("remote/worktrees/large-incremental");
    let probe = large.join("target/debug/incremental");
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && probe.exists() {
            150
        } else {
            200
        })
    }))
    .unwrap();
    for path in [&small, &large] {
        workspace(&runner, path, runner.now() - 60 * 60 * 1000);
        cargo_output(path, "debug/incremental/foo/state.bin", runner.now());
    }
    fs::write(
        large.join("target/debug/incremental/foo/large.bin"),
        vec![9; 1024 * 1024],
    )
    .unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!large.join("target/debug/incremental").exists());
    assert!(small.join("target/debug/incremental").exists());
    assert!(small.join("target/artifact").exists() && large.join("target/artifact").exists());
}

#[test]
fn disk_eviction_bytes_times_idle_prefers_huge_workspace_over_older_small_ones() {
    let f = Fixture::new();
    let huge = f.scratch.join("remote/worktrees/huge");
    let probe = huge.clone();
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let runner = Runner::open(config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && probe.exists() {
            150
        } else {
            200
        })
    }))
    .unwrap();
    workspace(&runner, &huge, runner.now() - 60 * 60 * 1000);
    fs::write(huge.join("target/large.bin"), vec![8; 2 * 1024 * 1024]).unwrap();
    let small: Vec<_> = (0..3)
        .map(|n| f.scratch.join(format!("remote/worktrees/small-{n}")))
        .collect();
    for path in &small {
        workspace(&runner, path, runner.now() - 2 * 60 * 60 * 1000);
    }
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!huge.exists());
    assert!(small.iter().all(|path| path.exists()));
}

#[test]
fn disk_build_folder_measurement_and_ranking_never_claim_or_repair() {
    let f = Fixture::new();
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let core = Arc::new(Mutex::new(std::sync::Weak::<Core>::new()));
    let measuring = core.clone();
    let mut c = config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) {
            150
        } else {
            200
        })
    });
    // A background tick must not hold the state lock during the measurement probe.
    c.manual_tick = true;
    c.long_work = Some(Arc::new(move |label| {
        if label == "build_folder_measure" {
            let core = measuring.lock().unwrap().upgrade().unwrap();
            let inner = core.inner.try_lock().expect("measurement held state lock");
            assert!(inner.dropping.is_empty());
        }
        if label.starts_with("build_folder_") {
            observed.lock().unwrap().push(label);
        }
    }));
    let runner = Runner::open(c).unwrap();
    *core.lock().unwrap() = Arc::downgrade(&runner.core);
    let idle = f.scratch.join("remote/worktrees/measured");
    workspace(&runner, &idle, runner.now());
    let incremental = cargo_output(&idle, "debug/incremental/foo/state.bin", runner.now());
    runner.guard_build_folder(text(&idle));
    assert!(runner.core.inner.lock().unwrap().dropping.is_empty());
    assert_eq!(*calls.lock().unwrap(), ["build_folder_measure"]);

    let lazy = f.scratch.join("remote/worktrees/lazy-measurement");
    workspace(&runner, &lazy, runner.now());
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(runner.core.inner.lock().unwrap().dropping.is_empty());
    assert_eq!(
        *calls.lock().unwrap(),
        ["build_folder_measure", "build_folder_measure"]
    );
    assert!(incremental.exists() && lazy.exists());
}

#[test]
fn disk_eviction_ranks_cached_workspace_sizes_without_another_walk() {
    let f = Fixture::new();
    let large = f.scratch.join("remote/worktrees/cached-large");
    let small = f.scratch.join("remote/worktrees/cached-small");
    let probe = large.clone();
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let measurements = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = measurements.clone();
    let mut c = config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && probe.exists() {
            150
        } else {
            200
        })
    });
    // Only explicit guard and pressure calls may contribute measurement counts.
    c.manual_tick = true;
    c.long_work = Some(Arc::new(move |label| {
        if label == "build_folder_measure" {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let runner = Runner::open(c).unwrap();
    for path in [&large, &small] {
        workspace(&runner, path, runner.now() - 60 * 60 * 1000);
    }
    fs::write(large.join("target/large.bin"), vec![8; 2 * 1024 * 1024]).unwrap();
    runner.guard_build_folder(text(&large));
    runner.guard_build_folder(text(&small));
    assert_eq!(measurements.load(Ordering::SeqCst), 2);
    // Reverse the physical sizes after measuring. Ranking must use the cached
    // estimates rather than walking the trees again under disk pressure.
    fs::write(large.join("target/large.bin"), []).unwrap();
    fs::write(small.join("target/larger.bin"), vec![9; 4 * 1024 * 1024]).unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!large.exists() && small.exists());
    assert_eq!(measurements.load(Ordering::SeqCst), 2);
}

#[test]
fn disk_build_folder_incremental_ranking_uses_cached_sizes() {
    let f = Fixture::new();
    let large = f.scratch.join("remote/worktrees/cached-large-incremental");
    let small = f.scratch.join("remote/worktrees/cached-small-incremental");
    let probe = large.join("target/debug/incremental");
    let pressure = Arc::new(AtomicBool::new(false));
    let active = pressure.clone();
    let measurements = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = measurements.clone();
    let mut c = config(&f, move |_| {
        Ok(if active.load(Ordering::SeqCst) && probe.exists() {
            150
        } else {
            200
        })
    });
    // Only explicit guard and pressure calls may contribute measurement counts.
    c.manual_tick = true;
    c.long_work = Some(Arc::new(move |label| {
        if label == "build_folder_measure" {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let runner = Runner::open(c).unwrap();
    for path in [&large, &small] {
        workspace(&runner, path, runner.now() - 60 * 60 * 1000);
        cargo_output(path, "debug/incremental/foo/state.bin", runner.now());
    }
    let large_state = large.join("target/debug/incremental/foo/state.bin");
    let small_state = small.join("target/debug/incremental/foo/state.bin");
    fs::write(&large_state, vec![8; 2 * 1024 * 1024]).unwrap();
    runner.guard_build_folder(text(&large));
    runner.guard_build_folder(text(&small));
    assert_eq!(measurements.load(Ordering::SeqCst), 2);
    fs::write(&large_state, []).unwrap();
    fs::write(&small_state, vec![9; 4 * 1024 * 1024]).unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!large_state.exists() && small_state.exists());
    assert!(large.exists() && small.exists());
    assert_eq!(measurements.load(Ordering::SeqCst), 3);
}

#[test]
fn disk_build_folder_sweep_with_nothing_stale_never_claims_or_repairs() {
    let f = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let mut c = f.config();
    c.long_work = Some(Arc::new(move |label| {
        if matches!(label, "build_folder_claim" | "build_folder_repair") {
            observed.lock().unwrap().push(label);
        }
    }));
    let runner = Runner::open(c).unwrap();
    let idle = f.scratch.join("remote/worktrees/fresh-units");
    workspace(&runner, &idle, runner.now());
    let dep = cargo_output(
        &idle,
        "debug/deps/libfoo-0123456789abcdef.rlib",
        runner.now(),
    );
    let no_target = f.scratch.join("remote/worktrees/no-target");
    workspace(&runner, &no_target, runner.now());
    fs::remove_dir_all(no_target.join("target")).unwrap();
    runner.guard_build_folder(text(&no_target));
    runner.maintain_build_folders();
    assert!(calls.lock().unwrap().is_empty());
    assert!(runner.core.inner.lock().unwrap().dropping.is_empty());
    assert!(dep.exists() && no_target.exists());
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .build_swept_ms
        .contains_key(text(&idle)));
}

#[test]
fn disk_build_folder_incomplete_measurement_falls_back_to_repair() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        eprintln!("requires an unprivileged user so mode 000 denies directory reads");
        return;
    }
    let f = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let mut c = f.config();
    // Only the explicit guard may attempt repair of the unreadable fixture.
    c.manual_tick = true;
    c.build_folder_cap_bytes = 1;
    c.long_work = Some(Arc::new(move |label| {
        if label.starts_with("build_folder_") {
            observed.lock().unwrap().push(label);
        }
    }));
    let runner = Runner::open(c).unwrap();
    let idle = f.scratch.join("remote/worktrees/unreadable-target");
    workspace(&runner, &idle, runner.now());
    let incremental = cargo_output(&idle, "debug/incremental/foo/state.bin", runner.now());
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .build_guard_pending
        .insert(text(&idle).into());
    let target = idle.join("target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        fs::read_dir(&target).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    runner.guard_build_folder(text(&idle));
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();

    // A non-isolated runner has no privileged repair. It must try the fallback
    // and retain the pending guard, not accept the partial size as under cap.
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "build_folder_measure",
            "build_folder_claim",
            "build_folder_repair",
            "build_folder_measure"
        ]
    );
    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.build_folder_sizes.get(text(&idle)).unwrap().complete);
    assert!(inner.build_guard_pending.contains(text(&idle)));
    assert!(inner.dropping.is_empty());
    assert!(fs::symlink_metadata(incremental).unwrap().is_file());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires trusted host sudo and motor-job account; see motor-runner README"]
fn isolated_build_folder_reclaims_job_owned_units() {
    let f = Fixture::new();
    let idle = f.scratch.join("remote/worktrees/job-owned-build");
    let mut c = f.config();
    // Keep background sweeps out of the fixture's ownership and ACL changes.
    c.manual_tick = true;
    c.isolate_jobs = true;
    c.build_folder_cap_bytes = 1;
    let runner = Runner::open(c).unwrap();
    workspace(&runner, &idle, runner.now());
    let old = runner.now() - 49 * 60 * 60 * 1000;
    let dep = cargo_output(&idle, "debug/deps/libfoo-0123456789abcdef.rlib", old);
    cargo_output(
        &idle,
        "debug/.fingerprint/foo-0123456789abcdef/lib-foo",
        old,
    );
    let incremental = cargo_output(&idle, "debug/incremental/foo/state.bin", runner.now());
    let outside = f.scratch.join("outside-owned-gc");
    fs::write(&outside, "keep").unwrap();
    let link = idle.join("target/debug/deps/linked-1111111111111111");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(Command::new("sudo")
        .args(["-n", "chown", "-R", "-P", "motor-job:motor-job", "--"])
        .arg(idle.join("target"))
        .status()
        .unwrap()
        .success());
    assert!(Command::new("sudo")
        .args(["-n", "chmod", "-R", "1700", "--"])
        .arg(idle.join("target"))
        .status()
        .unwrap()
        .success());
    // Narrow the inherited ACL mask just as a job's chmod does.
    assert!(Command::new("sudo")
        .args(["-n", "setfacl", "-R", "-P", "-m", "m::---", "--"])
        .arg(idle.join("target"))
        .status()
        .unwrap()
        .success());
    runner.guard_build_folder(text(&idle));
    assert_eq!(
        fs::symlink_metadata(&incremental).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(fs::symlink_metadata(&dep).unwrap().is_file());
    runner.maintain_build_folders();
    assert_eq!(
        fs::symlink_metadata(&dep).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(outside.exists() && fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert!(idle.join("target/artifact").exists());
}
