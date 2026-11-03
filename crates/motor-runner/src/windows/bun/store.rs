//! Provenance-partitioned host storage with serialized quota enforcement.
use super::{bounded_file, protected_directory};
use motor_winagent::bun::{Check, Integrity, RefusalKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

pub(super) const DEFAULT_QUOTA: u64 = 16 * 1024 * 1024 * 1024;
const MAX_OBJECTS: usize = 65_536;
static ACTIVE_PINS: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Provenance {
    pub registry: String,
    pub origin: String,
    pub scope: Option<String>,
    pub credentialed: bool,
}
impl Provenance {
    pub fn partition(&self, root: &Path) -> PathBuf {
        let origin = format!(
            "{:x}",
            Sha256::digest(format!("{}\n{}", self.registry, self.credentialed).as_bytes())
        );
        let scope = self.scope.as_ref().map_or_else(
            || "_public".into(),
            |scope| format!("{:x}", Sha256::digest(scope.as_bytes())),
        );
        root.join(origin).join(scope)
    }
}

#[derive(Default, Debug)]
pub(crate) struct Lease {
    job_id: Option<uuid::Uuid>,
    // Workspaces can request the same blob repeatedly. Keep one shared lock
    // per store path, bounded by the store's object limit, not by lockfile rows.
    pins: BTreeMap<PathBuf, File>,
}
impl Lease {
    pub(super) fn for_job(job_id: uuid::Uuid) -> Self {
        Self {
            job_id: Some(job_id),
            pins: BTreeMap::new(),
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let pins = std::mem::take(&mut self.pins);
        let released = pins.len();
        drop(pins);
        let total = ACTIVE_PINS.fetch_sub(released, Ordering::Relaxed) - released;
        tracing::info!(job_id = ?self.job_id, released_pins = released, runner_pins = total,
            "Bun package lease dropped");
    }
}
pub(crate) fn log_pin_usage() {
    // Compare tracked leases with the kernel on Linux so an untracked open
    // descriptor is visible too. Never emit registry paths or credentials.
    #[cfg(target_os = "linux")]
    let open_pin_fds = fs::read_dir("/proc/self/fd").ok().map(|entries| {
        entries
            .filter_map(|entry| fs::read_link(entry.ok()?.path()).ok())
            .filter(|path| {
                let path = path.to_string_lossy();
                path.contains("/bun-cas/")
                    && (path.ends_with(".pin") || path.ends_with(".pin (deleted)"))
            })
            .count()
    });
    #[cfg(not(target_os = "linux"))]
    let open_pin_fds: Option<usize> = None;
    tracing::info!(
        runner_pins = ACTIVE_PINS.load(Ordering::Relaxed),
        ?open_pin_fds,
        "Bun package pin usage"
    );
}
#[cfg(test)]
impl Lease {
    pub(crate) fn for_test(reference: File) -> Self {
        ACTIVE_PINS.fetch_add(1, Ordering::Relaxed);
        Self {
            pins: BTreeMap::from([(PathBuf::new(), reference)]),
            job_id: None,
        }
    }
    pub(crate) fn pin_count(&self) -> usize {
        self.pins.len()
    }
    // Only the Linux descriptor-count test uses it (it reads /proc/self/fd).
    #[cfg(target_os = "linux")]
    pub(crate) fn prepare_for_test(root: &Path) -> Self {
        let source = Provenance {
            registry: "https://registry.npmjs.org/".into(),
            origin: "https://registry.npmjs.org".into(),
            scope: None,
            credentialed: false,
        };
        let mut lease = Self::default();
        for body in [b"first".as_slice(), b"second", b"third"] {
            let integrity = Integrity {
                algorithm: "sha256".into(),
                hex: format!("{:x}", Sha256::digest(body)),
            };
            get(root, &source, &integrity, 1024, &mut lease, || {
                Ok(body.to_vec())
            })
            .unwrap();
        }
        lease
    }
}

fn contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn lock(root: &Path) -> io::Result<File> {
    protected_directory(root)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("publication.lock"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) && Instant::now() < deadline => {
                std::thread::park_timeout(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    }
}
fn provenance(root: &Path, expected: &Provenance, create: bool) -> io::Result<PathBuf> {
    let partition = expected.partition(root);
    protected_directory(partition.parent().unwrap())?;
    protected_directory(&partition)?;
    let path = partition.join("provenance.json");
    if path.exists() {
        let actual: Provenance = serde_json::from_slice(&bounded_file(&path, 16 * 1024)?)?;
        if actual != *expected {
            return Err(io::Error::other("Bun provenance mismatch"));
        }
    } else {
        if !create {
            return Err(io::Error::other("Bun blob has no recorded provenance"));
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&serde_json::to_vec(expected)?)?;
        file.sync_all()?;
    }
    Ok(partition)
}
fn pin(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("pin"))
}
fn touch(root: &Path, path: &Path) -> io::Result<()> {
    let counter = root.join("access-counter");
    let previous = match fs::read_to_string(&counter) {
        Ok(value) => value.parse::<u64>().map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    let next = previous
        .checked_add(1)
        .ok_or_else(|| io::Error::other("Bun access counter exhausted"))?;
    fs::write(counter, next.to_string())?;
    fs::write(path.with_extension("used"), next.to_string())
}
fn blobs(root: &Path, output: &mut Vec<(u64, PathBuf, u64)>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other("Bun store link"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other("Bun store reparse point"));
            }
        }
        if metadata.is_dir() {
            blobs(&entry.path(), output)?;
        } else {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.len(), 64 | 128) && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                let used = fs::read_to_string(entry.path().with_extension("used"))
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                output.push((used, entry.path(), metadata.len()));
            }
        }
    }
    Ok(())
}
fn prune(mut directory: &Path, root: &Path) -> io::Result<()> {
    while directory != root {
        let entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
        if entries.len() == 1 && entries[0].file_name() == "provenance.json" {
            fs::remove_file(entries[0].path())?;
        } else if !entries.is_empty() {
            break;
        }
        fs::remove_dir(directory)?;
        directory = directory
            .parent()
            .ok_or_else(|| io::Error::other("Bun store parent"))?;
    }
    Ok(())
}
fn make_room(root: &Path, quota: u64, additional: u64, new_object: bool) -> io::Result<()> {
    let mut entries = Vec::new();
    blobs(root, &mut entries)?;
    let mut size = entries.iter().try_fold(additional, |n, entry| {
        n.checked_add(entry.2)
            .ok_or_else(|| io::Error::other("Bun size overflow"))
    })?;
    let mut count = entries.len() + usize::from(new_object);
    entries.sort();
    for (_, path, bytes) in entries {
        if size <= quota && count <= MAX_OBJECTS {
            return Ok(());
        }
        let reference = pin(&path)?;
        match fs2::FileExt::try_lock_exclusive(&reference) {
            Ok(()) => {
                fs::remove_file(&path)?;
                let _ = fs::remove_file(path.with_extension("used"));
                let _ = fs::remove_file(path.with_extension("pin"));
                size -= bytes;
                count -= 1;
                drop(reference);
                prune(path.parent().unwrap(), root)?;
            }
            Err(error) if contended(&error) => (),
            Err(error) => return Err(error),
        }
    }
    if size <= quota && count <= MAX_OBJECTS {
        Ok(())
    } else {
        Err(io::Error::other(
            "Bun host quota is occupied by active jobs",
        ))
    }
}
fn mismatch(
    root: &Path,
    source: &Provenance,
    integrity: &Integrity,
    bytes: &[u8],
) -> io::Result<()> {
    let directory = root.join("quarantine");
    protected_directory(&directory)?;
    // One bounded diagnostic replaces the previous mismatch; rejected bodies
    // are never retained and cannot consume the package-store byte budget.
    fs::write(
        directory.join("last-mismatch.json"),
        serde_json::to_vec(&serde_json::json!({
            "origin": source.origin, "scope": source.scope, "expected": integrity,
            "actual_sha256": format!("{:x}", Sha256::digest(bytes)), "size": bytes.len()
        }))?,
    )
}

#[cfg(test)]
pub(super) fn get(
    root: &Path,
    source: &Provenance,
    integrity: &Integrity,
    quota: u64,
    lease: &mut Lease,
    download: impl FnMut() -> Result<Vec<u8>, RefusalKind>,
) -> Result<Vec<u8>, RefusalKind> {
    get_checked(root, source, integrity, quota, lease, download).map_err(|e| e.0)
}

pub(super) fn get_checked(
    root: &Path,
    source: &Provenance,
    integrity: &Integrity,
    quota: u64,
    lease: &mut Lease,
    mut download: impl FnMut() -> Result<Vec<u8>, RefusalKind>,
) -> Result<Vec<u8>, (RefusalKind, Check)> {
    if !integrity.valid() {
        return Err((RefusalKind::BunLockInvalid, Check::Lock));
    }
    let cache_error = |_| (RefusalKind::BunCacheMissing, Check::CachePublish);
    let publication = lock(root).map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheLock))?;
    let partition = source.partition(root);
    let directory = partition.join(&integrity.algorithm);
    let path = directory.join(&integrity.hex);
    if path.exists() {
        provenance(root, source, false)
            .map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheProvenance))?;
        #[cfg(windows)]
        super::protection::check(&path, 0)
            .map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheProtection))?;
    }
    let cached = match bounded_file(&path, motor_winagent::bun::MAX_BLOB_BYTES) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err((RefusalKind::BunCacheMissing, Check::CacheRead)),
    };
    drop(publication);
    let bytes = match cached {
        Some(bytes) => bytes,
        None => download().map_err(|kind| (kind, Check::Download))?,
    };
    let _publication = lock(root).map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheLock))?;
    if !integrity.verify(&bytes) {
        mismatch(root, source, integrity, &bytes).map_err(cache_error)?;
        if path.exists() {
            let reference = pin(&path).map_err(cache_error)?;
            if fs2::FileExt::try_lock_exclusive(&reference).is_ok() {
                let _ = fs::remove_file(&path);
            }
        }
        return Err((RefusalKind::BunIntegrityMismatch, Check::Integrity));
    }
    let reference = if path.exists() {
        let reference = if lease.pins.contains_key(&path) {
            None
        } else {
            let reference = pin(&path).map_err(cache_error)?;
            fs2::FileExt::lock_shared(&reference).map_err(cache_error)?;
            Some(reference)
        };
        make_room(root, quota, 0, false)
            .map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheQuota))?;
        reference
    } else {
        make_room(root, quota, bytes.len() as u64, true)
            .map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheQuota))?;
        provenance(root, source, true)
            .map_err(|_| (RefusalKind::BunCacheMissing, Check::CacheProvenance))?;
        protected_directory(&directory).map_err(cache_error)?;
        let reference = pin(&path).map_err(cache_error)?;
        fs2::FileExt::lock_shared(&reference).map_err(cache_error)?;
        Some(reference)
    };
    if !path.exists() {
        let temporary = directory.join(format!(".{}", uuid::Uuid::now_v7()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(cache_error)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(cache_error)?;
        fs::hard_link(&temporary, &path).map_err(cache_error)?;
        fs::remove_file(temporary).map_err(cache_error)?;
    }
    touch(root, &path).map_err(cache_error)?;
    if let Some(reference) = reference {
        let previous = lease.pins.insert(path, reference);
        if previous.is_none() {
            let total = ACTIVE_PINS.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(job_id = ?lease.job_id, lease_pins = lease.pins.len(), runner_pins = total,
                "Bun package pin acquired");
        }
    }
    Ok(bytes)
}
