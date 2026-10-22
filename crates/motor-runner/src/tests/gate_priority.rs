use super::*;

fn job(request: &Run, key: &str, weight: u32, order: u64, gate: bool, now: u64) -> Job {
    let mut request = request.clone();
    request.job_id = Uuid::now_v7();
    request.workspace_key = key.into();
    request.cwd = key.into();
    request.weight_hint = Some(weight);
    request.gate_job = gate;
    request.queue_wait_limit_s = if gate { 1800 } else { 300 };
    let mut job = Job::new(request.job_id, Some(request), now, order, weight);
    job.state = JobState::Queued;
    job.queued_ms = Some(now);
    job
}

fn runner(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.max_weight = 64;
    config.cpu_topology = Some(CpuTopology::new((0..65).map(|cpu| vec![cpu]).collect()).unwrap());
    config.disable_warm_builds = true;
    // The sibling fences below exist only while siblings are checked out in
    // place at their host paths.
    config.sibling_snapshot_mode_override = Some(false);
    Runner::open(config).unwrap()
}

fn holders(inner: &mut Inner, request: &Run, now: u64) -> [Uuid; 3] {
    let mut first_cpu = 1;
    [56, 6, 2].map(|weight| {
        let mut held = job(request, &format!("holder-{weight}"), weight, 0, false, now);
        held.state = JobState::Running;
        held.started_ms = Some(now);
        held.cpus = (first_cpu..first_cpu + weight).collect();
        first_cpu += weight;
        let id = held.id;
        inner.jobs.insert(id, held);
        id
    })
}

#[test]
fn gate_gets_next_eight_permits_before_older_workers_and_later_gates() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let held = holders(&mut inner, &request, now);
    let worker = job(&request, &f.key, 2, 1, false, now - BACKFILL_GRACE_MS);
    let gate = job(&request, &f.key, 8, 2, true, now);
    let later_gate = job(&request, "later-gate", 2, 3, true, now);
    let ids = [worker.id, gate.id, later_gate.id];
    for job in [worker, gate, later_gate] {
        inner.jobs.insert(job.id, job);
    }
    runner.dispatch(&mut inner, now).unwrap();
    assert!(inner.starting.is_empty(), "64/64 permits are held");
    inner.jobs.remove(&held[2]);
    runner.dispatch(&mut inner, now).unwrap();
    assert!(
        inner.starting.is_empty(),
        "neither a worker nor a later gate may backfill the first gate"
    );
    inner.jobs.remove(&held[1]);
    runner.dispatch(&mut inner, now).unwrap();
    assert_eq!(inner.starting, BTreeSet::from([ids[1]]));
    assert_eq!(inner.jobs[&ids[1]].cpus.len(), 8);
    assert!(inner.jobs[&ids[0]].setup_ms.is_none());
    assert!(inner.jobs[&ids[2]].setup_ms.is_none());
    inner.jobs.remove(&held[0]);
}

#[test]
fn waiting_gate_does_not_stop_or_repin_running_workers() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let held = holders(&mut inner, &request, now);
    let before: Vec<_> = held
        .iter()
        .map(|id| serde_json::to_value(&inner.jobs[id]).unwrap())
        .collect();
    let gate = job(&request, &f.key, 8, 1, true, now);
    inner.jobs.insert(gate.id, gate);
    runner.dispatch(&mut inner, now).unwrap();
    let after: Vec<_> = held
        .iter()
        .map(|id| serde_json::to_value(&inner.jobs[id]).unwrap())
        .collect();
    assert_eq!(before, after, "priority must change only queued admission");
    for id in held {
        inner.jobs.remove(&id);
    }
}

#[test]
fn worker_behind_gate_keeps_its_full_queue_wait_budget() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let (held, worker_id, gate_id) = {
        let mut inner = runner.core.inner.lock().unwrap();
        let held = holders(&mut inner, &request, now);
        let worker = job(&request, "worker", 2, 1, false, now);
        let gate = job(&request, &f.key, 8, 2, true, now);
        let ids = (worker.id, gate.id);
        inner.jobs.insert(worker.id, worker);
        inner.jobs.insert(gate.id, gate);
        runner.dispatch(&mut inner, now).unwrap();
        (held, ids.0, ids.1)
    };
    f.clock.store(now + 299_999, Ordering::SeqCst);
    assert_eq!(
        runner.withdraw(worker_id).unwrap(),
        WithdrawOutcome::Started
    );
    assert!(runner.core.inner.lock().unwrap().jobs[&worker_id]
        .terminal
        .is_none());
    f.clock.store(now + 300_000, Ordering::SeqCst);
    assert_eq!(
        runner.withdraw(worker_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    assert_eq!(runner.withdraw(gate_id).unwrap(), WithdrawOutcome::Started);
    let mut inner = runner.core.inner.lock().unwrap();
    assert!(inner.jobs[&gate_id].terminal.is_none());
    for id in held {
        inner.jobs.remove(&id);
    }
}

#[test]
fn without_a_gate_all_permits_remain_available_to_workers() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let worker = job(&request, &f.key, 64, 1, false, now);
    let id = worker.id;
    inner.jobs.insert(id, worker);
    runner.dispatch(&mut inner, now).unwrap();
    assert!(inner.starting.contains(&id));
    assert_eq!(inner.jobs[&id].cpus.len(), 64);
}

#[tokio::test]
async fn gate_mark_is_honored_only_after_capability_negotiation() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (socket, server) = listener(&f, &runner);
    for capable in [false, true] {
        let mut stream = UnixStream::connect(&*socket).await.unwrap();
        write_frame_async(
            &mut stream,
            &Frame::Hello {
                return_paths: false,
                return_chunked: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: false,
                gate_jobs: capable,
                platforms: Vec::new(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_next(&mut stream).await, Frame::Hello { gate_jobs, .. } if gate_jobs == capable)
        );
        let (mut request, _) = f.run("true");
        request.gate_job = true;
        write_frame_async(&mut stream, &Frame::Run(request.clone()))
            .await
            .unwrap();
        assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
        assert_eq!(
            runner.core.inner.lock().unwrap().jobs[&request.job_id].gate_job(),
            capable
        );
        runner.cancel(request.job_id).unwrap();
    }
    server.abort();
}

#[test]
fn workspace_conflicted_gate_does_not_block_independent_workers() {
    for conflict in ["occupied", "deleting", "sibling"] {
        let f = Fixture::new();
        let runner = runner(&f);
        let (request, _) = f.run("true");
        let now = runner.now();
        let mut inner = runner.core.inner.lock().unwrap();
        let held = holders(&mut inner, &request, now);
        inner.jobs.remove(&held[1]);
        inner.jobs.remove(&held[2]);
        let gate_key = if conflict == "occupied" {
            "holder-56"
        } else {
            &f.key
        };
        let mut gate = job(&request, gate_key, 8, 1, true, now);
        gate.request.as_mut().unwrap().siblings.clear();
        if conflict == "deleting" {
            inner.dropping.insert(gate_key.to_owned());
        } else if conflict == "sibling" {
            let mut sibling = request.siblings[0].clone();
            sibling.canonical_path = "holder-56".into();
            gate.request.as_mut().unwrap().siblings.push(sibling);
        }
        let worker_key = f.scratch.join("remote/worktrees/independent");
        let mut worker = job(&request, text(&worker_key), 2, 2, false, now);
        worker.request.as_mut().unwrap().siblings.clear();
        let gate_id = gate.id;
        let worker_id = worker.id;
        inner.jobs.insert(gate_id, gate);
        inner.jobs.insert(worker_id, worker);
        runner.dispatch(&mut inner, now).unwrap();
        let started = inner.starting.clone();
        let gate_waiting = inner.jobs[&gate_id].setup_ms.is_none();
        inner.jobs.remove(&held[0]);
        inner.dropping.clear();
        drop(inner);
        assert_eq!(
            started,
            BTreeSet::from([worker_id]),
            "{conflict} must not fence unrelated workspaces"
        );
        assert!(gate_waiting, "the conflicting gate must remain queued");
    }
}

#[test]
fn capacity_blocked_gate_still_fences_smaller_workers() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let held = holders(&mut inner, &request, now);
    inner.jobs.remove(&held[2]);
    let mut gate = job(&request, &f.key, 8, 2, true, now);
    gate.request.as_mut().unwrap().siblings.clear();
    let worker_key = f.scratch.join("remote/worktrees/independent");
    let mut worker = job(&request, text(&worker_key), 2, 1, false, now);
    worker.request.as_mut().unwrap().siblings.clear();
    inner.jobs.insert(gate.id, gate);
    inner.jobs.insert(worker.id, worker);
    runner.dispatch(&mut inner, now).unwrap();
    let started = inner.starting.clone();
    inner.jobs.remove(&held[0]);
    inner.jobs.remove(&held[1]);
    drop(inner);
    assert!(
        started.is_empty(),
        "the gate must reserve the next freed vCPUs from its first second"
    );
}

#[test]
fn large_retained_table_and_blocked_heads_use_a_bounded_number_of_scheduler_scans() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    for number in 1..=10_000u128 {
        let mut retained = Job::new(Uuid::from_u128(number), None, now, 0, 0);
        retained.terminal(Outcome::Exit { code: 0 }, now);
        inner.jobs.insert(retained.id, retained);
    }
    let mut holder = job(&request, &f.canonical, 2, 0, false, now);
    holder.request.as_mut().unwrap().siblings.clear();
    holder.state = JobState::Running;
    holder.started_ms = Some(now);
    holder.cpus = vec![1, 2];
    let holder_id = holder.id;
    inner.jobs.insert(holder_id, holder);
    let mut blocked = Vec::new();
    for number in 0..40 {
        let waiting = job(
            &request,
            &format!("{}-{number}", f.key),
            2,
            number + 1,
            true,
            now,
        );
        blocked.push(waiting.id);
        inner.jobs.insert(waiting.id, waiting);
    }
    let mut independent = job(&request, &f.key, 2, 100, false, now);
    independent.request.as_mut().unwrap().siblings.clear();
    let selected = independent.id;
    inner.jobs.insert(selected, independent);
    inner.scheduler_visits.store(0, Ordering::SeqCst);
    runner.dispatch(&mut inner, now).unwrap();
    let visits = inner.scheduler_visits.load(Ordering::SeqCst);
    let starts = inner.starting.clone();
    let cpus = inner.jobs[&selected].cpus.clone();
    let blocked_stayed_queued = blocked.iter().all(|id| inner.jobs[id].setup_ms.is_none());
    for id in blocked {
        inner.jobs.remove(&id);
    }
    inner.jobs.remove(&holder_id);
    drop(inner);
    assert_eq!(starts, BTreeSet::from([selected]));
    assert_eq!(cpus, vec![3, 4]);
    assert!(blocked_stayed_queued);
    assert!(
        visits <= 3 * 10_042,
        "blocked heads multiplied retained-table scans: {visits}"
    );
}

#[test]
fn queue_wait_clock_updates_scan_a_large_retained_table_once() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 64;
    config.disable_warm_builds = true;
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    for number in 1..=10_000u128 {
        let mut retained = Job::new(Uuid::from_u128(number), None, now, 0, 0);
        retained.terminal(Outcome::Exit { code: 0 }, now);
        inner.jobs.insert(retained.id, retained);
    }
    let mut owner = job(&request, &f.key, 2, 0, false, now);
    owner.request.as_mut().unwrap().siblings.clear();
    owner.state = JobState::Running;
    owner.started_ms = Some(now);
    let owner_id = owner.id;
    inner.jobs.insert(owner_id, owner);
    let mut waiting = job(&request, &f.key, 2, 1, false, now);
    waiting.request.as_mut().unwrap().siblings.clear();
    waiting.start_queue_wait(now);
    let waiting_id = waiting.id;
    inner.jobs.insert(waiting_id, waiting);

    inner.scheduler_visits.store(0, Ordering::SeqCst);
    runner.dispatch(&mut inner, now).unwrap();
    let visits = inner.scheduler_visits.load(Ordering::SeqCst);
    assert_eq!(
        inner.jobs[&waiting_id].queue_entry.load(Ordering::Acquire),
        PAUSED_QUEUE
    );
    assert!(!inner.starting.contains(&waiting_id));
    assert!(
        visits <= 3 * 10_002,
        "queue clock update multiplied retained-table scans: {visits}"
    );
}

#[test]
fn dispatch_indexes_preserve_running_and_reserved_sibling_conflicts() {
    let f = Fixture::new();
    let runner = runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let mut active = job(&request, "/active/workspace", 2, 1, false, now);
    active.state = JobState::Running;
    active.started_ms = Some(now);
    active.request.as_mut().unwrap().siblings = vec![SiblingSnapshot {
        canonical_path: "/shared/running".into(),
        commit: "old".into(),
        tree_hash: String::new(),
        base_commit: None,
    }];
    let mut base = job(&request, "base:/repository", 4, 2, false, now);
    base.base = true;
    base.state = JobState::Running;
    base.started_ms = Some(now);
    base.request.as_mut().unwrap().cwd = "/derived/fill".into();
    base.request.as_mut().unwrap().siblings.clear();
    let ids = [active.id, base.id];
    inner.jobs.insert(active.id, active);
    inner.jobs.insert(base.id, base);
    let reserved = SiblingSnapshot {
        canonical_path: "/shared/reserved".into(),
        commit: "new".into(),
        tree_hash: String::new(),
        base_commit: None,
    };
    inner.reserved.insert(
        "/setup/workspace".into(),
        Reservation {
            repository: String::new(),
            commit: String::new(),
            siblings: vec![reserved.clone()],
            since_ms: now,
        },
    );
    let index = DispatchIndex::build(&inner);
    let mut results = Vec::new();
    for key in [
        "/active/workspace",
        "/setup/workspace",
        "/shared/running/child",
        "/shared/reserved",
        "/free",
    ] {
        results.push((index.occupied(key), Runner::occupied(&inner, key)));
    }
    for (path, commit) in [
        ("/shared/running", "old"),
        ("/shared/running", "different"),
        ("/derived/fill/child", "old"),
        ("/shared/reserved", "new"),
        ("/free", "new"),
    ] {
        let siblings = vec![SiblingSnapshot {
            canonical_path: path.into(),
            commit: commit.into(),
            tree_hash: String::new(),
            base_commit: None,
        }];
        results.push((
            index.siblings_blocked(&siblings, &inner.catalog.sibling_commits),
            Runner::siblings_blocked(&inner, &siblings),
        ));
    }
    let reserved_before_materialization = index.siblings_blocked(
        std::slice::from_ref(&reserved),
        &inner.catalog.sibling_commits,
    );
    inner
        .catalog
        .sibling_commits
        .insert(reserved.canonical_path.clone(), reserved.commit.clone());
    let reserved_after_materialization =
        index.siblings_blocked(&[reserved], &inner.catalog.sibling_commits);
    for id in ids {
        inner.jobs.remove(&id);
    }
    inner.reserved.clear();
    drop(inner);
    assert!(
        results
            .iter()
            .all(|(indexed, established)| indexed == established),
        "indexed ownership differs from established overlap/commit rules: {results:?}"
    );
    assert!(reserved_before_materialization);
    assert!(!reserved_after_materialization);
}

#[test]
fn dispatch_indexes_add_each_starting_weight_and_cpu_claim_before_selecting_next() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_weight = 4;
    config.disable_warm_builds = true;
    config.cpu_topology = Some(CpuTopology::new((0..65).map(|cpu| vec![cpu]).collect()).unwrap());
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let mut ids = Vec::new();
    for order in 1..=3 {
        let mut queued = job(
            &request,
            &format!("{}-{order}", f.key),
            2,
            order,
            false,
            now,
        );
        queued.request.as_mut().unwrap().siblings.clear();
        ids.push(queued.id);
        inner.jobs.insert(queued.id, queued);
    }
    runner.dispatch(&mut inner, now).unwrap();
    let starts = inner.starting.clone();
    let claims = [
        inner.jobs[&ids[0]].cpus.clone(),
        inner.jobs[&ids[1]].cpus.clone(),
    ];
    let third_waiting = inner.jobs[&ids[2]].setup_ms.is_none();
    inner.jobs.remove(&ids[2]);
    drop(inner);
    assert_eq!(starts, BTreeSet::from([ids[0], ids[1]]));
    assert_eq!(claims, [vec![1, 2], vec![3, 4]]);
    assert!(
        third_waiting,
        "new setup reservations must consume capacity immediately"
    );
}

/// A runner whose scheduler runs only when a test calls `dispatch`, so a test
/// can step admission and read queue clocks under one lock without a
/// background tick starting jobs in between.
fn manual_runner(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.max_weight = 64;
    config.cpu_topology = Some(CpuTopology::new((0..65).map(|cpu| vec![cpu]).collect()).unwrap());
    config.disable_warm_builds = true;
    config.manual_tick = true;
    // The sibling fences below exist only while siblings are checked out in
    // place at their host paths.
    config.sibling_snapshot_mode_override = Some(false);
    Runner::open(config).unwrap()
}

/// A job in another repository that declares the fixture's sibling, whose
/// canonical path is also `workspace_job`'s main workspace.
fn sibling_user(f: &Fixture, request: &Run, name: &str, order: u64, now: u64) -> Job {
    let user = job(request, &format!("{}-{name}", f.key), 2, order, false, now);
    assert_eq!(
        user.request.as_ref().unwrap().siblings[0].canonical_path,
        f.canonical
    );
    user
}

/// A job whose main workspace is the fixture sibling's canonical path, as a
/// direct run in that repository is: the sibling checkout and this workspace
/// are the same host directory.
fn workspace_job(f: &Fixture, request: &Run, order: u64, gate: bool, now: u64) -> Job {
    let mut main = job(request, &f.canonical, 2, order, gate, now);
    main.request.as_mut().unwrap().siblings.clear();
    main
}

fn run_now(inner: &mut Inner, id: Uuid, now: u64) {
    let key = inner.jobs[&id].key().to_owned();
    inner.starting.remove(&id);
    inner.reserved.remove(&key);
    let job = inner.jobs.get_mut(&id).unwrap();
    job.state = JobState::Running;
    job.started_ms = Some(now);
}

#[test]
fn overlapping_sibling_users_cannot_starve_a_queued_job_on_their_checkout() {
    let f = Fixture::new();
    let runner = manual_runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let mut first = sibling_user(&f, &request, "user-0", 1, now);
    first.state = JobState::Running;
    first.started_ms = Some(now);
    let mut running = std::collections::VecDeque::from([first.id]);
    inner.jobs.insert(first.id, first);
    let main = workspace_job(&f, &request, 2, true, now);
    let main_id = main.id;
    inner.jobs.insert(main_id, main);

    // Each round a new sibling user arrives while the older ones still run,
    // and then the oldest running one finishes. Without a hold, a newer
    // sibling user has always started before the older one finishes, so the
    // checkout is never free and the workspace job never starts.
    let mut later_users = Vec::new();
    let mut later_started = Vec::new();
    let mut main_started_in_round = None;
    for round in 1..=6u64 {
        let user = sibling_user(&f, &request, &format!("user-{round}"), 2 + round, now);
        later_users.push(user.id);
        inner.jobs.insert(user.id, user);
        runner.dispatch(&mut inner, now).unwrap();
        if inner.starting.contains(&main_id) {
            main_started_in_round = Some(round);
            break;
        }
        let started: Vec<_> = later_users
            .iter()
            .copied()
            .filter(|id| inner.starting.contains(id))
            .collect();
        for id in started {
            run_now(&mut inner, id, now);
            running.push_back(id);
            later_started.push(id);
        }
        if let Some(oldest) = running.pop_front() {
            inner.jobs.remove(&oldest);
        }
    }
    for id in running {
        inner.jobs.remove(&id);
    }
    later_started.extend(
        later_users
            .iter()
            .copied()
            .filter(|id| inner.starting.contains(id)),
    );
    let phase = runner
        .queue_wait_phase(&inner, &DispatchIndex::build(&inner), later_users[0])
        .map(str::to_owned);
    drop(inner);
    assert_eq!(
        main_started_in_round,
        Some(2),
        "the workspace job must start once the sibling user that arrived before it finishes"
    );
    assert!(
        later_started.is_empty(),
        "sibling users that arrived after the workspace job must wait for it: {later_started:?}"
    );
    assert_eq!(phase.as_deref(), Some("sibling checkout owner"));
}

#[test]
fn held_sibling_user_queue_wait_pauses_only_while_held() {
    let f = Fixture::new();
    let runner = manual_runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let mut earlier = sibling_user(&f, &request, "earlier", 1, now);
    earlier.state = JobState::Running;
    earlier.started_ms = Some(now);
    let earlier_id = earlier.id;
    inner.jobs.insert(earlier_id, earlier);
    let main = workspace_job(&f, &request, 2, false, now);
    let main_id = main.id;
    inner.jobs.insert(main_id, main);
    let held = sibling_user(&f, &request, "held", 3, now);
    let held_id = held.id;
    inner.jobs.insert(held_id, held);

    runner.dispatch(&mut inner, now).unwrap();
    let held_phase = runner
        .queue_wait_phase(&inner, &DispatchIndex::build(&inner), held_id)
        .map(str::to_owned);
    let held_entry = inner.jobs[&held_id].queue_entry.load(Ordering::Acquire);
    let held_wait = inner.jobs[&held_id].queue_wait_ms(now + 100_000);

    // The workspace job goes away and the earlier user now fills the runner,
    // so only capacity blocks the formerly held job: its clock must resume.
    inner.jobs.remove(&main_id);
    inner.jobs.get_mut(&earlier_id).unwrap().weight = 63;
    runner.dispatch(&mut inner, now + 100_000).unwrap();
    let capacity_phase = runner
        .queue_wait_phase(&inner, &DispatchIndex::build(&inner), held_id)
        .map(str::to_owned);
    let resumed_wait = inner.jobs[&held_id].queue_wait_ms(now + 100_400);
    let started = inner.jobs[&held_id].setup_ms.is_some();
    inner.jobs.remove(&earlier_id);
    drop(inner);

    assert_eq!(held_phase.as_deref(), Some("queued workspace job"));
    assert_eq!(held_entry, PAUSED_QUEUE);
    assert_eq!(
        held_wait, 0,
        "time held for the workspace job is not queue wait"
    );
    assert!(!started);
    assert_eq!(capacity_phase.as_deref(), Some("permits"));
    assert_eq!(resumed_wait, 400, "the clock resumes once the hold ends");
}

#[test]
fn repositories_naming_each_other_as_siblings_never_hold_each_other() {
    for first_is_x in [true, false] {
        let f = Fixture::new();
        let runner = manual_runner(&f);
        let (request, _) = f.run("true");
        let now = runner.now();
        let x_key = format!("{}-x", f.key);
        let y_key = format!("{}-y", f.key);
        let mut inner = runner.core.inner.lock().unwrap();
        let mut ids = Vec::new();
        for (key, other, order) in [
            (&x_key, &y_key, if first_is_x { 1 } else { 2 }),
            (&y_key, &x_key, if first_is_x { 2 } else { 1 }),
        ] {
            let mut queued = job(&request, key, 2, order, false, now);
            queued.request.as_mut().unwrap().siblings[0].canonical_path = other.clone();
            ids.push(queued.id);
            inner.jobs.insert(queued.id, queued);
        }
        runner.dispatch(&mut inner, now).unwrap();
        let started = inner.starting.clone();
        drop(inner);
        let earlier = if first_is_x { ids[0] } else { ids[1] };
        assert_eq!(
            started,
            BTreeSet::from([earlier]),
            "the earlier of two mutually dependent jobs must start"
        );
    }
}

#[test]
fn sibling_user_accepted_before_the_workspace_job_starts_first() {
    let f = Fixture::new();
    let runner = manual_runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let user = sibling_user(&f, &request, "earlier", 1, now);
    let user_id = user.id;
    inner.jobs.insert(user_id, user);
    let main = workspace_job(&f, &request, 2, false, now);
    let main_id = main.id;
    inner.jobs.insert(main_id, main);
    runner.dispatch(&mut inner, now).unwrap();
    let started = inner.starting.clone();
    let main_phase = runner
        .queue_wait_phase(&inner, &DispatchIndex::build(&inner), main_id)
        .map(str::to_owned);
    drop(inner);
    assert_eq!(started, BTreeSet::from([user_id]));
    assert_eq!(main_phase.as_deref(), Some("sibling user of this checkout"));
}

/// A workspace job whose withdrawal is still being saved stays in the queue
/// as an ordering fence until the withdrawal is published: it does not start,
/// and it still holds back the sibling users accepted after it, even one that
/// dispatch reaches before it. Once the withdrawal is published, they start.
#[test]
fn workspace_job_being_withdrawn_still_holds_later_sibling_users() {
    let f = Fixture::new();
    let runner = manual_runner(&f);
    let (request, _) = f.run("true");
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let main = workspace_job(&f, &request, 1, false, now);
    let main_id = main.id;
    inner.jobs.insert(main_id, main);
    // Queued past the backfill grace, so dispatch reaches it first.
    let mut user = sibling_user(&f, &request, "later", 2, now);
    user.queued_ms = Some(now - BACKFILL_GRACE_MS);
    let user_id = user.id;
    inner.jobs.insert(user_id, user);
    inner.terminal_saves.insert(main_id);
    runner.dispatch(&mut inner, now).unwrap();
    let started = inner.starting.clone();
    let phase = runner
        .queue_wait_phase(&inner, &DispatchIndex::build(&inner), user_id)
        .map(str::to_owned);
    // The withdrawal is saved and published.
    inner.terminal_saves.remove(&main_id);
    let withdrawn = inner.jobs.get_mut(&main_id).unwrap();
    withdrawn.terminal(
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded,
        },
        now,
    );
    withdrawn.state = JobState::Tombstone;
    runner.dispatch(&mut inner, now).unwrap();
    let after = inner.starting.clone();
    drop(inner);
    assert!(
        started.is_empty(),
        "a job started while a workspace job's withdrawal was pending: {started:?}"
    );
    assert_eq!(phase.as_deref(), Some("queued workspace job"));
    assert_eq!(after, BTreeSet::from([user_id]));
}

/// An 8-permit runner whose gate job `gate` (with `gate_weight` permits)
/// failed to start and is waiting out its retry delay, while a 2-permit job
/// runs in workspace `owned` and another waits behind it, and a 2-permit
/// worker waits in a workspace of its own. Returns the runner and the ids
/// of the gate, the job behind the owner and the worker.
fn retry_delayed_gate(f: &Fixture, gate_weight: u32) -> (Runner, Uuid, Uuid, Uuid) {
    let mut config = f.config();
    config.max_weight = 8;
    config.cpu_topology = Some(CpuTopology::new((0..9).map(|cpu| vec![cpu]).collect()).unwrap());
    config.disable_warm_builds = true;
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (mut request, _) = f.run("true");
    request.siblings.clear();
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    let mut owner = job(&request, &format!("{}-owned", f.key), 2, 1, false, now);
    owner.state = JobState::Running;
    owner.started_ms = Some(now);
    let gate = job(
        &request,
        &format!("{}-gate", f.key),
        gate_weight,
        2,
        true,
        now,
    );
    let behind = job(&request, &format!("{}-owned", f.key), 2, 3, false, now);
    let worker = job(&request, &format!("{}-worker", f.key), 2, 4, false, now);
    let ids = (gate.id, behind.id, worker.id);
    inner.start_retries.insert(
        gate.id,
        StartRetry {
            not_before_ms: now + START_RETRY_MS,
            delay_ms: START_RETRY_MS,
            reason: "injected".into(),
        },
    );
    for job in [owner, gate, behind, worker] {
        inner.jobs.insert(job.id, job);
    }
    drop(inner);
    (runner, ids.0, ids.1, ids.2)
}

/// A gate job waiting out its start retry delay keeps its claim on
/// capacity, as if it were waiting for permits: a smaller worker does not
/// take its place. The clock and `waiting_on` helpers agree: the job queued
/// behind a workspace owner is held back by the gate's capacity fence too,
/// so its clock runs, and the gate itself is waiting on permits. This holds
/// whether or not the gate would fit the free permits.
#[test]
fn a_gate_in_its_start_retry_delay_still_fences_capacity() {
    for gate_weight in [8, 4] {
        let f = Fixture::new();
        let (runner, gate, behind, worker) = retry_delayed_gate(&f, gate_weight);
        let now = runner.now();
        let mut inner = runner.core.inner.lock().unwrap();
        runner.dispatch(&mut inner, now).unwrap();
        let started = inner.starting.clone();
        let index = DispatchIndex::build(&inner);
        let gate_phase = runner.queue_wait_phase(&inner, &index, gate);
        let behind_entry = inner.jobs[&behind].queue_entry.load(Ordering::Acquire);
        drop(inner);
        assert!(
            started.is_empty(),
            "gate of {gate_weight} permits: a job started in its place: {started:?} (worker {worker})"
        );
        assert_eq!(gate_phase, Some("permits"), "gate of {gate_weight} permits");
        assert_ne!(
            behind_entry, PAUSED_QUEUE,
            "gate of {gate_weight} permits: the job behind the owner was paused \
             although the gate's capacity fence holds it back too"
        );
    }
}

/// A gate whose earlier start attempt is still being cleaned up is skipped
/// like a checkout conflict, which can last until its VM is confirmed
/// stopped: it fences nothing, and the job behind a workspace owner pauses
/// as it would with no gate at all.
#[test]
fn a_gate_with_cleanup_pending_fences_nothing() {
    let f = Fixture::new();
    let (runner, gate, behind, worker) = retry_delayed_gate(&f, 8);
    let now = runner.now();
    let mut inner = runner.core.inner.lock().unwrap();
    inner.start_retries.clear();
    runner
        .config()
        .windows_runtime
        .set_cleanup_pending(gate, true);
    runner.dispatch(&mut inner, now).unwrap();
    let started = inner.starting.clone();
    let behind_entry = inner.jobs[&behind].queue_entry.load(Ordering::Acquire);
    drop(inner);
    runner
        .config()
        .windows_runtime
        .set_cleanup_pending(gate, false);
    assert_eq!(started, BTreeSet::from([worker]));
    assert_eq!(behind_entry, PAUSED_QUEUE);
}
