use super::*;
use cortexkit_test_support::ScratchDir;
use std::{
    fs as host,
    os::unix::fs::{symlink, PermissionsExt},
};

fn write(path: &Path, text: &str) {
    host::create_dir_all(path.parent().unwrap()).unwrap();
    host::write(path, text).unwrap();
}
fn git(path: &Path, args: &[&str]) -> String {
    crate::workspace::git(path, args).unwrap()
}
fn tool(path: PathBuf, text: &str) -> Tool {
    crate::test_helpers::write_test_executable(&path, text.as_bytes()).unwrap();
    Tool {
        sha256: format!("{:x}", Sha256::digest(host::read(&path).unwrap())),
        path,
    }
}
fn commit(path: &Path) -> (String, String) {
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "snapshot",
        ],
    );
    (
        git(path, &["rev-parse", "HEAD"]),
        git(path, &["rev-parse", "HEAD^{tree}"]),
    )
}
struct Fixture {
    scratch: ScratchDir,
    staging: Staging,
    source: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let scratch = ScratchDir::new("macos-prep");
        let tools = scratch.join("tools");
        host::create_dir_all(&tools).unwrap();
        let git_path = ["/usr/bin/git", "/bin/git"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .unwrap();
        let real_git = host::canonicalize(git_path).unwrap();
        // Execute the system Git, but hash-check the same direct binary used by prep.
        let git_tool = Tool {
            sha256: format!("{:x}", Sha256::digest(host::read(&real_git).unwrap())),
            path: real_git,
        };
        let cargo = tool(tools.join("cargo"), "#!/bin/sh\nexit 0\n");
        let rustc = tool(tools.join("rustc"), "#!/bin/sh\nexit 0\n");
        let bun = tool(tools.join("bun"), "#!/bin/sh\nexit 0\n");
        let mirrors = scratch.join("mirrors");
        host::create_dir_all(&mirrors).unwrap();
        let staging = Staging::new(
            scratch.join("state"),
            mirrors,
            Tools {
                cargo,
                rustc,
                bun,
                git: git_tool,
            },
        )
        .unwrap();
        let source = scratch.join("source");
        host::create_dir_all(&source).unwrap();
        git(&source, &["init"]);
        Self {
            scratch,
            staging,
            source,
        }
    }
    fn request(&self) -> Run {
        let (commit, tree) = commit(&self.source);
        let repository = "/Users/motor/project";
        let mirror = self.staging.trees.mirror(repository);
        git(
            &self.source,
            &[
                "clone",
                "--mirror",
                self.source.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        Run {
            return_paths: vec![],
            return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
            return_chunked: false,
            gate_job: false,
            network: None,
            platform: Some(motor_protocol::Platform::Macos),
            workspace_kind: None,
            job_id: uuid::Uuid::now_v7(),
            workspace_key: repository.into(),
            repository_root: repository.into(),
            cwd: repository.into(),
            command: "true".into(),
            env: BTreeMap::new(),
            weight_hint: None,
            timeout: None,
            queue_wait_limit_s: 300,
            snapshot_commit: commit.clone(),
            tree_hash: tree,
            integration_tip: commit.clone(),
            siblings: vec![],
            bundles: vec![],
            git_dependencies: vec![],
            base_commit: Some(commit),
        }
    }
}

#[test]
fn caller_paths_are_strict_and_never_touched() {
    let f = Fixture::new();
    let mut r = f.request();
    for path in [
        "Users/a/r",
        "/Users/a/../r",
        "/Users/./r",
        "/Users//r",
        "/Users/a/r/",
        "/tmp/r",
        "/Users/a/a b",
        "",
    ] {
        r.cwd = path.into();
        assert!(validate_paths(&r)
            .unwrap_err()
            .to_string()
            .contains("macos_path_invalid"));
    }
    r.cwd = r.workspace_key.clone();
    let caller = f.scratch.join("caller-must-not-exist");
    r.workspace_key = caller.to_string_lossy().into();
    r.repository_root = r.workspace_key.clone();
    r.cwd = r.workspace_key.clone();
    assert!(f.staging.prepare(&r, Cancellation::default()).is_err());
    assert!(!caller.exists());
    assert!(!f.staging.state.join("jobs").exists());
}

#[test]
fn mirrored_path_mapping_and_symlinked_ancestor_are_nofollow() {
    let f = Fixture::new();
    write(&f.source.join("hello"), "snapshot");
    let r = f.request();
    let outside = f.scratch.join("sentinel");
    host::create_dir_all(&outside).unwrap();
    write(&outside.join("marker"), "unchanged");
    let canonical = f.staging.state.join("canonical");
    host::create_dir_all(&canonical).unwrap();
    symlink(&outside, canonical.join("Users")).unwrap();
    assert!(f.staging.prepare(&r, Cancellation::default()).is_err());
    assert_eq!(
        host::read_to_string(outside.join("marker")).unwrap(),
        "unchanged"
    );
    assert_eq!(host::read_dir(&outside).unwrap().count(), 1);
    host::remove_file(canonical.join("Users")).unwrap();
    let mut r = r;
    r.job_id = uuid::Uuid::now_v7();
    let prepared = f.staging.prepare(&r, Cancellation::default()).unwrap();
    assert_eq!(
        prepared.workspace,
        prepared.tree.join("Users/motor/project")
    );
    assert_eq!(
        host::read_to_string(prepared.workspace.join("hello")).unwrap(),
        "snapshot"
    );
    assert!(!prepared
        .workspace
        .join(".git/objects/info/alternates")
        .exists());
    assert_eq!(
        git(&prepared.workspace, &["log", "-1", "--format=%H"]),
        r.snapshot_commit
    );
}

#[test]
fn preflight_rejects_sources_before_fetch_and_never_publishes() {
    for (file, text, source) in [
        ("Cargo.lock", "[[package]]\nname='bad'\nversion='1.0.0'\nsource='git+file:///state/canonical/private#abc'\n", "git+file:///state/canonical/private"),
        ("Cargo.lock", "[[package]]\nname='bad'\nversion='1.0.0'\nsource='registry+https://evil.invalid/index'\n", "registry+https://evil.invalid/index"),
        ("package.json", r#"{"dependencies":{"bad":"git+file:///state/canonical"}}"#, "git+file:///state/canonical"),
        ("package.json", r#"{"dependencies":{"bad":"file:/Users/private"}}"#, "/Users/private"),
        ("package.json", r#"{"dependencies":{"bad":"file:../../../../outside"}}"#, "outside"),
        ("Cargo.toml", "[dependencies]\nbad={path='/Users/private'}\n", "/Users/private"),
        ("bun.lock", r#"{"packages":{"bad":["bad@1.0.0","file:///state/canonical",{},"sha512-x"]}}"#, "file:///state/canonical"),
        ("package.json", r#"{"workspaces":["/state/canonical/*"]}"#, "/state/canonical/*"),
        ("package.json", r#"{"workspaces":["../../../../outside/*"]}"#, "outside/*"),
        ("Cargo.toml", "[workspace]\nmembers=['../../../../outside/*']\n", "outside/*"),
    ] {
        let mut f = Fixture::new(); let marker = f.scratch.join("fetch-ran");
        f.staging.tools.cargo = tool(f.scratch.join("tools/cargo"), &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
        f.staging.tools.bun = tool(f.scratch.join("tools/bun"), &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
        write(&f.source.join(file), text); let r = f.request();
        let error = f.staging.prepare(&r, Cancellation::default()).err().unwrap().to_string();
        assert!(error.contains(source), "{error}"); assert!(!marker.exists());
        let job = f.staging.state.join("jobs").join(r.job_id.to_string());
        assert!(!job.join("cargo-home").exists());
        assert!(!job.join("Users/motor/project/node_modules").exists());
    }
}

#[test]
fn prep_copies_only_allowlisted_inputs_and_disables_scripts() {
    let mut f = Fixture::new();
    let marker = f.scratch.join("marker");
    write(
        &f.source.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    );
    write(&f.source.join("Cargo.lock"), "version=4\n");
    write(
        &f.source.join("src/lib.rs"),
        "compile_error!(\"must not compile\");",
    );
    for name in [
        ".cargo/config.toml",
        ".cargo/config",
        "rust-toolchain.toml",
        "rust-toolchain",
        ".npmrc",
        "bunfig.toml",
    ] {
        write(&f.source.join(name), &format!("touch {}", marker.display()));
    }
    write(
        &f.source.join("package.json"),
        &format!(
            r#"{{"scripts":{{"install":"touch {}"}}}}"#,
            marker.display()
        ),
    );
    write(
        &f.source.join("bun.lock"),
        "{\"lockfileVersion\":1,\"workspaces\":{},\"packages\":{},}",
    );
    let cargo_script = r#"#!/bin/sh
set -eu
[ "$1" = fetch ] && [ "$2" = --locked ]
[ -n "$RUSTC" ] && [ -z "${RUSTC_WRAPPER}" ] && [ -z "${CARGO_BUILD_RUSTC_WRAPPER}" ]
[ -z "${MACOS_POISON:-}" ]
manifest="$4"; root="${manifest%/Cargo.toml}"
[ ! -e "$root/.cargo/config.toml" ] && [ ! -e "$root/.cargo/config" ]
[ ! -e "$root/rust-toolchain.toml" ] && [ ! -e "$root/rust-toolchain" ]
[ ! -e "$root/.npmrc" ] && [ ! -e "$root/bunfig.toml" ]
[ ! -s "$root/src/lib.rs" ]
mkdir -p "$CARGO_HOME/registry/cache"
printf fetched > "$CARGO_HOME/registry/cache/demo"
"#;
    f.staging.tools.cargo = tool(f.scratch.join("tools/cargo"), cargo_script);
    f.staging.tools.bun = tool(f.scratch.join("tools/bun"), "#!/bin/sh\nset -eu\n[ \"$*\" = 'install --frozen-lockfile --ignore-scripts' ]\nmkdir -p node_modules packages/a/node_modules\nprintf installed > node_modules/demo\nprintf nested > packages/a/node_modules/demo\n");
    let r = f.request();
    let prepared = f.staging.prepare(&r, Cancellation::default()).unwrap();
    assert!(!marker.exists());
    assert_eq!(
        host::read_to_string(prepared.cargo_home.join("registry/cache/demo")).unwrap(),
        "fetched"
    );
    assert_eq!(
        host::read_to_string(prepared.workspace.join("node_modules/demo")).unwrap(),
        "installed"
    );
    assert_eq!(
        host::read_to_string(prepared.workspace.join("packages/a/node_modules/demo")).unwrap(),
        "nested"
    );
    assert!(!f
        .staging
        .state
        .join("canonical/Users/motor/project/node_modules")
        .exists());
}

#[test]
fn escaping_install_link_refuses_all_publication() {
    for (target, installation) in [
        ("/outside", "node_modules"),
        ("../../../../../../outside", "node_modules"),
        ("/outside", "packages/a/node_modules"),
    ] {
        let mut f = Fixture::new();
        write(&f.source.join("package.json"), "{}");
        f.staging.tools.bun = tool(
            f.scratch.join("tools/bun"),
            &format!(
                "#!/bin/sh\nmkdir -p '{installation}'\nln -s '{target}' '{installation}/escape'\n"
            ),
        );
        let r = f.request();
        let error = f
            .staging
            .prepare(&r, Cancellation::default())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("node_modules/escape"), "{error}");
        let job = f.staging.state.join("jobs").join(r.job_id.to_string());
        assert!(!job.join("cargo-home").exists());
        assert!(!job.join("Users/motor/project/node_modules").exists());
        assert!(!job.join("Users/motor/project").join(installation).exists());
    }
}

#[test]
fn pinned_tools_are_checked_on_every_prep_and_fetch_errors_name_package() {
    let mut f = Fixture::new();
    write(&f.source.join("package.json"), "{}");
    f.staging.tools.bun = tool(
        f.scratch.join("tools/bun"),
        "#!/bin/sh\necho 'failed to fetch named-package' >&2\nexit 1\n",
    );
    let r = f.request();
    let error = f
        .staging
        .prepare(&r, Cancellation::default())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("named-package"), "{error}");
    host::set_permissions(
        &f.staging.tools.bun.path,
        host::Permissions::from_mode(0o700),
    )
    .unwrap();
    host::write(&f.staging.tools.bun.path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut r = r;
    r.job_id = uuid::Uuid::now_v7();
    assert!(f
        .staging
        .prepare(&r, Cancellation::default())
        .err()
        .unwrap()
        .to_string()
        .contains("hash mismatch"));
    assert!(!f
        .staging
        .state
        .join("jobs")
        .join(r.job_id.to_string())
        .exists());
}

#[test]
fn prep_timeout_and_cancel_kill_the_entire_process_group() {
    for cancelled in [false, true] {
        let mut f = Fixture::new();
        write(&f.source.join("package.json"), "{}");
        let pids = f.scratch.join("pids");
        f.staging.tools.bun = tool(
            f.scratch.join("tools/bun"),
            &format!(
                "#!/bin/sh\nsleep 1000 &\necho $$ $! > '{}'\nwait\n",
                pids.display()
            ),
        );
        let r = f.request();
        let cancellation = Cancellation::default();
        std::thread::scope(|scope| {
            let token = cancellation.clone();
            if cancelled {
                let pids = &pids;
                scope.spawn(move || {
                    let until = Instant::now() + Duration::from_secs(10);
                    while !pids.exists() {
                        assert!(Instant::now() < until);
                        std::thread::park_timeout(Duration::from_millis(10));
                    }
                    token.cancel();
                });
            }
            let error = f
                .staging
                .prepare_with_timeout(
                    &r,
                    cancellation,
                    if cancelled {
                        Duration::from_secs(10)
                    } else {
                        Duration::from_secs(2)
                    },
                )
                .err()
                .unwrap()
                .to_string();
            assert!(
                error.contains(if cancelled { "cancelled" } else { "timed out" }),
                "{error}"
            );
        });
        let pids = host::read_to_string(&pids).unwrap();
        for pid in pids
            .split_whitespace()
            .map(|s| s.parse::<libc::pid_t>().unwrap())
        {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                // A killed, reparented Linux child may remain as an inert zombie until init reaps it.
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                #[cfg(target_os = "linux")]
                let alive = alive
                    && !host::read_to_string(format!("/proc/{pid}/stat"))
                        .is_ok_and(|s| s.contains(") Z "));
                if !alive {
                    break;
                }
                assert!(Instant::now() < until, "prep process {pid} remains");
                std::thread::park_timeout(Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn concurrent_sibling_versions_survive_canonical_and_cache_eviction() {
    let mut f = Fixture::new();
    write(&f.source.join("hello"), "workspace");
    let mut one = f.request();
    let mut two = one.clone();
    two.job_id = uuid::Uuid::now_v7();
    one.workspace_key = "/Users/motor/w1".into();
    one.cwd = one.workspace_key.clone();
    two.workspace_key = "/Users/motor/w2".into();
    two.cwd = two.workspace_key.clone();
    let source = f.scratch.join("sibling");
    host::create_dir_all(&source).unwrap();
    git(&source, &["init"]);
    let mut snapshots = Vec::new();
    for version in ["c1", "c2"] {
        write(&source.join("content"), version);
        write(
            &source.join("Cargo.toml"),
            "[package]\nname='sibling'\nversion='0.1.0'\n",
        );
        write(
            &source.join("Cargo.lock"),
            &format!("# {version}\nversion=4\n"),
        );
        write(&source.join("package.json"), "{}");
        write(
            &source.join("bun.lock"),
            &format!("{{\"lockfileVersion\":1,\"version\":\"{version}\"}}"),
        );
        snapshots.push(commit(&source));
    }
    let logical = "/Users/motor/sibling";
    let mirror = f.staging.trees.mirror(logical);
    git(
        &source,
        &[
            "clone",
            "--mirror",
            source.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    );
    for (r, (commit, tree)) in [&mut one, &mut two].into_iter().zip(snapshots) {
        r.siblings.push(motor_protocol::SiblingSnapshot {
            canonical_path: logical.into(),
            commit,
            tree_hash: tree,
            base_commit: None,
        });
    }
    f.staging.tools.cargo = tool(f.scratch.join("tools/cargo"), "#!/bin/sh\nset -eu\nroot=\"${4%/Cargo.toml}\"\nmkdir -p \"$CARGO_HOME/registry/cache\"\ncat \"$root/Cargo.lock\" > \"$CARGO_HOME/registry/cache/version\"\n");
    f.staging.tools.bun = tool(
        f.scratch.join("tools/bun"),
        "#!/bin/sh\nset -eu\nmkdir node_modules\ncat bun.lock > node_modules/version\n",
    );
    let stop = AtomicBool::new(false);
    let (a, b) = std::thread::scope(|scope| {
        let eviction = scope.spawn(|| {
            while !stop.load(Ordering::Acquire) {
                f.staging
                    .evict_canonical(logical, Cancellation::default())
                    .unwrap();
                f.staging.evict_prep(Cancellation::default()).unwrap();
                std::thread::park_timeout(Duration::from_millis(2));
            }
        });
        let a = scope.spawn(|| f.staging.prepare(&one, Cancellation::default()).unwrap());
        let b = scope.spawn(|| f.staging.prepare(&two, Cancellation::default()).unwrap());
        let a = a.join().unwrap();
        let b = b.join().unwrap();
        stop.store(true, Ordering::Release);
        eviction.join().unwrap();
        (a, b)
    });
    for (job, expected) in [(&a, "c1"), (&b, "c2")] {
        let sibling = &job.siblings[0].1;
        assert_eq!(
            host::read_to_string(sibling.join("content")).unwrap(),
            expected
        );
        assert!(host::read_to_string(sibling.join("node_modules/version"))
            .unwrap()
            .contains(expected));
        assert!(
            host::read_to_string(job.cargo_home.join("registry/cache/version"))
                .unwrap()
                .contains(expected)
        );
        assert!(!sibling.join(".git/objects/info/alternates").exists());
    }
    // Guest writes, including Git execution hooks, never return to canonical or another job.
    let sentinel = f.scratch.join("outside-sentinel");
    write(&sentinel, "intact");
    write(
        &a.workspace.join(".git/config"),
        "[core]\nfsmonitor = /outside/marker\n",
    );
    symlink(&sentinel, a.workspace.join("target")).unwrap();
    f.staging.remove_job_trees(one.job_id).unwrap();
    assert_eq!(host::read_to_string(sentinel).unwrap(), "intact");
    assert_eq!(
        host::read_to_string(b.siblings[0].1.join("content")).unwrap(),
        "c2"
    );
}

fn real_tool(name: &str) -> Tool {
    let output = if matches!(name, "cargo" | "rustc") {
        Command::new("rustup")
            .args(["which", name])
            .output()
            .unwrap()
    } else {
        Command::new("/bin/sh")
            .args(["-c", &format!("command -v {name}")])
            .output()
            .unwrap()
    };
    assert!(
        output.status.success(),
        "required test tool {name} is unavailable"
    );
    let path = host::canonicalize(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    Tool {
        sha256: format!("{:x}", Sha256::digest(host::read(&path).unwrap())),
        path,
    }
}

#[test]
fn real_cargo_and_bun_ignore_hostile_config_and_lifecycle_scripts() {
    let mut f = Fixture::new();
    f.staging.tools.cargo = real_tool("cargo");
    f.staging.tools.rustc = real_tool("rustc");
    f.staging.tools.bun = real_tool("bun");
    let marker = f.scratch.join("hostile-marker");
    let provider = f.scratch.join("credential-provider");
    tool(
        provider.clone(),
        &format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    );
    write(&f.source.join(".cargo/config.toml"), &format!("[registry]\nglobal-credential-providers = ['{}']\n[registries.crates-io]\ncredential-provider = '{}'\n", provider.display(), provider.display()));
    write(
        &f.source.join("Cargo.toml"),
        "[package]\nname='safe-prep'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        &f.source.join("Cargo.lock"),
        "version=4\n[[package]]\nname='safe-prep'\nversion='0.1.0'\n",
    );
    write(
        &f.source.join("src/lib.rs"),
        "compile_error!(\"prep must not compile caller code\");",
    );
    write(
        &f.source.join("package.json"),
        &format!(
            r#"{{"name":"safe-prep","scripts":{{"preinstall":"touch {}","install":"touch {}","postinstall":"touch {}"}}}}"#,
            marker.display(),
            marker.display(),
            marker.display()
        ),
    );
    // Generate a real frozen lock using trusted inputs; no dependency or registry is needed.
    let output = Command::new(&f.staging.tools.bun.path)
        .args(["install", "--ignore-scripts"])
        .current_dir(&f.source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let r = f.request();
    f.staging.prepare(&r, Cancellation::default()).unwrap();
    assert!(!marker.exists());
}

#[test]
fn bundled_private_commit_with_unreachable_url_preps_offline_and_missing_commit_refuses() {
    let mut f = Fixture::new();
    f.staging.tools.cargo = real_tool("cargo");
    f.staging.tools.rustc = real_tool("rustc");
    let private = f.scratch.join("private");
    host::create_dir_all(&private).unwrap();
    git(&private, &["init"]);
    write(
        &private.join("Cargo.toml"),
        "[package]\nname='private-pkg'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(&private.join("src/lib.rs"), "pub fn dependency() {}\n");
    let (oid, _) = commit(&private);
    let url = "https://127.0.0.1:1/never-contact/private";
    write(&f.source.join("Cargo.toml"), &format!("[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nprivate-pkg={{git='{url}',rev='{oid}'}}\n"));
    write(
        &f.source.join("src/lib.rs"),
        "pub fn consumer() { private_pkg::dependency(); }\n",
    );
    write(&f.source.join("Cargo.lock"), &format!("version=4\n[[package]]\nname='consumer'\nversion='0.1.0'\ndependencies=['private-pkg']\n[[package]]\nname='private-pkg'\nversion='0.1.0'\nsource='git+{url}?rev={oid}#{oid}'\n"));
    let mut r = f.request();
    let dependency = motor_protocol::GitDependency {
        url: url.into(),
        commits: vec![oid.clone()],
        bundle: None,
    };
    r.git_dependencies.push(dependency.clone());
    let error = f
        .staging
        .prepare(&r, Cancellation::default())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(url) && error.contains(&oid), "{error}");
    assert!(!f.staging.state.join("prep-cargo").exists());
    let reference = format!("refs/ck-motor/dep/{oid}");
    git(&private, &["update-ref", &reference, &oid]);
    let upload = f.scratch.join("upload");
    host::create_dir_all(&upload).unwrap();
    let bundle = upload.join("0");
    git(
        &private,
        &["bundle", "create", bundle.to_str().unwrap(), &reference],
    );
    let bytes = host::read(&bundle).unwrap();
    let mut dependency = dependency;
    dependency.bundle = Some(motor_protocol::BundleManifest {
        index: 0,
        repository: url.into(),
        length: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    });
    f.staging
        .trees
        .validate_dependency_bundles(&upload, &[dependency.clone()])
        .unwrap();
    f.staging
        .trees
        .import_dependencies(&upload, &[dependency])
        .unwrap();
    r.job_id = uuid::Uuid::now_v7();
    let prepared = f.staging.prepare(&r, Cancellation::default()).unwrap();
    assert!(prepared.cargo_home.join("git/checkouts").is_dir());
    // The resulting job cache is sufficient even after both host-only caches disappear.
    f.staging.evict_prep(Cancellation::default()).unwrap();
    let mut cmd = Command::new(&f.staging.tools.cargo.path);
    cmd.env("CARGO_HOME", &prepared.cargo_home)
        .env("RUSTC", &f.staging.tools.rustc.path)
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .args(["check", "--locked", "--offline"])
        .current_dir(&prepared.workspace);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn relative_install_links_and_symlinked_package_ancestors_stay_inside_job_tree() {
    let f = Fixture::new();
    let root = f.scratch.join("mirrored/Users/motor/main");
    let sibling = f.scratch.join("mirrored/Users/motor/sibling");
    write(&sibling.join("bin/run"), "binary");
    host::create_dir_all(root.join("node_modules/.bin")).unwrap();
    symlink("../../sibling", root.join("node_modules/sibling")).unwrap();
    symlink("../sibling/bin/run", root.join("node_modules/.bin/run")).unwrap();
    let roots = vec![root.clone(), sibling.clone()];
    preflight::validate_links(
        &root,
        &roots,
        &Control {
            cancel: Cancellation::default(),
            deadline: Instant::now() + PREP_TIMEOUT,
        },
    )
    .unwrap();
    host::remove_file(root.join("node_modules/sibling")).unwrap();
    symlink("/outside", root.join("node_modules/sibling")).unwrap();
    assert!(preflight::validate_links(
        &root,
        &roots,
        &Control {
            cancel: Cancellation::default(),
            deadline: Instant::now() + PREP_TIMEOUT
        }
    )
    .unwrap_err()
    .to_string()
    .contains("node_modules"));
}

#[test]
fn private_source_cannot_hide_behind_an_at_sign_in_bun_lock() {
    let f = Fixture::new();
    write(&f.source.join("package.json"), "{}");
    write(
        &f.source.join("bun.lock"),
        r#"{"packages":{"bad":["bad@git+file:///outside@1.0.0",{},""]}}"#,
    );
    let r = f.request();
    let error = f
        .staging
        .prepare(&r, Cancellation::default())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("git+file:///outside@1.0.0"), "{error}");
    assert!(!f.staging.state.join("prep-bun").exists());
}

#[test]
fn cancelled_prep_never_publishes_and_mirror_lock_wait_is_interruptible() {
    let f = Fixture::new();
    write(
        &f.source.join("Cargo.toml"),
        "[package]\nname='locked'\nversion='0.1.0'\n",
    );
    let url = "https://private.invalid/repo";
    let oid = "a".repeat(40);
    write(
        &f.source.join("Cargo.lock"),
        &format!("[[package]]\nname='private'\nversion='0.1.0'\nsource='git+{url}#{oid}'\n"),
    );
    let mut request = f.request();
    request
        .git_dependencies
        .push(motor_protocol::GitDependency {
            url: url.into(),
            commits: vec![oid],
            bundle: None,
        });
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(f
        .staging
        .prepare(&request, cancelled)
        .err()
        .unwrap()
        .to_string()
        .contains("cancelled"));
    assert!(!f.staging.state.join("jobs").exists());
    let lock = f
        .staging
        .trees
        .dependency_lock(&f.staging.trees.dependency_mirror(url));
    let _held = lock.lock().unwrap();
    let cancel = Cancellation::default();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| f.staging.prepare(&request, cancel.clone()));
        let copied = f
            .staging
            .state
            .join("prep")
            .join(request.job_id.to_string())
            .join("Users/motor/project/Cargo.lock");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !copied.exists() {
            assert!(Instant::now() < deadline);
            std::thread::park_timeout(Duration::from_millis(10));
        }
        let start = Instant::now();
        cancel.cancel();
        let error = worker.join().unwrap().err().unwrap().to_string();
        assert!(error.contains("cancelled"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2));
    });
    assert!(!f.staging.state.join("prep-cargo").exists());
}

#[test]
fn binary_bun_lock_refusal_is_actionable_before_fetch() {
    let mut f = Fixture::new();
    let marker = f.scratch.join("must-not-fetch");
    f.staging.tools.bun = tool(
        f.scratch.join("tools/bun"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    write(&f.source.join("package.json"), "{}");
    write(
        &f.source.join("bun.lockb"),
        "untrusted binary lockfile input",
    );
    let request = f.request();
    let error = f
        .staging
        .prepare(&request, Cancellation::default())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("/Users/motor/project/bun.lockb"), "{error}");
    assert!(
        error.contains("bun install --save-text-lockfile"),
        "{error}"
    );
    assert!(error.contains("commit bun.lock"), "{error}");
    assert!(!marker.exists());
    let job = f
        .staging
        .state
        .join("jobs")
        .join(request.job_id.to_string());
    assert!(!job.join("cargo-home").exists());
    assert!(!job.join("Users/motor/project/node_modules").exists());
}

#[test]
fn disk_eviction_uses_canonical_workspace_and_prep_locks() {
    let f = Fixture::new();
    let request = f.request();
    let prepared = f
        .staging
        .prepare(&request, Cancellation::default())
        .unwrap();
    let mut config = crate::engine::Config::new(f.staging.state.clone(), "macos");
    config.free_space_probe = Arc::new(|_| Ok(30 << 30));
    config.macos = Some(Arc::new(VmConfig {
        image: "/image".into(),
        helper: "/helper".into(),
        tools: f.staging.tools.clone(),
        toolchain_version: "sealed".into(),
    }));
    let control = Control {
        cancel: Cancellation::default(),
        deadline: Instant::now() + PREP_TIMEOUT,
    };
    let path_lock = control
        .lock(&f.staging.path_lock(&request.workspace_key))
        .unwrap();
    let prep_lock = control.lock(&config.state_root.join("prep.lock")).unwrap();
    let entry = config
        .macos_runtime
        .begin(&config.state_root, request.job_id, &request.workspace_key)
        .unwrap();
    config
        .macos_runtime
        .workspace(&config.state_root, &entry)
        .unwrap();
    let canonical = config
        .state_root
        .join("canonical")
        .join(mapped(&request.workspace_key).unwrap());
    let store = config
        .state_root
        .join("targets")
        .join(lifecycle::key(&request.workspace_key));
    host::create_dir_all(&store).unwrap();
    host::write(store.join("cached"), "whole").unwrap();
    assert!(evict(&config, f.staging.trees.mirrors.clone())
        .unwrap()
        .is_none());
    assert!(canonical.exists());
    assert!(store.exists());
    assert!(config.state_root.join("prep-cargo").exists());
    // Other tests fork children concurrently; explicitly unlock so inherited
    // CLOEXEC descriptors cannot extend this fixture's deliberate lock hold.
    fs2::FileExt::unlock(&path_lock).unwrap();
    fs2::FileExt::unlock(&prep_lock).unwrap();
    drop(path_lock);
    drop(prep_lock);
    let workspace_lock = entry.owned.lock().unwrap().workspace_lock.take().unwrap();
    fs2::FileExt::unlock(&workspace_lock).unwrap();
    assert!(canonical.join(".git").is_dir());
    evict(&config, f.staging.trees.mirrors.clone()).unwrap();
    assert!(!canonical.exists());
    assert!(!store.exists());
    assert!(!config.state_root.join("prep-cargo").exists());
    assert!(prepared.workspace.exists());
    config.free_space_probe = Arc::new(|_| Ok(19 << 30));
    assert!(evict(&config, f.staging.trees.mirrors.clone())
        .unwrap()
        .is_some());
}
#[test]
fn cross_job_poisoning_never_runs_guest_git_hooks_or_follows_guest_links() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let mut request = f.request();
    let config = crate::engine::Config::new(f.staging.state.clone(), "macos");
    let runtime = Runtime::default();
    let entry = runtime
        .begin(&config.state_root, request.job_id, &request.workspace_key)
        .unwrap();
    let first = f
        .staging
        .prepare(&request, Cancellation::default())
        .unwrap();
    let marker = f.scratch.join("marker");
    let sentinel = f.scratch.join("sentinel");
    host::create_dir(&sentinel).unwrap();
    host::write(sentinel.join("whole"), "unchanged").unwrap();
    let script = format!("#!/bin/sh\nprintf poisoned > '{}'\n", marker.display());
    let hook = first.workspace.join(".git/hooks/post-checkout");
    host::create_dir_all(hook.parent().unwrap()).unwrap();
    tool(hook, &script);
    host::write(
        first.workspace.join(".git/config"),
        format!(
            "[core]\nfsmonitor = sh -c 'printf poisoned > {}'\n",
            marker.display()
        ),
    )
    .unwrap();
    host::create_dir(first.workspace.join("node_modules")).unwrap();
    symlink(&config.state_root, first.workspace.join("node_modules/x")).unwrap();
    symlink(&sentinel, first.workspace.join("target")).unwrap();
    assert!(execution::target_out(&config, &entry, &first.workspace).is_err());
    entry.owned.lock().unwrap().record.stop_proven = true;
    runtime.finish(&config.state_root, &entry);
    request.job_id = uuid::Uuid::now_v7();
    let entry = runtime
        .begin(&config.state_root, request.job_id, &request.workspace_key)
        .unwrap();
    let second = f
        .staging
        .prepare(&request, Cancellation::default())
        .unwrap();
    execution::target_in(&config, &entry, &second).unwrap();
    host::create_dir(second.workspace.join("target")).unwrap();
    host::write(second.workspace.join("target/whole"), "new job").unwrap();
    execution::target_out(&config, &entry, &second.workspace).unwrap();
    entry.owned.lock().unwrap().record.stop_proven = true;
    runtime.finish(&config.state_root, &entry);
    assert!(!marker.exists());
    assert_eq!(
        host::read_to_string(sentinel.join("whole")).unwrap(),
        "unchanged"
    );
    assert!(!second.workspace.exists());
    assert!(!first.workspace.exists());
}

#[test]
fn apfs_case_aliases_share_locks_and_cannot_overlap() {
    let f = Fixture::new();
    let mut request = f.request();
    assert_eq!(
        f.staging.path_lock("/Users/Motor/Project"),
        f.staging.path_lock("/Users/motor/project")
    );
    assert_eq!(
        lifecycle::key("/Users/Motor/Project"),
        lifecycle::key("/Users/motor/project")
    );
    request.siblings.push(motor_protocol::SiblingSnapshot {
        canonical_path: "/Users/MOTOR/PROJECT/sub".into(),
        commit: request.snapshot_commit.clone(),
        base_commit: None,
        tree_hash: String::new(),
    });
    assert!(validate_paths(&request)
        .unwrap_err()
        .to_string()
        .contains("overlapping"));
}

#[test]
fn guest_copy_and_sync_reject_excessive_depth_before_stack_or_fd_exhaustion() {
    let scratch = ScratchDir::new("macos-copy-depth");
    let source = scratch.join("source");
    let destination = scratch.join("destination");
    let mut dir = Dir::open(&source, true).unwrap();
    for _ in 0..(fs::MAX_TREE_DEPTH + 16) {
        dir = dir.child("d".as_ref(), true).unwrap();
    }
    drop(dir);
    let source = Dir::open(&source, false).unwrap();
    let destination = Dir::open(&destination, true).unwrap();
    let control = Control {
        cancel: Cancellation::default(),
        deadline: Instant::now() + PREP_TIMEOUT,
    };
    assert!(source
        .clone_into(&destination, &control)
        .unwrap_err()
        .to_string()
        .contains("depth limit"));
    assert!(source
        .sync_all()
        .unwrap_err()
        .to_string()
        .contains("depth limit"));
    assert!(source
        .copy_inputs(&destination, &control)
        .unwrap_err()
        .to_string()
        .contains("depth limit"));
}
