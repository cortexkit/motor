#[cfg(test)]
use crate::storage::atomic_json;
use crate::storage::{invalid, Catalog, Workspace};
use motor_protocol::*;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

pub(crate) mod dependencies;
mod transfer;
pub(crate) use dependencies::validate_dependencies;

pub(crate) fn name(path: &str) -> String {
    format!("{:x}", Sha256::digest(path.as_bytes()))
}

fn git_command(path: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0");
    command
}

fn git_input(mut command: Command, input: &[u8]) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input)?;
    child.wait_with_output()
}

pub(crate) fn git(path: &Path, args: &[&str]) -> io::Result<String> {
    let output = git_command(path).args(args).output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

pub(crate) fn blob(path: &Path, revision: &str) -> io::Result<Vec<u8>> {
    let output = git_command(path).args(["show", revision]).output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}

/// Preserve force-added ignored inputs as staged additions when HEAD is restored
/// to the caller's commit. Otherwise verification and jobs see them as untracked
/// ignored files, even though they were included in the snapshot tree.
fn stage_snapshot_ignored(path: &Path, tree_hash: &str) -> io::Result<()> {
    let added = git_command(path)
        .args([
            "diff-tree",
            "--no-renames",
            "-r",
            "-z",
            "--name-only",
            "--diff-filter=A",
            "HEAD",
            tree_hash,
        ])
        .output()?;
    if !added.status.success() {
        return Err(invalid(String::from_utf8_lossy(&added.stderr)));
    }
    if added.stdout.is_empty() {
        return Ok(());
    }
    let mut check = git_command(path);
    check.args(["check-ignore", "-z", "--stdin", "--no-index"]);
    let ignored = git_input(check, &added.stdout)?;
    // check-ignore returns 1 when none of the additions match an ignore rule.
    if !ignored.status.success() && ignored.status.code() != Some(1) {
        return Err(invalid(String::from_utf8_lossy(&ignored.stderr)));
    }
    if ignored.stdout.is_empty() {
        return Ok(());
    }
    let mut add = git_command(path);
    // update-index consumes literal, NUL-framed names, not wildcard pathspecs.
    add.args(["update-index", "--add", "-z", "--stdin"]);
    let updated = git_input(add, &ignored.stdout)?;
    if !updated.status.success() {
        return Err(invalid(String::from_utf8_lossy(&updated.stderr)));
    }
    Ok(())
}

pub(crate) fn absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
        && !Path::new(path)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

pub(crate) fn validate(
    workspace: &str,
    repository: &str,
    siblings: &[SiblingSnapshot],
) -> io::Result<()> {
    if !absolute(workspace)
        || !absolute(repository)
        || (workspace != repository && Path::new(repository).starts_with(workspace))
    {
        return Err(invalid("invalid workspace path"));
    }
    for s in siblings {
        if !absolute(&s.canonical_path)
            || Path::new(&s.canonical_path).parent() != Path::new(repository).parent()
            || s.canonical_path == repository
            || Path::new(workspace).starts_with(&s.canonical_path)
        {
            return Err(invalid("invalid sibling path"));
        }
    }
    Ok(())
}

/// Resolve existing ancestors as well as future destinations. A missing suffix
/// must not hide an existing symlink pointing outside an authorized root.
fn destination(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(error);
            }
            let parent = path
                .parent()
                .ok_or_else(|| invalid("destination has no parent"))?;
            Ok(destination(parent)?.join(
                path.file_name()
                    .ok_or_else(|| invalid("destination has no name"))?,
            ))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn validate_destinations(
    roots: &[PathBuf],
    key: &str,
    cwd: Option<&str>,
    siblings: &[SiblingSnapshot],
) -> io::Result<()> {
    let allowed: Vec<_> = roots
        .iter()
        .map(|root| {
            destination(root).map_err(|error| {
                invalid(format!(
                    "cannot resolve owned root: path={} allowed_roots={roots:?}: {error}",
                    root.display()
                ))
            })
        })
        .collect::<io::Result<_>>()?;
    let resolve = |path: &str| -> io::Result<PathBuf> {
        if absolute(path) {
            let canonical = destination(Path::new(path)).map_err(|error| {
                invalid(format!(
                    "cannot resolve destination: path={path} allowed_roots={roots:?}: {error}"
                ))
            })?;
            // Compare resolved paths only: a root or a request path may reach the
            // same directory through a symlinked prefix (macOS /var is
            // /private/var), and what matters is where the files actually land.
            if allowed
                .iter()
                .any(|resolved| canonical.starts_with(resolved) && canonical != *resolved)
            {
                return Ok(canonical);
            }
        }
        Err(invalid(format!(
            "destination outside runner-owned roots: path={path} allowed_roots={roots:?}"
        )))
    };
    let workspace = resolve(key)?;
    if let Some(cwd) = cwd {
        if !resolve(cwd)?.starts_with(&workspace) {
            return Err(invalid(format!("cwd outside workspace: {cwd}")));
        }
    }
    for sibling in siblings {
        resolve(&sibling.canonical_path)?;
    }
    Ok(())
}

pub(crate) struct Trees {
    pub mirrors: PathBuf,
    pub state: PathBuf,
    /// Mirrors with a compaction in flight.
    pub compacting: Arc<Mutex<BTreeSet<PathBuf>>>,
    pub dependency_locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
}

/// At most this many of a mirror's pinned commits are reported back in a
/// have reply, newest integration tips first. The sender checks each one
/// locally, so the bound keeps a long-lived runner's reply cheap to use.
const HELD_REPORT_LIMIT: usize = 32;

/// A mirror is compacted once it holds more than this many ordinary packs.
const MAX_MIRROR_PACKS: usize = 3;

/// Ordinary (non-cruft) packs in a bare mirror.
pub(crate) fn packs(mirror: &Path) -> usize {
    let Ok(entries) = fs::read_dir(mirror.join("objects/pack")) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "pack") && !p.with_extension("mtimes").exists()
        })
        .count()
}

/// Record a populated workspace, and the sibling commits it checked out.
pub(crate) fn record_workspace(
    catalog: &mut Catalog,
    key: &str,
    root: &str,
    commit: &str,
    siblings: &[SiblingSnapshot],
    now: u64,
) {
    record_siblings(catalog, siblings);
    catalog.workspaces.insert(
        key.to_string(),
        Workspace {
            kind: WorkspaceKind::Task,
            repository: root.to_string(),
            snapshot: commit.to_string(),
            siblings: siblings.to_vec(),
            used_ms: now,
        },
    );
}

pub(crate) fn record_siblings(catalog: &mut Catalog, siblings: &[SiblingSnapshot]) {
    for s in siblings {
        catalog
            .sibling_commits
            .insert(s.canonical_path.clone(), s.commit.clone());
    }
}

/// The warm build for `commit` of a repository, read from the root
/// `Cargo.toml` in its tree. A repository declares its own commands as
///
/// ```toml
/// [workspace.metadata.motor]
/// warm-commands = ["cargo nextest run --workspace --no-run"]
/// ```
///
/// (or under `[package.metadata.motor]` for a single-package repository), and
/// they run in order. A Cargo project that declares nothing builds every
/// target of its whole workspace, and a tree with no root `Cargo.toml` has
/// nothing to warm. The runner never assumes a particular repository's
/// package names.
pub(crate) fn warm_command(mirror: &Path, commit: &str) -> String {
    warm_plan(mirror, commit).0
}

/// Keep whether the command was explicitly requested, even when it equals the
/// default: a requested command must run rather than take a no-dependency shortcut.
pub(crate) fn warm_plan(mirror: &Path, commit: &str) -> (String, bool) {
    let Ok(manifest) = git(mirror, &["show", &format!("{commit}:Cargo.toml")]) else {
        return ("true".to_string(), false);
    };
    manifest_warm_plan(&manifest, commit)
}

/// The warm command a root `Cargo.toml` declares, or the whole-workspace
/// default; `commit` only labels warnings.
#[cfg(test)]
pub(crate) fn manifest_warm_command(manifest: &str, commit: &str) -> String {
    manifest_warm_plan(manifest, commit).0
}

fn manifest_warm_plan(manifest: &str, commit: &str) -> (String, bool) {
    const DEFAULT: &str = "cargo build --workspace --all-targets";
    let table = match manifest.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            tracing::warn!(%commit, %error, "unreadable Cargo.toml; warming the whole workspace");
            return (DEFAULT.to_string(), false);
        }
    };
    let declared = ["workspace", "package"].iter().find_map(|section| {
        table
            .get(*section)?
            .get("metadata")?
            .get("motor")?
            .get("warm-commands")
    });
    let Some(declared) = declared else {
        return (DEFAULT.to_string(), false);
    };
    let commands: Option<Vec<&str>> = declared
        .as_array()
        .map(|a| a.iter().map(|c| c.as_str()).collect::<Option<_>>())
        .unwrap_or(None);
    match commands {
        Some(commands) if !commands.is_empty() => (commands.join(" && "), true),
        _ => {
            tracing::warn!(%commit, "warm-commands is not a list of strings; warming the whole workspace");
            (DEFAULT.to_string(), false)
        }
    }
}

pub(crate) fn resolve_caller_commit(mirror: &Path, commit: &str, _tree_hash: &str) -> String {
    if let Ok(info) = git(mirror, &["log", "-1", "--format=%an%x00%s", commit]) {
        if let Some((author, subject)) = info.split_once('\0') {
            if author == "ck-motor" && subject == "ck-motor snapshot" {
                if let Ok(parent) = git(mirror, &["rev-parse", "--verify", &format!("{commit}^")]) {
                    return parent;
                }
            }
        }
    }
    commit.to_string()
}

impl Trees {
    pub fn mirror(&self, repository: &str) -> PathBuf {
        self.mirrors.join(name(repository))
    }
    pub fn ensure_mirror(&self, repository: &str) -> io::Result<PathBuf> {
        let path = self.mirror(repository);
        if !path.exists() {
            fs::create_dir_all(&path)?;
            git(&path, &["init", "--bare"])?;
        }
        Ok(path)
    }
    pub fn fetch(
        &self,
        directory: &Path,
        bundles: &[BundleManifest],
        root: &str,
        commit: &str,
        tip: &str,
        siblings: &[SiblingSnapshot],
    ) -> io::Result<()> {
        for b in bundles {
            if b.repository != root && !siblings.iter().any(|s| s.canonical_path == b.repository) {
                return Err(invalid("undeclared bundle repository"));
            }
        }
        for b in bundles {
            let mirror = self.ensure_mirror(&b.repository)?;
            // No destination ref: a later failed fetch leaves only unreachable
            // objects, not a held snapshot that have_reply would advertise.
            let spool = directory.join(b.index.to_string());
            let spool = spool.to_str().ok_or_else(|| invalid("spool path"))?;
            let heads = git(&mirror, &["bundle", "list-heads", spool])?;
            let refs: Vec<_> = heads
                .lines()
                .filter_map(|line| line.split_once(' ').map(|(_, r)| r))
                .collect();
            if refs.is_empty() || refs.iter().any(|r| !r.starts_with("refs/ck-motor/")) {
                return Err(invalid("invalid bundle refs"));
            }
            // Fetch's own automatic maintenance would start a detached gc
            // racing the compaction below; the runner compacts explicitly.
            let mut args = vec![
                "-c",
                "maintenance.auto=false",
                "fetch",
                "--progress",
                "--no-tags",
                "--no-write-fetch-head",
                spool,
            ];
            args.extend(refs);
            transfer::fetch(&mirror, &args)?;
        }
        for b in bundles {
            self.compact(&self.mirror(&b.repository));
        }
        for (repository, snapshot) in std::iter::once((root, commit)).chain(
            siblings
                .iter()
                .map(|s| (s.canonical_path.as_str(), s.commit.as_str())),
        ) {
            git(
                &self.ensure_mirror(repository)?,
                &["cat-file", "-e", &format!("{snapshot}^{{commit}}")],
            )?;
        }
        if !tip.is_empty() {
            git(
                &self.ensure_mirror(root)?,
                &["cat-file", "-e", &format!("{tip}^{{commit}}")],
            )?;
        }
        Ok(())
    }
    pub fn pin(
        &self,
        root: &str,
        commit: &str,
        tip: &str,
        siblings: &[SiblingSnapshot],
    ) -> io::Result<()> {
        for (repository, snapshot) in std::iter::once((root, commit)).chain(
            siblings
                .iter()
                .map(|s| (s.canonical_path.as_str(), s.commit.as_str())),
        ) {
            git(
                &self.mirror(repository),
                &[
                    "update-ref",
                    &format!("refs/snapshots/{snapshot}"),
                    snapshot,
                ],
            )?;
        }
        if !tip.is_empty() {
            git(
                &self.mirror(root),
                &["update-ref", &format!("refs/tips/{tip}"), tip],
            )?;
        }
        Ok(())
    }
    /// Report the newest offered commits reachable from pinned history, then
    /// up to 32 pinned ref targets as hints for older clients. Offered ancestry
    /// is not capped: a common older branch must remain a bundle prerequisite
    /// even when newer refs belong to snapshots the sender does not possess.
    pub fn have(&self, repositories: Vec<RepositoryCandidates>) -> Vec<RepositoryCandidates> {
        repositories
            .into_iter()
            .map(|mut r| {
                let path = self.mirror(&r.repository);
                // Integration tips carry real dates and sort first; snapshot
                // commits all share one fixed date and follow them.
                let listed = git(
                    &path,
                    &[
                        "for-each-ref",
                        "--sort=-committerdate",
                        "--format=%(objectname)",
                        "refs/tips",
                        "refs/snapshots",
                    ],
                )
                .unwrap_or_default();
                let mut refs: Vec<String> = Vec::new();
                for commit in listed.lines() {
                    if !refs.iter().any(|r| r == commit) {
                        refs.push(commit.to_string());
                    }
                }
                // Walk pinned history once, rather than spawning merge-base for
                // every candidate/ref pair. Report the newest common frontier
                // independently of the fallback ref cap: recent dirty snapshots
                // can crowd out the old branch tip the sender actually knows.
                let history = git(
                    &path,
                    &[
                        "rev-list",
                        "--parents",
                        "--glob=refs/tips/*",
                        "--glob=refs/snapshots/*",
                    ],
                )
                .unwrap_or_default();
                let edges: Vec<Vec<&str>> = history
                    .lines()
                    .map(|l| l.split_whitespace().collect())
                    .collect();
                let reachable: BTreeSet<&str> =
                    edges.iter().filter_map(|e| e.first().copied()).collect();
                let common: BTreeSet<&str> = r
                    .commits
                    .iter()
                    .map(String::as_str)
                    .filter(|c| reachable.contains(c))
                    .collect();
                let mut frontier = common.clone();
                for edge in &edges {
                    if common.contains(edge[0]) {
                        for parent in &edge[1..] {
                            frontier.remove(parent);
                        }
                    }
                }
                let frontier: BTreeSet<String> = frontier.into_iter().map(str::to_owned).collect();
                r.commits.retain(|c| frontier.contains(c));
                for held in refs.into_iter().take(HELD_REPORT_LIMIT) {
                    if !r.commits.contains(&held) {
                        r.commits.push(held);
                    }
                }
                r
            })
            .collect()
    }
    /// Fold a mirror's packs into one once transfers have added several. Each
    /// bundle fetch writes its own pack even when the mirror already has every
    /// object in it, so without this a mirror grows by one copy of whatever
    /// each transfer carried. Unreachable objects (failed or not yet pinned
    /// transfers, retired snapshots) go to a cruft pack and are only dropped
    /// after the same two-week grace git gc uses, so a transfer fetched but not
    /// yet pinned keeps its objects. The repack runs on its own thread so it
    /// never delays the job whose transfer triggered it, and at most one runs
    /// per mirror at a time.
    pub fn compact(&self, mirror: &Path) {
        if packs(mirror) <= MAX_MIRROR_PACKS
            || !self.compacting.lock().unwrap().insert(mirror.to_path_buf())
        {
            return;
        }
        let mirror = mirror.to_path_buf();
        let compacting = self.compacting.clone();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let before = packs(&mirror);
            match git(
                &mirror,
                &[
                    "repack",
                    "-d",
                    "-q",
                    "--cruft",
                    "--cruft-expiration=2.weeks.ago",
                ],
            ) {
                Ok(_) => tracing::info!(
                    mirror = %mirror.display(),
                    packs_before = before,
                    packs_after = packs(&mirror),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "mirror compacted"
                ),
                Err(error) => {
                    tracing::error!(mirror = %mirror.display(), %error, "mirror compaction failed")
                }
            }
            compacting.lock().unwrap().remove(&mirror);
        });
    }
    pub(crate) fn sync_workspace(
        &self,
        repository: &str,
        path: &Path,
        snapshot_commit: &str,
        tree_hash: &str,
        caller_commit: &str,
    ) -> io::Result<()> {
        #[cfg(test)]
        crate::test_cleanup::record_checkout_work("sync_workspace", path);
        let mirror = self.mirror(repository);
        let mirror_str = mirror.to_str().ok_or_else(|| invalid("mirror path"))?;
        let path_str = path.to_str().ok_or_else(|| invalid("workspace path"))?;

        if !path.join(".git").exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            git(
                &mirror,
                &["clone", "--shared", "--no-checkout", mirror_str, path_str],
            )?;
            git(path, &["checkout", "--force", "--detach", snapshot_commit])?;
            if git(path, &["read-tree", tree_hash]).is_ok() {
                git(path, &["clean", "-ffdx", "-e", "/target"])?;
            }
            if caller_commit != snapshot_commit {
                git(path, &["update-ref", "--no-deref", "HEAD", caller_commit])?;
                git(path, &["read-tree", "HEAD"])?;
                stage_snapshot_ignored(path, tree_hash)?;
            }
            return Ok(());
        }

        // Job isolation dissociates a checkout from its mirror before each job
        // (the job can't read the mirror), so a reused workspace or shared
        // sibling checkout no longer sees objects fetched into the mirror
        // since. Re-attach it so the next commit's tree is readable; the
        // next job's isolation dissociates it again.
        let alternates = path.join(".git/objects/info/alternates");
        if !alternates.is_file() {
            let objects = git(
                &mirror,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "objects",
                ],
            )?;
            fs::write(&alternates, format!("{}\n", objects.trim()))?;
        }

        let current_tree = self.tree_hash(path)?;
        if current_tree != tree_hash
            && git(
                &mirror,
                &["cat-file", "-e", &format!("{tree_hash}^{{tree}}")],
            )
            .is_ok()
        {
            let diff = git(
                path,
                &[
                    "diff-tree",
                    "--no-renames",
                    "--name-status",
                    "-r",
                    "-z",
                    &current_tree,
                    tree_hash,
                ],
            )?;
            let items: Vec<&str> = diff.split('\0').filter(|s| !s.is_empty()).collect();
            let mut to_checkout = Vec::new();
            let mut to_delete = Vec::new();
            let mut i = 0;
            while i + 1 < items.len() {
                let status = items[i];
                let file_path = items[i + 1];
                i += 2;
                if status.starts_with('D') {
                    to_delete.push(file_path);
                } else {
                    to_checkout.push(file_path);
                }
            }
            for file_path in to_delete {
                let full = path.join(file_path);
                if full.is_file() || full.is_symlink() {
                    let _ = fs::remove_file(&full);
                } else if full.is_dir() {
                    let _ = fs::remove_dir_all(&full);
                }
            }
            for chunk in to_checkout.chunks(100) {
                let mut args = vec!["--literal-pathspecs", "checkout", tree_hash, "--"];
                args.extend_from_slice(chunk);
                git(path, &args)?;
            }
            git(path, &["read-tree", tree_hash])?;
            git(path, &["clean", "-ffdx", "-e", "/target"])?;
        }

        let current_head = git(path, &["rev-parse", "HEAD"]).ok();
        let head_moved = current_head.as_deref() != Some(caller_commit);
        if head_moved {
            git(path, &["update-ref", "--no-deref", "HEAD", caller_commit])?;
        }
        if head_moved || current_tree != tree_hash {
            git(path, &["read-tree", "HEAD"])?;
            stage_snapshot_ignored(path, tree_hash)?;
        }

        Ok(())
    }
    /// Check out each sibling at its snapshot unless `recorded` (the catalog's
    /// sibling commits) and the checkout itself both already show it. Callers
    /// record the siblings in the catalog after this succeeds.
    pub fn siblings(
        &self,
        workspace: &Path,
        siblings: &[SiblingSnapshot],
        recorded: &BTreeMap<String, String>,
    ) -> io::Result<()> {
        fs::create_dir_all(
            workspace
                .parent()
                .ok_or_else(|| invalid("workspace parent"))?,
        )?;
        for s in siblings {
            let path = Path::new(&s.canonical_path);
            let caller_commit = s.base_commit.clone().unwrap_or_else(|| {
                resolve_caller_commit(&self.mirror(&s.canonical_path), &s.commit, &s.tree_hash)
            });
            // Catalog entries survive harness teardown and external deletion.
            // A recorded commit is not proof that its checkout still exists or
            // that another process has not moved its HEAD since materialization.
            let checkout_matches = path.is_dir()
                && path.join(".git").exists()
                && git(path, &["rev-parse", "HEAD"]).ok().as_deref() == Some(&caller_commit)
                && self.tree_hash(path).ok().as_deref() == Some(s.tree_hash.as_str());
            if recorded.get(&s.canonical_path) != Some(&s.commit) || !checkout_matches {
                self.sync_workspace(
                    &s.canonical_path,
                    path,
                    &s.commit,
                    &s.tree_hash,
                    &caller_commit,
                )?;
                if self.tree_hash(path)? != s.tree_hash {
                    return Err(invalid("sibling tree hash mismatch"));
                }
            }
            let link = workspace
                .parent()
                .ok_or_else(|| invalid("workspace parent"))?
                .join(path.file_name().ok_or_else(|| invalid("sibling name"))?);
            if link != path {
                if link.symlink_metadata().is_ok() {
                    if !link.is_symlink() || fs::read_link(&link)? != path {
                        return Err(invalid("sibling link conflicts"));
                    }
                } else {
                    std::os::unix::fs::symlink(path, link)?;
                }
            }
        }
        Ok(())
    }
    /// Check out source and siblings, preserving an existing target directory.
    /// New workspaces start with an empty target. Checkout runs without the
    /// engine state lock and the engine records the result afterwards.
    // The source repository, snapshot tree and caller commit have different
    // checkout roles; keep those inputs explicit beside the sibling snapshots.
    #[allow(clippy::too_many_arguments)]
    pub fn populate(
        &self,
        key: &str,
        root: &str,
        commit: &str,
        siblings: &[SiblingSnapshot],
        recorded: &BTreeMap<String, String>,
        tree_hash: &str,
        caller_commit: &str,
    ) -> io::Result<()> {
        let path = Path::new(key);
        self.siblings(path, siblings, recorded)?;
        self.sync_workspace(root, path, commit, tree_hash, caller_commit)?;
        let target = path.join("target");
        if !target.exists() {
            fs::create_dir_all(&target)?;
        }
        Ok(())
    }
    /// Populate a workspace and record it in `catalog` in one step, for tests
    /// that exercise materialization directly.
    #[cfg(test)]
    pub fn materialize(
        &self,
        key: &str,
        root: &str,
        commit: &str,
        siblings: &[SiblingSnapshot],
        now: u64,
        catalog: &mut Catalog,
    ) -> io::Result<()> {
        let tree_hash = git(
            &self.ensure_mirror(root)?,
            &["rev-parse", &format!("{commit}^{{tree}}")],
        )?;
        self.populate(
            key,
            root,
            commit,
            siblings,
            &catalog.sibling_commits,
            &tree_hash,
            commit,
        )?;
        record_workspace(catalog, key, root, commit, siblings, now);
        atomic_json(&self.state.join("catalog.json"), catalog)
    }
    // These parameters are distinct checkout inputs; grouping them would hide
    // rather than clarify which repository, snapshot, and commit each step uses.
    pub fn tree_hash(&self, path: &Path) -> io::Result<String> {
        let index = self.state.join(format!("index-{}", Uuid::now_v7()));
        let run = |args: &[&str]| -> io::Result<String> {
            let out = Command::new("git")
                .arg("--no-optional-locks")
                .arg("-C")
                .arg(path)
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_KEY_0")
                .env_remove("GIT_CONFIG_VALUE_0")
                .output()?;
            if !out.status.success() {
                return Err(invalid(String::from_utf8_lossy(&out.stderr)));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        // Target belongs to this runner, not to the synced source snapshot. A
        // warm clone must not enter the tree hash even in a minimal repository
        // that has no target entry in its ignore rules. Force-added target paths
        // in the real index are excluded too: staging cannot turn runner-owned
        // build artifacts into source inputs.
        let result = (|| {
            run(&["read-tree", "HEAD"])?;
            // git add reports an ignored target as an error even when a
            // negative pathspec excludes it. Enumerate source paths instead;
            // NUL framing also preserves filenames containing newlines.
            let listed = Command::new("git")
                .arg("--no-optional-locks")
                .arg("-C")
                .arg(path)
                .args([
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ])
                .env("GIT_INDEX_FILE", &index)
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_KEY_0")
                .env_remove("GIT_CONFIG_VALUE_0")
                .output()?;
            if !listed.status.success() {
                return Err(invalid(String::from_utf8_lossy(&listed.stderr)));
            }
            // Read the real index, not the temporary HEAD index, to recover
            // ignored inputs that were force-added but never committed.
            let ignored = git_command(path)
                .args([
                    "ls-files",
                    "-z",
                    "--cached",
                    "--ignored",
                    "--exclude-standard",
                ])
                .output()?;
            if !ignored.status.success() {
                return Err(invalid(String::from_utf8_lossy(&ignored.stderr)));
            }
            let mut paths = Vec::new();
            for name in listed
                .stdout
                .split(|b| *b == 0)
                .chain(ignored.stdout.split(|b| *b == 0))
                .filter(|s| !s.is_empty())
            {
                if name != b"target" && !name.starts_with(b"target/") {
                    paths.extend_from_slice(name);
                    paths.push(0);
                }
            }
            // --remove records tracked deletions and omits force-added ignored
            // paths that no longer exist on disk, matching snapshot capture.
            let mut child = Command::new("git")
                .arg("--no-optional-locks")
                .arg("-C")
                .arg(path)
                .args(["update-index", "--add", "--remove", "-z", "--stdin"])
                .env("GIT_INDEX_FILE", &index)
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_KEY_0")
                .env_remove("GIT_CONFIG_VALUE_0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(&paths)?;
            let updated = child.wait_with_output()?;
            if !updated.status.success() {
                return Err(invalid(String::from_utf8_lossy(&updated.stderr)));
            }
            run(&["write-tree"])
        })();
        let _ = fs::remove_file(index);
        result
    }
    pub fn changes(&self, path: &Path) -> Vec<String> {
        let Ok(status) = git(
            path,
            &["status", "--porcelain", "-z", "--untracked-files=all"],
        ) else {
            return vec![];
        };
        let mut records = status.split('\0');
        let mut changes = Vec::new();
        while let Some(record) = records.next() {
            let Some(path) = record.get(3..) else {
                continue;
            };
            let prefix = &record.as_bytes()[..3];
            if prefix[2] != b' ' {
                continue;
            }
            changes.push(path.to_string());
            // Porcelain -z emits the destination first, then an unprefixed
            // source path for renames and copies. The source is not a status.
            if prefix[..2].iter().any(|b| matches!(b, b'R' | b'C')) {
                records.next();
            }
        }
        changes
    }
}
