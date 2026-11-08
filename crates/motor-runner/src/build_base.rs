//! Compatibility inputs for format-1 build bases. Generated executables and
//! snapshot-selected toolchains are deliberately never run to identify a base.
use crate::{compile_cache, workspace};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io, path::Path, process::Command};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct Timestamp {
    pub seconds: i64,
    pub nanos: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FillLaunch {
    pub inputs: CompatibleInputs,
    pub fixed_path: std::path::PathBuf,
    pub reference: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub format: u32,
    pub commit: String,
    pub tree: String,
    #[serde(flatten)]
    pub inputs: CompatibleInputs,
    pub fixed_path: std::path::PathBuf,
    pub fill_start: Timestamp,
    pub newest_mtime: Timestamp,
}

impl Timestamp {
    pub(crate) fn now() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before Unix epoch");
        Self {
            seconds: now.as_secs() as i64,
            nanos: now.subsec_nanos() as i64,
        }
    }
}

pub(crate) fn repository_store(state: &Path, repository: &str) -> std::path::PathBuf {
    state.join("build-bases").join(workspace::name(repository))
}

pub(crate) fn staging(
    state: &Path,
    repository: &str,
    id: motor_protocol::Uuid,
) -> std::path::PathBuf {
    repository_store(state, repository).join(format!("staging-{id}"))
}

/// The generation's parent stays runner-owned and is never bound into a fill.
/// The writer owns the generation so the helper can inspect its entire tree.
pub(crate) fn prepare_target(
    config: &crate::Config,
    request: &motor_protocol::Run,
) -> io::Result<Option<std::path::PathBuf>> {
    if crate::caller_path::fixed_paths(config, request)
        .ok()
        .flatten()
        .is_none()
    {
        return Ok(None);
    }
    use std::os::unix::fs::DirBuilderExt;
    let state = config.state_dir();
    let store = repository_store(&state, &request.repository_root);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&store)?;
    let generation = staging(&state, &request.repository_root, request.job_id);
    fs::DirBuilder::new().mode(0o700).create(&generation)?;
    let target = generation.join("target");
    fs::create_dir(&target)?;
    if config.isolate_jobs {
        crate::isolation::own_build_base_staging(&generation)?;
    }
    Ok(Some(target))
}

pub(crate) fn remove_staging(
    state: &Path,
    repository: &str,
    id: motor_protocol::Uuid,
) -> io::Result<()> {
    crate::permissions::staging_operation("remove-staging", state, &workspace::name(repository), id)
}

pub(crate) fn remove_staging_key(
    state: &Path,
    key: &str,
    id: motor_protocol::Uuid,
) -> io::Result<()> {
    crate::permissions::staging_operation("remove-staging", state, key, id)
}

fn cstring(name: &std::ffi::OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes()).map_err(io::Error::other)
}

fn open_directory(
    parent: &std::os::fd::OwnedFd,
    name: &std::ffi::OsStr,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

fn stat_entry(parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> io::Result<libc::stat> {
    use std::os::fd::AsRawFd;
    let name = cstring(name)?;
    let mut stat = std::mem::MaybeUninit::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

/// Stat only until every tracked inode has passed the reference-time check.
/// Content is then read through no-follow descriptors and compared with trusted
/// mirror blobs, never through Git configuration in the writer-owned checkout.
pub(crate) fn verify_unchanged(
    trees: &workspace::Trees,
    request: &motor_protocol::Run,
    checkout: &Path,
    reference: Timestamp,
) -> io::Result<()> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    };
    let root = crate::permissions::physical_root(checkout)?;
    let metadata = fs::File::from(root.try_clone()?).metadata()?;
    if (Timestamp {
        seconds: metadata.ctime(),
        nanos: metadata.ctime_nsec(),
    }) > reference
    {
        return Err(io::Error::other(
            "tracked directory inode changed: workspace root",
        ));
    }
    let mirror = trees.mirror(&request.repository_root);
    let entries = workspace::snapshot_tree(&mirror, &request.tree_hash)?;
    let mut files = Vec::new();
    for entry in entries.split(|byte| *byte == 0).filter(|s| !s.is_empty()) {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| io::Error::other("invalid tracked tree entry"))?;
        let header = std::str::from_utf8(&entry[..tab]).map_err(io::Error::other)?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || !matches!(fields[0], "100644" | "100755" | "120000") {
            return Err(io::Error::other("unsupported tracked base entry"));
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(&entry[tab + 1..]));
        let mut parent = root.try_clone()?;
        let mut parts = path.components().peekable();
        while let Some(part) = parts.next() {
            let std::path::Component::Normal(name) = part else {
                return Err(io::Error::other("invalid tracked path"));
            };
            let stat = stat_entry(&parent, name)?;
            if (Timestamp {
                seconds: stat.st_ctime,
                nanos: stat.st_ctime_nsec,
            }) > reference
            {
                return Err(io::Error::other(format!(
                    "tracked inode changed: {}",
                    path.display()
                )));
            }
            if parts.peek().is_some() {
                parent = open_directory(&parent, name)?;
            } else {
                files.push((
                    path.to_owned(),
                    fields[0].to_owned(),
                    fields[2].to_owned(),
                    stat,
                ));
            }
        }
    }
    for (path, mode, oid, stat) in files {
        let mut parent = root.try_clone()?;
        for component in path.parent().unwrap().components() {
            let std::path::Component::Normal(name) = component else {
                return Err(io::Error::other("invalid tracked parent"));
            };
            parent = open_directory(&parent, name)?;
        }
        let name = path.file_name().unwrap();
        let current = stat_entry(&parent, name)?;
        if (
            current.st_dev,
            current.st_ino,
            current.st_ctime,
            current.st_ctime_nsec,
        ) != (stat.st_dev, stat.st_ino, stat.st_ctime, stat.st_ctime_nsec)
        {
            return Err(io::Error::other(
                "tracked inode changed during verification",
            ));
        }
        let name_c = cstring(name)?;
        let bytes = if mode == "120000" {
            if stat.st_mode & libc::S_IFMT != libc::S_IFLNK {
                return Err(io::Error::other("tracked symlink type changed"));
            }
            let mut bytes = vec![
                0;
                usize::try_from(stat.st_size)
                    .map_err(io::Error::other)?
                    .saturating_add(1)
            ];
            let len = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name_c.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if len < 0 {
                return Err(io::Error::last_os_error());
            }
            bytes.truncate(len as usize);
            bytes
        } else {
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG
                || (stat.st_mode & 0o100 != 0) != (mode == "100755")
            {
                return Err(io::Error::other("tracked file type or mode changed"));
            }
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut file = unsafe { fs::File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if (metadata.dev(), metadata.ino()) != (stat.st_dev, stat.st_ino) || !metadata.is_file()
            {
                return Err(io::Error::other(
                    "tracked inode replaced during verification",
                ));
            }
            let mut bytes = Vec::new();
            use std::io::Read;
            file.read_to_end(&mut bytes)?;
            bytes
        };
        if bytes != workspace::snapshot_blob(&mirror, &oid)? {
            return Err(io::Error::other(format!(
                "fill checkout no longer verifies against base tree: {}",
                name.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn newest_mtime(root: &std::os::fd::OwnedFd) -> io::Result<Timestamp> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let metadata = fs::File::from(root.try_clone()?).metadata()?;
    let mut newest = Timestamp {
        seconds: metadata.mtime(),
        nanos: metadata.mtime_nsec(),
    };
    let entries =
        |fd: &std::os::fd::OwnedFd| fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    let mut stack = vec![(root.try_clone()?, entries(root)?)];
    while let Some((directory, listing)) = stack.last_mut() {
        let Some(entry) = listing.next() else {
            stack.pop();
            continue;
        };
        let name = entry?.file_name();
        let stat = stat_entry(directory, &name)?;
        newest = newest.max(Timestamp {
            seconds: stat.st_mtime,
            nanos: stat.st_mtime_nsec,
        });
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_directory(directory, &name)?;
            let listing = entries(&child)?;
            stack.push((child, listing));
        }
    }
    Ok(newest)
}

pub(crate) fn publication_eligible(
    job: &crate::storage::Job,
    success: bool,
) -> io::Result<&FillLaunch> {
    if !job.base {
        return Err(io::Error::other("not a fill"));
    }
    if job.started_ms.is_none() {
        return Err(io::Error::other("fill never ran"));
    }
    if job.kill.is_some() {
        return Err(io::Error::other("fill cancelled or timed out"));
    }
    if !success {
        return Err(io::Error::other("fill did not exit successfully"));
    }
    job.base_launch
        .as_ref()
        .ok_or_else(|| io::Error::other("fill launch identity unavailable"))
}

/// The caller proves the fill unit stopped before entering publication. A
/// failed publication leaves `current` unchanged; remove its staging generation
/// only with remove-staging.
pub(crate) fn publish(
    state: &Path,
    trees: &workspace::Trees,
    checkout: &Path,
    job: &crate::storage::Job,
    success: bool,
) -> io::Result<()> {
    let launch = publication_eligible(job, success)?;
    let request = job
        .request
        .as_ref()
        .ok_or_else(|| io::Error::other("fill request missing"))?;
    let generation = staging(state, &request.repository_root, job.id);
    // No metadata repair, including an ACL restore, may precede this check.
    crate::permissions::staging_operation(
        "check-staging-links",
        state,
        &workspace::name(&request.repository_root),
        job.id,
    )?;
    verify_unchanged(trees, request, checkout, launch.reference)?;
    crate::isolation::reclaim_build_base(&generation)?;
    let root = crate::permissions::physical_root(&generation)?;
    let target = open_directory(&root, std::ffi::OsStr::new("target"))?;
    let bd = open_directory(&target, std::ffi::OsStr::new("bd")).map_err(|error| {
        io::Error::other(format!("fill left no non-empty bd directory: {error}"))
    })?;
    use std::os::fd::AsRawFd;
    if fs::read_dir(format!("/proc/self/fd/{}", bd.as_raw_fd()))?
        .next()
        .transpose()?
        .is_none()
    {
        return Err(io::Error::other("fill left an empty bd directory"));
    }
    let record = Record {
        format: 1,
        commit: request.snapshot_commit.clone(),
        tree: request.tree_hash.clone(),
        inputs: launch.inputs.clone(),
        fixed_path: launch.fixed_path.clone(),
        fill_start: launch.reference,
        newest_mtime: newest_mtime(&target)?,
    };
    publish_record(&generation, &record)
}

/// Called only after the link, checkout, ownership and build-output checks.
pub(crate) fn publish_record(generation: &Path, record: &Record) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let root = crate::permissions::physical_root(generation)?;
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            c"base.json".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&record)?)?;
    file.sync_all()?;
    fs::File::from(root.try_clone()?).sync_all()?;
    let parent = crate::permissions::physical_root(generation.parent().unwrap())?;
    let staging_name = cstring(generation.file_name().unwrap())?;
    let rename = |from: &std::ffi::CStr, to: &std::ffi::CStr, flags| {
        if unsafe {
            libc::renameat2(
                parent.as_raw_fd(),
                from.as_ptr(),
                parent.as_raw_fd(),
                to.as_ptr(),
                flags,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    match rename(&staging_name, c"current", libc::RENAME_NOREPLACE) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            rename(&staging_name, c"current", libc::RENAME_EXCHANGE)?;
            let retired = cstring(std::ffi::OsStr::new(&format!(
                "retired-{}",
                motor_protocol::Uuid::now_v7()
            )))?;
            // After exchange the staging name already holds a published,
            // runner-owned generation. Never route it to the staging helper.
            if let Err(error) = rename(&staging_name, &retired, libc::RENAME_NOREPLACE) {
                tracing::error!(%error, "published base exchanged; old generation retirement deferred");
            } else if let Err(error) = fs::remove_dir_all(
                generation
                    .parent()
                    .unwrap()
                    .join(retired.to_str().map_err(io::Error::other)?),
            ) {
                tracing::warn!(%error, "retired build base removal deferred to startup");
            }
        }
        Err(e) => return Err(e),
    }
    // Rename is the commit point. A later durability error must not be reported
    // as a refusal that claims the previous generation remained current.
    if let Err(error) = fs::File::from(parent).sync_all() {
        tracing::warn!(%error, "published build base directory sync failed");
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolchainIdentity {
    pub rustc_verbose: String,
    pub rust_toolchain: Option<String>,
    pub rust_toolchain_toml: Option<String>,
    pub wrapper: Option<WrapperIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum WrapperIdentity {
    Supervisor {
        source_sha256: String,
    },
    Sccache {
        script_sha256: String,
        backend_sha256: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CompatibleInputs {
    pub toolchain: ToolchainIdentity,
    pub environment: String,
}

/// Unavailable is not a compatible identity and cannot be persisted as one.
#[derive(Debug)]
pub(crate) enum Compatibility {
    Available(CompatibleInputs),
    Unavailable(String),
}

impl Compatibility {
    /// Seeding must skip reuse on Unavailable and log its reason once for the
    /// job. Do not substitute an empty identity or compare two errors as equal.
    pub(crate) fn for_seed(&self) -> Result<&CompatibleInputs, &str> {
        self.available()
    }

    /// Publication must refuse Unavailable and log its reason once for the
    /// fill. Persist only the launch's Available inputs, including on restart.
    pub(crate) fn for_publication(&self) -> Result<&CompatibleInputs, &str> {
        self.available()
    }

    /// The due check must not count an unavailable identity as a difference;
    /// missing bases and changed tips can still independently make a fill due.
    pub(crate) fn differs_for_due(&self, published: &CompatibleInputs) -> bool {
        matches!(self, Self::Available(current) if current != published)
    }

    fn available(&self) -> Result<&CompatibleInputs, &str> {
        match self {
            Self::Available(inputs) => Ok(inputs),
            Self::Unavailable(reason) => Err(reason),
        }
    }
}

/// Probe once in the runner's own environment, never in a job's checkout. Keep
/// failure as data so an unreadable compiler disables reuse rather than jobs.
pub(crate) struct RunnerToolchain(Result<String, String>);

impl RunnerToolchain {
    pub(crate) fn probe() -> Self {
        Self(Self::read())
    }

    fn read() -> Result<String, String> {
        let output = Command::new("rustc")
            .arg("-vV")
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!("runner rustc -vV failed: {}", output.status));
        }
        let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        if text.is_empty() {
            return Err("runner rustc -vV returned no identity".into());
        }
        Ok(text)
    }
}

/// All inputs exist before wrapper compilation. `env` must come from
/// backend::job_environment with the unit's selected wrapper and transport.
/// `sccache_backend` is the shared Cargo home's bin/sccache, not the generated
/// sandbox script or a command resolved in the job's PATH.
pub(crate) fn compatibility(
    runner: &RunnerToolchain,
    repository: &Path,
    tree: &str,
    env: &BTreeMap<String, String>,
    sccache_backend: &Path,
) -> Compatibility {
    let collect = || -> io::Result<CompatibleInputs> {
        let rustc_verbose = runner
            .0
            .as_ref()
            .map_err(|e| io::Error::other(e.clone()))?
            .clone();
        let entries = workspace::git(
            repository,
            &[
                "ls-tree",
                "--full-tree",
                "-z",
                tree,
                "--",
                "rust-toolchain",
                "rust-toolchain.toml",
            ],
        )?;
        let mut blobs = BTreeMap::new();
        for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
            let (metadata, name) = entry
                .split_once('\t')
                .ok_or_else(|| io::Error::other("invalid toolchain tree entry"))?;
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() != 3 || fields[1] != "blob" {
                return Err(io::Error::other(format!(
                    "toolchain input is not a blob: {name}"
                )));
            }
            blobs.insert(name, fields[2].to_owned());
        }
        let wrapper = match env.get("RUSTC_WRAPPER").map(String::as_str) {
            Some("/motor-tools/prefrontal-build-slot") => Some(WrapperIdentity::Supervisor {
                source_sha256: supervisor_digest(),
            }),
            Some("sccache" | "/motor-tools/sccache") => Some(WrapperIdentity::Sccache {
                script_sha256: format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("sccache-wrapper.sh"))
                ),
                backend_sha256: format!("{:x}", Sha256::digest(fs::read(sccache_backend)?)),
            }),
            _ => None,
        };
        Ok(CompatibleInputs {
            toolchain: ToolchainIdentity {
                rustc_verbose,
                rust_toolchain: blobs.remove("rust-toolchain"),
                rust_toolchain_toml: blobs.remove("rust-toolchain.toml"),
                wrapper,
            },
            environment: compile_cache::environment_fingerprint(env),
        })
    };
    match collect() {
        Ok(inputs) => Compatibility::Available(inputs),
        Err(error) => Compatibility::Unavailable(error.to_string()),
    }
}

pub(crate) fn supervisor_digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(include_bytes!("../../prefrontal-build-slot/src/main.rs"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    struct Fixture {
        scratch: ScratchDir,
        runner: RunnerToolchain,
        tree: String,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = ScratchDir::new("build-base-identity");
            workspace::git(&scratch, &["init", "-q"]).unwrap();
            fs::write(scratch.join("Cargo.toml"), "[workspace]\n").unwrap();
            workspace::git(&scratch, &["add", "Cargo.toml"]).unwrap();
            let tree = workspace::git(&scratch, &["write-tree"]).unwrap();
            Self {
                scratch,
                runner: RunnerToolchain(Ok(
                    "rustc test\nbinary: rustc\nhost: test-host\nrelease: test\n".into(),
                )),
                tree,
            }
        }

        fn identity(&self, wrapper: Option<&str>) -> Compatibility {
            let env = wrapper
                .map(|value| BTreeMap::from([("RUSTC_WRAPPER".into(), value.into())]))
                .unwrap_or_default();
            compatibility(
                &self.runner,
                &self.scratch,
                self.tree.trim(),
                &env,
                &self.scratch.join("sccache-backend"),
            )
        }

        fn stage(&mut self, path: &str, content: &str) {
            fs::write(self.scratch.join(path), content).unwrap();
            workspace::git(&self.scratch, &["add", path]).unwrap();
            self.tree = workspace::git(&self.scratch, &["write-tree"]).unwrap();
        }
    }

    #[test]
    fn identity_records_snapshot_root_blobs_absence_and_runner_verbose_version() {
        let mut f = Fixture::new();
        let original = f.identity(None).for_seed().unwrap().clone();
        assert_eq!(original.toolchain.rust_toolchain, None);
        assert_eq!(original.toolchain.rust_toolchain_toml, None);
        assert_eq!(
            original.toolchain.rustc_verbose,
            f.runner.0.as_ref().unwrap().as_str()
        );
        // Invalid toolchain text is only data: it must never be invoked.
        f.stage("rust-toolchain", "not-an-installed-toolchain\n");
        f.stage("rust-toolchain.toml", "also not a toolchain\n");
        let identity = f.identity(None).for_seed().unwrap().clone();
        for (name, actual) in [
            ("rust-toolchain", &identity.toolchain.rust_toolchain),
            (
                "rust-toolchain.toml",
                &identity.toolchain.rust_toolchain_toml,
            ),
        ] {
            let expected = workspace::git(
                &f.scratch,
                &["rev-parse", &format!("{}:{name}", f.tree.trim())],
            )
            .unwrap();
            assert_eq!(actual.as_deref(), Some(expected.trim()));
        }
        assert_ne!(identity, original);
        fs::write(f.scratch.join("rust-toolchain"), "unstaged edit").unwrap();
        assert_eq!(f.identity(None).for_seed().unwrap(), &identity);
        f.runner = RunnerToolchain(Ok("rustc same release\nhost: other-host\n".into()));
        assert!(f.identity(None).differs_for_due(&identity));
    }

    #[test]
    fn identity_is_stable_before_supervisor_build_and_after_rebuild() {
        let f = Fixture::new();
        let before = f
            .identity(Some("/motor-tools/prefrontal-build-slot"))
            .for_seed()
            .unwrap()
            .clone();
        assert_eq!(
            before.toolchain.wrapper,
            Some(WrapperIdentity::Supervisor {
                source_sha256: supervisor_digest()
            })
        );
        let source = f.scratch.join("supervisor.rs");
        let output = f.scratch.join("supervisor");
        fs::write(
            &source,
            include_str!("../../prefrontal-build-slot/src/main.rs"),
        )
        .unwrap();
        for _ in 0..2 {
            crate::platform::build_wrapper(&source, &output, &f.scratch.join("slots"), 2).unwrap();
            // A fill must record the same pre-setup source identity as a reader.
            assert_eq!(
                f.identity(Some("/motor-tools/prefrontal-build-slot"))
                    .for_publication()
                    .unwrap(),
                &before
            );
            fs::remove_file(&output).unwrap();
        }
    }

    #[test]
    fn sccache_identity_hashes_embedded_script_and_backend_not_generated_tools() {
        let f = Fixture::new();
        fs::write(f.scratch.join("sccache-backend"), b"backend one").unwrap();
        let before = f
            .identity(Some("/motor-tools/sccache"))
            .for_seed()
            .unwrap()
            .clone();
        assert_eq!(
            before.toolchain.wrapper,
            Some(WrapperIdentity::Sccache {
                script_sha256: format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("sccache-wrapper.sh"))
                ),
                backend_sha256: format!("{:x}", Sha256::digest(b"backend one")),
            })
        );
        fs::write(f.scratch.join("sccache"), "generated wrapper copy").unwrap();
        assert_eq!(
            f.identity(Some("/motor-tools/sccache"))
                .for_publication()
                .unwrap(),
            &before
        );
        // The build-server spelling resolves to the same embedded wrapper.
        assert_eq!(
            f.identity(Some("sccache")).for_seed().unwrap().toolchain,
            before.toolchain
        );
        fs::write(f.scratch.join("sccache-backend"), b"backend two").unwrap();
        assert!(f
            .identity(Some("/motor-tools/sccache"))
            .differs_for_due(&before));
    }

    #[test]
    fn unreadable_inputs_are_unavailable_not_compatible_or_due_differences() {
        let mut f = Fixture::new();
        let published = f.identity(None).for_seed().unwrap().clone();
        let assert_unavailable = |result: Compatibility| {
            assert!(matches!(result, Compatibility::Unavailable(_)));
            assert!(!result.for_seed().unwrap_err().is_empty());
            assert!(!result.for_publication().unwrap_err().is_empty());
            assert!(!result.differs_for_due(&published));
        };
        assert_unavailable(f.identity(Some("/motor-tools/sccache")));
        fs::create_dir(f.scratch.join("sccache-backend")).unwrap();
        assert_unavailable(f.identity(Some("sccache")));
        f.tree = "missing-tree".into();
        assert_unavailable(f.identity(None));
        f.tree = workspace::git(&f.scratch, &["write-tree"]).unwrap();
        f.runner = RunnerToolchain(Err("runner rustc unreadable".into()));
        assert_unavailable(f.identity(None));
    }

    #[test]
    fn runner_probe_uses_verbose_host_compiler_output() {
        let runner = RunnerToolchain::probe();
        let text = runner.0.unwrap();
        assert!(text.contains("\nbinary: rustc\n"));
        assert!(text.contains("\nhost: "));
        assert!(text.contains("\nrelease: "));
    }
}
