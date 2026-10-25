use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
};

#[derive(Serialize, Deserialize)]
struct Snapshot {
    mirror: PathBuf,
    commit: String,
    destination: String,
}
#[derive(Serialize, Deserialize)]
struct Plan {
    staging: PathBuf,
    snapshots: Vec<Snapshot>,
    routes: BTreeMap<String, PathBuf>,
    workspace: String,
    tree_hash: String,
    #[cfg(any(unix, test))]
    cargo: PathBuf,
    #[cfg(any(unix, test))]
    rustup: PathBuf,
    #[cfg(any(unix, test))]
    toolchain_bin: PathBuf,
    #[cfg(any(unix, test))]
    toolchain_source: PathBuf,
}
fn mapped(path: &str) -> io::Result<PathBuf> {
    motor_winagent::mapped_cwd(path)?;
    Ok(PathBuf::from("m").join(path.trim_start_matches('/')))
}

pub(crate) fn error_at_path(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

pub(super) fn prepare(
    config: &Config,
    trees: &Trees,
    setup: &Setup,
    staging: &Path,
) -> io::Result<PathBuf> {
    #[cfg(windows)]
    super::ensure_parent_guard(config)?;
    let request = &setup.request;
    mapped(&request.workspace_key)?;
    mapped(&request.cwd)?;
    if !Path::new(&request.cwd).starts_with(&request.workspace_key) {
        return Err(invalid(format!("cwd outside snapshot: {}", request.cwd)));
    }
    let lockfile = crate::workspace::git(
        &trees.mirror(&request.repository_root),
        &["show", &format!("{}:Cargo.lock", request.snapshot_commit)],
    )
    .map_err(|_| {
        invalid(format!(
            "snapshot has no Cargo.lock: {}",
            request.workspace_key
        ))
    })?;
    let (routes, missing) = trees.dependency_fetch_routes(&request.git_dependencies, &lockfile)?;
    if !missing.is_empty() {
        return Err(invalid(missing.join("; ")));
    }
    validate_lock(&lockfile, &routes)?;
    let locks: Vec<_> = routes
        .values()
        .map(|path| trees.dependency_lock(path))
        .collect();
    let _guards: Vec<_> = locks.iter().map(|lock| lock.lock().unwrap()).collect();
    trees.refresh_dependency_aliases(&lockfile, &routes)?;
    let mut snapshots = vec![Snapshot {
        mirror: trees.mirror(&request.repository_root),
        commit: request.snapshot_commit.clone(),
        destination: request.workspace_key.clone(),
    }];
    for sibling in &request.siblings {
        mapped(&sibling.canonical_path)?;
        snapshots.push(Snapshot {
            mirror: trees.mirror(&sibling.canonical_path),
            commit: sibling.commit.clone(),
            destination: sibling.canonical_path.clone(),
        });
    }
    #[cfg(unix)]
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| invalid("HOME is unset"))?;
    #[cfg(unix)]
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    // The dedicated preparation account cannot traverse the runner's home.
    // Resolve the installed toolchain and give the unit read-only access to it.
    #[cfg(unix)]
    let rustup = cargo.join("bin/rustup");
    #[cfg(unix)]
    let toolchain = command(
        config,
        &rustup.to_string_lossy(),
        &["which".into(), "cargo".into()],
    )
    .map_err(|error| super::vm::path_error("locate preparation toolchain with", &rustup, error))?;
    #[cfg(unix)]
    if !toolchain.status.success() {
        return Err(invalid("cannot locate trusted preparation toolchain"));
    }
    #[cfg(unix)]
    let cargo_executable =
        PathBuf::from(String::from_utf8(toolchain.stdout).map_err(invalid)?.trim());
    #[cfg(unix)]
    let toolchain_bin = cargo_executable
        .parent()
        .ok_or_else(|| invalid("invalid cargo toolchain path"))?
        .to_path_buf();
    #[cfg(unix)]
    let plan = Plan {
        staging: staging.into(),
        snapshots,
        routes,
        workspace: request.workspace_key.clone(),
        tree_hash: request.tree_hash.clone(),
        cargo: "/opt/motor-prep-toolchain/bin/cargo".into(),
        rustup: "/opt/motor-prep-toolchain".into(),
        toolchain_source: toolchain_bin.parent().unwrap().to_path_buf(),
        toolchain_bin: "/opt/motor-prep-toolchain/bin".into(),
    };
    #[cfg(windows)]
    let plan = {
        #[cfg(test)]
        let toolchain = config
            .windows
            .as_ref()
            .unwrap()
            .layout_root
            .join("prep-toolchain");
        let routes = super::trusted_prep::copy_routes(staging, &routes)?;
        Plan {
            staging: staging.into(),
            snapshots,
            routes,
            workspace: request.workspace_key.clone(),
            tree_hash: request.tree_hash.clone(),
            #[cfg(test)]
            cargo: toolchain.join("bin/cargo.exe"),
            #[cfg(test)]
            rustup: toolchain.clone(),
            #[cfg(test)]
            toolchain_bin: toolchain.join("bin"),
            #[cfg(test)]
            toolchain_source: toolchain,
        }
    };
    let plan_path = staging.join("prepare.json");
    atomic_json(&plan_path, &plan)
        .map_err(|error| super::vm::path_error("write preparation plan", &plan_path, error))?;
    #[cfg(unix)]
    super::vm::acl(config, "u:motor-prep:r", &plan_path)?;
    #[cfg(unix)]
    {
        #[cfg(test)]
        let executable = match &config.windows_prep_executable {
            Some(path) => path.clone(),
            None => std::env::current_exe()?,
        };
        #[cfg(not(test))]
        let executable = std::env::current_exe()?;
        let args = prep_args(config, setup.id, &plan, &plan_path, &executable);
        let output = run_preparation(config, setup.id, &args).map_err(|error| {
            super::vm::path_error("run trusted preparation for", staging, error)
        })?;
        super::prep_error::log_warnings(&output.stderr);
        if !output.status.success() {
            return Err(super::prep_error::refused(&output));
        }
        super::vm::restore_staging_access(config, staging)?;
    }
    #[cfg(windows)]
    execute_with_registry(&plan_path, |job_staging, _workspace, lockfile, routes| {
        super::trusted_prep::prepare_local_registry(config, job_staging, lockfile, routes)
    })?;
    let tar = staging.join("tree.tar");
    let mut file = File::open(&tar)
        .map_err(|error| super::vm::path_error("open trusted preparation tree", &tar, error))?;
    motor_winagent::validate_tree(&mut file)
        .map_err(|error| super::vm::path_error("validate trusted preparation tree", &tar, error))?;
    Ok(tar)
}

#[cfg(unix)]
fn prep_args(
    config: &Config,
    id: Uuid,
    plan: &Plan,
    plan_path: &Path,
    executable: &Path,
) -> Vec<String> {
    vec![
        "-n".into(),
        "/usr/bin/systemd-run".into(),
        "--wait".into(),
        "--pipe".into(),
        "--collect".into(),
        format!("--unit=motor-windows-{}-prep-{id}.service", config.instance),
        crate::JOB_MEMORY_PROPERTIES[0].into(),
        crate::JOB_MEMORY_PROPERTIES[1].into(),
        crate::JOB_MEMORY_PROPERTIES[2].into(),
        "--property=User=motor-prep".into(),
        "--property=ProtectSystem=strict".into(),
        "--property=ProtectHome=yes".into(),
        "--property=NoNewPrivileges=yes".into(),
        "--property=PrivateTmp=yes".into(),
        "--property=KillMode=control-group".into(),
        "--property=TimeoutStopSec=5s".into(),
        "--property=TasksMax=256".into(),
        "--property=MemoryMax=2G".into(),
        "--property=RuntimeMaxSec=600".into(),
        format!("--property=ReadWritePaths={}", plan.staging.display()),
        format!(
            "--property=BindReadOnlyPaths={}:/opt/motor-prep-toolchain {}:/opt/motor-prep-runner",
            plan.toolchain_source.display(),
            executable.display()
        ),
        "--".into(),
        "/usr/bin/env".into(),
        "-i".into(),
        "PATH=/usr/bin:/bin".into(),
        "/opt/motor-prep-runner".into(),
        "windows-prepare".into(),
        plan_path.to_string_lossy().into_owned(),
    ]
}

#[cfg(unix)]
fn run_preparation(config: &Config, id: Uuid, args: &[String]) -> io::Result<Output> {
    #[cfg(test)]
    if config.windows_command.is_some() {
        return command(config, "/usr/bin/sudo", args);
    }
    #[cfg(target_os = "linux")]
    {
        use std::{io::Read, process::Stdio, time::Duration};
        let mut child = Command::new("/usr/bin/sudo")
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stderr = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0; 8192];
            while let Ok(n) = stderr.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                captured.extend_from_slice(&buffer[..n.min(65536 - captured.len())]);
            }
            captured
        });
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if config.windows_runtime.setup_aborted(id) {
                let unit = format!("motor-windows-{}-prep-{id}.service", config.instance);
                let _ = systemctl(config, &["kill", "--kill-whom=all", "--signal=KILL", &unit]);
                let _ = systemctl(config, &["stop", &unit]);
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(invalid("Windows trusted preparation cancelled"));
            }
            std::thread::park_timeout(Duration::from_millis(10));
        };
        Ok(Output {
            status,
            stdout: Vec::new(),
            stderr: reader
                .join()
                .map_err(|_| invalid("Windows preparation log reader stopped"))?,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (config, id, args);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows trusted preparation requires Linux",
        ))
    }
}

pub(super) fn validate_git_source_url(url: &str) -> io::Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|_| invalid(format!("unsupported Git bundle source URL: {url}")))?;
    if !matches!(parsed.scheme(), "https" | "http")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(invalid(format!("unsupported Git bundle source URL: {url}")));
    }
    Ok(())
}

#[cfg(any(windows, test))]
pub(super) fn local_registry_source_config(
    local_registry: &Path,
    source_name: &str,
) -> io::Result<String> {
    if !local_registry.is_absolute()
        || local_registry
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid(format!(
            "local Cargo registry path must be absolute and normalized: {}",
            local_registry.display()
        )));
    }
    let path = local_registry
        .to_str()
        .ok_or_else(|| invalid("local Cargo registry path is not UTF-8"))?
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    Ok(format!(
        "[source.crates-io]\nreplace-with = \"{source_name}\"\n\n[source.{source_name}]\nlocal-registry = \"{path}\"\n"
    ))
}

fn validate_lock(lock: &str, routes: &BTreeMap<String, PathBuf>) -> io::Result<()> {
    for url in routes.keys() {
        validate_git_source_url(url)?;
    }
    let value: toml::Value = toml::from_str(lock).map_err(invalid)?;
    for package in value
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(source) = package.get("source").and_then(toml::Value::as_str) {
            if source == "registry+https://github.com/rust-lang/crates.io-index"
                || source == "sparse+https://index.crates.io/"
            {
                continue;
            }
            if let Some(git) = source.strip_prefix("git+") {
                let url = git.split(['?', '#']).next().unwrap_or(git);
                if routes.contains_key(url) {
                    validate_git_source_url(url)?;
                    continue;
                }
            }
            return Err(invalid(format!(
                "unsupported or undeclared dependency source: {source}"
            )));
        }
    }
    Ok(())
}

fn git(snapshot: &Snapshot, args: &[&str]) -> io::Result<std::process::Output> {
    let mut command = Command::new(if cfg!(windows) {
        r"C:\Program Files\Git\cmd\git.exe"
    } else {
        "/usr/bin/git"
    });
    command
        .env_clear()
        .env(
            "PATH",
            if cfg!(windows) {
                r"C:\Windows\System32;C:\Program Files\Git\cmd"
            } else {
                "/usr/bin:/bin"
            },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .arg("-c")
        .arg(format!("safe.directory={}", snapshot.mirror.display()))
        .arg("--git-dir")
        .arg(&snapshot.mirror)
        .args(args);
    #[cfg(windows)]
    command.env("SYSTEMROOT", "C:\\Windows");
    let output = command.output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output)
}

/// Entry point for the credential-free motor-prep transient unit. Only the
/// runner constructs its plan; the job's command is never evaluated on the host.
#[cfg(any(unix, test))]
pub(crate) fn execute(plan_path: &Path) -> io::Result<()> {
    execute_with_fetch(plan_path, |cmd| {
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(invalid(String::from_utf8_lossy(&output.stderr)));
        }
        Ok(())
    })
}

#[cfg(any(unix, test))]
fn cargo_command(plan: &Plan, manifest: &Path, args: &[&str]) -> Command {
    // Cargo reads repository configuration from its working directory's ancestors,
    // not from the manifest path, so trusted commands run outside extracted trees.
    let mut cmd = Command::new(&plan.cargo);
    cmd.env_clear()
        .env(
            "PATH",
            if cfg!(windows) {
                format!(
                    "{};C:\\Windows\\System32;C:\\Program Files\\Git\\cmd",
                    plan.toolchain_bin.display()
                )
            } else {
                format!("{}:/usr/bin:/bin", plan.toolchain_bin.display())
            },
        )
        .env("CARGO_HOME", plan.staging.join("cargo-home"))
        .env("RUSTUP_HOME", &plan.rustup)
        .args(args)
        .arg("--manifest-path")
        .arg(manifest)
        .current_dir(&plan.staging);
    cmd
}

fn stage_snapshots(plan: &Plan) -> io::Result<(Vec<PathBuf>, BTreeSet<String>, BTreeSet<String>)> {
    let mut roots = Vec::new();
    let mut cases = BTreeMap::new();
    let mut members = BTreeSet::new();
    let mut regular_files = BTreeSet::new();
    for (index, snapshot) in plan.snapshots.iter().enumerate() {
        let root = plan.staging.join(mapped(&snapshot.destination)?);
        if roots
            .iter()
            .any(|other: &PathBuf| root.starts_with(other) || other.starts_with(&root))
        {
            return Err(invalid(format!(
                "overlapping snapshot: {}",
                snapshot.destination
            )));
        }
        if index == 0 {
            let tree = git(
                snapshot,
                &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)],
            )?;
            let computed = String::from_utf8_lossy(&tree.stdout);
            if computed.trim() != plan.tree_hash {
                return Err(Trees::tree_hash_mismatch(
                    &snapshot.mirror,
                    &plan.tree_hash,
                    computed.trim(),
                ));
            }
        }
        fs::create_dir_all(&root)?;
        let archive = git(
            snapshot,
            &[
                "archive",
                "--format=tar",
                &format!("{}^{{tree}}", snapshot.commit),
            ],
        )?;
        for entry in tar::Archive::new(io::Cursor::new(archive.stdout)).entries()? {
            let mut entry = entry?;
            let name = entry
                .path()?
                .to_str()
                .ok_or_else(|| invalid("non-UTF8 snapshot path"))?
                .trim_end_matches('/')
                .to_owned();
            let transferred = format!("{}/{}", mapped(&snapshot.destination)?.display(), name);
            #[cfg(windows)]
            let transferred = transferred.replace('\\', "/");
            motor_winagent::validate_archive_path(&transferred)?;
            check_case(&transferred, &mut cases)?;
            if !members.insert(transferred.clone()) {
                return Err(invalid(format!("duplicate snapshot path: {transferred}")));
            }
            let kind = entry.header().entry_type();
            if kind.is_symlink() {
                // Git for Windows checks out symlinks as files by default. Stage the
                // target text instead, so transferred snapshots contain no links.
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| invalid(format!("snapshot link without target: {transferred}")))?
                    .into_owned();
                let path = root.join(&name);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?
                    .write_all(&target)?;
                continue;
            }
            if !kind.is_file() && !kind.is_dir() {
                return Err(invalid(format!(
                    "snapshot special file: {}/{name}",
                    snapshot.destination
                )));
            }
            if kind.is_file() {
                regular_files.insert(transferred);
            }
            entry.unpack_in(&root)?;
        }
        roots.push(root);
    }
    Ok((roots, members, regular_files))
}

#[cfg(any(unix, windows, test))]
fn validate_staged_manifests(
    plan: &Plan,
    roots: &[PathBuf],
    regular_files: &BTreeSet<String>,
) -> io::Result<PathBuf> {
    let workspace = plan.staging.join(mapped(&plan.workspace)?);
    let root_lockfile = format!("{}/Cargo.lock", mapped(&plan.workspace)?.display());
    #[cfg(windows)]
    let root_lockfile = root_lockfile.replace('\\', "/");
    if !workspace.join("Cargo.lock").is_file()
        || (!plan.snapshots.is_empty() && !regular_files.contains(&root_lockfile))
    {
        return Err(invalid(format!(
            "snapshot has no Cargo.lock: {}",
            plan.workspace
        )));
    }
    for root in roots {
        validate_manifests(root, roots, &plan.routes)?;
    }
    fs::create_dir_all(plan.staging.join("cargo-home"))?;
    fs::create_dir_all(plan.staging.join("home"))?;
    fs::create_dir_all(plan.staging.join("fetch-temp"))?;
    Ok(workspace)
}

#[cfg(any(unix, test))]
pub(crate) fn execute_with_fetch(
    plan_path: &Path,
    mut fetch: impl FnMut(&mut Command) -> io::Result<()>,
) -> io::Result<()> {
    let plan: Plan = read_json(plan_path)?;
    let (roots, members, regular_files) =
        super::prep_error::step("extract snapshots", &plan.staging, || {
            stage_snapshots(&plan)
        })?;
    let workspace = super::prep_error::step("validate manifests", &plan.staging, || {
        validate_staged_manifests(&plan, &roots, &regular_files)
    })?;
    super::prep_error::step("fetch dependencies", &plan.staging, || {
        fetch_manifest(&plan, &workspace.join("Cargo.toml"), &mut fetch)
    })?;
    let root_lockfile = format!("{}/Cargo.lock", mapped(&plan.workspace)?.display());
    #[cfg(windows)]
    let root_lockfile = root_lockfile.replace('\\', "/");
    for lockfile_name in members
        .iter()
        .filter(|name| name.ends_with("/Cargo.lock") && *name != &root_lockfile)
    {
        let manifest_name = format!(
            "{}Cargo.toml",
            lockfile_name.strip_suffix("Cargo.lock").unwrap()
        );
        let lockfile = plan.staging.join(lockfile_name);
        let result =
            if !regular_files.contains(lockfile_name) || !regular_files.contains(&manifest_name) {
                Err(invalid(
                    "nested lockfile and manifest must both be tracked regular files",
                ))
            } else {
                fetch_manifest(&plan, &plan.staging.join(manifest_name), &mut fetch)
            };
        if result.is_err() {
            eprintln!(
                "{}",
                super::prep_error::nested_warning(
                    &plan.staging,
                    &lockfile,
                    invalid("nested Cargo fetch failed"),
                )
            );
        }
    }
    super::prep_error::step("stage dependencies", &plan.staging, || {
        build_tar(&plan.staging)
    })
}

#[cfg(any(unix, test))]
fn fetch_manifest(
    plan: &Plan,
    manifest: &Path,
    fetch: &mut impl FnMut(&mut Command) -> io::Result<()>,
) -> io::Result<()> {
    let mut command = cargo_command(
        plan,
        manifest,
        &[
            "fetch",
            "--locked",
            "--target",
            "x86_64-pc-windows-msvc",
            "--target",
            "aarch64-pc-windows-msvc",
        ],
    );
    command.args(["--config", "net.git-fetch-with-cli=true"]);
    crate::workspace::dependencies::configure_fetch_git(
        &mut command,
        &plan.staging.join("home"),
        &plan.routes,
    )?;
    let mut count = plan.routes.len();
    for mirror in plan.routes.values() {
        command
            .env(format!("GIT_CONFIG_KEY_{count}"), "safe.directory")
            .env(format!("GIT_CONFIG_VALUE_{count}"), mirror);
        count += 1;
    }
    command.env("GIT_CONFIG_COUNT", count.to_string());
    fetch(&mut command)
}

#[cfg(any(windows, test))]
fn combined_registry_lockfile(
    plan: &Plan,
    members: &BTreeSet<String>,
    regular_files: &BTreeSet<String>,
) -> io::Result<String> {
    let root_lockfile = format!("{}/Cargo.lock", mapped(&plan.workspace)?.display());
    #[cfg(windows)]
    let root_lockfile = root_lockfile.replace('\\', "/");
    if !regular_files.contains(&root_lockfile) {
        return Err(invalid("root Cargo.lock is not a tracked regular file"));
    }

    let mut packages = Vec::new();
    for name in members.iter().filter(|name| name.ends_with("/Cargo.lock")) {
        if name != &root_lockfile {
            if !regular_files.contains(name) {
                continue;
            }
            let manifest = format!("{}Cargo.toml", name.strip_suffix("Cargo.lock").unwrap());
            if !regular_files.contains(&manifest) {
                continue;
            }
        }
        let contents = fs::read_to_string(plan.staging.join(name))?;
        let parsed: toml::Value = match toml::from_str(&contents) {
            Ok(value) => value,
            Err(error) if name != &root_lockfile => {
                tracing::warn!(lockfile = %name, %error, "skipping invalid nested Cargo.lock");
                continue;
            }
            Err(error) => return Err(invalid(format!("root Cargo.lock: {error}"))),
        };
        let Some(entries) = parsed.get("package").and_then(toml::Value::as_array) else {
            if name == &root_lockfile {
                return Err(invalid("root Cargo.lock has no package entries"));
            }
            tracing::warn!(lockfile = %name, "skipping nested Cargo.lock without package entries");
            continue;
        };
        packages.extend(entries.iter().cloned());
    }

    let mut merged = String::from("version = 4\n");
    for package in packages {
        merged.push_str("\n[[package]]\n");
        for key in ["name", "version", "source", "checksum"] {
            if let Some(value) = package.get(key).and_then(toml::Value::as_str) {
                merged.push_str(key);
                merged.push_str(" = ");
                merged.push_str(&serde_json::to_string(value).map_err(invalid)?);
                merged.push('\n');
            }
        }
    }
    Ok(merged)
}

#[cfg(any(windows, test))]
pub(crate) fn execute_with_registry(
    plan_path: &Path,
    prepare_registry: impl FnOnce(&Path, &Path, &str, &BTreeMap<String, PathBuf>) -> io::Result<()>,
) -> io::Result<()> {
    let plan: Plan = read_json(plan_path)?;
    let (roots, members, regular_files) =
        super::prep_error::step("extract snapshots", &plan.staging, || {
            stage_snapshots(&plan)
        })?;
    let workspace = super::prep_error::step("validate manifests", &plan.staging, || {
        let workspace = validate_staged_manifests(&plan, &roots, &regular_files)?;
        validate_repository_cargo_sources(&plan.staging)?;
        Ok(workspace)
    })?;
    super::prep_error::step("stage dependencies", &plan.staging, || {
        let lockfile = combined_registry_lockfile(&plan, &members, &regular_files)?;
        prepare_registry(&plan.staging, &workspace, &lockfile, &plan.routes)?;
        build_tar(&plan.staging)
    })
}

fn check_case(path: &str, cases: &mut BTreeMap<String, String>) -> io::Result<()> {
    let mut prefix = String::new();
    for component in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        if let Some(original) = cases.insert(prefix.to_lowercase(), prefix.clone()) {
            if original != prefix {
                return Err(invalid(format!("case collision: {prefix} and {original}")));
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn validate_repository_cargo_sources(staging: &Path) -> io::Result<()> {
    let mut directories = vec![staging.to_path_buf()];
    while let Some(directory) = directories.pop() {
        crate::filesystem::identity(&directory)?;
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_dir() {
                continue;
            }
            crate::filesystem::identity(&path)?;
            if path.file_name().is_some_and(|name| name == ".cargo") {
                for name in ["config", "config.toml"] {
                    let config_path = path.join(name);
                    let metadata = match fs::symlink_metadata(&config_path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    };
                    if !metadata.is_file() {
                        return Err(invalid(format!(
                            "Cargo config is not a regular file: {}",
                            config_path.display()
                        )));
                    }
                    crate::filesystem::identity(&config_path)?;
                    let bytes = fs::read_to_string(&config_path).map_err(|error| {
                        invalid(format!(
                            "cannot read Cargo config {}: {error}",
                            config_path.display()
                        ))
                    })?;
                    let config: toml::Value = toml::from_str(&bytes).map_err(|error| {
                        invalid(format!(
                            "invalid Cargo config {}: {error}",
                            config_path.display()
                        ))
                    })?;
                    for key in ["source", "registry", "registries"] {
                        if let Some(value) = config.get(key) {
                            let names = value
                                .as_table()
                                .map(|table| table.keys().cloned().collect::<Vec<_>>().join(", "))
                                .filter(|names| !names.is_empty())
                                .unwrap_or_else(|| "invalid table".into());
                            return Err(invalid(format!(
                                "unsupported repository Cargo {key} configuration in {}: {names}",
                                config_path.display()
                            )));
                        }
                    }
                }
            }
            directories.push(path);
        }
    }
    Ok(())
}

fn validate_manifests(
    root: &Path,
    roots: &[PathBuf],
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            validate_manifests(&path, roots, routes)?;
        } else if path.file_name().is_some_and(|n| n == "Cargo.toml") {
            let value: toml::Value =
                toml::from_str(&fs::read_to_string(&path)?).map_err(invalid)?;
            validate_dependencies(&value, path.parent().unwrap(), roots, routes)?;
        }
    }
    Ok(())
}
fn validate_dependencies(
    value: &toml::Value,
    manifest: &Path,
    roots: &[PathBuf],
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    if let Some(table) = value.as_table() {
        for (key, value) in table {
            if matches!(
                key.as_str(),
                "dependencies" | "dev-dependencies" | "build-dependencies" | "patch" | "replace"
            ) {
                check_sources(value, manifest, roots, routes)?;
            } else {
                validate_dependencies(value, manifest, roots, routes)?;
            }
        }
    }
    Ok(())
}
fn check_sources(
    value: &toml::Value,
    manifest: &Path,
    roots: &[PathBuf],
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    if let Some(table) = value.as_table() {
        if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
            let path = manifest.join(path);
            let resolved = path
                .canonicalize()
                .map_err(|_| invalid(format!("missing path dependency: {}", path.display())))?;
            #[cfg(unix)]
            let inside = roots.iter().any(|root| resolved.starts_with(root));
            #[cfg(windows)]
            let inside = roots.iter().any(|root| {
                root.canonicalize()
                    .is_ok_and(|root| resolved.starts_with(root))
            });
            if !inside {
                return Err(invalid(format!(
                    "outside path dependency: {}",
                    path.display()
                )));
            }
        }
        if let Some(url) = table.get("git").and_then(toml::Value::as_str) {
            if !routes.contains_key(url) {
                return Err(invalid(format!(
                    "unsupported or undeclared git dependency: {url}"
                )));
            }
        }
        if let Some(registry) = table.get("registry").and_then(toml::Value::as_str) {
            if registry != "crates-io" {
                return Err(invalid(format!("off-registry dependency: {registry}")));
            }
        }
        for value in table.values() {
            check_sources(value, manifest, roots, routes)?;
        }
    }
    Ok(())
}

pub(super) fn build_tar(staging: &Path) -> io::Result<()> {
    let mut builder = tar::Builder::new(File::create(staging.join("tree.tar"))?);
    let mut seen = BTreeSet::new();
    append(&mut builder, staging, &staging.join("m"), &mut seen)?;
    for name in [
        "config.toml",
        "gitconfig",
        "git-rewrites.json",
        "local-registry",
        "git-mirrors",
        "registry/index",
        "registry/cache",
        "git/db",
        "git/checkouts",
    ] {
        let path = staging.join("cargo-home").join(name);
        if path.exists() {
            append(&mut builder, staging, &path, &mut seen)?;
        }
    }
    builder.finish()?;
    drop(builder);
    motor_winagent::validate_tree(&mut File::open(staging.join("tree.tar"))?)
}
fn append(
    builder: &mut tar::Builder<File>,
    staging: &Path,
    path: &Path,
    seen: &mut BTreeSet<String>,
) -> io::Result<()> {
    let relative = path.strip_prefix(staging).map_err(invalid)?;
    let name = relative
        .to_str()
        .ok_or_else(|| invalid(format!("non-UTF8 path: {}", path.display())))?;
    #[cfg(windows)]
    let name = name.replace('\\', "/");
    #[cfg(windows)]
    let name = name.as_str();
    motor_winagent::validate_archive_path(name)?;
    if !seen.insert(name.to_lowercase()) {
        return Err(invalid(format!("case collision: {name}")));
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        builder.append_dir(relative, path)?;
        let mut entries = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            append(builder, staging, &entry, seen)?;
        }
    } else if metadata.is_file() {
        builder.append_file(relative, &mut File::open(path)?)?;
    } else {
        return Err(invalid(format!("link or special file: {}", path.display())));
    }
    Ok(())
}

pub(crate) fn store_collection(
    config: &Config,
    id: Uuid,
    bytes: &[u8],
    deleted: &[String],
) -> io::Result<()> {
    motor_winagent::validate_collection(&mut io::Cursor::new(bytes))?;
    for path in deleted {
        motor_winagent::validate_archive_path(path)?;
        if !path.starts_with("m/") {
            return Err(invalid(format!(
                "deleted member is not a source path: {path}"
            )));
        }
    }
    let mut paths = deleted.to_vec();
    for entry in tar::Archive::new(io::Cursor::new(bytes)).entries()? {
        let entry = entry?;
        let path = entry
            .path()?
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 collect path"))?
            .to_owned();
        #[cfg(windows)]
        let path = path.replace('\\', "/");
        if !path.starts_with("m/") {
            return Err(invalid(format!(
                "collect member is not a source path: {path}"
            )));
        }
        if entry.header().entry_type().is_file() || entry.header().entry_type().is_symlink() {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    let job = config.state_dir().join("jobs").join(id.to_string());
    fs::create_dir_all(&job)
        .map_err(|error| super::vm::path_error("create collection directory", &job, error))?;
    let tar_path = job.join("windows-changes.tar");
    fs::write(&tar_path, bytes)
        .map_err(|error| super::vm::path_error("write collection tree", &tar_path, error))?;
    let manifest_path = job.join("windows-changes.json");
    atomic_json(&manifest_path, &paths)
        .map_err(|error| super::vm::path_error("write collection manifest", &manifest_path, error))
}

#[cfg(test)]
mod trusted_cargo_config_tests {
    #[test]
    fn collected_links_are_only_inert_tar_bytes_on_the_host() {
        let root = ScratchDir::new("windows-collected-links");
        let config = Config::new(root.path().join("state"), "links");
        let id = Uuid::now_v7();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, "m/escape", "/etc/shadow")
            .unwrap();
        let bytes = builder.into_inner().unwrap();
        super::store_collection(&config, id, &bytes, &["m/deleted".into()]).unwrap();
        let job = config.state_dir().join("jobs").join(id.to_string());
        assert_eq!(
            std::fs::read(job.join("windows-changes.tar")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read_dir(&job).unwrap().count(), 2);
        assert!(!job.join("m").exists());
        let manifest: Vec<String> = read_json(&job.join("windows-changes.json")).unwrap();
        assert_eq!(manifest, ["m/deleted", "m/escape"]);
        for path in ["../outside", "cargo-home/escape", "m/../escape", "m/a\\b"] {
            assert!(
                super::store_collection(&config, Uuid::now_v7(), &bytes, &[path.into()]).is_err()
            );
        }
    }
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn cargo_metadata_ignores_repository_config_from_the_staging_directory() {
        let scratch = ScratchDir::new("windows-prep-cargo-config");
        let staging = scratch.path().join("staging");
        let repository = staging.join("m/repository");
        let cargo_home = staging.join("cargo-home");
        let sentinel = staging.join("sentinel-target");
        fs::create_dir_all(repository.join("src")).unwrap();
        fs::create_dir_all(repository.join(".cargo")).unwrap();
        fs::create_dir_all(&cargo_home).unwrap();
        fs::write(
            repository.join("Cargo.toml"),
            "[workspace]\n[package]\nname='config-fixture'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(repository.join("src/lib.rs"), "").unwrap();
        fs::write(
            repository.join(".cargo/config.toml"),
            format!(
                "[build]\ntarget-dir = {:?}\n[source.crates-io]\nreplace-with = 'evil'\n[source.evil]\nregistry = 'sparse+https://example.invalid/'\n",
                sentinel.to_string_lossy()
            ),
        )
        .unwrap();

        let cargo = PathBuf::from(env!("CARGO"));
        let plan = Plan {
            staging: staging.clone(),
            snapshots: Vec::new(),
            routes: BTreeMap::new(),
            workspace: "m/repository".into(),
            tree_hash: String::new(),
            cargo: cargo.clone(),
            rustup: std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup"))
                })
                .expect("test toolchain has a Rustup home"),
            toolchain_bin: cargo.parent().unwrap().to_path_buf(),
            toolchain_source: PathBuf::new(),
        };
        let output = cargo_command(
            &plan,
            &repository.join("Cargo.toml"),
            &[
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--offline",
            ],
        )
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            metadata["target_directory"].as_str(),
            Some(repository.join("target").to_str().unwrap()),
            "trusted Cargo must use the repository's default target directory"
        );
        assert_ne!(metadata["target_directory"].as_str(), sentinel.to_str());
    }
}

#[cfg(test)]
mod git_source_validation_tests {
    use super::*;

    fn lock_with_source(source: &str) -> String {
        format!(
            "[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\nsource = \"git+{source}?rev=main#0123456789012345678901234567890123456789\"\n"
        )
    }

    #[test]
    fn valid_http_git_bundle_source_is_accepted() {
        let url = "https://example.invalid/fixture.git";
        let routes = BTreeMap::from([(url.to_owned(), PathBuf::from("git-mirrors/fixture"))]);
        assert!(validate_lock(&lock_with_source(url), &routes).is_ok());
    }

    #[test]
    fn unsupported_git_schemes_and_credentials_fail_before_fetch() {
        for url in [
            "ssh://example.invalid/fixture.git",
            "file:///tmp/fixture.git",
            "https://user:secret@example.invalid/fixture.git",
        ] {
            let routes = BTreeMap::from([(url.to_owned(), PathBuf::from("git-mirrors/fixture"))]);
            let error = validate_lock(&lock_with_source(url), &routes)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(url),
                "Git source error did not name {url}: {error}"
            );
        }
    }

    #[test]
    fn unused_invalid_git_route_is_refused_before_fetch() {
        let url = "https://user:secret@example.invalid/unused.git";
        let routes = BTreeMap::from([(url.to_owned(), PathBuf::from("git-mirrors/unused"))]);
        let error = validate_lock("version = 3\n", &routes)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(url),
            "route error did not name {url}: {error}"
        );
    }
}

#[cfg(test)]
mod repository_cargo_source_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::cell::Cell;

    #[test]
    fn repository_source_override_is_named_and_rejected_before_registry_fetch() {
        let scratch = ScratchDir::new("windows-prep-source-override");
        let staging = scratch.path().join("staging");
        let repository = staging.join("m/repository");
        fs::create_dir_all(repository.join(".cargo")).unwrap();
        fs::write(repository.join("Cargo.lock"), "version = 3\n").unwrap();
        fs::write(
            repository.join(".cargo/config.toml"),
            "[source.crates-io]\nreplace-with = 'private-mirror'\n[source.private-mirror]\nregistry = 'sparse+https://mirror.invalid/index/'\n",
        )
        .unwrap();

        let plan = Plan {
            staging: staging.clone(),
            snapshots: Vec::new(),
            routes: BTreeMap::new(),
            workspace: "/repository".into(),
            tree_hash: String::new(),
            cargo: PathBuf::new(),
            rustup: PathBuf::new(),
            toolchain_bin: PathBuf::new(),
            toolchain_source: PathBuf::new(),
        };
        let plan_path = staging.join("prepare.json");
        atomic_json(&plan_path, &plan).unwrap();
        let registry_called = Cell::new(false);
        let error = execute_with_registry(&plan_path, |_, _, _, _| {
            registry_called.set(true);
            Ok(())
        })
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("unsupported repository Cargo source configuration"),
            "unexpected registry preparation error: {error}"
        );
        assert!(error.contains("crates-io"));
        assert!(error.contains("private-mirror"));
        assert!(
            !registry_called.get(),
            "network fetch ran before config refusal"
        );
    }

    #[test]
    fn harmless_repository_cargo_build_config_is_allowed() {
        let scratch = ScratchDir::new("windows-prep-harmless-cargo-config");
        let staging = scratch.path().join("staging");
        let cargo = staging.join("m/repository/.cargo");
        fs::create_dir_all(&cargo).unwrap();
        fs::write(
            cargo.join("config.toml"),
            "[build]\ntarget-dir = 'target'\n",
        )
        .unwrap();
        validate_repository_cargo_sources(&staging).unwrap();
    }
}

#[cfg(test)]
mod local_registry_source_config_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn written_source_path_is_absolute_and_names_the_registry_directory() {
        let scratch = ScratchDir::new("local-registry-absolute-source");
        let registry = scratch
            .path()
            .join("fetch-temp/local-registry-second-job/local-registry");
        fs::create_dir_all(&registry).unwrap();
        let config = local_registry_source_config(&registry, "motor-local").unwrap();
        let parsed: toml::Value = toml::from_str(&config).unwrap();
        let written_path = parsed["source"]["motor-local"]["local-registry"]
            .as_str()
            .expect("local-registry source path");
        assert!(Path::new(written_path).is_absolute());
        assert_eq!(PathBuf::from(written_path), registry);
    }
}

#[cfg(test)]
mod nested_lockfile_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn lockfile(root: &str, dependency: &str, checksum: &str) -> String {
        format!(
            "version = 4\n\n[[package]]\nname = \"{root}\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"{dependency}\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n"
        )
    }

    #[test]
    fn local_registry_lockfile_includes_nested_workspaces_with_manifests() {
        let scratch = ScratchDir::new("windows-prep-nested-lockfiles");
        let staging = scratch.path().to_path_buf();
        let root = staging.join(mapped("/repository").unwrap());
        let nested = root.join("tools/nested");
        let orphan = root.join("tools/orphan");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&orphan).unwrap();
        let root_lock = root.join("Cargo.lock");
        let root_manifest = root.join("Cargo.toml");
        let nested_lock = nested.join("Cargo.lock");
        let nested_manifest = nested.join("Cargo.toml");
        let orphan_lock = orphan.join("Cargo.lock");
        fs::write(
            &root_lock,
            lockfile("root", "root-dependency", &"a".repeat(64)),
        )
        .unwrap();
        fs::write(&root_manifest, "[package]\nname = \"root\"\n").unwrap();
        fs::write(
            &nested_lock,
            lockfile("nested", "nested-dependency", &"b".repeat(64)),
        )
        .unwrap();
        fs::write(&nested_manifest, "[package]\nname = \"nested\"\n").unwrap();
        fs::write(
            &orphan_lock,
            lockfile("orphan", "orphan-dependency", &"c".repeat(64)),
        )
        .unwrap();

        let root_lock = root_lock.strip_prefix(&staging).unwrap();
        let root_manifest = root_manifest.strip_prefix(&staging).unwrap();
        let nested_lock = nested_lock.strip_prefix(&staging).unwrap();
        let nested_manifest = nested_manifest.strip_prefix(&staging).unwrap();
        let orphan_lock = orphan_lock.strip_prefix(&staging).unwrap();
        let root_lock = root_lock.to_string_lossy().replace('\\', "/");
        let root_manifest = root_manifest.to_string_lossy().replace('\\', "/");
        let nested_lock = nested_lock.to_string_lossy().replace('\\', "/");
        let nested_manifest = nested_manifest.to_string_lossy().replace('\\', "/");
        let members = [
            root_lock.clone(),
            root_manifest.clone(),
            nested_lock.clone(),
            nested_manifest.clone(),
            orphan_lock.to_string_lossy().replace('\\', "/"),
        ]
        .into_iter()
        .collect();
        let regular_files = [
            root_lock,
            root_manifest,
            nested_lock,
            nested_manifest,
            orphan_lock.to_string_lossy().replace('\\', "/"),
        ]
        .into_iter()
        .collect();
        let plan = Plan {
            staging,
            snapshots: Vec::new(),
            routes: BTreeMap::new(),
            workspace: "/repository".into(),
            tree_hash: String::new(),
            cargo: PathBuf::new(),
            rustup: PathBuf::new(),
            toolchain_bin: PathBuf::new(),
            toolchain_source: PathBuf::new(),
        };

        let merged = combined_registry_lockfile(&plan, &members, &regular_files).unwrap();
        let parsed: toml::Value = toml::from_str(&merged).unwrap();
        let packages = parsed["package"].as_array().unwrap();
        let names: BTreeSet<_> = packages
            .iter()
            .filter_map(|package| package["name"].as_str())
            .collect();
        assert!(names.contains("root-dependency"));
        assert!(names.contains("nested-dependency"));
        assert!(!names.contains("orphan-dependency"));
    }
}


#[cfg(test)]
mod path_error_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn not_found_error_names_the_missing_path() {
        let scratch = ScratchDir::new("windows-prep-not-found-path");
        let missing = scratch.join("prep-toolchain/bin/cargo.exe");
        let source = fs::read(&missing).unwrap_err();
        let error = super::error_at_path("read pinned Cargo", &missing, source);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains(&missing.to_string_lossy().to_string()),
            "{error}"
        );
    }
}
