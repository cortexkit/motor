use super::*;

fn write_local_package_fixture(workspace: &Path) {
    let package = workspace.join("vendor/fixture-dep");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"fixture-dep","version":"1.0.0","main":"index.js"}"#,
    )
    .unwrap();
    fs::write(package.join("index.js"), "module.exports = 'fixture';\n").unwrap();
}

fn write_lock_mismatch_probe(workspace: &Path) {
    let package = workspace.join("vendor/lock-mismatch-probe");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"lock-mismatch-probe","version":"1.0.0","main":"index.js"}"#,
    )
    .unwrap();
    fs::write(package.join("index.js"), "module.exports = 'probe';\n").unwrap();
}

#[test]
fn javascript_prepare_refuses_missing_npm_by_name() {
    let f = Fixture::new();
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-missing-npm-fixture","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    fs::write(
        f.workspace.join("package-lock.json"),
        r#"{"name":"runner-missing-npm-fixture","lockfileVersion":3,"packages":{}}"#,
    )
    .unwrap();
    let (manager, lockfile, mut command) = root_install_command(&f.workspace)
        .unwrap()
        .expect("a package-lock.json workspace must require a locked install");
    assert_eq!(manager, "npm");
    assert_eq!(lockfile, "package-lock.json");
    assert_eq!(command.get_program(), "npm");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["ci", "--ignore-scripts"]
    );
    // Hide npm for this child only; changing the process-wide PATH would race
    // other tests and would make this regression depend on host provisioning.
    let empty_path = f.scratch.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    command.env("PATH", &empty_path);
    let error = run_javascript_install(&f.workspace, manager, lockfile, &mut command)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("npm not installed on the build server"),
        "{error}"
    );
    assert!(error.contains("package-lock.json"), "{error}");
    assert!(
        error.contains(&f.workspace.display().to_string()),
        "{error}"
    );
    assert!(!f.workspace.join("node_modules").exists());
}

fn write_pnpm_workspace_fixture(workspace: &Path) {
    write_local_package_fixture(workspace);
    fs::write(
        workspace.join("pnpm-workspace.yaml"),
        "packages:\n  - vendor/*\n",
    )
    .unwrap();
    fs::write(
        workspace.join("package.json"),
        r#"{"name":"runner-pnpm-fixture","version":"1.0.0","private":true,"scripts":{"preinstall":"echo ran > lifecycle-ran"},"dependencies":{"fixture-dep":"workspace:*"}}"#,
    )
    .unwrap();
}

fn lock_pnpm_workspace(workspace: &Path) {
    let lock = Command::new("pnpm")
        .args([
            "install",
            "--lockfile-only",
            "--ignore-scripts",
            "--offline",
        ])
        .current_dir(workspace)
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "could not create locked pnpm fixture: {}\n{}",
        String::from_utf8_lossy(&lock.stdout),
        String::from_utf8_lossy(&lock.stderr)
    );
    assert!(workspace.join("pnpm-lock.yaml").is_file());
}

#[test]
fn javascript_prepare_requires_locked_pnpm_install_and_refuses_missing_pnpm_by_name() {
    let f = Fixture::new();
    write_pnpm_workspace_fixture(&f.workspace);
    fs::write(
        f.workspace.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\n",
    )
    .unwrap();
    let (manager, lockfile, mut command) = root_install_command(&f.workspace)
        .unwrap()
        .expect("a pnpm-lock.yaml workspace must require a locked install");
    assert_eq!(manager, "pnpm");
    assert_eq!(lockfile, "pnpm-lock.yaml");
    assert_eq!(command.get_program(), "pnpm");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["install", "--frozen-lockfile", "--ignore-scripts"]
    );
    assert_eq!(command.get_current_dir(), Some(f.workspace.as_path()));
    // Hide pnpm for this child only so the refusal is independent of host tools.
    let empty_path = f.scratch.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    command.env("PATH", &empty_path);
    let error = run_javascript_install(&f.workspace, manager, lockfile, &mut command)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("pnpm not installed on the build server"),
        "{error}"
    );
    assert!(error.contains("pnpm-lock.yaml"), "{error}");
    assert!(
        error.contains(&f.workspace.display().to_string()),
        "{error}"
    );
    assert!(!f.workspace.join("node_modules").exists());
}

#[test]
fn javascript_lockfile_precedence_keeps_bun_then_npm_then_pnpm() {
    let f = Fixture::new();
    write_pnpm_workspace_fixture(&f.workspace);
    for lockfile in [
        "bun.lock",
        "bun.lockb",
        "package-lock.json",
        "pnpm-lock.yaml",
    ] {
        fs::write(f.workspace.join(lockfile), "fixture lockfile").unwrap();
    }
    // Remove the selected lockfile on each iteration to verify the remaining
    // manager precedence, including the pnpm-only fallback, without installing
    // or invoking a package manager on the host.
    for (expected_manager, expected_lockfile) in [
        ("bun", "bun.lock"),
        ("bun", "bun.lockb"),
        ("npm", "package-lock.json"),
        ("pnpm", "pnpm-lock.yaml"),
    ] {
        let (manager, lockfile, _) = root_install_command(&f.workspace)
            .unwrap()
            .expect("remaining lockfiles require an install");
        assert_eq!(manager, expected_manager);
        assert_eq!(lockfile, expected_lockfile);
        fs::remove_file(f.workspace.join(expected_lockfile)).unwrap();
    }
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_prepare_installs_pnpm_workspace_without_lifecycle_scripts() {
    let f = Fixture::new();
    write_pnpm_workspace_fixture(&f.workspace);
    lock_pnpm_workspace(&f.workspace);

    let sandbox = sandbox_result(&f).unwrap();
    assert!(f.workspace.join("node_modules").is_dir());
    let (status, stdout, stderr) = output(
        &sandbox,
        "test -f node_modules/fixture-dep/index.js && test ! -e lifecycle-ran && node -e \"if (require('fixture-dep') !== 'fixture') process.exit(1)\"",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    drop(sandbox);
    assert!(!f.workspace.join("lifecycle-ran").exists());
    restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_prepare_refuses_stale_pnpm_workspace_lockfile_by_name() {
    let f = Fixture::new();
    write_pnpm_workspace_fixture(&f.workspace);
    lock_pnpm_workspace(&f.workspace);
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-pnpm-fixture","version":"1.0.0","private":true,"scripts":{"preinstall":"echo ran > lifecycle-ran"},"dependencies":{"fixture-dep":"workspace:^"}}"#,
    )
    .unwrap();

    let error = match sandbox_result(&f) {
        Ok(sandbox) => {
            drop(sandbox);
            panic!("a stale pnpm workspace lockfile was accepted")
        }
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("locked pnpm install using pnpm-lock.yaml failed"),
        "{error}"
    );
    assert!(
        error.contains(&f.workspace.display().to_string()),
        "{error}"
    );
    assert!(!f.workspace.join("lifecycle-ran").exists());
    restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_prepare_installs_bun_lock_without_lifecycle_scripts() {
    let f = Fixture::new();
    write_local_package_fixture(&f.workspace);
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-bun-fixture","version":"1.0.0","private":true,"scripts":{"preinstall":"echo ran > lifecycle-ran"},"dependencies":{"fixture-dep":"file:vendor/fixture-dep"}}"#,
    )
    .unwrap();
    let bun = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".bun/bin/bun"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("bun"));
    let lock = Command::new(bun)
        .args(["install", "--lockfile-only", "--ignore-scripts"])
        .current_dir(&f.workspace)
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "could not create locked Bun fixture: {}",
        String::from_utf8_lossy(&lock.stderr)
    );
    assert!(f.workspace.join("bun.lock").is_file());

    let sandbox = sandbox_result(&f).unwrap();
    assert!(f.workspace.join("node_modules").is_dir());
    let (status, stdout, stderr) = output(
        &sandbox,
        "test -f node_modules/fixture-dep/index.js && test ! -e lifecycle-ran",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    drop(sandbox);

    // The next snapshot tracks the new package so the manager's frozen-lockfile
    // check, rather than local-path validation, must reject the stale lock.
    write_lock_mismatch_probe(&f.workspace);
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-bun-fixture","version":"1.0.0","private":true,"scripts":{"preinstall":"echo ran > lifecycle-ran"},"dependencies":{"fixture-dep":"file:vendor/fixture-dep","lock-mismatch-probe":"file:vendor/lock-mismatch-probe"}}"#,
    )
    .unwrap();
    let error = match sandbox_result(&f) {
        Ok(sandbox) => {
            drop(sandbox);
            panic!("a stale Bun lockfile was accepted")
        }
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("locked bun install using bun.lock failed"),
        "{error}"
    );
    assert!(
        error.contains("JavaScript setup in directory . using bun.lock"),
        "{error}"
    );
    assert!(!f.workspace.join("lifecycle-ran").exists());
    restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_prepare_installs_npm_lock_without_lifecycle_scripts() {
    let f = Fixture::new();
    write_local_package_fixture(&f.workspace);
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-npm-fixture","version":"1.0.0","private":true,"scripts":{"preinstall":"echo ran > lifecycle-ran"},"dependencies":{"fixture-dep":"file:vendor/fixture-dep"}}"#,
    )
    .unwrap();
    let lock = Command::new("npm")
        .args([
            "install",
            "--package-lock-only",
            "--ignore-scripts",
            "--offline",
            "--no-audit",
            "--no-fund",
        ])
        .current_dir(&f.workspace)
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "could not create locked npm fixture: {}",
        String::from_utf8_lossy(&lock.stderr)
    );
    assert!(f.workspace.join("package-lock.json").is_file());

    let sandbox = sandbox_result(&f).unwrap();
    assert!(f.workspace.join("node_modules").is_dir());
    let (status, stdout, stderr) = output(
        &sandbox,
        "test -f node_modules/fixture-dep/index.js && test ! -e lifecycle-ran",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    drop(sandbox);

    // The next snapshot tracks the new package so the manager's frozen-lockfile
    // check, rather than local-path validation, must reject the stale lock.
    write_lock_mismatch_probe(&f.workspace);
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-npm-fixture","version":"1.0.0","private":true,"scripts":{"preinstall":"echo ran > lifecycle-ran"},"dependencies":{"fixture-dep":"file:vendor/fixture-dep","lock-mismatch-probe":"file:vendor/lock-mismatch-probe"}}"#,
    )
    .unwrap();
    let error = match sandbox_result(&f) {
        Ok(sandbox) => {
            drop(sandbox);
            panic!("a stale npm lockfile was accepted")
        }
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("locked npm install using package-lock.json failed"),
        "{error}"
    );
    assert!(
        error.contains("JavaScript setup in directory . using package-lock.json"),
        "{error}"
    );
    assert!(!f.workspace.join("lifecycle-ran").exists());
    restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_prepare_refuses_an_unlocked_js_workspace_by_name() {
    let f = Fixture::new();
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"runner-unlocked-fixture","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    let error = match sandbox_result(&f) {
        Ok(sandbox) => {
            drop(sandbox);
            panic!("workspace without a JavaScript lockfile was accepted")
        }
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains(
            "workspace directory . has package.json but no supported JavaScript lockfile"
        ),
        "{error}"
    );
    assert!(
        error.contains("refusing an unlocked JavaScript install"),
        "{error}"
    );
}

fn tracked_tree(workspace: &Path) -> String {
    checked(Command::new("git").arg("init").arg("-q").arg(workspace)).unwrap();
    checked(
        Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["add", "-f", "."]),
    )
    .unwrap();
    workspace::git(workspace, &["write-tree"]).unwrap()
}

fn sibling_snapshot(path: &Path, tree: &str) -> SiblingSnapshot {
    SiblingSnapshot {
        canonical_path: path.canonicalize().unwrap().to_string_lossy().into_owned(),
        commit: String::new(),
        tree_hash: tree.to_owned(),
        base_commit: None,
    }
}

fn write_js_pair(workspace: &Path, directory: &str, lockfile: &str) {
    let dir = workspace.join(directory);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("package.json"), "{}").unwrap();
    fs::write(dir.join(lockfile), "fixture lockfile").unwrap();
}

#[test]
fn javascript_discovery_uses_job_tree_and_ignores_untracked_and_node_modules() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "nested", "package-lock.json");
    write_js_pair(&f.workspace, "node_modules/tracked", "bun.lock");
    fs::create_dir_all(f.workspace.join("member")).unwrap();
    fs::write(f.workspace.join("member/package.json"), "{}").unwrap();
    let tree = tracked_tree(&f.workspace);
    write_js_pair(&f.workspace, "untracked", "pnpm-lock.yaml");
    checked(
        Command::new("git")
            .arg("-C")
            .arg(&f.workspace)
            .args(["add", "untracked"]),
    )
    .unwrap();
    let installs =
        discover_javascript_installs(&javascript_tracked_paths(&f.workspace, &tree).unwrap())
            .unwrap();
    assert_eq!(
        installs,
        [JavascriptInstall {
            directory: "nested".into(),
            lockfile: "package-lock.json"
        }]
    );
}

#[test]
fn javascript_discovery_refuses_nested_orphan_lock_and_unlocked_root() {
    let f = Fixture::new();
    fs::create_dir_all(f.workspace.join("nested")).unwrap();
    fs::write(f.workspace.join("nested/package-lock.json"), "{}").unwrap();
    let tree = tracked_tree(&f.workspace);
    let error =
        discover_javascript_installs(&javascript_tracked_paths(&f.workspace, &tree).unwrap())
            .unwrap_err()
            .to_string();
    assert!(
        error.contains("nested/package-lock.json") && error.contains("no package.json"),
        "{error}"
    );
    let error = discover_javascript_installs(&["package.json".into()])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("workspace directory .") && error.contains("refusing an unlocked"),
        "{error}"
    );
}

#[test]
fn javascript_discovery_order_is_shallowest_then_path_with_one_lock_per_directory() {
    let paths: Vec<PathBuf> = [
        "z/deep/package.json",
        "z/deep/bun.lock",
        "b/package.json",
        "b/pnpm-lock.yaml",
        "a/package.json",
        "a/bun.lockb",
        "a/bun.lock",
        "a/package-lock.json",
        "a/pnpm-lock.yaml",
        "package.json",
        "package-lock.json",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    let installs = discover_javascript_installs(&paths).unwrap();
    assert_eq!(
        installs
            .iter()
            .map(|i| (i.directory.to_str().unwrap(), i.lockfile))
            .collect::<Vec<_>>(),
        [
            ("", "package-lock.json"),
            ("a", "bun.lock"),
            ("b", "pnpm-lock.yaml"),
            ("z/deep", "bun.lock")
        ]
    );
}

fn root_install_command(
    workspace: &Path,
) -> io::Result<Option<(&'static str, &'static str, Command)>> {
    let tree = tracked_tree(workspace);
    let installs = discover_javascript_installs(&javascript_tracked_paths(workspace, &tree)?)?;
    let Some(install) = installs
        .into_iter()
        .find(|i| i.directory.as_os_str().is_empty())
    else {
        return Ok(None);
    };
    let command = javascript_install_command(workspace, &install)?;
    Ok(Some((
        javascript_manager(install.lockfile),
        install.lockfile,
        command,
    )))
}

#[test]
fn javascript_nested_install_failure_refuses_and_does_not_install_later_directories() {
    let f = Fixture::new();
    for dir in ["a", "z"] {
        write_js_pair(&f.workspace, dir, "package-lock.json");
    }
    let tree = tracked_tree(&f.workspace);
    let mut called = Vec::new();
    let paths = javascript_tracked_paths(&f.workspace, &tree).unwrap();
    let error = prepare_javascript_with(&f.workspace, &paths, |install, command| {
        called.push(install.directory.clone());
        assert_eq!(
            command.get_current_dir(),
            Some(f.workspace.join(&install.directory).as_path())
        );
        *command = Command::new("sh");
        command.args(["-c", "echo stale-lock >&2; exit 1"]);
        run_javascript_install(
            &f.workspace.join(&install.directory),
            javascript_manager(install.lockfile),
            &install.directory.join(install.lockfile).to_string_lossy(),
            command,
        )
    })
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("locked npm install using a/package-lock.json failed"),
        "{error}"
    );
    assert_eq!(called, [PathBuf::from("a")]);
}

#[test]
fn javascript_install_order_matches_discovery_order() {
    let f = Fixture::new();
    for dir in ["", "a", "b", "a/deep"] {
        write_js_pair(&f.workspace, dir, "package-lock.json");
    }
    let tree = tracked_tree(&f.workspace);
    let mut called = Vec::new();
    let paths = javascript_tracked_paths(&f.workspace, &tree).unwrap();
    prepare_javascript_with(&f.workspace, &paths, |install, _| {
        called.push(install.directory.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(called, ["", "a", "b", "a/deep"].map(PathBuf::from));
}

/// Plain copy, not a reflink: the fixture lives in the source checkout, which
/// is usually a different filesystem from the scratch workspace, and reflinks
/// cannot cross filesystems.
///
/// Only the committed fixture is copied. When these tests run as a remote job,
/// the runner has already installed `node_modules` into each fixture directory
/// (they are tracked directories with a lockfile), and those trees hold
/// symlinks to directories that a plain file copy cannot handle. The tests
/// install their own dependencies, so the pre-installed tree is skipped.
fn copy_committed_js_fixture(workspace: &Path, manager: &str) {
    fn copy(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == "node_modules" {
                continue;
            }
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-fixtures/javascript")
            .join(manager),
        &workspace.join(manager),
    );
}

#[test]
fn javascript_real_managers_install_nested_and_refuse_stale_lock() {
    let f = Fixture::new();
    for manager in ["npm", "bun", "pnpm"] {
        copy_committed_js_fixture(&f.workspace, manager);
    }
    let tree = tracked_tree(&f.workspace);
    prepare_javascript_dependencies(
        &f.state,
        &f.workspace.to_string_lossy(),
        &f.workspace,
        &tree,
    )
    .unwrap();
    for manager in ["npm", "bun", "pnpm"] {
        let dir = f.workspace.join(manager);
        checked(
            Command::new("node")
                .args([
                    "-e",
                    "if (require('fixture-dep') !== 'fixture') process.exit(1)",
                ])
                .current_dir(&dir),
        )
        .unwrap();
        assert!(!dir.join("lifecycle-ran").exists());
    }
    let manifest = f.workspace.join("npm/package.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    fs::create_dir_all(f.workspace.join("npm/vendor/missing")).unwrap();
    fs::write(
        f.workspace.join("npm/vendor/missing/package.json"),
        r#"{"name":"missing","version":"1.0.0"}"#,
    )
    .unwrap();
    value["dependencies"]["missing"] = "file:vendor/missing".into();
    fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    let tree = tracked_tree(&f.workspace);
    let error = prepare_javascript_dependencies(
        &f.state,
        &f.workspace.to_string_lossy(),
        &f.workspace,
        &tree,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("locked npm install using npm/package-lock.json failed"),
        "{error}"
    );
}

struct FakeJavascriptManager {
    executable: PathBuf,
    version: PathBuf,
    calls: PathBuf,
    interrupted: PathBuf,
    /// Lists member directories, relative to the install directory, that the
    /// fake install also gives a node_modules, as a workspace install does.
    members: PathBuf,
    bin: PathBuf,
}
impl FakeJavascriptManager {
    fn new(f: &Fixture) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let bin = f.scratch.join("fake-bin");
        fs::create_dir(&bin).unwrap();
        let executable = bin.join("npm");
        fs::write(
            &executable,
            r#"#!/bin/sh
if [ "$1" = --version ]; then /bin/cat "$JS_VERSION"; exit; fi
printf 'install\n' >> "$JS_CALLS"
if [ -f "$JS_INTERRUPTED" ]; then kill -TERM $$; fi
/bin/mkdir -p node_modules/.bin
printf '#!/bin/sh\nprintf fixture' > node_modules/.bin/fixture-tool
/bin/chmod +x node_modules/.bin/fixture-tool
if [ -f "$JS_MEMBERS" ]; then for m in $(/bin/cat "$JS_MEMBERS"); do /bin/mkdir -p "$m/node_modules/member-dep"; done; fi
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(bin.join("node"), "#!/bin/sh\nprintf 'node-fixture-v1\\n'\n").unwrap();
        fs::set_permissions(bin.join("node"), fs::Permissions::from_mode(0o755)).unwrap();
        let version = f.scratch.join("fake-version");
        fs::write(&version, "manager-v1\n").unwrap();
        Self {
            executable,
            version,
            calls: f.scratch.join("install-calls"),
            interrupted: f.scratch.join("interrupt-install"),
            members: f.scratch.join("member-dirs"),
            bin,
        }
    }
    fn prepare(&self, f: &Fixture, tree: &str) -> io::Result<()> {
        self.prepare_checkout(f, &f.workspace, tree)
    }
    fn prepare_checkout(&self, f: &Fixture, checkout: &Path, tree: &str) -> io::Result<()> {
        let tracked = javascript_tracked_tree(checkout, tree)?;
        self.prepare_tracked_checkout(f, checkout, &tracked)
    }
    fn prepare_tracked(&self, f: &Fixture, tracked: &JavascriptTree) -> io::Result<()> {
        self.prepare_tracked_checkout(f, &f.workspace, tracked)
    }
    fn prepare_tracked_checkout(
        &self,
        f: &Fixture,
        checkout: &Path,
        tracked: &JavascriptTree,
    ) -> io::Result<()> {
        prepare_javascript_with(checkout, &tracked.paths, |install, command| {
            let mut fake = Command::new(&self.executable);
            fake.args(command.get_args())
                .current_dir(command.get_current_dir().unwrap())
                .env("PATH", &self.bin)
                .env("JS_VERSION", &self.version)
                .env("JS_CALLS", &self.calls)
                .env("JS_INTERRUPTED", &self.interrupted)
                .env("JS_MEMBERS", &self.members);
            cache_javascript_install(
                &f.state,
                &checkout.to_string_lossy(),
                checkout,
                tracked,
                install,
                &mut fake,
            )
        })
    }
    fn count(&self) -> usize {
        fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .count()
    }
}

fn prepare_checkouts_with_fake_manager(
    f: &Fixture,
    manager: &FakeJavascriptManager,
    workspace_tree: &str,
    siblings: &[SiblingSnapshot],
) -> io::Result<()> {
    prepare_javascript_checkouts(
        &f.workspace,
        &f.workspace.to_string_lossy(),
        workspace_tree,
        siblings,
        |checkout, _repository, tree| manager.prepare_checkout(f, checkout, tree),
    )
}

#[test]
fn javascript_sibling_with_lockfile_gets_node_modules() {
    let f = Fixture::new();
    let sibling = f.scratch.join("sibling");
    fs::create_dir_all(&sibling).unwrap();
    write_js_pair(&sibling, "", "package-lock.json");
    let workspace_tree = tracked_tree(&f.workspace);
    let sibling_tree = tracked_tree(&sibling);
    let siblings = [sibling_snapshot(&sibling, &sibling_tree)];
    let manager = FakeJavascriptManager::new(&f);

    prepare_checkouts_with_fake_manager(&f, &manager, &workspace_tree, &siblings).unwrap();

    assert!(sibling.join("node_modules/.bin/fixture-tool").is_file());
    assert!(!f.workspace.join("node_modules").exists());
    assert_eq!(manager.count(), 1);
}

#[test]
fn javascript_sibling_without_lockfiles_is_untouched() {
    let f = Fixture::new();
    let sibling = f.scratch.join("sibling");
    fs::create_dir_all(&sibling).unwrap();
    fs::write(sibling.join("README.md"), "not a JavaScript checkout").unwrap();
    let sibling_tree = tracked_tree(&sibling);
    let sentinel = sibling.join("node_modules/keep-me");
    fs::create_dir_all(&sentinel).unwrap();
    fs::write(sentinel.join("marker"), "preserved").unwrap();
    let workspace_tree = tracked_tree(&f.workspace);
    let siblings = [sibling_snapshot(&sibling, &sibling_tree)];
    let manager = FakeJavascriptManager::new(&f);

    prepare_checkouts_with_fake_manager(&f, &manager, &workspace_tree, &siblings).unwrap();

    assert_eq!(manager.count(), 0);
    assert_eq!(fs::read(sentinel.join("marker")).unwrap(), b"preserved");
}

#[test]
fn javascript_sibling_local_dependency_escape_is_refused_like_workspace() {
    fn write_escaping_install(checkout: &Path) -> String {
        fs::create_dir_all(checkout).unwrap();
        fs::write(
            checkout.join("package.json"),
            r#"{"dependencies":{"outside":"file:../outside.tgz"}}"#,
        )
        .unwrap();
        fs::write(checkout.join("package-lock.json"), "fixture lockfile").unwrap();
        tracked_tree(checkout)
    }

    let f = Fixture::new();
    let reference = f.scratch.join("reference-workspace");
    let reference_tree = write_escaping_install(&reference);
    let sibling = f.scratch.join("sibling");
    let sibling_tree = write_escaping_install(&sibling);
    let workspace_tree = tracked_tree(&f.workspace);
    let manager = FakeJavascriptManager::new(&f);
    let expected = manager
        .prepare_checkout(&f, &reference, &reference_tree)
        .unwrap_err()
        .to_string();
    let siblings = [sibling_snapshot(&sibling, &sibling_tree)];
    let error = prepare_checkouts_with_fake_manager(&f, &manager, &workspace_tree, &siblings)
        .unwrap_err()
        .to_string();

    assert_eq!(error, expected);
    assert!(
        error.contains("JavaScript manifest package.json"),
        "{error}"
    );
    assert!(error.contains("file:../outside.tgz"), "{error}");
    assert!(
        error.contains("path resolves outside the repository"),
        "{error}"
    );
    assert_eq!(manager.count(), 0);
}

#[test]
fn javascript_sibling_second_job_reuses_install_cache_for_same_tree() {
    let f = Fixture::new();
    let workspace_tree = tracked_tree(&f.workspace);
    let sibling = f.scratch.join("sibling");
    fs::create_dir_all(&sibling).unwrap();
    write_js_pair(&sibling, "", "package-lock.json");
    let tree = tracked_tree(&sibling);
    let manager = FakeJavascriptManager::new(&f);
    let siblings = [sibling_snapshot(&sibling, &tree)];
    prepare_checkouts_with_fake_manager(&f, &manager, &workspace_tree, &siblings).unwrap();
    fs::remove_dir_all(sibling.join("node_modules")).unwrap();

    prepare_checkouts_with_fake_manager(&f, &manager, &workspace_tree, &siblings).unwrap();

    assert_eq!(
        manager.count(),
        1,
        "the second sibling job must hit the cache"
    );
    assert!(sibling.join("node_modules/.bin/fixture-tool").is_file());
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        1
    );
}

#[test]
fn javascript_cache_second_job_skips_install_and_input_changes_miss() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "nested", "package-lock.json");
    let tree = tracked_tree(&f.workspace);
    let manager = FakeJavascriptManager::new(&f);
    manager.prepare(&f, &tree).unwrap();
    assert_eq!(manager.count(), 1);
    fs::remove_dir_all(f.workspace.join("nested/node_modules")).unwrap();
    manager.prepare(&f, &tree).unwrap();
    assert_eq!(manager.count(), 1, "a second job must not call install");
    let result = Command::new(f.workspace.join("nested/node_modules/.bin/fixture-tool"))
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, b"fixture");
    for path in ["nested/package-lock.json", "nested/package.json"] {
        fs::write(f.workspace.join(path), r#"{"changed":true}"#).unwrap();
        let tree = tracked_tree(&f.workspace);
        manager.prepare(&f, &tree).unwrap();
    }
    assert_eq!(manager.count(), 3);
    fs::write(&manager.version, "manager-v2\n").unwrap();
    let tree = tracked_tree(&f.workspace);
    manager.prepare(&f, &tree).unwrap();
    assert_eq!(manager.count(), 4);
}

#[test]
fn javascript_cache_key_covers_manifests_configs_lockfile_and_tool_versions() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "nested", "package-lock.json");
    fs::write(
        f.workspace.join("nested/package.json"),
        r#"{"dependencies":{"tool":"file:vendor/tool.js"}}"#,
    )
    .unwrap();
    let input_names = [
        "nested/member/package.json",
        "nested/.npmrc",
        "nested/bunfig.toml",
        "nested/pnpm-workspace.yaml",
        ".npmrc",
        "nested/vendor/tool.js",
    ];
    for name in input_names {
        fs::create_dir_all(f.workspace.join(name).parent().unwrap()).unwrap();
        fs::write(f.workspace.join(name), "{}").unwrap();
    }
    let tree = tracked_tree(&f.workspace);
    let tracked = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    let install = &discover_javascript_installs(&tracked.paths).unwrap()[0];
    let key = javascript_cache_key(
        &f.workspace,
        &f.workspace.to_string_lossy(),
        &tracked,
        install,
        b"npm-v1",
        b"node-v1",
    )
    .unwrap();
    for name in ["nested/package.json", "nested/package-lock.json"]
        .into_iter()
        .chain(input_names)
    {
        let original = fs::read(f.workspace.join(name)).unwrap();
        fs::write(f.workspace.join(name), r#"{"changed":true}"#).unwrap();
        let changed_tree = tracked_tree(&f.workspace);
        let changed = javascript_tracked_tree(&f.workspace, &changed_tree).unwrap();
        assert_ne!(
            key,
            javascript_cache_key(
                &f.workspace,
                &f.workspace.to_string_lossy(),
                &changed,
                install,
                b"npm-v1",
                b"node-v1"
            )
            .unwrap(),
            "{name} must be an input"
        );
        fs::write(f.workspace.join(name), original).unwrap();
    }
    assert_ne!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &tracked,
            install,
            b"npm-v2",
            b"node-v1"
        )
        .unwrap()
    );
    assert_ne!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &tracked,
            install,
            b"npm-v1",
            b"node-v2"
        )
        .unwrap()
    );
    fs::write(
        f.workspace.join("nested/package.json"),
        "not the job manifest",
    )
    .unwrap();
    assert_eq!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &tracked,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap(),
        "working tree edits cannot change the captured job tree's key"
    );
}

#[test]
fn javascript_workspace_install_with_member_modules_is_never_cached() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "nested", "package-lock.json");
    fs::create_dir_all(f.workspace.join("nested/member")).unwrap();
    fs::write(f.workspace.join("nested/member/package.json"), "{}").unwrap();
    let tree = tracked_tree(&f.workspace);
    let manager = FakeJavascriptManager::new(&f);
    fs::write(&manager.members, "member\n").unwrap();
    manager.prepare(&f, &tree).unwrap();
    assert!(f
        .workspace
        .join("nested/member/node_modules/member-dep")
        .is_dir());
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        0,
        "an entry would restore the root without the member's node_modules"
    );
    // The next job starts from a cleaned checkout.
    for dir in ["nested/node_modules", "nested/member/node_modules"] {
        fs::remove_dir_all(f.workspace.join(dir)).unwrap();
    }
    manager.prepare(&f, &tree).unwrap();
    assert_eq!(
        manager.count(),
        2,
        "a workspace install runs fresh every job"
    );
    assert!(f
        .workspace
        .join("nested/member/node_modules/member-dep")
        .is_dir());
}

#[test]
fn javascript_interrupted_install_never_publishes_a_cache_entry() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "nested", "package-lock.json");
    let tree = tracked_tree(&f.workspace);
    let manager = FakeJavascriptManager::new(&f);
    fs::write(&manager.interrupted, "interrupt").unwrap();
    assert!(manager
        .prepare(&f, &tree)
        .unwrap_err()
        .to_string()
        .contains("nested/package-lock.json"));
    assert_eq!(manager.count(), 1);
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        0
    );
    fs::remove_file(&manager.interrupted).unwrap();
    manager.prepare(&f, &tree).unwrap();
    assert_eq!(manager.count(), 2);
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        1
    );
}

#[test]
fn javascript_real_npm_cache_clones_bin_into_second_workspace_and_times_hit() {
    let f = Fixture::new();
    copy_committed_js_fixture(&f.workspace, "npm");
    let tree = tracked_tree(&f.workspace);
    let cold_start = Instant::now();
    prepare_javascript_dependencies(
        &f.state,
        &f.workspace.to_string_lossy(),
        &f.workspace,
        &tree,
    )
    .unwrap();
    let cold = cold_start.elapsed();
    let second = f.scratch.join("second-workspace");
    fs::create_dir(&second).unwrap();
    copy_committed_js_fixture(&second, "npm");
    let second_tree = tracked_tree(&second);
    assert_eq!(tree, second_tree);
    let hit_start = Instant::now();
    prepare_javascript_dependencies(
        &f.state,
        &f.workspace.to_string_lossy(),
        &second,
        &second_tree,
    )
    .unwrap();
    let hit = hit_start.elapsed();
    let result = Command::new(second.join("npm/node_modules/.bin/fixture-tool"))
        .current_dir(second.join("npm"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"fixture\n");
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        1
    );
    println!("npm fixture cold={cold:?}, second-job cache hit={hit:?}");
}

fn commit_javascript_tree(workspace: &Path) -> String {
    let tree = tracked_tree(workspace);
    workspace::git(
        workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture snapshot",
        ],
    )
    .unwrap();
    tree
}

#[test]
fn javascript_root_cache_hits_across_unrelated_source_commits() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "", "package-lock.json");
    fs::write(f.workspace.join("source.rs"), "first source").unwrap();
    let first_tree = commit_javascript_tree(&f.workspace);
    let first = javascript_tracked_tree(&f.workspace, &first_tree).unwrap();
    let install = &discover_javascript_installs(&first.paths).unwrap()[0];
    let first_key = javascript_cache_key(
        &f.workspace,
        &f.workspace.to_string_lossy(),
        &first,
        install,
        b"npm-v1",
        b"node-v1",
    )
    .unwrap();
    let manager = FakeJavascriptManager::new(&f);
    manager.prepare_tracked(&f, &first).unwrap();
    fs::remove_dir_all(f.workspace.join("node_modules")).unwrap();
    fs::write(f.workspace.join("source.rs"), "second source").unwrap();
    let second_tree = commit_javascript_tree(&f.workspace);
    assert_ne!(first_tree, second_tree);
    let second = javascript_tracked_tree(&f.workspace, &second_tree).unwrap();
    assert_eq!(
        first_key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &second,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap()
    );
    manager.prepare_tracked(&f, &second).unwrap();
    assert_eq!(
        manager.count(),
        1,
        "unrelated commits must reuse the root install"
    );
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        1
    );
    assert_eq!(
        fs::read_dir(f.state.join("js-install-locks"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn javascript_file_dependency_directory_changes_miss_relative_to_member_manifest() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "install", "package-lock.json");
    write_local_package_fixture(&f.workspace);
    fs::create_dir_all(f.workspace.join("install/member")).unwrap();
    fs::write(
        f.workspace.join("install/member/package.json"),
        r#"{"dependencies":{"fixture-dep":"file:../../vendor/fixture-dep"}}"#,
    )
    .unwrap();
    fs::write(f.workspace.join("install/source.rs"), "unrelated source").unwrap();
    let tree = tracked_tree(&f.workspace);
    let first = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    let install = &discover_javascript_installs(&first.paths).unwrap()[0];
    let key = javascript_cache_key(
        &f.workspace,
        &f.workspace.to_string_lossy(),
        &first,
        install,
        b"npm-v1",
        b"node-v1",
    )
    .unwrap();
    let manager = FakeJavascriptManager::new(&f);
    manager.prepare_tracked(&f, &first).unwrap();
    fs::write(
        f.workspace.join("install/source.rs"),
        "another unrelated source",
    )
    .unwrap();
    let tree = tracked_tree(&f.workspace);
    let unrelated = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    assert_eq!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &unrelated,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap()
    );
    fs::write(
        f.workspace.join("vendor/fixture-dep/index.js"),
        "module.exports = 'changed';\n",
    )
    .unwrap();
    let tree = tracked_tree(&f.workspace);
    let changed = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    assert_ne!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &changed,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap()
    );
    manager.prepare_tracked(&f, &changed).unwrap();
    assert_eq!(
        manager.count(),
        2,
        "changing a local dependency must reinstall"
    );
}

#[test]
fn javascript_local_dependency_blob_ids_cover_file_link_and_portal_specs() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "install", "package-lock.json");
    fs::write(
        f.workspace.join("packed dependency.tgz"),
        "first tracked archive",
    )
    .unwrap();
    for (section, scheme) in [
        ("dependencies", "file:"),
        ("optionalDependencies", "link:"),
        ("overrides", "portal:"),
    ] {
        let spec = format!("{scheme}../packed dependency.tgz");
        let value = serde_json::json!({ section: {"fixture-dep": spec} });
        fs::write(
            f.workspace.join("install/package.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let tree = tracked_tree(&f.workspace);
        let tracked = javascript_tracked_tree(&f.workspace, &tree).unwrap();
        let install = &discover_javascript_installs(&tracked.paths).unwrap()[0];
        let key = javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &tracked,
            install,
            b"npm-v1",
            b"node-v1",
        )
        .unwrap();
        fs::write(
            f.workspace.join("packed dependency.tgz"),
            format!("changed archive {scheme}"),
        )
        .unwrap();
        let tree = tracked_tree(&f.workspace);
        let changed = javascript_tracked_tree(&f.workspace, &tree).unwrap();
        assert_ne!(
            key,
            javascript_cache_key(
                &f.workspace,
                &f.workspace.to_string_lossy(),
                &changed,
                install,
                b"npm-v1",
                b"node-v1"
            )
            .unwrap(),
            "{scheme} file contents must be keyed"
        );
    }
}

#[test]
fn javascript_local_dependency_outside_or_untracked_refuses_before_install() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "install", "package-lock.json");
    let manager = FakeJavascriptManager::new(&f);
    let outside = f.scratch.join("outside.tgz");
    fs::write(&outside, "not in the repository").unwrap();
    let specs = [
        "file:../../outside.tgz".to_string(),
        format!("link:{}", outside.display()),
        "portal:../untracked.tgz".to_string(),
    ];
    for spec in specs {
        let value = serde_json::json!({"dependencies":{"fixture-dep":spec}});
        fs::write(
            f.workspace.join("install/package.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let tree = tracked_tree(&f.workspace);
        fs::write(f.workspace.join("untracked.tgz"), "not in the job tree").unwrap();
        let error = manager.prepare(&f, &tree).unwrap_err().to_string();
        assert!(
            error.contains("install/package.json") && error.contains(&spec),
            "{error}"
        );
        assert!(
            error.contains("outside the repository") || error.contains("not tracked"),
            "{error}"
        );
        fs::remove_file(f.workspace.join("untracked.tgz")).unwrap();
    }
    assert_eq!(manager.count(), 0);
    assert!(!f.state.join("js-installs").exists());
}

#[test]
fn javascript_local_dependency_symlink_cannot_escape_repository() {
    let f = Fixture::new();
    write_js_pair(&f.workspace, "install", "package-lock.json");
    fs::write(
        f.workspace.join("install/package.json"),
        r#"{"dependencies":{"fixture-dep":"file:../linked.tgz"}}"#,
    )
    .unwrap();
    let outside = f.scratch.join("outside.tgz");
    fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, f.workspace.join("linked.tgz")).unwrap();
    let tree = tracked_tree(&f.workspace);
    let manager = FakeJavascriptManager::new(&f);
    let error = manager.prepare(&f, &tree).unwrap_err().to_string();
    assert!(
        error.contains("install/package.json")
            && error.contains("file:../linked.tgz")
            && error.contains("outside the repository"),
        "{error}"
    );
    assert_eq!(manager.count(), 0);
}

#[test]
fn javascript_discovery_and_cache_reuse_one_collected_tree_without_more_git_calls() {
    let f = Fixture::new();
    for dir in ["a", "b"] {
        write_js_pair(&f.workspace, dir, "package-lock.json");
    }
    let tree = tracked_tree(&f.workspace);
    let tracked = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    fs::rename(f.workspace.join(".git"), f.scratch.join("saved-git")).unwrap();
    let manager = FakeJavascriptManager::new(&f);
    manager.prepare_tracked(&f, &tracked).unwrap();
    for dir in ["a", "b"] {
        fs::remove_dir_all(f.workspace.join(dir).join("node_modules")).unwrap();
    }
    manager.prepare_tracked(&f, &tracked).unwrap();
    assert_eq!(
        manager.count(),
        2,
        "each directory installs once without further Git access"
    );
}

fn write_transitive_javascript_fixture(f: &Fixture) {
    write_js_pair(&f.workspace, "install", "package-lock.json");
    fs::write(
        f.workspace.join("install/package.json"),
        r#"{"dependencies":{"fixture-a":"file:../local/A"}}"#,
    )
    .unwrap();
    for name in ["A", "B"] {
        fs::create_dir_all(f.workspace.join("local").join(name)).unwrap();
        fs::write(
            f.workspace.join("local").join(name).join("index.js"),
            format!("module.exports = '{name}';\n"),
        )
        .unwrap();
    }
    fs::write(
        f.workspace.join("local/A/package.json"),
        r#"{"name":"fixture-a","version":"1.0.0","dependencies":{"fixture-b":"file:../B"}}"#,
    )
    .unwrap();
    fs::write(
        f.workspace.join("local/B/package.json"),
        r#"{"name":"fixture-b","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(f.workspace.join("local/unrelated.rs"), "unrelated source").unwrap();
}

#[test]
fn javascript_transitive_local_dependency_changes_miss_without_keying_unrelated_siblings() {
    let f = Fixture::new();
    write_transitive_javascript_fixture(&f);
    let tree = tracked_tree(&f.workspace);
    let first = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    let install = &discover_javascript_installs(&first.paths).unwrap()[0];
    let key = javascript_cache_key(
        &f.workspace,
        &f.workspace.to_string_lossy(),
        &first,
        install,
        b"npm-v1",
        b"node-v1",
    )
    .unwrap();
    let manager = FakeJavascriptManager::new(&f);
    manager.prepare_tracked(&f, &first).unwrap();
    fs::write(
        f.workspace.join("local/unrelated.rs"),
        "changed unrelated sibling",
    )
    .unwrap();
    let tree = tracked_tree(&f.workspace);
    let unrelated = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    assert_eq!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &unrelated,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap()
    );
    manager.prepare_tracked(&f, &unrelated).unwrap();
    assert_eq!(manager.count(), 1);
    fs::write(
        f.workspace.join("local/B/index.js"),
        "module.exports = 'changed B';\n",
    )
    .unwrap();
    let tree = tracked_tree(&f.workspace);
    let changed = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    assert_eq!(
        first.objects[Path::new("local/A")].id,
        changed.objects[Path::new("local/A")].id,
        "the directly declared dependency A has not changed"
    );
    assert_ne!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &changed,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap(),
        "B's content must be keyed through A's manifest"
    );
    manager.prepare_tracked(&f, &changed).unwrap();
    assert_eq!(
        manager.count(),
        2,
        "changing transitive dependency B must reinstall"
    );
}

#[test]
fn javascript_local_dependency_cycle_terminates_and_reuses_cache() {
    let f = Fixture::new();
    write_transitive_javascript_fixture(&f);
    fs::write(
        f.workspace.join("local/B/package.json"),
        r#"{"name":"fixture-b","version":"1.0.0","dependencies":{"fixture-a":"portal:../A"}}"#,
    )
    .unwrap();
    let tree = tracked_tree(&f.workspace);
    let tracked = javascript_tracked_tree(&f.workspace, &tree).unwrap();
    let install = &discover_javascript_installs(&tracked.paths).unwrap()[0];
    let key = javascript_cache_key(
        &f.workspace,
        &f.workspace.to_string_lossy(),
        &tracked,
        install,
        b"npm-v1",
        b"node-v1",
    )
    .unwrap();
    assert_eq!(
        key,
        javascript_cache_key(
            &f.workspace,
            &f.workspace.to_string_lossy(),
            &tracked,
            install,
            b"npm-v1",
            b"node-v1"
        )
        .unwrap()
    );
    let manager = FakeJavascriptManager::new(&f);
    manager.prepare_tracked(&f, &tracked).unwrap();
    fs::remove_dir_all(f.workspace.join("install/node_modules")).unwrap();
    manager.prepare_tracked(&f, &tracked).unwrap();
    assert_eq!(
        manager.count(),
        1,
        "A/B cycle should produce a stable cache key"
    );
    assert_eq!(
        fs::read_dir(f.state.join("js-installs")).unwrap().count(),
        1
    );
}

#[test]
fn javascript_transitive_local_dependency_refusals_name_manifest_and_spec() {
    let f = Fixture::new();
    write_transitive_javascript_fixture(&f);
    let manager = FakeJavascriptManager::new(&f);
    for spec in [
        "file:../../../outside".to_string(),
        format!("link:{}", f.scratch.join("outside").display()),
        "portal:../untracked".to_string(),
    ] {
        let value = serde_json::json!({"dependencies":{"invalid":spec}});
        fs::write(
            f.workspace.join("local/B/package.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let tree = tracked_tree(&f.workspace);
        let error = manager.prepare(&f, &tree).unwrap_err().to_string();
        assert!(
            error.contains("local/B/package.json") && error.contains(&spec),
            "{error}"
        );
        assert!(
            error.contains("outside the repository") || error.contains("not tracked"),
            "{error}"
        );
    }
    assert_eq!(
        manager.count(),
        0,
        "invalid transitive inputs refuse before installation"
    );
    assert!(!f.state.join("js-installs").exists());
}

#[test]
fn javascript_tracked_local_probe_reaches_bun_and_npm_frozen_lock_refusal() {
    for (manager, lockfile) in [("bun", "bun.lock"), ("npm", "package-lock.json")] {
        let f = Fixture::new();
        copy_committed_js_fixture(&f.workspace, manager);
        let tree = tracked_tree(&f.workspace);
        prepare_javascript_dependencies(
            &f.state,
            &f.workspace.to_string_lossy(),
            &f.workspace,
            &tree,
        )
        .unwrap();
        let directory = f.workspace.join(manager);
        write_lock_mismatch_probe(&directory);
        let manifest = directory.join("package.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value["dependencies"]["lock-mismatch-probe"] = "file:vendor/lock-mismatch-probe".into();
        fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        let tree = tracked_tree(&f.workspace);
        let error = prepare_javascript_dependencies(
            &f.state,
            &f.workspace.to_string_lossy(),
            &f.workspace,
            &tree,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(&format!(
                "locked {manager} install using {manager}/{lockfile} failed"
            )),
            "{error}"
        );
        assert!(!directory.join("lifecycle-ran").exists());
    }
}

#[test]
fn javascript_distinct_repositories_never_share_install_caches() {
    let f = Fixture::new();
    let manager = FakeJavascriptManager::new(&f);
    for name in ["repository-a", "repository-b"] {
        let checkout = f.scratch.join(name);
        fs::create_dir(&checkout).unwrap();
        write_js_pair(&checkout, "", "package-lock.json");
        let tree = tracked_tree(&checkout);
        manager.prepare_checkout(&f, &checkout, &tree).unwrap();
    }
    assert_eq!(
        manager.count(),
        2,
        "different repositories must both install"
    );
    assert_eq!(
        fs::read_dir(f.state.join("js-manager-caches"))
            .unwrap()
            .count(),
        2,
        "different repositories must use different manager caches"
    );
}

#[test]
fn javascript_manager_caches_ignore_inherited_locations() {
    let f = Fixture::new();
    let manager = FakeJavascriptManager::new(&f);
    for lockfile in ["bun.lock", "package-lock.json", "pnpm-lock.yaml"] {
        let checkout = f.scratch.join(lockfile);
        fs::create_dir(&checkout).unwrap();
        write_js_pair(&checkout, "", lockfile);
        let tree = tracked_tree(&checkout);
        let tracked = javascript_tracked_tree(&checkout, &tree).unwrap();
        let install = &discover_javascript_installs(&tracked.paths).unwrap()[0];
        let mut command = Command::new(&manager.executable);
        command
            .args(
                javascript_install_command(&checkout, install)
                    .unwrap()
                    .get_args(),
            )
            .current_dir(&checkout)
            .env("PATH", &manager.bin)
            .env("JS_VERSION", &manager.version)
            .env("JS_CALLS", &manager.calls)
            .env("JS_INTERRUPTED", &manager.interrupted)
            .env("JS_MEMBERS", &manager.members);
        for name in [
            "BUN_INSTALL",
            "BUN_INSTALL_CACHE_DIR",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "npm_config_cache",
            "NPM_CONFIG_CACHE",
            "pnpm_config_store_dir",
            "PNPM_CONFIG_STORE_DIR",
        ] {
            command.env(name, f.scratch.join("operator-cache"));
        }
        cache_javascript_install(
            &f.state,
            &checkout.to_string_lossy(),
            &checkout,
            &tracked,
            install,
            &mut command,
        )
        .unwrap();
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for name in [
            "BUN_INSTALL",
            "NPM_CONFIG_CACHE",
            "pnpm_config_store_dir",
            "PNPM_CONFIG_STORE_DIR",
        ] {
            assert_eq!(
                env.get(std::ffi::OsStr::new(name)),
                Some(&None),
                "{name} must be stripped"
            );
        }
        for name in [
            "BUN_INSTALL_CACHE_DIR",
            "npm_config_cache",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
        ] {
            let value = env
                .get(std::ffi::OsStr::new(name))
                .and_then(|v| *v)
                .expect("explicit runner cache location");
            assert!(Path::new(value).starts_with(&f.state), "{name}: {value:?}");
        }
        let args: Vec<_> = command.get_args().collect();
        let cache_flag = match javascript_manager(lockfile) {
            "bun" => "--cache-dir",
            "npm" => "--cache",
            _ => "--store-dir",
        };
        let index = args
            .iter()
            .position(|arg| *arg == cache_flag)
            .expect("explicit manager cache flag");
        assert!(Path::new(args[index + 1]).starts_with(&f.state));
    }
}

#[test]
fn javascript_bun_same_path_changed_tarball_installs_changed_bytes() {
    let f = Fixture::new();
    let producer = f.scratch.join("producer/package");
    fs::create_dir_all(&producer).unwrap();
    fs::write(
        f.workspace.join("package.json"),
        r#"{"name":"consumer","dependencies":{"local-producer":"file:producer.tgz"}}"#,
    )
    .unwrap();
    for marker in ["first", "changed"] {
        fs::write(
            producer.join("package.json"),
            format!(r#"{{"name":"local-producer","version":"1.0.0","marker":"{marker}"}}"#),
        )
        .unwrap();
        checked(
            Command::new("tar")
                .args(["-czf"])
                .arg(f.workspace.join("producer.tgz"))
                .arg("-C")
                .arg(producer.parent().unwrap())
                .arg("package"),
        )
        .unwrap();
        let _ = fs::remove_file(f.workspace.join("bun.lock"));
        // Resolve the new bytes independently so the frozen install has a valid lockfile.
        checked(
            Command::new("bun")
                .args(["install", "--lockfile-only", "--ignore-scripts"])
                .current_dir(&f.workspace)
                .env(
                    "BUN_INSTALL_CACHE_DIR",
                    f.scratch.join(format!("lock-cache-{marker}")),
                ),
        )
        .unwrap();
        let tree = tracked_tree(&f.workspace);
        let tracked = javascript_tracked_tree(&f.workspace, &tree).unwrap();
        let install = &discover_javascript_installs(&tracked.paths).unwrap()[0];
        let mut command = javascript_install_command(&f.workspace, install).unwrap();
        // Reuse one scratch Bun cache for both tarball versions to reproduce
        // path-keyed stale hits without writing to the operator's personal cache.
        command.env("BUN_INSTALL_CACHE_DIR", f.scratch.join("shared-bun-cache"));
        let _ = fs::remove_dir_all(f.workspace.join("node_modules"));
        cache_javascript_install(
            &f.state,
            &f.workspace.to_string_lossy(),
            &f.workspace,
            &tracked,
            install,
            &mut command,
        )
        .unwrap();
        let installed: serde_json::Value = serde_json::from_slice(
            &fs::read(f.workspace.join("node_modules/local-producer/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            installed["marker"], marker,
            "same-path tarball must install its current bytes"
        );
    }
}

#[test]
fn javascript_manager_cache_reuses_registry_inputs_but_scopes_local_content() {
    for local in [false, true] {
        let f = Fixture::new();
        write_js_pair(&f.workspace, "", "package-lock.json");
        if local {
            write_local_package_fixture(&f.workspace);
            fs::write(
                f.workspace.join("package.json"),
                r#"{"dependencies":{"fixture-dep":"file:vendor/fixture-dep"}}"#,
            )
            .unwrap();
        }
        let manager = FakeJavascriptManager::new(&f);
        let tree = tracked_tree(&f.workspace);
        manager.prepare(&f, &tree).unwrap();
        if local {
            fs::write(
                f.workspace.join("vendor/fixture-dep/index.js"),
                "changed producer bytes",
            )
            .unwrap();
        } else {
            fs::write(
                f.workspace.join("package-lock.json"),
                "changed registry lockfile",
            )
            .unwrap();
        }
        let changed = tracked_tree(&f.workspace);
        manager.prepare(&f, &changed).unwrap();
        assert_eq!(manager.count(), 2);
        assert_eq!(
            fs::read_dir(f.state.join("js-manager-caches"))
                .unwrap()
                .count(),
            if local { 2 } else { 1 }
        );
    }
}
