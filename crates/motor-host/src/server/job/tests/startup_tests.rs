use super::*;

fn managed() -> (Arc<Runtime>, Arc<Fake>, Arc<Storage>) {
    let (old, fake, storage) = fixture();
    let settings = old.host.lock().unwrap().settings.clone();
    drop(old);
    let (journal, snapshot) = crate::store::SqliteJournal::open(storage.root.path()).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = settings;
    host.enforce_storage();
    fake.capacity_available.store(true, Ordering::SeqCst);
    *fake.readiness.lock().unwrap() = Some(motor_vm_helper::images::Readiness {
        job_vms_configured: true,
        workspace_disk_max_bytes: Some(1024),
        published_generations: vec!["gen-1".into(), "gen-2".into()],
    });
    let runtime = Runtime::with_broker_and_image_generations(
        "host".into(),
        host,
        fake.clone(),
        "http://broker".into(),
        "http://keys".into(),
    );
    runtime
        .configure_job_resolvers(vec!["1.1.1.1".into()])
        .unwrap();
    runtime.probe_storage();
    (runtime, fake, storage)
}

#[test]
fn missing_readiness_support_refuses_startup() {
    let (runtime, fake, _storage) = managed();
    *fake.readiness.lock().unwrap() = None;
    assert_eq!(
        runtime.validate_job_startup().unwrap_err().to_string(),
        "helper does not support per-job readiness"
    );
}

#[test]
fn unconfigured_job_helper_refuses_startup() {
    let (runtime, fake, _storage) = managed();
    fake.readiness
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .job_vms_configured = false;
    assert_eq!(
        runtime.validate_job_startup().unwrap_err().to_string(),
        "helper job_vms is not configured"
    );
}

#[test]
fn no_active_generation_refuses_startup() {
    let (runtime, _fake, _storage) = managed();
    assert!(runtime
        .validate_job_startup()
        .unwrap_err()
        .to_string()
        .starts_with("no active job image generation;"));
}

#[test]
fn missing_helper_workspace_maximum_refuses_startup_and_offline_selection() {
    let (runtime, fake, _storage) = managed();
    fake.readiness
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .workspace_disk_max_bytes = None;
    assert_eq!(
        runtime.validate_job_startup().unwrap_err().to_string(),
        "helper workspace_disk_max_bytes is not configured"
    );
    assert_eq!(
        runtime
            .select_initial_job_images("gen-1", unix_ms())
            .unwrap_err()
            .to_string(),
        "helper workspace_disk_max_bytes is not configured"
    );
}

#[test]
fn small_helper_workspace_maximum_refuses_startup_and_offline_selection() {
    let (runtime, fake, _storage) = managed();
    fake.readiness
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .workspace_disk_max_bytes = Some(1023);
    assert_eq!(
        runtime.validate_job_startup().unwrap_err().to_string(),
        "helper workspace_disk_max_bytes is below host workspace_disk_cap_bytes"
    );
    assert_eq!(
        runtime
            .select_initial_job_images("gen-1", unix_ms())
            .unwrap_err()
            .to_string(),
        "helper workspace_disk_max_bytes is below host workspace_disk_cap_bytes"
    );
}

#[test]
fn unpublished_active_generation_refuses_startup() {
    let (runtime, fake, _storage) = managed();
    runtime
        .select_initial_job_images("gen-1", unix_ms())
        .unwrap();
    fake.readiness
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .published_generations
        .clear();
    assert_eq!(
        runtime.validate_job_startup().unwrap_err().to_string(),
        "active job image generation gen-1 is no longer published and verified by the helper"
    );
}

#[test]
fn empty_active_image_hello_refuses_before_expensive_helper_readiness() {
    let (runtime, fake, _) = managed();
    runtime
        .select_initial_job_images("gen-1", unix_ms())
        .unwrap();
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .images
        .get_mut("gen-1")
        .unwrap()
        .manifest
        .hello = json!({});
    fake.calls.lock().unwrap().clear();
    let error = runtime.validate_job_startup().unwrap_err();
    assert!(format!("{error:#}").contains("pinned Hello"));
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn offline_selection_is_durable_fenced_and_cannot_replace_an_active_generation() {
    let (runtime, fake, storage) = managed();
    assert!(crate::store::SqliteJournal::open(storage.root.path()).is_err());
    let before = runtime.host.lock().unwrap().snapshot.clone();
    assert!(runtime
        .select_initial_job_images("unknown", unix_ms())
        .unwrap_err()
        .to_string()
        .contains("not published and verified"));
    assert_eq!(runtime.host.lock().unwrap().snapshot, before);
    runtime
        .select_initial_job_images("gen-1", unix_ms())
        .unwrap();
    runtime
        .select_initial_job_images("gen-1", unix_ms())
        .unwrap();
    assert!(runtime
        .select_initial_job_images("gen-2", unix_ms())
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "publish:gen-1")
            .count(),
        1
    );
    drop(runtime);
    let (journal, snapshot) = crate::store::SqliteJournal::open(storage.root.path()).unwrap();
    assert!(snapshot.per_job.images["gen-1"].current);
    assert!(!snapshot.per_job.images.contains_key("gen-2"));
    drop(journal);
}

#[test]
fn switching_back_into_per_job_replaces_an_idle_generation_but_never_a_held_one() {
    let (runtime, fake, storage) = managed();
    runtime
        .select_initial_job_images("gen-1", unix_ms())
        .unwrap();
    // A recovery holding the old generation pins it, so even a mode switch
    // must not move the host off it.
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .images
        .get_mut("gen-1")
        .unwrap()
        .recovery_holders
        .insert("recovery".into(), "account".into());
    let error = runtime
        .select_job_images_replacing_idle("gen-2", unix_ms())
        .unwrap_err()
        .to_string();
    assert!(error.contains("still held"), "{error}");
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .images
        .get_mut("gen-1")
        .unwrap()
        .recovery_holders
        .clear();
    // The plain bootstrap still refuses; only the explicit switch path replaces.
    assert!(runtime
        .select_initial_job_images("gen-2", unix_ms())
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    runtime
        .select_job_images_replacing_idle("gen-2", unix_ms())
        .unwrap();
    let calls = fake.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|call| call == "publish:gen-2"));
    // The superseded generation is collected like any other once it is idle.
    assert!(calls.iter().any(|call| call == "remove:gen-1"));
    drop(runtime);
    let (journal, snapshot) = crate::store::SqliteJournal::open(storage.root.path()).unwrap();
    assert!(snapshot.per_job.images["gen-2"].current);
    assert!(snapshot
        .per_job
        .images
        .get("gen-1")
        .is_none_or(|generation| !generation.current));
    drop(journal);
}

#[tokio::test]
async fn image_managed_runtime_starts_and_serves_a_job_without_a_tenant_vm() {
    let (runtime, fake, _storage) = managed();
    runtime
        .select_initial_job_images("gen-1", unix_ms())
        .unwrap();
    runtime.prepare_job_startup().unwrap();
    runtime.recover().unwrap();
    runtime.tick(unix_ms()).unwrap();
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    queued(&mut client, JOB).await;
    next_type(&mut client, "output").await;
    {
        let host = runtime.host.lock().unwrap();
        assert!(host.tenants.is_empty());
        assert!(host.snapshot.leases[JOB].delivered);
        assert_eq!(
            host.snapshot.per_job.executions[JOB]
                .image_generation
                .as_deref(),
            Some("gen-1")
        );
    }
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == &format!("start:{JOB}"))
            .count(),
        1
    );
    assert!(calls.contains(&"prepare:gen-1".to_owned()));
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
}

#[test]
fn tenant_startup_and_host_status_keep_the_original_bytes_without_helper_traffic() {
    let (runtime, fake, _storage) = fixture();
    runtime.host.lock().unwrap().settings = crate::config::HostSettings::Tenant;
    let before = runtime.host.lock().unwrap().snapshot.clone();
    runtime.prepare_job_startup().unwrap();
    let status = runtime.host_admin(&json!({"op":"host_status"})).unwrap();
    assert_eq!(serde_json::to_vec(&status).unwrap(), br#"{"accept_until_ms":null,"active_leases":0,"bounded_drain_cancelled":false,"bounded_drain_started":false,"broker_draining":false,"broker_heartbeat_at_ms":null,"draining":false,"no_op":null,"ok":true,"set_at_ms":null}"#);
    assert_eq!(runtime.host.lock().unwrap().snapshot, before);
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn per_job_status_exposes_background_work_and_counts_pending_execution_teardown() {
    let (runtime, _fake, _storage) = fixture();
    claim(&runtime, &claims(JOB));
    // Lease closure precedes proof that its VM released the disk.
    runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .leases
        .get_mut(JOB)
        .unwrap()
        .state = crate::state::LeaseState::Ended;
    let status = runtime.host_admin(&json!({"op":"host_status"})).unwrap();
    assert_eq!(status["active_leases"], 0);
    assert_eq!(status["active_job_vms"], 1);
    assert_eq!(status["active_fills"], 0);
    assert_eq!(status["active_recoveries"], 0);
    let drain = runtime.host_admin(&json!({"op":"drain_host"})).unwrap();
    for field in ["active_job_vms", "active_fills", "active_recoveries"] {
        assert!(drain.get(field).is_none());
    }
}
