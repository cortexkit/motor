use super::*;
use cortexkit_test_support::ScratchDir;
use std::{
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    process::ExitStatus,
    sync::{Arc, Mutex},
};

fn output(code: i32, stdout: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

/// A helper whose main process exited with status 0: with RemainAfterExit
/// the unit stays loaded and active in the `exited` state.
const SUCCEEDED: &str = "LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\n\
                         ExecMainCode=1\nExecMainStatus=0\n";

/// A successful unit once it has been stopped and unloaded.
const UNLOADED: &str = "LoadState=not-found\nActiveState=inactive\nSubState=dead\n";

/// A config whose systemctl calls are recorded in `calls` and answered with
/// `before` for `show` until the runner asks systemd to stop the unit, and
/// with `after` from then on. Helper starts are recorded in `launched` and
/// succeed.
fn fake(
    scratch: &ScratchDir,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    before: &'static str,
    after: &'static str,
    launched: Arc<Mutex<Vec<Vec<String>>>>,
) -> Config {
    let mut config = Config::new(scratch.join("runner"), "main");
    fs::create_dir_all(config.state_dir()).unwrap();
    config.systemd_command = Some(Arc::new(move |args| {
        let mut calls = calls.lock().unwrap();
        calls.push(args.iter().map(|arg| arg.to_string()).collect());
        let stopped = calls.iter().any(|call| call[0] == "stop");
        Ok(output(
            0,
            match (args[0], stopped) {
                ("show", false) => before,
                ("show", true) => after,
                _ => "",
            },
        ))
    }));
    config.snapshot_helper_launcher = Some(Arc::new(move |args| {
        launched.lock().unwrap().push(args.to_vec());
        Ok(output(0, ""))
    }));
    config
}

/// A config whose helper ends as `state` says.
fn ending(scratch: &ScratchDir, state: &'static str) -> Config {
    fake(scratch, Arc::default(), state, UNLOADED, Arc::default())
}

fn shell(directory: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exit 0"])
        .current_dir(directory)
        .env("MOTOR_HELPER_SET", "a \"quoted\" $value\\")
        .env_remove("HOME");
    command
}

fn run_prefetch(config: &Config, scratch: &ScratchDir, limit: Duration) -> io::Result<()> {
    let view = View::prefetch(&scratch.join("w/a"), &[]).unwrap();
    run(
        config,
        Helper::CargoPrefetch,
        limit,
        &shell(&config.state_dir()),
        &view,
    )
}

#[test]
fn tenant_snapshot_helpers_bind_daemon_dns_without_hiding_etc() {
    let scratch = ScratchDir::new("tenant-helper-dns");
    let mut config = Config::new(&*scratch, "tenant-dns");
    fs::create_dir_all(config.state_dir()).unwrap();
    let command = shell(&config.state_dir());
    let view = View::prefetch(&scratch.join("worktrees/a"), &[]).unwrap();
    let files = Files::new(&config, Uuid::now_v7());
    let directory = scratch.join("resolver");
    let resolver = directory.join("resolv.conf");
    let marker = scratch.join("dns-marker");
    // The state a running runner derives, from this config and from the
    // attachment's marker (read from a test file instead of /run).
    let tenant = |config: &Config| {
        TenantResolver::under(config.tenant_vm, config.boat_profile, &marker, &directory)
    };
    // The read-only binding of /etc/resolv.conf the unit would get, if any.
    let resolv = |arguments: &[String]| {
        arguments
            .iter()
            .filter_map(|arg| arg.strip_prefix("--property=BindReadOnlyPaths="))
            .flat_map(str::split_whitespace)
            .find(|binding| binding.ends_with(":/etc/resolv.conf"))
            .map(str::to_owned)
    };
    let run = |config: &Config, tenant: TenantResolver| {
        arguments(
            config,
            "motor-snaphelper-tenant-dns-test.service",
            Duration::from_secs(120),
            &command,
            &view,
            &files,
            tenant,
        )
    };

    config.tenant_vm = false;
    config.boat_profile = false;
    let host = run(&config, tenant(&config)).unwrap();
    assert_eq!(resolv(&host), None, "{host:?}");

    // Boat runs as --tenant-vm in a sandbox that no tenant ever attached to: it
    // has no private resolver and its /etc/resolv.conf is the sandbox's own, so
    // its helpers get the resolver they always had before tenant helpers needed
    // a bind, and the unit arguments are the same as on an unattached host.
    config.tenant_vm = true;
    config.boat_profile = true;
    let boat = run(&config, tenant(&config)).unwrap();
    assert_eq!(resolv(&boat), None, "{boat:?}");
    let without_unit = |arguments: &[String]| {
        arguments
            .iter()
            .filter(|arg| !arg.starts_with("--unit="))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(without_unit(&boat), without_unit(&host), "{boat:?}");

    // A tenant whose attachment installed the private resolver gets it bound.
    config.boat_profile = false;
    crate::tenant_agent::dns::install(&directory, &["1.1.1.1".into()], unsafe { libc::geteuid() })
        .unwrap();
    fs::write(&marker, "1\n").unwrap();
    let attached = run(&config, tenant(&config)).unwrap();
    assert_eq!(
        resolv(&attached),
        Some(bind(&resolver, Path::new("/etc/resolv.conf")).unwrap()),
        "{attached:?}"
    );

    for arguments in [&host, &boat, &attached] {
        assert!(!arguments
            .iter()
            .any(|arg| arg.contains("TemporaryFileSystem=/etc")));
        assert!(!arguments
            .iter()
            .any(|arg| arg.contains("PrivateNetwork=") || arg.contains("NetworkNamespacePath=")));
    }

    // A tenant that expects a private resolver but has none fails by name
    // instead of resolving through the VM's empty global resolver.
    fs::remove_file(&resolver).unwrap();
    let error = run(&config, tenant(&config)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the tenant's private resolver is missing"),
        "{error}"
    );
    fs::remove_file(&marker).unwrap();
    let error = run(&config, tenant(&config)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the tenant's private resolver is missing"),
        "{error}"
    );
}

#[test]
fn snapshot_helper_names_match_only_their_own_instance() {
    let id = Uuid::now_v7();
    assert!(belongs_to("main", &unit_name("main", id)));
    // `main-2-<uuid>` starts with `main-`, but `2-<uuid>` is not a UUID.
    assert!(!belongs_to("main", &unit_name("main-2", id)));
    assert!(belongs_to("main-2", &unit_name("main-2", id)));
    assert!(!belongs_to("main-2", &unit_name("main", id)));
    for unit in [
        format!("motor-snaphelper-main-{}.service", id.simple()),
        format!(
            "motor-snaphelper-main-{}.service",
            id.hyphenated().to_string().to_uppercase()
        ),
        format!("motor-snaphelper-main-{id}"),
        format!("motor-snaphelper-main-{id}.service.bak"),
        format!("motor-job-{id}.service"),
        "motor-snaphelper-main-.service".to_string(),
    ] {
        assert!(!belongs_to("main", &unit), "{unit}");
    }
}

#[test]
fn snapshot_prefetch_helper_binds_snapshots_at_sibling_paths_and_hides_neighbours() {
    let scratch = ScratchDir::new("snapshot-helper-arguments");
    let launched: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let mut config = fake(
        &scratch,
        Arc::default(),
        SUCCEEDED,
        UNLOADED,
        launched.clone(),
    );
    config.snapshot_prefetch_timeout = Duration::from_secs(321);
    let workspace = scratch.join("worktrees/a");
    let sibling = scratch.join("source/b b");
    let snapshot = config.sibling_snapshots_dir().join("b/1234");
    for path in [&workspace, &sibling, &snapshot] {
        fs::create_dir_all(path).unwrap();
    }
    let view = View::prefetch(&workspace, &[(snapshot.clone(), sibling.clone())]).unwrap();
    run(
        &config,
        Helper::CargoPrefetch,
        Helper::CargoPrefetch.limit(&config),
        &shell(&config.state_dir()),
        &view,
    )
    .unwrap();
    let launched = launched.lock().unwrap();
    let arguments = &launched[0];
    let unit = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--unit="))
        .unwrap();
    assert!(belongs_to("main", unit), "{unit}");
    let w = |path: &Path| word(path).unwrap();
    for expected in [
        "--service-type=exec".to_string(),
        "--property=Slice=motor-jobs.slice".into(),
        "--property=OOMScoreAdjust=900".into(),
        "--property=OOMPolicy=kill".into(),
        // The unit stays loaded after a successful exit, so the runner can
        // read how its main process ended.
        "--property=RemainAfterExit=yes".into(),
        format!("--uid={}", unsafe { libc::geteuid() }),
        "--property=RuntimeMaxSec=321000ms".into(),
        "--property=KillMode=control-group".into(),
        "--property=SendSIGKILL=yes".into(),
        format!("--property=WorkingDirectory={}", w(&config.state_dir())),
        // Both the worktree's parent and the sibling's parent are hidden, so
        // the helper sees only the declared checkouts there.
        format!(
            "--property=TemporaryFileSystem={}:ro",
            w(&scratch.join("source"))
        ),
        format!(
            "--property=TemporaryFileSystem={}:ro",
            w(&scratch.join("worktrees"))
        ),
        format!(
            "--property=BindReadOnlyPaths={} {} {}",
            bind(&workspace, &workspace).unwrap(),
            bind(&snapshot, &sibling).unwrap(),
            bind(&snapshot, &scratch.join("worktrees/b b")).unwrap(),
        ),
    ] {
        assert!(
            arguments.contains(&expected),
            "{expected} missing from {arguments:?}"
        );
    }
    // A path's space cannot split it into two mount entries.
    assert!(w(&sibling).ends_with("b\\x20b"));
    let separator = arguments.iter().position(|a| a == "--").unwrap();
    assert_eq!(arguments[separator + 1..], ["/bin/sh", "-c", "exit 0"]);
    // The environment travels in a file, never as unit properties.
    assert!(!arguments.iter().any(|a| a.contains("MOTOR_HELPER_SET")));
    // The helper's output goes to a file, not through systemd-run, whose own
    // exit status says nothing about how the main process ended.
    assert!(!arguments.iter().any(|a| a == "--wait" || a == "--pipe"));
    assert!(arguments
        .iter()
        .any(|a| a.starts_with("--property=StandardError=append:")));
}

#[test]
fn snapshot_javascript_helper_runs_in_the_snapshot_at_the_sibling_path() {
    let scratch = ScratchDir::new("snapshot-helper-javascript");
    let launched: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let config = fake(
        &scratch,
        Arc::default(),
        SUCCEEDED,
        UNLOADED,
        launched.clone(),
    );
    let sibling = scratch.join("source/b");
    let snapshot = config.sibling_snapshots_dir().join("b/1234.tmp-x");
    fs::create_dir_all(snapshot.join("web")).unwrap();
    let view = View::javascript(&snapshot, &sibling);
    run(
        &config,
        Helper::JavascriptInstall,
        Helper::JavascriptInstall.limit(&config),
        &shell(&snapshot.join("web")),
        &view,
    )
    .unwrap();
    let arguments = &launched.lock().unwrap()[0];
    for expected in [
        format!(
            "--property=BindPaths={}",
            bind(&snapshot, &sibling).unwrap()
        ),
        format!(
            "--property=WorkingDirectory={}",
            word(&sibling.join("web")).unwrap()
        ),
        format!(
            "--property=TemporaryFileSystem={}:ro",
            word(&scratch.join("source")).unwrap()
        ),
        format!(
            "--property=RuntimeMaxSec={}ms",
            DEFAULT_LIMIT_FOR_TESTS.as_millis()
        ),
    ] {
        assert!(
            arguments.contains(&expected),
            "{expected} missing from {arguments:?}"
        );
    }
}

const DEFAULT_LIMIT_FOR_TESTS: Duration = Duration::from_secs(15 * 60);

#[test]
fn snapshot_helper_environment_is_private_and_removed_after_the_run() {
    let scratch = ScratchDir::new("snapshot-helper-environment");
    let seen: Arc<Mutex<Option<(u32, String)>>> = Arc::default();
    let mut config = ending(&scratch, SUCCEEDED);
    let observed = seen.clone();
    config.snapshot_helper_launcher = Some(Arc::new(move |args| {
        let file = args
            .iter()
            .find_map(|a| a.strip_prefix("--property=EnvironmentFile="))
            .unwrap();
        let mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;
        *observed.lock().unwrap() = Some((mode, fs::read_to_string(file).unwrap()));
        Ok(output(0, ""))
    }));
    let view = View::prefetch(&scratch.join("w/a"), &[]).unwrap();
    run(
        &config,
        Helper::CargoPrefetch,
        Duration::from_secs(5),
        &shell(&config.state_dir()),
        &view,
    )
    .unwrap();
    let (mode, text) = seen.lock().unwrap().take().unwrap();
    assert_eq!(mode, 0o600);
    assert!(
        text.contains("MOTOR_HELPER_SET=\"a \\\"quoted\\\" \\$value\\\\\"\n"),
        "{text}"
    );
    // A variable the command removed is not passed on.
    assert!(
        !text.lines().any(|line| line.starts_with("HOME=")),
        "{text}"
    );
    assert_eq!(
        fs::read_dir(config.state_dir().join(ENVIRONMENT_DIRECTORY))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn snapshot_helper_timeout_fails_setup_naming_the_helper_and_its_limit() {
    let scratch = ScratchDir::new("snapshot-helper-timeout");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let mut config = fake(
        &scratch,
        calls.clone(),
        "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=timeout\n\
         ExecMainCode=2\nExecMainStatus=15\n",
        UNLOADED,
        Arc::default(),
    );
    // The unit runs for its whole limit before systemd stops it.
    config.snapshot_helper_launcher = Some(Arc::new(|_| {
        std::thread::sleep(Duration::from_millis(20));
        Ok(output(0, ""))
    }));
    let error = run_prefetch(&config, &scratch, Duration::from_millis(10)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let message = error.to_string();
    for expected in [
        "Cargo prefetch helper unit motor-snaphelper-main-",
        "time limit of 0.01 s",
        "--snapshot-prefetch-timeout-s",
    ] {
        assert!(message.contains(expected), "{message}");
    }
    // The failed unit is unloaded once its result has been read.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.last().unwrap()[0], "reset-failed");
}

#[test]
fn snapshot_helper_failure_is_not_reported_as_a_timeout() {
    let scratch = ScratchDir::new("snapshot-helper-failure");
    let config = ending(
        &scratch,
        "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\n\
         ExecMainCode=1\nExecMainStatus=3\n",
    );
    let view = View::javascript(&scratch.join("snapshot"), &scratch.join("source/b"));
    fs::create_dir_all(scratch.join("snapshot")).unwrap();
    let error = run(
        &config,
        Helper::JavascriptInstall,
        Duration::from_secs(5),
        &shell(&scratch.join("snapshot")),
        &view,
    )
    .unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::TimedOut);
    let message = error.to_string();
    assert!(
        message.contains("JavaScript install helper unit motor-snaphelper-main-")
            && message.contains("exited with status 3"),
        "{message}"
    );
}

/// systemd counts a main process killed by SIGTERM during a stop as a clean
/// end and reports Result=success, as for a helper that startup cleanup
/// stopped. That is not a successful prefetch or install.
#[test]
fn snapshot_helper_killed_by_a_stop_fails_although_systemd_reports_success() {
    let scratch = ScratchDir::new("snapshot-helper-stopped");
    let config = ending(
        &scratch,
        "LoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\n\
         ExecMainCode=2\nExecMainStatus=15\n",
    );
    let error = run_prefetch(&config, &scratch, Duration::from_secs(60)).unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::TimedOut);
    let message = error.to_string();
    assert!(
        message.contains("killed by signal SIGTERM (15)") && message.contains("result success"),
        "{message}"
    );
}

/// A helper unit that was stopped and unloaded before the runner could read
/// how its main process ended has not shown that it succeeded.
#[test]
fn snapshot_helper_unloaded_before_its_end_was_read_fails() {
    let scratch = ScratchDir::new("snapshot-helper-unloaded");
    let config = ending(&scratch, UNLOADED);
    let error = run_prefetch(&config, &scratch, Duration::from_secs(60)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("stopped and unloaded from outside"),
        "{error}"
    );
}

#[test]
fn snapshot_helper_success_needs_exit_status_zero_and_result_success() {
    let exit = |result: &str, code: i32, status: i32| {
        classify(
            &Observed::Ended(MainExit {
                result: Some(result.into()),
                code: Some(code),
                status: Some(status),
            }),
            Duration::from_secs(1),
            Duration::from_secs(60),
        )
    };
    assert_eq!(exit("success", libc::CLD_EXITED, 0), Verdict::Succeeded);
    for (result, code, status, reason) in [
        (
            "success",
            libc::CLD_KILLED,
            15,
            "killed by signal SIGTERM (15)",
        ),
        (
            "signal",
            libc::CLD_KILLED,
            9,
            "killed by signal SIGKILL (9)",
        ),
        (
            "core-dump",
            libc::CLD_DUMPED,
            11,
            "dumped core on signal SIGSEGV (11)",
        ),
        ("exit-code", libc::CLD_EXITED, 1, "exited with status 1"),
        // A stop that needed SIGKILL before the limit is not a time-out.
        (
            "timeout",
            libc::CLD_KILLED,
            9,
            "killed by signal SIGKILL (9)",
        ),
    ] {
        match exit(result, code, status) {
            Verdict::Failed(message) => assert!(message.contains(reason), "{message}"),
            verdict => panic!("{result}/{code}/{status}: {verdict:?}"),
        }
    }
    assert!(matches!(
        classify(
            &Observed::Ended(MainExit::default()),
            Duration::ZERO,
            Duration::from_secs(60)
        ),
        Verdict::Failed(message) if message.contains("reported no exit")
    ));
    assert_eq!(
        classify(&Observed::Overran, Duration::ZERO, Duration::from_secs(60)),
        Verdict::TimedOut
    );
}

/// A main process that exited 0 but left processes that ignore SIGTERM:
/// the stop that removes them needs SIGKILL and systemd then reports
/// Result=timeout, but the helper's work is done.
#[test]
fn snapshot_helper_main_success_with_killed_leftovers_is_not_a_timeout() {
    let scratch = ScratchDir::new("snapshot-helper-leftovers");
    let config = fake(
        &scratch,
        Arc::default(),
        SUCCEEDED,
        "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=timeout\n\
         ExecMainCode=1\nExecMainStatus=0\n",
        Arc::default(),
    );
    run_prefetch(&config, &scratch, Duration::from_secs(60)).unwrap();
}

#[test]
fn snapshot_helper_returns_only_after_its_unit_is_inactive() {
    let scratch = ScratchDir::new("snapshot-helper-settle");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    // The main process has exited and the unit is still active; it becomes
    // inactive only once the runner has stopped it.
    let config = fake(
        &scratch,
        calls.clone(),
        SUCCEEDED,
        "LoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\n",
        Arc::default(),
    );
    run_prefetch(&config, &scratch, Duration::from_secs(5)).unwrap();
    let calls = calls.lock().unwrap();
    let verbs: Vec<_> = calls.iter().map(|call| call[0].as_str()).collect();
    assert_eq!(verbs, ["show", "show", "stop", "show"]);
}

#[test]
fn snapshot_helper_startup_stops_only_this_instances_units() {
    let scratch = ScratchDir::new("snapshot-helper-startup");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let main_id = Uuid::now_v7();
    let main = unit_name("main", main_id);
    let other = unit_name("main-2", Uuid::now_v7());
    let listing: &'static str = Box::leak(
        format!(
            "{main} loaded active running main helper\n\
             {other} loaded active running main-2 helper\n\
             motor-snaphelper-main-garbage.service loaded active running not a helper\n"
        )
        .into_boxed_str(),
    );
    let mut config = Config::new(scratch.join("runner"), "main");
    config.free_space_floor_bytes = 0;
    config.cpu_topology =
        Some(crate::cpu::CpuTopology::new((0..4).map(|cpu| vec![cpu]).collect()).unwrap());
    config.disable_warm_builds = true;
    config.manual_tick = true;
    config.sibling_snapshot_mode_override = Some(true);
    let recorded = calls.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        let mut calls = recorded.lock().unwrap();
        calls.push(args.iter().map(|arg| arg.to_string()).collect());
        let stopped = calls.iter().any(|call| call[0] == "stop");
        Ok(output(
            0,
            match args[0] {
                "list-units" => listing,
                "show" if stopped => "LoadState=loaded\nActiveState=inactive\nResult=success\n",
                "show" => "LoadState=loaded\nActiveState=active\nResult=success\n",
                _ => "",
            },
        ))
    }));
    let stale = config.state_dir().join(ENVIRONMENT_DIRECTORY);
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("left.env"), "A=\"b\"\n").unwrap();
    // The files an earlier run of this runner wrote for its helper before
    // starting the unit, as every helper of this runner has while it runs.
    fs::write(stale.join(format!("{main_id}.env")), "A=\"b\"\n").unwrap();
    fs::write(stale.join(format!("{main_id}.log")), "").unwrap();
    let runner = crate::engine::Runner::open(config).unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].last().unwrap(), "motor-snaphelper-main-*");
    let touched: BTreeSet<&str> = calls[1..]
        .iter()
        .map(|call| call.last().unwrap().as_str())
        .collect();
    assert_eq!(touched, BTreeSet::from([main.as_str()]));
    assert!(calls.iter().any(|call| call[0] == "stop"));
    assert_eq!(fs::read_dir(&stale).unwrap().count(), 0);
    drop(runner);
}

#[test]
fn snapshot_helper_startup_does_nothing_outside_snapshot_mode() {
    let scratch = ScratchDir::new("snapshot-helper-startup-off");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let mut config = Config::new(scratch.join("runner"), "main");
    config.free_space_floor_bytes = 0;
    config.cpu_topology =
        Some(crate::cpu::CpuTopology::new((0..4).map(|cpu| vec![cpu]).collect()).unwrap());
    config.disable_warm_builds = true;
    config.manual_tick = true;
    config.sibling_snapshot_mode_override = Some(false);
    let recorded = calls.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        recorded
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string()).collect());
        Ok(output(0, ""))
    }));
    let runner = crate::engine::Runner::open(config).unwrap();
    assert!(!calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.iter().any(|arg| arg.contains("snaphelper"))));
    drop(runner);
}

/// Two runners on one host with the same instance name but different state
/// directories, as concurrent test fixtures are: one opening must not stop
/// the other's in-flight helper, which would fail that runner's setup. The
/// helper's files live in its own runner's state directory only.
#[test]
fn snapshot_helper_startup_leaves_helpers_of_another_state_directory_running() {
    let scratch = ScratchDir::new("snapshot-helper-startup-shared-instance");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let id = Uuid::now_v7();
    let unit = unit_name("main", id);
    let busy = Config::new(scratch.join("busy"), "main");
    let files = busy.state_dir().join(ENVIRONMENT_DIRECTORY);
    fs::create_dir_all(&files).unwrap();
    fs::write(files.join(format!("{id}.env")), "A=\"b\"\n").unwrap();
    fs::write(files.join(format!("{id}.log")), "").unwrap();
    let listing: &'static str =
        Box::leak(format!("{unit} loaded active running in-flight helper\n").into_boxed_str());
    let mut config = Config::new(scratch.join("opening"), "main");
    config.free_space_floor_bytes = 0;
    config.cpu_topology =
        Some(crate::cpu::CpuTopology::new((0..4).map(|cpu| vec![cpu]).collect()).unwrap());
    config.disable_warm_builds = true;
    config.manual_tick = true;
    config.sibling_snapshot_mode_override = Some(true);
    let recorded = calls.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        recorded
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string()).collect());
        Ok(output(
            0,
            match args[0] {
                "list-units" => listing,
                _ => "LoadState=loaded\nActiveState=active\nResult=success\n",
            },
        ))
    }));
    let runner = crate::engine::Runner::open(config).unwrap();
    let calls = calls.lock().unwrap().clone();
    drop(runner);
    assert_eq!(calls.len(), 1, "only the listing: {calls:?}");
    assert_eq!(calls[0][0], "list-units");
    assert_eq!(fs::read_dir(&files).unwrap().count(), 2);
}

/// A runner config that starts real helper units through sudo and systemd.
/// The instance name is unique, so these tests never touch the helpers of a
/// runner that serves jobs on the same host.
#[cfg(target_os = "linux")]
fn isolated_config(scratch: &ScratchDir, instance: &str) -> Config {
    let mut config = Config::new(scratch.join("runner"), instance);
    config.isolate_jobs = true;
    fs::create_dir_all(config.state_dir()).unwrap();
    config
}

#[cfg(target_os = "linux")]
fn unique_instance() -> String {
    format!("isotest{}", &Uuid::now_v7().simple().to_string()[24..])
}

/// Whether `pid` names a process that has not exited.
#[cfg(target_os = "linux")]
fn process_alive(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|stat| {
        !matches!(
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next()),
            Some("Z" | "X")
        )
    })
}

/// A helper whose descendant ignores SIGTERM, once when the helper's main
/// process exits on its own and once when it runs past its limit: after
/// setup returns, neither the unit nor the descendant is left.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_snapshot_helper_leaves_no_term_resistant_descendant() {
    let scratch = ScratchDir::new("snapshot-helper-term");
    let config = isolated_config(&scratch, &unique_instance());
    let script = scratch.join("descendant.sh");
    // SIG_IGN survives exec, so the background sleep ignores SIGTERM too.
    fs::write(
        &script,
        "trap '' TERM\nsleep 600 &\necho $! > \"$1\"\nif [ \"$2\" = stay ]; then exec sleep 600; fi\n",
    )
    .unwrap();
    for (mode, limit) in [
        ("exit", Duration::from_secs(60)),
        ("stay", Duration::from_secs(3)),
    ] {
        let pidfile = scratch.join(format!("{mode}.pid"));
        let mut command = Command::new("/bin/sh");
        command
            .arg(&script)
            .arg(&pidfile)
            .arg(mode)
            .current_dir(&*scratch);
        let result = run(
            &config,
            Helper::CargoPrefetch,
            limit,
            &command,
            &View::default(),
        );
        if mode == "stay" {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
            assert!(error.to_string().contains("time limit of 3 s"), "{error}");
        } else {
            result.unwrap();
        }
        let pid = fs::read_to_string(&pidfile).unwrap();
        assert!(!process_alive(&pid), "{mode}: descendant {pid} survived");
        let listing = crate::backend::systemctl(
            &config,
            &[
                "list-units",
                "--all",
                "--no-legend",
                "--plain",
                &format!("{UNIT_PREFIX}{}-*", config.instance),
            ],
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&listing.stdout).trim().is_empty(),
            "{mode}: {}",
            String::from_utf8_lossy(&listing.stdout)
        );
    }
}

/// Two runner instances on one host, `<name>` and `<name>-2`, as `main` and
/// `main-2` are on the build server. Restarting the first stops its own
/// in-flight helper and leaves the second's running.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_snapshot_helper_restart_leaves_other_instance_helper_running() {
    let scratch = ScratchDir::new("snapshot-helper-instances");
    let name = unique_instance();
    let main = isolated_config(&scratch, &name);
    let other = isolated_config(&scratch, &format!("{name}-2"));
    let units = |config: &Config| -> Vec<String> {
        let listing = crate::backend::systemctl(
            config,
            &[
                "list-units",
                "--no-legend",
                "--plain",
                "--state=active",
                &format!("{UNIT_PREFIX}{}-*", config.instance),
            ],
        )
        .unwrap();
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|unit| belongs_to(&config.instance, unit))
            .map(str::to_string)
            .collect()
    };
    let start = |config: Config| {
        let directory = scratch.to_path_buf();
        std::thread::spawn(move || {
            let mut command = Command::new("/bin/sleep");
            command.arg("120").current_dir(directory);
            run(
                &config,
                Helper::JavascriptInstall,
                Duration::from_secs(120),
                &command,
                &View::default(),
            )
        })
    };
    let main_helper = start(main.clone());
    let other_helper = start(other.clone());
    let deadline = Instant::now() + Duration::from_secs(20);
    while units(&main).len() != 1 || units(&other).len() != 1 {
        assert!(Instant::now() < deadline, "helpers did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    let other_unit = units(&other).remove(0);
    stop_stale(&main).unwrap();
    assert!(units(&main).is_empty());
    assert!(main_helper.join().unwrap().is_err());
    assert_eq!(units(&other), std::slice::from_ref(&other_unit));
    settle(&other, &other_unit).unwrap();
    assert!(other_helper.join().unwrap().is_err());
}

/// The helper's mount namespace shows the snapshot at the sibling's path and
/// at the worktree alias, and nothing else from those parent directories,
/// while the host checkout at the sibling's path holds something else.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_snapshot_helper_sees_the_snapshot_at_the_sibling_path() {
    let scratch = ScratchDir::new("snapshot-helper-view");
    let config = isolated_config(&scratch, &unique_instance());
    let workspace = scratch.join("worktrees/a");
    let sibling = scratch.join("source/b");
    let neighbour = scratch.join("source/undeclared");
    let snapshot = config.sibling_snapshots_dir().join("b/1");
    for (path, marker) in [
        (&workspace, "workspace"),
        (&sibling, "host"),
        (&neighbour, "neighbour"),
        (&snapshot, "snapshot"),
    ] {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("marker"), marker).unwrap();
    }
    let out = config.state_dir().join("out");
    let script = format!(
        "cat {p}/marker > {o}.p; cat {alias}/marker > {o}.alias; ls -A {parent} > {o}.parent; \
         cat {w}/marker > {o}.w",
        p = sibling.display(),
        alias = scratch.join("worktrees/b").display(),
        parent = scratch.join("source").display(),
        w = workspace.display(),
        o = out.display(),
    );
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", &script])
        .current_dir(config.state_dir());
    let view = View::prefetch(&workspace, &[(snapshot.clone(), sibling.clone())]).unwrap();
    run(
        &config,
        Helper::CargoPrefetch,
        Duration::from_secs(60),
        &command,
        &view,
    )
    .unwrap();
    let read = |suffix: &str| fs::read_to_string(out.with_extension(suffix)).unwrap();
    assert_eq!(read("p"), "snapshot");
    assert_eq!(read("alias"), "snapshot");
    assert_eq!(read("w"), "workspace");
    assert_eq!(read("parent").trim(), "b");
    assert_eq!(fs::read_to_string(sibling.join("marker")).unwrap(), "host");
}
