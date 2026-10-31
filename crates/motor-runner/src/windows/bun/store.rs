//! Provenance-partitioned host storage with serialized quota enforcement.
use super::{bounded_file, protected_directory};
use motor_winagent::bun::{Integrity, RefusalKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(super) const DEFAULT_QUOTA: u64 = 16 * 1024 * 1024 * 1024;
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
    pins: Vec<File>,
}
#[cfg(test)]
impl Lease {
    pub(crate) fn for_test(reference: File) -> Self {
        Self {
            pins: vec![reference],
        }
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
        if size <= quota && count <= 65_536 {
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
    if size <= quota && count <= 65_536 {
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

pub(super) fn get(
    root: &Path,
    source: &Provenance,
    integrity: &Integrity,
    quota: u64,
    lease: &mut Lease,
    mut download: impl FnMut() -> Result<Vec<u8>, RefusalKind>,
) -> Result<Vec<u8>, RefusalKind> {
    if !integrity.valid() {
        return Err(RefusalKind::BunLockInvalid);
    }
    let cache_error = |_| RefusalKind::BunCacheMissing;
    let publication = lock(root).map_err(cache_error)?;
    let partition = source.partition(root);
    let directory = partition.join(&integrity.algorithm);
    let path = directory.join(&integrity.hex);
    if path.exists() {
        provenance(root, source, false).map_err(cache_error)?;
        #[cfg(windows)]
        super::protection::check(&path, 0).map_err(cache_error)?;
    }
    let cached = match bounded_file(&path, motor_winagent::bun::MAX_BLOB_BYTES) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err(RefusalKind::BunCacheMissing),
    };
    drop(publication);
    let bytes = match cached {
        Some(bytes) => bytes,
        None => download()?,
    };
    let _publication = lock(root).map_err(cache_error)?;
    if !integrity.verify(&bytes) {
        mismatch(root, source, integrity, &bytes).map_err(cache_error)?;
        if path.exists() {
            let reference = pin(&path).map_err(cache_error)?;
            if fs2::FileExt::try_lock_exclusive(&reference).is_ok() {
                let _ = fs::remove_file(&path);
            }
        }
        return Err(RefusalKind::BunIntegrityMismatch);
    }
    let reference = if path.exists() {
        let reference = pin(&path).map_err(cache_error)?;
        fs2::FileExt::lock_shared(&reference).map_err(cache_error)?;
        make_room(root, quota, 0, false).map_err(cache_error)?;
        reference
    } else {
        make_room(root, quota, bytes.len() as u64, true).map_err(cache_error)?;
        provenance(root, source, true).map_err(cache_error)?;
        protected_directory(&directory).map_err(cache_error)?;
        let reference = pin(&path).map_err(cache_error)?;
        fs2::FileExt::lock_shared(&reference).map_err(cache_error)?;
        reference
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
    lease.pins.push(reference);
    Ok(bytes)
}
