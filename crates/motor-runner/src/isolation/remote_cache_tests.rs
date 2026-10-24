//! The remote cache transport in job and fill units: which sockets each unit
//! gets, how its fixed shell starts the forwarders, and what a unit can do to
//! its own forwarder.
//!
//! Most tests here run without sudo or the job accounts. They build units
//! with the real preparation code (privileged commands answered by
//! [`fake_privileged_output`]) or take the shell a unit command renders, and
//! run that shell directly on this host against a real gateway, with
//! `/motor-output`, the forwarder and the socket directory pointed at their
//! host paths. On this host every unit shares one network namespace, so each
//! unit gets its own forwarder port; in a real unit each has a private
//! loopback. The tests marked as needing the isolation gate run real units.
use super::*;
use crate::remote_cache::gateway::{Endpoint, Tool};
use crate::remote_cache::transport::tests::{
    free_port, s3_path, stored_sccache, FakeCache, ProcessGroup,
};
use crate::remote_cache::transport::{Forwarder, Role, Transport};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

/// A unit shell running on this host, in its own process group.
struct Running {
    child: std::process::Child,
    _group: ProcessGroup,
}

impl Running {
    /// Waits for the shell; returns its exit code and standard output. The
    /// process group, and any forwarder still serving in it, is killed when
    /// `self` is dropped.
    fn finish(mut self) -> (Option<i32>, String) {
        let mut stdout = String::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        (self.child.wait().unwrap().code(), stdout)
    }
}

/// Starts the shell `sandbox` renders for `command` (without the durable
/// gate's files) on this host. `forwarder` is the forwarder binary to run and
/// `sockets` the unit's socket directory, which becomes the shell's working
/// directory so the forwarder can name its socket by a short relative path.
fn start_unit_shell(
    sandbox: &Sandbox,
    command: &str,
    forwarder: &Path,
    sockets: &Path,
    workspace: &Path,
) -> Running {
    use std::os::unix::process::CommandExt;
    let rendered = sandbox
        .command(command, workspace, &[], false, &[], 0)
        .unwrap();
    let args: Vec<_> = rendered
        .get_args()
        .map(|arg| arg.to_str().unwrap().to_owned())
        .collect();
    assert_eq!(args.last().unwrap(), command);
    let shell = args[args.len() - 3]
        .replace("/motor-tools/cache-forwarder", forwarder.to_str().unwrap())
        .replace("/motor-gateway/", "")
        // The writer's Cargo preflight is not what these tests check.
        .replace("/bin/bash /motor-tools/writer-cargo-check.sh", "true");
    let mut child = Command::new("/bin/bash")
        .args(["-c", &shell, USER, command])
        .current_dir(sockets)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let group = ProcessGroup(child.id() as i32);
    // Release the gate.
    child.stdin.take().unwrap().write_all(b"x").unwrap();
    Running {
        child,
        _group: group,
    }
}

/// Shell source that sends one request through the forwarder on `port` and
/// prints the whole answer.
fn request(port: u16, method: &str, path: &str, body: &str) -> String {
    let length = if method == "PUT" {
        format!("content-length: {}\\r\\n", body.len())
    } else {
        String::new()
    };
    format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port} || exit 41; printf '{method} {path} HTTP/1.1\\r\\nhost: x\\r\\nconnection: close\\r\\n{length}\\r\\n{body}' >&3; cat <&3; exec 3>&-; echo; "
    )
}

/// Shell source that kills every forwarder in the unit's own process group
/// and prints `killed` for each.
const KILL_OWN_FORWARDER: &str = "read -r _ _ _ _ group _ </proc/self/stat; for p in /proc/[0-9]*; do read -r _ _ _ _ g _ <\"$p/stat\" 2>/dev/null || continue; test \"$g\" = \"$group\" || continue; read -r c <\"$p/comm\" 2>/dev/null || continue; if test \"$c\" = cache-forwarder; then kill -9 \"${p##*/}\" && echo killed; fi; done; ";

/// Shell source that prints how many forwarders run in the unit's own
/// process group.
const COUNT_OWN_FORWARDERS: &str = "read -r _ _ _ _ group _ </proc/self/stat; n=0; for p in /proc/[0-9]*; do read -r _ _ _ _ g _ <\"$p/stat\" 2>/dev/null || continue; test \"$g\" = \"$group\" || continue; read -r c <\"$p/comm\" 2>/dev/null || continue; if test \"$c\" = cache-forwarder; then n=$((n+1)); fi; done; echo forwarders=$n; ";

fn with_forwarders(mut sandbox: Sandbox, forwarders: &[Forwarder]) -> Sandbox {
    sandbox.cache_forwarders = forwarders.to_vec();
    sandbox
}

fn rendered_argv(
    sandbox: &Sandbox,
    f: &Fixture,
    durable: bool,
    env: &[(String, String)],
) -> String {
    let command = sandbox
        .command("cargo test", &f.workspace, env, durable, &[1, 2], 2)
        .unwrap();
    let scratch = f.scratch.to_str().unwrap();
    let id = sandbox.id.to_string();
    let mut text = format!("{}\n", command.get_program().to_str().unwrap());
    for arg in command.get_args() {
        let arg = arg
            .to_str()
            .unwrap()
            .replace(scratch, "<SCRATCH>")
            .replace(&id, "<ID>");
        text.push_str(&arg);
        text.push('\n');
    }
    text
}

/// SHA-256 of the rendered argv of every remote case below.
///
/// Updated when the writer shell began asking an installed script whether the
/// tree it is about to compile has a root `Cargo.toml`, so a declared fill
/// without one is not skipped; only the `writer` cases changed.
const REMOTE_PINNED_SHA256: &str =
    "c20b3f34052769f35470cd8a8fb6abf91ae9bfe48d65c2e8eebb8b17069f9991";

/// Every unit kind, rendered with and without forwarders: only forwarder starts
/// and active-tool statistics change. Forwarders start right after the gate; with no
/// forwarders nothing changes at all (the argv pinned in `command_golden`
/// covers the unchanged units byte for byte).
#[test]
fn forwarders_start_right_after_the_gate_and_change_nothing_else() {
    let f = Fixture::new();
    let bound = |mut sandbox: Sandbox| {
        sandbox.read_only = vec!["/usr".into(), "/etc/passwd".into()];
        sandbox.read_write = vec![f.workspace.to_str().unwrap().into()];
        sandbox
    };
    let job_tools = [Forwarder::new(Tool::Sccache, 4301)];
    let fill_tools = [
        Forwarder::new(Tool::Sccache, 4301),
        Forwarder::new(Tool::Ccache, 4302),
    ];
    let make = |kind: &str| {
        let mut sandbox = bound(f.command_sandbox());
        match kind {
            "writer" => {
                sandbox.writer = true;
                sandbox.user = WRITER_USER;
                sandbox
            }
            "tenant" => sandbox.in_tenant_vm(true),
            "boat-private-pids" => sandbox.with_boat_profile(true, true, Some(14 << 30)),
            _ => sandbox,
        }
    };
    let cases: [(&str, &[Forwarder]); 4] = [
        ("build-server", &job_tools),
        ("writer", &fill_tools),
        ("tenant", &job_tools),
        ("boat-private-pids", &job_tools),
    ];
    let cached = [("SCCACHE_DIR".to_string(), "/motor-cache".to_string())];
    let mut pinned = String::new();
    for (name, tools) in cases {
        let starts: String = tools.iter().map(Forwarder::start).collect();
        assert_eq!(
            starts,
            tools
                .iter()
                .map(|forwarder| format!(
                    "/motor-tools/cache-forwarder 127.0.0.1:{} /motor-gateway/{}.sock </dev/null >/dev/null 2>&1; ",
                    forwarder.port(),
                    forwarder.tool().id()
                ))
                .collect::<String>()
        );
        let remote = with_forwarders(make(name), tools);
        let mut plain = make(name);
        plain.id = remote.id;
        // sccache statistics are captured once its integration is on, which
        // also covers a job unit without a local cache directory. A fill's
        // shell already captures them, so only a job's shell gains that part.
        let fill = name == "writer";
        for durable in [true, false] {
            for env in [&[][..], &cached[..]] {
                let with = rendered_argv(&remote, &f, durable, env);
                let without = rendered_argv(&plain, &f, durable, env);
                assert!(
                    with.contains(&format!("\n{SHELL_GATE}{starts}")),
                    "{name}: {with}"
                );
                assert_eq!(with.matches(&starts).count(), 1, "{name}: {with}");
                let ccache_stats =
                    "ccache --print-stats >/motor-output/ccache-stats 2>/dev/null || true; ";
                // When the ccache forwarder is enabled, assert its added stats
                // capture exactly once, then remove only that capture so the
                // pinned digest and comparison with no-forwarder argv remain
                // unchanged.
                let normalized = if durable
                    && tools
                        .iter()
                        .any(|forwarder| forwarder.tool() == Tool::Ccache)
                {
                    assert_eq!(with.matches(ccache_stats).count(), 1, "{name}: {with}");
                    with.replacen(ccache_stats, "", 1)
                } else {
                    with.clone()
                };
                let stripped = normalized.replacen(&starts, "", 1);
                let stats_command = " sccache --show-stats --stats-format=json >/motor-output/cache-stats 2>/dev/null || true;";
                let expected = if durable && env.is_empty() && !fill {
                    assert_eq!(stripped.matches(stats_command).count(), 1, "{name}: {with}");
                    stripped.replacen(stats_command, "", 1)
                } else {
                    stripped
                };
                assert_eq!(expected, without, "{name}");
                pinned.push_str(&format!(
                    "== {name} durable={durable} cached={}\n",
                    !env.is_empty()
                ));
                pinned.push_str(&normalized);
            }
        }
    }
    let digest = format!("{:x}", sha2::Sha256::digest(pinned.as_bytes()));
    assert!(
        digest == REMOTE_PINNED_SHA256,
        "remote unit argv changed (sha256 {digest}); if intended, update REMOTE_PINNED_SHA256. Rendered argv:\n{pinned}"
    );
}

/// A Cargo workspace committed to Git, as sandbox preparation expects.
fn cargo_fixture(workspace: &Path) -> String {
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = 'transport-fixture'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(
        workspace.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"transport-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    workspace::git(workspace, &["init", "-q"]).unwrap();
    workspace::git(workspace, &["add", "."]).unwrap();
    workspace::git(workspace, &["write-tree"]).unwrap()
}

/// A job unit as a Boat or tenant VM prepares it (no shared compile cache,
/// no compiler supervisor), with or without a remote cache.
fn prepare_job(f: &Fixture, tree: &str, remote: Option<&Transport>) -> Sandbox {
    let target = f.scratch.join("job-target");
    fs::create_dir_all(&target).unwrap();
    Sandbox::prepare_role(
        Uuid::now_v7(),
        &f.state,
        &f.workspace,
        &target,
        &[],
        None,
        &f.state.join("slots"),
        1,
        &BTreeMap::new(),
        tree,
        f.workspace.to_str().unwrap(),
        false,
        false,
        false,
        None,
        &JavascriptTools::for_role(ToolchainRole::BuildServer),
        remote,
    )
    .unwrap_or_else(|error| panic!("prepare job sandbox: {error}"))
}

/// A fill unit in its own workspace, as production fills run in their own
/// `.motor-base-…` checkout: preparing a fill hands its workspace to the
/// writer account with private permissions, after which the runner can no
/// longer read it to prepare any other unit. Returns the fill and its
/// workspace.
fn prepare_fill(f: &Fixture, remote: Option<&Transport>) -> (Sandbox, PathBuf) {
    let workspace = f.scratch.join("fill-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let tree = cargo_fixture(&workspace);
    let target = f.scratch.join("fill-target");
    fs::create_dir_all(&target).unwrap();
    let fill = Sandbox::prepare_writer(
        Uuid::now_v7(),
        &f.state,
        &workspace,
        &target,
        None,
        &f.state.join("slots"),
        1,
        &BTreeMap::new(),
        &tree,
        &JavascriptTools::for_role(ToolchainRole::BuildServer),
        false,
        remote,
    )
    .unwrap_or_else(|error| panic!("prepare fill sandbox: {error}"));
    (fill, workspace)
}

/// A unit's mounts with its private runtime directory replaced by a
/// placeholder, so units prepared under different ids compare.
fn mounts(sandbox: &Sandbox) -> (Vec<String>, Vec<String>) {
    let runtime = word(&sandbox.runtime.0).unwrap();
    let neutral = |list: &Vec<String>| {
        list.iter()
            .map(|mount| mount.replace(&runtime, "<RUNTIME>"))
            .collect()
    };
    (neutral(&sandbox.read_only), neutral(&sandbox.read_write))
}

/// Whether the command line names `path` or anything under it.
fn names(arguments: &[String], path: &Path) -> bool {
    let path = path.to_str().unwrap();
    arguments.iter().any(|argument| argument.starts_with(path))
}

#[test]
fn ccache_outputs_are_preallocated_bound_and_granted_for_jobs_and_no_cargo_fills() {
    let f = Fixture::new();
    fs::write(
        f.workspace.join("unit.c"),
        "int answer(void) { return 42; }",
    )
    .unwrap();
    workspace::git(&f.workspace, &["init", "-q"]).unwrap();
    workspace::git(&f.workspace, &["add", "."]).unwrap();
    let tree = workspace::git(&f.workspace, &["write-tree"]).unwrap();
    let cache = FakeCache::new();
    let remote = cache.transport(&[(Tool::Ccache, free_port())]);
    let acls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let log = acls.clone();
    crate::systemd::with_process_hook(
        Box::new(move |command| {
            let args = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            log.lock().unwrap().push(args);
            fake_privileged_output(command).unwrap_or_else(|| command.output())
        }),
        || {
            for writer in [false, true] {
                let target = f.scratch.join(if writer {
                    "ccache-fill-target"
                } else {
                    "ccache-job-target"
                });
                fs::create_dir_all(&target).unwrap();
                let sandbox = Sandbox::prepare_role(
                    Uuid::now_v7(),
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
                    writer,
                    false,
                    false,
                    None,
                    &JavascriptTools::for_role(ToolchainRole::BuildServer),
                    Some(&remote),
                )
                .unwrap();
                let stats = sandbox.runtime.0.join("output/ccache-stats");
                assert!(fs::symlink_metadata(&stats).unwrap().is_file());
                assert_eq!(fs::metadata(&stats).unwrap().len(), 0);
                assert!(sandbox
                    .read_write
                    .contains(&bind(&stats, Path::new("/motor-output/ccache-stats")).unwrap()));
                assert!(acls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|args| args.iter().any(|arg| arg == stats.to_str().unwrap())));
                let command = sandbox
                    .command("exit 7", &f.workspace, &[], true, &[], 0)
                    .unwrap();
                let args = command
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                let shell = &args[args.len() - 3];
                assert!(shell.contains("ccache --print-stats >/motor-output/ccache-stats"));
                assert!(
                    shell.find("code=$?").unwrap() < shell.find("ccache --print-stats").unwrap()
                );
                if writer {
                    assert_eq!(
                        fs::read_to_string(sandbox.runtime.0.join("tools/writer-cargo-check.sh"))
                            .unwrap(),
                        crate::writer::fill::CARGO_CHECK
                    );
                    assert!(sandbox
                        .runtime
                        .0
                        .join("tools/writer-preflight.py")
                        .is_file());
                }
            }
        },
    );
    assert!(!cache_output_names(false).contains(&"ccache-stats"));
}

#[test]
fn ccache_stats_capture_preserves_the_build_exit_code_even_if_stats_fail() {
    let f = Fixture::new();
    let output = f.scratch.join("capture-output");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("ccache-stats"), "").unwrap();
    fs::write(output.join("exit"), [b' '; 32]).unwrap();
    let shell = with_ccache_statistics(
        "code=7; printf '%s\\n' \"$code\" 1<>/motor-output/exit; exit \"$code\"",
        true,
    )
    .replace("/motor-output", output.to_str().unwrap());
    let stub = "ccache() { printf 'remote_storage_hit 1\\nremote_storage_error 0\\nremote_storage_timeout 0\\n'; return 9; }; ";
    let status = Command::new("/bin/bash")
        .args(["-c", &format!("{stub}{shell}")])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));
    let stats =
        crate::remote_cache::ccache::statistics(&fs::read(output.join("ccache-stats")).unwrap())
            .unwrap();
    assert_eq!(stats[crate::remote_cache::ccache::REMOTE_HIT], 1);
    assert!(fs::read_to_string(output.join("exit"))
        .unwrap()
        .starts_with("7\n"));
}

#[test]
fn prepared_units_get_only_their_own_sockets_and_the_forwarder() {
    let f = Fixture::new();
    let tree = cargo_fixture(&f.workspace);
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"cached");
    let (job_port, fill_port, fill_ccache_port) = (free_port(), free_port(), free_port());
    let reads = cache.transport(&[(Tool::Sccache, job_port)]);
    let writes = cache.transport(&[(Tool::Sccache, fill_port), (Tool::Ccache, fill_ccache_port)]);
    let acls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let log = acls.clone();
    crate::systemd::with_process_hook(
        Box::new(move |command| {
            let arguments: Vec<String> = command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            let program = command.get_program().to_string_lossy().into_owned();
            if program.ends_with("setfacl")
                || (program.ends_with("sudo")
                    && arguments
                        .get(1)
                        .is_some_and(|name| name.ends_with("setfacl")))
            {
                log.lock().unwrap().push(arguments);
            }
            fake_privileged_output(command).unwrap_or_else(|| command.output())
        }),
        || {
            let plain = prepare_job(&f, &tree, None);
            let job = prepare_job(&f, &tree, Some(&reads));
            let (fill, fill_workspace) = prepare_fill(&f, Some(&writes));
            let job_sockets = job.runtime.0.join("gateway");
            let fill_sockets = fill.runtime.0.join("gateway");
            let job_tools = job.runtime.0.join("tools");

            // The job sees its own socket directory and the tool directory,
            // which carries the forwarder although this role has no shared
            // compile cache. Nothing else of its mounts changes.
            let gateway_mount = bind(&job_sockets, Path::new("/motor-gateway")).unwrap();
            let tools_mount = bind(&job_tools, Path::new("/motor-tools")).unwrap();
            assert!(job.read_only.contains(&gateway_mount));
            assert!(job.read_only.contains(&tools_mount));
            let (mut read_only, read_write) = mounts(&job);
            let neutral = |mount: &str| mount.replace(&word(&job.runtime.0).unwrap(), "<RUNTIME>");
            read_only.retain(|mount| {
                *mount != neutral(&gateway_mount) && *mount != neutral(&tools_mount)
            });
            assert_eq!((read_only, read_write), mounts(&plain));
            assert_eq!(job.path, plain.path);
            assert!(plain.cache_forwarders.is_empty());
            assert!(plain.runtime.2.get().is_none());
            assert!(!plain.runtime.0.join("gateway").exists());
            assert!(!plain.runtime.0.join("tools").exists());
            let forwarder = job_tools.join("cache-forwarder");
            assert_eq!(
                fs::metadata(&forwarder).unwrap().permissions().mode() & 0o777,
                0o755
            );
            assert_eq!(
                job.cache_forwarders,
                [Forwarder::new(Tool::Sccache, job_port)]
            );
            assert_eq!(
                fill.cache_forwarders,
                [
                    Forwarder::new(Tool::Sccache, fill_port),
                    Forwarder::new(Tool::Ccache, fill_ccache_port)
                ]
            );

            // A job's socket is a read socket; a fill's are write sockets.
            let endpoints = |sandbox: &Sandbox| {
                sandbox
                    .runtime
                    .2
                    .get()
                    .unwrap()
                    .listeners()
                    .iter()
                    .map(|listener| (listener.binding().tool(), listener.binding().endpoint()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(endpoints(&job), [(Tool::Sccache, Endpoint::Read)]);
            assert_eq!(
                endpoints(&fill),
                [
                    (Tool::Sccache, Endpoint::Write),
                    (Tool::Ccache, Endpoint::Write)
                ]
            );

            // Neither unit's mounts name the other's runtime directory.
            let all = |sandbox: &Sandbox| {
                sandbox
                    .read_only
                    .iter()
                    .chain(&sandbox.read_write)
                    .cloned()
                    .collect::<Vec<_>>()
            };
            let job_runtime = word(&job.runtime.0).unwrap();
            let fill_runtime = word(&fill.runtime.0).unwrap();
            assert!(all(&job).iter().all(|mount| !mount.contains(&fill_runtime)));
            assert!(all(&fill).iter().all(|mount| !mount.contains(&job_runtime)));
            assert!(all(&fill).contains(&bind(&fill_sockets, Path::new("/motor-gateway")).unwrap()));

            // Only the unit's own account may connect: one grant per unit,
            // naming exactly its sockets, and no grant to the job account
            // ever reaches a fill's socket directory.
            let acls = acls.lock().unwrap().clone();
            let job_grants: Vec<_> = acls.iter().filter(|acl| names(acl, &job_sockets)).collect();
            let fill_grants: Vec<_> = acls
                .iter()
                .filter(|acl| names(acl, &fill_sockets))
                .collect();
            assert_eq!(job_grants.len(), 1, "{acls:?}");
            assert_eq!(fill_grants.len(), 1, "{acls:?}");
            let tail = |acl: &Vec<String>| {
                let start = acl.iter().position(|argument| argument == "-P").unwrap();
                acl[start..].to_vec()
            };
            assert_eq!(
                tail(job_grants[0]),
                [
                    "-P".to_string(),
                    "-m".into(),
                    "u:motor-job:rw".into(),
                    "--".into(),
                    job_sockets.join("sccache.sock").to_str().unwrap().into(),
                ]
            );
            assert_eq!(
                tail(fill_grants[0]),
                [
                    "-P".to_string(),
                    "-m".into(),
                    "u:motor-cache:rw".into(),
                    "--".into(),
                    fill_sockets.join("sccache.sock").to_str().unwrap().into(),
                    fill_sockets.join("ccache.sock").to_str().unwrap().into(),
                ]
            );
            for acl in &acls {
                if acl.iter().any(|argument| argument.contains("motor-job")) {
                    assert!(!names(acl, &fill_sockets), "{acl:?}");
                }
            }

            // The job reads through its forwarder; its read socket refuses a
            // PUT, and its socket directory holds no write socket at all.
            let mut entries: Vec<_> = fs::read_dir(&job_sockets)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            entries.sort();
            assert_eq!(entries, ["sccache.sock"]);
            let script = format!(
                "{}{}exit 3",
                request(job_port, "GET", &s3_path("k"), ""),
                request(job_port, "PUT", &s3_path("job-write"), "nope"),
            );
            let (code, stdout) =
                start_unit_shell(&job, &script, &forwarder, &job_sockets, &f.workspace).finish();
            assert_eq!(code, Some(3), "{stdout}");
            assert!(stdout.starts_with("HTTP/1.1 200 OK\r\n"), "{stdout}");
            assert!(stdout.contains("\r\n\r\ncached"), "{stdout}");
            assert!(stdout.contains("HTTP/1.1 403 "), "{stdout}");

            // The fill PUTs through its forwarder and write socket.
            let script = format!(
                "{}exit 0",
                request(fill_port, "PUT", &s3_path("fill"), "hello")
            );
            let (code, stdout) = start_unit_shell(
                &fill,
                &script,
                &fill.runtime.0.join("tools/cache-forwarder"),
                &fill_sockets,
                &fill_workspace,
            )
            .finish();
            assert_eq!(code, Some(0), "{stdout}");
            assert!(stdout.starts_with("HTTP/1.1 200 OK\r\n"), "{stdout}");
            let puts: Vec<_> = cache
                .store
                .seen()
                .into_iter()
                .filter(|seen| seen.method == "PUT")
                .map(|seen| (seen.path, seen.body))
                .collect();
            assert_eq!(puts, [(stored_sccache("fill"), b"hello".to_vec())]);
            let ledger = fill.runtime.2.get().unwrap().listeners()[0]
                .binding()
                .ledger()
                .unwrap()
                .summary();
            assert_eq!((ledger.puts_stored, ledger.puts_failed), (1, 0));

            // Reclaiming a unit's runtime directory stops its sockets.
            let job_socket = job_sockets.join("sccache.sock");
            drop(job);
            assert!(!job_socket.exists());
        },
    );
}

#[test]
fn a_job_that_kills_its_forwarder_affects_only_itself() {
    let f = Fixture::new();
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"cached");
    let forwarder = prepare_cache_forwarder(&f.state).unwrap();
    let (port_a, port_b) = (free_port(), free_port());
    let sockets_a = cache
        .transport(&[(Tool::Sccache, port_a)])
        .open(&f.scratch.join("gateway-a"), Role::Job("job-a"))
        .unwrap();
    let sockets_b = cache
        .transport(&[(Tool::Sccache, port_b)])
        .open(&f.scratch.join("gateway-b"), Role::Job("job-b"))
        .unwrap();
    let job_a = with_forwarders(f.command_sandbox(), sockets_a.forwarders());
    let job_b = with_forwarders(f.command_sandbox(), sockets_b.forwarders());
    let ready = f.scratch.join("b-ready");
    let go = f.scratch.join("b-go");
    // Job B's forwarder is up before job A kills its own, and B reads only
    // after A has finished.
    let b = start_unit_shell(
        &job_b,
        &format!(
            "touch '{}'; for i in $(seq 600); do test -e '{}' && break; sleep 0.05; done; {}exit 7",
            ready.display(),
            go.display(),
            request(port_b, "GET", &s3_path("k"), "")
        ),
        &forwarder,
        &f.scratch.join("gateway-b"),
        &f.workspace,
    );
    let started = Instant::now();
    while !ready.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "job B never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let (code, stdout) = start_unit_shell(
        &job_a,
        &format!(
            "{KILL_OWN_FORWARDER}for i in $(seq 100); do if ! (exec 3<>/dev/tcp/127.0.0.1/{port_a}) 2>/dev/null; then echo uncached; exit 7; fi; sleep 0.05; done; echo forwarder-alive; exit 99"
        ),
        &forwarder,
        &f.scratch.join("gateway-a"),
        &f.workspace,
    )
    .finish();
    // A killed exactly its own forwarder, lost its cache, and kept the exit
    // status its command chose.
    assert_eq!(code, Some(7), "{stdout}");
    assert_eq!(stdout, "killed\nuncached\n");
    fs::write(&go, "").unwrap();
    let (code, stdout) = b.finish();
    assert_eq!(code, Some(7), "{stdout}");
    assert!(stdout.starts_with("HTTP/1.1 200 OK\r\n"), "{stdout}");
    assert!(stdout.contains("\r\n\r\ncached"), "{stdout}");
    assert_eq!(cache.gateway.stats(Tool::Sccache).hits, 1);
}

#[test]
fn a_job_whose_forwarder_port_is_taken_runs_uncached_with_its_exit_code() {
    let f = Fixture::new();
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"cached");
    let forwarder = prepare_cache_forwarder(&f.state).unwrap();
    // Something else of job A already holds its forwarder's port.
    let occupier = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port_a = occupier.local_addr().unwrap().port();
    let port_b = free_port();
    let sockets_a = cache
        .transport(&[(Tool::Sccache, port_a)])
        .open(&f.scratch.join("gateway-a"), Role::Job("job-a"))
        .unwrap();
    let sockets_b = cache
        .transport(&[(Tool::Sccache, port_b)])
        .open(&f.scratch.join("gateway-b"), Role::Job("job-b"))
        .unwrap();
    let job_a = with_forwarders(f.command_sandbox(), sockets_a.forwarders());
    let job_b = with_forwarders(f.command_sandbox(), sockets_b.forwarders());
    let (code, stdout) = start_unit_shell(
        &job_a,
        &format!("{COUNT_OWN_FORWARDERS}echo ran; exit 5"),
        &forwarder,
        &f.scratch.join("gateway-a"),
        &f.workspace,
    )
    .finish();
    assert_eq!(code, Some(5), "{stdout}");
    assert_eq!(stdout, "forwarders=0\nran\n");
    let (code, stdout) = start_unit_shell(
        &job_b,
        &format!("{}exit 5", request(port_b, "GET", &s3_path("k"), "")),
        &forwarder,
        &f.scratch.join("gateway-b"),
        &f.workspace,
    )
    .finish();
    assert_eq!(code, Some(5), "{stdout}");
    assert!(stdout.starts_with("HTTP/1.1 200 OK\r\n"), "{stdout}");
    drop(occupier);
}

/// A job's or fill's command that tries to connect to each Unix socket in
/// `paths` and prints `connected` or the errno for each. The process changes
/// into the socket's directory first, so a long host path cannot fail the
/// connect for its length alone.
fn connect_script(paths: &[&Path]) -> String {
    let paths: Vec<_> = paths
        .iter()
        .map(|path| format!("'{}'", path.display()))
        .collect();
    format!(
        "python3 - {} <<'EOF'\nimport os, socket, sys\nfor path in sys.argv[1:]:\n    try:\n        os.chdir(os.path.dirname(path))\n        s = socket.socket(socket.AF_UNIX)\n        s.connect(os.path.basename(path))\n        print('connected')\n    except OSError as error:\n        print('errno', error.errno)\nEOF\n",
        paths.join(" ")
    )
}

/// Gives the scratch tree back to the runner when a test ends, after fills
/// handed parts of it to the writer account, so the fixture can remove it.
struct ReclaimScratch(PathBuf);

impl Drop for ReclaimScratch {
    fn drop(&mut self) {
        reclaim_runtime(&self.0).expect("reclaim scratch handed to the writer account");
    }
}

/// What the runner sets up once before any real fill unit can start, as at
/// runner startup: the writer's shared compile cache (a fill binds it
/// read-write, and that mount is not optional) and the writer account's read
/// access to the shared toolchain.
fn prepare_fill_host(f: &Fixture) -> ReclaimScratch {
    let reclaim = ReclaimScratch(f.scratch.clone());
    let account =
        validate_writer_account().unwrap_or_else(|error| panic!("writer account: {error}"));
    prepare_writer_shared_read_access(&JavascriptTools::for_role(ToolchainRole::BuildServer))
        .unwrap_or_else(|error| panic!("writer read access to the toolchain: {error}"));
    prepare_writer_cache(&f.state.join("sccache/fleet"), account)
        .unwrap_or_else(|error| panic!("writer compile cache: {error}"));
    reclaim
}

/// Runs `script` in a real unit; every step names itself when it fails.
fn run_unit(
    sandbox: &Sandbox,
    script: &str,
    cwd: &Path,
) -> (std::process::ExitStatus, String, String) {
    let mut launch = sandbox
        .launch(script, cwd, &[])
        .unwrap_or_else(|error| panic!("launch unit: {error}"));
    launch
        .release()
        .unwrap_or_else(|error| panic!("release the unit's gate: {error}"));
    let mut stdout = String::new();
    let mut stderr = String::new();
    launch
        .stdout
        .read_to_string(&mut stdout)
        .unwrap_or_else(|error| panic!("read unit stdout: {error}"));
    launch
        .stderr
        .read_to_string(&mut stderr)
        .unwrap_or_else(|error| panic!("read unit stderr: {error}"));
    let status = launch
        .wait()
        .unwrap_or_else(|error| panic!("wait for unit: {error}"));
    launch.reap();
    (status, stdout, stderr)
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_job_cannot_connect_to_a_fills_write_endpoint() {
    let f = Fixture::new();
    let _reclaim = prepare_fill_host(&f);
    let tree = cargo_fixture(&f.workspace);
    let cache = FakeCache::new();
    cache.store.put_object(&stored_sccache("k"), b"cached");
    let port = free_port();
    let job = prepare_job(&f, &tree, Some(&cache.transport(&[(Tool::Sccache, port)])));
    let (fill, _) = prepare_fill(&f, Some(&cache.transport(&[(Tool::Sccache, port)])));
    let write_socket = fill.runtime.0.join("gateway/sccache.sock");
    assert!(write_socket.exists());
    let script = format!(
        "{}{}exit 0",
        connect_script(&[&write_socket, Path::new("/motor-gateway/sccache.sock")]),
        request(port, "GET", &s3_path("k"), "")
    );
    let (status, stdout, stderr) = run_unit(&job, &script, &f.workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    let mut lines = stdout.lines();
    // The fill's write socket does not exist inside the job's root at all:
    // connecting fails before any request could be refused.
    assert_eq!(lines.next(), Some("errno 2"), "{stdout}\n{stderr}");
    // The job's own read socket and forwarder work.
    assert_eq!(lines.next(), Some("connected"), "{stdout}\n{stderr}");
    assert!(stdout.contains("HTTP/1.1 200 OK\r\n"), "{stdout}\n{stderr}");
    assert!(stdout.contains("\r\n\r\ncached"), "{stdout}\n{stderr}");
    assert!(cache.store.seen().iter().all(|seen| seen.method != "PUT"));
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_fill_puts_through_its_write_socket() {
    let f = Fixture::new();
    let _reclaim = prepare_fill_host(&f);
    let cache = FakeCache::new();
    let port = free_port();
    let (fill, workspace) = prepare_fill(&f, Some(&cache.transport(&[(Tool::Sccache, port)])));
    let script = format!("{}exit 0", request(port, "PUT", &s3_path("fill"), "hello"));
    let (status, stdout, stderr) = run_unit(&fill, &script, &workspace);
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.starts_with("HTTP/1.1 200 OK\r\n"),
        "{stdout}\n{stderr}"
    );
    let puts: Vec<_> = cache
        .store
        .seen()
        .into_iter()
        .filter(|seen| seen.method == "PUT")
        .map(|seen| (seen.path, seen.body))
        .collect();
    assert_eq!(puts, [(stored_sccache("fill"), b"hello".to_vec())]);
    let ledger = fill.runtime.2.get().unwrap().listeners()[0]
        .binding()
        .ledger()
        .unwrap()
        .summary();
    assert_eq!((ledger.puts_stored, ledger.puts_failed), (1, 0));
}
