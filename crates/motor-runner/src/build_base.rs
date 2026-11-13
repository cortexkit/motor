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

impl Record {
    pub(crate) fn times_valid(&self) -> bool {
        self.fill_start <= self.newest_mtime && self.newest_mtime <= Timestamp::now()
    }
}

pub(crate) const REFILL_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;

/// Match checkout file-presence checks using only immutable tree entries.
/// Resolving a link could depend on files outside the snapshot, so a link at
/// any path component makes compatibility unavailable.
pub(crate) fn snapshot_file(repository: &Path, tree: &str, path: &str) -> io::Result<bool> {
    let components: Vec<_> = path.split('/').collect();
    let mut prefix = String::new();
    for (index, component) in components.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let entry = workspace::git(repository, &["ls-tree", "-z", tree, "--", &prefix])?;
        if entry.is_empty() {
            return Ok(false);
        }
        let mode = entry
            .split_whitespace()
            .next()
            .ok_or_else(|| io::Error::other(format!("invalid presence entry: {prefix}")))?;
        if mode == "120000" {
            return Err(io::Error::other(format!(
                "symlink in build-base presence path: {prefix}"
            )));
        }
        if index + 1 == components.len() {
            return Ok(matches!(mode, "100644" | "100755"));
        }
        if mode != "040000" {
            return Ok(false);
        }
    }
    Ok(false)
}

/// Read only the published record: an absent or unreadable generation needs a
/// replacement, but unavailable current identity is not itself a difference.
pub(crate) fn needs_fill(
    state: &Path,
    repository: &str,
    tip: &str,
    fixed_path: &Path,
    current: &Compatibility,
) -> bool {
    let record = fs::read(repository_store(state, repository).join("current/base.json"))
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).map_err(io::Error::other));
    match record {
        Ok(record) => {
            record.format != 1
                || record.commit != tip
                || record.fixed_path != fixed_path
                || current.differs_for_due(&record.inputs)
                || !record.times_valid()
        }
        Err(_) => true,
    }
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

    pub(crate) fn reference() -> io::Result<Self> {
        let coarse = || {
            let mut value = std::mem::MaybeUninit::uninit();
            if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, value.as_mut_ptr()) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let value = unsafe { value.assume_init() };
            Ok(Self {
                seconds: value.tv_sec,
                nanos: value.tv_nsec,
            })
        };
        let reference = coarse()?;
        // Linux inode ctime uses the coarse realtime tick. Starting within that
        // same tick would let a write/restore receive ctime <= the reference.
        // Advance one tick before launching so every later mutation is newer.
        while coarse()? <= reference {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(reference)
    }
}

pub(crate) fn repository_store(state: &Path, repository: &str) -> std::path::PathBuf {
    state.join("build-bases").join(workspace::name(repository))
}

#[derive(Default)]
struct Leases {
    readers: BTreeMap<(u64, u64), usize>,
    retired: BTreeMap<(u64, u64), std::path::PathBuf>,
    usage_warned: std::collections::BTreeSet<std::path::PathBuf>,
}

fn leases() -> &'static std::sync::Mutex<Leases> {
    static LEASES: std::sync::OnceLock<std::sync::Mutex<Leases>> = std::sync::OnceLock::new();
    LEASES.get_or_init(Default::default)
}

/// Registers the open directory's device and inode to delay retired-generation
/// removal, even while publication exchanges its name. Keep this reader alive
/// until cloning has finished.
pub(crate) struct PinnedBase {
    pub directory: fs::File,
    identity: (u64, u64),
}

pub(crate) fn pin_current(store: &Path) -> io::Result<PinnedBase> {
    use std::os::unix::fs::MetadataExt;
    let mut leases = leases().lock().unwrap();
    let parent = crate::permissions::physical_root(store)?;
    let directory = fs::File::from(open_directory(&parent, std::ffi::OsStr::new("current"))?);
    let stat = directory.metadata()?;
    let identity = (stat.dev(), stat.ino());
    *leases.readers.entry(identity).or_default() += 1;
    if let Err(error) = read_usage(store, identity) {
        warn_usage(store, &error, &mut leases);
    }
    touch_usage(store, identity, &mut leases);
    Ok(PinnedBase {
        directory,
        identity,
    })
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<(u64, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_ms: Option<u64>,
}

fn timestamp_ms(time: Timestamp) -> u64 {
    (time.seconds.max(0) as u64)
        .saturating_mul(1000)
        .saturating_add(time.nanos.max(0) as u64 / 1_000_000)
}

fn usage_directory(store: &Path) -> io::Result<std::os::fd::OwnedFd> {
    let parent = crate::permissions::physical_root(store.parent().unwrap())?;
    open_directory(&parent, store.file_name().unwrap())
}

fn usage_file(
    directory: &std::os::fd::OwnedFd,
    name: &std::ffi::CStr,
    create: bool,
) -> io::Result<fs::File> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    };
    let flags = if create {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    } else {
        libc::O_RDONLY
    };
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    let stat = file.metadata()?;
    if !stat.is_file()
        || stat.uid() != unsafe { libc::geteuid() }
        || stat.mode() & 0o777 != 0o600
        || stat.nlink() != 1
    {
        return Err(io::Error::other(
            "unsafe build base usage file owner or mode",
        ));
    }
    Ok(file)
}

fn read_usage(store: &Path, identity: (u64, u64)) -> io::Result<u64> {
    let directory = usage_directory(store)?;
    let usage: Usage = serde_json::from_reader(usage_file(&directory, c"usage.json", false)?)?;
    if usage.generation != Some(identity) {
        return Err(io::Error::other("build base usage generation differs"));
    }
    usage
        .last_used_ms
        .ok_or_else(|| io::Error::other("build base last use unknown"))
}

fn write_usage(store: &Path, identity: (u64, u64), last_used_ms: u64) -> io::Result<()> {
    use std::{io::Write, os::fd::AsRawFd};
    let directory = usage_directory(store)?;
    let temporary = cstring(std::ffi::OsStr::new(&format!(
        ".usage-{}",
        motor_protocol::Uuid::now_v7()
    )))?;
    let result = (|| {
        let mut file = usage_file(&directory, &temporary, true)?;
        file.write_all(&serde_json::to_vec(&Usage {
            generation: Some(identity),
            last_used_ms: Some(last_used_ms),
        })?)?;
        file.sync_all()?;
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                c"usage.json".as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        fs::File::from(directory.try_clone()?).sync_all()
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

fn warn_usage(store: &Path, error: &io::Error, leases: &mut Leases) {
    if leases.usage_warned.insert(store.into()) {
        tracing::warn!(repository = %store.display(), %error, "build base last use unavailable; using recorded fill start");
    }
}

fn touch_usage(store: &Path, identity: (u64, u64), leases: &mut Leases) {
    if let Err(error) = write_usage(store, identity, timestamp_ms(Timestamp::now())) {
        warn_usage(store, &error, leases);
    }
}

pub(crate) struct EvictionCandidate {
    pub store: std::path::PathBuf,
    pub last_used_ms: u64,
    identity: (u64, u64),
}

/// Only published generations are candidates. Staging generations are removed
/// after their fill stops; retired generations are removed after their last lease.
pub(crate) fn eviction_candidates(state: &Path) -> io::Result<Vec<EvictionCandidate>> {
    use std::os::unix::fs::MetadataExt;
    let root = state.join("build-bases");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut leases = leases().lock().unwrap();
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let store = entry.path();
        let parent = crate::permissions::physical_root(&store)?;
        let directory = match open_directory(&parent, std::ffi::OsStr::new("current")) {
            Ok(directory) => fs::File::from(directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let stat = directory.metadata()?;
        let identity = (stat.dev(), stat.ino());
        if leases.readers.contains_key(&identity) {
            continue;
        }
        let last_used_ms = read_usage(&store, identity).unwrap_or_else(|error| {
            warn_usage(&store, &error, &mut leases);
            // base.json is runner-authored and immutable, unlike filesystem times.
            let record: io::Result<Record> = (|| {
                use std::os::fd::AsFd;
                let directory = directory.as_fd().try_clone_to_owned()?;
                Ok(serde_json::from_reader(usage_record(&directory)?)?)
            })();
            record.map_or(0, |record| timestamp_ms(record.fill_start))
        });
        candidates.push(EvictionCandidate {
            store,
            last_used_ms,
            identity,
        });
    }
    candidates.sort_by(|a, b| {
        a.last_used_ms
            .cmp(&b.last_used_ms)
            .then_with(|| a.store.cmp(&b.store))
    });
    Ok(candidates)
}

fn usage_record(directory: &std::os::fd::OwnedFd) -> io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"base.json".as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("base record is not a file"));
    }
    Ok(file)
}

/// Claim the exact generation selected by the pressure pass while excluding
/// seed pins and publication. A seed can no longer open it after retirement.
pub(crate) fn claim_eviction(
    candidate: &EvictionCandidate,
) -> io::Result<Option<std::path::PathBuf>> {
    use std::os::unix::fs::MetadataExt;
    let leases = leases().lock().unwrap();
    let current = candidate.store.join("current");
    let stat = match fs::symlink_metadata(&current) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !stat.is_dir()
        || (stat.dev(), stat.ino()) != candidate.identity
        || leases.readers.contains_key(&candidate.identity)
    {
        return Ok(None);
    }
    // A pin may have completed between selection and claiming. Do not evict
    // its newly warm base using the old LRU position.
    if read_usage(&candidate.store, candidate.identity)
        .is_ok_and(|used| used != candidate.last_used_ms)
    {
        return Ok(None);
    }
    let retired = candidate
        .store
        .join(format!("retired-{}", motor_protocol::Uuid::now_v7()));
    fs::rename(&current, &retired)?;
    Ok(Some(retired))
}

/// Published generations are runner-owned and can be removed directly once no
/// seed holds their inode open.
pub(crate) fn remove_retired(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut leases = leases().lock().unwrap();
    let stat = fs::symlink_metadata(path)?;
    let identity = (stat.dev(), stat.ino());
    if leases.readers.contains_key(&identity) {
        leases.retired.insert(identity, path.into());
        Ok(())
    } else {
        fs::remove_dir_all(path)
    }
}

impl Drop for PinnedBase {
    fn drop(&mut self) {
        let mut leases = leases().lock().unwrap();
        let count = leases.readers.get_mut(&self.identity).unwrap();
        *count -= 1;
        if *count == 0 {
            leases.readers.remove(&self.identity);
            if let Some(path) = leases.retired.remove(&self.identity) {
                if let Err(error) = fs::remove_dir_all(path) {
                    tracing::warn!(%error, "retired build base removal deferred to startup");
                }
            }
        }
    }
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
    let repository = crate::caller_path::request(request)
        .caller_repository_root
        .text();
    let store = repository_store(&state, repository);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&store)?;
    let generation = staging(&state, repository, request.job_id);
    fs::DirBuilder::new().mode(0o700).create(&generation)?;
    let target = generation.join("target");
    fs::create_dir(&target)?;
    if config.isolate_jobs {
        crate::isolation::own_build_base_staging(&generation)?;
    }
    Ok(Some(target))
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
    let mirror = trees.mirror(
        crate::caller_path::request(request)
            .caller_repository_root
            .text(),
    );
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
        let expected = workspace::snapshot_blob(&mirror, &oid)?;
        let matches = if mode == "120000" {
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
            bytes == expected
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
            contents_match(&mut file, &expected)?
        };
        if !matches {
            return Err(io::Error::other(format!(
                "fill checkout no longer verifies against base tree: {}",
                name.to_string_lossy()
            )));
        }
    }
    Ok(())
}

fn contents_match(reader: &mut impl io::Read, expected: &[u8]) -> io::Result<bool> {
    // Bound reads of writer-owned files, including an oversized replacement.
    let mut buffer = [0; 8192];
    for chunk in expected.chunks(buffer.len()) {
        match reader.read_exact(&mut buffer[..chunk.len()]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(error) => return Err(error),
        }
        if &buffer[..chunk.len()] != chunk {
            return Ok(false);
        }
    }
    Ok(reader.read(&mut buffer[..1])? == 0)
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
    publish_after_link_check(state, trees, checkout, job, success, |repository| {
        crate::permissions::staging_operation(
            "check-staging-links",
            state,
            &workspace::name(repository),
            job.id,
        )
    })
}

#[cfg(test)]
pub(crate) fn publish_with_authority(
    state: &Path,
    trees: &workspace::Trees,
    checkout: &Path,
    job: &crate::storage::Job,
    success: bool,
    authority: &crate::source_seal::authority::Authority,
) -> io::Result<()> {
    publish_after_link_check(state, trees, checkout, job, success, |repository| {
        crate::permissions::staging_authorized(
            authority,
            "check-staging-links",
            state,
            &workspace::name(repository),
            job.id,
        )
    })
}

fn publish_after_link_check(
    state: &Path,
    trees: &workspace::Trees,
    checkout: &Path,
    job: &crate::storage::Job,
    success: bool,
    check_links: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<()> {
    let started = std::time::Instant::now();
    let launch = publication_eligible(job, success)?;
    let request = job
        .request
        .as_ref()
        .ok_or_else(|| io::Error::other("fill request missing"))?;
    let repository = crate::caller_path::request(request)
        .caller_repository_root
        .text();
    let generation = staging(state, repository, job.id);
    // No metadata repair, including an ACL restore, may precede this check.
    check_links(repository)?;
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
    publish_record(&generation, &record)?;
    tracing::info!(job_id = %job.id, elapsed_ms = started.elapsed().as_millis() as u64, tip = %record.commit, repository = %repository, "build base published");
    Ok(())
}

/// Called only after the link, checkout, ownership and build-output checks.
pub(crate) fn publish_record(generation: &Path, record: &Record) -> io::Result<()> {
    publish_record_synced(
        generation,
        record,
        #[cfg(test)]
        sync_generation,
    )
}

fn sync_generation(root: &std::os::fd::OwnedFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::syncfs(root.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn publish_record_synced(
    generation: &Path,
    record: &Record,
    #[cfg(test)] sync: impl FnOnce(&std::os::fd::OwnedFd) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;
    let mut leases = leases().lock().unwrap();
    let mut retired_path = None;
    let store = generation.parent().unwrap();
    let ancestor = crate::permissions::physical_root(store.parent().unwrap())?;
    let parent = open_directory(&ancestor, store.file_name().unwrap())?;
    // physical_root returns O_PATH; fsync needs an ordinary directory handle.
    // Open both directories through their entries, without following links.
    let root = open_directory(&parent, generation.file_name().unwrap())?;
    // The writer has stopped. Persist its artifacts before making a record that
    // can certify this generation after a crash; Cargo does not fsync outputs.
    #[cfg(test)]
    sync(&root)?;
    #[cfg(not(test))]
    sync_generation(&root)?;
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
    let staging_name = cstring(generation.file_name().unwrap())?;
    // Call the syscall directly: the musl libc crate has no renameat2 wrapper,
    // and the static musl runner is built from this same code.
    let rename = |from: &std::ffi::CStr, to: &std::ffi::CStr, flags: libc::c_uint| {
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
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
            } else {
                retired_path = Some(store.join(retired.to_str().map_err(io::Error::other)?));
            }
        }
        Err(e) => return Err(e),
    }
    // Rename is the commit point. A later durability error must not be reported
    // as a refusal that claims the previous generation remained current.
    if let Err(error) = fs::File::from(parent).sync_all() {
        tracing::warn!(%error, "published build base directory sync failed");
    }
    if let Ok(stat) = fs::symlink_metadata(store.join("current")) {
        touch_usage(store, (stat.dev(), stat.ino()), &mut leases);
    }
    drop(leases);
    if let Some(retired) = retired_path {
        if let Err(error) = remove_retired(&retired) {
            tracing::warn!(%error, "retired build base removal deferred to startup");
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolchainIdentity {
    // The runner's verbose compiler output when no selector exists; otherwise
    // the snapshot's exact release. Floating selectors cannot identify a base.
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
        static IDENTITY: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
        Self(IDENTITY.get_or_init(Self::read).clone())
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

fn selected_release(repository: &Path, blob: &str, is_toml: bool) -> io::Result<String> {
    let invalid = || io::Error::other("build-base toolchain selector must name an exact release");
    let size = workspace::git(repository, &["cat-file", "-s", blob])?
        .trim()
        .parse::<usize>()
        .map_err(|_| invalid())?;
    if size > 16 * 1024 {
        return Err(invalid());
    }
    let bytes = workspace::git_bytes(repository, &["cat-file", "blob", blob])?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
    let release = if is_toml {
        let value: toml::Value = toml::from_str(text).map_err(|_| invalid())?;
        let root = value.as_table().ok_or_else(invalid)?;
        let toolchain = root
            .get("toolchain")
            .and_then(toml::Value::as_table)
            .ok_or_else(invalid)?;
        if root.len() != 1
            || toolchain.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "channel" | "components" | "targets" | "profile"
                )
            })
        {
            return Err(invalid());
        }
        for key in ["components", "targets"] {
            if toolchain.get(key).is_some_and(|value| {
                !value
                    .as_array()
                    .is_some_and(|items| items.iter().all(toml::Value::is_str))
            }) {
                return Err(invalid());
            }
        }
        if toolchain.get("profile").is_some_and(|value| {
            !matches!(value.as_str(), Some("minimal" | "default" | "complete"))
        }) {
            return Err(invalid());
        }
        toolchain
            .get("channel")
            .and_then(toml::Value::as_str)
            .ok_or_else(invalid)?
            .to_owned()
    } else {
        text.trim().to_owned()
    };
    let parts: Vec<_> = release.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return Err(invalid());
    }
    Ok(release)
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
        snapshot_file(repository, tree, "Cargo.toml")?;
        snapshot_file(repository, tree, "crates/prefrontal-build-slot/src/main.rs")?;
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
            if fields.len() != 3 || fields[1] != "blob" || !matches!(fields[0], "100644" | "100755")
            {
                return Err(io::Error::other(format!(
                    "toolchain input is not a blob: {name}"
                )));
            }
            blobs.insert(name, fields[2].to_owned());
        }
        let rustc_verbose = match blobs.len() {
            0 => runner
                .0
                .as_ref()
                .map_err(|e| io::Error::other(e.clone()))?
                .clone(),
            1 => {
                let (name, blob) = blobs.first_key_value().unwrap();
                selected_release(repository, blob, *name == "rust-toolchain.toml")?
            }
            _ => return Err(io::Error::other("multiple build-base toolchain selectors")),
        };
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
    fn publication_barrier_precedes_record_and_failure_keeps_current() {
        use std::{cell::Cell, os::unix::fs::MetadataExt};
        let f = Fixture::new();
        let store = repository_store(&f.scratch, "/repo");
        let current = store.join("current");
        fs::create_dir_all(&current).unwrap();
        fs::write(current.join("base.json"), b"old record").unwrap();
        fs::write(current.join("artifact"), b"old artifact").unwrap();
        let old_inode = fs::metadata(&current).unwrap().ino();
        let generation = staging(&f.scratch, "/repo", motor_protocol::Uuid::now_v7());
        fs::create_dir_all(generation.join("target/bd")).unwrap();
        fs::write(generation.join("target/bd/artifact"), b"new artifact").unwrap();
        let record = Record {
            format: 1,
            commit: "new".into(),
            tree: f.tree.trim().into(),
            inputs: f.identity(None).for_seed().unwrap().clone(),
            fixed_path: "/w/account/key/repo".into(),
            fill_start: Timestamp {
                seconds: 1,
                nanos: 0,
            },
            newest_mtime: Timestamp {
                seconds: 2,
                nanos: 0,
            },
        };
        let called = Cell::new(false);
        let error = publish_record_synced(&generation, &record, |fd| {
            called.set(true);
            assert_eq!(
                fs::File::from(fd.try_clone()?).metadata()?.ino(),
                fs::metadata(&generation)?.ino()
            );
            assert!(!generation.join("base.json").exists());
            Err(io::Error::from_raw_os_error(libc::EIO))
        })
        .unwrap_err();
        assert!(called.get());
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(!generation.join("base.json").exists());
        assert_eq!(fs::metadata(&current).unwrap().ino(), old_inode);
        assert_eq!(fs::read(current.join("base.json")).unwrap(), b"old record");
        assert_eq!(fs::read(current.join("artifact")).unwrap(), b"old artifact");
        publish_record_synced(&generation, &record, |fd| {
            assert!(!generation.join("base.json").exists());
            sync_generation(fd)
        })
        .unwrap();
        assert_eq!(
            fs::read(current.join("target/bd/artifact")).unwrap(),
            b"new artifact"
        );
    }

    #[test]
    fn source_comparison_bounds_reads_and_stops_at_expected_length() {
        struct Oversized {
            read: usize,
        }
        impl io::Read for Oversized {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert!(buffer.len() <= 8192, "unbounded read");
                self.read += buffer.len();
                assert!(self.read <= 20_001, "read beyond trusted blob length");
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }
        let expected = vec![b'x'; 20_000];
        let mut oversized = Oversized { read: 0 };
        assert!(!contents_match(&mut oversized, &expected).unwrap());
        assert_eq!(oversized.read, 20_001);
        assert!(contents_match(&mut expected.as_slice(), &expected).unwrap());
        assert!(!contents_match(&mut &expected[..19_999], &expected).unwrap());
        let mut different = expected.clone();
        different[16_384] = b'y';
        assert!(!contents_match(&mut different.as_slice(), &expected).unwrap());
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
        f.stage(
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.100.0\"\n",
        );
        let identity = f.identity(None).for_seed().unwrap().clone();
        let expected = workspace::git(
            &f.scratch,
            &[
                "rev-parse",
                &format!("{}:rust-toolchain.toml", f.tree.trim()),
            ],
        )
        .unwrap();
        assert_eq!(identity.toolchain.rust_toolchain, None);
        assert_eq!(
            identity.toolchain.rust_toolchain_toml.as_deref(),
            Some(expected.trim())
        );
        assert_eq!(identity.toolchain.rustc_verbose, "1.100.0");
        assert_ne!(identity, original);
        fs::write(f.scratch.join("rust-toolchain.toml"), "unstaged edit").unwrap();
        assert_eq!(f.identity(None).for_seed().unwrap(), &identity);
        f.runner = RunnerToolchain(Ok("rustc same release\nhost: other-host\n".into()));
        assert_eq!(f.identity(None).for_seed().unwrap(), &identity);
    }

    #[test]
    fn exact_selector_release_identifies_compiler_independently_of_runner_default() {
        for (name, text) in [
            ("rust-toolchain", "1.100.0\n"),
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.100.0\"\ncomponents = [\"clippy\"]\nprofile = \"minimal\"\n"),
        ] {
            let mut f = Fixture::new();
            let default = f.identity(None).for_seed().unwrap().clone();
            f.stage(name, text);
            let pinned = f.identity(None).for_seed().unwrap().clone();
            assert_eq!(pinned.toolchain.rustc_verbose, "1.100.0");
            assert_ne!(pinned, default);
            assert_eq!(f.identity(None).for_publication().unwrap(), &pinned);
            f.stage(name, if name.ends_with("toml") { "[toolchain]\nchannel = \"1.99.1\"\n" } else { "1.99.1\n" });
            assert_eq!(f.identity(None).for_seed().unwrap().toolchain.rustc_verbose, "1.99.1");
            assert!(f.identity(None).differs_for_due(&pinned));
        }
    }

    #[test]
    fn floating_ambiguous_and_unreadable_selectors_disable_reuse_and_publication() {
        for (name, text, both) in [
            ("rust-toolchain", "stable\n", false),
            ("rust-toolchain", "1.100\n", false),
            ("rust-toolchain", "nightly-2026-11-01\n", false),
            ("rust-toolchain", "custom\n", false),
            ("rust-toolchain", "1.100.0\nextra\n", false),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"stable\"\n",
                false,
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"1.100\"\n",
                false,
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"beta\"\n",
                false,
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"nightly-2026-11-01\"\n",
                false,
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"1.100.0\"\npath = \"/custom\"\n",
                false,
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\npath = \"/custom\"\n",
                false,
            ),
            ("rust-toolchain.toml", "not toml", false),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"1.100.0\"\n",
                true,
            ),
        ] {
            let mut f = Fixture::new();
            let published = f.identity(None).for_seed().unwrap().clone();
            f.stage(name, text);
            if both {
                f.stage("rust-toolchain", "1.100.0\n");
            }
            let identity = f.identity(None);
            assert!(identity.for_seed().is_err(), "{text}");
            assert!(identity.for_publication().is_err(), "{text}");
            assert!(!identity.differs_for_due(&published));
        }
        let mut f = Fixture::new();
        f.stage(
            "rust-toolchain.toml",
            &format!(
                "[toolchain]\nchannel = \"1.100.0\"\n#{}",
                "x".repeat(16 * 1024)
            ),
        );
        assert!(f.identity(None).for_seed().is_err());
        assert!(f.identity(None).for_publication().is_err());
    }

    #[test]
    fn seed_time_step_stamps_sources_and_leaves_both_cargo_layouts_untouched() {
        use crate::cargo_gc::fixture::{files, materialize, V1, V2};
        use std::time::{Duration, UNIX_EPOCH};
        for layout in [V1, V2] {
            let mut f = Fixture::new();
            f.stage("stable.rs", "unchanged");
            f.stage("changed.rs", "before");
            let base = Record {
                format: 1,
                commit: "unused".into(),
                tree: f.tree.trim().into(),
                inputs: f.identity(None).for_seed().unwrap().clone(),
                fixed_path: f.scratch.to_path_buf(),
                fill_start: Timestamp {
                    seconds: 1,
                    nanos: 0,
                },
                newest_mtime: Timestamp {
                    seconds: 2,
                    nanos: 0,
                },
            };
            f.stage("changed.rs", "after");
            let target = f.scratch.join("target/bd/ab/0123456789abcd");
            materialize(&target, layout);
            let outputs = files(&target);
            let output_time = UNIX_EPOCH + Duration::from_secs(2);
            for output in &outputs {
                fs::File::open(output)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(output_time))
                    .unwrap();
            }
            let new =
                crate::seed_times::apply(&f.scratch, &f.scratch, f.tree.trim(), &base).unwrap();
            assert_eq!(
                fs::metadata(f.scratch.join("stable.rs"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                UNIX_EPOCH + Duration::from_secs(1)
            );
            assert_eq!(
                fs::metadata(f.scratch.join("changed.rs"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                UNIX_EPOCH + Duration::new(new.seconds as u64, new.nanos as u32)
            );
            for output in outputs {
                assert_eq!(
                    fs::metadata(&output).unwrap().modified().unwrap(),
                    output_time,
                    "{output:?}"
                );
            }
        }
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
