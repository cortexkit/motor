use crate::caller_path as paths;
#[cfg(test)]
use crate::storage::atomic_json;
use crate::storage::{invalid, Catalog, Workspace};
use motor_protocol::*;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::path::Component;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

pub(crate) mod dependencies;
#[cfg(test)]
mod fetch_tests;
mod transfer;
pub(crate) use dependencies::validate_dependencies;
pub(crate) use transfer::bounded_fetch_detail;

/// A file in each JavaScript install and manager cache entry naming the
/// mirror (`name` of the repository root) whose installs created it, so the
/// entry can be retired with that repository.
pub(crate) const JAVASCRIPT_CACHE_OWNER: &str = "motor-repository";

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

/// `git` with its standard output returned byte for byte, for output that
/// carries paths: a path need not be valid UTF-8.
pub(crate) fn git_bytes(path: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = git_command(path).args(args).output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}

pub(crate) fn delete_refs(path: &Path, references: &[String]) -> io::Result<()> {
    if references.is_empty() {
        return Ok(());
    }
    let mut input = String::from("start\n");
    for reference in references {
        input.push_str(&format!("delete {reference}\n"));
    }
    input.push_str("prepare\ncommit\n");
    let mut command = git_command(path);
    command.args(["update-ref", "--stdin"]);
    let output = git_input(command, input.as_bytes())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(invalid(String::from_utf8_lossy(&output.stderr)))
    }
}

/// Point each named ref at its commit in one transaction. Git refuses a target
/// object the repository does not have, so a successful call proves every
/// commit is present and now a ref target.
pub(crate) fn update_refs(path: &Path, references: &[(String, String)]) -> io::Result<()> {
    if references.is_empty() {
        return Ok(());
    }
    let mut input = String::from("start\n");
    for (reference, commit) in references {
        input.push_str(&format!("update {reference} {commit}\n"));
    }
    input.push_str("prepare\ncommit\n");
    let mut command = git_command(path);
    command.args(["update-ref", "--stdin"]);
    let output = git_input(command, input.as_bytes())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(invalid(String::from_utf8_lossy(&output.stderr)))
    }
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
    #[cfg(windows)]
    {
        // These are caller checkout identifiers, not paths on the Windows host.
        // A POSIX root is intentionally not Path::is_absolute on Windows.
        path.starts_with('/')
            && !path.contains(['\\', '\0'])
            && !path.split('/').any(|part| matches!(part, "." | ".."))
    }
    #[cfg(unix)]
    {
        Path::new(path).is_absolute()
            && !Path::new(path)
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    }
}

#[cfg(test)]
pub(crate) fn validate(
    workspace: &str,
    repository: &str,
    siblings: &[SiblingSnapshot],
    snapshot_paths: bool,
) -> io::Result<()> {
    validate_names(workspace, repository, siblings, snapshot_paths)?;
    validate_host_overlap(
        paths::host_str(workspace),
        paths::host_str(repository),
        siblings,
        snapshot_paths,
    )
}

pub(crate) fn validate_request(
    plan: paths::RequestPlan<'_>,
    siblings: &[SiblingSnapshot],
    snapshot_paths: bool,
) -> io::Result<()> {
    validate_names(
        plan.caller_workspace_key.text(),
        plan.caller_repository_root.text(),
        siblings,
        snapshot_paths,
    )
    .and_then(|()| {
        validate_host_overlap(
            plan.host_workspace,
            plan.host_repository_root,
            siblings,
            snapshot_paths,
        )
    })
    .map_err(|error| {
        crate::workspace_access::Unreadable::wrap(
            plan.host_workspace.as_path(),
            "validate workspace paths",
            error,
        )
    })
}

pub(crate) fn validate_names(
    workspace: &str,
    repository: &str,
    siblings: &[SiblingSnapshot],
    snapshot_paths: bool,
) -> io::Result<()> {
    if !absolute(workspace)
        || !absolute(repository)
        || (workspace != repository && Path::new(repository).starts_with(workspace))
    {
        return Err(invalid("invalid workspace path"));
    }
    for s in siblings {
        if !absolute(paths::sibling(s).caller.text())
            || (!snapshot_paths
                && Path::new(paths::sibling(s).caller.text()).parent()
                    != Path::new(repository).parent())
            || paths::sibling(s).caller.text() == repository
            || Path::new(workspace).starts_with(paths::sibling(s).caller.text())
        {
            return Err(invalid("invalid sibling path"));
        }
    }
    Ok(())
}

fn validate_host_overlap(
    workspace: paths::HostPath<'_>,
    repository: paths::HostPath<'_>,
    siblings: &[SiblingSnapshot],
    snapshot_paths: bool,
) -> io::Result<()> {
    if snapshot_paths {
        let mut paths = vec![
            destination(workspace.as_path())?,
            destination(repository.as_path())?,
        ];
        for (sibling, alias) in siblings.iter().zip(sibling_aliases(workspace, siblings)?) {
            let path = destination(paths::sibling(sibling).host.as_path())?;
            let alias = alias.canonical;
            for mount in [&path, &alias] {
                if paths
                    .iter()
                    .any(|other| mount.starts_with(other) || other.starts_with(mount))
                {
                    return Err(invalid(
                        "sibling or its worktree alias overlaps workspace, repository or another sibling",
                    ));
                }
            }
            paths.push(path);
            paths.push(alias);
        }
    }
    Ok(())
}

/// Resolve existing ancestors as well as future destinations. A missing suffix
/// must not hide an existing symlink pointing outside an authorized root.
pub(crate) fn destination(path: &Path) -> io::Result<PathBuf> {
    resolve_destination(path).map_err(|error| {
        crate::workspace_access::Unreadable::wrap(path, "resolve destination", error)
    })
}

fn resolve_destination(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::symlink_metadata(path) {
                // Created after canonicalize looked, by a setup running
                // alongside (a first workspace and a cache fill both create
                // the shared parent folder): look once more.
                Ok(metadata) if !metadata.file_type().is_symlink() => {
                    return fs::canonicalize(path);
                }
                Ok(_) => return Err(error),
                Err(_) => {}
            }
            let parent = path
                .parent()
                .ok_or_else(|| invalid("destination has no parent"))?;
            Ok(resolve_destination(parent)?.join(
                path.file_name()
                    .ok_or_else(|| invalid("destination has no name"))?,
            ))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn validate_destinations(
    roots: &[PathBuf],
    key: paths::HostPath<'_>,
    cwd: Option<paths::HostPath<'_>>,
    siblings: &[SiblingSnapshot],
) -> io::Result<()> {
    let allowed = allowed_roots(roots)?;
    let resolve = |path: paths::HostPath<'_>| {
        authorize_destination(
            roots,
            &allowed,
            &path.as_path().to_string_lossy(),
            destination(path.as_path()),
        )
    };
    let workspace = resolve(key)?;
    if let Some(cwd) = cwd {
        if !resolve(cwd)?.starts_with(&workspace) {
            return Err(invalid(format!(
                "cwd outside workspace: {}",
                cwd.as_path().display()
            )));
        }
    }
    for sibling in siblings {
        resolve(paths::sibling(sibling).host)?;
    }
    Ok(())
}

fn allowed_roots(roots: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    roots
        .iter()
        .map(|root| {
            destination(root).map_err(|error| {
                invalid(format!(
                    "cannot resolve owned root: path={} allowed_roots={roots:?}: {error}",
                    root.display()
                ))
            })
        })
        .collect()
}

fn authorize_destination(
    roots: &[PathBuf],
    allowed: &[PathBuf],
    path: &str,
    canonical: io::Result<PathBuf>,
) -> io::Result<PathBuf> {
    if absolute(path) {
        let canonical = canonical.map_err(|error| {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return crate::workspace_access::Unreadable::wrap(
                    Path::new(path),
                    "resolving the path inside the runner-owned roots",
                    error,
                );
            }
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
        "destination outside runner-owned roots: path={path} allowed_roots={roots:?}; \
             move the checkout under one of the allowed roots"
    )))
}

/// A snapshot sibling's worktree alias and its resolved mount destination.
/// A dangling host alias is accepted only if its target resolves to the sibling's
/// authorized path. The sandbox hides that link with a snapshot mount.
pub(crate) struct SiblingAlias {
    pub path: PathBuf,
    pub canonical: PathBuf,
}

pub(crate) fn validate_alias_destinations(
    roots: &[PathBuf],
    aliases: &[SiblingAlias],
) -> io::Result<()> {
    let allowed = allowed_roots(roots)?;
    for alias in aliases {
        authorize_destination(
            roots,
            &allowed,
            alias
                .path
                .to_str()
                .ok_or_else(|| invalid("non-UTF8 sibling alias"))?,
            Ok(alias.canonical.clone()),
        )?;
    }
    Ok(())
}

pub(crate) fn sibling_aliases(
    key: paths::HostPath<'_>,
    siblings: &[SiblingSnapshot],
) -> io::Result<Vec<SiblingAlias>> {
    siblings
        .iter()
        .map(|sibling| {
            let path = key
                .as_path()
                .parent()
                .ok_or_else(|| invalid("workspace has no parent"))?
                .join(
                    paths::sibling(sibling)
                        .host
                        .as_path()
                        .file_name()
                        .ok_or_else(|| invalid("sibling path has no name"))?,
                );
            let canonical = match destination(&path) {
                Ok(canonical) => canonical,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) =>
                {
                    let parent = path
                        .parent()
                        .ok_or_else(|| invalid("alias has no parent"))?;
                    let target = parent.join(fs::read_link(&path).map_err(|error| {
                        crate::workspace_access::Unreadable::wrap(
                            &path,
                            "read sibling alias",
                            error,
                        )
                    })?);
                    if destination(&target)? != destination(paths::sibling(sibling).host.as_path())?
                    {
                        return Err(error);
                    }
                    // Retiring an in-place sibling leaves its worktree alias dangling.
                    // Snapshot mounts hide that host link: validate the virtual mount
                    // node, but only for a link to this sibling's authorized path.
                    destination(parent)?.join(
                        path.file_name()
                            .ok_or_else(|| invalid("alias has no name"))?,
                    )
                }
                Err(error) => return Err(error),
            };
            Ok(SiblingAlias { path, canonical })
        })
        .collect::<io::Result<Vec<_>>>()
        .map_err(|error| {
            crate::workspace_access::Unreadable::wrap(
                key.as_path(),
                "resolve sibling aliases",
                error,
            )
        })
}

pub(crate) struct Trees {
    pub mirrors: PathBuf,
    pub state: PathBuf,
    /// Mirrors with a compaction in flight.
    pub compacting: Arc<Mutex<BTreeSet<PathBuf>>>,
    pub mirror_locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
}

/// At most this many of a mirror's pinned commits are reported back in a
/// have reply, newest integration tips first. The sender checks each one
/// locally, so the bound keeps a long-lived runner's reply cheap to use.
const HELD_REPORT_LIMIT: usize = 32;

/// Namespace of the refs a have reply leaves behind: one per reported commit,
/// named `refs/held/<commit>`. The sender builds its bundle with the
/// reported commits as prerequisites, and git accepts a prerequisite only when
/// it is reachable from a ref or its whole unreferenced history is still
/// intact. The records that keep snapshot and tip refs alive can expire
/// between the reply and the bundle's arrival, so the reply pins its own refs.
/// The runner prunes them once the offer's lifetime has passed.
pub(crate) const HELD_REFS: &str = "refs/held/";

/// Prerequisite commits a bundle file declares: the `-<oid>` lines of its
/// header, which ends at the first empty line, before the pack data.
pub(crate) fn bundle_prerequisites(path: &Path) -> io::Result<Vec<String>> {
    use std::io::{BufRead, Read};
    // A header lists every ref and prerequisite; a megabyte is far more than
    // any real bundle needs, and the bound keeps a corrupt file from being
    // read whole.
    let mut reader = io::BufReader::new(fs::File::open(path)?).take(1 << 20);
    let mut prerequisites = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 || line == b"\n" {
            return Ok(prerequisites);
        }
        if let Some(rest) = line.strip_prefix(b"-") {
            let oid: String = String::from_utf8_lossy(rest)
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            if !oid.is_empty() {
                prerequisites.push(oid);
            }
        }
    }
}

/// A loose ref file holds an object id or a `ref: ` symbolic target. After an
/// unclean stop a ref file can be empty or partly written; git then fails
/// every fetch's connectivity check while `for-each-ref` only warns about it.
fn valid_loose_ref(content: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(content) else {
        return false;
    };
    let text = text.trim_end_matches('\n');
    text.starts_with("ref: ")
        || (matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Remove the refs of a mirror that git cannot use, and return each removed
/// ref with the reason. A ref is unusable when its file is unreadable, its
/// commit or that commit's root tree is missing, or it points at one of
/// `commits` (a failed bundle's prerequisites) whose root tree is missing.
/// One such ref makes every later bundle fetch into the mirror fail, and a
/// have reply would keep reporting what it points at. Objects are never
/// deleted: checkouts borrow them through alternates. The check reads only
/// the ref files, one `for-each-ref` and one `cat-file --batch-check`, so it
/// does not walk history. Callers hold the runner's ref mutation lock.
pub(crate) fn repair_mirror(mirror: &Path, commits: &[String]) -> io::Result<Vec<String>> {
    let mut removed = Vec::new();
    let mut pending = vec![mirror.join("refs")];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if path.extension().is_none_or(|e| e != "lock")
                && !valid_loose_ref(&fs::read(&path).unwrap_or_default())
            {
                fs::remove_file(&path)?;
                let name = path.strip_prefix(mirror).unwrap_or(&path);
                removed.push(format!("{} (unreadable ref file)", name.display()));
            }
        }
    }
    let listed = git(
        mirror,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    )?;
    let refs: Vec<(&str, &str)> = listed.lines().filter_map(|l| l.split_once(' ')).collect();
    let mut input = String::new();
    for (oid, _) in &refs {
        input.push_str(&format!("{oid}\n{oid}^{{tree}}\n"));
    }
    for commit in commits {
        input.push_str(&format!("{commit}^{{tree}}\n"));
    }
    let mut command = git_command(mirror);
    command.args(["cat-file", "--batch-check=%(objecttype)"]);
    let output = git_input(command, input.as_bytes())?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    let types = String::from_utf8_lossy(&output.stdout);
    let types: Vec<&str> = types.lines().collect();
    if types.len() != refs.len() * 2 + commits.len() {
        return Err(invalid("cat-file --batch-check answered a different count"));
    }
    let broken_commits: BTreeSet<&str> = commits
        .iter()
        .zip(&types[refs.len() * 2..])
        .filter(|(_, kind)| **kind != "tree")
        .map(|(commit, _)| commit.as_str())
        .collect();
    let mut stale = Vec::new();
    for (i, (oid, name)) in refs.iter().enumerate() {
        let reason = if types[2 * i] != "commit" {
            "commit missing"
        } else if types[2 * i + 1] != "tree" {
            "tree missing"
        } else if broken_commits.contains(oid) {
            "prerequisite tree missing"
        } else {
            continue;
        };
        stale.push((name.to_string(), reason));
    }
    let names: Vec<String> = stale.iter().map(|(name, _)| name.clone()).collect();
    if delete_refs(mirror, &names).is_err() {
        // One ref the transaction cannot lock must not keep the others.
        stale.retain(|(name, _)| git(mirror, &["update-ref", "-d", name]).is_ok());
    }
    removed.extend(
        stale
            .into_iter()
            .map(|(name, reason)| format!("{name} ({reason})")),
    );
    Ok(removed)
}

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
    record_workspace_entry(catalog, key, root, commit, siblings, now);
}

/// Record a populated workspace without recording sibling checkouts, for a
/// setup that mounted its siblings from snapshots and so checked none out at
/// its host path. The entry's sibling freshness record is kept: it describes
/// the workspace's `target/`, which a new setup keeps.
pub(crate) fn record_workspace_entry(
    catalog: &mut Catalog,
    key: &str,
    root: &str,
    commit: &str,
    siblings: &[SiblingSnapshot],
    now: u64,
) {
    let sibling_freshness = catalog
        .workspaces
        .remove(key)
        .map(|previous| previous.sibling_freshness)
        .unwrap_or_default();
    catalog.workspaces.insert(
        key.to_string(),
        Workspace {
            kind: WorkspaceKind::Task,
            repository: root.to_string(),
            snapshot: commit.to_string(),
            siblings: siblings.to_vec(),
            used_ms: now,
            sibling_freshness,
        },
    );
}

pub(crate) fn record_siblings(catalog: &mut Catalog, siblings: &[SiblingSnapshot]) {
    for s in siblings {
        catalog
            .sibling_commits
            .insert(paths::sibling(s).caller.text().clone(), s.commit.clone());
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
/// nothing to warm from Cargo. The runner never assumes a particular
/// repository's package names.
#[cfg(test)]
pub(crate) fn warm_command(mirror: &Path, commit: &str) -> String {
    warm_plan(mirror, commit).0
}

/// Keep whether the command was explicitly requested, even when it equals the
/// default: a requested command must run rather than take a no-dependency shortcut.
///
/// A tree with no root `Cargo.toml` has no Cargo fill, so whatever its
/// `.motor/cache.toml` declares is the whole warm plan. Its steps are
/// explicit for the same reason, and a tree that declares nothing still runs
/// `true`.
///
/// The one command a fill runs is the plan's steps joined with `&&`, which is
/// what [`warm_steps`] returns separately for a caller that keeps the steps.
#[cfg(test)]
pub(crate) fn warm_plan(mirror: &Path, commit: &str) -> (String, bool) {
    let (steps, explicit) = warm_steps(mirror, commit);
    (steps.join(" && "), explicit)
}

/// The warm build as the ordered steps a fill runs, together with whether any
/// of them was explicitly requested.
///
/// A Cargo tree warms with Cargo's own command, then with whatever
/// `.motor/cache.toml` declares. A tree with no root `Cargo.toml` has no Cargo
/// command and warms from its declaration alone. Steps run in this order in one
/// shell, so a failing step fails the fill.
pub(crate) fn warm_steps(mirror: &Path, commit: &str) -> (Vec<String>, bool) {
    let declared: Vec<String> = crate::writer::declaration::read(mirror, commit)
        .ok()
        .flatten()
        .map(|declaration| {
            declaration
                .fills
                .iter()
                .map(crate::writer::declaration::Step::command)
                .collect()
        })
        .unwrap_or_default();
    let mut steps = Vec::new();
    let mut explicit = false;
    if let Ok(manifest) = git(mirror, &["show", &format!("{commit}:Cargo.toml")]) {
        let (cargo, requested) = manifest_warm_plan(&manifest, commit);
        steps.push(cargo);
        explicit = requested;
    }
    // Declared steps follow the Cargo warm command in the same fill, so a Cargo
    // tree that also declares a ccache step runs both. A tree with neither
    // warms with `true`, as it did before declarations existed.
    steps.extend(declared.iter().cloned());
    explicit |= !declared.is_empty();
    if steps.is_empty() {
        steps.push("true".to_string());
    }
    (steps, explicit)
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
    pub fn mirror_lock(&self, mirror: &Path) -> Arc<Mutex<()>> {
        self.mirror_locks
            .lock()
            .unwrap()
            .entry(mirror.to_path_buf())
            .or_default()
            .clone()
    }

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
        self.fetch_with(
            directory,
            bundles,
            (root, commit, tip, siblings),
            |_| Ok(()),
        )
    }

    fn fetch_with(
        &self,
        directory: &Path,
        bundles: &[BundleManifest],
        snapshot: (&str, &str, &str, &[SiblingSnapshot]),
        before_fetch: impl Fn(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        let (root, commit, tip, siblings) = snapshot;
        for b in bundles {
            if b.repository != root
                && !siblings
                    .iter()
                    .any(|s| paths::sibling(s).caller.text() == &b.repository)
            {
                return Err(invalid("undeclared bundle repository"));
            }
        }
        for b in bundles {
            // Initialization and import share one per-mirror guard. Holding it
            // outside the runner's state lock leaves other repositories free.
            let mirror = self.mirror(&b.repository);
            let lock = self.mirror_lock(&mirror);
            let _fetch_guard = lock.lock().unwrap();
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
            before_fetch(&mirror)?;
            transfer::fetch(&mirror, &args)?;
        }
        for b in bundles {
            self.compact(&self.mirror(&b.repository));
        }
        for (repository, snapshot) in std::iter::once((root, commit)).chain(
            siblings
                .iter()
                .map(|s| (paths::sibling(s).caller.text().as_str(), s.commit.as_str())),
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
                .map(|s| (paths::sibling(s).caller.text().as_str(), s.commit.as_str())),
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
    /// The answer is only valid while those refs exist; `Runner::have` pins
    /// it with `HELD_REFS` before replying.
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
                // Held refs (see HELD_REFS) are deliberately not walked:
                // they exist only to keep an earlier reply valid, and counting
                // them would let every reply renew the previous one forever.
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
        let fetch_lock = self.mirror_lock(&mirror);
        let compacting = self.compacting.clone();
        std::thread::spawn(move || {
            // Repacking must not rewrite the object store during an import.
            let _fetch_guard = fetch_lock.lock().unwrap();
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
        path: paths::HostPath<'_>,
        snapshot_commit: &str,
        tree_hash: &str,
        caller_commit: &str,
    ) -> io::Result<()> {
        let path = path.as_path();
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
            let path = paths::sibling(s).host.as_path();
            let caller_commit = s.base_commit.clone().unwrap_or_else(|| {
                resolve_caller_commit(
                    &self.mirror(paths::sibling(s).caller.text()),
                    &s.commit,
                    &s.tree_hash,
                )
            });
            // Catalog entries survive harness teardown and external deletion.
            // A recorded commit is not proof that its checkout still exists or
            // that another process has not moved its HEAD since materialization.
            let checkout_matches = path.is_dir()
                && path.join(".git").exists()
                && git(path, &["rev-parse", "HEAD"]).ok().as_deref() == Some(&caller_commit)
                && self.tree_hash(path).ok().as_deref() == Some(s.tree_hash.as_str());
            if recorded.get(paths::sibling(s).caller.text()) != Some(&s.commit) || !checkout_matches
            {
                self.sync_workspace(
                    paths::sibling(s).caller.text(),
                    crate::caller_path::host(path),
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
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(path, link)?;
                    #[cfg(windows)]
                    return Err(invalid(
                        "native Windows hosts do not materialize host workspaces",
                    ));
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
        key: paths::HostPath<'_>,
        root: &str,
        commit: &str,
        siblings: &[SiblingSnapshot],
        recorded: &BTreeMap<String, String>,
        tree_hash: &str,
        caller_commit: &str,
    ) -> io::Result<()> {
        self.siblings(key.as_path(), siblings, recorded)?;
        self.populate_main(key, root, commit, tree_hash, caller_commit)
    }
    /// The main-workspace part of `populate`: check out the source,
    /// preserving an existing target directory. A setup that mounts its
    /// siblings from snapshots calls this alone, so it never touches a
    /// sibling's host path.
    pub(crate) fn populate_main(
        &self,
        key: paths::HostPath<'_>,
        root: &str,
        commit: &str,
        tree_hash: &str,
        caller_commit: &str,
    ) -> io::Result<()> {
        let path = key.as_path();
        self.sync_workspace(
            root,
            crate::caller_path::host(path),
            commit,
            tree_hash,
            caller_commit,
        )?;
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
            crate::caller_path::host_str(key),
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
        // Exclude the root target entry and all its descendants regardless of Git
        // tracking or ignore status; nested directories such as src/target are source.
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
            let mut remove_paths = Vec::new();
            for name in listed
                .stdout
                .split(|b| *b == 0)
                .chain(ignored.stdout.split(|b| *b == 0))
                .filter(|s| !s.is_empty())
            {
                if name != b"target" && !name.starts_with(b"target/") {
                    paths.extend_from_slice(name);
                    paths.push(0);
                } else {
                    remove_paths.extend_from_slice(name);
                    remove_paths.push(0);
                }
            }
            // Skipping updates alone would retain committed target entries from
            // read-tree HEAD. Remove them from the temporary index as well.
            if !remove_paths.is_empty() {
                let mut remove = git_command(path);
                remove.env("GIT_INDEX_FILE", &index).args([
                    "update-index",
                    "--force-remove",
                    "-z",
                    "--stdin",
                ]);
                let removed = git_input(remove, &remove_paths)?;
                if !removed.status.success() {
                    return Err(invalid(String::from_utf8_lossy(&removed.stderr)));
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

    pub(crate) fn tree_hash_mismatch(path: &Path, expected: &str, computed: &str) -> io::Error {
        let diff = git_command(path)
            .args([
                "diff-tree",
                "--no-renames",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-z",
                "--end-of-options",
                expected,
                computed,
                "--",
            ])
            .output();
        let paths = match diff {
            Ok(diff) if diff.status.success() => {
                let names: Vec<_> = diff
                    .stdout
                    .split(|b| *b == 0)
                    .filter(|n| !n.is_empty())
                    .collect();
                // Keep complete path text until refusal redaction runs; cutting
                // a path here could expose a prefix of an environment secret.
                let mut shown = names
                    .iter()
                    .take(5)
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                if names.len() > 5 {
                    // Keep the count visible even if long paths reach the detail limit.
                    shown = format!("({} more) {shown}", names.len() - 5);
                }
                shown
            }
            _ => "unavailable (caller tree cannot be compared)".into(),
        };
        invalid(format!(
            "tree_hash_mismatch: computed_tree={computed}; differing paths: {paths}"
        ))
    }

    /// Paths the job changed in its workspace: modified, deleted, staged and
    /// new untracked files. Writes under ignored paths such as target/ are not
    /// listed, nor are node_modules trees the runner prepared in the supplied
    /// workspace-relative package directories. A failed scan is an error, never
    /// an empty list, because callers read an empty list as "no changes".
    pub fn changes(
        &self,
        path: &Path,
        prepared_javascript_directories: &[PathBuf],
    ) -> io::Result<Vec<String>> {
        // After an isolated job the workspace root belongs to the job account,
        // so Git refuses the runner's scan as "dubious ownership" unless this
        // one tree is trusted. Trusting it is safe: the workspace's .git, and
        // so all Git configuration and hooks, stays runner-owned and read-only
        // to jobs, and status only reads the job's files as data. fsmonitor
        // is off so no configured helper process runs.
        let safe = format!("safe.directory={}", path.display());
        let status = git(
            path,
            &[
                "-c",
                &safe,
                "-c",
                "core.fsmonitor=false",
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
            ],
        )?;
        let mut records = status.split('\0');
        let prepared_modules: Vec<_> = prepared_javascript_directories
            .iter()
            .map(|directory| directory.join("node_modules"))
            .collect();
        let mut changes = Vec::new();
        while let Some(record) = records.next() {
            let Some(path) = record.get(3..) else {
                continue;
            };
            let prefix = &record.as_bytes()[..3];
            if prefix[2] != b' ' {
                continue;
            }
            if !prepared_modules
                .iter()
                .any(|modules| Path::new(path).starts_with(modules))
            {
                changes.push(path.to_string());
            }
            // Porcelain -z emits the destination first, then an unprefixed
            // source path for renames and copies. The source is not a status.
            if prefix[..2].iter().any(|b| matches!(b, b'R' | b'C')) {
                records.next();
            }
        }
        Ok(changes)
    }
}
