use crate::{storage::Storage, Config};
use motor_protocol::*;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const INDEX_LOCK_WAIT: Duration = Duration::from_secs(10);
const INDEX_LOCK_POLL: Duration = Duration::from_millis(50);

pub(crate) fn command(path: &Path) -> Command {
    let mut cmd = Command::new("git");
    // Ignore ambient repository/index selectors and AFT's injected hook path.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
    ] {
        cmd.env_remove(name);
    }
    cmd.arg("--no-optional-locks").arg("-C").arg(path);
    cmd
}
fn output(cmd: &mut Command) -> io::Result<String> {
    let result = cmd.output()?;
    if !result.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&result.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8(result.stdout)
        .map_err(io::Error::other)?
        .trim()
        .into())
}
pub(crate) fn git(path: &Path, args: &[&str]) -> io::Result<String> {
    output(command(path).args(args))
}
fn text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("non-UTF8 repository path"))
}

pub(crate) fn validate(_config: &Config, key: &str, root: &str, cwd: &str) -> io::Result<()> {
    if !Path::new(key).is_absolute()
        || !Path::new(root).is_absolute()
        || !Path::new(cwd).is_absolute()
    {
        return Err(io::Error::other("paths must be absolute"));
    }
    let key = fs::canonicalize(key).map_err(|_| io::Error::other("key cannot be canonicalized"))?;
    let root = fs::canonicalize(root)
        .map_err(|_| io::Error::other("repository_root cannot be canonicalized"))?;
    let cwd = fs::canonicalize(cwd).map_err(|_| io::Error::other("cwd cannot be canonicalized"))?;
    if !cwd.starts_with(&key) {
        return Err(io::Error::other("cwd is outside workspace key"));
    }
    let canonical_git_path = |path: &Path, flag: &str, check: &str| {
        git(path, &["rev-parse", "--path-format=absolute", flag])
            .and_then(fs::canonicalize)
            .map_err(|_| io::Error::other(check.to_string()))
    };
    if canonical_git_path(&key, "--show-toplevel", "key has no repository toplevel")? != key {
        return Err(io::Error::other("key is not the worktree root"));
    }
    if canonical_git_path(
        &root,
        "--show-toplevel",
        "repository_root has no repository toplevel",
    )? != root
    {
        return Err(io::Error::other("repository_root is not a repository root"));
    }
    // Path containment alone also admits nested repositories and linked worktrees.
    // The command's checkout must be the one that will actually be snapshotted.
    let cwd_check = "cwd belongs to a different checkout than the workspace key";
    if canonical_git_path(&cwd, "--show-toplevel", cwd_check)? != key {
        return Err(io::Error::other(cwd_check));
    }
    let dir = canonical_git_path(&key, "--git-dir", "key has no git dir")?;
    let common = canonical_git_path(&key, "--git-common-dir", "key has no common dir")?;
    let stated = canonical_git_path(
        &root,
        "--git-common-dir",
        "repository_root has no common dir",
    )?;
    if common != stated || (key != root && dir == common) {
        return Err(io::Error::other(
            "not a linked worktree of the stated repository",
        ));
    }
    Ok(())
}

pub(crate) fn workspace_kind(config: &Config, key: &str) -> io::Result<WorkspaceKind> {
    let key = fs::canonicalize(key)?;
    Ok(
        if config
            .worktree_roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|root| key.starts_with(root)))
        {
            WorkspaceKind::Task
        } else {
            WorkspaceKind::Head
        },
    )
}

pub(crate) struct Spool(pub PathBuf);
impl Spool {
    pub fn new(parent: &Path, id: Uuid) -> io::Result<Self> {
        let path = parent.join(format!("transfer-{id}"));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Index(PathBuf);
impl Drop for Index {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("lock"));
    }
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub path: PathBuf,
    pub repository: String,
    pub head: String,
    pub commit: String,
    pub tree_hash: String,
    pub tip: Option<String>,
}
pub(crate) struct Snapshots {
    pub main: Snapshot,
    pub siblings: Vec<Snapshot>,
    pub spool: Spool,
    pub git_dependencies: BTreeMap<String, Vec<String>>,
}
#[derive(Debug)]
pub(crate) struct Failure {
    pub tree_hash: Option<String>,
    pub check: String,
}

// Allow incidental unignored build artifacts, but cap their total size so a
// snapshot cannot silently include large database copies.
pub(crate) const UNTRACKED_CONTENT_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

// Exclude the root target entry and all its descendants regardless of Git
// tracking or ignore status; nested directories such as src/target are source.
fn runner_owned(name: &[u8]) -> bool {
    name == b"target" || name.starts_with(b"target/")
}

pub(crate) fn check_untracked_content<'a>(
    paths: impl Iterator<Item = &'a Path>,
    spool: &Path,
) -> Result<(), Failure> {
    let inventory = (|| -> io::Result<_> {
        let mut total = 0_u64;
        let mut largest = Vec::new();
        for (i, path) in paths.enumerate() {
            let index = Index(spool.join(format!("untracked-index-{i}")));
            let head = git(path, &["rev-parse", "HEAD"])?;
            output(
                command(path)
                    .env("GIT_INDEX_FILE", &index.0)
                    .args(["read-tree", &head]),
            )?;
            // Match the snapshot's HEAD-based index, not the caller's staged index.
            // Inspect logical sizes without reading contents or following symlinks.
            let listed = command(path)
                .env("GIT_INDEX_FILE", &index.0)
                .args(["ls-files", "--others", "--exclude-standard", "-z"])
                .output()?;
            if !listed.status.success() {
                return Err(io::Error::other(
                    "cannot enumerate untracked snapshot files",
                ));
            }
            for name in listed.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
                if runner_owned(name) {
                    continue;
                }
                let relative = Path::new(std::ffi::OsStr::from_bytes(name));
                let metadata = fs::symlink_metadata(path.join(relative))?;
                if !metadata.is_file() && !metadata.is_symlink() {
                    continue;
                }
                total = total.saturating_add(metadata.len());
                let label = if i == 0 {
                    relative.to_string_lossy().into_owned()
                } else {
                    format!(
                        "{}/{}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        relative.display()
                    )
                };
                let mut label: String = label.chars().flat_map(char::escape_default).collect();
                if label.len() > 160 {
                    label.truncate(160);
                    label.push_str("...");
                }
                largest.push((metadata.len(), label));
                largest.sort_unstable_by(|a, b| b.cmp(a));
                largest.truncate(3);
            }
        }
        Ok((total, largest))
    })()
    .map_err(|_| Failure {
        tree_hash: None,
        check: "cannot inspect untracked snapshot files".into(),
    })?;
    let (total, largest) = inventory;
    if total > UNTRACKED_CONTENT_LIMIT {
        let files = largest
            .into_iter()
            .map(|(bytes, name)| format!("untracked {name} is {bytes} bytes"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Failure { tree_hash: None, check: format!(
            "untracked total is {total} bytes; the limit is {UNTRACKED_CONTENT_LIMIT} bytes; {files}"
        ) });
    }
    Ok(())
}

fn prune(path: &Path) -> io::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    for reference in git(
        path,
        &["for-each-ref", "--format=%(refname)", "refs/ck-motor/"],
    )?
    .lines()
    {
        if let Some(id) = reference
            .strip_prefix("refs/ck-motor/")
            .and_then(|s| s.split('/').next())
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            if let Some((seconds, nanos)) = id.get_timestamp().map(|t| t.to_unix()) {
                if now.saturating_sub(seconds * 1000 + u64::from(nanos) / 1_000_000) > 3_600_000 {
                    git(path, &["update-ref", "-d", reference])?;
                }
            }
        }
    }
    Ok(())
}
fn snapshot(
    path: &Path,
    repository: String,
    spool: &Path,
    index_number: usize,
    inject: bool,
) -> Result<Snapshot, Failure> {
    snapshot_with_hook(path, repository, spool, index_number, inject, |_| {})
}

pub(crate) fn local_default_branch(root: &Path) -> Option<(String, String)> {
    // Local refs choose the branch and commit to check against origin. Agents can
    // rewrite them, so default_tip verifies both with the remote before a cache fill.
    let advertised = git(
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|target| {
        target
            .strip_prefix("refs/remotes/origin/")
            .map(str::to_owned)
    })
    .map(|branch| format!("refs/heads/{branch}"));
    let candidate = advertised
        .iter()
        .map(String::as_str)
        .chain(["refs/heads/main", "refs/heads/master"])
        .find_map(|reference| {
            git(
                root,
                &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
            )
            .ok()
            .map(|tip| (reference.to_owned(), tip))
        });
    candidate
}

#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    head: String,
    files: BTreeMap<PathBuf, Option<FileStamp>>,
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    inode: u64,
    device: u64,
    mode: u32,
    submodule_head: Option<String>,
}

fn head_gitlinks(path: &Path, head: &str) -> io::Result<BTreeSet<PathBuf>> {
    let mut cmd = command(path);
    cmd.args(["ls-tree", "-r", "-z", head]);
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    let mut gitlinks = BTreeSet::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if entry.starts_with(b"160000 commit") {
            if let Some(tab) = entry.iter().position(|b| *b == b'\t') {
                let name = &entry[tab + 1..];
                gitlinks.insert(PathBuf::from(std::ffi::OsStr::from_bytes(name)));
            }
        }
    }
    Ok(gitlinks)
}

fn is_untracked_repository(
    root: &Path,
    relative: &Path,
    head_gitlinks: &BTreeSet<PathBuf>,
) -> bool {
    let target = root.join(relative);
    !relative.as_os_str().is_empty()
        && fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir())
        && target.join(".git").exists()
        && !head_gitlinks.contains(relative)
}

fn untracked_directories(
    root: &Path,
    relative: &Path,
    head_gitlinks: &BTreeSet<PathBuf>,
    paths: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    if runner_owned(relative.as_os_str().as_bytes())
        || is_untracked_repository(root, relative, head_gitlinks)
    {
        return Ok(());
    }
    paths.insert(relative.to_path_buf());
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        if entry.file_name() == ".git" || !entry.file_type()?.is_dir() {
            continue;
        }
        let child = relative.join(entry.file_name());
        if is_untracked_repository(root, &child, head_gitlinks) {
            continue;
        }
        let ignored = command(root)
            .args(["check-ignore", "--quiet", "--"])
            .arg(&child)
            .status()?;
        match ignored.code() {
            Some(0) => {}
            Some(1) => untracked_directories(root, &child, head_gitlinks, paths)?,
            _ => return Err(io::Error::other("cannot enumerate snapshot directories")),
        }
    }
    Ok(())
}

/// Adds to the snapshot index every path the caller's real index tracks that
/// also matches an ignore rule and still exists in the working tree. Paths are
/// passed NUL-separated and literal, so names with glob characters or spaces
/// never match anything else.
fn force_add_staged_ignored(path: &Path, index: &Index) -> io::Result<()> {
    let listed = command(path)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--ignored",
            "--exclude-standard",
        ])
        .output()?;
    if !listed.status.success() {
        return Err(io::Error::other(
            "cannot list staged ignored snapshot files",
        ));
    }
    let mut paths = Vec::new();
    for name in listed.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        if runner_owned(name) {
            continue;
        }
        // A staged path deleted from the working tree is left as HEAD has it;
        // `add -A` already recorded the deletion of any committed path.
        if fs::symlink_metadata(path.join(std::ffi::OsStr::from_bytes(name))).is_ok() {
            paths.extend_from_slice(name);
            paths.push(0);
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let mut child = command(path)
        .env("GIT_INDEX_FILE", &index.0)
        .args([
            "--literal-pathspecs",
            "add",
            "--force",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&paths)?;
    }
    let result = child.wait_with_output()?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "cannot add staged ignored files to the snapshot: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    Ok(())
}

fn fingerprint(path: &Path, index: &Index) -> io::Result<Fingerprint> {
    let head = git(path, &["rev-parse", "HEAD"])?;
    let head_gitlinks = head_gitlinks(path, &head)?;
    output(
        command(path)
            .env("GIT_INDEX_FILE", &index.0)
            .args(["read-tree", &head]),
    )?;
    // Enumerate against HEAD, just like capture, not the caller's staged index.
    // NUL framing preserves whitespace and non-UTF8 names. Missing tracked files
    // remain in the map so deletion/recreation cannot disappear from the check.
    let listed = command(path)
        .env("GIT_INDEX_FILE", &index.0)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
    if !listed.status.success() {
        return Err(io::Error::other("cannot enumerate snapshot files"));
    }
    let mut paths = BTreeSet::from([PathBuf::new()]);
    for name in listed.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        if runner_owned(name) {
            continue;
        }
        let is_dir = name.ends_with(b"/");
        let clean = name.strip_suffix(b"/").unwrap_or(name);
        let relative = PathBuf::from(std::ffi::OsStr::from_bytes(clean));
        if is_dir && is_untracked_repository(path, &relative, &head_gitlinks) {
            if let Some(parent) = relative.parent() {
                paths.extend(parent.ancestors().map(Path::to_path_buf));
            }
            continue;
        }
        paths.extend(relative.ancestors().map(Path::to_path_buf));
    }
    let directories = command(path)
        .env("GIT_INDEX_FILE", &index.0)
        .args([
            "ls-files",
            "--others",
            "--directory",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
    if !directories.status.success() {
        return Err(io::Error::other("cannot enumerate snapshot directories"));
    }
    // Empty untracked directories can gain and lose files during git add. Their
    // timestamps catch that race even when both file inventories are identical.
    for name in directories
        .stdout
        .split(|b| *b == 0)
        .filter(|n| n.ends_with(b"/"))
    {
        let clean = name.strip_suffix(b"/").unwrap_or(name);
        let relative = PathBuf::from(std::ffi::OsStr::from_bytes(clean));
        untracked_directories(path, &relative, &head_gitlinks, &mut paths)?;
    }
    // Git reads ignore and attribute files even when their own ignore rules
    // exclude them from the source inventory. Watch those checkout inputs too.
    let controls: Vec<_> = paths
        .iter()
        .filter(|p| path.join(p).is_dir())
        .flat_map(|p| [p.join(".gitignore"), p.join(".gitattributes")])
        .collect();
    paths.extend(controls);
    let common = PathBuf::from(git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    paths.extend([
        common.join("config"),
        common.join("info/exclude"),
        common.join("info/attributes"),
    ]);
    let mut files = BTreeMap::new();
    for relative in paths {
        let file = path.join(&relative);
        let stamp = match fs::symlink_metadata(&file) {
            Ok(m) => Some(FileStamp {
                size: m.len(),
                mtime: (m.mtime(), m.mtime_nsec()),
                ctime: (m.ctime(), m.ctime_nsec()),
                inode: m.ino(),
                device: m.dev(),
                mode: m.mode(),
                submodule_head: if m.is_dir()
                    && !relative.as_os_str().is_empty()
                    && head_gitlinks.contains(&relative)
                    && file.join(".git").exists()
                {
                    Some(git(&file, &["rev-parse", "HEAD"])?)
                } else {
                    None
                },
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        files.insert(relative, stamp);
    }
    Ok(Fingerprint { head, files })
}

pub(crate) fn snapshot_with_hook(
    path: &Path,
    repository: String,
    spool: &Path,
    index_number: usize,
    inject: bool,
    after_capture: impl FnMut(usize),
) -> Result<Snapshot, Failure> {
    snapshot_with_hook_and_timeout(
        path,
        repository,
        spool,
        index_number,
        inject,
        INDEX_LOCK_WAIT,
        after_capture,
    )
}

fn index_lock_path(path: &Path) -> io::Result<PathBuf> {
    let lock = PathBuf::from(git(path, &["rev-parse", "--git-path", "index.lock"])?);
    Ok(if lock.is_absolute() {
        lock
    } else {
        path.join(lock)
    })
}

fn wait_for_index_lock_clear(path: &Path, lock: &Path, deadline: Instant) -> io::Result<bool> {
    let mut saw_lock = false;
    loop {
        match fs::symlink_metadata(lock) {
            Ok(_) => saw_lock = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(saw_lock),
            Err(error) => return Err(error),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::other(format!(
                "a git operation is still running in {} ({} exists); retry when it finishes, or remove the lock if no git process is running",
                path.display(),
                lock.display()
            )));
        }
        thread::sleep(INDEX_LOCK_POLL.min(deadline - now));
    }
}

fn snapshot_with_hook_and_timeout(
    path: &Path,
    repository: String,
    spool: &Path,
    index_number: usize,
    inject: bool,
    lock_wait_timeout: Duration,
    mut after_capture: impl FnMut(usize),
) -> Result<Snapshot, Failure> {
    let mut tree_hash = None;
    let deadline = Instant::now() + lock_wait_timeout;
    let result = (|| -> io::Result<Snapshot> {
        prune(path)?;
        // Git keeps this lock in the per-worktree Git directory while it updates
        // the index and moves HEAD; rev-parse resolves the correct linked-worktree path.
        let lock = index_lock_path(path)?;
        for attempt in 0..3 {
            // A writer that started before capture must finish before we read HEAD.
            wait_for_index_lock_clear(path, &lock, deadline)?;
            let index = Index(spool.join(format!("index-{index_number}")));
            let before = match fingerprint(path, &index) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let head = before.head.clone();
            let head_gitlinks = head_gitlinks(path, &head)?;
            // If an untracked repository has no commit checked out, git add -A
            // would fail with "fatal: adding files failed" trying to resolve its
            // HEAD as a gitlink. Exclude untracked repositories that have no
            // commit from git add.
            let others = command(path)
                .env("GIT_INDEX_FILE", &index.0)
                .args([
                    "ls-files",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                    "-z",
                ])
                .output()?;
            if !others.status.success() {
                return Err(io::Error::other(
                    "cannot enumerate untracked snapshot files",
                ));
            }
            let mut add_paths = Vec::new();
            for name in others.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
                if runner_owned(name) {
                    continue;
                }
                if name.ends_with(b"/") {
                    let clean = name.strip_suffix(b"/").unwrap_or(name);
                    let rel = Path::new(std::ffi::OsStr::from_bytes(clean));
                    if is_untracked_repository(path, rel, &head_gitlinks) {
                        let has_commit =
                            git(&path.join(rel), &["rev-parse", "--verify", "HEAD"]).is_ok();
                        if !has_commit {
                            continue;
                        }
                    }
                }
                add_paths.extend_from_slice(name);
                add_paths.push(0);
            }
            // Enumerate literal source paths rather than using a negative target
            // pathspec: git add rejects an ignored target even when excluded.
            if !add_paths.is_empty() {
                let mut child = command(path)
                    .env("GIT_INDEX_FILE", &index.0)
                    .args([
                        "--literal-pathspecs",
                        "add",
                        "-A",
                        "--pathspec-from-file=-",
                        "--pathspec-file-nul",
                    ])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;
                child.stdin.take().unwrap().write_all(&add_paths)?;
                let added = child.wait_with_output()?;
                if !added.status.success() {
                    return Err(io::Error::other(
                        String::from_utf8_lossy(&added.stderr).into_owned(),
                    ));
                }
            }
            // Git tracks a path once it is in the caller's index, even when it
            // matches .gitignore. The snapshot index starts from HEAD, so `add -A`
            // above skips such a path when it is staged but not yet committed.
            // Add those explicitly from the caller's real index.
            force_add_staged_ignored(path, &index)?;
            // Untracked nested repositories and linked worktrees are added as
            // mode 160000 gitlinks by git add -A when they have a commit checked out.
            // Remove new gitlinks and all runner-owned paths inherited from HEAD.
            let listed = command(path)
                .env("GIT_INDEX_FILE", &index.0)
                .args(["ls-files", "-s", "-z"])
                .output()?;
            if !listed.status.success() {
                return Err(io::Error::other("cannot inspect snapshot index"));
            }
            let mut remove_paths = Vec::new();
            for entry in listed.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                if let Some(tab) = entry.iter().position(|b| *b == b'\t') {
                    let entry_path = &entry[tab + 1..];
                    let rel = Path::new(std::ffi::OsStr::from_bytes(entry_path));
                    if runner_owned(entry_path)
                        || (entry.starts_with(b"160000 ") && !head_gitlinks.contains(rel))
                    {
                        remove_paths.extend_from_slice(entry_path);
                        remove_paths.push(0);
                    }
                }
            }
            if !remove_paths.is_empty() {
                let mut child = command(path)
                    .env("GIT_INDEX_FILE", &index.0)
                    .args(["update-index", "--force-remove", "-z", "--stdin"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(&remove_paths)?;
                }
                let status = child.wait_with_output()?;
                if !status.status.success() {
                    return Err(io::Error::other(
                        String::from_utf8_lossy(&status.stderr).into_owned(),
                    ));
                }
            }
            let mut cmd = command(path);
            cmd.env("GIT_INDEX_FILE", &index.0).arg("write-tree");
            let tree = output(&mut cmd)?;
            after_capture(attempt);
            // If a writer overlapped capture, wait for it and rebuild from its final HEAD.
            if wait_for_index_lock_clear(path, &lock, deadline)? {
                continue;
            }
            // A moving checkout must never publish the temporary index's mixed tree.
            // Rebuild the index on each retry and compare HEAD and the complete path
            // inventory, including newly created untracked files and symlink metadata.
            if !fingerprint(path, &index).is_ok_and(|after| before == after) {
                continue;
            }
            tree_hash = Some(tree.clone());
            let head_tree = git(path, &["rev-parse", &format!("{head}^{{tree}}")])?;
            let commit = if !inject && head_tree == tree {
                head.clone()
            } else {
                let mut cmd = command(path);
                // Stable metadata makes an unchanged snapshot the same commit on the
                // next run, allowing have/bundle to exclude its large unchanged blobs.
                cmd.envs(SNAPSHOT_IDENTITY).args([
                    "commit-tree",
                    if inject { "invalid-tree" } else { &tree },
                    "-p",
                    &head,
                    "-m",
                    "ck-motor snapshot",
                ]);
                output(&mut cmd)?
            };
            return Ok(Snapshot {
                path: path.into(),
                repository,
                head,
                commit,
                tree_hash: tree,
                tip: None,
            });
        }
        Err(io::Error::other("checkout changed during capture"))
    })();
    result.map_err(|error| {
        let detail = error.to_string();
        Failure {
            tree_hash,
            check: if detail == "checkout changed during capture"
                || detail.starts_with("a git operation is still running in ")
            {
                detail
            } else {
                "snapshot capture failed".into()
            },
        }
    })
}

impl Snapshots {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        config: &Config,
        key: &str,
        root: &str,
        siblings: &[String],
        parent: &Path,
        id: Uuid,
        base: Option<&str>,
        storage: &Storage,
    ) -> Result<Self, Failure> {
        let spool = Spool::new(parent, id).map_err(|_| Failure {
            tree_hash: None,
            check: "cannot create snapshot spool".into(),
        })?;
        check_untracked_content(
            std::iter::once(Path::new(key)).chain(siblings.iter().map(Path::new)),
            &spool.0,
        )?;
        let mut main = snapshot(
            Path::new(key),
            root.into(),
            &spool.0,
            0,
            config.inject_snapshot_failure,
        )?;
        let hash = Some(main.tree_hash.clone());
        main.tip = crate::default_tip::tip(Path::new(root), storage, &spool.0);
        if let Some(base) = base {
            // prepare warms the caller's base, not uncommitted main-tree edits.
            main.commit = git(
                Path::new(key),
                &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
            )
            .map_err(|_| Failure {
                tree_hash: hash.clone(),
                check: format!("prepare base {base} is not a commit in {key}"),
            })?;
            main.tree_hash = git(
                Path::new(key),
                &["rev-parse", &format!("{}^{{tree}}", main.commit)],
            )
            .map_err(|_| Failure {
                tree_hash: hash.clone(),
                check: format!("cannot read the tree of prepare base {}", main.commit),
            })?;
        }
        let mut snapshots = Vec::new();
        let parent = fs::canonicalize(root)
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        for (i, sibling) in siblings.iter().enumerate() {
            let path = fs::canonicalize(sibling).map_err(|error| Failure {
                tree_hash: hash.clone(),
                check: format!("sibling {sibling} cannot be resolved: {error}"),
            })?;
            if path.parent() != parent.as_deref() {
                // motor-runner recreates each sibling on the build host at this
                // same absolute path and accepts only direct neighbours of the
                // repository root, so it would refuse a sibling anywhere else.
                return Err(Failure {
                    tree_hash: hash,
                    check: format!(
                        "sibling {} must sit in the same folder as the repository root {root}; for a linked worktree, the repository root is its main checkout",
                        path.display()
                    ),
                });
            }
            snapshots.push(
                snapshot(
                    &path,
                    text(&path)
                        .map_err(|_| Failure {
                            tree_hash: hash.clone(),
                            check: format!("sibling path {} is not valid UTF-8", path.display()),
                        })?
                        .into(),
                    &spool.0,
                    i + 1,
                    config.inject_snapshot_failure,
                )
                .map_err(|f| Failure {
                    tree_hash: hash.clone(),
                    check: f.check,
                })?,
            );
        }
        let git_dependencies =
            lockfile_sources(&main.path, &main.commit).map_err(|error| Failure {
                tree_hash: hash,
                check: format!(
                    "cannot read git dependencies from Cargo.lock at {}: {error}",
                    main.commit
                ),
            })?;
        Ok(Self {
            main,
            siblings: snapshots,
            spool,
            git_dependencies,
        })
    }
    pub fn commits(&self) -> BTreeMap<String, String> {
        std::iter::once(&self.main)
            .chain(&self.siblings)
            .map(|s| (s.repository.clone(), s.commit.clone()))
            .collect()
    }
    pub fn sibling_frames(&self) -> Vec<SiblingSnapshot> {
        self.siblings
            .iter()
            .map(|s| SiblingSnapshot {
                canonical_path: s.repository.clone(),
                commit: s.commit.clone(),
                tree_hash: s.tree_hash.clone(),
                base_commit: Some(s.head.clone()),
            })
            .collect()
    }
    pub fn candidates(&self, previous: &BTreeMap<String, String>) -> Vec<RepositoryCandidates> {
        std::iter::once(&self.main)
            .chain(&self.siblings)
            .map(|s| {
                let mut commits = vec![s.head.clone(), s.commit.clone()];
                if let Some(tip) = &s.tip {
                    commits.push(tip.clone());
                }
                if let Some(previous) = previous.get(&s.repository) {
                    commits.push(previous.clone());
                }
                // Offer ancestry as well as the new endpoints. A held tip can be
                // older than this worktree's HEAD and hidden by the ref-report
                // cap, or be a dirty snapshot unavailable in the local object
                // store. Its shared history is still a valid prerequisite.
                let mut revisions = vec!["rev-list", "--topo-order", &s.head, &s.commit];
                if let Some(tip) = &s.tip {
                    revisions.push(tip);
                }
                if let Ok(ancestors) = git(&s.path, &revisions) {
                    commits.extend(ancestors.lines().map(str::to_owned));
                }
                let mut seen = std::collections::BTreeSet::new();
                commits.retain(|c| seen.insert(c.clone()));
                RepositoryCandidates {
                    repository: s.repository.clone(),
                    commits,
                }
            })
            .collect()
    }
    pub fn bundle(
        &self,
        id: Uuid,
        held: &[RepositoryCandidates],
    ) -> io::Result<Vec<BundleManifest>> {
        let mut manifests = Vec::new();
        for snapshot in std::iter::once(&self.main).chain(&self.siblings) {
            let held = held
                .iter()
                .find(|r| r.repository == snapshot.repository)
                .map(|r| r.commits.as_slice())
                .unwrap_or_default();
            let needed = !held.contains(&snapshot.commit)
                || snapshot.tip.as_ref().is_some_and(|t| !held.contains(t));
            if !needed {
                continue;
            }
            let mut refs = Refs {
                path: snapshot.path.clone(),
                names: Vec::new(),
            };
            refs.add(format!("refs/ck-motor/{id}/snapshot"), &snapshot.commit)?;
            if let Some(tip) = &snapshot.tip {
                if !held.contains(tip) {
                    refs.add(format!("refs/ck-motor/{id}/tip"), tip)?;
                }
            }
            let index = manifests.len() as u32;
            let path = self.spool.0.join(format!("bundle-{index}"));
            // Exclude what the runner reports holding: the candidates we
            // offered, and the commits its own refs point at (its earlier
            // integration tips and snapshots), as far as we have them locally.
            // A reported commit is a bundle prerequisite only if git can walk
            // it here, so require its tree as well as the commit object.
            let mut excluded = Vec::new();
            for commit in held {
                if git(
                    &snapshot.path,
                    &["cat-file", "-e", &format!("{commit}^{{tree}}")],
                )
                .is_ok()
                {
                    excluded.push(commit.clone());
                }
            }
            let mut parents = vec![snapshot.commit.clone()];
            for commit in &excluded {
                if git(
                    &snapshot.path,
                    &["merge-base", "--is-ancestor", commit, &snapshot.commit],
                )
                .is_err()
                {
                    parents.push(commit.clone());
                }
            }
            if parents.len() > 1 {
                // Dirty snapshots are siblings parented on HEAD, not on each
                // other. Git's negative revision walk does not prune unchanged
                // blobs from a non-ancestor snapshot. A transport-only merge
                // makes held commits traversal boundaries without changing the
                // snapshot sent in run/prepare or its single HEAD parent.
                let tree = git(
                    &snapshot.path,
                    &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)],
                )?;
                let mut anchor = command(&snapshot.path);
                anchor.envs(SNAPSHOT_IDENTITY).arg("commit-tree").arg(tree);
                for parent in parents {
                    anchor.args(["-p", &parent]);
                }
                anchor.args(["-m", "ck-motor transport reachability"]);
                let commit = output(&mut anchor)?;
                refs.add(format!("refs/ck-motor/{id}/delta"), &commit)?;
            }
            let mut cmd = command(&snapshot.path);
            cmd.args(["bundle", "create"]).arg(&path).args(&refs.names);
            for commit in excluded {
                cmd.arg(format!("^{commit}"));
            }
            output(&mut cmd)?;
            refs.clear()?;
            let mut file = File::open(path)?;
            let length = file.metadata()?.len();
            let mut digest = Sha256::new();
            let mut bytes = [0; 65536];
            loop {
                let n = file.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                digest.update(&bytes[..n]);
            }
            manifests.push(BundleManifest {
                index,
                repository: snapshot.repository.clone(),
                length,
                sha256: format!("{:x}", digest.finalize()),
            });
        }
        Ok(manifests)
    }

    pub fn transfer_bundles(
        &self,
        id: Uuid,
        held: &[RepositoryCandidates],
        dependency_capability: Option<&[GitDependencyCandidates]>,
    ) -> io::Result<(Vec<BundleManifest>, Vec<GitDependency>, Vec<BundleManifest>)> {
        let workspace = self.bundle(id, held)?;
        let mut upload = workspace.clone();
        let dependencies = dependency_bundles(
            &self.git_dependencies,
            dependency_capability,
            &cargo_home()?,
            &self.spool.0,
            &mut upload,
            id,
        )?;
        Ok((workspace, dependencies, upload))
    }
}

fn cargo_home() -> io::Result<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .ok_or_else(|| io::Error::other("no Cargo cache home"))
}

pub(crate) fn valid_dependency_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn parse_lockfile(lock: &str) -> io::Result<BTreeMap<String, Vec<String>>> {
    let lock: toml::Value = toml::from_str(lock).map_err(io::Error::other)?;
    let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(source) = package
            .get("source")
            .and_then(toml::Value::as_str)
            .and_then(|s| s.strip_prefix("git+"))
        else {
            continue;
        };
        let Some((url, commit)) = source.rsplit_once('#') else {
            tracing::warn!(source, "git dependency has no locked commit");
            continue;
        };
        if !valid_dependency_commit(commit) {
            tracing::warn!(source, "git dependency has an invalid locked commit");
            continue;
        }
        let url = url.split('?').next().unwrap();
        sources.entry(url.into()).or_default().push(commit.into());
    }
    for commits in sources.values_mut() {
        commits.sort();
        commits.dedup();
    }
    Ok(sources)
}

pub(crate) fn lockfile_sources(
    path: &Path,
    commit: &str,
) -> io::Result<BTreeMap<String, Vec<String>>> {
    let object = format!("{commit}:Cargo.lock");
    if git(path, &["cat-file", "-e", &object]).is_err() {
        return Ok(BTreeMap::new());
    }
    parse_lockfile(&git(path, &["show", &object])?)
}

pub(crate) fn cached_commit(home: &Path, url: &str, commit: &str) -> Option<PathBuf> {
    let name = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .trim_end_matches(".git");
    let prefix = format!("{name}-");
    let mut candidates: Vec<_> = fs::read_dir(home.join("git/db"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .find(|p| git(p, &["cat-file", "-t", commit]).is_ok_and(|t| t == "commit"))
}

pub(crate) fn dependency_bundles(
    sources: &BTreeMap<String, Vec<String>>,
    capability: Option<&[GitDependencyCandidates]>,
    home: &Path,
    spool: &Path,
    upload: &mut Vec<BundleManifest>,
    id: Uuid,
) -> io::Result<Vec<GitDependency>> {
    let Some(held) = capability else {
        return Ok(Vec::new());
    };
    let mut declarations = Vec::new();
    for (url, commits) in sources {
        for commit in commits {
            let mut dependency = GitDependency {
                url: url.clone(),
                commits: vec![commit.clone()],
                bundle: None,
            };
            let cache = cached_commit(home, url, commit);
            if let Some(cache) = &cache {
                if let Some(warning) = submodule_warning(cache, url, commit) {
                    tracing::warn!(job_id = %id, url, "{warning}");
                }
            }
            if !held
                .iter()
                .any(|h| h.url == *url && h.commits.contains(commit))
            {
                if let Some(cache) = cache {
                    let index = upload.len() as u32;
                    let temporary = spool.join(format!("dependency-{index}.git"));
                    fs::create_dir(&temporary)?;
                    git(
                        &temporary,
                        &[
                            "init",
                            "--bare",
                            if commit.len() == 64 {
                                "--object-format=sha256"
                            } else {
                                "--object-format=sha1"
                            },
                        ],
                    )?;
                    let reference = format!("refs/ck-motor/dep/{commit}");
                    // Fetch into our own repository; even refs and gc metadata in
                    // Cargo's cache must remain untouched.
                    output(
                        command(&temporary)
                            .args([
                                "-c",
                                "gc.auto=0",
                                "fetch",
                                "--no-auto-maintenance",
                                "--no-tags",
                            ])
                            .arg(&cache)
                            .arg(format!("{commit}:{reference}")),
                    )?;
                    let bundle_path = spool.join(format!("bundle-{index}"));
                    output(
                        command(&temporary)
                            .args(["bundle", "create"])
                            .arg(&bundle_path)
                            .arg(&reference),
                    )?;
                    let bytes = fs::read(&bundle_path)?;
                    let manifest = BundleManifest {
                        index,
                        repository: url.clone(),
                        length: bytes.len() as u64,
                        sha256: format!("{:x}", Sha256::digest(&bytes)),
                    };
                    dependency.bundle = Some(manifest.clone());
                    upload.push(manifest);
                }
            }
            declarations.push(dependency);
        }
    }
    Ok(declarations)
}

pub(crate) fn submodule_warning(cache: &Path, url: &str, commit: &str) -> Option<String> {
    git(cache, &["cat-file", "-e", &format!("{commit}:.gitmodules")])
        .ok()
        .map(|_| {
            format!("git dependency {url} has submodules; submodule repositories are not shipped")
        })
}
const SNAPSHOT_IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "ck-motor"),
    ("GIT_AUTHOR_EMAIL", "motor@localhost"),
    ("GIT_COMMITTER_NAME", "ck-motor"),
    ("GIT_COMMITTER_EMAIL", "motor@localhost"),
    ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
    ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
];
struct Refs {
    path: PathBuf,
    names: Vec<String>,
}
impl Refs {
    fn add(&mut self, name: String, commit: &str) -> io::Result<()> {
        self.names.push(name.clone());
        git(&self.path, &["update-ref", &name, commit])?;
        Ok(())
    }
    fn clear(&mut self) -> io::Result<()> {
        for name in &self.names {
            git(&self.path, &["update-ref", "-d", name])?;
        }
        self.names.clear();
        Ok(())
    }
}
impl Drop for Refs {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn repository() -> (ScratchDir, PathBuf, PathBuf, String) {
        let scratch = ScratchDir::new("motor-snapshot-index-lock");
        let checkout = scratch.join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "-q"]).unwrap();
        fs::write(checkout.join("source.txt"), "before\n").unwrap();
        git(&checkout, &["add", "-A"]).unwrap();
        git(
            &checkout,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        let head = git(&checkout, &["rev-parse", "HEAD"]).unwrap();
        let spool = scratch.join("spool");
        fs::create_dir_all(&spool).unwrap();
        (scratch, checkout, spool, head)
    }

    fn prepare_commit(checkout: &Path, parent: &str) -> String {
        git(checkout, &["add", "-A"]).unwrap();
        let tree = git(checkout, &["write-tree"]).unwrap();
        let commit = git(
            checkout,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit-tree",
                &tree,
                "-p",
                parent,
                "-m",
                "next",
            ],
        )
        .unwrap();
        git(checkout, &["reset", "--mixed", parent]).unwrap();
        commit
    }

    fn build_snapshots(checkout: &Path, root: &Path, spool: &Path) -> Snapshots {
        let storage = Storage::open(&spool.join("store")).unwrap();
        Snapshots::build(
            &Config::default(),
            text(checkout).unwrap(),
            text(root).unwrap(),
            &[],
            spool,
            Uuid::now_v7(),
            None,
            &storage,
        )
        .unwrap_or_else(|failure| panic!("{}", failure.check))
    }

    fn origin(checkout: &Path, remote: &Path) {
        git(
            checkout,
            &[
                "clone",
                "--bare",
                "-q",
                text(checkout).unwrap(),
                text(remote).unwrap(),
            ],
        )
        .unwrap();
        git(remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        git(
            checkout,
            &["remote", "add", "origin", text(remote).unwrap()],
        )
        .unwrap();
    }

    fn local_tip(root: &Path) -> Option<String> {
        local_default_branch(root).map(|(_, commit)| commit)
    }

    #[test]
    fn untracked_top_level_target_does_not_change_snapshot_or_fingerprint() {
        let (_scratch, checkout, spool, head) = repository();
        fs::create_dir_all(checkout.join("target/build")).unwrap();
        let index = Index(spool.join("fingerprint-index"));
        let before = fingerprint(&checkout, &index).unwrap();
        fs::write(checkout.join("target/build/artifact"), "build output").unwrap();
        let captured = snapshot(&checkout, "test".into(), &spool, 0, false).unwrap();
        assert_eq!(
            captured.tree_hash,
            git(&checkout, &["rev-parse", "HEAD^{tree}"]).unwrap()
        );
        assert_eq!(captured.commit, head);
        assert_eq!(fingerprint(&checkout, &index).unwrap(), before);
    }

    #[test]
    fn untracked_size_cap_excludes_only_top_level_target() {
        let (_scratch, checkout, spool, _) = repository();
        fs::create_dir_all(checkout.join("target")).unwrap();
        let artifact = checkout.join("target/large");
        fs::File::create(&artifact)
            .unwrap()
            .set_len(UNTRACKED_CONTENT_LIMIT + 1)
            .unwrap();
        check_untracked_content(std::iter::once(checkout.as_path()), &spool).unwrap();
        fs::create_dir_all(checkout.join("src/target")).unwrap();
        fs::rename(artifact, checkout.join("src/target/large")).unwrap();
        assert!(
            check_untracked_content(std::iter::once(checkout.as_path()), &spool)
                .unwrap_err()
                .check
                .contains("src/target/large")
        );
    }

    #[test]
    fn nested_target_is_source_but_committed_top_level_target_is_not() {
        let (_scratch, checkout, spool, head) = repository();
        fs::create_dir_all(checkout.join("target")).unwrap();
        fs::create_dir_all(checkout.join("src/target")).unwrap();
        fs::write(checkout.join("target/committed"), "build output").unwrap();
        let commit = prepare_commit(&checkout, &head);
        git(&checkout, &["reset", "--hard", &commit]).unwrap();
        fs::write(checkout.join("target/committed"), "changed output").unwrap();
        fs::write(checkout.join("src/target/input"), "source input").unwrap();
        let captured = snapshot(&checkout, "test".into(), &spool, 0, false).unwrap();
        assert_eq!(
            git(
                &checkout,
                &["ls-tree", "-r", "--name-only", &captured.tree_hash]
            )
            .unwrap(),
            "source.txt\nsrc/target/input"
        );
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]).unwrap(), commit);
    }

    #[test]
    fn sibling_beside_a_linked_worktree_names_the_main_checkout_it_must_sit_beside() {
        let (scratch, checkout, spool, _) = repository();
        let gate = scratch.join("gate");
        fs::create_dir_all(&gate).unwrap();
        git(
            &gate,
            &["clone", "-q", checkout.to_str().unwrap(), "sibling"],
        )
        .unwrap();
        let linked = gate.join("workspace");
        git(
            &checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        )
        .unwrap();
        let root = fs::canonicalize(&checkout).unwrap();
        let sibling = fs::canonicalize(gate.join("sibling")).unwrap();
        let storage = Storage::open(&spool.join("store")).unwrap();

        let failure = Snapshots::build(
            &Config::default(),
            text(&linked).unwrap(),
            text(&root).unwrap(),
            &[text(&sibling).unwrap().to_string()],
            &spool,
            Uuid::now_v7(),
            None,
            &storage,
        )
        .err()
        .expect("a sibling beside the linked worktree, not its main checkout, must refuse");

        assert_eq!(
            failure.check,
            format!(
                "sibling {} must sit in the same folder as the repository root {}; for a linked worktree, the repository root is its main checkout",
                sibling.display(),
                root.display()
            )
        );
    }

    #[test]
    fn local_default_branch_prefers_origin_name_but_uses_local_commit() {
        let (_scratch, checkout, _spool, head) = repository();
        git(&checkout, &["branch", "-m", "feature"]).unwrap();
        let local = prepare_commit(&checkout, &head);
        git(
            &checkout,
            &["update-ref", "refs/heads/reviewed/trunk", &local],
        )
        .unwrap();
        for reference in [
            "refs/heads/main",
            "refs/heads/master",
            "refs/remotes/origin/reviewed/trunk",
        ] {
            git(&checkout, &["update-ref", reference, &head]).unwrap();
        }
        git(
            &checkout,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/reviewed/trunk",
            ],
        )
        .unwrap();
        assert_eq!(local_tip(&checkout), Some(local));
    }

    #[test]
    fn local_default_branch_falls_back_to_main_then_master_never_head() {
        let (_scratch, checkout, _spool, head) = repository();
        git(&checkout, &["branch", "-m", "feature"]).unwrap();
        assert_eq!(local_tip(&checkout), None);
        let main = prepare_commit(&checkout, &head);
        git(&checkout, &["update-ref", "refs/heads/main", &main]).unwrap();
        git(&checkout, &["update-ref", "refs/heads/master", &head]).unwrap();
        assert_eq!(local_tip(&checkout), Some(main.clone()));
        git(
            &checkout,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/missing",
            ],
        )
        .unwrap();
        assert_eq!(local_tip(&checkout), Some(main));
        git(&checkout, &["update-ref", "-d", "refs/heads/main"]).unwrap();
        assert_eq!(local_tip(&checkout), Some(head.clone()));
        git(&checkout, &["update-ref", "-d", "refs/heads/master"]).unwrap();
        git(
            &checkout,
            &["update-ref", "refs/remotes/origin/missing", &head],
        )
        .unwrap();
        assert_eq!(local_tip(&checkout), None);
    }

    #[test]
    fn default_branch_tip_is_shared_by_root_and_linked_feature_worktree() {
        let (scratch, checkout, spool, head) = repository();
        git(&checkout, &["branch", "-m", "main"]).unwrap();
        origin(&checkout, &scratch.join("origin.git"));
        let linked = scratch.join("linked");
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                text(&linked).unwrap(),
            ],
        )
        .unwrap();
        fs::write(linked.join("source.txt"), "feature work\n").unwrap();
        let root = build_snapshots(&checkout, &checkout, &spool);
        let feature = build_snapshots(&linked, &checkout, &spool);
        assert_ne!(feature.main.commit, head);
        assert_eq!(root.main.tip, Some(head));
        assert_eq!(feature.main.tip, root.main.tip);
    }

    #[test]
    fn snapshot_bundle_includes_divergent_default_branch_lockfile() {
        let (scratch, checkout, spool, head) = repository();
        git(&checkout, &["branch", "-m", "feature"]).unwrap();
        fs::write(checkout.join("Cargo.lock"), "# reviewed lockfile\n").unwrap();
        let reviewed = prepare_commit(&checkout, &head);
        git(&checkout, &["update-ref", "refs/heads/main", &reviewed]).unwrap();
        fs::write(checkout.join("Cargo.lock"), "# unmerged lockfile\n").unwrap();
        let feature = prepare_commit(&checkout, &head);
        git(&checkout, &["reset", "--hard", &feature]).unwrap();
        origin(&checkout, &scratch.join("origin.git"));
        let snapshots = build_snapshots(&checkout, &checkout, &spool);
        assert_eq!(snapshots.main.commit, feature);
        assert_eq!(snapshots.main.tip, Some(reviewed.clone()));
        assert!(snapshots.candidates(&BTreeMap::new())[0]
            .commits
            .contains(&reviewed));
        let manifests = snapshots.bundle(Uuid::now_v7(), &[]).unwrap();
        assert_eq!(manifests.len(), 1);
        let mirror = scratch.join("mirror");
        fs::create_dir_all(&mirror).unwrap();
        git(&mirror, &["init", "--bare", "-q"]).unwrap();
        git(
            &mirror,
            &[
                "fetch",
                text(&snapshots.spool.0.join("bundle-0")).unwrap(),
                "refs/ck-motor/*:refs/ck-motor/*",
            ],
        )
        .unwrap();
        assert_eq!(
            git(&mirror, &["show", &format!("{reviewed}:Cargo.lock")]).unwrap(),
            "# reviewed lockfile"
        );
        assert_eq!(
            git(&mirror, &["show", &format!("{feature}:Cargo.lock")]).unwrap(),
            "# unmerged lockfile"
        );
        let held = [RepositoryCandidates {
            repository: snapshots.main.repository.clone(),
            commits: vec![feature],
        }];
        assert_eq!(snapshots.bundle(Uuid::now_v7(), &held).unwrap().len(), 1);
    }

    #[test]
    fn snapshots_without_default_branch_have_no_tip() {
        let (_scratch, checkout, spool, _head) = repository();
        git(&checkout, &["branch", "-m", "feature"]).unwrap();
        let snapshots = build_snapshots(&checkout, &checkout, &spool);
        assert_eq!(snapshots.main.tip, None);
        assert_eq!(snapshots.bundle(Uuid::now_v7(), &[]).unwrap().len(), 1);
    }

    #[test]
    fn waits_for_git_operation_before_capturing_head() {
        let (_scratch, checkout, spool, initial) = repository();
        fs::write(checkout.join("source.txt"), "after commit\n").unwrap();
        let next = prepare_commit(&checkout, &initial);
        let lock = index_lock_path(&checkout).unwrap();
        fs::write(&lock, b"held").unwrap();

        let commit_checkout = checkout.clone();
        let commit_lock = lock.clone();
        let next_head = next.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            git(&commit_checkout, &["update-ref", "HEAD", &next_head]).unwrap();
            fs::remove_file(commit_lock).unwrap();
        });

        let started = Instant::now();
        let snapshot =
            snapshot_with_hook(&checkout, "repo".into(), &spool, 0, false, |_| {}).unwrap();
        writer.join().unwrap();

        assert!(started.elapsed() >= Duration::from_millis(250));
        assert_eq!(snapshot.head, next);
        assert_ne!(snapshot.head, initial);
        assert!(!lock.exists());
    }

    #[test]
    fn refuses_when_git_operation_lock_does_not_clear() {
        let (_scratch, checkout, spool, _) = repository();
        let lock = index_lock_path(&checkout).unwrap();
        fs::write(&lock, b"held").unwrap();
        let timeout = Duration::from_millis(220);
        let started = Instant::now();

        let failure = snapshot_with_hook_and_timeout(
            &checkout,
            "repo".into(),
            &spool,
            0,
            false,
            timeout,
            |_| {},
        )
        .err()
        .expect("a held Git lock must refuse the snapshot");

        let expected = format!(
            "a git operation is still running in {} ({} exists); retry when it finishes, or remove the lock if no git process is running",
            checkout.display(),
            lock.display()
        );
        assert_eq!(failure.check, expected);
        assert!(started.elapsed() >= timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(lock.exists(), "snapshot must not remove the caller's lock");
    }

    #[test]
    fn lock_appearing_during_capture_forces_a_retry() {
        let (_scratch, checkout, spool, initial) = repository();
        let lock = index_lock_path(&checkout).unwrap();
        let mut remover = None;
        let mut attempts = Vec::new();

        let snapshot = snapshot_with_hook(&checkout, "repo".into(), &spool, 0, false, |attempt| {
            attempts.push(attempt);
            if attempt == 0 {
                fs::write(&lock, b"held").unwrap();
                let lock = lock.clone();
                remover = Some(thread::spawn(move || {
                    thread::sleep(Duration::from_millis(150));
                    fs::remove_file(lock).unwrap();
                }));
            }
        })
        .unwrap();
        remover.unwrap().join().unwrap();

        assert_eq!(attempts, [0, 1]);
        assert_eq!(snapshot.head, initial);
        assert!(!lock.exists());
    }

    #[test]
    fn linked_worktree_uses_its_own_index_lock_path() {
        let (_scratch, checkout, spool, _) = repository();
        let linked = checkout.parent().unwrap().join("linked");
        git(
            &checkout,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        )
        .unwrap();
        let main_lock = index_lock_path(&checkout).unwrap();
        let linked_lock = index_lock_path(&linked).unwrap();
        assert_ne!(main_lock, linked_lock);
        fs::write(&linked_lock, b"held").unwrap();

        let failure = snapshot_with_hook_and_timeout(
            &linked,
            "repo".into(),
            &spool,
            0,
            false,
            Duration::from_millis(120),
            |_| {},
        )
        .err()
        .expect("the linked worktree lock must refuse the snapshot");

        assert_eq!(
            failure.check,
            format!(
                "a git operation is still running in {} ({} exists); retry when it finishes, or remove the lock if no git process is running",
                linked.display(),
                linked_lock.display()
            )
        );
        assert!(linked_lock.exists());
    }
}
