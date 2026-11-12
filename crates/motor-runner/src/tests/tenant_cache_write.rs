use super::super::remote_cache_rollout::{provision, provision_generation, runtime_with_store};
use super::*;
use crate::remote_cache::gateway::tests::TENANT;
use crate::remote_cache::provisioning::{HostRole, KeyRole, Layout};
use crate::remote_cache::runtime::Runtime;
use tokio::io::AsyncWriteExt;

fn provisioned(keyed: bool) -> (Fixture, Runner, Arc<Runtime>, Layout) {
    let f = Fixture::new();
    let layout = Layout::new(f.scratch.join("provisioning"), HostRole::TenantVm);
    layout.prepare().unwrap();
    provision(&layout, true);
    if !keyed {
        fs::remove_file(layout.key_path(KeyRole::Write)).unwrap();
    }
    let (cache, _) = runtime_with_store(&layout);
    cache.watch.reread();
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.remote_cache = true;
    config.remote_cache_runtime = Some(cache.clone());
    let runner = Runner::open(config).unwrap();
    (f, runner, cache, layout)
}

// Exercise the production eligibility predicate without opening an isolated
// runner or executing a unit. Record persistence uses the plain runner above;
// account validation remains the responsibility of isolated Runner::open.
fn eligibility_config(runner: &Runner, cache: &Arc<Runtime>) -> Config {
    let mut config = runner.config().clone();
    config.isolate_jobs = true;
    config.remote_cache_error = None;
    config.remote_cache_runtime = Some(cache.clone());
    config
}

fn admit(runner: &Runner, config: &Config, run: Run, account: Option<&str>) -> Admission {
    runner
        .accept_from(run, false, None, None, |id| {
            config.tenant_cache_grant(id, account)
        })
        .unwrap()
}

fn request(f: &Fixture) -> Run {
    let (mut run, _) = f.run("true");
    run.weight_hint = Some(2);
    run.timeout = Some(30);
    run.platform = None;
    run.network = None;
    run
}

fn decision(runner: &Runner, id: Uuid) -> (bool, Option<String>) {
    let inner = runner.core.inner.lock().unwrap();
    let job = &inner.jobs[&id];
    (job.cache_write, job.cache_write_account.clone())
}

#[test]
fn eligibility_requires_host_permission_isolation_tenant_role_and_account_key() {
    let (_, runner, cache, _) = provisioned(true);
    let config = eligibility_config(&runner, &cache);
    let id = Uuid::now_v7();
    assert_eq!(
        config.tenant_cache_grant(id, Some(TENANT)),
        Some(TENANT.into())
    );
    assert_eq!(config.tenant_cache_grant(id, None), None);
    for case in [
        "not-isolated",
        "boat",
        "build-server",
        "per-job",
        "switch-off",
    ] {
        let mut denied = config.clone();
        match case {
            "not-isolated" => denied.isolate_jobs = false,
            "boat" => denied.boat_profile = true,
            "build-server" => denied.tenant_vm = false,
            "per-job" => denied.per_job = true,
            "switch-off" => denied.remote_cache = false,
            _ => unreachable!(),
        }
        assert_eq!(denied.tenant_cache_grant(id, Some(TENANT)), None, "{case}");
    }
}

#[tokio::test]
async fn first_job_record_fixes_access_despite_later_run_or_attach_tokens() {
    for admitted in [false, true] {
        let (f, runner, cache, _) = provisioned(true);
        let config = eligibility_config(&runner, &cache);
        let run = request(&f);
        let id = run.job_id;
        assert!(matches!(
            admit(&runner, &config, run.clone(), admitted.then_some(TENANT)),
            Admission::Accepted(_)
        ));
        let mut token = lease(id, LeasePurpose::Run);
        token.sub = TENANT.into();
        token.cache_write = admitted;
        let expected = (admitted, admitted.then(|| TENANT.to_owned()));
        assert_eq!(decision(&runner, id), expected);
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(runner.record_path(id)).unwrap()).unwrap();
        assert_eq!(
            stored.get("cache_write").and_then(|v| v.as_bool()),
            admitted.then_some(true)
        );
        for purpose in [LeasePurpose::Run, LeasePurpose::Attach] {
            for later in [false, true] {
                token.purpose = purpose;
                token.cache_write = later;
                let (other, task) = connect(&runner, token.clone()).await;
                assert!(matches!(
                    admit(&runner, &config, run.clone(), later.then_some(TENANT)),
                    Admission::Duplicate
                ));
                assert_eq!(decision(&runner, id), expected);
                drop(other);
                task.await.unwrap().unwrap();
            }
        }
    }
}

#[tokio::test]
async fn read_only_channel_ignores_a_clients_cache_write_field() {
    let (f, runner, _, _) = provisioned(true);
    let run = request(&f);
    let id = run.job_id;
    let (mut client, task) = connect(&runner, lease(id, LeasePurpose::Run)).await;
    let mut wire = serde_json::to_value(Frame::Run(run)).unwrap();
    wire["cache_write"] = true.into();
    let bytes = serde_json::to_vec(&wire).unwrap();
    client
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .unwrap();
    client.write_all(&bytes).await.unwrap();
    assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
    assert_eq!(decision(&runner, id), (false, None));
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn keyless_blocked_or_wrong_account_admission_cannot_be_upgraded_later() {
    for case in ["keyless", "blocked", "other-account"] {
        let (f, runner, cache, layout) = provisioned(case != "keyless");
        let config = eligibility_config(&runner, &cache);
        if case == "blocked" {
            cache.gateway.set_job_write_block(true).await;
        }
        let run = request(&f);
        let id = run.job_id;
        let account = if case == "other-account" {
            "acct:other"
        } else {
            TENANT
        };
        let logs = capture_runner_logs(|| {
            assert!(matches!(
                admit(&runner, &config, run.clone(), Some(account)),
                Admission::Accepted(_)
            ));
        });
        assert!(logs.contains("admitting read-only"), "{case}: {logs}");
        assert_eq!(decision(&runner, id), (false, None));
        provision_generation(&layout, true, TENANT, "2");
        cache.watch.reread();
        cache.gateway.set_job_write_block(false).await;
        assert_eq!(
            config.tenant_cache_grant(id, Some(TENANT)),
            Some(TENANT.into())
        );
        assert!(matches!(
            admit(&runner, &config, run, Some(TENANT)),
            Admission::Duplicate
        ));
        assert_eq!(decision(&runner, id), (false, None));
    }
}

#[tokio::test]
async fn attach_without_a_job_never_grants_cache_access() {
    let (f, runner, _, _) = provisioned(true);
    let run = request(&f);
    let mut token = lease(run.job_id, LeasePurpose::Attach);
    token.sub = TENANT.into();
    token.cache_write = true;
    let (client, task) = connect(&runner, token).await;
    drop(client);
    task.await.unwrap().unwrap();
    assert!(!runner.has_job_record(run.job_id));
    runner.accept_tenant(run.clone(), None, None, None).unwrap();
    assert_eq!(decision(&runner, run.job_id), (false, None));
}

#[test]
fn account_rotation_between_admission_and_setup_binds_only_read_sockets() {
    use crate::remote_cache::{
        gateway::{Endpoint, Tool},
        transport::Role,
    };
    let (f, runner, cache, layout) = provisioned(true);
    let config = eligibility_config(&runner, &cache);
    let run = request(&f);
    let id = run.job_id;
    assert!(matches!(
        admit(&runner, &config, run.clone(), Some(TENANT)),
        Admission::Accepted(_)
    ));
    assert_eq!(decision(&runner, id), (true, Some(TENANT.into())));
    let mut setup = Setup {
        id,
        request: run,
        cache_write_account: decision(&runner, id).1,
        cpus: vec![1],
        memory_mib: None,
        base: false,
        recorded: BTreeMap::new(),
        snapshots: None,
    };
    for (stage, expected) in [
        ("admitted", Endpoint::JobCreate),
        ("rotated", Endpoint::Read),
        ("missing-account", Endpoint::Read),
    ] {
        if stage == "rotated" {
            // A rotation must use a fresh generation, not mutate the active one.
            provision_generation(&layout, true, "acct:other", "2");
            cache.watch.reread();
        }
        if stage == "missing-account" {
            provision_generation(&layout, true, TENANT, "3");
            cache.watch.reread();
            setup.cache_write_account = None;
        }
        let generation = cache.watch.admit().unwrap();
        if stage == "rotated" {
            assert_eq!(generation.tenant, "acct:other");
        }
        let transport = backend::job_cache_transport(
            &config,
            &setup,
            &generation,
            &cache,
            &[Tool::Sccache, Tool::Ccache],
        )
        .unwrap();
        let sockets = transport
            .open(&f.scratch.join(stage), Role::Job(&id.to_string()))
            .unwrap();
        assert_eq!(sockets.listeners()[0].binding().endpoint(), expected);
        assert_eq!(sockets.listeners()[1].binding().endpoint(), Endpoint::Read);
        assert_eq!(decision(&runner, id), (true, Some(TENANT.into())));
    }
}

#[test]
fn job_vm_socket_grant_requires_both_trusted_boot_flag_and_credential_write_key() {
    use crate::remote_cache::{
        gateway::{Endpoint, Tool},
        transport::Role,
    };
    for keyed in [true, false] {
        let (f, runner, cache, _) = provisioned(keyed);
        let mut config = eligibility_config(&runner, &cache);
        config.per_job = true;
        let run = request(&f);
        let id = run.job_id;
        let mut setup = Setup {
            id,
            request: run,
            cache_write_account: None,
            cpus: vec![1],
            memory_mib: None,
            base: false,
            recorded: BTreeMap::new(),
            snapshots: None,
        };
        let generation = cache.watch.admit().unwrap();
        for (index, boot) in [
            "motor.per_job=1 motor.cache_write=1",
            "motor.per_job=1",
            "motor.cache_write=1",
            "motor.per_job=1 motor.cache_write=0",
            "motor.per_job=1 motor.cache_write=1 motor.cache_write=1",
            "motor.per_job=1 motor.cache_write=1 motor.fill=1",
        ]
        .iter()
        .enumerate()
        {
            // Even a host frame claiming writes cannot supply boot authority.
            setup.cache_write_account = Some(TENANT.into());
            let granted = backend::job_cache_write_for_boot(&config, &setup, &generation, boot);
            assert_eq!(granted, keyed && index == 0, "keyed={keyed} boot={boot}");
            let transport = cache
                .transport(TENANT, &[Tool::Sccache, Tool::Ccache])
                .unwrap()
                .job_write(granted);
            let sockets = transport
                .open(
                    &f.scratch.join(format!("guest-{index}")),
                    Role::Job(&id.to_string()),
                )
                .unwrap();
            assert_eq!(
                sockets.listeners()[0].binding().endpoint(),
                if keyed && index == 0 {
                    Endpoint::JobCreate
                } else {
                    Endpoint::Read
                }
            );
            assert_eq!(sockets.listeners()[1].binding().endpoint(), Endpoint::Read);
            assert!(sockets
                .listeners()
                .iter()
                .all(|listener| listener.binding().ledger().is_none()));
        }
    }
}

#[test]
fn dedicated_job_gateway_refuses_fill_binding_without_root_fill_boot() {
    use crate::remote_cache::gateway::tests::{FakeConnector, FakeStore};
    use crate::remote_cache::{
        gateway::{Gateway, Limits, Tool},
        runtime::Runtime,
        transport::Role,
    };
    for (index, boot) in [
        "motor.per_job=1 motor.cache_write=1",
        "motor.per_job=1 motor.fill=0",
        "motor.per_job=1 motor.fill=1",
    ]
    .iter()
    .enumerate()
    {
        let (f, runner, cache, _) = provisioned(true);
        let mut config = runner.config().clone();
        config.per_job = true;
        let gateway = Gateway::with_connector(
            Limits::for_config_and_boot(&config, boot),
            Arc::new(cache.watch.clone()),
            Arc::new(FakeConnector(Arc::new(FakeStore::default()))),
        );
        let cache =
            Runtime::with_gateway(cache.watch.clone(), gateway, vec![(Tool::Sccache, 3893)])
                .unwrap();
        let directory = f.scratch.join(format!("fill-{index}"));
        let result = cache
            .transport(TENANT, &[Tool::Sccache])
            .unwrap()
            .open(&directory, Role::Fill);
        if index == 2 {
            let sockets = result.unwrap();
            assert_eq!(
                sockets.listeners()[0].binding().endpoint(),
                crate::remote_cache::gateway::Endpoint::Write
            );
        } else {
            assert_eq!(
                result.err().expect("Job VM bound a Fill writer").kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert!(!directory.join("sccache.sock").exists());
        }
    }
}
