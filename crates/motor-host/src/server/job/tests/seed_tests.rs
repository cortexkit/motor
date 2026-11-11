use super::*;

fn base(runtime: &Arc<Runtime>, id: &str, account: &str, repository: &str) {
    runtime.host.lock().unwrap().snapshot.per_job.bases.insert(
        id.into(),
        crate::state::jobs::storage::Base {
            sub: account.into(),
            repository_key: Some(crate::jobs::fill_source::repository_key(repository)),
            have: Vec::new(),
            last_used_ms: unix_ms(),
            evicting: false,
        },
    );
}

#[tokio::test]
async fn new_workspace_seeds_matching_base_before_boot_without_the_host_lock() {
    let (runtime, fake, _) = fixture();
    hold_boot(&runtime, &fake, "seed");
    let c = claims(JOB);
    let request = run(JOB);
    let root = request["repository_root"].as_str().unwrap();
    base(&runtime, "matching", &c.sub, root);
    let id = claim(&runtime, &c);
    let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    let (mut client, task) = start(runtime.clone(), c.clone(), id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    write_frame(
        &mut client,
        &json!({"type":"have","repositories":[],"git_dependencies":[]}),
    )
    .await
    .unwrap();
    next_type(&mut client, "have_reply").await;
    assert!(fake.seeds.lock().unwrap().is_empty());
    assert!(fake.calls.lock().unwrap().is_empty());
    write_frame(&mut client, &request).await.unwrap();
    next_type(&mut client, "accepted").await;
    fake.entered.notified().await;
    // The copy call stays in flight while this probe waits out any brief
    // session reads. It must acquire the lock before the copy can finish.
    let r = runtime.clone();
    let unlocked_during_copy = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || drop(r.host.lock().unwrap())),
    )
    .await;
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    unlocked_during_copy
        .expect("host lock available while the helper copies the seed")
        .unwrap();
    next_type(&mut client, "output").await;
    assert_eq!(
        *fake.seeds.lock().unwrap(),
        [motor_vm_helper::job::Seed {
            cache_write_job_id: None,
            account: c.sub,
            repository_key: crate::jobs::fill_source::repository_key(root),
            base_id: "matching".into(),
            disk_id: disk.clone(),
        }]
    );
    assert_eq!(
        fake.workspace_contents.lock().unwrap()[&disk],
        Some("matching".into())
    );
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn existing_workspace_is_untouched_when_a_new_base_is_available() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    base(
        &runtime,
        "new-base",
        &c.sub,
        run(JOB)["repository_root"].as_str().unwrap(),
    );
    let id = claim(&runtime, &c);
    existing_workspace(&runtime, JOB);
    let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    fake.workspace_contents
        .lock()
        .unwrap()
        .insert(disk.clone(), Some("own-builds".into()));
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    wait_for_runner_hello(&runtime, JOB).await;
    write_frame(&mut client, &run(JOB)).await.unwrap();
    next_type(&mut client, "accepted").await;
    next_type(&mut client, "output").await;
    assert!(fake.seeds.lock().unwrap().is_empty());
    assert_eq!(
        fake.workspace_contents.lock().unwrap()[&disk],
        Some("own-builds".into())
    );
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[tokio::test]
async fn run_repository_selects_only_its_own_accounts_matching_base_or_nothing() {
    for selected in ["/home/repo-b", "/home/missing"] {
        let (runtime, fake, _) = fixture();
        let c = claims(JOB);
        base(&runtime, "repo-a", &c.sub, "/home/repo-a");
        base(&runtime, "repo-b", &c.sub, "/home/repo-b");
        base(
            &runtime,
            "foreign",
            "org:01ARZ3NDEKTSV4RRFFQ69G5FAV",
            selected,
        );
        let id = claim(&runtime, &c);
        let (mut client, task) = start(runtime.clone(), c.clone(), id).await;
        next_type(&mut client, "lease_ok").await;
        write_frame(&mut client, &hello()).await.unwrap();
        next_type(&mut client, "hello").await;
        let mut request = run(JOB);
        request["repository_root"] = json!(selected);
        write_frame(&mut client, &request).await.unwrap();
        next_type(&mut client, "accepted").await;
        next_type(&mut client, "output").await;
        let seeds = fake.seeds.lock().unwrap().clone();
        if selected == "/home/repo-b" {
            assert_eq!(seeds.len(), 1);
            assert_eq!(seeds[0].account, c.sub);
            assert_eq!(seeds[0].base_id, "repo-b");
        } else {
            assert!(seeds.is_empty());
        }
        drop(client);
        task.await.unwrap().unwrap();
        runtime.stop_job(JOB, true, false).unwrap();
    }
}

#[tokio::test]
async fn failed_seed_falls_back_to_an_empty_disk_and_the_job_runs() {
    let (runtime, fake, _) = fixture();
    let c = claims(JOB);
    base(
        &runtime,
        "matching",
        &c.sub,
        run(JOB)["repository_root"].as_str().unwrap(),
    );
    fake.fail_seed.store(true, Ordering::SeqCst);
    let id = claim(&runtime, &c);
    let disk = runtime.host.lock().unwrap().snapshot.per_job.executions[JOB]
        .disk_id
        .clone();
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    next_type(&mut client, "output").await;
    assert_eq!(fake.seeds.lock().unwrap().len(), 1);
    assert_eq!(fake.workspace_contents.lock().unwrap()[&disk], None);
    {
        let host = runtime.host.lock().unwrap();
        assert!(host.snapshot.leases[JOB].delivered);
        assert!(!host.snapshot.per_job.disks[&disk].quarantined);
        assert!(host.snapshot.per_job.disks[&disk].have.is_empty());
    }
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}
