use super::*;
use std::io::Read;

#[path = "writer.rs"]
mod writer;

#[path = "privilege.rs"]
mod privilege;

#[test]
fn stale_lockfile_refusal_says_to_update_it_locally() {
    // Both wordings Cargo has used for a manifest edited past its lockfile.
    for cargo in [
        "error: the lock file /w/Cargo.lock needs to be updated but --locked was \
         passed to prevent this",
        "error: cannot update the lock file /w/Cargo.lock because --locked was \
         passed to prevent this",
    ] {
        let detail = fetch_failure("fetch job dependencies", cargo);
        assert!(detail.starts_with("fetch job dependencies: error: "));
        assert!(detail.contains("update it in your own checkout"));
    }
    let other = fetch_failure("fetch job dependencies", "failed to download serde");
    assert_eq!(other, "fetch job dependencies: failed to download serde");
}

#[test]
fn memory_max_scales_per_vcpu_with_a_sixteen_gib_floor() {
    for vcpus in 1..=4 {
        assert_eq!(crate::backend::memory_max(vcpus, None), 17_179_869_184);
    }
    assert_eq!(crate::backend::memory_max(5, None), 18_790_481_920);
    assert_eq!(crate::backend::memory_max(8, None), 30_064_771_072);
    assert_eq!(crate::backend::memory_max(30, None), 112_742_891_520);
}

#[test]
fn memory_limit_gib_formats_floor_and_scaled_limits() {
    for (vcpus, limit) in [
        (1, "16"),
        (2, "16"),
        (4, "16"),
        (5, "17.5"),
        (8, "28"),
        (30, "105"),
    ] {
        assert_eq!(crate::backend::memory_limit_gib(vcpus, None), limit);
    }
}

#[test]
fn test_memory_floor_override_changes_the_computed_limit() {
    let mut config = crate::engine::Config::new("unused", "main");
    config.memory_floor_gib = Some(1);

    assert_eq!(
        crate::backend::memory_max(1, config.memory_floor_gib),
        7 * (1 << 29)
    );
    assert_eq!(
        crate::backend::memory_limit_gib(1, config.memory_floor_gib),
        "3.5"
    );
}

#[test]
fn systemd_job_command_enforces_recorded_cpu_set_and_memory() {
    let f = Fixture::new();
    let sandbox = f.command_sandbox();
    for cpus in [&[1, 2, 17, 18][..], &[7][..], &[1, 2, 3, 17, 18][..]] {
        let command = sandbox
            .command("nproc", &f.workspace, &[], true, cpus, cpus.len() as u32)
            .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        let allowed = args
            .iter()
            .filter(|arg| arg.starts_with("--property=AllowedCPUs="))
            .collect::<Vec<_>>();
        assert_eq!(allowed.len(), 1);
        assert_eq!(
            crate::cpu::parse_list(allowed[0].strip_prefix("--property=AllowedCPUs=").unwrap())
                .unwrap(),
            cpus
        );
        let memory_max = if cpus.len() == 5 {
            18_790_481_920_u64
        } else {
            17_179_869_184_u64
        };
        assert!(args.contains(&format!("--property=MemoryMax={memory_max}").as_str()));
        assert!(args.contains(
            &format!(
                "--property=TemporaryFileSystem=/dev/shm:mode=1777,nosuid,nodev,size={memory_max}"
            )
            .as_str()
        ));
        assert!(!args.iter().any(|arg| arg.contains("CPUAccounting=")));
        assert!(!args.contains(&"--pipe"));
        assert!(!args.contains(&"--quiet"));
        for property in [
            "StandardInput=file:",
            "StandardOutput=append:",
            "StandardError=append:",
        ] {
            assert!(args
                .iter()
                .any(|arg| arg.starts_with(&format!("--property={property}"))));
        }
        assert!(!args.iter().any(|arg| arg.contains("CPUQuota")));
    }
}

#[test]
fn boat_command_preserves_isolation_and_limits_without_private_pids() {
    let f = Fixture::new();
    for (boat, private_pids) in [(false, true), (true, false), (true, true)] {
        let command = f
            .command_sandbox()
            .with_boat_profile(boat, private_pids, Some(14 * (1 << 30)))
            .without_provider_environment(&["BOAT_FUTURE".to_owned()])
            .command("true", &f.workspace, &[], true, &[1, 2], 2)
            .unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args.contains(&"--property=PrivatePIDs=yes"), private_pids);
        let expected_memory = if boat {
            14 * (1_u64 << 30)
        } else {
            16 * (1_u64 << 30)
        };
        assert!(args.contains(&format!("--property=MemoryMax={expected_memory}").as_str()));
        for property in [
            "PrivateNetwork=yes",
            "PrivateDevices=yes",
            "ProtectProc=invisible",
            "NoNewPrivileges=yes",
            "KillMode=control-group",
            "User=motor-job",
        ] {
            assert!(args.contains(&format!("--property={property}").as_str()));
        }
        assert!(args
            .iter()
            .any(|a| a.starts_with("--property=TemporaryFileSystem=/dev/shm:")));
        assert_eq!(args.contains(&"--property=CPUQuota=200%"), boat);
        assert_eq!(args.contains(&"--property=TasksMax=4096"), boat);
        assert_eq!(args.contains(&"--property=ProcSubset=pid"), boat);
        assert_eq!(
            args.contains(
                &"--property=UnsetEnvironment=ASCII_TOKEN BOAT_CURRENT_ID_FILE BOAT_FUTURE BOAT_ID"
            ),
            boat
        );
    }
}

#[test]
fn tenant_job_units_limit_tasks_and_deny_vsock_only_in_tenant_mode() {
    let f = Fixture::new();
    for enabled in [false, true] {
        let sandbox = f.command_sandbox().in_tenant_vm(enabled);
        let command = sandbox
            .command("true", &f.workspace, &[], true, &[0], 1)
            .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(args.contains(&"--property=TasksMax=4096"), enabled);
        assert_eq!(
            args.contains(&"--property=RestrictAddressFamilies=~AF_VSOCK"),
            enabled
        );
        assert_eq!(
            args.contains(&"--property=SystemCallArchitectures=native"),
            enabled
        );
    }
}

#[test]
#[ignore = "requires a trusted tenant VM with AF_VSOCK and job isolation"]
fn hosted_job_cannot_open_vsock() {
    let host_socket =
        unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    assert!(
        host_socket >= 0,
        "operator fixture must support AF_VSOCK outside the job"
    );
    unsafe {
        libc::close(host_socket);
    }
    let f = Fixture::new();
    let sandbox = f.sandbox().in_tenant_vm(true);
    let (status, stdout, stderr) = output(&sandbox,
        "python3 -c 'import socket,errno;\ntry: socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM)\nexcept OSError as e: assert e.errno in (errno.EPERM,errno.EAFNOSUPPORT); print(\"blocked\")\nelse: raise AssertionError(\"AF_VSOCK admitted\")'", &f.workspace);
    assert!(status.success(), "{stderr}");
    assert_eq!(stdout.trim(), "blocked");
}

#[test]
fn systemd_job_command_bounds_shm_without_cpu_placement() {
    let f = Fixture::new();
    let sandbox = f.command_sandbox();
    let command = sandbox
        .command("nproc", &f.workspace, &[], true, &[], 1)
        .unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    let memory_max = 17_179_869_184_u64;
    assert!(args.contains(&format!("--property=MemoryMax={memory_max}").as_str()));
    assert!(args.contains(
        &format!(
            "--property=TemporaryFileSystem=/dev/shm:mode=1777,nosuid,nodev,size={memory_max}"
        )
        .as_str()
    ));
    assert!(!args
        .iter()
        .any(|arg| arg.starts_with("--property=AllowedCPUs=")));
}

#[test]
fn capped_cpu_placement_keeps_requested_memory_limit() {
    let f = Fixture::new();
    let sandbox = f.command_sandbox();
    let requested_vcpus = 30;
    let cpus: Vec<_> = (1..12).chain(17..28).collect();
    assert_eq!(cpus.len(), 22);
    assert!(requested_vcpus > cpus.len() as u32);
    let command = sandbox
        .command("nproc", &f.workspace, &[], true, &cpus, requested_vcpus)
        .unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    assert!(
        args.contains(&"--property=MemoryMax=112742891520"),
        "{args:?}"
    );
    assert!(args.contains(
        &"--property=AllowedCPUs=1,2,3,4,5,6,7,8,9,10,11,17,18,19,20,21,22,23,24,25,26,27"
    ));
}

struct Fixture {
    cleanup: crate::test_cleanup::Cleanup,
    scratch: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let scratch = cortexkit_test_support::scratch_root()
            .join(format!("motor-isolation-{}", Uuid::now_v7()));
        let workspace = scratch.join("workspace");
        let state = scratch.join("runner-state");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&state).unwrap();
        let mut cleanup = crate::test_cleanup::Cleanup::default();
        cleanup.capture(&scratch);
        Self {
            cleanup,
            scratch,
            workspace,
            state,
        }
    }
    fn sandbox(&self) -> Sandbox {
        let target = self.workspace.join("target");
        fs::create_dir_all(&target).unwrap();
        Sandbox::prepare(
            Uuid::now_v7(),
            &self.state,
            &self.workspace,
            &target,
            &[],
            None,
            &self.state.join("slots"),
            1,
        )
        .unwrap()
    }
    fn command_sandbox(&self) -> Sandbox {
        Sandbox {
            id: Uuid::now_v7(),
            root: self.scratch.join("root"),
            home: PathBuf::from("/motor-home"),
            cargo: PathBuf::from("/motor-home/cargo"),
            rustup: PathBuf::from("/toolchain"),
            path: "/usr/bin:/bin".into(),
            read_only: Vec::new(),
            read_write: Vec::new(),
            wrapper: None,
            runtime: std::sync::Arc::new(RuntimeDir(
                self.scratch.join("unprepared"),
                std::sync::OnceLock::new(),
            )),
            retain_failed: false,
            network: false,
            tenant_vm: false,
            private_pids: true,
            boat_profile: false,
            boat_memory_max: None,
            provider_unset_env: Vec::new(),
            parent_umask: 0o022,
            memory_floor_gib: None,
            user: USER,
            writer: false,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.cleanup.finish() {
            let _ = fs::remove_dir_all(&self.scratch);
        } else if !std::thread::panicking() {
            panic!("isolation fixture did not quiesce before scratch cleanup");
        }
    }
}
fn output(
    sandbox: &Sandbox,
    command: &str,
    cwd: &Path,
) -> (std::process::ExitStatus, String, String) {
    let mut launch = sandbox.launch(command, cwd, &[]).unwrap();
    launch.release().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    launch.stdout.read_to_string(&mut stdout).unwrap();
    launch.stderr.read_to_string(&mut stderr).unwrap();
    let status = launch.wait().unwrap();
    launch.reap();
    (status, stdout, stderr)
}

fn sandbox_result(f: &Fixture) -> io::Result<Sandbox> {
    let target = f.workspace.join("target");
    fs::create_dir_all(&target)?;
    Sandbox::prepare(
        Uuid::now_v7(),
        &f.state,
        &f.workspace,
        &target,
        &[],
        None,
        &f.state.join("slots"),
        1,
    )
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_fixture_backstop_resets_a_failed_unit_after_panic() {
    let f = Fixture::new();
    // Match engine sandboxes: systemd must retain failure until reset-failed,
    // rather than collect it automatically and conceal a missing backstop.
    let sandbox = f.sandbox().retaining_failed_units();
    let id = sandbox.id;
    let reached_failure = std::sync::atomic::AtomicBool::new(false);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let f = f;
        let sandbox = sandbox;
        let mut launch = sandbox.launch("exit 23", &f.workspace, &[]).unwrap();
        launch.release().unwrap();
        assert!(!launch.wait().unwrap().success());
        let result = Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "/usr/bin/systemctl",
                "show",
                "--value",
                "--property=Result",
                &unit(id),
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "exit-code");
        reached_failure.store(true, std::sync::atomic::Ordering::SeqCst);
        // Do not reap: the fixture must cover the test abandoning a failed unit.
        panic!("deliberate inner test failure");
    }));
    let state = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/bin/systemctl",
            "show",
            "--value",
            "--property=LoadState",
            &unit(id),
        ])
        .output()
        .unwrap();
    // A disabled-backstop control must still clean up its deliberately failed
    // unit after capturing the state, even when the assertion below goes red.
    for operation in ["stop", "reset-failed"] {
        let _ = Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/systemctl", operation, &unit(id)])
            .status();
    }
    assert!(result.is_err());
    assert!(reached_failure.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(String::from_utf8_lossy(&state.stdout).trim(), "not-found");
}

#[test]
fn systemd_job_command_preserves_shell_source_and_forces_private_tmp() {
    let f = Fixture::new();
    // Command construction needs no sudo, system manager, or prepared mounts.
    // Keep the runtime path absent so dropping it has nothing to reclaim.
    let sandbox = Sandbox {
        id: Uuid::now_v7(),
        root: f.scratch.join("root"),
        home: PathBuf::from("/motor-home"),
        cargo: PathBuf::from("/motor-home/cargo"),
        rustup: PathBuf::from("/toolchain"),
        path: "/usr/bin:/bin".into(),
        read_only: Vec::new(),
        read_write: Vec::new(),
        wrapper: None,
        runtime: std::sync::Arc::new(RuntimeDir(
            f.scratch.join("unprepared"),
            std::sync::OnceLock::new(),
        )),
        retain_failed: false,
        network: false,
        tenant_vm: false,
        private_pids: true,
        boat_profile: false,
        boat_memory_max: None,
        provider_unset_env: Vec::new(),
        parent_umask: 0o022,
        memory_floor_gib: None,
        user: USER,
        writer: false,
    };
    let source = r#"printf '%s\n' "$TMPDIR" "$HOME" "${X:-y}""#;
    let env =
        ["TMPDIR", "TMP", "TEMP"].map(|key| (key.to_string(), "/tmp/caller-shared".to_string()));
    let command = sandbox
        .command(source, &f.workspace, &env, false, &[], 0)
        .unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    assert!(args.contains(&"--expand-environment=no"));
    // By default systemd unloads the unit on exit. A sandbox that retains failed
    // units (the engine's do) keeps it loaded until finish has read its Result.
    assert!(args.contains(&"--collect"));
    assert!(!args.contains(&"--property=CollectMode=inactive"));
    let retained = sandbox.retaining_failed_units();
    let command = retained
        .command(source, &f.workspace, &env, false, &[], 0)
        .unwrap();
    let retained_args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    assert!(!retained_args.contains(&"--collect"));
    assert!(retained_args.contains(&"--property=CollectMode=inactive"));
    assert_eq!(args.last(), Some(&source));
    for key in ["TMPDIR", "TMP", "TEMP"] {
        let prefix = format!("--setenv={key}=");
        let values: Vec<_> = args
            .iter()
            .copied()
            .filter(|arg| arg.starts_with(&prefix))
            .collect();
        let expected = format!("{prefix}/motor-home/tmp");
        assert_eq!(values, [expected.as_str()], "{key}");
    }
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_cannot_access_runner_or_other_jobs() {
    let f = Fixture::new();
    let other = f.scratch.join("other-job");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("canary"), "other job secret").unwrap();
    fs::write(f.state.join("canary"), "runner secret").unwrap();
    let operator = PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(format!(".motor-canary-{}", Uuid::now_v7()));
    fs::write(&operator, "operator secret").unwrap();
    // The canary sits in the operator's real home, so remove it even when a
    // later step panics; otherwise a failed run leaves it there.
    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _canary = RemoveOnDrop(operator.clone());
    fs::write(f.workspace.join("canary"), "own workspace\n").unwrap();
    // All forbidden canaries are actually readable by the daemon. A missing
    // fixture cannot accidentally make a denied-read assertion pass.
    for path in [
        other.join("canary"),
        f.state.join("canary"),
        operator.clone(),
    ] {
        assert!(!fs::read(&path).unwrap().is_empty());
    }
    let sandbox = f.sandbox();
    let script = format!(
        "set -eu; cat canary; id -u; test -z \"$(ls -A /tmp)\"; for p in '{}' '{}' '{}'; do if cat \"$p\"; then exit 41; fi; done; touch target/writable",
        other.join("canary").display(), f.state.join("canary").display(), operator.display()
    );
    let (status, stdout, stderr) = output(&sandbox, &script, &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    let uid = stdout.lines().nth(1).unwrap().parse::<u32>().unwrap();
    assert_ne!(
        uid,
        unsafe { libc::geteuid() },
        "job ran as the administrative runner account"
    );
    assert_ne!(uid, 0);
    assert!(f.workspace.join("target/writable").is_file());
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_can_read_0640_shared_registry_crate_after_preparation() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let cargo_home = f.scratch.join("cargo");
    let registry = cargo_home.join("registry");
    let source = registry.join("src/fnv-1.0.7");
    let shared_git = cargo_home.join("git");
    let git_source = shared_git.join("checkouts/repo");
    let rustup_home = f.scratch.join("rustup");
    let bun_bin = f.scratch.join("home/.bun/bin");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&git_source).unwrap();
    fs::create_dir_all(cargo_home.join("bin")).unwrap();
    fs::create_dir_all(rustup_home.join("toolchains/stable/bin")).unwrap();
    fs::create_dir_all(&bun_bin).unwrap();
    let crate_file = source.join("lib.rs");
    fs::write(&crate_file, "pub fn canary() {}\n").unwrap();
    fs::set_permissions(&crate_file, fs::Permissions::from_mode(0o640)).unwrap();
    let git_object = git_source.join("pack-123.pack");
    fs::write(&git_object, "git object canary\n").unwrap();
    fs::set_permissions(&git_object, fs::Permissions::from_mode(0o640)).unwrap();
    let rustc = rustup_home.join("toolchains/stable/bin/rustc");
    fs::write(&rustc, "toolchain canary\n").unwrap();
    fs::set_permissions(&rustc, fs::Permissions::from_mode(0o640)).unwrap();
    let cargo = cargo_home.join("bin/cargo");
    fs::write(&cargo, "cargo canary\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o640)).unwrap();
    let bun = bun_bin.join("bun");
    fs::write(&bun, "bun canary\n").unwrap();
    fs::set_permissions(&bun, fs::Permissions::from_mode(0o640)).unwrap();

    let mut config = crate::Config::new(f.state.clone(), "cache-acl-startup");
    config.isolate_jobs = true;
    config.shared_read_roots = Some((cargo_home, rustup_home, Some(bun_bin)));
    let _runner = crate::Runner::open(config).unwrap();

    // Cargo can fetch more crates after daemon startup. A new crate directory
    // must inherit the defaults already installed on every existing directory.
    let later_crate = registry.join("src/later-1.0.0");
    fs::create_dir(&later_crate).unwrap();
    let later_file = later_crate.join("lib.rs");
    fs::write(&later_file, "pub fn later() {}\n").unwrap();
    fs::set_permissions(&later_file, fs::Permissions::from_mode(0o640)).unwrap();
    let later_git = shared_git.join("checkouts/new-repo");
    fs::create_dir(&later_git).unwrap();
    let later_object = later_git.join("pack-456.pack");
    fs::write(&later_object, "later git object canary\n").unwrap();
    fs::set_permissions(&later_object, fs::Permissions::from_mode(0o640)).unwrap();

    for path in [
        &crate_file,
        &later_file,
        &git_object,
        &later_object,
        &rustc,
        &cargo,
        &bun,
    ] {
        let output = Command::new("/usr/bin/sudo")
            .args(["-n", "-u", USER, "--", "/usr/bin/cat"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "motor-job could not read {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
    }
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_can_write_runner_owned_existing_target_files_after_preparation() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let f = Fixture::new();
    let target = f.workspace.join("target");
    fs::create_dir_all(&target).unwrap();
    let old_output = target.join("libsqlite3.a");
    fs::write(&old_output, "old build output\n").unwrap();
    fs::set_permissions(&old_output, fs::Permissions::from_mode(0o640)).unwrap();

    let job_uid = Command::new("/usr/bin/id")
        .args(["-u", USER])
        .output()
        .unwrap();
    assert!(job_uid.status.success());
    let job_uid = String::from_utf8_lossy(&job_uid.stdout)
        .trim()
        .parse::<u32>()
        .unwrap();
    assert_ne!(fs::metadata(&old_output).unwrap().uid(), job_uid);

    let sandbox = f.sandbox();
    assert_eq!(fs::metadata(&old_output).unwrap().uid(), job_uid);
    let (status, stdout, stderr) = output(
        &sandbox,
        "printf 'rebuilt\\n' >> target/libsqlite3.a; grep -q rebuilt target/libsqlite3.a",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_has_owned_sandbox_ancestors_and_private_tmp() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let uid = Command::new("/usr/bin/id")
        .args(["-u", USER])
        .output()
        .unwrap();
    assert!(uid.status.success());
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let script = format!(
        r#"set -eu
printf 'TMPDIR=%s\n' "$TMPDIR"
test "$TMPDIR" = /motor-home/tmp
test "$TMP" = "$TMPDIR"
test "$TEMP" = "$TMPDIR"
stat -c '%u %a' / /tmp "$HOME" "$TMPDIR"
test "$(id -u)" = "{uid}"
test "$(stat -c '%u %a' "$TMPDIR")" = "{uid} 700"
check_chain() {{
  p="$1"
  while :; do
    owner=$(stat -c '%u' "$p")
    mode=$(stat -c '%a' "$p")
    if [ "$owner" != 0 ] && [ "$owner" != "{uid}" ]; then echo bad-owner:$p:$owner >&2; exit 51; fi
    if [ "$p" != /tmp ] && (( (8#$mode & 022) != 0 )); then echo group-writable-ancestor:$p:$mode >&2; exit 52; fi
    [ "$p" = / ] && break
    p=$(dirname "$p")
  done
}}
check_chain "$PWD"
check_chain "$HOME"
check_chain "$TMPDIR"
touch "$PWD/checkout-write-probe"
printf 'private scratch\n' > "$TMPDIR/write-probe"
test "$(cat "$TMPDIR/write-probe")" = 'private scratch'
"#
    );
    let (status, stdout, stderr) = output(&sandbox, &script, &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        stdout,
        format!("TMPDIR=/motor-home/tmp\n0 755\n0 1777\n{uid} 700\n{uid} 700\n")
    );
    assert!(f.workspace.join("checkout-write-probe").is_file());
    restore_runner_access(&f.scratch).unwrap();
    assert_eq!(
        fs::read_to_string(sandbox.runtime.0.join("home/tmp/write-probe")).unwrap(),
        "private scratch\n"
    );
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_cannot_write_shared_registry() {
    let f = Fixture::new();
    fs::create_dir_all(PathBuf::from(std::env::var_os("CARGO_HOME").unwrap()).join("git")).unwrap();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(&sandbox,
        "set -eu; test \"$CARGO_HOME\" = \"$HOME/cargo\"; test -d \"$CARGO_HOME/registry/src\"; for source in registry git; do if touch \"$CARGO_HOME/$source/.motor-write-probe\"; then exit 42; fi; done; test ! -e /run/motor-tenant-daemon/resolv.conf; touch \"$CARGO_HOME/private-lock\"",
        &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("Read-only file system"), "{stderr}");
    // /motor-home is bound from runtime/home. Its job-owned 0700 directories
    // deny the runner traversal until access is restored after the job exits.
    restore_runner_access(&sandbox.runtime.0).unwrap();
    assert!(sandbox.runtime.0.join("home/cargo/private-lock").is_file());
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_has_no_passwordless_sudo() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; test -x /usr/bin/sudo; if /usr/bin/sudo -n true; then exit 43; fi",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(!stderr.is_empty());
    assert!(!stderr.contains("command not found"));
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_cancel_kills_entire_cgroup_including_term_ignoring_child() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let mut launch = sandbox.launch(
        "trap '' TERM; setsid /bin/bash -c 'trap \"\" TERM; exec sleep 600' </dev/null >/dev/null 2>&1 & touch started; wait",
        &f.workspace, &[]).unwrap();
    launch.release().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.workspace.join("started").exists() {
        assert!(Instant::now() < deadline, "child did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let cgroup = Command::new("systemctl")
        .args(["show", "--value", "-p", "ControlGroup", &unit(sandbox.id)])
        .output()
        .unwrap();
    assert!(cgroup.status.success());
    let cgroup = PathBuf::from("/sys/fs/cgroup").join(
        String::from_utf8_lossy(&cgroup.stdout)
            .trim()
            .trim_start_matches('/'),
    );
    let pids = fs::read_to_string(cgroup.join("cgroup.procs")).unwrap();
    assert!(
        pids.lines().count() >= 2,
        "must include both leader and child: {pids}"
    );
    let identities: Vec<_> = pids
        .lines()
        .map(|p| platform::identity(p.parse().unwrap()).unwrap())
        .collect();
    platform::signal(launch.identity, false);
    platform::signal(launch.identity, true);
    assert!(!launch.wait().unwrap().success());
    launch.reap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if identities
            .iter()
            .all(|p| platform::identity(p.pid).is_none_or(|now| now.start != p.start))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cgroup child survived cancellation: {pids}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_gate_eof_executes_nothing() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let mut launch = sandbox
        .launch("touch should-not-run", &f.workspace, &[])
        .unwrap();
    // Dropping stdin models a daemon crash before the durable launch record.
    let replacement = File::open("/dev/null").unwrap();
    drop(std::mem::replace(&mut launch.gate, replacement));
    assert!(!launch.wait().unwrap().success());
    launch.reap();
    assert!(!f.workspace.join("should-not-run").exists());
}

/// A sandbox that can't start is refused before start. Its unit must not stay
/// loaded as failed, or every such refusal would leave a failed unit behind
/// for the server health check to report.
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_sandbox_that_never_starts_leaves_no_loaded_unit() {
    let f = Fixture::new();
    // Only a sandbox that retains failed units (CollectMode=inactive, as the
    // engine's are) can leak one: without that setting, systemd unloads a
    // failed unit by itself, and this test would pass even without the fix.
    let mut sandbox = f.sandbox().retaining_failed_units();
    // A required bind whose source doesn't exist fails the unit at namespace
    // setup, before the job's command can run.
    sandbox.read_only.push(
        bind(
            &f.scratch.join("missing-bind-source"),
            Path::new("/motor-missing"),
        )
        .unwrap(),
    );
    let error = match sandbox.launch("touch should-not-run", &f.workspace, &[]) {
        Ok(_) => panic!("a sandbox with a missing required bind must not start"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("sandbox did not start"),
        "{error}"
    );
    assert!(!f.workspace.join("should-not-run").exists());
    let state = Command::new("/usr/bin/systemctl")
        .args(["show", "--property=LoadState", &unit(sandbox.id)])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&state.stdout).trim(),
        "LoadState=not-found",
        "the unit of a sandbox that never started is still loaded"
    );
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_service_identity_does_not_alias_another_instances_job_id() {
    let a = Fixture::new();
    let b = Fixture::new();
    let job_id = Uuid::now_v7();
    let prepare = |f: &Fixture| {
        let target = f.workspace.join("target");
        fs::create_dir_all(&target).unwrap();
        Sandbox::prepare(
            job_id,
            &f.state,
            &f.workspace,
            &target,
            &[],
            None,
            &f.state.join("slots"),
            1,
        )
        .unwrap()
    };
    let first = prepare(&a);
    let second = prepare(&b);
    assert_ne!(
        first.id, second.id,
        "caller IDs must not select system services"
    );
    let mut first_launch = first.launch("touch first", &a.workspace, &[]).unwrap();
    let mut second_launch = second.launch("touch second", &b.workspace, &[]).unwrap();
    platform::signal(first_launch.identity, true);
    assert!(!first_launch.wait().unwrap().success());
    first_launch.reap();
    // Killing the first account-boundary cgroup must not affect the second
    // gated service, even though both requests used the same public job ID.
    second_launch.release().unwrap();
    assert!(second_launch.wait().unwrap().success());
    second_launch.reap();
    assert!(b.workspace.join("second").exists());
    assert!(!a.workspace.join("first").exists());
}

/// Measurements are opt-in so the normal gate does not depend on an external
/// checkout or perturb its target cache. Run on the build server with
/// MOTOR_BENCH_WORKSPACE and MOTOR_BENCH_TARGET set to an isolated source copy.
#[test]
#[ignore = "requires the trusted build-server benchmark gate plus MOTOR_BENCH_WORKSPACE and MOTOR_BENCH_TARGET"]
fn measure_isolated_launch_and_warm_cargo_check() {
    let f = Fixture::new();
    let workspace = PathBuf::from(std::env::var_os("MOTOR_BENCH_WORKSPACE").unwrap());
    let target = PathBuf::from(std::env::var_os("MOTOR_BENCH_TARGET").unwrap());
    fs::create_dir_all(&target).unwrap();
    let sandbox = Sandbox::prepare(
        Uuid::now_v7(),
        &f.state,
        &workspace,
        &target,
        &[],
        None,
        &f.state.join("slots"),
        1,
    )
    .unwrap();
    let env: Vec<_> = ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"]
        .iter()
        .map(|k| ((*k).to_string(), std::env::var(k).unwrap()))
        .chain([
            (
                "CARGO_TARGET_DIR".into(),
                target.to_string_lossy().into_owned(),
            ),
            ("CARGO_BUILD_JOBS".into(), "4".into()),
        ])
        .collect();
    for isolated in [false, true] {
        let mut samples = Vec::new();
        for _ in 0..10 {
            // Each invocation needs a fresh service name (systemd --collect
            // may still be asynchronously collecting the previous unit).
            let sandbox = Sandbox {
                id: Uuid::now_v7(),
                root: sandbox.root.clone(),
                home: sandbox.home.clone(),
                cargo: sandbox.cargo.clone(),
                rustup: sandbox.rustup.clone(),
                path: sandbox.path.clone(),
                read_only: sandbox.read_only.clone(),
                read_write: sandbox.read_write.clone(),
                wrapper: None,
                runtime: sandbox.runtime.clone(),
                retain_failed: false,
                network: false,
                tenant_vm: false,
                private_pids: true,
                boat_profile: false,
                boat_memory_max: None,
                provider_unset_env: Vec::new(),
                parent_umask: 0o022,
                memory_floor_gib: None,
                user: USER,
                writer: false,
            };
            let started = Instant::now();
            let mut launch = if isolated {
                sandbox.launch("echo first-command", &workspace, &env)
            } else {
                platform::launch("echo first-command", &workspace, &env)
            }
            .unwrap();
            launch.release().unwrap();
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(&launch.stdout), &mut line)
                .unwrap();
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(line, "first-command\n");
            assert!(launch.wait().unwrap().success());
            launch.reap();
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "launch isolated={isolated}: 10 runs median_ms={:.3} samples_ms={samples:?}",
            (samples[4] + samples[5]) / 2.0
        );
    }
    for isolated in [false, true] {
        // Warm once within each backend, then time the same check. Source paths
        // change when CARGO_HOME becomes private, so the first isolated check
        // must not be mistaken for a warm measurement.
        for measured in [false, true] {
            let started = Instant::now();
            let command = "cargo check -p motor-protocol";
            let sandbox = Sandbox {
                id: Uuid::now_v7(),
                root: sandbox.root.clone(),
                home: sandbox.home.clone(),
                cargo: sandbox.cargo.clone(),
                rustup: sandbox.rustup.clone(),
                path: sandbox.path.clone(),
                read_only: sandbox.read_only.clone(),
                read_write: sandbox.read_write.clone(),
                wrapper: None,
                runtime: sandbox.runtime.clone(),
                retain_failed: false,
                network: false,
                tenant_vm: false,
                private_pids: true,
                boat_profile: false,
                boat_memory_max: None,
                provider_unset_env: Vec::new(),
                parent_umask: 0o022,
                memory_floor_gib: None,
                user: USER,
                writer: false,
            };
            let mut launch = if isolated {
                sandbox.launch(command, &workspace, &env)
            } else {
                platform::launch(command, &workspace, &env)
            }
            .unwrap();
            launch.release().unwrap();
            let mut stderr = String::new();
            launch.stderr.read_to_string(&mut stderr).unwrap();
            let status = launch.wait().unwrap();
            launch.reap();
            assert!(status.success(), "{stderr}");
            if measured {
                println!("warm cargo check -p motor-protocol isolated={isolated}: elapsed_ms={:.3}\n{stderr}", started.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_stdin_is_eof_after_durable_gate() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    // Bash returns 1 on EOF and 142 on timeout. A held-open gate pipe must not
    // masquerade as the noninteractive stdin supplied by the old backend.
    let (status, stdout, stderr) = output(
        &sandbox,
        "read -r -t 0.2 line; code=$?; test \"$code\" = 1",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_private_tmp_supports_build_filesystem_reflinks() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(&sandbox,
        "set -eu; test -z \"$(ls -A /tmp)\"; printf scratch > /tmp/source; cp --reflink=always /tmp/source /tmp/copy; test \"$(cat /tmp/copy)\" = scratch; mkdir -m 700 /tmp/private",
        &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    let runtime = sandbox.runtime.0.clone();
    drop(sandbox);
    assert!(
        !runtime.exists(),
        "private scratch must be reclaimed after the job"
    );
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_uses_022_umask_and_creates_755_directories_under_loose_parent() {
    let f = Fixture::new();
    let mut sandbox = f.sandbox();
    // Set a looser parent-service umask, then verify the sandbox resets it to
    // 0022 before the job creates files.
    sandbox.parent_umask = 0o002;
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; umask; mkdir d; stat -c %a d",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout, "0022\n755\n");
}

#[path = "javascript_tests.rs"]
mod javascript_tests;

#[path = "target_retention_tests.rs"]
mod target_retention_tests;

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_can_read_nonprocess_proc_files() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let files = [
        "/proc/sys/kernel/random/boot_id",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/loadavg",
    ];
    let script = format!("set -eu; for p in {}; do IFS= read -r line < \"$p\"; test -n \"$line\"; printf '%s\\n' \"$p\"; done", files.join(" "));
    let (status, stdout, stderr) = output(&sandbox, &script, &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout.lines().collect::<Vec<_>>(), files);
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_resolves_localhost_to_loopback() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; getent hosts localhost; getent hosts ip6-localhost; test ! -w /etc/hosts",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines[0], ["127.0.0.1", "localhost"], "{stdout}");
    assert_eq!(lines[1][0], "::1", "{stdout}");
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; cat /etc/hosts; if getent hosts daily-cloudcode-pa.googleapis.com; then exit 1; fi; if getent hosts cloudcode-pa.googleapis.com; then exit 1; fi",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout, JOB_HOSTS);
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_binds_low_ports_on_its_own_loopback_only() {
    let host = fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start").unwrap();
    assert_ne!(
        host.trim(),
        "0",
        "the host must keep its own low-port restriction"
    );
    let f = Fixture::new();
    let sandbox = f.sandbox();
    // The job binds 443 on both loopback families as the unprivileged job
    // account, without any capability, and its namespace still has only lo.
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; cat /proc/sys/net/ipv4/ip_unprivileged_port_start; grep '^CapBnd' /proc/self/status; ls /sys/class/net; id -un; python3 -c 'import socket
for fam, host in ((socket.AF_INET, \"127.0.0.1\"), (socket.AF_INET6, \"::1\")):
    s = socket.socket(fam); s.bind((host, 443)); s.listen(); print(\"bound\", host)'",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            "0",
            "CapBnd:\t0000000000000000",
            "lo",
            "motor-job",
            "bound 127.0.0.1",
            "bound ::1"
        ],
        "{stdout}"
    );
    // Setting it happened inside the job's namespace, not on the host.
    assert_eq!(
        fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start").unwrap(),
        host
    );
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_resolves_declared_loopback_hosts_including_child() {
    let f = Fixture::new();
    fs::create_dir_all(f.workspace.join(".motor")).unwrap();
    fs::write(
        f.workspace.join(".motor/hosts"),
        "127.0.0.1 daily-cloudcode-pa.googleapis.com cloudcode-pa.googleapis.com\n",
    )
    .unwrap();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; getent hosts daily-cloudcode-pa.googleapis.com; getent hosts cloudcode-pa.googleapis.com; sh -c 'set -eu; getent hosts daily-cloudcode-pa.googleapis.com; getent hosts cloudcode-pa.googleapis.com'; test ! -w /etc/hosts; if printf '127.0.0.1 injected.example\\n' >> /etc/hosts; then exit 1; fi",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 4, "{stdout}");
    for (line, name) in lines.iter().zip([
        "daily-cloudcode-pa.googleapis.com",
        "cloudcode-pa.googleapis.com",
        "daily-cloudcode-pa.googleapis.com",
        "cloudcode-pa.googleapis.com",
    ]) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        assert_eq!(fields[0], "127.0.0.1", "{stdout}");
        assert!(fields[1..].contains(&name), "{stdout}");
    }
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_refuses_public_hosts_before_start() {
    let f = Fixture::new();
    fs::create_dir_all(f.workspace.join(".motor")).unwrap();
    fs::write(
        f.workspace.join(".motor/hosts"),
        "# local test server\n8.8.8.8 daily-cloudcode-pa.googleapis.com\n",
    )
    .unwrap();
    let error = match sandbox_result(&f) {
        Ok(_) => panic!("public address must refuse sandbox setup"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains(".motor/hosts: line 2:"), "{error}");
    assert!(error.contains("address must be"), "{error}");
    assert!(!f.state.join("sandboxes").exists());
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_extracts_archives_with_gnu_tar() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; d=$(mktemp -d); mkdir -p \"$d/src/dist\" \"$d/out\"; \
         printf hi > \"$d/src/dist/a.js\"; tar -czf \"$d/p.tgz\" -C \"$d/src\" dist; \
         tar -xzf \"$d/p.tgz\" -C \"$d/out\"; cat \"$d/out/dist/a.js\"",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout, "hi");
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_loads_the_default_openssl_config() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "set -eu; test -r /usr/lib/ssl/openssl.cnf; openssl rand -hex 4",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_runs_with_no_new_privileges() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "grep '^NoNewPrivs:' /proc/self/status",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        stdout.split_whitespace().collect::<Vec<_>>(),
        ["NoNewPrivs:", "1"]
    );
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_cannot_see_another_accounts_process() {
    use std::os::unix::fs::MetadataExt;
    let other = crate::test_process::ChildGuard::spawn(Command::new("sleep").arg("300")).unwrap();
    let host_proc = PathBuf::from(format!("/proc/{}", other.id()));
    let owner = fs::metadata(&host_proc).unwrap().uid();
    assert_eq!(owner, unsafe { libc::geteuid() });
    assert!(!fs::read(host_proc.join("status")).unwrap().is_empty());
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        &format!("set -eu; id -u; test ! -e '{}'", host_proc.display()),
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_ne!(
        stdout.trim().parse::<u32>().unwrap(),
        owner,
        "fixture process must belong to another account"
    );
}

// PrivateDevices=yes gives a job a private /dev, and systemd fills its /dev/pts
// by bind-mounting whatever the host has at /dev/pts. If the host has lost its
// devpts mount, for example because an unmount propagated from a shared bind of
// /dev, jobs get a bare devtmpfs directory there. Then /dev/ptmx fails with
// ENODEV and every pseudo-terminal test inside a job fails.
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_can_open_a_pty() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    // A later mount at the same mount point hides earlier ones, so the job's
    // visible /dev/pts is the one no other /dev/pts entry is stacked on.
    let script = r#"
import os
mounts = []
with open("/proc/self/mountinfo") as table:
    for line in table:
        fields, _, tail = line.partition(" - ")
        fields = fields.split()
        if fields[4] == "/dev/pts":
            mounts.append((fields[0], fields[1], tail.split()[0]))
assert mounts, "no /dev/pts mount"
stacked = {parent for (_, parent, _) in mounts}
visible = [fstype for (mount, _, fstype) in mounts if mount not in stacked]
print("fstype", " ".join(visible))
leader, follower = os.openpty()
print("terminal", os.ttyname(follower))
"#;
    let (status, stdout, stderr) = output(
        &sandbox,
        &format!("exec python3 -c '{script}'"),
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines[0], "fstype devpts", "{stdout}");
    assert!(lines[1].starts_with("terminal /dev/pts/"), "{stdout}");
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_starts_with_no_ignored_or_blocked_signals() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(&sandbox, "exec cat /proc/self/status", &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    for field in ["SigIgn:", "SigBlk:"] {
        let value = stdout
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap()
            .trim();
        assert_eq!(
            value, "0000000000000000",
            "{field} in job status:\n{stdout}"
        );
    }
}

#[test]
fn cache_readiness_probe_checks_live_tcp_and_abstract_socket_endpoints() {
    use std::{
        net::TcpListener,
        os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        },
    };
    let scratch = cortexkit_test_support::ScratchDir::new("sccache-readiness");
    let probe = prepare_cache_readiness_probe(scratch.path()).unwrap();
    let tcp = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = tcp.local_addr().unwrap().port().to_string();
    let tcp_status = || {
        Command::new(&probe)
            .env_remove("SCCACHE_SERVER_UDS")
            .env("SCCACHE_SERVER_PORT", &port)
            .status()
            .unwrap()
            .success()
    };
    assert!(tcp_status());
    drop(tcp);
    assert!(!tcp_status());
    let name = format!("motor-cache-probe-{}", Uuid::now_v7());
    let socket = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let listener = UnixListener::bind_addr(&socket).unwrap();
    let uds_status = || {
        Command::new(&probe)
            .env("SCCACHE_SERVER_UDS", format!("\\x00{name}"))
            .status()
            .unwrap()
            .success()
    };
    assert!(uds_status());
    drop(listener);
    assert!(!uds_status());
}

#[test]
fn durable_cache_drivers_preserve_raw_shell_comments_heredocs_and_exit_codes() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let output = f.scratch.join("driver-output");
    let tools = f.scratch.join("driver-tools");
    fs::create_dir_all(&output).unwrap();
    fs::create_dir_all(&tools).unwrap();
    for (name, script) in [
        ("python3", "#!/bin/bash\nexit 0\n"),
        (
            "sccache",
            "#!/bin/bash\nprintf '{\"stats\":{\"cache_misses\":{\"counts\":{\"Rust\":0}}}}\\n'\n",
        ),
    ] {
        fs::write(tools.join(name), script).unwrap();
        fs::set_permissions(tools.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut sandbox = Sandbox {
        id: Uuid::now_v7(),
        root: f.scratch.join("root"),
        home: PathBuf::from("/motor-home"),
        cargo: PathBuf::from("/motor-home/cargo"),
        rustup: PathBuf::from("/toolchain"),
        path: "/usr/bin:/bin".into(),
        read_only: vec![],
        read_write: vec![],
        wrapper: None,
        runtime: std::sync::Arc::new(RuntimeDir(
            f.scratch.join("unprepared"),
            std::sync::OnceLock::new(),
        )),
        retain_failed: false,
        network: false,
        tenant_vm: false,
        private_pids: true,
        boat_profile: false,
        boat_memory_max: None,
        provider_unset_env: Vec::new(),
        parent_umask: 0o022,
        memory_floor_gib: None,
        user: USER,
        writer: false,
    };
    for writer in [false, true] {
        sandbox.writer = writer;
        for (raw, code, text) in [
            ("printf comment-ok; # trailing comment", 0, "comment-ok"),
            ("cat <<'EOF'\nheredoc-ok\nEOF", 0, "heredoc-ok\n"),
            ("exit 7 # trailing comment", 7, ""),
        ] {
            let command = sandbox
                .command(
                    raw,
                    &f.workspace,
                    &[("SCCACHE_DIR".into(), "/motor-cache".into())],
                    true,
                    &[],
                    0,
                )
                .unwrap();
            let args: Vec<_> = command.get_args().collect();
            assert_eq!(args.last().unwrap().to_str().unwrap(), raw);
            let shell = args[args.len() - 3]
                .to_str()
                .unwrap()
                .replace("/motor-output", output.to_str().unwrap());
            let mut child = Command::new("/bin/bash")
                .args(["-c", &shell, USER, raw])
                .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"x").unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(code));
            assert_eq!(fs::read_to_string(output.join("stdout")).unwrap(), text);
            assert_eq!(
                fs::read_to_string(output.join("exit")).unwrap().trim(),
                code.to_string()
            );
            assert!(!fs::read(output.join("cache-stats")).unwrap().is_empty());
        }
    }
}

#[test]
fn branch_first_fetches_tip_only_git_sources_before_writer_offline_preflight() {
    let f = Fixture::new();
    let dependency = f.scratch.join("tip-only-dependency");
    fs::create_dir_all(dependency.join("src")).unwrap();
    workspace::git(&dependency, &["init", "-q"]).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='tip_only'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn value() -> u32 { 7 }\n",
    )
    .unwrap();
    workspace::git(&dependency, &["add", "."]).unwrap();
    workspace::git(
        &dependency,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "tip dependency",
        ],
    )
    .unwrap();
    let oid = workspace::git(&dependency, &["rev-parse", "HEAD"]).unwrap();
    let mirror = f.scratch.join("dependency.git");
    workspace::git(
        &dependency,
        &[
            "clone",
            "--bare",
            dependency.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    )
    .unwrap();
    let url = format!(
        "https://branch-first-{}.invalid/private.git",
        Uuid::now_v7()
    );
    let tip = f.workspace.join("tip");
    let branch = f.workspace.join("branch");
    for root in [&tip, &branch] {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    }
    let package = "[package]\nname='branch-first'\nversion='0.1.0'\nedition='2021'\n";
    fs::write(branch.join("Cargo.toml"), package).unwrap();
    fs::write(
        branch.join("Cargo.lock"),
        "version=4\n[[package]]\nname='branch-first'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(
        tip.join("Cargo.toml"),
        format!("{package}[dependencies]\ntip_only={{git='{url}',rev='{oid}'}}\n"),
    )
    .unwrap();
    let lock = format!("version=4\n[[package]]\nname='branch-first'\nversion='0.1.0'\ndependencies=['tip_only']\n[[package]]\nname='tip_only'\nversion='0.1.0'\nsource='git+{url}?rev={oid}#{oid}'\n");
    fs::write(tip.join("Cargo.lock"), &lock).unwrap();
    let cargo = f.scratch.join("empty-cargo-home");
    fs::create_dir_all(&cargo).unwrap();
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".rustup")
            .into_os_string()
    }));
    let metadata = || {
        Command::new("cargo")
            .args(["metadata", "--locked", "--offline", "--format-version", "1"])
            .current_dir(&tip)
            .env("CARGO_HOME", &cargo)
            .env("RUSTUP_HOME", &rustup)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .output()
            .unwrap()
    };
    prefetch_cargo_dependencies(
        false,
        false,
        &f.state,
        &branch,
        &BTreeMap::new(),
        &cargo,
        &rustup,
    )
    .unwrap();
    let missing = metadata();
    assert!(
        !missing.status.success(),
        "branch preparation unexpectedly fetched tip-only sources"
    );
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("offline"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(
        !cargo.join("git/checkouts").exists(),
        "tip dependency must initially be absent"
    );
    let routes = BTreeMap::from([(url, mirror)]);
    prefetch_cargo_dependencies(true, false, &f.state, &tip, &routes, &cargo, &rustup).unwrap();
    let ready = metadata();
    assert!(
        ready.status.success(),
        "tip-only sources were not prepared before offline preflight: {}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let graph: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
    assert!(graph["packages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "tip_only"));
    assert_eq!(fs::read_to_string(tip.join("Cargo.lock")).unwrap(), lock);
    assert!(
        !tip.join("target").exists(),
        "trusted fetch must not compile anything"
    );
}

#[test]
fn runtime_cleanup_reservation_survives_until_last_handle_is_dropped() {
    let scratch = cortexkit_test_support::ScratchDir::new("runtime-cleanup-count");
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = std::sync::Arc::new(RuntimeDir(
        scratch.join("absent-runtime"),
        std::sync::OnceLock::new(),
    ));
    runtime.track(count.clone());
    let last = runtime.clone();
    drop(runtime);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(last);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn net_operator_lock() -> File {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    // nextest runs each test in its own process. Serialize host-state fixtures
    // across those processes so startup cleanup cannot remove another test's orphan.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/run/lock/motor-net-operator-tests.lock")
        .unwrap();
    lock.lock_exclusive().unwrap();
    lock
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_public_https_dns_read_only_resolver() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("public").await;
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_input_rejects_host_and_gateway() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("input").await;
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_forbidden_destinations_fail_fast_tcp_and_udp() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("forbidden").await;
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_private_namespace_low_ports_and_cargo_defaults() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("environment").await;
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_unlocked_public_git_fetch() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("fetch").await;
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_offline_job_control() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("offline").await;
}

#[tokio::test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
async fn isolated_net_restart_survives_and_cleans_up() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("restart").await;
}

#[tokio::test]
#[ignore = "operator only: use script/runner/measure-job-network.py for persistent results"]
async fn isolated_net_measurement() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("measurement").await;
}

#[test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
fn isolated_net_attach_failure_discards_unstarted_unit() {
    let _guard = net_operator_lock();
    let f = Fixture::new();
    let sandbox = f.sandbox().with_network().unwrap();
    let name = crate::network::linux::interface(sandbox.id);
    // Every isolated runner another test opens tidies orphaned mnet-* links at
    // startup, keeping only links whose owning job unit is alive. Give the
    // colliding link a live owner, as a real job's link has, and create it
    // under the host network lock the tidy-up takes, as attach does: between
    // `ip link add` and setting its alias the link has no owner, and a tidy-up
    // running then deleted it.
    let owner = Uuid::now_v7();
    let owner_unit = crate::backend::host_unit_name(owner);
    checked(Command::new("/usr/bin/sudo").args([
        "-n",
        "/usr/bin/systemd-run",
        "--quiet",
        "--collect",
        &format!("--unit={owner_unit}"),
        "/usr/bin/sleep",
        "300",
    ]))
    .unwrap();
    let host_lock = crate::network::linux::lock().unwrap();
    checked(Command::new("/usr/bin/sudo").args([
        "-n",
        "/usr/sbin/ip",
        "link",
        "add",
        &name,
        "type",
        "veth",
        "peer",
        "name",
        "mj-collision",
    ]))
    .unwrap();
    checked(Command::new("/usr/bin/sudo").args([
        "-n",
        "/usr/sbin/ip",
        "link",
        "set",
        "dev",
        &name,
        "alias",
        &format!("motor-net:{owner}:16383"),
    ]))
    .unwrap();
    drop(host_lock);
    let result = sandbox.launch("touch never-started", &f.workspace, &[]);
    checked(Command::new("/usr/bin/sudo").args([
        "-n",
        "/usr/sbin/ip",
        "link",
        "delete",
        "dev",
        &name,
    ]))
    .unwrap();
    checked(Command::new("/usr/bin/sudo").args(["-n", "/usr/bin/systemctl", "stop", &owner_unit]))
        .unwrap();
    let error = result
        .err()
        .expect("a colliding interface must refuse the launch");
    assert!(error.to_string().contains("network"), "{error}");
    assert!(!f.workspace.join("never-started").exists());
    assert!(!unit_alive(sandbox.id).unwrap());
}

#[test]
#[ignore = "operator only: requires provisioned motor_net and trusted host isolation"]
fn isolated_net_startup_reconciles_leftovers_but_preserves_live_unit() {
    let _guard = net_operator_lock();
    let f = Fixture::new();
    let root = f.state.join("slots");
    let sandbox = f.sandbox().with_network().unwrap();
    let mut launch = sandbox.launch("true", &f.workspace, &[]).unwrap();
    let live = crate::network::linux::interface(sandbox.id);
    let dead = Uuid::now_v7();
    let orphan = crate::network::linux::interface(dead);
    checked(Command::new("/usr/bin/sudo").args([
        "-n",
        "/usr/sbin/ip",
        "link",
        "add",
        &orphan,
        "type",
        "veth",
        "peer",
        "name",
        "mj-orphan-test",
    ]))
    .unwrap();
    checked(Command::new("/usr/bin/sudo").args([
        "-n",
        "/usr/sbin/ip",
        "link",
        "set",
        "dev",
        &orphan,
        "alias",
        &format!("motor-net:{dead}:16383"),
    ]))
    .unwrap();
    let mut config = crate::Config::new(&f.state, "net-reconcile");
    config.isolate_jobs = true;
    config.job_network = Some(crate::network::JobNetwork::Veth);
    config.build_slot_root = root;
    crate::network::initialize(&mut config);
    assert!(
        config.job_network.is_some(),
        "{}",
        config.job_network_unavailable
    );
    let exists = |name: &str| {
        Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/sbin/ip", "link", "show", "dev", name])
            .status()
            .unwrap()
            .success()
    };
    assert!(!exists(&orphan));
    assert!(exists(&live), "startup removed a live job's veth");
    launch.release().unwrap();
    assert!(launch.wait().unwrap().success());
    launch.reap();
}

#[tokio::test]
#[ignore = "operator only: routed outside namespace and motor_net packet counters"]
async fn isolated_net_forward_drops_inbound_and_unrelated() {
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("forward").await;
}

#[test]
fn tenant_job_unit_does_not_inherit_daemon_dns_or_network_authority() {
    let f = Fixture::new();
    let sandbox = f.command_sandbox().in_tenant_vm(true);
    let command = sandbox
        .command("true", &f.workspace, &[], false, &[], 2)
        .unwrap();
    let words: Vec<_> = command
        .get_args()
        .map(|word| word.to_string_lossy())
        .collect();
    assert!(words
        .iter()
        .any(|word| word == "--property=PrivateNetwork=yes"));
    assert!(words
        .iter()
        .any(|word| word == "--property=CapabilityBoundingSet="));
    assert!(words.iter().any(|word| word == "--property=User=motor-job"));
    assert!(words
        .iter()
        .any(|word| word == "--setenv=CARGO_HOME=/motor-home/cargo"));
    assert!(
        !words.iter().any(|word| word.contains("motor-tenant-daemon")
            || word.contains("JoinsNamespaceOf")
            || word.contains("CARGO_HOME=/tenant/cargo-home"))
    );
}
