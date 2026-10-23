//! Host-only macOS staging and dependency preparation, before VM admission.
//!
//! The scheduler must keep the job Queued until `Staging::prepare` returns. This
//! module owns no VM slots and exposes only per-job trees to its caller. Mirrors
//! must already contain the validated snapshot and dependency bundles.
use crate::{storage::invalid, workspace::Trees};
use motor_protocol::Run;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
mod execution;
mod fs;
mod lifecycle;
mod preflight;
mod process;
pub(crate) use execution::MacosBackend;
pub(crate) use lifecycle::evict;
pub(crate) use lifecycle::{Runtime, VmConfig};
#[cfg(test)]
mod tests;
use fs::Dir;

/// Maximum preparation runtime: ten minutes, including time spent waiting for locks.
pub const PREP_TIMEOUT: Duration = Duration::from_secs(600);

/// Validate identifiers without inspecting or touching the corresponding host paths.
pub fn validate_paths(request: &Run) -> io::Result<()> {
    if request.siblings.len() > 62 {
        return Err(invalid(
            "macos_path_invalid: at most 62 sibling shares are supported",
        ));
    }
    for path in [
        &request.repository_root,
        &request.workspace_key,
        &request.cwd,
    ]
    .into_iter()
    .chain(request.siblings.iter().map(|s| &s.canonical_path))
    {
        mapped(path)?;
    }
    if !Path::new(&request.cwd.to_ascii_lowercase())
        .starts_with(request.workspace_key.to_ascii_lowercase())
    {
        return Err(invalid("macos_path_invalid: cwd outside workspace"));
    }
    let roots: Vec<_> = std::iter::once(&request.workspace_key)
        .chain(request.siblings.iter().map(|s| &s.canonical_path))
        .collect();
    for (i, root) in roots.iter().enumerate() {
        if roots.iter().skip(i + 1).any(|other| {
            let root = root.to_ascii_lowercase();
            let other = other.to_ascii_lowercase();
            Path::new(&root).starts_with(&other) || Path::new(&other).starts_with(&root)
        }) {
            return Err(invalid("macos_path_invalid: overlapping snapshots"));
        }
    }
    Ok(())
}

fn mapped(path: &str) -> io::Result<&Path> {
    if !path.starts_with("/Users/")
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
        || path[1..].split('/').any(|c| matches!(c, "" | "." | ".."))
    {
        return Err(invalid(format!("macos_path_invalid: {path}")));
    }
    Ok(Path::new(&path[1..]))
}

/// Path to a standalone tool binary and its expected SHA-256.
#[derive(Clone, serde::Deserialize)]
pub struct Tool {
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Clone, serde::Deserialize)]
pub struct Tools {
    pub cargo: Tool,
    pub rustc: Tool,
    pub bun: Tool,
    pub git: Tool,
}
impl Tools {
    fn verify(&self, control: &Control) -> io::Result<()> {
        for tool in [&self.cargo, &self.rustc, &self.bun, &self.git] {
            let parent = Dir::open(
                tool.path.parent().ok_or_else(|| invalid("tool parent"))?,
                false,
            )?;
            let mut file = parent.file(
                tool.path.file_name().ok_or_else(|| invalid("tool name"))?,
                false,
            )?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                control.check()?;
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            if format!("{:x}", hash.finalize()) != tool.sha256 {
                return Err(invalid(format!(
                    "pinned tool hash mismatch: {}",
                    tool.path.display()
                )));
            }
        }
        Ok(())
    }
}

/// Cancellation is checked while acquiring locks and while any prep process runs.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}
struct Control {
    cancel: Cancellation,
    deadline: Instant,
}
/// Controller-only flock ownership ends here, not when the last forked copy
/// happens to exec. Slot leases are different: helpers intentionally share them.
struct ScopedLock {
    file: File,
    owner: libc::pid_t,
}
impl ScopedLock {
    fn new_locked(file: File) -> Self {
        Self {
            file,
            owner: unsafe { libc::getpid() },
        }
    }
    fn try_exclusive(file: File) -> io::Result<Self> {
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(Self::new_locked(file))
    }
}
fn unlock_file(file: &File) -> io::Result<()> {
    loop {
        match fs2::FileExt::unlock(file) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}
impl Drop for ScopedLock {
    fn drop(&mut self) {
        // A fork-without-exec child must not unlock its still-live parent's
        // ownership if it happens to drop inherited Rust values.
        if self.owner == unsafe { libc::getpid() } {
            let _ = unlock_file(&self.file);
        }
    }
}
impl std::os::fd::AsRawFd for ScopedLock {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.file)
    }
}
impl Control {
    fn check(&self) -> io::Result<()> {
        if self.cancel.0.load(Ordering::Acquire) {
            return Err(invalid("macOS preparation cancelled"));
        }
        if Instant::now() >= self.deadline {
            return Err(invalid("macOS preparation timed out"));
        }
        Ok(())
    }
    fn lock(&self, path: &Path) -> io::Result<ScopedLock> {
        let dir = Dir::open(path.parent().ok_or_else(|| invalid("lock parent"))?, true)?;
        let name = path.file_name().ok_or_else(|| invalid("lock name"))?;
        let file = match dir.file(name, true) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => dir.file(name, false)?,
            Err(e) => return Err(e),
        };
        loop {
            self.check()?;
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(ScopedLock::new_locked(file)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::park_timeout(Duration::from_millis(10))
                }
                Err(e) => return Err(e),
            }
        }
    }
    fn mutex<'a, T>(&self, lock: &'a Mutex<T>) -> io::Result<std::sync::MutexGuard<'a, T>> {
        loop {
            self.check()?;
            match lock.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::park_timeout(Duration::from_millis(10))
                }
                Err(e) => return Err(invalid(e)),
            }
        }
    }
}

/// Host-only cache layout. Construct this using the runner's existing mirror root.
pub struct Staging {
    state: PathBuf,
    trees: Trees,
    pub tools: Tools,
}
/// Only these paths may be passed to a VM share builder. Prep and canonical are absent.
pub struct Prepared {
    pub tree: PathBuf,
    pub workspace: PathBuf,
    pub siblings: Vec<(String, PathBuf)>,
    pub cargo_home: PathBuf,
}
impl Staging {
    pub fn new(state: PathBuf, mirrors: PathBuf, tools: Tools) -> io::Result<Self> {
        Dir::open(&state, true)?;
        Dir::open(&mirrors, false)?;
        Ok(Self {
            trees: Trees {
                mirrors,
                state: state.clone(),
                compacting: Arc::new(Mutex::new(BTreeSet::new())),
                dependency_locks: Mutex::new(BTreeMap::new()),
            },
            state,
            tools,
        })
    }
    fn path_lock(&self, logical: &str) -> PathBuf {
        self.state.join("canonical-locks").join(format!(
            "{:x}.lock",
            Sha256::digest(logical.to_ascii_lowercase().as_bytes())
        ))
    }
    /// Eviction uses the same lock as materialization and cloning; active jobs use independent copies.
    pub fn evict_canonical(&self, logical: &str, cancel: Cancellation) -> io::Result<()> {
        let path = self.state.join("canonical").join(mapped(logical)?);
        let control = Control {
            cancel,
            deadline: Instant::now() + PREP_TIMEOUT,
        };
        let _lock = control.lock(&self.path_lock(logical))?;
        match Dir::open(path.parent().unwrap(), false) {
            Ok(dir) => dir.remove(path.file_name().unwrap())?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }
    pub fn prepare(&self, request: &Run, cancel: Cancellation) -> io::Result<Prepared> {
        self.prepare_with_timeout(request, cancel, PREP_TIMEOUT)
    }
    /// Delete host-only caches without racing fetch or cargo-home publication.
    pub fn evict_prep(&self, cancel: Cancellation) -> io::Result<()> {
        let control = Control {
            cancel,
            deadline: Instant::now() + PREP_TIMEOUT,
        };
        let _lock = control.lock(&self.state.join("prep.lock"))?;
        let dir = Dir::open(&self.state, false)?;
        dir.remove("prep-cargo".as_ref())?;
        dir.remove("prep-bun".as_ref())
    }
    /// Call only after the VM is proven stopped, or after failed pre-admission prep.
    pub fn remove_job_trees(&self, id: uuid::Uuid) -> io::Result<()> {
        for tree in ["jobs", "prep"] {
            match Dir::open(&self.state.join(tree), false) {
                Ok(dir) => dir.remove(id.to_string().as_ref())?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn prepare_with_timeout(
        &self,
        request: &Run,
        cancel: Cancellation,
        timeout: Duration,
    ) -> io::Result<Prepared> {
        validate_paths(request)?;
        let control = Control {
            cancel,
            deadline: Instant::now() + timeout,
        };
        control.check()?;
        self.tools.verify(&control)?;
        let job = self.state.join("jobs").join(request.job_id.to_string());
        let prep = self.state.join("prep").join(request.job_id.to_string());
        // A job ID is single-use; never reuse a tree that may have been guest-written.
        for path in [&job, &prep] {
            let parent = Dir::open(path.parent().unwrap(), true)?;
            let name = path.file_name().unwrap();
            match parent.kind(name) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(invalid("job staging path already exists")),
            }
            parent.child(name, true)?;
        }
        let mut roots = vec![(
            request.workspace_key.clone(),
            request.repository_root.clone(),
            request.snapshot_commit.clone(),
            request
                .base_commit
                .clone()
                .unwrap_or_else(|| request.snapshot_commit.clone()),
            request.tree_hash.clone(),
        )];
        for s in &request.siblings {
            roots.push((
                s.canonical_path.clone(),
                s.canonical_path.clone(),
                s.commit.clone(),
                s.base_commit.clone().unwrap_or_else(|| s.commit.clone()),
                s.tree_hash.clone(),
            ));
        }
        for (logical, repository, commit, caller, tree_hash) in &roots {
            control.check()?;
            let _lock = control.lock(&self.path_lock(logical))?;
            let canonical = self.state.join("canonical").join(mapped(logical)?);
            let parent = Dir::open(canonical.parent().unwrap(), true)?;
            parent.remove(canonical.file_name().unwrap())?;
            parent.child(canonical.file_name().unwrap(), true)?;
            self.checkout(repository, &canonical, commit, caller, tree_hash, &control)?;
            let destination = job.join(mapped(logical)?);
            Dir::open(&canonical, false)?.clone_into(&Dir::open(&destination, true)?, &control)?;
        }
        let prep_roots: Vec<_> = roots
            .iter()
            .map(|(logical, ..)| Ok(prep.join(mapped(logical)?)))
            .collect::<io::Result<_>>()?;
        for ((logical, ..), dest) in roots.iter().zip(&prep_roots) {
            Dir::open(&job.join(mapped(logical)?), false)?
                .copy_inputs(&Dir::open(dest, true)?, &control)?;
        }
        let routes = preflight::validate(
            &prep_roots,
            &request.git_dependencies,
            &self.trees,
            &control,
        )?;
        let _lock = control.lock(&self.state.join("prep.lock"))?;
        let locks: Vec<_> = routes
            .values()
            .map(|path| self.trees.dependency_lock(path))
            .collect();
        let _guards: Vec<_> = locks
            .iter()
            .map(|lock| control.mutex(lock))
            .collect::<io::Result<_>>()?;
        for root in &prep_roots {
            if let Ok(lock) = fs::read(&root.join("Cargo.lock")) {
                self.trees.refresh_dependency_aliases(&lock, &routes)?;
            }
        }
        let cargo_home = self.state.join("prep-cargo");
        let bun_cache = self.state.join("prep-bun");
        let home = prep.join("home");
        for path in [&cargo_home, &bun_cache, &home] {
            Dir::open(path, true)?;
        }
        for root in &prep_roots {
            if Dir::open(root, false)?.contains("Cargo.toml".as_ref())? {
                let mut cmd = self.command(
                    &self.tools.cargo.path,
                    &prep,
                    &home,
                    &cargo_home,
                    &bun_cache,
                );
                cmd.args(["fetch", "--locked", "--manifest-path"])
                    .arg(root.join("Cargo.toml"))
                    .args(["--config", "net.git-fetch-with-cli=true"]);
                crate::workspace::dependencies::configure_fetch_git(&mut cmd, &home, &routes)?;
                // Only generated rewrites may use file transport, never a manifest source.
                cmd.env(
                    "GIT_ALLOW_PROTOCOL",
                    if routes.is_empty() {
                        "https"
                    } else {
                        "file:https"
                    },
                );
                process::run(&mut cmd, &prep, &control)?;
            }
            if Dir::open(root, false)?.contains("package.json".as_ref())? {
                let mut cmd =
                    self.command(&self.tools.bun.path, root, &home, &cargo_home, &bun_cache);
                cmd.args(["install", "--frozen-lockfile", "--ignore-scripts"]);
                crate::workspace::dependencies::configure_fetch_git(&mut cmd, &home, &routes)?;
                cmd.env(
                    "GIT_ALLOW_PROTOCOL",
                    if routes.is_empty() {
                        "https"
                    } else {
                        "file:https"
                    },
                );
                process::run(&mut cmd, &prep, &control)?;
            }
        }
        // Validate all installations before publishing any of them.
        for root in &prep_roots {
            preflight::validate_links(root, &prep_roots, &control)?;
        }
        for ((logical, ..), root) in roots.iter().zip(&prep_roots) {
            for installation in preflight::installations(root, &control)? {
                Dir::open(&installation, false)?.clone_into(
                    &Dir::open(
                        &job.join(mapped(logical)?)
                            .join(installation.strip_prefix(root).map_err(invalid)?),
                        true,
                    )?,
                    &control,
                )?;
            }
        }
        let job_cargo = job.join("cargo-home");
        Dir::open(&job_cargo, true)?;
        for relative in [
            "registry/index",
            "registry/cache",
            "git/db",
            "git/checkouts",
        ] {
            let path = cargo_home.join(relative);
            match Dir::open(&path, false) {
                Ok(dir) => {
                    dir.clone_into(&Dir::open(&job_cargo.join(relative), true)?, &control)?
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        control.check()?;
        Ok(Prepared {
            workspace: job.join(mapped(&request.workspace_key)?),
            siblings: request
                .siblings
                .iter()
                .map(|s| {
                    Ok((
                        s.canonical_path.clone(),
                        job.join(mapped(&s.canonical_path)?),
                    ))
                })
                .collect::<io::Result<_>>()?,
            cargo_home: job_cargo,
            tree: job,
        })
    }
    fn command(&self, tool: &Path, cwd: &Path, home: &Path, cargo: &Path, bun: &Path) -> Command {
        let mut cmd = Command::new(tool);
        cmd.env_clear()
            .current_dir(cwd)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("CARGO_HOME", cargo)
            .env("BUN_INSTALL_CACHE_DIR", bun)
            .env("RUSTC", &self.tools.rustc.path)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "")
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    self.tools.git.path.parent().unwrap().display()
                ),
            );
        cmd
    }
    fn checkout(
        &self,
        repository: &str,
        path: &Path,
        commit: &str,
        caller: &str,
        tree_hash: &str,
        control: &Control,
    ) -> io::Result<()> {
        for oid in [commit, caller] {
            if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(invalid("invalid snapshot commit"));
            }
        }
        let mirror = self.trees.mirror(repository);
        Dir::open(&mirror, false)?;
        let home = self.state.join("git-home");
        Dir::open(&home, true)?;
        let git = |cwd: &Path, args: &[&std::ffi::OsStr]| -> io::Result<String> {
            let mut cmd = Command::new(&self.tools.git.path);
            cmd.env_clear()
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_ALLOW_PROTOCOL", "file")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                ])
                .args(args)
                .current_dir(cwd);
            process::output(&mut cmd, &self.state, control)
        };
        git(
            path,
            &[
                "clone".as_ref(),
                "--shared".as_ref(),
                "--no-checkout".as_ref(),
                "--template=".as_ref(),
                mirror.as_os_str(),
                ".".as_ref(),
            ],
        )?;
        git(
            path,
            &[
                "checkout".as_ref(),
                "--force".as_ref(),
                "--detach".as_ref(),
                commit.as_ref(),
            ],
        )?;
        if !tree_hash.is_empty() {
            let computed = git(
                path,
                &["rev-parse".as_ref(), format!("{commit}^{{tree}}").as_ref()],
            )?;
            if computed.trim() != tree_hash {
                return Err(Trees::tree_hash_mismatch(
                    &mirror,
                    tree_hash,
                    computed.trim(),
                ));
            }
        }
        git(path, &["repack".as_ref(), "-a".as_ref(), "-d".as_ref()])?;
        fs::remove(&path.join(".git/objects/info/alternates"))?;
        git(
            path,
            &["remote".as_ref(), "remove".as_ref(), "origin".as_ref()],
        )?;
        if caller != commit {
            git(
                path,
                &["update-ref".as_ref(), "HEAD".as_ref(), caller.as_ref()],
            )?;
        }
        Ok(())
    }
}
