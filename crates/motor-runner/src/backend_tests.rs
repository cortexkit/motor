use super::*;
use cortexkit_test_support::ScratchDir;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[cfg(target_os = "linux")]
#[test]
fn boat_launch_rechecks_storage_and_workspace() {
    // Use /dev/shm so this scratch directory is on a different device from
    // root, even when the usual test temp directory shares root's XFS disk.
    let scratch = PathBuf::from("/dev/shm").join(format!("motor-boat-storage-{}", Uuid::now_v7()));
    fs::create_dir(&scratch).unwrap();
    let state = scratch.join("state");
    let workspace = scratch.join("workspace");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&workspace).unwrap();
    verify_boat_storage(&scratch, &state, &workspace).unwrap();
    let error = verify_boat_storage(Path::new("/proc"), &state, &workspace).unwrap_err();
    assert!(error
        .to_string()
        .contains("/Users is not on the runner state disk"));
    fs::remove_dir(&workspace).unwrap();
    let error = verify_boat_storage(&scratch, &state, &workspace).unwrap_err();
    assert!(error.to_string().contains("before job launch"));
    fs::write(&workspace, "not a directory").unwrap();
    assert!(verify_boat_storage(&scratch, &state, &workspace).is_err());
    assert!(verify_boat_storage(Path::new("/"), Path::new("/"), Path::new("/")).is_err());
    fs::remove_dir_all(&scratch).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn boat_backend_refuses_storage_before_dispatch() {
    let scratch = ScratchDir::new("boat-launch-storage");
    let mut config = Config::new(scratch.join("runner"), "boat");
    config.boat_profile = true;
    config.isolate_jobs = true;
    let setup = setup(&scratch);
    let prepared = Prepared {
        cwd: scratch.join("missing-workspace"),
        target: scratch.join("target"),
        sandbox: None,
        wrapper: None,
        vm: None,
    };
    let error = HostBackend
        .launch(&config, &trees(&config), &setup, &prepared, 1)
        .err()
        .unwrap();
    assert!(error
        .to_string()
        .contains("Boat storage verification failed before job launch"));
}

#[cfg(target_os = "linux")]
#[test]
fn boat_launch_without_a_quota_lease_never_starts_even_without_isolation() {
    let scratch = ScratchDir::new("boat-launch-no-quota");
    let mut config = Config::new(scratch.join("runner"), "boat");
    config.boat_profile = true;
    let mut setup = setup(scratch.path());
    setup.request.base_commit = Some("source".into());
    let prepared = Prepared {
        cwd: scratch.path().to_owned(),
        target: scratch.join("target"),
        sandbox: None,
        wrapper: None,
        vm: None,
    };
    match HostBackend.launch(&config, &trees(&config), &setup, &prepared, 1) {
        Ok(mut launch) => {
            // The native fixture holds its harmless `exit 0` behind a gate.
            // Release and reap it before reporting the missing quota backstop.
            launch.execution.release().unwrap();
            let _ = launch.execution.wait();
            launch.execution.reap();
            panic!("Boat launched without a quota lease");
        }
        Err(error) => assert!(error.to_string().contains("Boat quota lease"), "{error}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn boat_cached_depth_is_bounded_before_permission_repair() {
    let scratch = ScratchDir::new("boat-preflight-before-repair");
    let workspace = scratch.join("remote/worktrees/deep");
    fs::create_dir_all(&workspace).unwrap();
    let _tree = crate::boat_quota::DeepTree::unlabelled(&workspace, 2200, 1);
    let mut config = Config::new(scratch.join("runner"), "boat");
    config.boat_profile = true;
    config.isolate_jobs = true;
    config.workspace_roots = vec![scratch.join("remote/worktrees")];
    config.owned_workspace_roots = vec![scratch.join("remote")];
    crate::systemd::with_process_hook(
        Box::new(|_| panic!("privileged permission repair ran before the quota preflight")),
        || {
            let error = HostBackend
                .materialize(&config, &trees(&config), &setup(&workspace))
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("traversal depth budget"),
                "{error}"
            );
        },
    );
}

#[cfg(target_os = "linux")]
#[test]
fn boat_cached_long_names_request_cold_eviction_before_path_based_helpers() {
    let scratch = ScratchDir::new("boat-preflight-long-names");
    let workspace = scratch.join("remote/worktrees/long");
    fs::create_dir_all(&workspace).unwrap();
    let _tree = crate::boat_quota::DeepTree::unlabelled(&workspace, 30, 200);
    let mut config = Config::new(scratch.join("runner"), "boat");
    config.boat_profile = true;
    config.isolate_jobs = true;
    config.workspace_roots = vec![scratch.join("remote/worktrees")];
    config.owned_workspace_roots = vec![scratch.join("remote")];
    crate::systemd::with_process_hook(
        Box::new(|_| panic!("path-based permission helper reached an over-PATH_MAX cached tree")),
        || {
            let error = HostBackend
                .materialize(&config, &trees(&config), &setup(&workspace))
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("pathname-length budget"),
                "{error}"
            );
            assert!(
                crate::boat_quota::cold_cache(&error).is_some(),
                "cached path rejection must request the ordinary cold eviction"
            );
        },
    );
}

fn trees(config: &Config) -> Trees {
    Trees {
        dependency_locks: Mutex::new(BTreeMap::new()),
        mirrors: config.state_dir().join("mirrors"),
        state: config.state_dir(),
        compacting: Arc::default(),
    }
}

fn setup(cwd: &Path) -> Setup {
    let cwd = cwd.to_string_lossy().into_owned();
    let id = Uuid::now_v7();
    Setup {
        id,
        cpus: vec![1, 17],
        request: Run {
            return_paths: Vec::new(),
            return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
            return_chunked: false,
            gate_job: false,
            network: None,
            platform: None,
            job_id: id,
            base_commit: None,
            workspace_kind: None,
            git_dependencies: Vec::new(),
            workspace_key: cwd.clone(),
            repository_root: cwd.clone(),
            cwd,
            command: "exit 0".into(),
            env: BTreeMap::from([("RUSTFLAGS".into(), "-C debuginfo=1".into())]),
            weight_hint: None,
            timeout: None,
            queue_wait_limit_s: 300,
            snapshot_commit: String::new(),
            tree_hash: String::new(),
            integration_tip: String::new(),
            siblings: Vec::new(),
            bundles: Vec::new(),
        },
        base: false,
        recorded: BTreeMap::new(),
        snapshots: None,
    }
}

#[derive(Debug, PartialEq)]
struct LaunchCall {
    command: String,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    cpus: Vec<u32>,
    requested_vcpus: u32,
}

/// Records dispatch through the same IsolationLauncher boundary as systemd.
/// A direct process launch would neither record the call nor return the
/// PermissionDenied error with the "isolation launcher observed" message.
struct ObservedIsolationLauncher(Arc<Mutex<Option<LaunchCall>>>);

impl IsolationLauncher for ObservedIsolationLauncher {
    fn launch(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        cpus: &[u32],
        requested_vcpus: u32,
    ) -> io::Result<platform::Launch> {
        *self.0.lock().unwrap() = Some(LaunchCall {
            command: command.into(),
            cwd: cwd.into(),
            env: env.iter().cloned().collect(),
            cpus: cpus.to_vec(),
            requested_vcpus,
        });
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "isolation launcher observed",
        ))
    }
}

fn launch_error(result: io::Result<Launch>) -> io::Error {
    match result {
        Err(error) => error,
        Ok(mut launch) => {
            // A broken isolation path may have spawned a child waiting for the
            // execution gate. Release, wait for and reap it before failing the
            // assertion so the test leaves no waiting process behind.
            launch.execution.release().unwrap();
            launch.execution.wait().unwrap();
            launch.execution.reap();
            panic!("HostBackend bypassed the prepared isolation launcher");
        }
    }
}

#[test]
fn host_backend_launch_uses_isolation_launcher() {
    let scratch = ScratchDir::new("motor-host-launch-dispatch");
    let mut config = Config::new(scratch.join("state"), "main");
    config.isolate_jobs = true;
    let setup = setup(scratch.path());
    let observed = Arc::new(Mutex::new(None));
    let prepared = Prepared {
        cwd: scratch.path().into(),
        target: scratch.join("target"),
        wrapper: Some("/motor-tools/prefrontal-build-slot".into()),
        sandbox: Some(Box::new(ObservedIsolationLauncher(observed.clone()))),
        vm: None,
    };
    let error = launch_error(HostBackend.launch(&config, &trees(&config), &setup, &prepared, 8));
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "isolation launcher observed");
    let observed = observed.lock().unwrap();
    let call = observed
        .as_ref()
        .expect("isolation launcher must be invoked");
    assert_eq!(call.command, "exit 0");
    assert_eq!(call.cpus, setup.cpus);
    assert_eq!(call.requested_vcpus, 8);
    assert_eq!(call.cwd, scratch.path());
    assert_eq!(
        call.env["CARGO_TARGET_DIR"],
        prepared.target.to_string_lossy()
    );
    assert_eq!(
        call.env["RUSTC_WRAPPER"],
        "/motor-tools/prefrontal-build-slot"
    );
    assert_eq!(call.env["RUSTFLAGS"], "-C debuginfo=1");
}

#[test]
fn host_backend_refuses_isolated_launch_without_prepared_sandbox() {
    let scratch = ScratchDir::new("motor-host-launch-missing-sandbox");
    let mut config = Config::new(scratch.join("state"), "main");
    config.isolate_jobs = true;
    let prepared = Prepared {
        cwd: scratch.path().into(),
        target: scratch.join("target"),
        wrapper: None,
        sandbox: None,
        vm: None,
    };
    let error = launch_error(HostBackend.launch(
        &config,
        &trees(&config),
        &setup(scratch.path()),
        &prepared,
        8,
    ));
    assert_eq!(error.to_string(), "isolated job has no prepared sandbox");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires trusted operator, motor-job account, ACL tools and systemd private PID support"]
fn isolated_host_backend_launch_runs_in_motor_job_service_with_022_umask() {
    let scratch = ScratchDir::new("motor-host-isolated-service");
    let workspace = scratch.join("workspace");
    let target = workspace.join("target");
    let state = scratch.join("state");
    let cleanup = crate::test_cleanup::Cleanup::default();
    fs::create_dir_all(&target).unwrap();
    fs::create_dir_all(&state).unwrap();
    let mut config = Config::new(&state, "main");
    config.isolate_jobs = true;
    let mut setup = setup(&workspace);
    setup.request.command = "id -un; umask".into();
    let sandbox = crate::isolation::Sandbox::prepare(
        setup.id,
        &state,
        &workspace,
        &target,
        &[],
        None,
        &config.build_slot_root,
        config.rustc_slots,
    )
    .unwrap();
    let prepared = Prepared {
        cwd: workspace,
        target,
        wrapper: None,
        sandbox: Some(Box::new(sandbox)),
        vm: None,
    };
    let mut launch = HostBackend
        .launch(&config, &trees(&config), &setup, &prepared, 8)
        .unwrap();
    cleanup.0.lock().unwrap().register(launch.process.unwrap());
    let unit = launch.process.unwrap().unit;
    launch.execution.release().unwrap();
    let status = launch.execution.wait().unwrap();
    let output = state
        .join("sandboxes")
        .join(setup.id.to_string())
        .join("output");
    let stdout = fs::read_to_string(output.join("stdout")).unwrap();
    let stderr = fs::read_to_string(output.join("stderr")).unwrap();
    launch.execution.reap();
    assert!(
        unit.is_some(),
        "isolated job must retain its systemd unit identity"
    );
    assert!(status.success(), "{stderr}");
    assert_eq!(stdout, "motor-job\n0022\n");
}

#[cfg(target_os = "linux")]
#[test]
fn hosted_jobs_bypass_shared_compile_cache_but_keep_the_fixed_target_folder() {
    for rust in [false, true] {
        let scratch = ScratchDir::new("motor-hosted-cache-env");
        if rust {
            fs::write(scratch.join("Cargo.toml"), "[workspace]\n").unwrap();
        }
        for (boat, tenant) in [(false, false), (true, false), (false, true), (true, true)] {
            for supervisor in [false, true] {
                for writer in [false, true] {
                    // Writers always fill a Rust workspace.
                    if writer && !rust {
                        continue;
                    }
                    hosted_cache_case(&scratch, rust, boat, tenant, supervisor, writer);
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn tenant_rust_launch_builds_in_fixed_target_without_compile_cache() {
    let scratch = ScratchDir::new("motor-tenant-launch-target");
    fs::write(scratch.join("Cargo.toml"), "[workspace]\n").unwrap();
    let mut config = Config::new(scratch.join("state"), "main");
    config.isolate_jobs = true;
    config.tenant_vm = true;
    let mut setup = setup(scratch.path());
    setup
        .request
        .env
        .insert("SCCACHE_DIR".into(), "/submitted".into());
    let observed = Arc::new(Mutex::new(None));
    let prepared = Prepared {
        cwd: scratch.path().into(),
        target: scratch.join("target"),
        wrapper: None,
        sandbox: Some(Box::new(ObservedIsolationLauncher(observed.clone()))),
        vm: None,
    };
    let error = launch_error(HostBackend.launch(&config, &trees(&config), &setup, &prepared, 2));
    assert_eq!(error.to_string(), "isolation launcher observed");
    let observed = observed.lock().unwrap();
    let env = &observed.as_ref().unwrap().env;
    assert_eq!(env["CARGO_TARGET_DIR"], "/run/motor/target");
    assert!(!env.contains_key("RUSTC_WRAPPER"), "{env:?}");
    assert!(
        !env.keys().any(|key| key.starts_with("SCCACHE_")),
        "{env:?}"
    );
}

#[cfg(target_os = "linux")]
fn hosted_cache_case(
    scratch: &ScratchDir,
    rust: bool,
    boat: bool,
    tenant: bool,
    supervisor: bool,
    writer: bool,
) {
    let case =
        format!("rust={rust} boat={boat} tenant={tenant} supervisor={supervisor} writer={writer}");
    let mut config = Config::new(scratch.join("state"), "main");
    config.isolate_jobs = true;
    config.boat_profile = boat;
    config.tenant_vm = tenant;
    let mut setup = setup(scratch.path());
    setup.base = writer;
    let prepared = Prepared {
        cwd: scratch.path().into(),
        target: scratch.join("target"),
        wrapper: supervisor.then(|| "/motor-tools/prefrontal-build-slot".into()),
        sandbox: None,
        vm: None,
    };
    let mut env = BTreeMap::from([(
        "CARGO_TARGET_DIR".into(),
        prepared.target.to_string_lossy().into_owned(),
    )]);
    compile_cache_environment(&config, &setup, &prepared, &mut env).unwrap();
    let mut read_only = Vec::new();
    let mut read_write = Vec::new();
    let mut destinations = Vec::new();
    crate::isolation::compile_cache_mounts(
        &config.state_dir(),
        scratch.path(),
        &prepared.target,
        writer,
        config.shared_compile_cache(),
        &mut read_only,
        &mut read_write,
        &mut destinations,
    )
    .unwrap();
    // An isolated Rust job builds in /run/motor/target on every Linux runner, whether or
    // not it has the shared compile cache: that folder is bound to the job's
    // own target folder, so `./target` and CARGO_TARGET_DIR are one directory.
    let fixed_target = format!("{}:/run/motor/target", prepared.target.display());
    if rust {
        assert_eq!(env["CARGO_TARGET_DIR"], "/run/motor/target", "{case}");
        assert!(read_write.contains(&fixed_target), "{case}: {read_write:?}");
        assert!(
            destinations.contains(&PathBuf::from("/run/motor/target")),
            "{case}: {destinations:?}"
        );
    } else {
        assert_eq!(
            env["CARGO_TARGET_DIR"],
            prepared.target.to_string_lossy(),
            "{case}"
        );
        assert!(
            read_only.is_empty() && read_write.is_empty() && destinations.is_empty(),
            "{case}"
        );
    }
    if boat || tenant {
        // Hosted runners have no shared compile cache: no wrapper, no sccache
        // settings and no cache mount.
        assert!(!env.contains_key("RUSTC_WRAPPER"), "{case}: {env:?}");
        assert!(
            !env.keys().any(|key| key.starts_with("SCCACHE_")),
            "{case}: {env:?}"
        );
        assert!(
            !read_only
                .iter()
                .chain(&read_write)
                .any(|mount| mount.ends_with(":/motor-cache")),
            "{case}"
        );
        assert!(
            !destinations.contains(&PathBuf::from("/motor-cache")),
            "{case}"
        );
    } else if rust {
        assert_eq!(
            env["RUSTC_WRAPPER"],
            if supervisor {
                "/motor-tools/prefrontal-build-slot"
            } else {
                "sccache"
            },
            "{case}"
        );
        assert_eq!(env["SCCACHE_DIR"], "/motor-cache", "{case}");
        assert_eq!(
            env["SCCACHE_LOCAL_RW_MODE"],
            if writer { "READ_WRITE" } else { "READ_ONLY" },
            "{case}"
        );
        let mounts = if writer { &read_write } else { &read_only };
        assert!(
            mounts.iter().any(|mount| mount.ends_with(":/motor-cache")),
            "{case}: {mounts:?}"
        );
        assert!(
            destinations.contains(&PathBuf::from("/motor-cache")),
            "{case}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn isolated_launch_cache_environment_preserves_wrapper_precedence() {
    for rust in [false, true] {
        for supervisor in [false, true] {
            for configured in [false, true] {
                let scratch = ScratchDir::new("motor-reader-launch-env");
                if rust {
                    fs::write(scratch.join("Cargo.toml"), "[workspace]\n").unwrap();
                }
                if configured {
                    fs::create_dir_all(scratch.join(".cargo")).unwrap();
                    fs::write(
                        scratch.join(".cargo/config.toml"),
                        "[build]\nrustc-wrapper='sentinel'\n",
                    )
                    .unwrap();
                }
                let mut config = Config::new(scratch.join("state"), "main");
                config.isolate_jobs = true;
                config.sccache_size_gib = 17;
                let mut setup = setup(scratch.path());
                setup
                    .request
                    .env
                    .insert("RUSTC_WRAPPER".into(), "submitted-sentinel".into());
                setup
                    .request
                    .env
                    .insert("CARGO_INCREMENTAL".into(), "1".into());
                setup
                    .request
                    .env
                    .insert("CARGO_NET_OFFLINE".into(), "false".into());
                let observed = Arc::new(Mutex::new(None));
                let prepared = Prepared {
                    cwd: scratch.path().into(),
                    target: scratch.join("target"),
                    wrapper: supervisor.then(|| "/motor-tools/prefrontal-build-slot".into()),
                    sandbox: Some(Box::new(ObservedIsolationLauncher(observed.clone()))),
                    vm: None,
                };
                let error = launch_error(HostBackend.launch(
                    &config,
                    &trees(&config),
                    &setup,
                    &prepared,
                    2,
                ));
                assert_eq!(error.to_string(), "isolation launcher observed");
                let observed = observed.lock().unwrap();
                let env = &observed.as_ref().unwrap().env;
                let expected = if supervisor {
                    Some("/motor-tools/prefrontal-build-slot")
                } else if rust && !configured {
                    Some("sccache")
                } else {
                    None
                };
                assert_eq!(env.get("RUSTC_WRAPPER").map(String::as_str), expected);
                if rust {
                    assert_eq!(env["CARGO_TARGET_DIR"], "/run/motor/target");
                    assert_eq!(env["CARGO_HOME"], "/motor-home/cargo");
                    assert_eq!(env["CARGO_NET_OFFLINE"], "true");
                    assert!(!env.contains_key("CARGO_INCREMENTAL"));
                    assert_eq!(env["SCCACHE_DIR"], "/motor-cache");
                    assert_eq!(env["SCCACHE_CACHE_SIZE"], "17G");
                    assert_eq!(env["SCCACHE_LOCAL_RW_MODE"], "READ_ONLY");
                    assert_eq!(env["SCCACHE_IGNORE_SERVER_IO_ERROR"], "1");
                } else {
                    assert!(!env.keys().any(|key| key.starts_with("SCCACHE_")));
                    assert_eq!(env["CARGO_TARGET_DIR"], prepared.target.to_string_lossy());
                }
            }
        }
    }
}

#[test]
fn a_long_job_command_in_the_unit_description_does_not_refuse_the_listing() {
    // systemd-run puts the whole command line in the description column, so
    // a caller's long command produced a row of several kilobytes and every
    // later admission failed with "listing exceeds row limit".
    let long = "x".repeat(64 * 1024);
    let listing = format!(
        "motor-job-a.service loaded active running [systemd-run] /bin/bash -c {long}\n\
         motor-job-b.service loaded inactive dead [systemd-run] /bin/bash -c true\n"
    );
    let units = super::parse_live_job_units(listing.as_bytes()).unwrap();
    assert_eq!(
        units.into_iter().collect::<Vec<_>>(),
        vec!["motor-job-a.service"]
    );
}

#[test]
fn the_listing_stays_bounded_by_rows_and_total_size() {
    let rows: String = (0..10_001)
        .map(|i| format!("motor-job-{i}.service loaded active running d\n"))
        .collect();
    assert!(super::parse_live_job_units(rows.as_bytes()).is_err());
    let huge = vec![b'x'; 64 * 1024 * 1024 + 1];
    assert!(super::parse_live_job_units(&huge).is_err());
}
