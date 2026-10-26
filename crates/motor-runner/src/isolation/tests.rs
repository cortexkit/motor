use super::*;
use std::io::Read;

#[path = "writer.rs"]
mod writer;

#[path = "privilege.rs"]
mod privilege;

#[path = "command_golden_tests.rs"]
mod command_golden;
#[path = "compatibility_tests.rs"]
mod compatibility;
#[path = "mapped_tests.rs"]
mod mapped;

#[path = "remote_cache_tests.rs"]
mod remote_cache;

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
        for property in [
            "Slice=motor-jobs.slice",
            "OOMScoreAdjust=900",
            "OOMPolicy=kill",
        ] {
            assert!(args.contains(&format!("--property={property}").as_str()));
        }
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
#[ignore = "requires installed memory-policy units and a running user-service runner on a dedicated host"]
fn isolated_job_memory_budget_and_oom_priority_protect_runner() {
    let f = Fixture::new();
    let sandbox = f.sandbox();
    let (status, stdout, stderr) = output(
        &sandbox,
        "cat /proc/self/oom_score_adj /proc/self/cgroup",
        &f.workspace,
    );
    assert!(status.success(), "{stderr}");
    assert_eq!(stdout.lines().next(), Some("900"));
    assert!(
        stdout.contains("/motor.slice/motor-jobs.slice/"),
        "{stdout}"
    );

    let runner_unit =
        std::env::var("MOTOR_TEST_RUNNER_UNIT").unwrap_or_else(|_| "motor-runner.service".into());
    let runner = Command::new("/usr/bin/systemctl")
        .args([
            "--user",
            "show",
            "--value",
            "--property=MainPID",
            &runner_unit,
        ])
        .output()
        .unwrap();
    assert!(runner.status.success());
    let pid: u32 = String::from_utf8(runner.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        pid > 0,
        "start the installed runner user service before this test"
    );
    assert_eq!(
        fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))
            .unwrap()
            .trim(),
        "-900"
    );
    let group = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert!(!group.contains("/motor-jobs.slice"), "{group}");

    crate::systemd::read_job_memory_budget().unwrap();
    let group = Path::new("/sys/fs/cgroup/motor.slice/motor-jobs.slice");
    let limit = |name| {
        fs::read_to_string(group.join(name))
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
    };
    let high = limit("memory.high");
    let max = limit("memory.max");
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap();
    let total: u64 = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024;
    assert!(
        high > 0 && high < max && max < total,
        "high={high} max={max} total={total}"
    );
}

fn test_identity() -> pid_init::JobIdentity {
    pid_init::JobIdentity {
        uid: 997,
        gid: 996,
        groups: vec![996],
        proc_protections: vec![("/proc/sys".into(), true)],
    }
}

#[test]
fn boat_command_preserves_isolation_and_limits_without_private_pids() {
    let f = Fixture::new();
    for (boat, private_pids) in [(false, true), (true, false), (true, true)] {
        let command = f
            .command_sandbox()
            .with_boat_profile(boat, private_pids, Some(14 * (1 << 30)))
            .with_pid_init(Some(test_identity()))
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
            "ProtectKernelTunables=yes",
            "NoNewPrivileges=yes",
            "KillMode=control-group",
        ] {
            assert!(args.contains(&format!("--property={property}").as_str()));
        }
        // Without PrivatePIDs the unit starts as root for the PID namespace
        // init, which drops to motor-job itself before the wrapper runs.
        assert_eq!(args.contains(&"--property=User=motor-job"), private_pids);
        assert!(args
            .iter()
            .any(|a| a.starts_with("--property=TemporaryFileSystem=/dev/shm:")));
        assert_eq!(args.contains(&"--property=CPUQuota=200%"), boat);
        assert_eq!(args.contains(&"--property=TasksMax=4096"), boat);
        // Boat jobs see the same non-process /proc entries as build-server
        // jobs (/proc/stat, /proc/meminfo, ...): the sandbox VM runs one job,
        // so those entries describe nothing but that job's own machine.
        assert!(!args.iter().any(|a| a.contains("ProcSubset")), "{args:?}");
        assert_eq!(
            args.contains(
                &"--property=UnsetEnvironment=ASCII_TOKEN BOAT_CURRENT_ID_FILE BOAT_FUTURE BOAT_ID"
            ),
            boat
        );
    }
}

/// The Boat unit for systemd before 257 differs from the PrivatePIDs Boat
/// unit only where the PID namespace init needs it: no PrivatePIDs, no User=
/// or Group=, the init's capabilities, RestrictNamespaces=pid, the setup-error
/// bind, and the init in front of the unchanged wrapper argv.
#[test]
fn boat_pid_init_unit_changes_only_what_the_init_needs() {
    let f = Fixture::new();
    let render = |private_pids: bool, durable: bool| {
        let sandbox = f
            .command_sandbox()
            .with_boat_profile(true, private_pids, Some(14 << 30))
            .with_pid_init(Some(test_identity()));
        let mut sandbox = sandbox;
        sandbox.read_write = vec![f.workspace.to_str().unwrap().into()];
        let env = [("RUST_LOG".to_string(), "info".to_string())];
        let command = sandbox
            .command("cargo test", &f.workspace, &env, durable, &[1, 2], 2)
            .unwrap();
        let id = sandbox.id.to_string();
        command
            .get_args()
            .map(|arg| arg.to_str().unwrap().replace(&id, "<ID>"))
            .collect::<Vec<_>>()
    };
    let setup_error = format!(
        "{}:/motor-output/setup-error",
        f.scratch.join("unprepared/output/setup-error").display()
    );
    let workspace = f.workspace.to_str().unwrap();
    for durable in [true, false] {
        let private = render(true, durable);
        let init = render(false, durable);
        let removed = [
            "--property=PrivatePIDs=yes".to_owned(),
            "--property=User=motor-job".to_owned(),
            "--property=Group=motor-job".to_owned(),
            "--property=CapabilityBoundingSet=".to_owned(),
            "--property=RestrictNamespaces=yes".to_owned(),
            format!("--property=BindPaths={workspace}"),
        ];
        let added = [
            format!(
                "--property=CapabilityBoundingSet={}",
                pid_init::UNIT_CAPABILITIES
            ),
            "--property=RestrictNamespaces=pid".to_owned(),
            format!("--property=BindPaths={workspace} {setup_error}"),
        ];
        for arg in removed.iter() {
            assert!(private.contains(arg), "{arg} missing from {private:?}");
            assert!(!init.contains(arg), "{arg} kept in {init:?}");
        }
        for arg in added.iter() {
            assert!(init.contains(arg), "{arg} missing from {init:?}");
        }
        // The init's own arguments sit right before the wrapper's.
        let start = init
            .iter()
            .position(|arg| arg == pid_init::INSTALLED_RUNNER)
            .unwrap();
        assert_eq!(
            init[start..start + 7],
            [
                pid_init::INSTALLED_RUNNER,
                pid_init::MODE,
                "997",
                "996",
                "996",
                "[[\"/proc/sys\",true]]",
                "--"
            ]
        );
        assert_eq!(init[start + 7], "/bin/bash");
        // Everything else is the same argv in the same order.
        let rest = |args: &[String], drop: &[String]| -> Vec<String> {
            args.iter()
                .filter(|arg| !drop.contains(arg))
                .cloned()
                .collect()
        };
        let mut init_rest = rest(&init, &added);
        init_rest.drain(start - added.len()..start - added.len() + 7);
        assert_eq!(init_rest, rest(&private, &removed), "durable={durable}");
    }
}

#[test]
fn boat_var_tmp_bind_uses_the_same_quota_covered_runtime_as_tmp() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let sandbox = f
        .command_sandbox()
        .with_boat_profile(true, false, Some(14 << 30));
    fs::create_dir_all(&sandbox.root).unwrap();
    crate::systemd::with_process_hook(
        Box::new(|_| {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }),
        || {
            let sandbox = sandbox.with_boat_temporary_paths().unwrap();
            assert_eq!(
                sandbox.read_write,
                [format!(
                    "{}:/var/tmp",
                    f.scratch.join("unprepared/tmp").display()
                )]
            );
            assert!(sandbox.root.join("var").is_dir());
            assert!(!fs::symlink_metadata(sandbox.root.join("var"))
                .unwrap()
                .file_type()
                .is_symlink());
        },
    );
}

#[test]
fn boat_writer_unit_is_structurally_refused_even_if_cache_is_requested() {
    with_prepared_boat_sandbox(true, |f, sandbox| {
        let env = [("SCCACHE_DIR".into(), "/motor-cache".into())];
        let error = sandbox
            .command(
                "touch /motor-cache/unlimited",
                &f.workspace,
                &env,
                true,
                &[1],
                1,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "Boat jobs cannot use dependency writer units"
        );
    });
}

/// The answer to a command sandbox preparation runs that needs sudo or the
/// job accounts, so the real mount construction can run without either:
/// `id` reports made-up account ids, and sudo, setfacl, chown and chmod
/// succeed without doing anything. Every other command gets `None` and runs.
fn fake_privileged_output(command: &Command) -> Option<io::Result<std::process::Output>> {
    use std::os::unix::process::ExitStatusExt;
    let program = command.get_program().to_string_lossy();
    if program.ends_with("/id") {
        let args: Vec<_> = command.get_args().collect();
        let stdout = if args[0] == "-u" {
            format!(
                "{}\n",
                unsafe { libc::geteuid() } + if args[1] == WRITER_USER { 10002 } else { 10001 }
            )
        } else {
            format!("{}\n", args[1].to_string_lossy())
        };
        return Some(Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        }));
    }
    if ["sudo", "setfacl", "chown", "chmod"].iter().any(|name| {
        Path::new(command.get_program())
            .file_name()
            .is_some_and(|actual| actual == *name)
    }) {
        return Some(Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }));
    }
    None
}

fn with_prepared_boat_sandbox(writer: bool, inspect: impl FnOnce(&Fixture, Sandbox)) {
    let f = Fixture::new();
    fs::create_dir_all(f.workspace.join("src")).unwrap();
    fs::write(f.workspace.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        f.workspace.join("Cargo.toml"),
        "[package]\nname = 'quota-mount-fixture'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(
        f.workspace.join("Cargo.lock"),
        "version = 3\n[[package]]\nname = 'quota-mount-fixture'\nversion = '0.1.0'\n",
    )
    .unwrap();
    workspace::git(&f.workspace, &["init", "-q"]).unwrap();
    workspace::git(&f.workspace, &["add", "."]).unwrap();
    let tree = workspace::git(&f.workspace, &["write-tree"]).unwrap();
    let target = f.scratch.join("target");
    fs::create_dir_all(&target).unwrap();
    let id = Uuid::now_v7();
    // Exercise the real mount construction. Only account/privileged operations
    // are substituted; Git, Cargo fetch and the filesystem layout remain real.
    crate::systemd::with_process_hook(
        Box::new(|command| fake_privileged_output(command).unwrap_or_else(|| command.output())),
        || {
            let javascript = JavascriptTools::for_role(ToolchainRole::Boat);
            let sandbox = if writer {
                Sandbox::prepare_writer(
                    id,
                    &f.state,
                    &f.workspace,
                    &target,
                    None,
                    &f.state.join("slots"),
                    1,
                    &BTreeMap::new(),
                    &tree,
                    &javascript,
                    true,
                    None,
                )
            } else {
                Sandbox::prepare_role(
                    id,
                    &f.state,
                    &f.workspace,
                    &target,
                    &[],
                    None,
                    &f.state.join("slots"),
                    1,
                    &BTreeMap::new(),
                    &tree,
                    f.workspace.to_str().unwrap(),
                    false,
                    false,
                    false,
                    None,
                    &javascript,
                    None,
                )
                .map(|sandbox| sandbox.with_boat_profile(true, true, Some(14 << 30)))
            }
            .unwrap()
            .with_boat_temporary_paths()
            .unwrap()
            .with_pid_init(Some(test_identity()));
            inspect(&f, sandbox);
        },
    );
}

#[test]
fn boat_unit_every_writable_bind_is_backed_by_a_job_quota_root() {
    use std::os::unix::ffi::OsStringExt;
    for writer in [false, true] {
        with_prepared_boat_sandbox(writer, |f, mut sandbox| {
            let target = f.scratch.join("target");
            let roots = crate::boat_quota::job_roots(
                &f.workspace,
                &target,
                &sandbox.runtime.0,
                &f.state.join("spools").join("fixture"),
            );
            for private_pids in [true, false] {
                sandbox.private_pids = private_pids;
                let command = sandbox.command("true", &f.workspace, &[], true, &[1], 1);
                if writer {
                    assert_eq!(
                        command.unwrap_err().to_string(),
                        "Boat jobs cannot use dependency writer units"
                    );
                    continue;
                }
                let command = command.unwrap();
                let args: Vec<_> = command
                    .get_args()
                    .map(|arg| arg.to_str().unwrap())
                    .collect();
                assert!(args.contains(&"--property=ProtectSystem=strict"));
                assert!(args.contains(&"--property=ProtectHome=tmpfs"));
                assert!(!args
                    .iter()
                    .any(|arg| arg.starts_with("--property=ReadWritePaths=")));
                let binds = args
                    .iter()
                    .find_map(|arg| arg.strip_prefix("--property=BindPaths="))
                    .unwrap();
                assert!(!binds.is_empty(), "empty mount plan cannot prove coverage");
                for bind in binds.split_whitespace() {
                    let source = bind.split(':').next().unwrap();
                    let mut bytes = Vec::new();
                    let mut encoded = source.bytes();
                    while let Some(byte) = encoded.next() {
                        match byte {
                            b'\\' => {
                                assert_eq!(encoded.next(), Some(b'x'));
                                let hex: String =
                                    encoded.by_ref().take(2).map(char::from).collect();
                                bytes.push(u8::from_str_radix(&hex, 16).unwrap());
                            }
                            b'%' => {
                                assert_eq!(encoded.next(), Some(b'%'));
                                bytes.push(b'%');
                            }
                            byte => bytes.push(byte),
                        }
                    }
                    let source = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                    assert!(
                        roots.iter().any(|root| source.starts_with(root)),
                        "unquota'd writable source in Boat unit: {}",
                        source.display()
                    );
                }
            }
        });
    }
}

#[test]
fn boat_pid_init_unit_refuses_loader_variables_and_a_missing_identity() {
    let f = Fixture::new();
    let sandbox = || {
        f.command_sandbox()
            .with_boat_profile(true, false, Some(14 << 30))
    };
    let error = sandbox()
        .command("true", &f.workspace, &[], true, &[1], 1)
        .unwrap_err();
    assert!(error.to_string().contains("identity"), "{error}");
    let env = [("LD_PRELOAD".to_string(), "/tmp/x.so".to_string())];
    let error = sandbox()
        .with_pid_init(Some(test_identity()))
        .command("true", &f.workspace, &env, true, &[1], 1)
        .unwrap_err();
    assert!(error.to_string().contains("LD_PRELOAD"), "{error}");
    // Units without the init keep their environment rules.
    assert!(f
        .command_sandbox()
        .command("true", &f.workspace, &env, true, &[1], 1)
        .is_ok());
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

#[test]
fn job_memory_limit_never_exceeds_the_slice_memory_max() {
    let f = Fixture::new();
    // A 24 GiB slice, separate from any capacity-file Linux minimum.
    let slice = crate::systemd::JobMemoryBudget {
        max: 25_769_803_776,
        high: 21_474_836_480,
    };
    let memory_max = |vcpus, budget| {
        let sandbox = match budget {
            Some(budget) => f.command_sandbox().with_slice_budget(budget),
            None => f.command_sandbox(),
        };
        let command = sandbox
            .command("nproc", &f.workspace, &[], true, &[], vcpus)
            .unwrap();
        command
            .get_args()
            .filter_map(|arg| arg.to_str())
            .find_map(|arg| arg.strip_prefix("--property=MemoryMax="))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    // A request above the slice asks for the slice's own limit: the unit can
    // never be granted more, and the slice still reports an overrun as an OOM.
    assert_eq!(memory_max(30, Some(slice)), 25_769_803_776);
    // A request below the slice keeps its own limit.
    assert_eq!(memory_max(2, Some(slice)), 17_179_869_184);
    // Unread, a runner keeps the requested limit as before.
    assert_eq!(memory_max(30, None), 112_742_891_520);
    // An undeclared small host can clamp even below the per-job floor.
    let small = crate::systemd::JobMemoryBudget {
        max: 14_249_590_784,
        high: 12_573_168_640,
    };
    assert_eq!(memory_max(8, Some(small)), 14_249_590_784);
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
                std::sync::OnceLock::new(),
            )),
            retain_failed: false,
            network: false,
            tenant_vm: false,
            private_pids: true,
            boat_profile: false,
            boat_memory_max: None,
            pid_init: None,
            provider_unset_env: Vec::new(),
            cache_forwarders: Vec::new(),
            cache_generation: None,
            parent_umask: 0o022,
            memory_floor_gib: None,
            slice_budget: std::sync::Mutex::new(None),
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
/// A job unit asks for the slice's `MemoryMax` at most, and keeps its own
/// smaller request. The claim is derived from the slice actually read, so the
/// fixture holds on any host size. This starts a real sandbox and reads the
/// live slice, so it is an operator fixture:
///
/// ```text
/// cargo test -p motor-runner --lib -- --ignored isolated_job_unit_memory_max_is_clamped_to_the_jobs_slice
/// ```
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root and the motor-job account: it starts a real job unit"]
fn isolated_job_unit_memory_max_is_clamped_to_the_jobs_slice() {
    let slice = crate::systemd::read_job_memory_budget().unwrap();
    let f = Fixture::new();
    let memory_max = |sandbox: &Sandbox, vcpus: u32| {
        let command = sandbox
            .command("true", &f.workspace, &[], true, &[], vcpus)
            .unwrap();
        command
            .get_args()
            .filter_map(|arg| arg.to_str())
            .find_map(|arg| arg.strip_prefix("--property=MemoryMax="))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    let sandbox = f
        .sandbox()
        .with_slice_budget(slice)
        .with_memory_floor_gib(Some(16));
    // The smallest claim among the 32 vCPUs a VM may use whose own limit the
    // slice clamps. A slice above all of them cannot show a clamp at all.
    let claim = (1..=32).find(|vcpus| crate::backend::memory_max(*vcpus, Some(16)) > slice.max);
    let Some(claim) = claim else {
        eprintln!(
            "slice MemoryMax={} is above every admissible job limit; nothing to clamp",
            slice.max
        );
        return;
    };
    assert_eq!(memory_max(&sandbox, claim), slice.max);
    // A claim below the clamp keeps the job's own limit.
    assert_eq!(
        memory_max(&sandbox, 2),
        crate::backend::memory_max(2, Some(16))
    );
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
            std::sync::OnceLock::new(),
        )),
        retain_failed: false,
        network: false,
        tenant_vm: false,
        private_pids: true,
        boat_profile: false,
        boat_memory_max: None,
        pid_init: None,
        provider_unset_env: Vec::new(),
        cache_forwarders: Vec::new(),
        cache_generation: None,
        parent_umask: 0o022,
        memory_floor_gib: None,
        slice_budget: std::sync::Mutex::new(None),
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
                pid_init: None,
                provider_unset_env: Vec::new(),
                cache_forwarders: Vec::new(),
                cache_generation: None,
                parent_umask: 0o022,
                memory_floor_gib: None,
                slice_budget: std::sync::Mutex::new(None),
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
                pid_init: None,
                provider_unset_env: Vec::new(),
                cache_forwarders: Vec::new(),
                cache_generation: None,
                parent_umask: 0o022,
                memory_floor_gib: None,
                slice_budget: std::sync::Mutex::new(None),
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
            fd::AsRawFd,
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        },
    };
    // Other test threads keep forking processes, and each one shares every
    // open descriptor until it execs, so a dropped listener can go on
    // accepting for a moment. shutdown() stops a listening socket for every
    // copy at once. A child parked before exec holds copies during each check,
    // so the closed case cannot pass or fail by timing.
    fn stop_listening(fd: &impl AsRawFd) {
        assert_eq!(
            unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR) },
            0,
            "{}",
            io::Error::last_os_error()
        );
    }
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
    let forked = crate::test_helpers::fork_before_exec().unwrap();
    stop_listening(&tcp);
    assert!(!tcp_status());
    drop(forked);
    drop(tcp);
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
    let forked = crate::test_helpers::fork_before_exec().unwrap();
    stop_listening(&listener);
    assert!(!uds_status());
    drop(forked);
    drop(listener);
}

#[test]
fn durable_cache_drivers_preserve_raw_shell_comments_heredocs_and_exit_codes() {
    use std::io::Write;
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
        crate::test_helpers::write_test_executable(&tools.join(name), script.as_bytes()).unwrap();
    }
    // The writer shell runs this from the tool mount, as the sandbox installs
    // it; its Cargo check itself is the stub `python3` above.
    crate::test_helpers::write_test_executable(
        &tools.join("writer-cargo-check.sh"),
        crate::writer::fill::CARGO_CHECK.as_bytes(),
    )
    .unwrap();
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
            std::sync::OnceLock::new(),
        )),
        retain_failed: false,
        network: false,
        tenant_vm: false,
        private_pids: true,
        boat_profile: false,
        boat_memory_max: None,
        pid_init: None,
        provider_unset_env: Vec::new(),
        cache_forwarders: Vec::new(),
        cache_generation: None,
        parent_umask: 0o022,
        memory_floor_gib: None,
        slice_budget: std::sync::Mutex::new(None),
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
                .replace("/motor-output", output.to_str().unwrap())
                // The writer shell reaches the installed helpers through the
                // tool mount; this fixture provides them in a directory.
                .replace("/motor-tools", tools.to_str().unwrap());
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
    cargo_prefetch_fixture(false);
}

#[test]
fn nested_lockfile_prefetch_populates_offline_sources() {
    cargo_prefetch_fixture(true);
}

#[test]
fn cargo_prefetch_is_bounded_and_ignores_untracked_lockfiles() {
    let f = Fixture::new();
    let package = "[package]\nname='probe'\nversion='0.1.0'\n";
    fs::write(f.workspace.join("Cargo.toml"), package).unwrap();
    fs::write(f.workspace.join("Cargo.lock"), "version=4\n").unwrap();
    commit_cargo_fixture(&f.workspace);
    let untracked = f.workspace.join("untracked");
    fs::create_dir_all(&untracked).unwrap();
    fs::write(untracked.join("Cargo.toml"), package).unwrap();
    fs::write(untracked.join("Cargo.lock"), "version=4\n").unwrap();
    let mut skipped = Vec::new();
    assert!(
        cargo_lockfiles(&f.workspace, "HEAD", &mut |path, error| skipped
            .push((path.to_path_buf(), error.to_owned())))
        .unwrap()
        .is_empty()
    );
    assert!(skipped.is_empty());
    for i in 0..128 {
        let nested = f.workspace.join(format!("helper-{i}"));
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Cargo.toml"), package).unwrap();
        fs::write(nested.join("Cargo.lock"), "version=4\n").unwrap();
    }
    commit_cargo_fixture(&f.workspace);
    let locks = cargo_lockfiles(&f.workspace, "HEAD", &mut |path, error| {
        skipped.push((path.to_path_buf(), error.to_owned()))
    })
    .unwrap();
    assert_eq!(locks.len(), 127);
    assert_eq!(skipped.len(), 2);
    assert!(skipped
        .iter()
        .all(|(path, error)| path != Path::new("Cargo.lock")
            && error.contains("128 tracked Cargo.lock")));
}

#[test]
fn nested_cargo_fetch_failure_logs_lockfile_and_crate_without_refusing() {
    let f = Fixture::new();
    let helper = f.workspace.join("probes/helper");
    fs::create_dir_all(helper.join("src")).unwrap();
    fs::write(helper.join("src/lib.rs"), "").unwrap();
    fs::write(helper.join("Cargo.toml"), "[package]\nname='missing-crate'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nabsent={git='file:///nonexistent-motor-prefetch-dependency'}\n").unwrap();
    fs::write(helper.join("Cargo.lock"), "version=4\n").unwrap();
    commit_cargo_fixture(&f.workspace);
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".rustup")
            .into_os_string()
    }));
    let mut skipped = Vec::new();
    prefetch_cargo_with(
        &f.workspace,
        "HEAD",
        false,
        NESTED_CARGO_FETCH_TIMEOUT,
        |directory| {
            let mut command = workspace::dependencies::fetch_command(
                &f.state,
                directory,
                &BTreeMap::new(),
                &f.scratch.join("cargo"),
                &rustup,
            )?;
            command.env("CARGO_NET_OFFLINE", "true");
            Ok(command)
        },
        |path, error| skipped.push((path.to_path_buf(), error.to_owned())),
    )
    .unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].0, Path::new("probes/helper/Cargo.lock"));
    assert!(skipped[0].1.contains("absent"), "{skipped:?}");
}

#[test]
fn malformed_nested_cargo_pairs_are_skipped_with_diagnostics() {
    let f = Fixture::new();
    let missing = f.workspace.join("missing-manifest");
    let linked = f.workspace.join("linked-lock");
    fs::create_dir_all(&missing).unwrap();
    fs::create_dir_all(&linked).unwrap();
    fs::write(missing.join("Cargo.lock"), "version=4\n").unwrap();
    fs::write(linked.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(f.scratch.join("external-lock"), "version=4\n").unwrap();
    std::os::unix::fs::symlink(f.scratch.join("external-lock"), linked.join("Cargo.lock")).unwrap();
    commit_cargo_fixture(&f.workspace);
    let mut skipped = Vec::new();
    let locks = cargo_lockfiles(&f.workspace, "HEAD", &mut |path, error| {
        skipped.push((path.to_path_buf(), error.to_owned()))
    })
    .unwrap();
    assert!(locks.is_empty());
    assert_eq!(skipped.len(), 2);
    assert!(skipped
        .iter()
        .all(|(_, error)| error.contains("tracked regular Cargo.lock")));
    assert!(skipped
        .iter()
        .any(|(path, _)| path == Path::new("linked-lock/Cargo.lock")));
    assert!(skipped
        .iter()
        .any(|(path, _)| path == Path::new("missing-manifest/Cargo.lock")));
}

#[test]
fn stale_nested_lock_does_not_refuse_and_root_still_fetches() {
    cargo_stale_lock_fixture(false);
}

#[test]
fn stale_root_lock_still_refuses_before_nested_fetch() {
    cargo_stale_lock_fixture(true);
}

fn cargo_package_fixture(path: &Path, name: &str, stale: bool) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(path.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        path.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.2.0'\nedition='2021'\n[workspace]\n"),
    )
    .unwrap();
    let version = if stale { "0.1.0" } else { "0.2.0" };
    fs::write(
        path.join("Cargo.lock"),
        format!("version=4\n[[package]]\nname='{name}'\nversion='{version}'\n"),
    )
    .unwrap();
}

fn cargo_stale_lock_fixture(stale_root: bool) {
    let f = Fixture::new();
    cargo_package_fixture(&f.workspace, "root-probe", stale_root);
    let nested = f.workspace.join("probes/helper");
    cargo_package_fixture(&nested, "nested-probe", !stale_root);
    commit_cargo_fixture(&f.workspace);
    let root_lock = fs::read(f.workspace.join("Cargo.lock")).unwrap();
    let nested_lock = fs::read(nested.join("Cargo.lock")).unwrap();
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".rustup")
            .into_os_string()
    }));
    let mut fetched = Vec::new();
    let mut skipped = Vec::new();
    let result = prefetch_cargo_with(
        &f.workspace,
        "HEAD",
        false,
        NESTED_CARGO_FETCH_TIMEOUT,
        |directory| {
            fetched.push(directory.to_path_buf());
            let mut command = workspace::dependencies::fetch_command(
                &f.state,
                directory,
                &BTreeMap::new(),
                &f.scratch.join("cargo"),
                &rustup,
            )?;
            command.env("CARGO_NET_OFFLINE", "true");
            Ok(command)
        },
        |path, error| skipped.push((path.to_path_buf(), error.to_owned())),
    );
    if stale_root {
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("--locked was passed to prevent this"),
            "{error}"
        );
        assert_eq!(fetched.as_slice(), std::slice::from_ref(&f.workspace));
        assert!(skipped.is_empty());
    } else {
        result.unwrap();
        assert_eq!(fetched, [f.workspace.clone(), nested.clone()]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, Path::new("probes/helper/Cargo.lock"));
        assert!(
            skipped[0].1.contains("--locked was passed to prevent this"),
            "{skipped:?}"
        );
    }
    assert_eq!(fs::read(f.workspace.join("Cargo.lock")).unwrap(), root_lock);
    assert_eq!(fs::read(nested.join("Cargo.lock")).unwrap(), nested_lock);
}

#[test]
fn nested_cargo_fetch_deadline_skips_stall_and_gives_next_lock_a_fresh_bound() {
    let f = Fixture::new();
    for name in ["a-stalled", "b-ready"] {
        cargo_package_fixture(&f.workspace.join(name), name, false);
    }
    commit_cargo_fixture(&f.workspace);
    let marker = f.scratch.join("ready");
    let mut skipped = Vec::new();
    let started = Instant::now();
    prefetch_cargo_with(
        &f.workspace,
        "HEAD",
        false,
        Duration::from_millis(200),
        |directory| {
            let mut command = Command::new("sh");
            command.arg("-c");
            if directory.ends_with("a-stalled") {
                command.arg("printf 'cargo diagnostic before stall' >&2; sleep 30");
            } else {
                command
                    .arg("sleep 0.02; printf fetched > \"$MARKER\"")
                    .env("MARKER", &marker);
            }
            Ok(command)
        },
        |path, error| skipped.push((path.to_path_buf(), error.to_owned())),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(fs::read_to_string(marker).unwrap(), "fetched");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].0, Path::new("a-stalled/Cargo.lock"));
    assert!(skipped[0].1.contains("timed out"), "{skipped:?}");
    assert!(
        skipped[0].1.contains("cargo diagnostic before stall"),
        "{skipped:?}"
    );
}

fn commit_cargo_fixture(path: &Path) {
    workspace::git(path, &["init", "-q"]).unwrap();
    workspace::git(path, &["add", "."]).unwrap();
    workspace::git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "snapshot",
        ],
    )
    .unwrap();
}

fn cargo_prefetch_fixture(nested: bool) {
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
    fs::write(tip.join("Cargo.toml"), package).unwrap();
    fs::copy(branch.join("Cargo.lock"), tip.join("Cargo.lock")).unwrap();
    let helper = if nested {
        tip.join("probes/helper")
    } else {
        tip.clone()
    };
    fs::create_dir_all(helper.join("src")).unwrap();
    fs::write(helper.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
    fs::write(
        helper.join("Cargo.toml"),
        format!("{package}[dependencies]\ntip_only={{git='{url}',rev='{oid}'}}\n"),
    )
    .unwrap();
    let lock = format!("version=4\n[[package]]\nname='branch-first'\nversion='0.1.0'\ndependencies=['tip_only']\n[[package]]\nname='tip_only'\nversion='0.1.0'\nsource='git+{url}?rev={oid}#{oid}'\n");
    fs::write(helper.join("Cargo.lock"), &lock).unwrap();
    for root in [&tip, &branch] {
        commit_cargo_fixture(root);
    }
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
            .current_dir(&helper)
            .env("CARGO_HOME", &cargo)
            .env("RUSTUP_HOME", &rustup)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "")
            .output()
            .unwrap()
    };
    prefetch_cargo_dependencies(
        false,
        false,
        &f.state,
        &branch,
        "HEAD",
        &BTreeMap::new(),
        &cargo,
        &rustup,
        None,
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
    prefetch_cargo_dependencies(
        true, false, &f.state, &tip, "HEAD", &routes, &cargo, &rustup, None,
    )
    .unwrap();
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
    assert_eq!(fs::read_to_string(helper.join("Cargo.lock")).unwrap(), lock);
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
#[ignore = "operator only: non-root account with sudo -n, provisioned motor_net and trusted host isolation"]
async fn isolated_net_generation_appears_after_runner_start_without_restart() {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run generation operator tests as the operator account, not via sudo"
    );
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("generation-recovery").await;
}

#[tokio::test]
#[ignore = "operator only: non-root account with sudo -n, provisioned motor_net and trusted host isolation"]
async fn isolated_net_failed_reload_refuses_new_jobs() {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run generation operator tests as the operator account, not via sudo"
    );
    let _guard = net_operator_lock();
    crate::engine::net_operator_case("generation-reload").await;
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
    let (_generation, _) = crate::network::linux::validated_generation(&f.state);
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
    assert_eq!(
        crate::network::unsupported(&config),
        None,
        "startup must leave actual net admission ready, not merely configured"
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

/// The trusted prefetch resolves a `../b/crate` path dependency from the
/// sibling's snapshot, not from the host checkout at the sibling's path. The
/// host checkout's crate declares a dependency that the workspace's
/// Cargo.lock lacks, so a `--locked` fetch reading it fails; the snapshot's
/// crate matches the lock.
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_prefetch_resolves_sibling_path_dependency_from_snapshot() {
    let scratch = cortexkit_test_support::ScratchDir::new("snapshot-prefetch-view");
    let mut config = crate::engine::Config::new(
        scratch.join("runner"),
        format!("isotest{}", &Uuid::now_v7().simple().to_string()[24..]),
    );
    config.isolate_jobs = true;
    let state = config.state_dir();
    fs::create_dir_all(&state).unwrap();
    let workspace = scratch.join("source/a");
    let host = scratch.join("source/b");
    let snapshot = config.sibling_snapshots_dir().join("b/1");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='a'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nb-crate={path='../b/crate'}\n",
    )
    .unwrap();
    fs::write(
        workspace.join("Cargo.lock"),
        "version=4\n[[package]]\nname='a'\nversion='0.1.0'\ndependencies=['b-crate']\n[[package]]\nname='b-crate'\nversion='0.1.0'\n",
    )
    .unwrap();
    let package = "[package]\nname='b-crate'\nversion='0.1.0'\nedition='2021'\n";
    for (root, manifest) in [
        (&snapshot, package.to_string()),
        (
            &host,
            format!("{package}[dependencies]\nmotor_missing_probe='=0.1.0'\n"),
        ),
    ] {
        fs::create_dir_all(root.join("crate/src")).unwrap();
        fs::write(root.join("crate/src/lib.rs"), "pub fn b() {}\n").unwrap();
        fs::write(root.join("crate/Cargo.toml"), manifest).unwrap();
    }
    let cargo = scratch.join("cargo-home");
    fs::create_dir_all(&cargo).unwrap();
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".rustup")
            .into_os_string()
    }));
    let fetch = |snapshots: Option<&SnapshotSiblings<'_>>| {
        prefetch_cargo_dependencies(
            false,
            false,
            &state,
            &workspace,
            "HEAD",
            &BTreeMap::new(),
            &cargo,
            &rustup,
            snapshots,
        )
    };
    // Today's in-place prefetch reads the host checkout and fails.
    assert!(fetch(None).is_err());
    fetch(Some(&SnapshotSiblings {
        config: &config,
        siblings: vec![(snapshot.clone(), host.clone())],
    }))
    .unwrap();
}

/// A job sees each sibling at its canonical path and, from a worktree in
/// another directory, at the `../<name>` alias. With in-place siblings both
/// mounts show the checkout at the canonical path; in snapshot mode both show
/// the sibling's snapshot, never the host checkout at that path.
#[test]
fn sibling_mounts_bind_the_snapshot_at_the_sibling_path_and_its_alias() {
    let config = crate::engine::Config::new("/state-root", "main");
    let sibling = |path: &str| SiblingSnapshot {
        canonical_path: path.into(),
        commit: "a".repeat(40),
        tree_hash: "b".repeat(40),
        base_commit: None,
    };
    let worktree = Path::new("/w/worktrees/a");
    let beside = Path::new("/w/source/a");
    let siblings = [sibling("/w/source/commons")];
    let snapshot = PathBuf::from("/state-root/main/sibling-snapshots/commons/s1");
    let snapshots = SnapshotSiblings {
        config: &config,
        siblings: vec![(snapshot, PathBuf::from("/w/source/commons"))],
    };
    let mounts = |workspace: &Path, snapshots: Option<&SnapshotSiblings<'_>>| {
        sibling_mounts(workspace, &siblings, snapshots).unwrap()
    };

    assert_eq!(
        mounts(worktree, None),
        [
            (
                "/w/source/commons".to_string(),
                PathBuf::from("/w/source/commons")
            ),
            (
                "/w/source/commons:/w/worktrees/commons".to_string(),
                PathBuf::from("/w/worktrees/commons")
            ),
        ]
    );
    assert_eq!(
        mounts(worktree, Some(&snapshots)),
        [
            (
                "/state-root/main/sibling-snapshots/commons/s1:/w/source/commons".to_string(),
                PathBuf::from("/w/source/commons")
            ),
            (
                "/state-root/main/sibling-snapshots/commons/s1:/w/worktrees/commons".to_string(),
                PathBuf::from("/w/worktrees/commons")
            ),
        ]
    );
    // Beside the sibling, `../commons` is its canonical path: one mount.
    assert_eq!(
        mounts(beside, Some(&snapshots)),
        [(
            "/state-root/main/sibling-snapshots/commons/s1:/w/source/commons".to_string(),
            PathBuf::from("/w/source/commons")
        )]
    );
    // A sibling without a resolved snapshot is refused rather than mounted
    // from the host.
    assert!(sibling_mounts(worktree, &[sibling("/w/source/other")], Some(&snapshots)).is_err());
}

/// The job's Git config names the workspace and every place a sibling is
/// mounted, the canonical path and the `../<name>` alias, exactly and without
/// a wildcard.
#[test]
fn job_git_config_lists_the_workspace_and_both_sibling_mounts_as_safe() {
    let scratch = cortexkit_test_support::ScratchDir::new("safe-directories");
    let config = crate::engine::Config::new("/state-root", "main");
    let workspace = Path::new("/w/worktrees/a");
    let siblings = [SiblingSnapshot {
        canonical_path: "/w/source/commons".into(),
        commit: "a".repeat(40),
        tree_hash: "b".repeat(40),
        base_commit: None,
    }];
    let snapshots = SnapshotSiblings {
        config: &config,
        siblings: vec![(
            PathBuf::from("/state-root/main/sibling-snapshots/commons/s1"),
            PathBuf::from("/w/source/commons"),
        )],
    };
    for snapshots in [None, Some(&snapshots)] {
        let file = scratch.join(format!("gitconfig-{}", snapshots.is_some()));
        let mounts = sibling_mounts(workspace, &siblings, snapshots).unwrap();
        write_safe_directories(&file, &safe_directories(workspace, &mounts)).unwrap();
        let listed = workspace::git(
            &scratch,
            &[
                "config",
                "--file",
                file.to_str().unwrap(),
                "--get-all",
                "safe.directory",
            ],
        )
        .unwrap();
        assert_eq!(
            listed.lines().collect::<Vec<_>>(),
            [
                "/w/worktrees/a",
                "/w/source/commons",
                "/w/worktrees/commons"
            ],
            "snapshots: {}",
            snapshots.is_some()
        );
    }
}

/// Git inside a job reads a sibling owned by another account, through a
/// read-only mount, at both the sibling's path and its alias. Bind mounts
/// need root, so the test stands in for each mount with a read-only copy of a
/// dissociated snapshot, and Git's own test switch
/// makes it treat every repository as owned by someone else. With the job's
/// Git config, `rev-parse`, `log` and `status` work and print no warning; a
/// directory the config does not list is refused, as the alias was before it
/// was listed.
#[test]
fn job_git_reads_a_sibling_owned_by_another_account_at_its_path_and_alias() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = cortexkit_test_support::ScratchDir::new("safe-sibling-git");
    let root = fs::canonicalize(&*scratch).unwrap();
    let git_in = |directory: &Path, args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    git_in(&source, &["init", "-q"]);
    fs::write(source.join("source"), "first\n").unwrap();
    git_in(&source, &["add", "."]);
    git_in(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "first",
        ],
    );
    let head = workspace::git(&source, &["rev-parse", "HEAD"]).unwrap();
    // A snapshot as creation leaves it: a shared clone, dissociated from the
    // repository it was cloned from.
    let snapshot = root.join("snapshot");
    git_in(
        &root,
        &[
            "clone",
            "-q",
            "--shared",
            source.to_str().unwrap(),
            snapshot.to_str().unwrap(),
        ],
    );
    assert!(snapshot.join(".git/objects/info/alternates").is_file());
    crate::sibling_snapshots::store::dissociate(&snapshot).unwrap();
    fs::remove_dir_all(&source).unwrap();

    let workspace = root.join("w/worktrees/a");
    let canonical = root.join("w/source/commons");
    let alias = root.join("w/worktrees/commons");
    let unlisted = root.join("w/worktrees/unlisted");
    let siblings = [SiblingSnapshot {
        canonical_path: canonical.to_string_lossy().into_owned(),
        commit: head.clone(),
        tree_hash: "b".repeat(40),
        base_commit: None,
    }];
    let config = crate::engine::Config::new("/state-root", "main");
    let snapshots = SnapshotSiblings {
        config: &config,
        siblings: vec![(snapshot.clone(), canonical.clone())],
    };
    let mounts = sibling_mounts(&workspace, &siblings, Some(&snapshots)).unwrap();
    let gitconfig = root.join("gitconfig");
    write_safe_directories(&gitconfig, &safe_directories(&workspace, &mounts)).unwrap();
    for (_, destination) in &mounts {
        copy_read_only(&snapshot, destination);
    }
    copy_read_only(&snapshot, &unlisted);
    for path in [&canonical, &alias, &unlisted] {
        let mode = fs::metadata(path.join("source"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0, "{} is read-only", path.display());
    }

    let run = |directory: &Path, args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    };
    for path in [&canonical, &alias] {
        for (args, expected) in [
            (&["rev-parse", "HEAD"][..], format!("{head}\n")),
            (&["log", "-1", "--format=%H"][..], format!("{head}\n")),
            (&["status", "--porcelain"][..], String::new()),
        ] {
            let output = run(path, args);
            assert!(
                output.status.success() && output.stderr.is_empty(),
                "git {args:?} at {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
    }
    let refused = run(&unlisted, &["rev-parse", "HEAD"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("dubious ownership"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    for path in [&canonical, &alias, &unlisted] {
        make_writable(path);
    }
}

/// Copies `source` to `target` and removes every write bit, as a read-only
/// mount of a snapshot shows it.
fn copy_read_only(source: &Path, target: &Path) {
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let status = Command::new("cp")
        .arg("-pR")
        .arg(source)
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("chmod")
        .args(["-R", "a-w"])
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Gives the owner write access again, so the scratch directory can go.
fn make_writable(path: &Path) {
    let status = Command::new("chmod")
        .args(["-R", "u+w"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "requires a trusted operator host with root mount namespaces"]
fn isolated_boat_full_disk_wrapper_preserves_preallocated_exit() {
    let f = Fixture::new();
    let mut sandbox = f.command_sandbox();
    sandbox.boat_profile = true;
    sandbox.private_pids = false;
    sandbox.pid_init = Some(test_identity());
    let command = sandbox
        .command("exit 23", &f.workspace, &[], true, &[], 4)
        .unwrap();
    let shell = command
        .get_args()
        .find_map(|arg| arg.to_str().filter(|s| s.starts_with("IFS= read")))
        .unwrap()
        .replace("/motor-output", "\"$MOTOR_OUTPUT\"");
    let mount = f.scratch.join("full-disk");
    fs::create_dir(&mount).unwrap();
    let output = crate::privilege::command("/usr/bin/unshare")
        .args([
            "--mount",
            "/bin/bash",
            "-c",
            r#"
set -eu
mount --make-rprivate /
mount -t tmpfs -o size=1M tmpfs "$1"
export MOTOR_OUTPUT="$1"
printf '%32s' '' >"$1/exit"
touch "$1/stdout" "$1/stderr"
dd if=/dev/zero of="$1/fill" bs=4096 status=none 2>/dev/null && exit 91
# A failed fresh write proves the fixture really exhausted the mounted volume.
if printf x >"$1/control" 2>/dev/null; then exit 92; fi
set +e
/bin/bash -c "$2" motor-job 'exit 23' <<<g
code=$?
set -e
test "$code" = 23
test "$(stat -c %s "$1/exit")" = 32
test "$(tr -d ' \n' <"$1/exit")" = 23
"#,
            "full-disk-probe",
        ])
        .arg(&mount)
        .arg(&shell)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status={:?}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
