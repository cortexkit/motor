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
                // A tenant VM may fill its own cache; Boat only ever reads.
                assert_eq!(hosted.unit_remote_cache(true).is_some(), tenant);
            }
        }
    }
}

#[test]
fn only_a_tenant_whose_provisioning_holds_a_write_key_may_fill() {
    for keyed in [false, true] {
        let f = Fixture::new();
        let mut config = f.config();
        config.isolate_jobs = true;
        config.remote_cache = true;
        config.tenant_vm = true;
        let layout = Layout::new(f.scratch.join("provisioning"), HostRole::TenantVm);
        layout.prepare().unwrap();
        provision(&layout, true);
        if !keyed {
            fs::remove_file(layout.key_path(crate::remote_cache::provisioning::KeyRole::Write))
                .unwrap();
        }
        let runtime = runtime(&layout);
        runtime.watch.reread();
        config.remote_cache_runtime = Some(runtime);
        // Jobs read the cache either way.
        assert!(config.unit_remote_cache(false).is_some(), "{keyed}");
        assert_eq!(config.tenant_write_key_held(), keyed);
        // The account check is the last gate, so set it aside here: the
        // portable fixture has no `motor-cache` account.
        #[cfg(target_os = "linux")]
        {
            config.writer_account = Some(crate::isolation::WriterAccount { uid: 1 });
        }
        #[cfg(target_os = "linux")]
        assert_eq!(config.writer_enabled(), keyed);
        #[cfg(not(target_os = "linux"))]
        assert!(!config.writer_enabled());

        // Boat never fills, even with a write key in its files.
        let mut boat = config.clone();
        boat.tenant_vm = false;
        boat.boat_profile = true;
        assert!(!boat.writer_enabled());
        assert!(boat.unit_remote_cache(true).is_none());
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
/// decided whether a cache failure permits the job to start and built its
/// environment.
struct UncachedIsolationLauncher {
    /// Set for a Boat job: the runner's state directory. The real launcher
    /// rejects a workspace that is not on the sandbox's state disk, and a
    /// portable fixture checks the same thing here because the privileged
    /// launch is what it substitutes.
    boat_disk: Option<PathBuf>,
    /// The environment the backend built for this job, when the test wants to
    /// read it instead of running the job.
    captured: Option<CapturedEnvironment>,
}

/// Where a launch records the environment it was given, for a test that reads
/// the launch's output instead of running the job.
type CapturedEnvironment = Arc<Mutex<Option<BTreeMap<String, String>>>>;

impl crate::backend::IsolationLauncher for UncachedIsolationLauncher {
    fn launch(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        _cpus: &[u32],
        _requested_vcpus: u32,
    ) -> io::Result<crate::platform::Launch> {
        if let Some(state) = &self.boat_disk {
            use std::os::unix::fs::MetadataExt;
            let device = fs::metadata(state)?.dev();
            let workspace = fs::metadata(cwd)?;
            assert!(
                workspace.is_dir() && workspace.dev() == device,
                "workspace {} is not on the Boat state disk",
                cwd.display()
            );
        }
        if let Some(captured) = &self.captured {
            *captured.lock().unwrap() = Some(env.iter().cloned().collect());
            // The environment is what this test reads; the privileged launch
            // that would run it is not available to a portable test.
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "isolation launcher observed",
            ));
        }
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

/// The environment a Boat job launches with, in whatever cache state `config`
/// carries. Anything that refuses the job appears as the panic this test makes
/// visible, not as a missing variable.
fn boat_job_environment(f: &Fixture, config: &Config, phase: &str) -> BTreeMap<String, String> {
    // The Boat profile also refuses a job whose workspace is not on the
    // sandbox's mounted, project-quota disk. A portable scratch directory is
    // neither, and those two refusals are covered by their own tests, so clear
    // them here to reach the environment the launch builds.
    let mut config = config.clone();
    config.boat_profile = false;
    config.boat_cold_materialization = false;
    let captured = Arc::new(Mutex::new(None));
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
        host_cwd: f.main.clone(),
        target: f.scratch.join("boat-cache-target"),
        wrapper: None,
        sandbox: Some(Box::new(UncachedIsolationLauncher {
            boat_disk: None,
            captured: Some(captured.clone()),
        })),
        vm: None,
    };
    let trees = Trees {
        mirror_locks: Mutex::new(BTreeMap::new()),
        mirrors: config.state_dir().join("mirrors"),
        state: config.state_dir(),
        compacting: Arc::default(),
    };
    match crate::backend::HostBackend.launch(&config, &trees, &setup, &prepared, 1) {
        Ok(_) => {}
        // The substituted launcher records the environment instead of starting
        // a systemd unit a portable test cannot run.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => panic!("{phase} must not refuse a job: {error}"),
    }
    let env = captured.lock().unwrap().clone();
    drop(prepared);
    env.unwrap_or_else(|| panic!("{phase}: the job launch built no environment"))
}

fn assert_uncached_job_starts(f: &Fixture, config: &Config, phase: &str) {
    let boat_disk = config.boat_profile.then(|| config.state_dir());
    // A Boat job is refused unless its workspace sits on the sandbox's mounted,
    // project-quota disk; a portable scratch directory is not on one. That
    // refusal has its own test, so the launch runs without the profile flag and
    // the cache decision under test is the only thing that can fail here.
    let mut config = config.clone();
    config.boat_profile = false;
    config.boat_cold_materialization = false;

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
    let trees = Trees {
        mirror_locks: Mutex::new(BTreeMap::new()),
        mirrors: config.state_dir().join("mirrors"),
        state: config.state_dir(),
        compacting: Arc::default(),
    };
    let prepared = Prepared {
        host_cwd: f.main.clone(),
        target: f.scratch.join("uncached-target"),
        wrapper: None,
        sandbox: Some(Box::new(UncachedIsolationLauncher {
            boat_disk,
            captured: None,
        })),
        vm: None,
    };
    let mut launch = crate::backend::HostBackend
        .launch(&config, &trees, &setup, &prepared, 1)
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

/// CLOUD's broker owns `/run/motor/cache` on Boat, so a test writes the same
/// files and modes into a scratch directory and gives the layout that role.
fn boat_layout(directory: &Path) -> Layout {
    fs::create_dir(directory).unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    Layout::new(directory.to_path_buf(), HostRole::Boat)
}

/// The read-only provisioning set CLOUD installs: `read-key` at 0400 and
/// `config.json` at 0444, both owned by the runner's account.
fn provision_reader(layout: &Layout, enabled: bool) {
    let generation = generation("9", TENANT);
    let key = serde_json::json!({"generation":"9", "access_key_id":generation.read_key.access_key_id,
        "secret_access_key":generation.read_key.secret_access_key});
    let path = layout.key_path(crate::remote_cache::provisioning::KeyRole::Read);
    let temporary = layout.directory().join(".read-key-new");
    fs::write(&temporary, key.to_string()).unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o400)).unwrap();
    fs::rename(temporary, path).unwrap();
    let value = serde_json::json!({"generation":"9", "tenant":TENANT,
        "endpoint":generation.endpoint, "region":generation.region, "bucket":generation.bucket,
        "prefix":generation.prefix, "storage_class":generation.storage_class, "enabled":enabled});
    let path = layout.config_path();
    let temporary = layout.directory().join(".config-new");
    fs::write(&temporary, value.to_string()).unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o444)).unwrap();
    fs::rename(temporary, path).unwrap();
}

#[test]
fn boat_runner_switch_reads_provisioning_and_never_fills() {
    let f = Fixture::new();
    let mut config = f.config();
    config.boat_profile = true;
    config.tenant_vm = true;
    config.isolate_jobs = true;
    config.remote_cache = true;
    let snapshot = |config: &Config| {
        super::super::remote_cache_engine::snapshot(config, &Catalog::default(), 0)
    };
    // A job's environment with the cache on: the read-only store client the
    // gateway signs for, through the job's own keyless loopback forwarder.
    let enabled = |config: &Config| {
        assert_eq!(snapshot(config).state, RemoteCacheState::Available);
        let (generation, forwarders) = config.unit_remote_cache(false).unwrap();
        // The active generation is CLOUD's own file, and each job reaches
        // the gateway only through its own sccache read socket.
        assert_eq!(generation.generation.0, "9");
        assert_eq!(generation.tenant, TENANT);
        assert!(forwarders
            .iter()
            .any(|forwarder| forwarder.tool() == Tool::Sccache));
    };
    // Cached jobs need no cache variables at all: no store endpoint, no
    // wrapper, and, above all, nothing that would make the job fill.
    let uncached = |config: &Config, phase: &str| {
        let env = boat_job_environment(&f, config, phase);
        for name in [
            "SCCACHE_ENDPOINT",
            "SCCACHE_BUCKET",
            "SCCACHE_REGION",
            "SCCACHE_S3_RW_MODE",
            "SCCACHE_RECACHE",
            "RUSTC_WRAPPER",
        ] {
            assert!(
                !env.contains_key(name),
                "{phase}: a Boat job was given {name}"
            );
        }
    };

    // A Boat start reads /run/motor/cache, which CLOUD owns. Starting the
    // runtime must leave that directory alone rather than create or repair it
    // for its root-owned parent to replace under the running runner.
    let directory = f.scratch.join("run-motor-cache");
    let layout = boat_layout(&directory);
    let runtime = runtime(&layout);
    assert_eq!(layout.directory(), directory);
    assert!(directory.is_dir());
    assert!(!layout.directory().join("cache-provisioning").exists());
    assert!(!config
        .state_dir()
        .join(crate::remote_cache::provisioning::STATE_DIRECTORY)
        .exists());
    config.remote_cache_runtime = Some(runtime.clone());

    // The flag alone is not enough: until CLOUD's files arrive the runner is
    // uncached, and a job must still start.
    runtime.watch.reread();
    assert!(config.unit_remote_cache(false).is_none());
    assert_eq!(snapshot(&config).state, RemoteCacheState::CacheUnavailable);
    uncached(&config, "boat before provisioning");

    // CLOUD installs the files after every create and resume, which can be
    // long after the runner started; the watcher's re-read must pick them up.
    provision_reader(&layout, true);
    runtime.watch.reread();
    enabled(&config);
    // Boat never fills, whatever the switch and the provisioning say: the fill
    // path is refused before provisioning is read.
    assert!(config.remote_cache_available(true).is_none());
    assert!(config.unit_remote_cache(true).is_none());

    // The tenant's switch turns jobs uncached again, and back on.
    provision_reader(&layout, false);
    runtime.watch.reread();
    assert_eq!(snapshot(&config).state, RemoteCacheState::Off);
    provision_reader(&layout, true);
    runtime.watch.reread();
    enabled(&config);

    // CLOUD never installs a read-write key on Boat. A stray one must not
    // reach a job either: Boat reads only the read-only key, so an extra file
    // neither grants writes nor takes the working read key away.
    let write_key = layout.key_path(crate::remote_cache::provisioning::KeyRole::Write);
    fs::write(
        &write_key,
        serde_json::json!({"generation":"9", "access_key_id":"read-write", "secret_access_key":"s"})
            .to_string(),
    )
    .unwrap();
    fs::set_permissions(&write_key, fs::Permissions::from_mode(0o400)).unwrap();
    runtime.watch.reread();
    assert_eq!(snapshot(&config).state, RemoteCacheState::Available);
    assert_eq!(
        crate::remote_cache::provisioning::HostRole::Boat.keys(),
        &[crate::remote_cache::provisioning::KeyRole::Read]
    );
    fs::remove_file(&write_key).unwrap();

    // A file whose mode differs from the contract is not read: the read-only
    // key's mode is what keeps that credential from other accounts. The last
    // complete generation stays active, so one drifted file does not drop the
    // cache out from under running jobs.
    let drifted = layout.key_path(crate::remote_cache::provisioning::KeyRole::Read);
    fs::set_permissions(&drifted, fs::Permissions::from_mode(0o444)).unwrap();
    runtime.watch.reread();
    assert_eq!(snapshot(&config).state, RemoteCacheState::Available);
    enabled(&config);

    // A new generation with the key back at the contract's mode replaces the
    // drifted set without a restart, which is how a broker reinstall recovers
    // from a write that landed with the wrong mode.
    fs::set_permissions(&drifted, fs::Permissions::from_mode(0o400)).unwrap();
    provision_reader(&layout, true);
    runtime.watch.reread();
    enabled(&config);

    // Neither the cache nor any of its states gives Boat a way to write: the
    // fill path is refused before provisioning is read.
    assert!(config.remote_cache_available(true).is_none());
    assert!(config.unit_remote_cache(true).is_none());
}

#[test]
fn boat_manifest_switch_turns_the_cache_on_without_a_runtime() {
    // The switch decides whether the runner even reads provisioning, so it has
    // to be on from the first start, before CLOUD's files exist.
    let f = Fixture::new();
    let mut config = f.config();
    config.boat_profile = true;
    config.tenant_vm = true;
    config.isolate_jobs = true;
    assert!(config.remote_cache_available(false).is_none());
    config.remote_cache = true;
    assert!(config.remote_cache_available(false).is_some());
    assert!(config.remote_cache_available(true).is_none());
    // Without job isolation the switch is ignored on every host type.
    config.isolate_jobs = false;
    assert!(config.remote_cache_available(false).is_none());
}
