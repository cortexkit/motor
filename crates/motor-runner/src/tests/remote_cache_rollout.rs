use super::*;
use crate::remote_cache::gateway::tests::{generation, FakeConnector, FakeStore, BUCKET, TENANT};
use crate::remote_cache::gateway::{Gateway, Limits, Tool};
use crate::remote_cache::provisioning::{HostRole, Layout, Watch};
use crate::remote_cache::runtime::Runtime;
use crate::remote_cache::transport::{Role, Sockets};
use motor_protocol::RemoteCacheState;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

fn provision(layout: &Layout, enabled: bool) {
    let generation = generation("1", TENANT);
    for (name, key) in [
        ("read-key", &generation.read_key),
        ("write-key", generation.write_key.as_ref().unwrap()),
    ] {
        let value = serde_json::json!({"generation":"1", "access_key_id":key.access_key_id, "secret_access_key":key.secret_access_key});
        let path = layout.directory().join(name);
        let temporary = layout.directory().join(format!(".{name}-new"));
        fs::write(&temporary, value.to_string()).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o400)).unwrap();
        fs::rename(temporary, path).unwrap();
    }
    let value = serde_json::json!({"generation":"1", "tenant":TENANT,
        "endpoint":generation.endpoint, "region":generation.region, "bucket":generation.bucket,
        "prefix":generation.prefix, "storage_class":generation.storage_class, "enabled":enabled});
    let path = layout.config_path();
    let temporary = layout.directory().join(".config-new");
    fs::write(&temporary, value.to_string()).unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o444)).unwrap();
    fs::rename(temporary, path).unwrap();
}

fn runtime(layout: &Layout) -> Arc<Runtime> {
    let watch = Watch::new(layout.clone());
    let store = Arc::new(FakeStore::default());
    let gateway = Gateway::with_connector(
        Limits::default(),
        Arc::new(watch.clone()),
        Arc::new(FakeConnector(store)),
    );
    Runtime::with_gateway(
        watch,
        gateway,
        vec![(Tool::Sccache, 3893), (Tool::Ccache, 3894)],
    )
    .unwrap()
}

fn request(sockets: &Sockets, tool: Tool, method: &str, key: &str) -> u16 {
    let paths = sockets.paths();
    let socket = paths
        .iter()
        .find(|p| p.file_stem().unwrap() == tool.id())
        .unwrap();
    let directory = fs::File::open(socket.parent().unwrap()).unwrap();
    let address = format!(
        "/proc/self/fd/{}/{}",
        directory.as_raw_fd(),
        socket.file_name().unwrap().to_str().unwrap()
    );
    let mut stream = UnixStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let path = if tool == Tool::Sccache {
        format!("/{BUCKET}/{key}")
    } else {
        format!("/{key}")
    };
    let body = if method == "PUT" { "entry" } else { "" };
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn all_four_remote_cache_switch_combinations_use_real_gateway_and_transport() {
    for runner_switch in [false, true] {
        for tenant_switch in [false, true] {
            let f = Fixture::new();
            let mut config = f.config();
            config.isolate_jobs = true;
            config.remote_cache = runner_switch;
            let layout = Layout::new(f.scratch.join("provisioning"), HostRole::BuildServer);
            layout.prepare().unwrap();
            provision(&layout, tenant_switch);
            if runner_switch {
                let runtime = runtime(&layout);
                runtime.watch.reread();
                config.remote_cache_runtime = Some(runtime);
            }
            let on = runner_switch && tenant_switch;
            assert_eq!(config.unit_remote_cache(false).is_some(), on);
            assert_eq!(config.unit_remote_cache(true).is_some(), on);
            let snapshot =
                super::super::remote_cache_engine::snapshot(&config, &Catalog::default(), 0);
            assert_eq!(
                snapshot.state,
                if on {
                    RemoteCacheState::Available
                } else {
                    RemoteCacheState::Off
                }
            );
            if !on {
                continue;
            }
            let runtime = config.remote_cache_runtime.as_ref().unwrap();
            let transport = runtime
                .transport(TENANT, &[Tool::Sccache, Tool::Ccache])
                .unwrap();
            let writer = transport
                .open(&f.scratch.join("writer"), Role::Fill)
                .unwrap();
            let reader = transport
                .open(&f.scratch.join("reader"), Role::Job("job-1"))
                .unwrap();
            for tool in [Tool::Sccache, Tool::Ccache] {
                assert_eq!(request(&writer, tool, "PUT", "entry"), 200);
                assert_eq!(request(&reader, tool, "GET", "entry"), 200);
                assert_eq!(request(&reader, tool, "GET", "missing"), 404);
                assert_eq!(request(&reader, tool, "PUT", "forbidden"), 403);
            }
            assert_eq!(
                request(&writer, Tool::Sccache, "PUT", ".sccache_check"),
                200
            );
            assert_eq!(
                request(&reader, Tool::Sccache, "GET", ".sccache_check"),
                200
            );
            let snapshot =
                super::super::remote_cache_engine::snapshot(&config, &Catalog::default(), 0);
            assert_eq!(snapshot.sccache.hits, 1);
            assert_eq!(snapshot.sccache.probes, 1);
            assert_eq!(snapshot.sccache.misses, 1);
            assert_eq!(snapshot.sccache.refusals.read, 1);
            assert_eq!(snapshot.ccache.hits, 1);
            assert_eq!(snapshot.ccache.probes, 0);
            assert!(snapshot.sccache.p95_ms.is_some());
            provision(&layout, false);
            runtime.watch.reread();
            assert!(config.unit_remote_cache(false).is_none());
            assert_eq!(request(&reader, Tool::Sccache, "GET", "entry"), 404);
            provision(&layout, true);
            runtime.watch.reread();
            let new_reader = transport
                .open(&f.scratch.join("new-reader"), Role::Job("job-2"))
                .unwrap();
            assert_eq!(request(&new_reader, Tool::Sccache, "GET", "entry"), 200);
            for (boat, tenant) in [(true, false), (false, true)] {
                let mut hosted = config.clone();
                hosted.boat_profile = boat;
                hosted.tenant_vm = tenant;
                assert!(hosted.unit_remote_cache(false).is_some());
                assert!(hosted.unit_remote_cache(true).is_none());
            }
        }
    }
}

#[tokio::test]
async fn runner_switch_off_and_ignored_switch_never_read_or_create_provisioning() {
    for switch in [false, true] {
        let f = Fixture::new();
        let mut config = f.config();
        config.manual_tick = true;
        config.disable_warm_builds = true;
        config.remote_cache = switch;
        let directory = config.state_dir().join("cache-provisioning");
        let runner = Runner::open(config).unwrap();
        assert!(!directory.exists());
        assert!(runner.config().remote_cache_runtime.is_none());
        assert!(runner.publishing_identity().is_none());
        let health = runner.runner_status().health.unwrap().remote_cache.unwrap();
        assert_eq!(
            health.state,
            if switch {
                RemoteCacheState::CacheUnavailable
            } else {
                RemoteCacheState::Off
            }
        );
        if switch {
            assert_eq!(
                health.reason.as_deref(),
                Some("isolate_jobs is off; runner switch ignored")
            );
        }
        let (request, bytes) = f.run("exit 23");
        f.submit(&runner, &request, &bytes);
        until(|| {
            runner.tick().unwrap();
            runner.core.inner.lock().unwrap().jobs[&request.job_id]
                .terminal
                .is_some()
        })
        .await;
        assert_eq!(
            runner.core.inner.lock().unwrap().jobs[&request.job_id]
                .terminal
                .as_ref()
                .unwrap()
                .outcome,
            Outcome::Exit { code: 23 }
        );
    }
}

#[test]
fn publication_health_is_bounded_and_contains_no_store_identity() {
    let f = Fixture::new();
    let config = f.config();
    let mut catalog = Catalog::default();
    for index in 0..20 {
        let record = catalog
            .repositories
            .entry(format!("secret-tenant/repository-{index}"))
            .or_default();
        record.filled_descriptor = Some("D".into());
        record.requested_descriptor = Some("D".into());
        record.recorded_without_writer.insert("D".into());
        record.failed_descriptor = Some("f".repeat(64));
        record.publications.insert(
            "D".into(),
            crate::storage::PublicationRecord {
                identity: crate::storage::StoreIdentity {
                    endpoint: "secret-endpoint".into(),
                    bucket: "secret-bucket".into(),
                    prefix: "secret-prefix".into(),
                },
                started_ms: 10,
                keys: Vec::new(),
            },
        );
    }
    let health = super::super::remote_cache_engine::snapshot(&config, &catalog, 110);
    assert_eq!(health.repositories.len(), 16);
    assert_eq!(health.repositories_omitted, 4);
    for repository in health.repositories.values() {
        assert_eq!(repository.publication_age_ms, Some(100));
        assert!(repository.shortcut);
        assert_eq!(
            repository.held_descriptor.as_deref(),
            Some("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
        );
    }
    let encoded = serde_json::to_string(&health).unwrap();
    assert!(!encoded.contains("secret-"));
    assert!(encoded.len() < 8192, "{} bytes", encoded.len());
    println!(
        "bounded remote_cache health: {} bytes (16 repository summaries)",
        encoded.len()
    );
}

#[test]
fn cache_startup_errors_and_bind_failure_are_uncached_not_job_refusals() {
    let f = Fixture::new();
    let layout = Layout::new(f.scratch.join("provisioning"), HostRole::BuildServer);
    layout.prepare().unwrap();
    let runtime = runtime(&layout);
    let mut config = f.config();
    config.remote_cache = true;
    config.isolate_jobs = true;
    config.remote_cache_runtime = Some(runtime.clone());
    runtime.watch.reread();
    assert!(config.unit_remote_cache(false).is_none());
    assert_eq!(
        super::super::remote_cache_engine::snapshot(&config, &Catalog::default(), 0).state,
        RemoteCacheState::CacheUnavailable
    );
    assert_uncached_job_starts(&f, &config, "missing provisioning");
    provision(&layout, true);
    runtime.watch.reread();
    let transport = runtime.transport(TENANT, &[Tool::Sccache]).unwrap();
    fs::create_dir(f.scratch.join("occupied")).unwrap();
    let error = transport
        .open(&f.scratch.join("occupied"), Role::Job("job-1"))
        .err()
        .unwrap();
    transport.failed("socket bind or grant", &error);
    let health = super::super::remote_cache_engine::snapshot(&config, &Catalog::default(), 0);
    assert_eq!(health.state, RemoteCacheState::CacheUnavailable);
    assert!(health.reason.unwrap().contains("AlreadyExists"));
    assert_uncached_job_starts(&f, &config, "socket bind failure");
}

/// Substitute only the privileged systemd launch, after the real backend has
/// decided whether a cache failure permits the job to start and built its env.
struct UncachedIsolationLauncher;

impl crate::backend::IsolationLauncher for UncachedIsolationLauncher {
    fn launch(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        _cpus: &[u32],
        _requested_vcpus: u32,
    ) -> io::Result<crate::platform::Launch> {
        for key in [
            "SCCACHE_ENDPOINT",
            "SCCACHE_BUCKET",
            "CCACHE_REMOTE_STORAGE",
        ] {
            assert!(env.iter().all(|(name, _)| name != key), "unexpected {key}");
        }
        crate::platform::launch(command, cwd, env)
    }
}

fn assert_uncached_job_starts(f: &Fixture, config: &Config, phase: &str) {
    let (mut request, _) = f.run("exit 23");
    request.base_commit = Some("f".repeat(40));
    let setup = Setup {
        id: request.job_id,
        request,
        cpus: vec![1],
        base: false,
        recorded: BTreeMap::new(),
        snapshots: None,
    };
    let prepared = Prepared {
        cwd: f.main.clone(),
        target: f.scratch.join("uncached-target"),
        wrapper: None,
        sandbox: Some(Box::new(UncachedIsolationLauncher)),
        vm: None,
    };
    let trees = Trees {
        dependency_locks: Mutex::new(BTreeMap::new()),
        mirrors: config.state_dir().join("mirrors"),
        state: config.state_dir(),
        compacting: Arc::default(),
    };
    let mut launch = crate::backend::HostBackend
        .launch(config, &trees, &setup, &prepared, 1)
        .unwrap_or_else(|error| panic!("{phase} must not refuse an uncached job: {error}"));
    if let Some(process) = launch.process {
        f.cleanup.0.lock().unwrap().register(process);
    }
    launch.execution.release().unwrap();
    let outcome = launch.execution.wait().unwrap().wire();
    launch.execution.reap();
    assert_eq!(outcome, Outcome::Exit { code: 23 }, "{phase}");
}

#[test]
fn publishing_without_write_sockets_is_not_publication() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let outcome = runner.fill_publication(
        &crate::storage::StoreIdentity {
            endpoint: "e".into(),
            bucket: "b".into(),
            prefix: "p".into(),
        },
        Uuid::now_v7(),
        &[],
    );
    assert!(
        matches!(outcome, crate::remote_cache::publication::Outcome::Failed { reason } if reason == "no cache write sockets")
    );
}

#[test]
fn restart_remembers_switch_off_without_forgetting_local_records() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config.clone()).unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.publishing_seen = crate::remote_cache::publishing::Seen {
            switch: true,
            publishing: true,
            identity: Some(generation("1", TENANT).identity().recorded()),
        };
        inner
            .catalog
            .repositories
            .entry(f.root.clone())
            .or_default()
            .filled_descriptor = Some("D".into());
        runner.catalog(&inner).unwrap();
    }
    drop(runner);
    let runner = Runner::open(config).unwrap();
    let inner = runner.core.inner.lock().unwrap();
    let catalog = &inner.catalog;
    assert!(catalog.publishing_seen.is_default());
    assert_eq!(
        catalog.repositories[&f.root].filled_descriptor.as_deref(),
        Some("D")
    );
    assert!(!runner
        .config()
        .state_dir()
        .join("cache-provisioning")
        .exists());
}

#[tokio::test]
async fn production_watcher_connects_health_and_publishing_to_late_provisioning() {
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    // Reuse the portable backend's initialized core; privileged host-account
    // validation is outside this test. Everything after startup uses the real
    // process runtime, watcher callback, catalog persistence and health path.
    let mut core = Arc::try_unwrap(runner.core).ok().unwrap();
    core.config.isolate_jobs = true;
    core.config.remote_cache = true;
    let layout = Layout::new(f.scratch.join("late-provisioning"), HostRole::BuildServer);
    layout.prepare().unwrap();
    core.config.remote_cache_runtime = Some(runtime(&layout));
    core.publishing = Mutex::new(crate::remote_cache::publishing::State::new(true));
    let runner = Runner {
        core: Arc::new(core),
    };
    register_runner(&runner);
    runner.start_remote_cache();
    assert_eq!(
        runner
            .runner_status()
            .health
            .unwrap()
            .remote_cache
            .unwrap()
            .state,
        RemoteCacheState::CacheUnavailable
    );
    assert!(runner.publishing_identity().is_none());
    provision(&layout, true);
    until(|| {
        runner
            .runner_status()
            .health
            .unwrap()
            .remote_cache
            .unwrap()
            .state
            == RemoteCacheState::Available
    })
    .await;
    until(|| runner.publishing_identity().is_some()).await;
    assert!(
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .publishing_seen
            .publishing
    );
    provision(&layout, false);
    until(|| {
        runner
            .runner_status()
            .health
            .unwrap()
            .remote_cache
            .unwrap()
            .state
            == RemoteCacheState::Off
    })
    .await;
    until(|| runner.publishing_identity().is_none()).await;
    assert!(
        !runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .publishing_seen
            .publishing
    );
}
