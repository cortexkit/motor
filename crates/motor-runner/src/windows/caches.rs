use super::*;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    time::UNIX_EPOCH,
};

pub(crate) struct CacheDisks {
    root: PathBuf,
    locks: PathBuf,
    baseline: String,
    toolchain: String,
}
#[derive(Serialize, Deserialize)]
struct Usage {
    last_used_ms: u64,
}
pub(crate) struct CacheCandidate {
    pub path: PathBuf,
    pub last_used_ms: u64,
}

impl CacheDisks {
    pub(crate) fn new(vm: &VmConfig) -> Self {
        Self {
            root: vm.layout_root.join("vm/caches"),
            locks: vm.layout_root.join("vm/cache-locks"),
            baseline: vm.baseline.clone(),
            toolchain: vm.toolchain_version.clone(),
        }
    }
    pub(crate) fn key(&self, workspace: &str) -> String {
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(workspace, &self.baseline, &self.toolchain))
                    .expect("cache key serializes")
            )
        )
    }
    pub(crate) fn disk(&self, workspace: &str) -> PathBuf {
        self.root.join(format!("{}.qcow2", self.key(workspace)))
    }
    pub(crate) fn marker(&self, disk: &Path) -> PathBuf {
        disk.with_extension("attached")
    }
    fn usage(&self, disk: &Path) -> PathBuf {
        disk.with_extension("json")
    }
    fn valid_disk(&self, path: &Path) -> bool {
        path.parent() == Some(self.root.as_path())
            && path.extension().is_some_and(|s| s == "qcow2")
            && path.file_stem().is_some_and(|s| {
                s.len() == 64 && s.to_string_lossy().bytes().all(|b| b.is_ascii_hexdigit())
            })
    }
    fn real_root(&self) -> io::Result<()> {
        for path in [self.root.parent().expect("cache root parent"), &self.root] {
            if !fs::symlink_metadata(path)
                .map_err(|error| super::vm::path_error("stat cache root", path, error))?
                .is_dir()
            {
                return Err(invalid("Windows cache root is not a real directory"));
            }
        }
        Ok(())
    }
    pub(crate) fn lock(&self, disk: &Path) -> io::Result<File> {
        if !self.valid_disk(disk) {
            return Err(invalid("invalid Windows cache disk name"));
        }
        self.real_root()?;
        fs::create_dir_all(&self.locks).map_err(|error| {
            super::vm::path_error("create cache lock directory", &self.locks, error)
        })?;
        if !fs::symlink_metadata(&self.locks)
            .map_err(|error| {
                super::vm::path_error("stat cache lock directory", &self.locks, error)
            })?
            .is_dir()
        {
            return Err(invalid("Windows cache lock root is a symlink"));
        }
        fs::set_permissions(&self.locks, fs::Permissions::from_mode(0o700)).map_err(|error| {
            super::vm::path_error("chmod cache lock directory", &self.locks, error)
        })?;
        let path = self.locks.join(disk.file_stem().unwrap());
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| super::vm::path_error("open cache lock", &path, error))?;
        lock.lock_exclusive()
            .map_err(|error| super::vm::path_error("lock cache disk", &path, error))?;
        Ok(lock)
    }
    // The caller must hold lock(disk) across rechecking ownership and eviction.
    pub(crate) fn attached(&self, disk: &Path) -> io::Result<bool> {
        let path = self.marker(disk);
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(super::vm::path_error("stat cache attachment", &path, e)),
        }
    }
    pub(crate) fn last_used(&self, disk: &Path) -> io::Result<u64> {
        let usage = self.usage(disk);
        match fs::symlink_metadata(&usage) {
            Ok(m) if m.is_file() => Ok(read_json::<Usage>(&usage)?.last_used_ms),
            Ok(_) => Err(invalid("Windows cache usage is not a regular file")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(fs::symlink_metadata(disk)?
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                as u64),
            Err(e) => Err(e),
        }
    }
    /// Persist attachment before starting a VM. A crash at any subsequent phase
    /// leaves the marker in place, forcing startup to discard the disk.
    #[allow(dead_code)]
    pub(crate) fn attach(&self, workspace: &str, now: u64) -> io::Result<PathBuf> {
        let disk = self.disk(workspace);
        let _lock = self.lock(&disk)?;
        if self.attached(&disk)? {
            return Err(invalid("Windows cache disk is still attached"));
        }
        if !fs::symlink_metadata(&disk)
            .map_err(|error| super::vm::path_error("stat cache disk", &disk, error))?
            .is_file()
        {
            return Err(invalid("Windows cache disk is not a regular file"));
        }
        atomic_json(&self.usage(&disk), &Usage { last_used_ms: now }).map_err(|error| {
            super::vm::path_error("write cache usage", &self.usage(&disk), error)
        })?;
        let marker_path = self.marker(&disk);
        let marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker_path)
            .map_err(|error| {
                super::vm::path_error("create cache attachment", &marker_path, error)
            })?;
        marker
            .sync_all()
            .map_err(|error| super::vm::path_error("sync cache attachment", &marker_path, error))?;
        self.sync_root()?;
        Ok(disk)
    }
    /// Reuse requires all three facts: a known outcome, a clean agent dismount
    /// after command stop, and normal VM unit exit. Any missing fact discards it.
    #[allow(dead_code)]
    pub(crate) fn complete(
        &self,
        workspace: &str,
        now: u64,
        known_outcome: bool,
        clean_dismount: bool,
        normal_exit: bool,
    ) -> io::Result<()> {
        let disk = self.disk(workspace);
        let _lock = self.lock(&disk)?;
        if known_outcome && clean_dismount && normal_exit {
            if !self.attached(&disk)? {
                return Err(invalid("Windows cache completion lacks attachment marker"));
            }
            atomic_json(&self.usage(&disk), &Usage { last_used_ms: now }).map_err(|error| {
                super::vm::path_error("write cache usage", &self.usage(&disk), error)
            })?;
            let marker = self.marker(&disk);
            fs::remove_file(&marker).map_err(|error| {
                super::vm::path_error("remove cache attachment", &marker, error)
            })?;
            self.sync_root()
        } else {
            self.remove(&disk)
        }
    }
    pub(crate) fn remove(&self, disk: &Path) -> io::Result<()> {
        if !self.valid_disk(disk) {
            return Err(invalid("invalid Windows cache disk name"));
        }
        // Delete the disk before its marker: a crash must never make a dirty
        // disk appear reusable. Unlinking a sidecar never follows a symlink.
        for path in [disk.to_path_buf(), self.usage(disk), self.marker(disk)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(super::vm::path_error("remove cache file", &path, e)),
            }
        }
        self.sync_root()
    }
    fn sync_root(&self) -> io::Result<()> {
        File::open(&self.root)
            .and_then(|root| root.sync_all())
            .map_err(|error| super::vm::path_error("sync cache directory", &self.root, error))
    }
    pub(crate) fn reconcile(&self) -> io::Result<()> {
        self.real_root()?;
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "attached") {
                let disk = path.with_extension("qcow2");
                let _lock = self.lock(&disk)?;
                self.remove(&disk)?;
            }
        }
        Ok(())
    }
    pub(crate) fn candidates(
        &self,
        protected: &BTreeSet<PathBuf>,
        device: u64,
    ) -> io::Result<Vec<CacheCandidate>> {
        self.real_root()?;
        if fs::symlink_metadata(&self.root)?.dev() != device {
            return Err(invalid("Windows cache cleanup cannot cross a mount"));
        }
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if self.valid_disk(&path)
                && metadata.is_file()
                && metadata.dev() == device
                && !protected.contains(&path)
                && !self.attached(&path)?
            {
                candidates.push(CacheCandidate {
                    last_used_ms: self.last_used(&path)?,
                    path,
                });
            }
        }
        Ok(candidates)
    }
}
