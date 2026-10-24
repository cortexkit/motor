//! Linux jobs are transient system services, not children with the daemon's UID.
//! The existing administrative sudo grant is used only by the trusted daemon;
//! no setuid helper, job sudo grant, or new privileged service is installed.
use crate::remote_cache::transport::{self, Transport};
use crate::{platform, workspace};
use motor_protocol::SiblingSnapshot;
use sha2::Digest;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{FromRawFd, IntoRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, process::CommandExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

mod cargo_fetch;
mod hosts;
mod javascript_tools;
#[cfg(test)]
pub(crate) use javascript_tools::ToolchainRole;
pub(crate) use javascript_tools::{JavascriptTools, JOB_BUN_DIR};
pub(crate) mod pid_init;
mod readable;

const USER: &str = "motor-job";
pub(crate) const WRITER_USER: &str = "motor-cache";
/// The job's /etc/hosts. `localhost` maps to IPv4 only, as on the build server
/// and Ubuntu: a client that tried ::1 first would be refused by servers that
/// listen on 127.0.0.1 alone.
const JOB_HOSTS: &str = "127.0.0.1\tlocalhost\n::1\tip6-localhost\tip6-loopback\n";

fn checked(command: &mut Command) -> io::Result<()> {
    let output = crate::systemd::output(command)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// The job account's numeric identity, which the Boat PID namespace init
/// applies in place of systemd's `User=` and `Group=`.
pub(crate) fn job_identity() -> io::Result<pid_init::JobIdentity> {
    pid_init::JobIdentity::resolve(USER)
}

#[cfg(not(test))]
pub(crate) fn validate_account() -> io::Result<()> {
    validate_identity(
        USER,
        account_uid(USER)?,
        &[],
        account_groups(USER)?.as_deref(),
    )
}

/// Tests run without the provisioned sandbox account. A test that does need
/// the account rules calls `validate_identity` through the writer path, and
/// no test starts a job unit under the account, so only this gate is skipped.
#[cfg(test)]
pub(crate) fn validate_account() -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WriterAccount {
    pub(crate) uid: u32,
}

pub(crate) fn validate_writer_account() -> io::Result<WriterAccount> {
    let uid = account_uid(WRITER_USER)?;
    let job_uid = account_uid(USER)?
        .ok_or_else(|| io::Error::other("provision motor-job before starting the runner"))?;
    validate_identity(
        WRITER_USER,
        uid,
        &[job_uid],
        account_groups(WRITER_USER)?.as_deref(),
    )?;
    Ok(WriterAccount { uid: uid.unwrap() })
}

fn account_uid(user: &str) -> io::Result<Option<u32>> {
    let output = crate::systemd::output(Command::new("/usr/bin/id").args(["-u", user]))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
        .flatten())
}

fn account_groups(user: &str) -> io::Result<Option<String>> {
    let output = crate::systemd::output(Command::new("/usr/bin/id").args(["-nG", user]))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn validate_identity(
    user: &str,
    uid: Option<u32>,
    forbidden: &[u32],
    groups: Option<&str>,
) -> io::Result<()> {
    let uid = uid
        .ok_or_else(|| io::Error::other(format!("provision {user} before starting the runner")))?;
    if uid == 0 || uid == unsafe { libc::geteuid() } || forbidden.contains(&uid) {
        return Err(io::Error::other(format!(
            "{user} must differ from root, the runner and other sandbox accounts"
        )));
    }
    if groups.is_none_or(|groups| {
        groups.trim().is_empty()
            || groups
                .split_whitespace()
                .any(|g| ["sudo", "admin", "adm", "lxd", "docker"].contains(&g))
    }) {
        return Err(io::Error::other(format!(
            "{user} has an administrative or unknown group"
        )));
    }
    Ok(())
}

/// Both access and default ACLs are replaced, not merged: a legacy permissive
/// group or named-user ACL must never give a foreground job cache-write access.
fn writer_cache_acl(directory: bool, runner_write: bool) -> String {
    let access = if directory { "r-x" } else { "r--" };
    let owner = if directory { "rwx" } else { "rw-" };
    let runner_access = if runner_write { owner } else { access };
    let runner = unsafe { libc::geteuid() };
    let acl = format!(
        "u::{owner},u:{USER}:{access},u:{runner}:{runner_access},g::---,m::{runner_access},o::---"
    );
    if directory {
        format!("{acl},d:u::rwx,d:u:{USER}:r-x,d:u:{runner}:r-x,d:g::---,d:m::r-x,d:o::---")
    } else {
        acl
    }
}

pub(crate) fn prepare_private_state(state: &Path) -> io::Result<()> {
    // chmod on an extended ACL only closes its mask. Remove latent group grants
    // before another named-user grant can recompute that mask and reopen them.
    // Default ACLs must not leak access to newly created private runner files.
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args([
                "-b",
                "-k",
                "-m",
                &format!("u:{USER}:--x,g::---,o::---,m::--x"),
                "--",
            ])
            .arg(state),
    )
}

pub(crate) fn prepare_writer_cache(fleet: &Path, account: WriterAccount) -> io::Result<()> {
    let root = fleet
        .parent()
        .ok_or_else(|| io::Error::other("cache has no parent"))?;
    let state = root
        .parent()
        .ok_or_else(|| io::Error::other("cache has no state directory"))?;
    // sccache 0.18 in READ_ONLY mode tries to create `preprocessor/` inside the
    // cache and fails every C compile (cc-rs exit 254) on the read-only job
    // mount when it is missing, so a new or freshly evicted cache needs it from
    // the start rather than after the writer's first fill.
    let preprocessor = fleet.join("preprocessor");
    // Do not repair a symlink into somebody else's tree. Storage is fixed under
    // the instance state directory, never supplied by a job or a repository.
    for directory in [root, fleet, preprocessor.as_path()] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(io::Error::other(
                    "compiler cache must be a physical directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                checked(
                    crate::privilege::command("/usr/bin/mkdir")
                        .args(["-p", "--"])
                        .arg(directory),
                )?;
            }
            Err(error) => return Err(error),
        }
        // Adopt only an empty directory. Existing cache trees were created by
        // the writer; recursively repairing arbitrary old contents would both
        // trust an unknown owner and walk a potentially 200-GiB cache.
        if fs::symlink_metadata(directory)?.uid() != account.uid
            && fs::read_dir(directory)?.next().is_some()
        {
            return Err(io::Error::other(
                "nonempty compiler cache has an unexpected owner",
            ));
        }
        checked(
            crate::privilege::command("/usr/bin/chown")
                .args(["-P", &writer_cache_owner(account), "--"])
                .arg(directory),
        )?;
        repair_writer_cache_path(directory, true)?;
    }
    // Allow the runner to remove entries from the cache parent. Cross-parent
    // eviction also repairs fleet itself immediately before the rename.
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args([
                "-m",
                &format!("u:{}:rwx,m::rwx", unsafe { libc::geteuid() }),
                "--",
            ])
            .arg(root),
    )?;
    // State ancestors need traversal, not list or write access. ACLs on the
    // cache directories themselves give reads, including future writer entries.
    prepare_private_state(state)?;
    for ancestor in state.ancestors() {
        checked(
            crate::privilege::command("/usr/bin/setfacl")
                .args(["-m", &format!("u:{USER}:--x,u:{WRITER_USER}:--x"), "--"])
                .arg(ancestor),
        )?;
    }
    Ok(())
}

fn writer_cache_owner(account: WriterAccount) -> String {
    format!("{}:{WRITER_USER}", account.uid)
}

pub(crate) fn repair_writer_cache_path(path: &Path, directory: bool) -> io::Result<()> {
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-P", "--set", &writer_cache_acl(directory, false), "--"])
            .arg(path),
    )
}

pub(crate) fn retire_writer_cache(fleet: &Path, retired: &Path) -> io::Result<()> {
    let root = fleet
        .parent()
        .ok_or_else(|| io::Error::other("cache has no parent"))?;
    // Linux requires write/search on the source parent and write on a directory
    // moved across parents, to update its '..' entry. Repair both at eviction:
    // older caches lack the parent grant, and chmod can close an ACL's mask.
    // Replace ACLs rather than widening a mask over latent job-write grants;
    // only the runner gains write access, without walking the cache contents.
    for directory in [root, fleet] {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::other(
                "compiler cache must be a physical directory",
            ));
        }
        checked(
            crate::privilege::command("/usr/bin/setfacl")
                .args(["-P", "--set", &writer_cache_acl(true, true), "--"])
                .arg(directory),
        )?;
    }
    fs::rename(fleet, retired)
}

/// Prepare the immutable host trees mounted into jobs. Existing entries get
/// access ACLs once at daemon startup; recursive default ACLs on every Cargo
/// cache directory let trusted Cargo fetches add readable content between jobs
/// without rescanning the shared trees before each launch.
pub(crate) fn prepare_shared_read_access(javascript: &JavascriptTools) -> io::Result<()> {
    prepare_shared_read_access_as(USER, javascript)
}

pub(crate) fn prepare_writer_shared_read_access(javascript: &JavascriptTools) -> io::Result<()> {
    prepare_shared_read_access_as(WRITER_USER, javascript)
}

fn prepare_shared_read_access_as(user: &str, javascript: &JavascriptTools) -> io::Result<()> {
    let cargo_home = PathBuf::from(
        std::env::var_os("CARGO_HOME").ok_or_else(|| io::Error::other("CARGO_HOME unset"))?,
    );
    let rustup_home = PathBuf::from(
        std::env::var_os("RUSTUP_HOME").ok_or_else(|| io::Error::other("RUSTUP_HOME unset"))?,
    );
    prepare_shared_read_access_from_as(&cargo_home, &rustup_home, Some(javascript.bun_dir()), user)
}

#[cfg(test)]
pub(crate) fn prepare_shared_read_access_from(
    cargo_home: &Path,
    rustup_home: &Path,
    bun_bin: Option<&Path>,
) -> io::Result<()> {
    prepare_shared_read_access_from_as(cargo_home, rustup_home, bun_bin, USER)
}

fn prepare_shared_read_access_from_as(
    cargo_home: &Path,
    rustup_home: &Path,
    bun_bin: Option<&Path>,
    user: &str,
) -> io::Result<()> {
    for name in ["registry", "git"] {
        let path = cargo_home.join(name);
        fs::create_dir_all(&path)?;
        grant_read_only_tree_as(&path, true, user)?;
    }
    grant_read_only_tree_as(rustup_home, false, user)?;
    grant_read_only_tree_as(&cargo_home.join("bin"), false, user)?;

    if let Some(bun) = bun_bin {
        if path_is_directory_without_following(bun)? {
            grant_read_only_tree_as(bun, false, user)?;
        }
    }
    Ok(())
}

fn path_is_directory_without_following(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::other(format!(
            "refusing shared read-only symlink: {}",
            path.display()
        ))),
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn grant_read_only_tree_as(path: &Path, inherit: bool, user: &str) -> io::Result<()> {
    let _access = crate::tree_access::lock(path);
    if !path_is_directory_without_following(path)? {
        return Err(io::Error::other(format!(
            "shared read-only directory is missing: {}",
            path.display()
        )));
    }
    let acl = format!("u:{user}:rX");
    // Rewriting every ACL is the slow part of runner startup on a large tree,
    // and on a tenant VM's overlay it copies the whole toolchain into memory.
    // Skip it when the tree already gives this account read access.
    if let Some(uid) = account_uid(user)? {
        if readable::already_readable(path, uid, inherit) {
            tracing::info!(path = %path.display(), user, inherit, "shared tree already readable; ACL grant skipped");
            return Ok(());
        }
    }
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-m", &acl, "--"])
            .arg(path),
    )?;
    if inherit {
        // Every existing directory receives a read-only default ACL. `-P` on
        // find is essential: registry trees may contain untrusted symlinks.
        checked(
            Command::new("/usr/bin/find")
                .arg("-P")
                .arg(path)
                .args(["-type", "d", "-exec"])
                .args(crate::privilege::exec_words("/usr/bin/setfacl"))
                .args(["-P", "-d", "-m", &acl, "--", "{}", "+"]),
        )?;
    }
    Ok(())
}

/// Leaves the job account read and search access to a sibling snapshot tree,
/// and no write access through any ACL entry or mode bit. Unlike
/// `grant_read_only_tree_as`, this always rewrites the ACLs: that function
/// only adds a read grant, keeps any write grant already present, and skips
/// the grant entirely when mode bits allow reading, even on a 0666 entry.
/// Every extended entry, including default ACLs, is removed first, since
/// nothing adds content to a published snapshot. Group and other write bits
/// are then cleared: with no ACL left, the group bits are the owning group's
/// real permissions rather than an ACL mask. Last, the job account gets `rX`
/// and the mask caps every named and group entry at `rX`.
pub(crate) fn restrict_sibling_snapshot_access(path: &Path) -> io::Result<()> {
    if !path_is_directory_without_following(path)? {
        return Err(io::Error::other(format!(
            "sibling snapshot directory is missing: {}",
            path.display()
        )));
    }
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-b", "-k", "--"])
            .arg(path),
    )?;
    crate::sibling_snapshots::store::remove_group_and_other_write(path)?;
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-m", &format!("u:{USER}:rX,m::rX"), "--"])
            .arg(path),
    )
}

fn optional_regular_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::other(format!(
            "refusing JavaScript manifest or lockfile symlink: {}",
            path.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(io::Error::other(format!(
            "JavaScript manifest or lockfile is not a regular file: {}",
            path.display()
        ))),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

const JAVASCRIPT_LOCKFILES: [&str; 4] = [
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "pnpm-lock.yaml",
];

#[derive(Debug, PartialEq, Eq)]
struct JavascriptInstall {
    directory: PathBuf,
    lockfile: &'static str,
}

fn javascript_git(workspace: &Path, args: &[&std::ffi::OsStr]) -> io::Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}

struct JavascriptObject {
    mode: String,
    kind: String,
    id: String,
}

struct JavascriptTree {
    paths: Vec<PathBuf>,
    objects: BTreeMap<PathBuf, JavascriptObject>,
    manifests: BTreeMap<PathBuf, io::Result<serde_json::Value>>,
    /// The path at which the install sees this checkout, when that is not
    /// the checkout's own path: a sibling snapshot built in a temporary
    /// directory is installed as it will be seen, at the sibling's path.
    /// Local dependencies are then resolved against that path, and their
    /// symbolic links inside the snapshot, never on the host at that path.
    logical_root: Option<PathBuf>,
}

fn javascript_tracked_tree(workspace: &Path, tree: &str) -> io::Result<JavascriptTree> {
    use std::os::unix::ffi::OsStringExt;
    // -t includes directory object IDs as well as blobs, so local dependency
    // subtrees need neither another Git traversal nor a process per file.
    let entries = javascript_git(
        workspace,
        &[
            "ls-tree".as_ref(),
            "-r".as_ref(),
            "-t".as_ref(),
            "-z".as_ref(),
            tree.as_ref(),
        ],
    )?;
    let mut objects = BTreeMap::new();
    // ls-tree lists children, not the root whose verified ID the caller supplied.
    objects.insert(
        PathBuf::new(),
        JavascriptObject {
            mode: "040000".into(),
            kind: "tree".into(),
            id: tree.into(),
        },
    );
    let mut paths = Vec::new();
    let mut manifests = BTreeMap::new();
    for entry in entries.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| io::Error::other("malformed JavaScript Git tree entry"))?;
        let header = std::str::from_utf8(&entry[..tab]).map_err(io::Error::other)?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(io::Error::other("malformed JavaScript Git tree object"));
        }
        let path = PathBuf::from(std::ffi::OsString::from_vec(entry[tab + 1..].to_vec()));
        if path
            .components()
            .any(|part| part.as_os_str() == "node_modules")
        {
            continue;
        }
        let object = JavascriptObject {
            mode: fields[0].into(),
            kind: fields[1].into(),
            id: fields[2].into(),
        };
        if object.kind != "tree" {
            paths.push(path.clone());
        }
        if object.kind == "blob" && path.file_name().is_some_and(|name| name == "package.json") {
            // Read the materialized manifests before any install can alter the
            // checkout. Defer errors for manifests outside an install's inputs.
            let manifest = (|| {
                if !optional_regular_file(&workspace.join(&path))? {
                    return Err(io::Error::other("tracked manifest is missing"));
                }
                serde_json::from_slice(&fs::read(workspace.join(&path))?).map_err(io::Error::other)
            })();
            manifests.insert(path.clone(), manifest);
        }
        objects.insert(path, object);
    }
    Ok(JavascriptTree {
        paths,
        objects,
        manifests,
        logical_root: None,
    })
}

#[cfg(test)]
fn javascript_tracked_paths(workspace: &Path, tree: &str) -> io::Result<Vec<PathBuf>> {
    Ok(javascript_tracked_tree(workspace, tree)?.paths)
}

fn discover_javascript_installs(paths: &[PathBuf]) -> io::Result<Vec<JavascriptInstall>> {
    let mut directories: BTreeMap<PathBuf, (bool, [bool; 4])> = BTreeMap::new();
    for path in paths {
        let name = path.file_name().unwrap_or_default();
        if name == "package.json" || JAVASCRIPT_LOCKFILES.iter().any(|lock| name == *lock) {
            let entry = directories
                .entry(path.parent().unwrap_or(Path::new("")).to_path_buf())
                .or_default();
            if name == "package.json" {
                entry.0 = true;
            } else if let Some(i) = JAVASCRIPT_LOCKFILES.iter().position(|lock| name == *lock) {
                entry.1[i] = true;
            }
        }
    }
    let mut installs = Vec::new();
    for (directory, (manifest, locks)) in directories {
        let lockfile = locks
            .iter()
            .position(|present| *present)
            .map(|i| JAVASCRIPT_LOCKFILES[i]);
        let label = if directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &directory
        };
        match (manifest, lockfile) {
            (false, Some(lockfile)) => return Err(io::Error::other(format!(
                "workspace directory {} has {} but no package.json; refusing an unlocked JavaScript install",
                label.display(), directory.join(lockfile).display()
            ))),
            (true, None) if directory.as_os_str().is_empty() => return Err(io::Error::other(
                "workspace directory . has package.json but no supported JavaScript lockfile (expected bun.lock, bun.lockb, package-lock.json, or pnpm-lock.yaml); refusing an unlocked JavaScript install"
            )),
            (true, Some(lockfile)) => installs.push(JavascriptInstall { directory, lockfile }),
            _ => {}
        }
    }
    installs.sort_by(|a, b| {
        (a.directory.components().count(), &a.directory)
            .cmp(&(b.directory.components().count(), &b.directory))
    });
    Ok(installs)
}

fn prepare_javascript_dependencies(
    state: &Path,
    repository: &str,
    workspace: &Path,
    tree: &str,
    tools: &JavascriptTools,
) -> io::Result<()> {
    let tracked = javascript_tracked_tree(workspace, tree)?;
    prepare_javascript_with(workspace, &tracked.paths, tools, |install, command| {
        cache_javascript_install(
            state, repository, workspace, &tracked, install, command, tools,
        )
    })
}

/// Expands to one embedded file of a committed JavaScript fixture:
/// `(path below the check workspace, contents, executable)`.
macro_rules! javascript_fixture_file {
    ($manager:literal, $path:literal, $executable:literal) => {
        (
            concat!($manager, "/", $path),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/test-fixtures/javascript/",
                $manager,
                "/",
                $path
            )),
            $executable,
        )
    };
}

/// One offline locked install per supported lockfile kind (bun.lock,
/// package-lock.json, pnpm-lock.yaml), each with a local dependency and a
/// preinstall script that must not run. These are the committed fixtures the
/// real-manager tests install; they are embedded so an image can run the
/// check with nothing staged beside the runner.
const JAVASCRIPT_SETUP_FIXTURE: &[(&str, &str, bool)] = &[
    javascript_fixture_file!("bun", "package.json", false),
    javascript_fixture_file!("bun", "bun.lock", false),
    javascript_fixture_file!("bun", "vendor/fixture-dep/package.json", false),
    javascript_fixture_file!("bun", "vendor/fixture-dep/index.js", false),
    javascript_fixture_file!("bun", "vendor/fixture-dep/cli.js", true),
    javascript_fixture_file!("npm", "package.json", false),
    javascript_fixture_file!("npm", "package-lock.json", false),
    javascript_fixture_file!("npm", "vendor/fixture-dep/package.json", false),
    javascript_fixture_file!("npm", "vendor/fixture-dep/index.js", false),
    javascript_fixture_file!("npm", "vendor/fixture-dep/cli.js", true),
    javascript_fixture_file!("pnpm", "package.json", false),
    javascript_fixture_file!("pnpm", "pnpm-lock.yaml", false),
    javascript_fixture_file!("pnpm", "pnpm-workspace.yaml", false),
    javascript_fixture_file!("pnpm", "vendor/fixture-dep/package.json", false),
    javascript_fixture_file!("pnpm", "vendor/fixture-dep/index.js", false),
    javascript_fixture_file!("pnpm", "vendor/fixture-dep/cli.js", true),
];

/// Runs the runner's trusted JavaScript dependency setup with `tools` on the
/// embedded fixture, in a new directory `scratch`: the same discovery, tool
/// lookup, version probes, manager cache environment and locked install that
/// prepare a job's checkout. Only the install cache, which needs a reflink
/// filesystem, is left out. Returns one line per install naming the tool that
/// ran; any missing tool or failed install is an error naming it.
pub(crate) fn check_javascript_setup(
    tools: &JavascriptTools,
    scratch: &Path,
) -> io::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir(scratch).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("create new check directory {}: {error}", scratch.display()),
        )
    })?;
    let workspace = scratch.join("workspace");
    for (path, contents, executable) in JAVASCRIPT_SETUP_FIXTURE {
        let file = workspace.join(path);
        fs::create_dir_all(file.parent().expect("fixture files are in directories"))?;
        fs::write(&file, contents)?;
        if *executable {
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
        }
    }
    checked(Command::new("git").args(["init", "-q"]).arg(&workspace))?;
    checked(
        Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(["add", "-f", "."]),
    )?;
    let tree = workspace::git(&workspace, &["write-tree"])?;
    let tracked = javascript_tracked_tree(&workspace, &tree)?;
    let caches = scratch.join("manager-caches");
    let mut report = Vec::new();
    prepare_javascript_with(&workspace, &tracked.paths, tools, |install, command| {
        let (manager_version, _) = javascript_install_versions(command, install, tools)?;
        let manager = javascript_manager(install.lockfile);
        let cache = caches.join(manager);
        fs::create_dir_all(cache.join("home"))?;
        javascript_manager_cache_command(command, manager, &cache);
        let directory = workspace.join(&install.directory);
        let lockfile = install.directory.join(install.lockfile);
        run_javascript_install_via(
            &directory,
            manager,
            &lockfile.to_string_lossy(),
            command,
            checked,
        )?;
        if !directory
            .join("node_modules/fixture-dep/index.js")
            .is_file()
        {
            return Err(io::Error::other(
                "the locked install did not install the fixture's local dependency",
            ));
        }
        if directory.join("lifecycle-ran").exists() {
            return Err(io::Error::other("the fixture's preinstall script ran"));
        }
        report.push(format!(
            "{}: {} {}",
            lockfile.display(),
            Path::new(command.get_program()).display(),
            String::from_utf8_lossy(&manager_version).trim()
        ));
        Ok(())
    })?;
    if report.len() != 3 {
        return Err(io::Error::other(format!(
            "expected 3 locked installs, ran {}",
            report.len()
        )));
    }
    Ok(report.join("\n"))
}

/// Installs the JavaScript dependencies of a sibling snapshot being built
/// at `snapshot`, for the sibling whose canonical path is `path`. The install
/// sees the snapshot as jobs will: each package manager runs in a helper unit
/// that shows the snapshot at `path`, and local dependencies are checked and
/// resolved against `path` inside the snapshot. The host checkout at `path`,
/// which may be that repository's own workspace at another commit, is never
/// read. The install cache is keyed by `path`, as for an in-place sibling.
pub(crate) fn prepare_snapshot_javascript(
    config: &crate::engine::Config,
    path: &Path,
    snapshot: &Path,
    tree: &str,
) -> io::Result<()> {
    use crate::sibling_snapshots::helper::{self, Helper, View};
    let state = config.state_dir();
    let repository = path
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF-8 sibling path"))?;
    let mut tracked = javascript_tracked_tree(snapshot, tree)?;
    tracked.logical_root = Some(path.to_path_buf());
    let view = View::javascript(snapshot, path);
    let tools = JavascriptTools::for_config(config);
    prepare_javascript_with(snapshot, &tracked.paths, &tools, |install, command| {
        cache_javascript_install_via(
            &state,
            repository,
            snapshot,
            &tracked,
            install,
            command,
            &tools,
            |command| {
                helper::run(
                    config,
                    Helper::JavascriptInstall,
                    Helper::JavascriptInstall.limit(config),
                    command,
                    &view,
                )
            },
        )
    })
}

fn prepare_javascript_checkouts(
    workspace: &Path,
    repository: &str,
    tree: &str,
    siblings: &[SiblingSnapshot],
    mut prepare: impl FnMut(&Path, &str, &str) -> io::Result<()>,
) -> io::Result<()> {
    prepare(workspace, repository, tree)?;
    for sibling in siblings {
        prepare(
            Path::new(&sibling.canonical_path),
            &sibling.canonical_path,
            &sibling.tree_hash,
        )?;
    }
    Ok(())
}

fn prepare_javascript_with(
    workspace: &Path,
    paths: &[PathBuf],
    tools: &JavascriptTools,
    mut install: impl FnMut(&JavascriptInstall, &mut Command) -> io::Result<()>,
) -> io::Result<()> {
    for entry in discover_javascript_installs(paths)? {
        let result = javascript_install_command(workspace, &entry, tools)
            .and_then(|mut command| install(&entry, &mut command));
        result.map_err(|error| {
            io::Error::other(format!(
                "JavaScript setup in directory {} using {}: {error}",
                if entry.directory.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    &entry.directory
                }
                .display(),
                entry.directory.join(entry.lockfile).display()
            ))
        })?;
    }
    Ok(())
}

fn javascript_manager(lockfile: &str) -> &'static str {
    match lockfile {
        "bun.lock" | "bun.lockb" => "bun",
        "package-lock.json" => "npm",
        "pnpm-lock.yaml" => "pnpm",
        _ => unreachable!("discovery selects only supported lockfiles"),
    }
}

fn javascript_install_command(
    workspace: &Path,
    install: &JavascriptInstall,
    tools: &JavascriptTools,
) -> io::Result<Command> {
    let directory = workspace.join(&install.directory);
    for name in ["package.json", install.lockfile] {
        if !optional_regular_file(&directory.join(name))? {
            return Err(io::Error::other(format!(
                "tracked JavaScript input is missing: {}",
                install.directory.join(name).display()
            )));
        }
    }
    let node_modules = directory.join("node_modules");
    match fs::symlink_metadata(&node_modules) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::other(format!(
                "workspace {} has a non-directory node_modules entry",
                directory.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let manager = javascript_manager(install.lockfile);
    let mut command = Command::new(tools.require(manager)?);
    // npm and pnpm run on Node: refuse here, naming Node's configured path,
    // rather than let `env node` fail inside the manager.
    if manager != "bun" {
        tools.require("node")?;
    }
    if manager == "npm" {
        command.args(["ci", "--ignore-scripts"]);
    } else {
        command.args(["install", "--frozen-lockfile", "--ignore-scripts"]);
    }
    command
        .current_dir(directory)
        .env("PATH", tools.search_path());
    Ok(command)
}

/// For the sibling snapshot key: the `--version` output of each JavaScript
/// package manager (bun, npm, pnpm) at its configured location, by manager
/// name, and of Node. A manager that is missing or fails is left out and a
/// missing Node gives empty output, so installing one changes the key without
/// making snapshot setup fail here; an install that needs it still fails as it
/// does today.
pub(crate) fn javascript_runner_versions(
    tools: &JavascriptTools,
) -> (BTreeMap<String, Vec<u8>>, Vec<u8>) {
    let version = |executable: PathBuf| {
        Command::new(executable)
            .arg("--version")
            .env("PATH", tools.search_path())
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| output.stdout)
    };
    let managers = ["bun", "npm", "pnpm"]
        .into_iter()
        .filter_map(|manager| {
            version(tools.executable(manager)).map(|output| (manager.to_string(), output))
        })
        .collect();
    let node = version(tools.executable("node")).unwrap_or_default();
    (managers, node)
}

fn javascript_version(command: &Command, executable: &std::ffi::OsStr) -> io::Result<Vec<u8>> {
    let mut version = Command::new(executable);
    version.arg("--version");
    if let Some(directory) = command.get_current_dir() {
        version.current_dir(directory);
    }
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => {
                version.env(name, value);
            }
            None => {
                version.env_remove(name);
            }
        }
    }
    let output = version.output().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::other(format!(
                "{} not installed: {error}",
                executable.to_string_lossy()
            ))
        } else {
            error
        }
    })?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot read {} --version: {}",
            executable.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn javascript_key_input(path: &Path, install: &JavascriptInstall) -> bool {
    path == install.directory.join(install.lockfile)
        || (matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("package.json" | ".npmrc" | "bunfig.toml" | "pnpm-workspace.yaml")
        ) && (path.starts_with(&install.directory)
            || install
                .directory
                .starts_with(path.parent().unwrap_or(Path::new("")))))
}

fn javascript_local_specs<'a>(value: &'a serde_json::Value, specs: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(spec)
            if ["file:", "link:", "portal:"]
                .iter()
                .any(|prefix| spec.starts_with(prefix)) =>
        {
            specs.push(spec)
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                javascript_local_specs(value, specs);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                javascript_local_specs(value, specs);
            }
        }
        _ => {}
    }
}

fn javascript_local_dependency(
    workspace: &Path,
    tracked: &JavascriptTree,
    manifest: &Path,
    spec: &str,
) -> io::Result<Vec<PathBuf>> {
    let refuse = |reason: &str| {
        io::Error::other(format!(
            "JavaScript manifest {} has local dependency {spec:?}: {reason}",
            manifest.display()
        ))
    };
    let target = Path::new(spec.split_once(':').expect("local dependency scheme").1);
    let root = tracked.logical_root.as_deref().unwrap_or(workspace);
    let (base, target) = if target.is_absolute() {
        (
            Path::new(""),
            target
                .strip_prefix(root)
                .map_err(|_| refuse("path resolves outside the repository"))?,
        )
    } else {
        (manifest.parent().unwrap_or(Path::new("")), target)
    };
    let mut relative = base.to_path_buf();
    for component in target.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => return Err(refuse("path resolves outside the repository")),
        }
    }
    let object = tracked
        .objects
        .get(&relative)
        .ok_or_else(|| refuse("path is not tracked in the job tree"))?;
    if !matches!(object.kind.as_str(), "blob" | "tree") {
        return Err(refuse(
            "path has no tracked file or directory content in the job tree",
        ));
    }
    // A tracked symlink must not turn an apparently local reference into an
    // unkeyed dependency outside the checkout, or into an untracked target.
    let resolved = if let Some(logical) = &tracked.logical_root {
        resolve_within(workspace, logical, &relative)
            .map_err(|error| refuse(&format!("cannot resolve tracked path: {error}")))?
            .ok_or_else(|| refuse("path resolves outside the repository"))?
    } else {
        let canonical = workspace
            .join(&relative)
            .canonicalize()
            .map_err(|error| refuse(&format!("cannot resolve tracked path: {error}")))?;
        let root = workspace.canonicalize()?;
        canonical
            .strip_prefix(root)
            .map_err(|_| refuse("path resolves outside the repository"))?
            .to_path_buf()
    };
    let resolved_object = tracked
        .objects
        .get(&resolved)
        .ok_or_else(|| refuse("resolved path is not tracked in the job tree"))?;
    if !matches!(resolved_object.kind.as_str(), "blob" | "tree") {
        return Err(refuse(
            "resolved path has no tracked file or directory content in the job tree",
        ));
    }
    Ok(vec![relative, resolved])
}

/// Resolves the symbolic links in `relative` inside the checkout at `root`,
/// which the install sees at `logical`, and returns the resolved path
/// relative to the checkout, or `None` when it leads outside it. An absolute
/// link target counts as inside only when it lies under `logical`, and is
/// then followed inside `root`: nothing outside `root` is ever read, so a
/// different checkout on the host at `logical` cannot change the answer.
fn resolve_within(root: &Path, logical: &Path, relative: &Path) -> io::Result<Option<PathBuf>> {
    use std::collections::VecDeque;
    /// The kernel's limit on links followed in one path lookup.
    const MAX_LINKS: usize = 40;
    /// Queues the components of `path` ahead of the pending ones. Refuses
    /// a path with a root, which only an absolute link target can have.
    fn push(pending: &mut VecDeque<std::ffi::OsString>, path: &Path) -> bool {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => parts.push(name.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir => parts.push("..".into()),
                Component::RootDir | Component::Prefix(_) => return false,
            }
        }
        for part in parts.into_iter().rev() {
            pending.push_front(part);
        }
        true
    }
    let mut pending: VecDeque<std::ffi::OsString> = VecDeque::new();
    if !push(&mut pending, relative) {
        return Ok(None);
    }
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            if !resolved.pop() {
                return Ok(None);
            }
            continue;
        }
        let candidate = root.join(&resolved).join(&part);
        if !fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
            resolved.push(part);
            continue;
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(io::Error::other("too many levels of symbolic links"));
        }
        let target = fs::read_link(&candidate)?;
        let target = if target.is_absolute() {
            let Ok(inside) = target.strip_prefix(logical) else {
                return Ok(None);
            };
            resolved = PathBuf::new();
            inside.to_path_buf()
        } else {
            target
        };
        if !push(&mut pending, &target) {
            return Ok(None);
        }
    }
    Ok(Some(resolved))
}

fn javascript_cache_key(
    workspace: &Path,
    repository: &str,
    tracked: &JavascriptTree,
    install: &JavascriptInstall,
    manager_version: &[u8],
    node_version: &[u8],
) -> io::Result<String> {
    fn field(hash: &mut sha2::Sha256, bytes: &[u8]) {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let mut hash = sha2::Sha256::new();
    // v3 install entries can contain stale local tarball bytes fetched from a
    // shared manager cache. The new key domain prevents restoring those entries.
    field(&mut hash, b"motor-javascript-install-v4");
    field(&mut hash, repository.as_bytes());
    field(&mut hash, javascript_manager(install.lockfile).as_bytes());
    field(&mut hash, manager_version);
    field(&mut hash, node_version);
    field(&mut hash, std::env::consts::OS.as_bytes());
    field(&mut hash, std::env::consts::ARCH.as_bytes());
    field(&mut hash, install.lockfile.as_bytes());
    let (inputs, _) = javascript_cache_inputs(workspace, tracked, install)?;
    for path in inputs {
        let object = &tracked.objects[&path];
        field(&mut hash, path.as_os_str().as_bytes());
        field(&mut hash, object.mode.as_bytes());
        field(&mut hash, object.kind.as_bytes());
        field(&mut hash, object.id.as_bytes());
    }
    for (name, value) in javascript_selection_environment() {
        field(&mut hash, name.as_bytes());
        field(&mut hash, value.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// The daemon's environment variables that can change which JavaScript
/// dependencies an install selects, sorted by name. Daemon configuration can
/// affect dependency selection even with an identical lockfile (for example
/// NODE_ENV or npm_config_omit). Cache locations are controlled by the runner
/// and are not dependency-selection inputs. Both the install cache key and the
/// sibling snapshot key hash this list, so the two change together.
pub(crate) fn javascript_selection_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut environment: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            !javascript_cache_variable(&name)
                && ["NPM_CONFIG_", "BUN_", "PNPM_", "NODE_"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        .collect();
    environment.sort();
    environment
}

fn javascript_cache_inputs(
    workspace: &Path,
    tracked: &JavascriptTree,
    install: &JavascriptInstall,
) -> io::Result<(std::collections::BTreeSet<PathBuf>, bool)> {
    let mut has_local = false;
    let mut inputs: std::collections::BTreeSet<_> = tracked
        .paths
        .iter()
        .filter(|path| javascript_key_input(path, install))
        .cloned()
        .collect();
    let mut pending: Vec<_> = inputs
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "package.json"))
        .cloned()
        .collect();
    let mut visited = std::collections::BTreeSet::new();
    // Local packages can refer to siblings outside their own subtree. Follow
    // their manifests too, and visit each path once so dependency cycles finish.
    while let Some(path) = pending.pop() {
        if !visited.insert(path.clone()) {
            continue;
        }
        let manifest = tracked
            .manifests
            .get(&path)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "JavaScript manifest {} is not a tracked regular file",
                    path.display()
                ))
            })?
            .as_ref()
            .map_err(|error| {
                io::Error::other(format!("JavaScript manifest {}: {error}", path.display()))
            })?;
        let mut specs = Vec::new();
        for section in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
            "overrides",
            "resolutions",
        ] {
            if let Some(value) = manifest.get(section) {
                javascript_local_specs(value, &mut specs);
            }
        }
        for spec in specs {
            has_local = true;
            for dependency in javascript_local_dependency(workspace, tracked, &path, spec)? {
                if tracked.objects[&dependency].kind == "tree" {
                    let manifest = dependency.join("package.json");
                    if tracked.objects.contains_key(&manifest) {
                        inputs.insert(manifest.clone());
                        if !visited.contains(&manifest) {
                            pending.push(manifest);
                        }
                    }
                }
                inputs.insert(dependency);
            }
        }
    }
    Ok((inputs, has_local))
}

fn javascript_cache_variable(name: &str) -> bool {
    matches!(
        name.replace('-', "_").as_str(),
        "BUN_INSTALL"
            | "BUN_INSTALL_CACHE_DIR"
            | "BUN_INSTALL_GLOBAL_DIR"
            | "BUN_INSTALL_BIN"
            | "XDG_CACHE_HOME"
            | "XDG_DATA_HOME"
            | "PNPM_HOME"
            | "NPM_CONFIG_CACHE"
            | "NPM_CONFIG_STORE_DIR"
            | "NPM_CONFIG_CACHE_DIR"
            | "PNPM_CONFIG_STORE_DIR"
            | "PNPM_CONFIG_CACHE_DIR"
    )
}

fn javascript_manager_cache_key(
    repository: &str,
    manager: &str,
    local_key: Option<&str>,
) -> String {
    workspace::name(
        &serde_json::to_string(&(
            "motor-javascript-manager-v1",
            repository,
            manager,
            local_key,
        ))
        .expect("cache identity serialization"),
    )
}

fn javascript_manager_cache_command(command: &mut Command, manager: &str, cache: &Path) {
    // Clear both the daemon's environment and child-only overrides before
    // selecting cache locations. HOME also confines manager-specific fallbacks.
    let names: std::collections::BTreeSet<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .chain(command.get_envs().map(|(name, _)| name.to_os_string()))
        .collect();
    for name in names {
        if javascript_cache_variable(&name.to_string_lossy().to_ascii_uppercase()) {
            command.env_remove(name);
        }
    }
    command
        .env("HOME", cache.join("home"))
        .env("XDG_CACHE_HOME", cache.join("xdg-cache"))
        .env("XDG_DATA_HOME", cache.join("xdg-data"))
        .env("BUN_INSTALL_CACHE_DIR", cache.join("bun"))
        .env("npm_config_cache", cache.join("npm"));
    match manager {
        "bun" => {
            command.arg("--cache-dir").arg(cache.join("bun"));
        }
        "npm" => {
            command.arg("--cache").arg(cache.join("npm"));
        }
        _ => {
            command
                .arg("--store-dir")
                .arg(cache.join("pnpm-store"))
                .arg("--cache-dir")
                .arg(cache.join("xdg-cache/pnpm"));
        }
    }
}

fn cache_javascript_install(
    state: &Path,
    repository: &str,
    workspace: &Path,
    tracked: &JavascriptTree,
    install: &JavascriptInstall,
    command: &mut Command,
    tools: &JavascriptTools,
) -> io::Result<()> {
    cache_javascript_install_via(
        state, repository, workspace, tracked, install, command, tools, checked,
    )
}

/// The manager's and Node's `--version` output that key an install's cache
/// entry. Bun installs do not run Node, so their Node version is empty.
fn javascript_install_versions(
    command: &Command,
    install: &JavascriptInstall,
    tools: &JavascriptTools,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let manager_version = javascript_version(command, command.get_program())?;
    let node_version = if javascript_manager(install.lockfile) == "bun" {
        Vec::new()
    } else {
        javascript_version(command, tools.require("node")?.as_os_str())?
    };
    Ok((manager_version, node_version))
}

/// Like `cache_javascript_install`, with `run` running the package manager
/// when the cache has no entry, for example in a sibling snapshot helper
/// unit.
#[allow(clippy::too_many_arguments)]
fn cache_javascript_install_via(
    state: &Path,
    repository: &str,
    workspace: &Path,
    tracked: &JavascriptTree,
    install: &JavascriptInstall,
    command: &mut Command,
    tools: &JavascriptTools,
    run: impl FnOnce(&mut Command) -> io::Result<()>,
) -> io::Result<()> {
    let (manager_version, node_version) = javascript_install_versions(command, install, tools)?;
    let key = javascript_cache_key(
        workspace,
        repository,
        tracked,
        install,
        &manager_version,
        &node_version,
    )?;
    let lock = platform::javascript_cache_lock(state, &key)?;
    lock.lock_exclusive()?;
    let cache = state.join("js-installs");
    fs::create_dir_all(&cache)?;
    let entry = cache.join(&key);
    let modules = workspace.join(&install.directory).join("node_modules");
    if entry.is_dir() {
        if modules.exists() {
            fs::remove_dir_all(&modules)?;
        }
        platform::clone_target(&entry.join("node_modules"), &modules)?;
        File::open(&entry)?
            .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
        return Ok(());
    }
    let manager = javascript_manager(install.lockfile);
    let (_, has_local) = javascript_cache_inputs(workspace, tracked, install)?;
    // Bun keys local tarballs by path, not bytes. A content-scoped manager cache
    // prevents a changed local input from meeting an older path-keyed entry.
    let manager_key =
        javascript_manager_cache_key(repository, manager, has_local.then_some(key.as_str()));
    let manager_lock = platform::javascript_cache_lock(state, &manager_key)?;
    manager_lock.lock_exclusive()?;
    let manager_cache = state.join("js-manager-caches").join(&manager_key);
    fs::create_dir_all(manager_cache.join("home"))?;
    record_javascript_cache_owner(&manager_cache, repository)?;
    File::open(&manager_cache)?
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
    javascript_manager_cache_command(command, manager, &manager_cache);
    run_javascript_install_via(
        &workspace.join(&install.directory),
        javascript_manager(install.lockfile),
        &install.directory.join(install.lockfile).to_string_lossy(),
        command,
        run,
    )?;
    // A workspace install can also write node_modules inside member packages
    // (bun's isolated linker, pnpm, npm version conflicts), but a cache entry
    // stores only <dir>/node_modules. Caching such an install would give later
    // jobs members without their dependencies, so it is not cached and runs
    // fresh in every job.
    if javascript_install_wrote_member_modules(workspace, tracked, install) {
        return Ok(());
    }
    // Managers may omit node_modules entirely for a dependency-free package.
    // An empty tree is still a successful install and can be reused.
    fs::create_dir_all(&modules)?;
    struct TemporaryEntry(PathBuf);
    impl Drop for TemporaryEntry {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let temporary = TemporaryEntry(cache.join(format!("{key}.{}.tmp", Uuid::now_v7())));
    fs::create_dir(&temporary.0)?;
    record_javascript_cache_owner(&temporary.0, repository)?;
    platform::clone_target(&modules, &temporary.0.join("node_modules"))?;
    fs::rename(&temporary.0, &entry)?;
    Ok(())
}

/// Names the repository whose install created a cache entry, so the entry is
/// retired with that repository's mirror. Entries made before this marker
/// existed carry none and are left to disk-pressure eviction.
fn record_javascript_cache_owner(entry: &Path, repository: &str) -> io::Result<()> {
    let marker = entry.join(workspace::JAVASCRIPT_CACHE_OWNER);
    if !marker.exists() {
        fs::write(marker, workspace::name(repository))?;
    }
    Ok(())
}

/// Whether the install left a node_modules in any tracked directory below the
/// install directory. Installs run shallowest first in a cleaned checkout, so
/// any such tree was written by this install.
fn javascript_install_wrote_member_modules(
    workspace: &Path,
    tracked: &JavascriptTree,
    install: &JavascriptInstall,
) -> bool {
    let mut directories = std::collections::BTreeSet::new();
    for path in &tracked.paths {
        let Ok(relative) = path.strip_prefix(&install.directory) else {
            continue;
        };
        let mut current = relative.parent();
        while let Some(directory) = current.filter(|d| !d.as_os_str().is_empty()) {
            directories.insert(install.directory.join(directory));
            current = directory.parent();
        }
    }
    directories.iter().any(|directory| {
        fs::symlink_metadata(workspace.join(directory).join("node_modules")).is_ok()
    })
}

#[cfg(test)]
fn run_javascript_install(
    workspace: &Path,
    manager: &str,
    lockfile: &str,
    command: &mut Command,
) -> io::Result<()> {
    run_javascript_install_via(workspace, manager, lockfile, command, checked)
}

fn run_javascript_install_via(
    workspace: &Path,
    manager: &str,
    lockfile: &str,
    command: &mut Command,
    run: impl FnOnce(&mut Command) -> io::Result<()>,
) -> io::Result<()> {
    let program = PathBuf::from(command.get_program());
    run(command).map_err(|error| {
        let detail = if error.kind() == io::ErrorKind::NotFound {
            format!("{manager} not installed at {}: {error}", program.display())
        } else {
            error.to_string()
        };
        io::Error::other(format!(
            "locked {manager} install using {lockfile} failed in {}: {detail}",
            workspace.display()
        ))
    })
}

/// Let the job bind any port, including 443, on its own loopback.
///
/// The job sees only its own loopback and, when opted in, a firewalled veth.
/// Binding low ports cannot claim a host port, so the low-port restriction
/// protects nothing on the host; Docker sets the same value for containers.
/// It is set from the host while the unit's main process still waits
/// on the start gate, entering only that process's network namespace, so the
/// host keeps its own setting and the job gains no capability. A systemd `+`
/// pre-step cannot do this: under RootDirectory it runs in the sandbox root
/// without the unit's mount namespace, where /proc is an empty directory.
fn allow_low_ports(id: Uuid) -> io::Result<()> {
    let output = systemctl(&["show", "--property=MainPID", "--value", &unit(id)])?;
    let pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| io::Error::other("job unit has no main process"))?;
    let status = crate::systemd::output(crate::privilege::command("/usr/bin/nsenter").args([
        "--target",
        &pid.to_string(),
        "--net",
        "/usr/sbin/sysctl",
        "-q",
        "-w",
        "net.ipv4.ip_unprivileged_port_start=0",
    ]))?
    .status;
    if !status.success() {
        return Err(io::Error::other(format!(
            "cannot allow low ports in the job's network namespace: {status}"
        )));
    }
    Ok(())
}

fn unit(id: Uuid) -> String {
    crate::backend::host_unit_name(id)
}

pub(crate) fn unit_alive(id: Uuid) -> io::Result<bool> {
    crate::systemd::alive(&unit(id), systemctl)
}

fn systemctl(args: &[&str]) -> io::Result<std::process::Output> {
    crate::systemd::systemctl(args, true)
}

pub(crate) fn signal(id: Uuid, hard: bool) {
    // Stop marks a unit as stopping as well as killing its entire cgroup. It
    // also cancels a pending start; unlike kill(pid), it crosses the UID boundary.
    if let Err(error) = crate::systemd::signal(&unit(id), hard, systemctl) {
        tracing::error!(%id, %error, "could not stop isolated job cgroup");
    }
}

/// Stop a sandbox that never started and unload its unit. Job units stay loaded
/// after failing, so the runner can read why a started job died, but only the
/// finish path unloads them. A sandbox refused before start never reaches that
/// path, so without this its failed unit would stay loaded indefinitely.
fn discard_unstarted(id: Uuid) {
    signal(id, true);
    // The stop is asynchronous. Wait, bounded, for the unit to settle, so that
    // reset-failed acts on its final state rather than on a unit still stopping.
    let deadline = Instant::now() + Duration::from_secs(5);
    while unit_alive(id).unwrap_or(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    match systemctl(&["reset-failed", &unit(id)]) {
        // A unit that stopped cleanly, or was never created, is already unloaded.
        Ok(output) if output.status.success() || is_not_loaded(&output) => {}
        Ok(output) => tracing::warn!(
            %id,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "could not unload a sandbox unit that never started"
        ),
        Err(error) => {
            tracing::warn!(%id, %error, "could not unload a sandbox unit that never started")
        }
    }
}

fn is_not_loaded(output: &std::process::Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains("not loaded")
}

// BindPaths is a systemd word list, not a shell command. Escape separators,
// whitespace, quotes and specifiers so repository paths cannot add mounts.
fn word(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF8 mount path"))?;
    if !Path::new(path).is_absolute() {
        return Err(io::Error::other("non-absolute mount path"));
    }
    let mut result = String::new();
    for byte in path.bytes() {
        match byte {
            b'%' => result.push_str("%%"),
            b'/' | b'.' | b'_' | b'-' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                result.push(byte as char)
            }
            _ => result.push_str(&format!("\\x{byte:02x}")),
        }
    }
    Ok(result)
}

fn bind(source: &Path, destination: &Path) -> io::Result<String> {
    Ok(format!("{}:{}", word(source)?, word(destination)?))
}

/// The read-only mount that shows the runner's Bun directory to jobs at
/// [`JOB_BUN_DIR`], where every role's job PATH expects Bun, together with
/// that destination. On the build server and Boat the two paths are the same;
/// a tenant VM keeps Bun outside its /home. Without a Bun directory there is
/// no mount, and a job that runs `bun` fails to find it.
fn job_bun_mount(javascript: &JavascriptTools) -> io::Result<Option<(String, PathBuf)>> {
    let source = javascript.bun_dir();
    if !source.is_dir() {
        return Ok(None);
    }
    let destination = PathBuf::from(JOB_BUN_DIR);
    let mount = if source == destination {
        word(source)?
    } else {
        bind(source, &destination)?
    };
    Ok(Some((mount, destination)))
}

/// The start of every unit shell: wait at the durable gate, then set the
/// umask.
const SHELL_GATE: &str = "IFS= read -r -n 1 || exit 125; umask 022; ";

/// Inserts the start of each remote cache forwarder right after the gate, so
/// the forwarders are listening before anything else of the unit runs, the
/// writer's preflight included. Without forwarders the shell is unchanged.
fn with_cache_forwarders<'a>(
    shell: &'a str,
    forwarders: &[transport::Forwarder],
) -> io::Result<std::borrow::Cow<'a, str>> {
    if forwarders.is_empty() {
        return Ok(std::borrow::Cow::Borrowed(shell));
    }
    let rest = shell
        .strip_prefix(SHELL_GATE)
        .ok_or_else(|| io::Error::other("unit shell does not start at its gate"))?;
    let starts: String = forwarders.iter().map(transport::Forwarder::start).collect();
    Ok(std::borrow::Cow::Owned(format!(
        "{SHELL_GATE}{starts}{rest}"
    )))
}

/// The same list drives preallocation, bind mounts and writer grants.
fn cache_output_names(ccache: bool) -> Vec<&'static str> {
    let mut names = vec!["stdout", "stderr", "exit", "cache-stats"];
    if ccache {
        names.push(crate::remote_cache::ccache::STATS_FILE);
    }
    names
}

/// Capture after the command saves its status, so a statistics failure never
/// replaces the build's exit code. The output inode is allocated before launch.
fn with_ccache_statistics(shell: &str, enabled: bool) -> String {
    if enabled {
        shell.replace(
            "printf '%s\\n' \"$code\"",
            "ccache --print-stats >/motor-output/ccache-stats 2>/dev/null || true; printf '%s\\n' \"$code\"",
        )
    } else {
        shell.to_string()
    }
}

/// Lets `account` connect to each socket in `paths`, without following symlinks.
fn grant_sockets(paths: &[PathBuf], account: &str) -> io::Result<()> {
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-P", "-m", &format!("u:{account}:rw"), "--"])
            .args(paths),
    )
}

fn grant(path: &Path) -> io::Result<()> {
    grant_paths(&[path])
}

fn grant_paths(paths: &[&Path]) -> io::Result<()> {
    // Grant the job account access without following symlinks. Remove default
    // ACLs so new private files keep the job's normal umask-derived permissions;
    // restore daemon access only after the job has stopped.
    let acl = format!("u:{USER}:rwX");
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-k", "-m", &acl, "--"])
            .args(paths),
    )
}

pub(crate) fn prepare_job_writable_target(state: &Path, path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "job target is not a physical directory: {}",
            path.display()
        )));
    }
    let uid = crate::systemd::output(Command::new("/usr/bin/id").args(["-u", USER]))?;
    if !uid.status.success() {
        return Err(io::Error::other(format!(
            "cannot resolve job account for target {}: {}",
            path.display(),
            String::from_utf8_lossy(&uid.stderr)
        )));
    }
    let uid = String::from_utf8_lossy(&uid.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|_| io::Error::other(format!("invalid job account UID for {}", path.display())))?;
    let marker = state.join("job-targets").join(format!(
        "{:x}",
        sha2::Sha256::digest(path.as_os_str().as_bytes())
    ));
    let identity = format!("{}:{}", metadata.dev(), metadata.ino());
    // Record the target's device and inode only after recursive ownership and
    // mode repair succeeds. A match skips later tree walks, while a replaced
    // directory gets repaired even when it occupies the same path.
    if metadata.uid() == uid
        && fs::read_to_string(&marker).ok().as_deref() == Some(identity.as_str())
    {
        return Ok(());
    }
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["-R", "-P", "motor-job:motor-job", "--"])
            .arg(path),
    )?;
    crate::permissions::repair_tree(crate::permissions::Policy::OwnerWrite, &[path])?;
    fs::create_dir_all(marker.parent().unwrap())?;
    fs::write(marker, identity)?;
    Ok(())
}

fn create_sandbox_root(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "sandbox root is not a physical directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn set_sandbox_directory_owner_modes(groups: &[(&[PathBuf], &str)]) -> io::Result<()> {
    let paths: Vec<_> = groups.iter().flat_map(|(paths, _)| paths.iter()).collect();
    if paths.is_empty() {
        return Ok(());
    }
    // Validate the entire batch before changing any directory. The private
    // runtime tree is not exposed to a job until preparation has finished.
    for path in &paths {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::other(format!(
                "sandbox mount ancestor is not a physical directory: {}",
                path.display()
            )));
        }
    }
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["-h", "root:root", "--"])
            .args(&paths),
    )?;
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-b", "-k", "--"])
            .args(&paths),
    )?;
    for (paths, mode) in groups {
        if !paths.is_empty() {
            checked(
                crate::privilege::command("/usr/bin/chmod")
                    .args([mode, "--"])
                    .args(*paths),
            )?;
        }
    }
    Ok(())
}

fn prepare_root_owned_mount_ancestors(root: &Path, destination: &Path) -> io::Result<()> {
    if !destination.is_absolute() {
        return Err(io::Error::other(format!(
            "sandbox mount destination is not absolute: {}",
            destination.display()
        )));
    }
    let components = destination
        .components()
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(name) => Some(Ok(name.to_os_string())),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                Some(Err(io::Error::other(format!(
                    "unsafe sandbox mount destination: {}",
                    destination.display()
                ))))
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut current = root.to_path_buf();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::other(format!(
                    "sandbox mount ancestor is not a physical directory: {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn root_owned_mount_ancestors(root: &Path, destination: &Path) -> io::Result<Vec<PathBuf>> {
    let components = destination
        .components()
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(name) => Some(Ok(name.to_os_string())),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                Some(Err(io::Error::other(format!(
                    "unsafe sandbox mount destination: {}",
                    destination.display()
                ))))
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut current = root.to_path_buf();
    let mut paths = Vec::new();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component);
        paths.push(current.clone());
    }
    Ok(paths)
}

#[cfg(test)]
fn secure_root_owned_mount_ancestors(root: &Path, destination: &Path) -> io::Result<()> {
    set_sandbox_directory_owner_modes(&[(&root_owned_mount_ancestors(root, destination)?, "0755")])
}

fn prepare_job_home(path: &Path) -> io::Result<()> {
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-b", "-k", "--"])
            .arg(path),
    )?;
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["-R", "-P", "motor-job:motor-job", "--"])
            .arg(path),
    )?;
    crate::permissions::repair_tree(crate::permissions::Policy::Private, &[path])?;
    checked(
        crate::privilege::command("/usr/bin/chmod")
            .args(["0700", "--"])
            .arg(path),
    )
}

pub(crate) fn prepare_job_checkout_root(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "job checkout is not a physical directory: {}",
            path.display()
        )));
    }
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["motor-job:motor-job", "--"])
            .arg(path),
    )?;
    checked(
        crate::privilege::command("/usr/bin/chmod")
            .args(["0755", "--"])
            .arg(path),
    )
}

pub(crate) fn restore_runner_checkout_root(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    crate::test_cleanup::record_checkout_work("restore_runner_checkout_root", path);
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "runner checkout is not a physical directory: {}",
            path.display()
        )));
    }
    let owner = format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
        libc::getegid()
    });
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args([&owner, "--"])
            .arg(path),
    )?;
    checked(
        crate::privilege::command("/usr/bin/chmod")
            .args(["0755", "--"])
            .arg(path),
    )
}

pub(crate) fn reclaim_runtime(path: &Path) -> io::Result<()> {
    let _access = crate::tree_access::lock(path);
    // Boat uses bounded, descriptor-relative private GC even after a crash has
    // removed its lease. Recursive permission helpers cannot handle every legal
    // job-created path and must not strand its private data during recovery.
    if crate::boat_quota::reclaim_runtime(path)? || !path.exists() {
        return Ok(());
    }
    // ACLs alone cannot override sticky-directory unlink rules. Once the job's
    // cgroup is gone, reclaim the entire private tree (including nested sticky
    // directories and root-owned mount ancestors), without following symlinks.
    restore_runner_access(path)?;
    let owner = format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
        libc::getegid()
    });
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["-R", "-P", &owner, "--"])
            .arg(path),
    )?;
    crate::permissions::repair_tree(crate::permissions::Policy::OwnerWrite, &[path])
}

pub(crate) fn restore_runner_access(path: &Path) -> io::Result<()> {
    let _access = crate::tree_access::lock(path);
    #[cfg(test)]
    crate::test_cleanup::record_checkout_work("restore_runner_access", path);
    // Rust/Cargo create some cache files with mode 0600 and directories with
    // mode 0700. Those modes mask even an inherited named ACL. Once the prior
    // job has stopped, the trusted preparer must restore access before Git
    // cleanup or a reflink clone; it never follows job-created symlinks.
    let acl = format!("u:{}:rwX", unsafe { libc::geteuid() });
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-m", &acl, "--"])
            .arg(path),
    )
}

fn prepare_writer_trees(paths: &[&Path], account: WriterAccount) -> io::Result<()> {
    // Use the same canonical path order for every batch, so concurrent batches
    // cannot acquire overlapping tree guards in opposite orders.
    let mut guard_paths: Vec<_> = paths
        .iter()
        .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
        .collect();
    guard_paths.sort();
    guard_paths.dedup();
    let _access: Vec<_> = guard_paths
        .iter()
        .map(|path| crate::tree_access::lock(path))
        .collect();
    #[cfg(test)]
    for path in paths {
        crate::test_cleanup::record_checkout_work("prepare_writer_tree", path);
    }
    checked(
        crate::privilege::command("/usr/bin/chown")
            .args(["-R", "-P", &writer_cache_owner(account), "--"])
            .args(paths),
    )?;
    checked(
        crate::privilege::command("/usr/bin/setfacl")
            .args(["-R", "-P", "-b", "-k", "--"])
            .args(paths),
    )?;
    crate::permissions::repair_tree(crate::permissions::Policy::Private, paths)?;
    Ok(())
}

fn tenant_unit_properties() -> [&'static str; 3] {
    [
        "--property=TasksMax=4096",
        "--property=RestrictAddressFamilies=~AF_VSOCK",
        // Alternate syscall ABIs must not bypass the address-family filter.
        "--property=SystemCallArchitectures=native",
    ]
}

pub(crate) struct Sandbox {
    id: Uuid,
    root: PathBuf,
    home: PathBuf,
    cargo: PathBuf,
    rustup: PathBuf,
    path: String,
    read_only: Vec<String>,
    read_write: Vec<String>,
    pub(crate) wrapper: Option<PathBuf>,
    runtime: std::sync::Arc<RuntimeDir>,
    /// Keep a failed unit loaded after it exits, so the engine can read its
    /// Result (for example oom-kill) before resetting it. Only the engine sets
    /// this, because only the engine's finish path resets the unit again; any
    /// other caller lets systemd unload the unit as soon as it exits.
    retain_failed: bool,
    #[cfg(test)]
    parent_umask: u32,
    #[cfg(test)]
    memory_floor_gib: Option<u64>,
    /// The jobs slice's limits, as observed when this sandbox started its unit.
    /// The unit's `MemoryMax` is clamped to the slice's.
    slice_budget: std::sync::Mutex<Option<crate::systemd::JobMemoryBudget>>,
    // Operator fixtures reuse the production mount layout with the writer identity.
    user: &'static str,
    writer: bool,
    network: bool,
    tenant_vm: bool,
    private_pids: bool,
    boat_profile: bool,
    boat_memory_max: Option<u64>,
    /// Set for Boat jobs: the identity the PID namespace
    /// init drops to (see `pid_init`).
    pid_init: Option<pid_init::JobIdentity>,
    provider_unset_env: Vec<String>,
    /// The remote cache forwarders the unit's shell starts before its
    /// command, one per active tool; empty when the unit has no remote cache.
    cache_forwarders: Vec<crate::remote_cache::transport::Forwarder>,
}

pub(crate) struct RuntimeDir(
    PathBuf,
    std::sync::OnceLock<crate::cleanup::BackgroundDelete>,
    /// The unit's remote cache sockets, when it has any. They are served until
    /// this directory is reclaimed, which waits for the unit's last launch
    /// handle.
    std::sync::OnceLock<crate::remote_cache::transport::Sockets>,
);
pub(crate) fn recovered_runtime(state: &Path, id: Uuid) -> RuntimeDir {
    RuntimeDir(
        state.join("sandboxes").join(id.to_string()),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
    )
}
impl RuntimeDir {
    /// The write ledgers of the unit's cache sockets, empty unless they are a
    /// fill's write sockets.
    pub(crate) fn cache_ledgers(
        &self,
    ) -> Vec<std::sync::Arc<crate::remote_cache::gateway::WriteLedger>> {
        self.2
            .get()
            .map_or_else(Vec::new, |sockets| sockets.ledgers())
    }
    pub(crate) fn tracking(self, count: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.track(count);
        self
    }
    fn track(&self, count: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        assert!(self
            .1
            .set(crate::cleanup::BackgroundDelete::new(count))
            .is_ok());
    }
}
impl Drop for RuntimeDir {
    fn drop(&mut self) {
        // Stop serving the unit's cache sockets and remove them before the
        // directory holding them is reclaimed.
        drop(self.2.take());
        let _access = crate::tree_access::lock(&self.0);
        // A job can create mode-0700 temporary directories. Reclaim its private
        // home/tmp only after the last launch handle (and its cgroup) is gone.
        // Disk-pressure eviction may already have removed a dead job's tree
        // while this handle waited for the tree lock; nothing is left to do.
        if !self.0.exists() {
            return;
        }
        if let Err(error) = reclaim_runtime(&self.0) {
            tracing::warn!(%error, path = %self.0.display(), "could not reclaim private job directory ownership");
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() == io::ErrorKind::NotFound {
                return;
            }
            tracing::warn!(%error, path = %self.0.display(), "could not remove private job home");
        }
    }
}

const MAX_CARGO_LOCKFILES: usize = 128;
const NESTED_CARGO_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Optional fetches use only immutable snapshot entries, never warm untracked files.
fn cargo_lockfiles(
    workspace: &Path,
    tree: &str,
    skip: &mut impl FnMut(&Path, &str),
) -> io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    let entries = javascript_git(
        workspace,
        &["ls-tree".as_ref(), "-rz".as_ref(), tree.as_ref()],
    )?;
    let mut files = BTreeMap::new();
    for entry in entries
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (header, path) = entry.split_at(
            entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| io::Error::other("malformed Cargo Git tree entry"))?,
        );
        let path = PathBuf::from(std::ffi::OsString::from_vec(path[1..].to_vec()));
        files.insert(
            path,
            header.starts_with(b"100644 blob ") || header.starts_with(b"100755 blob "),
        );
    }
    let mut locks = Vec::new();
    let mut count = usize::from(files.contains_key(Path::new("Cargo.lock")));
    for (path, regular) in &files {
        if path != Path::new("Cargo.lock")
            && path.file_name().is_some_and(|name| name == "Cargo.lock")
        {
            count += 1;
            if count > MAX_CARGO_LOCKFILES {
                skip(path, "snapshot exceeds the 128 tracked Cargo.lock limit");
                continue;
            }
            let manifest = path.with_file_name("Cargo.toml");
            let valid = (|| {
                Ok::<_, io::Error>(
                    *regular
                        && files.get(&manifest) == Some(&true)
                        && optional_regular_file(&workspace.join(path))?
                        && optional_regular_file(&workspace.join(&manifest))?,
                )
            })();
            match valid {
                Ok(true) => locks.push(path.clone()),
                Ok(false) => skip(path, "nested prefetch requires tracked regular Cargo.lock and sibling Cargo.toml files"),
                Err(error) => skip(path, &error.to_string()),
            }
        }
    }
    Ok(locks)
}

fn bounded_cargo_fetch(command: &mut Command, limit: Duration) -> io::Result<()> {
    // GNU timeout owns a process group so its deadline also stops Cargo's Git
    // children. Give a TERM-resistant fetch only one additional second to exit.
    let mut bounded = Command::new("/usr/bin/timeout");
    bounded
        .args([
            "--verbose",
            "--kill-after=1s",
            &format!("{}s", limit.as_secs_f64()),
        ])
        .arg(command.get_program())
        .args(command.get_args());
    if let Some(directory) = command.get_current_dir() {
        bounded.current_dir(directory);
    }
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            bounded.env(name, value);
        } else {
            bounded.env_remove(name);
        }
    }
    let output = crate::systemd::output(&mut bounded)?;
    if output.status.success() {
        return Ok(());
    }
    let timeout = if matches!(output.status.code(), Some(124 | 137)) {
        format!("timed out after {limit:?}: ")
    } else {
        String::new()
    };
    Err(io::Error::other(format!(
        "{timeout}{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    )))
}

/// One trusted Cargo fetch of the prefetch, with the bound it runs under.
#[derive(Clone, Copy, Debug)]
enum CargoFetch {
    /// The workspace's root graph on the build server: no bound of its own.
    Root,
    /// One online or offline attempt of a tenant runner's root fetch.
    TenantAttempt(Duration),
    /// A nested lockfile's fetch, which is skipped when it fails.
    Nested(Duration),
}

/// Runs one fetch as a plain child of the daemon, on the host's paths.
fn run_cargo_fetch_on_host(command: &mut Command, fetch: CargoFetch) -> io::Result<()> {
    match fetch {
        CargoFetch::Root => checked(command),
        CargoFetch::TenantAttempt(limit) => cargo_fetch::bounded(command, limit),
        CargoFetch::Nested(limit) => bounded_cargo_fetch(command, limit),
    }
}

/// Qualifies the online tenant prefetch in a real snapshot helper, not merely
/// the daemon's namespace. A new scratch directory supplies an empty Cargo cache.
pub(crate) fn check_tenant_cargo_fetch(scratch: &Path) -> io::Result<()> {
    crate::privilege::run_helpers_directly_as_root()?;
    let rustup =
        PathBuf::from(std::env::var_os("RUSTUP_HOME").ok_or_else(|| {
            io::Error::other("RUSTUP_HOME is required for the tenant fetch check")
        })?);
    let mut config = crate::engine::Config::new(scratch, "state");
    config.tenant_vm = true;
    let snapshots = SnapshotSiblings {
        config: &config,
        siblings: Vec::new(),
    };
    cargo_fetch::check_online_via(scratch, &rustup, |command, limit| {
        let view =
            crate::sibling_snapshots::helper::View::prefetch(&scratch.join("workspace"), &[])?
                .keep(&scratch.join("cargo-home"), true)
                .keep(&rustup, false);
        snapshots.run(&view, command, CargoFetch::TenantAttempt(limit))
    })
}

/// In sibling snapshot mode the job's unit binds each sibling's snapshot at
/// the sibling's path (and at the worktree alias), and the Cargo prefetch runs
/// in helper units that show the same view, rather than whatever checkout is
/// on the host at that path.
pub(crate) struct SnapshotSiblings<'a> {
    pub(crate) config: &'a crate::engine::Config,
    /// Each sibling's snapshot directory and canonical path.
    pub(crate) siblings: Vec<(PathBuf, PathBuf)>,
}

impl SnapshotSiblings<'_> {
    /// Runs one fetch in a helper unit. Its limit is the configured prefetch
    /// limit, or the fetch's own bound when that is shorter.
    fn run(
        &self,
        view: &crate::sibling_snapshots::helper::View,
        command: &mut Command,
        fetch: CargoFetch,
    ) -> io::Result<()> {
        use crate::sibling_snapshots::helper::{self, Helper};
        let configured = Helper::CargoPrefetch.limit(self.config);
        let limit = match fetch {
            CargoFetch::Root => configured,
            CargoFetch::TenantAttempt(limit) | CargoFetch::Nested(limit) => limit.min(configured),
        };
        helper::run(self.config, Helper::CargoPrefetch, limit, command, view)
    }
}

#[cfg(test)]
fn prefetch_cargo_with(
    workspace: &Path,
    tree: &str,
    tenant_vm: bool,
    nested_limit: Duration,
    command: impl FnMut(&Path) -> io::Result<Command>,
    skip: impl FnMut(&Path, &str),
) -> io::Result<()> {
    prefetch_cargo_in(
        workspace,
        tree,
        tenant_vm,
        nested_limit,
        command,
        skip,
        run_cargo_fetch_on_host,
    )
}

fn prefetch_cargo_in(
    workspace: &Path,
    tree: &str,
    tenant_vm: bool,
    nested_limit: Duration,
    mut command: impl FnMut(&Path) -> io::Result<Command>,
    mut skip: impl FnMut(&Path, &str),
    mut run: impl FnMut(&mut Command, CargoFetch) -> io::Result<()>,
) -> io::Result<()> {
    // Only the root graph is an admission requirement. Experimental nested
    // crates may have stale locks or unavailable dependencies without breaking
    // unrelated jobs in the same repository.
    if workspace.join("Cargo.toml").is_file() {
        if tenant_vm {
            // The daemon alone resolves and downloads sources into persistent
            // tenant storage. A cache-only fallback can still run a warm job
            // during an outage; both attempts bound tool and cache-lock waits.
            cargo_fetch::prepare_via(
                || command(workspace),
                |command, limit| run(command, CargoFetch::TenantAttempt(limit)),
            )?;
        } else {
            run(&mut command(workspace)?, CargoFetch::Root)?;
        }
    }
    let locks = match cargo_lockfiles(workspace, tree, &mut skip) {
        Ok(locks) => locks,
        Err(error) => {
            skip(
                Path::new("."),
                &format!("nested lockfile discovery failed: {error}"),
            );
            return Ok(());
        }
    };
    for lockfile in locks {
        let fetched = command(&workspace.join(lockfile.parent().unwrap_or(Path::new(""))))
            .and_then(|mut command| run(&mut command, CargoFetch::Nested(nested_limit)));
        if let Err(error) = fetched {
            skip(&lockfile, &error.to_string());
        }
    }
    Ok(())
}

/// Populate sources as the trusted daemon, never as the cache-writing account.
/// Writer inputs are its mirror-materialized tip, not the requesting branch.
/// With `snapshots`, every fetch runs in a helper unit that shows the job's
/// sibling snapshots at the siblings' paths.
#[allow(clippy::too_many_arguments)]
fn prefetch_cargo_dependencies(
    writer: bool,
    tenant_vm: bool,
    state: &Path,
    workspace: &Path,
    tree: &str,
    dependency_routes: &BTreeMap<String, PathBuf>,
    cargo: &Path,
    rustup: &Path,
    snapshots: Option<&SnapshotSiblings<'_>>,
) -> io::Result<()> {
    let stage = if writer {
        "fetch integration-tip dependencies (Cargo.lock)"
    } else {
        "fetch job dependencies (Cargo.lock)"
    };
    let view = snapshots
        .map(|snapshots| {
            crate::sibling_snapshots::helper::View::prefetch(workspace, &snapshots.siblings).map(
                |view| {
                    let mut view = view.keep(cargo, true).keep(rustup, false);
                    for mirror in dependency_routes.values() {
                        view = view.keep(mirror, false);
                    }
                    view
                },
            )
        })
        .transpose()?;
    crate::setup_log::phase("fetch cargo dependencies", || {
        prefetch_cargo_in(
            workspace,
            tree,
            tenant_vm,
            NESTED_CARGO_FETCH_TIMEOUT,
            |directory| {
                let mut command = workspace::dependencies::fetch_command(
                    state,
                    directory,
                    dependency_routes,
                    cargo,
                    rustup,
                )?;
                // The tenant fallback re-runs this builder in offline mode, so
                // only the build server forces network on here.
                if !tenant_vm {
                    command.env("CARGO_NET_OFFLINE", "false");
                }
                Ok(command)
            },
            crate::setup_log::cargo_skip,
            |command, fetch| match (snapshots, &view) {
                (Some(snapshots), Some(view)) => snapshots.run(view, command, fetch),
                _ => run_cargo_fetch_on_host(command, fetch),
            },
        )
    })
    .map_err(|error| io::Error::other(fetch_failure(stage, &error.to_string())))
}

/// A stale lockfile is the caller's to fix, and it can't be fixed remotely:
/// a job's changes to tracked files never come back to the caller's checkout.
/// Say so, rather than leaving only Cargo's `--locked` message.
fn fetch_failure(stage: &str, error: &str) -> String {
    // Cargo has worded this both "needs to be updated but --locked was passed to
    // prevent this" and "cannot update the lock file ... because --locked was
    // passed to prevent this"; the shared tail matches both.
    if error.contains("--locked was passed to prevent this") {
        format!(
            "{stage}: {error}; Cargo.lock doesn't match Cargo.toml: update it in your \
             own checkout (cargo update runs there, not in a remote job, whose file \
             changes don't come back), then retry"
        )
    } else {
        format!("{stage}: {error}")
    }
}

/// The directories a job's Git may treat as repositories although another
/// account owns them: the workspace, and each place a sibling is mounted, its
/// canonical path and, when there is one, its `../<name>` alias. The sibling
/// files belong to the runner account (a snapshot, or an in-place checkout),
/// so without an entry Git inside the job refuses them as "dubious
/// ownership". Only these exact paths are listed, never a `*` wildcard.
fn safe_directories(workspace: &Path, sibling_mounts: &[(String, PathBuf)]) -> Vec<PathBuf> {
    std::iter::once(workspace.to_path_buf())
        .chain(
            sibling_mounts
                .iter()
                .map(|(_, destination)| destination.clone()),
        )
        .collect()
}

/// Adds one `safe.directory` entry per directory to the Git config file
/// `config`, the job's global config (`$HOME/.gitconfig`). Git reads this
/// setting only from global, system or command-line config, never from a
/// repository's own.
fn write_safe_directories(config: &Path, directories: &[PathBuf]) -> io::Result<()> {
    for directory in directories {
        checked(
            Command::new("git")
                .args(["config", "--file"])
                .arg(config)
                .args(["--add", "safe.directory"])
                .arg(directory),
        )?;
    }
    Ok(())
}

/// The read-only mounts that show each sibling to a job, as (systemd mount
/// spec, destination): one at the sibling's canonical path P and, when the
/// workspace's parent is not P's parent, one at the alias
/// `<workspace parent>/<name>` that `../<name>` resolves to. The source of
/// both is the sibling's snapshot when `snapshots` is given, otherwise the
/// in-place checkout at P.
fn sibling_mounts(
    workspace: &Path,
    siblings: &[SiblingSnapshot],
    snapshots: Option<&SnapshotSiblings<'_>>,
) -> io::Result<Vec<(String, PathBuf)>> {
    let mut mounts = Vec::new();
    for sibling in siblings {
        let path = Path::new(&sibling.canonical_path);
        let source = match snapshots {
            Some(snapshots) => snapshots
                .siblings
                .iter()
                .find(|(_, canonical)| canonical == path)
                .map(|(snapshot, _)| snapshot.as_path())
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "sibling {} has no resolved snapshot",
                        path.display()
                    ))
                })?,
            None => path,
        };
        mounts.push((
            if source == path {
                word(path)?
            } else {
                bind(source, path)?
            },
            path.to_path_buf(),
        ));
        let alias = workspace
            .parent()
            .ok_or_else(|| io::Error::other("workspace parent"))?
            .join(
                path.file_name()
                    .ok_or_else(|| io::Error::other("sibling name"))?,
            );
        if alias != path {
            mounts.push((bind(source, &alias)?, alias));
        }
    }
    Ok(mounts)
}

impl Sandbox {
    pub(crate) fn tracking_cleanup(
        self,
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.runtime.track(count);
        self
    }
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(crate) fn prepare(
        id: Uuid,
        state: &Path,
        workspace: &Path,
        target: &Path,
        siblings: &[SiblingSnapshot],
        wrapper: Option<&Path>,
        slots: &Path,
        limit: u32,
    ) -> io::Result<Self> {
        // An earlier isolation test may leave this checkout root owned by the
        // job account. Restore daemon access and root ownership before Git uses
        // the workspace for the next fixture.
        if fs::symlink_metadata(workspace)?.uid() != unsafe { libc::geteuid() } {
            restore_runner_access(workspace)?;
            restore_runner_checkout_root(workspace)?;
        }
        checked(Command::new("git").args(["init", "-q"]).arg(workspace))?;
        checked(Command::new("git").arg("-C").arg(workspace).args([
            "add",
            "-f",
            "--",
            ".",
            ":(exclude)target",
            ":(exclude)**/node_modules/**",
        ]))?;
        let tree = workspace::git(workspace, &["write-tree"])?;
        Self::prepare_with_dependencies(
            id,
            state,
            workspace,
            target,
            siblings,
            wrapper,
            slots,
            limit,
            &std::collections::BTreeMap::new(),
            &tree,
            &workspace.to_string_lossy(),
            false,
            true,
            None,
            &JavascriptTools::for_role(ToolchainRole::BuildServer),
            None,
        )
    }

    /// `snapshots` is given in sibling snapshot mode: the job then sees each
    /// sibling's snapshot at the sibling's path, and so does the Cargo
    /// prefetch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_with_dependencies(
        id: Uuid,
        state: &Path,
        workspace: &Path,
        target: &Path,
        siblings: &[SiblingSnapshot],
        wrapper: Option<&Path>,
        slots: &Path,
        limit: u32,
        dependency_routes: &std::collections::BTreeMap<String, PathBuf>,
        tree: &str,
        repository: &str,
        tenant_vm: bool,
        shared_compile_cache: bool,
        snapshots: Option<&SnapshotSiblings<'_>>,
        javascript: &JavascriptTools,
        remote: Option<&Transport>,
    ) -> io::Result<Self> {
        Self::prepare_role(
            id,
            state,
            workspace,
            target,
            siblings,
            wrapper,
            slots,
            limit,
            dependency_routes,
            tree,
            repository,
            false,
            tenant_vm,
            shared_compile_cache,
            snapshots,
            javascript,
            remote,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_writer(
        id: Uuid,
        state: &Path,
        workspace: &Path,
        target: &Path,
        wrapper: Option<&Path>,
        slots: &Path,
        limit: u32,
        dependency_routes: &BTreeMap<String, PathBuf>,
        tree: &str,
        javascript: &JavascriptTools,
        boat_profile: bool,
        remote: Option<&Transport>,
    ) -> io::Result<Self> {
        Self::prepare_role(
            id,
            state,
            workspace,
            target,
            &[],
            wrapper,
            slots,
            limit,
            dependency_routes,
            tree,
            &workspace.to_string_lossy(),
            true,
            false,
            true,
            None,
            javascript,
            remote,
        )
        .map(|sandbox| sandbox.with_boat_profile(boat_profile, true, None))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_role(
        id: Uuid,
        state: &Path,
        workspace: &Path,
        target: &Path,
        siblings: &[SiblingSnapshot],
        wrapper: Option<&Path>,
        slots: &Path,
        limit: u32,
        dependency_routes: &BTreeMap<String, PathBuf>,
        tree: &str,
        repository: &str,
        writer: bool,
        tenant_vm: bool,
        shared_compile_cache: bool,
        snapshots: Option<&SnapshotSiblings<'_>>,
        javascript: &JavascriptTools,
        remote: Option<&Transport>,
    ) -> io::Result<Self> {
        let declared_hosts = hosts::from_tree(workspace, tree)?;
        validate_account()?;
        let writer_account = if writer {
            Some(validate_writer_account()?)
        } else {
            None
        };
        // Fetch from a neutral directory: Cargo does not load an uploaded
        // .cargo/config.toml (credential providers/rustc wrappers can execute
        // programs). No build.rs or test runs in this trusted preparation step.
        {
            // Read-only default ACLs on the shared Cargo registry and Git cache
            // make newly fetched files readable to jobs without rescanning the
            // cache after every trusted dependency fetch.
            let cargo = PathBuf::from(
                std::env::var_os("CARGO_HOME")
                    .ok_or_else(|| io::Error::other("CARGO_HOME unset"))?,
            );
            let rustup = PathBuf::from(
                std::env::var_os("RUSTUP_HOME")
                    .ok_or_else(|| io::Error::other("RUSTUP_HOME unset"))?,
            );
            prefetch_cargo_dependencies(
                writer,
                tenant_vm,
                state,
                workspace,
                tree,
                dependency_routes,
                &cargo,
                &rustup,
                snapshots,
            )?;
        }
        // A snapshot's JavaScript dependencies were installed and its Git
        // directory made independent when it was created, so in snapshot mode
        // both steps below cover the workspace alone and never touch a
        // sibling's host path.
        let in_place_siblings = if snapshots.is_some() { &[] } else { siblings };
        // Offline jobs have no route to package registries. Resolve only from
        // a checked-in lockfile while still trusted, and never run package
        // lifecycle scripts as the daemon.
        if !writer {
            crate::setup_log::phase("prepare javascript dependencies", || {
                prepare_javascript_checkouts(
                    workspace,
                    repository,
                    tree,
                    in_place_siblings,
                    |checkout, repository, checkout_tree| {
                        prepare_javascript_dependencies(
                            state,
                            repository,
                            checkout,
                            checkout_tree,
                            javascript,
                        )
                    },
                )
            })?;
        }
        // Each checkout's Git metadata is read-only inside the job. Dissociate
        // it first so Git needs neither a mirror mount nor the daemon's home.
        for path in std::iter::once(workspace.to_path_buf()).chain(
            in_place_siblings
                .iter()
                .map(|s| PathBuf::from(&s.canonical_path)),
        ) {
            if path.join(".git/objects/info/alternates").is_file() {
                crate::setup_log::phase("dissociate checkout", || {
                    workspace::git(&path, &["repack", "-a", "-d"])
                })?;
                fs::remove_file(path.join(".git/objects/info/alternates"))?;
            }
        }
        // Each job gets a private root tree so its visible mount ancestors can
        // stay root-owned even when the host checkout lives under an operator's
        // writable home directory.
        let runtime = std::sync::Arc::new(RuntimeDir(
            state.join("sandboxes").join(id.to_string()),
            std::sync::OnceLock::new(),
            std::sync::OnceLock::new(),
        ));
        let root = runtime.0.join("root");
        create_sandbox_root(&root)?;
        let host_home = runtime.0.join("home");
        let tmp = runtime.0.join("tmp");
        fs::create_dir_all(&tmp)?;
        let home = PathBuf::from("/motor-home");
        // Only existing output inodes are writable, never the directory. A job
        // must not replace a log with a symlink that the trusted reader follows.
        let output = runtime.0.join("output");
        fs::create_dir_all(&output)?;
        let mut grant_paths = Vec::new();
        let mut writer_grants = Vec::new();
        let output_names = cache_output_names(
            remote.is_some_and(|r| r.has_tool(crate::remote_cache::gateway::Tool::Ccache)),
        );
        for name in &output_names {
            let file = output.join(name);
            if *name == "exit" {
                // Allocate and flush the exit block before job code can fill the
                // volume. The wrapper overwrites it without truncation.
                fs::write(&file, [b' '; 32])?;
                File::open(&file)?.sync_all()?;
            } else {
                fs::write(&file, "")?;
            }
            grant_paths.push(file);
        }
        // Supply only the default names and validated repository loopback
        // aliases: the job root has no resolver or host /etc/hosts to inherit.
        let hosts = runtime.0.join("hosts");
        fs::write(&hosts, format!("{JOB_HOSTS}{}", declared_hosts.concat()))?;
        let cargo = home.join("cargo");
        let cached_rust = shared_compile_cache && workspace.join("Cargo.toml").is_file();
        let wrapper = wrapper.filter(|_| shared_compile_cache);
        let mut mount_destinations = vec![
            workspace.to_path_buf(),
            target.to_path_buf(),
            home.clone(),
            PathBuf::from("/tmp"),
        ];
        fs::create_dir_all(host_home.join("tmp"))?;
        fs::create_dir_all(host_home.join("cargo/registry"))?;
        fs::create_dir_all(host_home.join("cargo/git"))?;
        let sibling_mounts = sibling_mounts(workspace, siblings, snapshots)?;
        write_safe_directories(
            &host_home.join(".gitconfig"),
            &safe_directories(workspace, &sibling_mounts),
        )?;
        prepare_job_home(&host_home)?;
        let shared_cargo = PathBuf::from(
            std::env::var_os("CARGO_HOME").ok_or_else(|| io::Error::other("CARGO_HOME unset"))?,
        );
        let rustup = PathBuf::from(
            std::env::var_os("RUSTUP_HOME").ok_or_else(|| io::Error::other("RUSTUP_HOME unset"))?,
        );
        let mut read_only = Vec::new();
        for path in [
            "/usr",
            "/bin",
            "/lib",
            "/lib64",
            "/etc/ld.so.cache",
            "/etc/passwd",
            "/etc/group",
            "/etc/nsswitch.conf",
            "/etc/alternatives",
            "/etc/ssl/certs",
            // OpenSSL's default config, /usr/lib/ssl/openssl.cnf, is a symlink
            // to this file; without it TLS clients and the openssl CLI fail.
            "/etc/ssl/openssl.cnf",
        ] {
            if Path::new(path).exists() {
                read_only.push(word(Path::new(path))?);
                mount_destinations.push(PathBuf::from(path));
            }
        }
        read_only.push(word(&rustup)?);
        mount_destinations.push(rustup.clone());
        let cargo_bin = shared_cargo.join("bin");
        read_only.push(word(&cargo_bin)?);
        mount_destinations.push(cargo_bin);
        let mut path = format!(
            "{}:/usr/local/bin:/usr/bin:/bin",
            shared_cargo.join("bin").display()
        );
        if let Some((mount, destination)) = job_bun_mount(javascript)? {
            read_only.push(mount);
            path = format!("{}:{path}", destination.display());
            mount_destinations.push(destination);
        }
        // The shared source/index trees are immutable. Cargo's locks and cache
        // bookkeeping live in the job's private CARGO_HOME, not the shared one.
        for name in ["registry", "git"] {
            let source = shared_cargo.join(name);
            if source.is_dir() {
                let destination = cargo.join(name);
                read_only.push(bind(&source, &destination)?);
                mount_destinations.push(destination);
            }
        }
        let mut read_write = vec![
            word(workspace)?,
            word(target)?,
            bind(&host_home, &home)?,
            bind(&tmp, Path::new("/tmp"))?,
        ];
        if cached_rust && writer {
            fs::create_dir_all(workspace.join("target"))?;
            read_write.push(bind(target, &workspace.join("target"))?);
            mount_destinations.push(workspace.join("target"));
        }
        compile_cache_mounts(
            state,
            workspace,
            target,
            writer,
            shared_compile_cache,
            &mut read_only,
            &mut read_write,
            &mut mount_destinations,
        )?;
        read_only.push(bind(&hosts, Path::new("/etc/hosts"))?);
        mount_destinations.push(PathBuf::from("/etc/hosts"));
        read_only.push(bind(&output, Path::new("/motor-output"))?);
        mount_destinations.push(PathBuf::from("/motor-output"));
        for name in &output_names {
            let destination = Path::new("/motor-output").join(name);
            read_write.push(bind(&output.join(name), &destination)?);
            mount_destinations.push(destination);
        }
        if workspace.join(".git").is_dir() {
            let git = workspace.join(".git");
            read_only.push(word(&git)?);
            mount_destinations.push(git);
        }
        for (mount, destination) in sibling_mounts {
            read_only.push(mount);
            mount_destinations.push(destination);
        }
        let tools = runtime.0.join("tools");
        // With a remote cache the tool directory carries the forwarder, so it
        // is mounted even where the shared compile cache is off.
        if cached_rust || wrapper.is_some() || remote.is_some() || writer {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir_all(&tools)?;
            fs::set_permissions(&tools, fs::Permissions::from_mode(0o755))?;
            if cached_rust {
                let cache_wrapper = tools.join("sccache");
                fs::copy(
                    prepare_cache_readiness_probe(state)?,
                    tools.join("sccache.ready"),
                )?;
                fs::set_permissions(
                    tools.join("sccache.ready"),
                    fs::Permissions::from_mode(0o755),
                )?;
                fs::write(&cache_wrapper, include_str!("sccache-wrapper.sh"))?;
                fs::set_permissions(&cache_wrapper, fs::Permissions::from_mode(0o755))?;
                fs::write(
                    cache_wrapper.with_extension("backend"),
                    shared_cargo
                        .join("bin/sccache")
                        .to_string_lossy()
                        .as_bytes(),
                )?;
                fs::set_permissions(
                    cache_wrapper.with_extension("backend"),
                    fs::Permissions::from_mode(0o644),
                )?;
                path = format!("/motor-tools:{path}");
            }
            if writer {
                fs::write(
                    tools.join("writer-preflight.py"),
                    crate::writer::fill::PREFLIGHT,
                )?;
                fs::set_permissions(
                    tools.join("writer-preflight.py"),
                    fs::Permissions::from_mode(0o644),
                )?;
                // The writer shell decides from the working tree whether
                // Cargo has anything to check.
                let cargo_check = tools.join("writer-cargo-check.sh");
                fs::write(&cargo_check, crate::writer::fill::CARGO_CHECK)?;
                fs::set_permissions(cargo_check, fs::Permissions::from_mode(0o755))?;
            }
            if remote.is_some() {
                // The forwarder holds no key, so the unit may read and run it.
                let forwarder = tools.join(transport::FORWARDER_NAME);
                fs::copy(prepare_cache_forwarder(state)?, &forwarder)?;
                fs::set_permissions(&forwarder, fs::Permissions::from_mode(0o755))?;
            }
            read_only.push(bind(&tools, Path::new("/motor-tools"))?);
            mount_destinations.push(PathBuf::from("/motor-tools"));
        }
        let mut cache_forwarders = Vec::new();
        if let Some(remote) = remote {
            let directory = runtime.0.join("gateway");
            let job = id.to_string();
            let (role, account) = if writer {
                (transport::Role::Fill, WRITER_USER)
            } else {
                (transport::Role::Job(&job), USER)
            };
            let sockets = remote.open(&directory, role)?;
            // Connecting needs write access to the socket file. Only this
            // unit's account gets it, and the directory is bound into this
            // unit's sandbox alone.
            grant_sockets(&sockets.paths(), account)?;
            read_only.push(bind(&directory, Path::new(transport::SANDBOX_SOCKETS))?);
            mount_destinations.push(PathBuf::from(transport::SANDBOX_SOCKETS));
            cache_forwarders = sockets.forwarders().to_vec();
            if runtime.2.set(sockets).is_err() {
                return Err(io::Error::other("sandbox cache sockets were opened twice"));
            }
        }
        // Copy the supervisor into the read-only tool mount. Bind only existing
        // slot locks read-write so jobs can acquire slots but cannot replace
        // locks or change the shared slot limit.
        let wrapper = if let Some(wrapper) = wrapper {
            let copy = tools.join("prefrontal-build-slot");
            fs::copy(wrapper, &copy)?;
            fs::write(copy.with_extension("root"), "/motor-slots")?;
            fs::write(
                copy.with_extension("cache"),
                if cached_rust {
                    PathBuf::from("/motor-tools/sccache")
                } else {
                    PathBuf::new()
                }
                .to_string_lossy()
                .as_bytes(),
            )?;
            read_only.push(bind(slots, Path::new("/motor-slots"))?);
            mount_destinations.push(PathBuf::from("/motor-slots"));
            for name in std::iter::once("queue.lock".to_string())
                .chain((0..limit).map(|i| format!("{i}.lock")))
            {
                let path = slots.join(&name);
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&path)?;
                grant_paths.push(path.clone());
                if writer {
                    writer_grants.push(path.clone());
                }
                let slot_destination = Path::new("/motor-slots").join(name);
                read_write.push(bind(&path, &slot_destination)?);
                mount_destinations.push(slot_destination);
            }
            Some(PathBuf::from("/motor-tools/prefrontal-build-slot"))
        } else {
            None
        };
        // Build the full destination tree before root owns it; the trusted
        // daemon cannot create deeper mount ancestors after changing a parent.
        let mut root_owned = std::collections::BTreeSet::from([root.clone(), output.clone()]);
        for destination in &mount_destinations {
            prepare_root_owned_mount_ancestors(&root, destination)?;
            root_owned.extend(root_owned_mount_ancestors(&root, destination)?);
        }
        let mut root_owned: Vec<_> = root_owned.into_iter().collect();
        root_owned.sort_by_key(|path| path.components().count());
        set_sandbox_directory_owner_modes(&[
            (&root_owned, "0755"),
            (std::slice::from_ref(&tmp), "1777"),
        ])?;
        // Target repair changes owner permissions only. Complete it before the
        // shared ACL grant, then restore the checkout root's final ACL mask with
        // chmod, just as for an individual workspace grant.
        prepare_job_writable_target(state, target)
            .map_err(|error| io::Error::other(format!("prepare job target: {error}")))?;
        grant_paths.extend([workspace.to_path_buf(), tmp.clone()]);
        if !target.starts_with(workspace) {
            grant_paths.push(target.to_path_buf());
        }
        self::grant_paths(&grant_paths.iter().map(PathBuf::as_path).collect::<Vec<_>>())?;
        prepare_job_checkout_root(workspace)?;
        if let Some(account) = writer_account {
            prepare_writer_trees(&[workspace, target, &host_home, &tmp], account)?;
            for name in &output_names {
                writer_grants.push(output.join(name));
            }
            checked(
                crate::privilege::command("/usr/bin/setfacl")
                    .args(["-m", &format!("u:{WRITER_USER}:rw"), "--"])
                    .args(writer_grants),
            )?;
        }
        Ok(Self {
            // Caller job IDs can be reused in a different runner instance.
            // The system-manager namespace is machine-wide, so never let a
            // duplicate request select (or cancel) another instance's unit.
            id: Uuid::now_v7(),
            root,
            home,
            cargo,
            rustup,
            path,
            read_only,
            read_write,
            wrapper,
            runtime,
            retain_failed: false,
            #[cfg(test)]
            parent_umask: 0o022,
            #[cfg(test)]
            memory_floor_gib: None,
            slice_budget: std::sync::Mutex::new(None),
            user: if writer { WRITER_USER } else { USER },
            writer,
            network: false,
            tenant_vm: false,
            private_pids: true,
            boat_profile: false,
            boat_memory_max: None,
            pid_init: None,
            provider_unset_env: Vec::new(),
            cache_forwarders,
        })
    }

    /// See [`Sandbox::retain_failed`].
    pub(crate) fn retaining_failed_units(mut self) -> Self {
        self.retain_failed = true;
        self
    }
    pub(crate) fn in_tenant_vm(mut self, enabled: bool) -> Self {
        self.tenant_vm = enabled;
        self
    }
    pub(crate) fn with_boat_profile(
        mut self,
        enabled: bool,
        private_pids: bool,
        memory_max: Option<u64>,
    ) -> Self {
        self.boat_profile = enabled;
        self.private_pids = private_pids;
        self.boat_memory_max = if enabled { memory_max } else { None };
        self
    }
    pub(crate) fn with_boat_temporary_paths(mut self) -> io::Result<Self> {
        // Do not depend on systemd's version-specific PrivateTmp /var/tmp
        // backing. Bind /var/tmp to runtime/tmp, the same quota-covered XFS
        // scratch already mounted at /tmp.
        let destination = Path::new("/var/tmp");
        prepare_root_owned_mount_ancestors(&self.root, destination)?;
        let ancestors = root_owned_mount_ancestors(&self.root, destination)?;
        set_sandbox_directory_owner_modes(&[(&ancestors, "0755")])?;
        self.read_write
            .push(bind(&self.runtime.0.join("tmp"), destination)?);
        Ok(self)
    }
    /// The job identity for Boat units that cannot use PrivatePIDs. Only those
    /// units use it; see [`Sandbox::uses_pid_init`].
    pub(crate) fn with_pid_init(mut self, identity: Option<pid_init::JobIdentity>) -> Self {
        self.pid_init = identity;
        self
    }
    /// Boat sandboxes start the unit as root and run
    /// the runner's PID namespace init in front of the wrapper.
    fn uses_pid_init(&self) -> bool {
        self.boat_profile && !self.private_pids
    }
    fn setup_error_path(&self) -> PathBuf {
        self.runtime
            .0
            .join("output")
            .join(pid_init::SETUP_ERROR_FILE)
    }
    pub(crate) fn without_provider_environment(mut self, names: &[String]) -> Self {
        self.provider_unset_env = names.to_vec();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_memory_floor_gib(mut self, floor_gib: Option<u64>) -> Self {
        self.memory_floor_gib = floor_gib;
        self
    }

    /// Pin the jobs slice limits a unit would observe at launch, without
    /// running systemctl; production reads them from the slice itself.
    #[cfg(test)]
    pub(crate) fn with_slice_budget(self, budget: crate::systemd::JobMemoryBudget) -> Self {
        self.slice_budget.lock().unwrap().replace(budget);
        self
    }

    pub(crate) fn with_network(mut self) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let resolver = self.runtime.0.join("resolv.conf");
        fs::write(&resolver, "nameserver 1.1.1.1\nnameserver 9.9.9.9\n")?;
        fs::set_permissions(&resolver, fs::Permissions::from_mode(0o444))?;
        grant(&resolver)?;
        self.read_only
            .push(bind(&resolver, Path::new("/etc/resolv.conf"))?);
        self.network = true;
        Ok(self)
    }

    fn command(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        durable: bool,
        cpus: &[u32],
        requested_vcpus: u32,
    ) -> io::Result<Command> {
        if self.boat_profile && self.writer {
            return Err(io::Error::other(
                "Boat jobs cannot use dependency writer units",
            ));
        }
        let user = self.user;
        let pid_init = if self.uses_pid_init() {
            Some(self.pid_init.as_ref().ok_or_else(|| {
                io::Error::other(
                    "a Boat job without PrivatePIDs needs the job account's identity for its PID namespace init",
                )
            })?)
        } else {
            None
        };
        let mut cmd = crate::privilege::command("/usr/bin/systemd-run");
        // sccache statistics are captured whenever its integration is on, which
        // a build-server job shows by its local cache directory and a Boat or
        // tenant VM job by its wrapper alone; neither sets the other's marker.
        let cached = env.iter().any(|(key, _)| key == "SCCACHE_DIR")
            || self
                .cache_forwarders
                .iter()
                .any(|forwarder| forwarder.tool() == crate::remote_cache::gateway::Tool::Sccache);
        // Output is a read-only directory with fixed writable file bind mounts,
        // so an exit file cannot be created or replaced by rename here. A normal
        // exit overwrites a preallocated block without truncation; if the shell is killed mid-write, the failed
        // unit retains its status for the engine's systemd fallback to read.
        let shell = if durable && self.writer {
            "IFS= read -r -n 1 || exit 125; umask 022; /bin/bash /motor-tools/writer-cargo-check.sh >/motor-output/stdout 2>/motor-output/stderr; code=$?; if test \"$code\" = 0; then /bin/bash -c \"$1\" </dev/null >>/motor-output/stdout 2>>/motor-output/stderr; code=$?; fi; sccache --show-stats --stats-format=json >/motor-output/cache-stats 2>>/motor-output/stderr || true; printf '%s\\n' \"$code\" 1<>/motor-output/exit; exit \"$code\""
        } else if durable && cached {
            "IFS= read -r -n 1 || exit 125; umask 022; /bin/bash -c \"$1\" </dev/null >/motor-output/stdout 2>/motor-output/stderr; code=$?; sccache --show-stats --stats-format=json >/motor-output/cache-stats 2>/dev/null || true; printf '%s\\n' \"$code\" 1<>/motor-output/exit; exit \"$code\""
        } else if durable {
            "IFS= read -r -n 1 || exit 125; umask 022; /bin/bash -c \"$1\" </dev/null >/motor-output/stdout 2>/motor-output/stderr; code=$?; printf '%s\\n' \"$code\" 1<>/motor-output/exit; exit \"$code\""
        } else if self.writer {
            "IFS= read -r -n 1 || exit 125; umask 022; /bin/bash /motor-tools/writer-cargo-check.sh || exit $?; exec /bin/bash -c \"$1\" </dev/null"
        } else {
            "IFS= read -r -n 1 || exit 125; umask 022; exec /bin/bash -c \"$1\" </dev/null"
        };
        let shell = with_ccache_statistics(
            shell,
            durable
                && self
                    .cache_forwarders
                    .iter()
                    .any(|f| f.tool() == crate::remote_cache::gateway::Tool::Ccache),
        );
        let shell = with_cache_forwarders(&shell, &self.cache_forwarders)?;
        cmd.args([
            "--wait",
            "--service-type=exec",
            // The job command is shell source, not systemd command-line syntax.
            // Leave dollar expansion to Bash inside the job's environment.
            "--expand-environment=no",
        ])
        .arg(format!("--unit={}", unit(self.id)));
        cmd.args(crate::systemd::JOB_MEMORY_PROPERTIES);
        if self.tenant_vm {
            cmd.args(tenant_unit_properties());
        }
        if self.boat_profile {
            // No ProcSubset=pid: like build-server jobs, Boat jobs see /proc/stat,
            // /proc/meminfo and the other non-process entries, which tools such
            // as ps read. A Boat sandbox VM admits one job, so those entries
            // describe only that job's machine. ProtectProc=invisible below still
            // hides other accounts' processes, and ProtectKernelTunables keeps
            // /proc/sys read-only.
            let names = crate::systemd::provider_names(
                "",
                self.provider_unset_env
                    .iter()
                    .cloned()
                    .chain(env.iter().map(|(key, _)| key.clone())),
            )?;
            cmd.arg(format!("--property=UnsetEnvironment={}", names.join(" ")));
            cmd.arg("--property=TasksMax=4096");
            cmd.arg("--property=CPUQuotaPeriodSec=100ms");
            cmd.arg(format!(
                "--property=CPUQuota={}%",
                u64::from(requested_vcpus) * 100
            ));
        }
        if self.private_pids {
            cmd.arg("--property=PrivatePIDs=yes");
        }
        if durable {
            // Inheriting --pipe stderr would let a job forge launcher accounting
            // through its parent's descriptors. Only stdin crosses the boundary.
            for (property, mode, path) in [
                ("StandardInput", "file", self.runtime.0.join("gate")),
                (
                    "StandardOutput",
                    "append",
                    self.runtime.0.join("output/stdout"),
                ),
                (
                    "StandardError",
                    "append",
                    self.runtime.0.join("output/stderr"),
                ),
            ] {
                let path = path
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF-8 sandbox stream path"))?;
                cmd.arg(format!("--property={property}={mode}:{path}"));
            }
        } else {
            cmd.args(["--quiet", "--pipe"]);
        }
        if self.retain_failed {
            cmd.arg("--property=CollectMode=inactive");
        } else {
            cmd.arg("--collect");
        }
        if !cpus.is_empty() {
            cmd.arg(format!(
                "--property=AllowedCPUs={}",
                crate::cpu::format_list(cpus)
            ));
        }
        // The memory limit follows requested_vcpus, even when placement is empty or smaller.
        #[cfg(test)]
        let memory_max = crate::backend::memory_max(requested_vcpus, self.memory_floor_gib);
        #[cfg(not(test))]
        let memory_max = crate::backend::memory_max(requested_vcpus);
        // A job's limit is a ceiling, while the shared slice's is a budget: a
        // request larger than the slice can never be honoured, so the unit asks
        // for the slice's own `MemoryMax` instead. An overrun still ends as an
        // OOM kill, either from the slice or from the unit's own limit. With no
        // observed budget (a command built before any unit started), the
        // requested limit stands as it always has.
        let memory_max = self.boat_memory_max.unwrap_or(memory_max);
        let memory_max = self
            .slice_budget
            .lock()
            .unwrap()
            .map_or(memory_max, |budget| memory_max.min(budget.max));
        cmd.arg(format!("--property=MemoryMax={memory_max}"));
        // Match the host's executable /dev/shm policy while bounding this
        // job's private tmpfs to the same memory limit as the service.
        cmd.arg(format!(
            "--property=TemporaryFileSystem=/dev/shm:mode=1777,nosuid,nodev,size={memory_max}"
        ));
        // The PID namespace init must start as root. It drops to the same
        // identity itself, from the job account's numeric ids.
        if pid_init.is_none() {
            cmd.arg(format!("--property=User={user}"))
                .arg(format!("--property=Group={user}"));
        }
        for property in [
            "NoNewPrivileges=yes",
            "CapabilityBoundingSet=",
            "ProtectSystem=strict",
            "ProtectHome=tmpfs",
            "PrivateTmp=yes",
            "PrivateDevices=yes",
            "PrivateNetwork=yes",
            "ProtectProc=invisible",
            "IgnoreSIGPIPE=no",
            // RestrictSUIDSGID is deliberately off. Its seccomp filter cannot
            // inspect openat2's arguments, so it fails every openat2 with ENOSYS,
            // and GNU tar 1.35 extracts through openat2 without falling back.
            // Set-id bits a job writes gain nothing: every job runs under
            // NoNewPrivileges as the one job account, and nothing outside a job
            // executes job-written files.
            "RestrictNamespaces=yes",
            "ProtectKernelTunables=yes",
            "ProtectKernelModules=yes",
            "ProtectControlGroups=yes",
            "MountAPIVFS=yes",
            "KillMode=control-group",
            "TimeoutStopSec=2s",
            "SendSIGKILL=yes",
        ] {
            // The PID namespace init needs these capabilities to create the
            // namespace, mount its /proc and change identity, and the seccomp
            // filter must let it create a PID namespace. It empties the
            // bounding set and adds its own filter refusing new PID namespaces
            // before the wrapper runs, so the job ends up with the values below.
            let property = match property {
                "CapabilityBoundingSet=" if pid_init.is_some() => {
                    format!("CapabilityBoundingSet={}", pid_init::UNIT_CAPABILITIES)
                }
                "RestrictNamespaces=yes" if pid_init.is_some() => "RestrictNamespaces=pid".into(),
                property => property.to_owned(),
            };
            cmd.arg(format!("--property={property}"));
        }
        #[cfg(test)]
        let parent_umask = self.parent_umask;
        #[cfg(not(test))]
        let parent_umask = 0o022;
        cmd.arg(format!("--property=UMask={parent_umask:04o}"));
        let mut read_write = self.read_write.clone();
        if pid_init.is_some() {
            read_write.push(bind(
                &self.setup_error_path(),
                &Path::new("/motor-output").join(pid_init::SETUP_ERROR_FILE),
            )?);
        }
        for (key, value) in [
            ("RootDirectory", word(&self.root)?),
            ("WorkingDirectory", word(cwd)?),
            ("BindReadOnlyPaths", self.read_only.join(" ")),
            ("BindPaths", read_write.join(" ")),
        ] {
            cmd.arg(format!("--property={key}={value}"));
        }
        let mut env: BTreeMap<_, _> = env.iter().cloned().collect();
        let tmp = self.home.join("tmp").to_string_lossy().into_owned();
        for (key, value) in [
            ("HOME", self.home.to_string_lossy().into_owned()),
            ("CARGO_HOME", self.cargo.to_string_lossy().into_owned()),
            ("RUSTUP_HOME", self.rustup.to_string_lossy().into_owned()),
            ("CARGO_NET_OFFLINE", "true".into()),
            ("USER", user.into()),
            ("LOGNAME", user.into()),
            ("SHELL", "/bin/bash".into()),
            // Override inherited/caller paths even if a client bypasses the
            // engine's allowlist. All temp-directory conventions use job scratch.
            ("TMPDIR", tmp.clone()),
            ("TMP", tmp.clone()),
            ("TEMP", tmp),
            ("PATH", self.path.clone()),
        ] {
            env.insert(key.into(), value);
        }
        if pid_init.is_some() {
            pid_init::refuse_loader_environment(env.keys().map(String::as_str))?;
        }
        for (key, value) in env {
            cmd.arg(format!("--setenv={key}={value}"));
        }
        // Hold the service at this stdin gate until the durable job record is
        // written; EOF on daemon crash exits without running the command. The
        // one-byte release switches the command's stdin to /dev/null so the
        // still-open gate writer cannot block ordinary input reads.
        // Cancellation cannot precede creation of an untracked privileged job.
        let wrapper = ["/bin/bash", "-c", &*shell, user, command];
        match pid_init {
            Some(identity) => cmd.args(pid_init::exec_start(identity, &wrapper)),
            None => cmd.args(wrapper),
        };
        Ok(cmd)
    }

    #[cfg(test)]
    pub(crate) fn launch(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
    ) -> io::Result<platform::Launch> {
        self.launch_inner(command, cwd, env, false, &[], 0)
    }
    pub(crate) fn launch_durable(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        cpus: &[u32],
        requested_vcpus: u32,
    ) -> io::Result<platform::Launch> {
        self.launch_inner(command, cwd, env, true, cpus, requested_vcpus)
    }
    fn launch_inner(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        durable: bool,
        cpus: &[u32],
        requested_vcpus: u32,
    ) -> io::Result<platform::Launch> {
        // Resolved before the unit exists and kept for the command built below.
        self.slice_budget
            .lock()
            .unwrap()
            .replace(crate::systemd::read_job_memory_budget()?);
        #[cfg(test)]
        crate::test_cleanup::record_unit(self.id, cwd);
        #[cfg(test)]
        let test_guard = crate::test_cleanup::LaunchGuard(platform::ProcessIdentity {
            pid: 0,
            start: 0,
            unit: Some(self.id),
        });
        if self.uses_pid_init() {
            use std::os::unix::fs::OpenOptionsExt;
            // The init reports a setup failure here, while still root. The file
            // is root-owned 0600, so the job cannot open it afterwards.
            let mut report = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(self.setup_error_path())?;
            report.write_all(&[b' '; pid_init::REPORT_BYTES])?;
            report.sync_all()?;
        }
        let gate = if durable {
            use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
            let path = self.runtime.0.join("gate");
            let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // Opening both ends avoids blocking systemd's pre-exec FIFO open.
            // The manager opens stdin before chroot/User=; jobs cannot reopen
            // this daemon-owned FIFO for writing.
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?,
            )
        } else {
            None
        };
        let mut child = self
            .command(command, cwd, env, durable, cpus, requested_vcpus)?
            .env_clear()
            .stdin(if durable {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let started = Instant::now();
        if !crate::systemd::wait_until_active(
            &unit(self.id),
            || Ok(child.try_wait()?.is_some() || started.elapsed() > Duration::from_secs(15)),
            std::thread::sleep,
        )? {
            discard_unstarted(self.id);
            let _ = child.kill();
            let output = child.wait_with_output()?;
            // A PID namespace init that failed before the unit counted as
            // started left its reason here, not in systemd-run's output.
            let setup = fs::read_to_string(self.setup_error_path()).unwrap_or_default();
            return Err(io::Error::other(format!(
                "sandbox did not start: {}{}",
                String::from_utf8_lossy(&output.stderr),
                setup.trim_end()
            )));
        }
        let active_at = Instant::now();
        if let Err(error) = allow_low_ports(self.id) {
            discard_unstarted(self.id);
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("sandbox did not start: {error}")));
        }
        if self.network {
            if let Err(error) = crate::network::linux::attach(self.id) {
                discard_unstarted(self.id);
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other(format!(
                    "sandbox network setup failed: {error}"
                )));
            }
            tracing::info!(job_id = %self.id, elapsed_us = active_at.elapsed().as_micros(), "network active-to-attach finished");
            #[cfg(test)]
            if std::env::var_os("MOTOR_NET_MEASURE_ATTACH").is_some() {
                println!("MOTOR_NET_ATTACH_US {}", active_at.elapsed().as_micros());
            }
        }
        let Some(mut identity) = platform::identity(child.id() as i32) else {
            // The launcher can be OOM-killed after the service became active.
            // The stdin gate is still closed: discard the unit before reporting
            // a setup refusal, rather than abandoning an unrecorded service.
            discard_unstarted(self.id);
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(
                "sandbox did not start: sandbox client vanished",
            ));
        };
        identity.unit = Some(self.id);
        Ok(platform::Launch {
            #[cfg(test)]
            _test_guard: test_guard,
            durable_output: durable,
            identity,
            stdout: unsafe { File::from_raw_fd(child.stdout.take().unwrap().into_raw_fd()) },
            stderr: unsafe { File::from_raw_fd(child.stderr.take().unwrap().into_raw_fd()) },
            gate: gate.unwrap_or_else(|| unsafe {
                File::from_raw_fd(child.stdin.take().unwrap().into_raw_fd())
            }),
            runtime: Some(self.runtime.clone()),
        })
    }
}

#[cfg(test)]
mod tests;

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_cache_mounts(
    state: &Path,
    workspace: &Path,
    target: &Path,
    writer: bool,
    shared_compile_cache: bool,
    read_only: &mut Vec<String>,
    read_write: &mut Vec<String>,
    destinations: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if !workspace.join("Cargo.toml").is_file() {
        return Ok(());
    }
    // Every Rust job builds in this fixed folder, bound to its own target
    // folder, whether or not the runner has a shared compile cache. Boat and
    // tenant jobs thus see the same build paths as build-server jobs.
    read_write.push(bind(target, Path::new(crate::compile_cache::TARGET))?);
    destinations.push(PathBuf::from(crate::compile_cache::TARGET));
    if !shared_compile_cache {
        return Ok(());
    }
    // Eviction may remove this root between setup and unit launch. An
    // optional bind lets the job start and compile misses locally.
    let cache = state.join("sccache/fleet");
    if writer {
        read_write.push(bind(&cache, Path::new(crate::compile_cache::CACHE))?);
    } else {
        read_only.push(format!(
            "-{}",
            bind(&cache, Path::new(crate::compile_cache::CACHE))?
        ));
    }
    destinations.push(PathBuf::from(crate::compile_cache::CACHE));
    Ok(())
}

fn prepare_cache_readiness_probe(state: &Path) -> io::Result<PathBuf> {
    prepare_embedded_tool(state, "sccache-ready", include_str!("sccache-ready.rs.in"))
}

/// The remote cache forwarder, built from the source embedded in the runner.
pub(crate) fn prepare_cache_forwarder(state: &Path) -> io::Result<PathBuf> {
    prepare_embedded_tool(
        state,
        transport::FORWARDER_NAME,
        include_str!("cache-forwarder.rs.in"),
    )
}

/// Compiles a dependency-free helper from `source`, embedded in the runner,
/// into the state directory once per source digest.
fn prepare_embedded_tool(state: &Path, name: &str, source: &str) -> io::Result<PathBuf> {
    use fs2::FileExt;
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(source));
    let output = state.join(format!("{name}-{digest}"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(output.with_extension("lock"))?;
    lock.lock_exclusive()?;
    if !output.is_file() {
        let source_path = output.with_extension("rs");
        fs::write(&source_path, source)?;
        let temporary = output.with_extension(format!("{}.tmp", Uuid::now_v7()));
        let result = checked(
            Command::new("rustc")
                .arg(&source_path)
                .args(["--edition=2021", "-O", "-o"])
                .arg(&temporary),
        );
        fs::remove_file(source_path)?;
        result?;
        fs::rename(temporary, &output)?;
    }
    Ok(output)
}
