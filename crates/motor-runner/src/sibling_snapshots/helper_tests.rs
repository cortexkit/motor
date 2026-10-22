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

/// A config whose systemctl calls are answered by `show` for `show` and
/// recorded in `calls`, and whose helper launches return `launched`.
fn config(
    scratch: &ScratchDir,
    instance: &str,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    show: &'static str,
    launched: Arc<Mutex<Vec<Vec<String>>>>,
    exit: i32,
) -> Config {
    let mut config = Config::new(scratch.join("runner"), instance);
    fs::create_dir_all(config.state_dir()).unwrap();
    config.systemd_command = Some(Arc::new(move |args| {
        calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string()).collect());
        Ok(output(0, if args[0] == "show" { show } else { "" }))
    }));
    config.snapshot_helper_launcher = Some(Arc::new(move |args| {
        launched.lock().unwrap().push(args.to_vec());
        Ok(output(exit, ""))
    }));
    config
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
    let calls = Arc::default();
    let launched: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let mut config = config(
        &scratch,
        "main",
        calls,
        "LoadState=not-found\nActiveState=inactive\nResult=success\n",
        launched.clone(),
        0,
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
        "--wait".to_string(),
        "--service-type=exec".into(),
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
}

#[test]
fn snapshot_javascript_helper_runs_in_the_snapshot_at_the_sibling_path() {
    let scratch = ScratchDir::new("snapshot-helper-javascript");
    let launched: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let config = config(
        &scratch,
        "main",
        Arc::default(),
        "LoadState=loaded\nActiveState=inactive\nResult=success\n",
        launched.clone(),
        0,
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
    let mut config = config(
        &scratch,
        "main",
        Arc::default(),
        "LoadState=not-found\nActiveState=inactive\n",
        Arc::default(),
        0,
    );
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
    let mut config = config(
        &scratch,
        "main",
        calls.clone(),
        "LoadState=loaded\nActiveState=failed\nResult=timeout\n",
        Arc::default(),
        1,
    );
    // The unit runs for its whole limit before systemd stops it.
    config.snapshot_helper_launcher = Some(Arc::new(|_| {
        std::thread::sleep(Duration::from_millis(20));
        Ok(output(1, ""))
    }));
    let view = View::prefetch(&scratch.join("w/a"), &[]).unwrap();
    let error = run(
        &config,
        Helper::CargoPrefetch,
        Duration::from_millis(10),
        &shell(&config.state_dir()),
        &view,
    )
    .unwrap_err();
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
    let config = config(
        &scratch,
        "main",
        Arc::default(),
        "LoadState=loaded\nActiveState=failed\nResult=exit-code\n",
        Arc::default(),
        3,
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
    assert!(
        error
            .to_string()
            .contains("JavaScript install helper unit motor-snaphelper-main-"),
        "{error}"
    );
}

/// A main process that succeeded but left processes that ignored SIGTERM:
/// systemd kills them and reports Result=timeout, but the helper did not run
/// out of time and its work is done.
#[test]
fn snapshot_helper_main_success_with_killed_leftovers_is_not_a_timeout() {
    let scratch = ScratchDir::new("snapshot-helper-leftovers");
    let config = config(
        &scratch,
        "main",
        Arc::default(),
        "LoadState=loaded\nActiveState=failed\nResult=timeout\nExecMainCode=1\nExecMainStatus=0\n",
        Arc::default(),
        1,
    );
    let view = View::prefetch(&scratch.join("w/a"), &[]).unwrap();
    run(
        &config,
        Helper::CargoPrefetch,
        Duration::from_secs(60),
        &shell(&config.state_dir()),
        &view,
    )
    .unwrap();
}

#[test]
fn snapshot_helper_returns_only_after_its_unit_is_inactive() {
    let scratch = ScratchDir::new("snapshot-helper-settle");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let mut config = config(&scratch, "main", calls.clone(), "", Arc::default(), 0);
    // The unit is still deactivating when systemd-run returns, and becomes
    // inactive only after the runner has stopped it.
    let recorded = calls.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        let mut calls = recorded.lock().unwrap();
        calls.push(args.iter().map(|arg| arg.to_string()).collect());
        let stopped = calls.iter().any(|call| call[0] == "kill");
        Ok(output(
            0,
            match (args[0], stopped) {
                ("show", false) => "LoadState=loaded\nActiveState=deactivating\nResult=success\n",
                ("show", true) => "LoadState=loaded\nActiveState=inactive\nResult=success\n",
                _ => "",
            },
        ))
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
    let calls = calls.lock().unwrap();
    let verbs: Vec<_> = calls.iter().map(|call| call[0].as_str()).collect();
    assert_eq!(verbs, ["show", "stop", "kill", "show"]);
    assert_eq!(calls[2][1..3], ["--kill-whom=all", "--signal=KILL"]);
}

#[test]
fn snapshot_helper_startup_stops_only_this_instances_units() {
    let scratch = ScratchDir::new("snapshot-helper-startup");
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let main = unit_name("main", Uuid::now_v7());
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
        let stopped = calls.iter().any(|call| call[0] == "kill");
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
    let runner = crate::engine::Runner::open(config).unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].last().unwrap(), "motor-snaphelper-main-*");
    let touched: BTreeSet<&str> = calls[1..]
        .iter()
        .map(|call| call.last().unwrap().as_str())
        .collect();
    assert_eq!(touched, BTreeSet::from([main.as_str()]));
    assert!(calls.iter().any(|call| call[0] == "kill"));
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
