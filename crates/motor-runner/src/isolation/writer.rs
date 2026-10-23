use super::*;

#[test]
fn writer_identity_refuses_missing_aliases_and_administrative_groups() {
    let runner = unsafe { libc::geteuid() };
    let writer = if runner == 50001 { 50002 } else { 50001 };
    assert!(validate_identity(WRITER_USER, Some(writer), &[50000], Some("motor-cache")).is_ok());
    for uid in [None, Some(0), Some(runner), Some(50000)] {
        assert!(
            validate_identity(WRITER_USER, uid, &[50000], Some("motor-cache")).is_err(),
            "unsafe writer uid {uid:?}"
        );
    }
    for group in ["sudo", "admin", "adm", "lxd", "docker"] {
        assert!(validate_identity(
            WRITER_USER,
            Some(writer),
            &[50000],
            Some(&format!("motor-cache {group}"))
        )
        .is_err());
    }
    for groups in [None, Some("")] {
        assert!(validate_identity(WRITER_USER, Some(writer), &[50000], groups).is_err());
    }
}

#[test]
fn writer_cache_policy_removes_job_write_access_and_inherits_reads() {
    assert_eq!(
        writer_cache_owner(WriterAccount { uid: 50123 }),
        "50123:motor-cache"
    );
    let directory = writer_cache_acl(true, false);
    let file = writer_cache_acl(false, false);
    assert!(directory.split(',').any(|entry| entry == "u:motor-job:r-x"));
    assert!(directory
        .split(',')
        .any(|entry| entry == "d:u:motor-job:r-x"));
    assert!(directory.split(',').any(|entry| entry == "d:m::r-x"));
    assert!(directory.split(',').any(|entry| entry == "u::rwx"));
    assert!(file.split(',').any(|entry| entry == "u:motor-job:r--"));
    for acl in [&directory, &file] {
        assert!(acl.split(',').any(|entry| entry == "g::---"));
        assert!(acl.split(',').any(|entry| entry == "o::---"));
        assert!(!acl
            .split(',')
            .any(|entry| entry.starts_with("u:motor-job:") && entry.ends_with("rwx")));
    }
}

#[test]
fn writer_cache_eviction_policy_grants_only_runner_write_without_inheriting_it() {
    let acl = writer_cache_acl(true, true);
    let entries: Vec<_> = acl.split(',').collect();
    let runner = unsafe { libc::geteuid() };
    assert!(entries.contains(&format!("u:{runner}:rwx").as_str()));
    for entry in [
        "u:motor-job:r-x",
        "m::rwx",
        "g::---",
        "o::---",
        "d:u:motor-job:r-x",
        "d:m::r-x",
        "d:g::---",
        "d:o::---",
    ] {
        assert!(entries.contains(&entry), "missing {entry}: {acl}");
    }
    assert!(entries.contains(&format!("d:u:{runner}:r-x").as_str()));
}

struct ReclaimWriterCache(PathBuf);

impl Drop for ReclaimWriterCache {
    fn drop(&mut self) {
        reclaim_runtime(&self.0).expect("reclaim writer test scratch");
    }
}

#[test]
#[ignore = "requires operator sudo, ACLs and provisioned motor-cache and motor-job accounts"]
fn isolated_writer_cache_eviction_repairs_cross_parent_permissions() {
    use std::os::unix::fs::PermissionsExt;

    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "probe must run as the runner, not root"
    );
    let f = Fixture::new();
    let _reclaim = ReclaimWriterCache(f.scratch.clone());
    let account = validate_writer_account().unwrap();
    let config = crate::engine::Config::new(&f.scratch, "runner-state");
    let fleet = config.sccache_dir();
    let root = fleet.parent().unwrap();
    prepare_writer_cache(&fleet, account).unwrap();
    let trash = config.state_dir().join("trash");
    fs::create_dir(&trash).unwrap();
    fs::set_permissions(&trash, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fs::metadata(&trash).unwrap().uid(), unsafe {
        libc::geteuid()
    });

    for scenario in ["fleet-read-only", "legacy-parent", "chmod-mask"] {
        prepare_writer_cache(&fleet, account).unwrap();
        if scenario == "legacy-parent" {
            // Older setup granted only reads to the runner on the source parent.
            repair_writer_cache_path(root, true).unwrap();
        } else if scenario == "chmod-mask" {
            // chmod removes effective write access through the ACL mask but
            // leaves named job-write entries that eviction repair must not revive.
            for directory in [root, fleet.as_path()] {
                checked(
                    crate::privilege::command("/usr/bin/setfacl")
                        .args([
                            "-m",
                            &format!("u:{}:rwx,u:{USER}:rwx,m::rwx", unsafe { libc::geteuid() }),
                            "--",
                        ])
                        .arg(directory),
                )
                .unwrap();
                checked(
                    crate::privilege::command("/usr/bin/chmod")
                        .args(["0750", "--"])
                        .arg(directory),
                )
                .unwrap();
            }
        } else {
            // Source parent and destination are writable, yet fleet is not.
            // With no sticky bits, EACCES isolates Linux's moved-directory check.
            checked(
                Command::new("/bin/sh")
                    .args([
                        "-c",
                        "test -w \"$1\" && ! test -w \"$2\" && test -w \"$3\"",
                        "probe",
                    ])
                    .arg(root)
                    .arg(&fleet)
                    .arg(&trash),
            )
            .unwrap();
        }
        for directory in [root, fleet.as_path(), trash.as_path()] {
            assert_eq!(fs::metadata(directory).unwrap().mode() & 0o1000, 0);
            assert_job_cannot_write(directory);
        }
        let retired = trash.join(scenario);
        assert_eq!(
            fs::rename(&fleet, &retired).unwrap_err().raw_os_error(),
            Some(libc::EACCES),
            "{scenario}"
        );
        retire_writer_cache(&fleet, &retired).unwrap();
        assert!(!fleet.exists(), "{scenario}");
        assert!(retired.is_dir(), "{scenario}");
        assert_job_cannot_write(root);
        assert_job_cannot_write(&retired);
    }
}

#[test]
#[ignore = "requires provisioned motor-cache, motor-job, sccache 0.18.0, sudo, ACLs and a real system manager"]
fn isolated_writer_cache_is_readable_by_existing_reader_and_never_writable() {
    use crate::writer::CacheFill;
    use std::io::BufRead;
    use std::os::unix::fs::MetadataExt;

    let f = Fixture::new();
    let _reclaim = ReclaimWriterCache(f.scratch.clone());
    let account = validate_writer_account().unwrap();
    prepare_writer_shared_read_access().unwrap();
    let config = crate::engine::Config::new(&f.scratch, "runner-state");
    let fleet = config.sccache_dir();
    prepare_writer_cache(&fleet, account).unwrap();
    assert_eq!(fs::metadata(&fleet).unwrap().uid(), account.uid);
    // Each fill gets its own scratch tree, prepared while the operator still owns
    // it. A CacheServer hands its tree to motor-cache with mode 0700, after which
    // the operator can neither create files there nor run git on it to prepare
    // another sandbox.
    let scratch = config.writer_scratch_dir();
    fs::create_dir(&scratch).unwrap();
    let fill_tree = |name: &str, source: &str| {
        let tree = scratch.join(name);
        fs::create_dir(&tree).unwrap();
        fs::create_dir(tree.join("target")).unwrap();
        fs::write(tree.join(format!("{name}.rs")), source).unwrap();
        tree
    };
    let old_work = fill_tree("old", "pub fn value() -> u32 { 1 }\n");
    let new_work = fill_tree("new", "pub fn value() -> u32 { 2 }\n");
    let server = CacheServer::new(&config, &old_work, Uuid::now_v7());
    let first = CacheFill::begin_at(&fleet, account).unwrap();
    let first_diagnostics = server.compile("old");
    let old_entries = cache_entries(&fleet);
    assert!(
        !old_entries.is_empty(),
        "fixture must create a real sccache entry\n{first_diagnostics}"
    );
    for entry in &old_entries {
        assert_eq!(fs::metadata(entry).unwrap().mode() & 0o777, 0o600);
        assert_job_cannot_read(entry);
    }
    first.finish().unwrap();
    let old_metadata: Vec<_> = old_entries
        .iter()
        .map(|entry| (entry.clone(), fs::metadata(entry).unwrap()))
        .collect();

    let mut sandbox = f.sandbox();
    sandbox
        .read_only
        .push(bind(&fleet, Path::new("/motor-cache")).unwrap());
    let old_key = old_entries[0]
        .strip_prefix(&fleet)
        .unwrap()
        .to_str()
        .unwrap();
    let command = format!("cat /motor-cache/{old_key} >/dev/null || exit 40; printf 'ready\\n'; for i in $(seq 1 1500); do test -f cache-key && break; sleep 0.02; done; entry=/motor-cache/\"$(cat cache-key)\"; cat \"$entry\" >/dev/null || exit 41; if printf poison > \"$entry\" 2>/dev/null; then exit 42; fi; if touch /motor-cache/job-entry 2>/dev/null; then exit 43; fi; printf 'readable\\n'");
    let mut launch = sandbox.launch(&command, &f.workspace, &[]).unwrap();
    launch.release().unwrap();
    let mut stdout = std::io::BufReader::new(&mut launch.stdout);
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    let second = CacheFill::begin_at(&fleet, account).unwrap();
    let second_server = CacheServer::new(&config, &new_work, Uuid::now_v7());
    let second_diagnostics = second_server.compile("new");
    let new_entries: Vec<_> = cache_entries(&fleet)
        .into_iter()
        .filter(|entry| !old_entries.contains(entry))
        .collect();
    assert!(
        !new_entries.is_empty(),
        "second fill must create a distinct entry\n{second_diagnostics}"
    );
    for entry in &new_entries {
        assert_eq!(fs::metadata(entry).unwrap().uid(), account.uid);
        assert_eq!(fs::metadata(entry).unwrap().mode() & 0o777, 0o600);
        assert_job_cannot_read(entry);
    }
    second.finish().unwrap();
    for (entry, before) in old_metadata {
        let after = fs::metadata(entry).unwrap();
        assert_eq!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec()),
            "repair must leave pre-fill entries untouched"
        );
    }
    // The running reader job owns its workspace, so hand it the key as the job
    // account rather than as the operator.
    checked(
        Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "-u",
                USER,
                "/bin/sh",
                "-c",
                "printf %s \"$1\" > \"$2\"",
                "handoff",
            ])
            .arg(new_entries[0].strip_prefix(&fleet).unwrap())
            .arg(f.workspace.join("cache-key")),
    )
    .unwrap();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let status = launch.wait().unwrap();
    launch.reap();
    assert!(status.success(), "reader: {status}, output={rest}");
    assert_eq!(rest, "readable\n");
    // The host DAC check is independent of the sandbox's read-only mount.
    // It catches an ACL that would let a job write through another mount.
    checked(Command::new("/usr/bin/sudo").args(["-n", "-u", USER, "/bin/bash", "-c", "test -r \"$2\" && ! test -w \"$1\" && ! test -w \"$2\" && ! touch \"$1/host-job-entry\" 2>/dev/null && ! (printf poison > \"$2\") 2>/dev/null", "reader"]).arg(&fleet).arg(&new_entries[0])).unwrap();
    let container = fleet.parent().unwrap();
    checked(
        Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "-u",
                USER,
                "/bin/sh",
                "-c",
                "test -r \"$1\" && ! test -w \"$1\"",
                "reader",
            ])
            .arg(container),
    )
    .unwrap();
    // Use the same repair and unprivileged rename as production eviction.
    let trash = config.state_dir().join("trash");
    fs::create_dir_all(&trash).unwrap();
    let retired = trash.join("fleet-eviction-test");
    retire_writer_cache(&fleet, &retired).unwrap();
    assert!(!fleet.exists());
    assert!(retired
        .join(new_entries[0].strip_prefix(&fleet).unwrap())
        .exists());
    prepare_writer_cache(&fleet, account).unwrap();
    // A freshly emptied cache must already hold sccache's preprocessor folder:
    // read-only sccache fails every C compile without it.
    let preprocessor = fleet.join("preprocessor");
    assert!(preprocessor.is_dir());
    assert_eq!(fs::metadata(&preprocessor).unwrap().uid(), account.uid);
    for directory in [
        container,
        fleet.as_path(),
        retired.as_path(),
        preprocessor.as_path(),
    ] {
        assert_job_cannot_write(directory);
    }
}

fn assert_job_cannot_write(path: &Path) {
    checked(
        Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "-u",
                USER,
                "/bin/sh",
                "-c",
                "! test -w \"$1\" && ! touch \"$1/host-job-entry\" 2>/dev/null",
                "reader",
            ])
            .arg(path),
    )
    .unwrap();
}

fn assert_job_cannot_read(path: &Path) {
    checked(
        Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "-u",
                USER,
                "/bin/bash",
                "-c",
                "! test -r \"$1\"",
                "reader",
            ])
            .arg(path),
    )
    .unwrap();
}

fn cache_entries(fleet: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    for first in "0123456789abcdef".chars() {
        for second in "0123456789abcdef".chars() {
            let directory = fleet.join(first.to_string()).join(second.to_string());
            if directory.is_dir() {
                for entry in fs::read_dir(directory).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_file() {
                        entries.push(entry.path());
                    }
                }
            }
        }
    }
    entries.sort();
    entries
}

struct CacheServer {
    executable: PathBuf,
    environment: Vec<(String, String)>,
    target: PathBuf,
    rustc: PathBuf,
    sandbox: Sandbox,
    uid: u32,
}

impl CacheServer {
    fn new(config: &crate::engine::Config, work: &Path, fill: Uuid) -> Self {
        let cargo = std::env::var("CARGO_HOME").unwrap();
        let sysroot = Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        assert!(sysroot.status.success());
        let rustc =
            PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim()).join("bin/rustc");
        let account = validate_writer_account().unwrap();
        let host_target = work.join("target");
        let mut sandbox = Sandbox::prepare(
            fill,
            &config.state_dir(),
            work,
            &host_target,
            &[],
            None,
            &config.state_dir().join("slots"),
            1,
        )
        .unwrap();
        // Direct-rustc fixtures have no Cargo.toml, so add the same fixed target
        // bind that production Rust workspaces receive during sandbox setup.
        fixed_compile_paths(&mut sandbox, work);
        sandbox.user = WRITER_USER;
        // Reuse production's toolchain/Cargo binds and ProtectHome layout. Only
        // private writable trees change owner; host tool ancestors need no ACLs.
        for path in [
            work.to_path_buf(),
            sandbox.runtime.0.join("home"),
            sandbox.runtime.0.join("tmp"),
        ] {
            private_writer_tree(&path, account);
        }
        for name in ["stdout", "stderr", "exit"] {
            private_writer_tree(&sandbox.runtime.0.join("output").join(name), account);
        }
        sandbox
            .read_write
            .push(bind(&config.sccache_dir(), Path::new("/motor-cache")).unwrap());
        let mut environment = std::collections::BTreeMap::new();
        crate::compile_cache::cargo_environment(&mut environment);
        environment.extend([
            ("SCCACHE_DIR".into(), "/motor-cache".into()),
            ("SCCACHE_CACHE_SIZE".into(), "1G".into()),
            ("SCCACHE_LOCAL_RW_MODE".into(), "READ_WRITE".into()),
            (
                "SCCACHE_SERVER_UDS".into(),
                config.writer_sccache_server_uds(fill),
            ),
        ]);
        cache_trace_environment(&mut environment, "writer");
        Self {
            executable: PathBuf::from(&cargo).join("bin/sccache"),
            environment: environment.into_iter().collect(),
            target: PathBuf::from(crate::compile_cache::TARGET),
            rustc,
            sandbox,
            uid: account.uid,
        }
    }
    // sccache 0.18.0 treats -o as non-cacheable. Cargo-style --emit/--out-dir
    // makes this linked rlib exercise a real cache put, not a plain compile.
    fn script() -> &'static str {
        r#"set -eu
 test "$(id -u)" = "$MOTOR_TEST_WRITER_UID"
 test "$USER" = motor-cache
 test "$LOGNAME" = motor-cache
 test "$CARGO_HOME" = /motor-home/cargo
 test "$("$MOTOR_TEST_SCCACHE" --version)" = "sccache 0.18.0"
 printf 'Writer cache environment: SCCACHE_DIR=%s SCCACHE_LOCAL_RW_MODE=%s SCCACHE_CACHE_SIZE=%s\n' "$SCCACHE_DIR" "$SCCACHE_LOCAL_RW_MODE" "$SCCACHE_CACHE_SIZE"
 stat -c 'Writer cache path: path=%n owner=%u:%g mode=%a device=%d inode=%i' -- "$SCCACHE_DIR"
 printf 'Writer cache mount (target/source/filesystem/options):\n'
 findmnt --target "$SCCACHE_DIR" --noheadings --output TARGET,SOURCE,FSTYPE,OPTIONS || { printf 'findmnt unavailable; sandbox mountinfo:\n'; cat /proc/self/mountinfo; }
 trap '"$MOTOR_TEST_SCCACHE" --stop-server' EXIT
  pwd
  env | LC_ALL=C sort | grep '^CARGO_' || true
  set -x
  "$MOTOR_TEST_SCCACHE" "$MOTOR_TEST_RUSTC" --crate-type=rlib --crate-name "$MOTOR_TEST_CRATE" --emit=link --out-dir "$MOTOR_TEST_OUT_DIR" "$MOTOR_TEST_SOURCE"
  set +x
 printf 'Writer sccache stats before shutdown:\n'
 "$MOTOR_TEST_SCCACHE" --show-stats
 "$MOTOR_TEST_SCCACHE" --stop-server
  if [ -n "${SCCACHE_ERROR_LOG:-}" ]; then cat "$SCCACHE_ERROR_LOG"; fi
  trap - EXIT"#
    }
    fn compile(&self, name: &str) -> String {
        let mut env = self.environment.clone();
        env.extend([
            ("MOTOR_TEST_WRITER_UID".into(), self.uid.to_string()),
            (
                "MOTOR_TEST_SCCACHE".into(),
                self.executable.to_string_lossy().into_owned(),
            ),
            (
                "MOTOR_TEST_RUSTC".into(),
                self.rustc.to_string_lossy().into_owned(),
            ),
            ("MOTOR_TEST_CRATE".into(), name.into()),
            (
                "MOTOR_TEST_SOURCE".into(),
                format!("/motor-dependencies/{name}.rs"),
            ),
            (
                "MOTOR_TEST_OUT_DIR".into(),
                self.target.to_string_lossy().into_owned(),
            ),
        ]);
        // Compile and stop the server in one unit: abstract sockets cannot be
        // reached from another unit's private network namespace. Unit teardown
        // also kills the server if compilation or explicit shutdown fails.
        let mut launch = self
            .sandbox
            .launch(Self::script(), Path::new("/motor-dependencies"), &env)
            .unwrap();
        launch.release().unwrap();
        let mut stdout = String::new();
        let mut stderr = String::new();
        launch.stdout.read_to_string(&mut stdout).unwrap();
        launch.stderr.read_to_string(&mut stderr).unwrap();
        let status = launch.wait().unwrap();
        launch.reap();
        assert!(
            status.success(),
            "writer: {status}, stdout={stdout}, stderr={stderr}"
        );
        format!("writer stdout:\n{stdout}\nwriter stderr:\n{stderr}")
    }
}

fn private_writer_tree(path: &Path, account: WriterAccount) {
    checked(
        Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "/usr/bin/chown",
                "-R",
                "-P",
                &writer_cache_owner(account),
                "--",
            ])
            .arg(path),
    )
    .unwrap();
    checked(
        Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/setfacl", "-R", "-P", "-b", "-k", "--"])
            .arg(path),
    )
    .unwrap();
    checked(
        &mut crate::permissions::command(crate::permissions::Policy::Private, &[path]).unwrap(),
    )
    .unwrap();
}

#[test]
fn writer_fixture_command_uses_sandbox_identity_and_mounts() {
    let f = Fixture::new();
    let mut sandbox = Sandbox {
        id: Uuid::now_v7(),
        root: f.scratch.join("root"),
        home: PathBuf::from("/motor-home"),
        cargo: PathBuf::from("/motor-home/cargo"),
        rustup: PathBuf::from("/private-operator-home/.rustup"),
        path: "/shared-cargo/bin:/usr/bin:/bin".into(),
        read_only: vec![
            "/private-operator-home/.rustup".into(),
            "/shared-cargo/bin".into(),
            "/shared-cargo/registry:/motor-home/cargo/registry".into(),
            "/shared-cargo/git:/motor-home/cargo/git".into(),
        ],
        read_write: vec![
            "/writer-work".into(),
            "/writer-target".into(),
            "/writer-home:/motor-home".into(),
            "/writer-cache:/motor-cache".into(),
        ],
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
        pid_init: None,
        provider_unset_env: Vec::new(),
        parent_umask: 0o022,
        memory_floor_gib: None,
        user: WRITER_USER,
        writer: false,
    };
    let command = sandbox
        .command(CacheServer::script(), &f.workspace, &[], false, &[], 0)
        .unwrap();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    for property in [
        "--property=User=motor-cache",
        "--property=Group=motor-cache",
        "--property=ProtectHome=tmpfs",
        "--property=PrivateNetwork=yes",
        "--property=ProtectSystem=strict",
        "--property=KillMode=control-group",
        "--setenv=USER=motor-cache",
        "--setenv=LOGNAME=motor-cache",
        "--setenv=CARGO_HOME=/motor-home/cargo",
    ] {
        assert!(args.contains(&property), "missing {property}");
    }
    assert!(args
        .contains(&format!("--property=RootDirectory={}", word(&sandbox.root).unwrap()).as_str()));
    assert!(args.contains(&"--property=BindReadOnlyPaths=/private-operator-home/.rustup /shared-cargo/bin /shared-cargo/registry:/motor-home/cargo/registry /shared-cargo/git:/motor-home/cargo/git"));
    assert!(args.contains(&"--property=BindPaths=/writer-work /writer-target /writer-home:/motor-home /writer-cache:/motor-cache"));
    let script = args.last().unwrap();
    let compile = script
        .lines()
        .position(|line| line.contains("--crate-type=rlib"))
        .unwrap();
    let stop = script
        .lines()
        .position(|line| line.trim() == "\"$MOTOR_TEST_SCCACHE\" --stop-server")
        .unwrap();
    assert!(compile < stop);
    assert!(script.contains("--emit=link --out-dir \"$MOTOR_TEST_OUT_DIR\""));
    assert!(!script.contains(" -o "));
    let stats = script
        .lines()
        .position(|line| line.trim() == "\"$MOTOR_TEST_SCCACHE\" --show-stats")
        .unwrap();
    assert!(compile < stats && stats < stop);
    assert!(script.contains("owner=%u:%g mode=%a"));
    assert!(script.contains("findmnt --target \"$SCCACHE_DIR\""));
    assert!(script.contains("trap '\"$MOTOR_TEST_SCCACHE\" --stop-server' EXIT"));
    sandbox.user = USER;
    let job = sandbox
        .command("true", &f.workspace, &[], false, &[], 0)
        .unwrap();
    let job_args: Vec<_> = job.get_args().map(|arg| arg.to_str().unwrap()).collect();
    for property in [
        "--property=User=motor-job",
        "--property=Group=motor-job",
        "--setenv=USER=motor-job",
        "--setenv=LOGNAME=motor-job",
    ] {
        assert!(job_args.contains(&property));
    }
}

fn fixed_compile_paths(sandbox: &mut Sandbox, work: &Path) {
    sandbox.read_write.push(
        bind(
            &work.join("target"),
            Path::new(crate::compile_cache::TARGET),
        )
        .unwrap(),
    );
    sandbox
        .read_only
        .push(bind(work, Path::new("/motor-dependencies")).unwrap());
    checked(
        Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/mkdir", "-p", "--"])
            .arg(sandbox.root.join("run/motor")),
    )
    .unwrap();
    secure_root_owned_mount_ancestors(&sandbox.root, Path::new(crate::compile_cache::TARGET))
        .unwrap();
}

fn cache_hashes(root: &Path) -> std::collections::BTreeMap<PathBuf, String> {
    use sha2::{Digest, Sha256};
    cache_entries(root)
        .into_iter()
        .map(|path| {
            let hash = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
            (path.strip_prefix(root).unwrap().to_path_buf(), hash)
        })
        .collect()
}

fn stat(output: &str, label: &str) -> u32 {
    output
        .lines()
        .find_map(|line| {
            line.strip_prefix(label)
                .and_then(|rest| rest.trim().parse().ok())
        })
        .unwrap_or_else(|| panic!("missing {label} in {output}"))
}

/// Requires sccache 0.18.0 patched to open entries without updating timestamps
/// in READ_ONLY mode. Stock 0.18.0 updates timestamps on every get: the sandbox's
/// read-only mount rejects that write with EROFS, so every lookup becomes a miss.
/// After a hit assertion fails, rerun with MOTOR_TEST_CACHE_TRACE=1 to print
/// server keys, lookup errors and the cache entry/shard permissions.
#[test]
#[ignore = "requires trusted operator, motor-cache, motor-job, patched sccache 0.18.0, sudo, ACLs and systemd"]
fn isolated_reader_hits_across_workspaces_and_cannot_poison_cache() {
    let f = Fixture::new();
    struct Reclaim(PathBuf);
    impl Drop for Reclaim {
        fn drop(&mut self) {
            reclaim_runtime(&self.0).expect("reclaim reader scratch");
        }
    }
    let _reclaim = Reclaim(f.scratch.clone());
    let account = validate_writer_account().unwrap();
    prepare_writer_shared_read_access().unwrap();
    let config = crate::engine::Config::new(&f.scratch, "runner-state");
    let fleet = config.sccache_dir();
    prepare_writer_cache(&fleet, account).unwrap();
    let work = f.scratch.join("writer");
    fs::create_dir_all(work.join("target")).unwrap();
    fs::write(work.join("dep.rs"), "pub fn value() -> u32 { 42 }\n").unwrap();
    let server = CacheServer::new(&config, &work, Uuid::now_v7());
    let fill = crate::writer::CacheFill::begin_at(&fleet, account).unwrap();
    let diagnostics = server.compile("dep");
    eprintln!("{diagnostics}");
    assert_eq!(stat(&diagnostics, "Cache misses"), 1, "{diagnostics}");
    fill.finish().unwrap();
    let before = cache_hashes(&fleet);
    assert!(!before.is_empty(), "writer must populate real entries");
    for name in ["reader-a", "reader-b"] {
        let workspace = f.scratch.join(name);
        fs::create_dir_all(workspace.join("target")).unwrap();
        fs::write(workspace.join("dep.rs"), "pub fn value() -> u32 { 42 }\n").unwrap();
        fs::write(
            workspace.join("miss.rs"),
            "pub fn uncached() -> u32 { 99 }\n",
        )
        .unwrap();
        let mut sandbox = Sandbox::prepare(
            Uuid::now_v7(),
            &f.state,
            &workspace,
            &workspace.join("target"),
            &[],
            None,
            &f.state.join("slots"),
            1,
        )
        .unwrap();
        fixed_compile_paths(&mut sandbox, &workspace);
        sandbox.read_only.push(format!(
            "-{}",
            bind(&fleet, Path::new(crate::compile_cache::CACHE)).unwrap()
        ));
        let mut env = std::collections::BTreeMap::new();
        crate::compile_cache::reader_environment(&mut env, 1);
        cache_trace_environment(&mut env, "reader");
        env.insert("RUSTC_WRAPPER".into(), "sccache".into());
        env.insert(
            "SCCACHE_SERVER_UDS".into(),
            config.writer_sccache_server_uds(Uuid::now_v7()),
        );
        env.insert(
            "MOTOR_TEST_SCCACHE".into(),
            server.executable.to_string_lossy().into_owned(),
        );
        env.insert(
            "MOTOR_TEST_RUSTC".into(),
            server.rustc.to_string_lossy().into_owned(),
        );
        let command = r#"set -e
trap '"$MOTOR_TEST_SCCACHE" --stop-server >/dev/null' EXIT
test "$CARGO_TARGET_DIR" = /run/motor/target
test ./target -ef /run/motor/target
if [ -n "${SCCACHE_ERROR_LOG:-}" ]; then
    find /motor-cache -type f -printf '%m %u:%g %s %p\n'
    getfacl -R /motor-cache
fi
if touch /motor-cache/job-entry 2>/dev/null; then exit 40; fi
env | LC_ALL=C sort | grep '^CARGO_' || true
set -x
(cd /motor-dependencies; pwd; "$MOTOR_TEST_SCCACHE" "$MOTOR_TEST_RUSTC" --crate-type=rlib --crate-name dep --emit=link --out-dir "$CARGO_TARGET_DIR" /motor-dependencies/dep.rs)
set +x
"$MOTOR_TEST_SCCACHE" --show-stats
if [ -n "${SCCACHE_ERROR_LOG:-}" ]; then cat "$SCCACHE_ERROR_LOG"; fi
(cd /motor-dependencies; "$MOTOR_TEST_SCCACHE" "$MOTOR_TEST_RUSTC" --crate-type=rlib --crate-name miss --emit=link --out-dir "$CARGO_TARGET_DIR" /motor-dependencies/miss.rs)
# A workspace-controlled build script attempts to overwrite every visible entry.
cat > build.rs <<'RS'
fn attack(p: &std::path::Path) {
    for entry in std::fs::read_dir(p).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() { attack(&p); }
        else { assert!(std::fs::write(&p, b"poison").is_err()); }
    }
}
fn main() { attack(std::path::Path::new("/motor-cache")); assert!(std::fs::write("/motor-cache/build-script-entry", b"poison").is_err()); }
RS
cat > Cargo.toml <<'TOML'
[package]
name = "hostile-build-script"
version = "0.1.0"
edition = "2021"
[[bin]]
name = "hostile-build-script"
path = "main.rs"
TOML
printf 'fn main() {}\n' > main.rs
cargo build --offline
test -f ./target/libdep.rlib
test -f ./target/libmiss.rlib
"#;
        let mut launch = sandbox
            .launch(command, &workspace, &env.into_iter().collect::<Vec<_>>())
            .unwrap();
        launch.release().unwrap();
        let mut stdout = String::new();
        let mut stderr = String::new();
        launch.stdout.read_to_string(&mut stdout).unwrap();
        launch.stderr.read_to_string(&mut stderr).unwrap();
        let status = launch.wait().unwrap();
        launch.reap();
        eprintln!("{name}: {status}\n{stdout}\n{stderr}");
        assert!(status.success(), "{name}: {stdout}\n{stderr}");
        assert_eq!(
            stat(&stdout, "Cache hits"),
            1,
            "{stdout}\nRerun with MOTOR_TEST_CACHE_TRACE=1 for key/lookup diagnostics"
        );
        assert_eq!(stat(&stdout, "Cache misses"), 0, "{stdout}");
        assert_eq!(
            cache_hashes(&fleet),
            before,
            "reader/build script altered cache"
        );
    }
}

fn cache_trace_environment(env: &mut std::collections::BTreeMap<String, String>, role: &str) {
    if std::env::var_os("MOTOR_TEST_CACHE_TRACE").is_some() {
        env.insert("SCCACHE_LOG".into(), "sccache=trace".into());
        env.insert(
            "SCCACHE_ERROR_LOG".into(),
            format!("/run/motor/target/sccache-{role}.log"),
        );
    }
}
