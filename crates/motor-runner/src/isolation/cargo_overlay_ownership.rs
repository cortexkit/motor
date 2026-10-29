use super::tree_fs::{self, Directory, Entries};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::isolation) struct Owner {
    pub uid: u32,
    pub gid: u32,
    device: u64,
    inode: u64,
}

impl Owner {
    pub(in crate::isolation) fn observed(path: &Path) -> io::Result<Self> {
        Self::read(path)
    }
    fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::from_stat(&Directory::open(path)?.stat()?))
    }

    fn from_stat(metadata: &libc::stat) -> Self {
        Self {
            uid: metadata.st_uid,
            gid: metadata.st_gid,
            device: metadata.st_dev,
            inode: metadata.st_ino,
        }
    }
}

#[derive(Clone)]
enum Validation {
    Pending,
    Ready(Owner),
    Failed(String),
    Retrying(String),
}

static VALIDATED: OnceLock<Mutex<BTreeMap<PathBuf, Validation>>> = OnceLock::new();

fn validations() -> &'static Mutex<BTreeMap<PathBuf, Validation>> {
    VALIDATED.get_or_init(Mutex::default)
}

/// The daemon and its snapshot helpers create cache entries as the daemon UID.
/// Validate old entries once, without blocking readiness or holding runner state.
/// Later job setup only checks the identity of the append-only archive root.
pub(in crate::isolation) fn start(home: &Path) {
    let mut pending = Vec::new();
    {
        let mut validated = validations().lock().unwrap();
        let path = home.join("registry/cache");
        if !validated.contains_key(&path) {
            validated.insert(path.clone(), Validation::Pending);
            pending.push(path);
        }
    }
    if pending.is_empty() {
        return;
    }
    let failed = pending.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("cargo-cache-ownership".into())
        .spawn(move || {
            for path in pending {
                retry_validation(&path, std::thread::sleep);
            }
        })
    {
        let mut validated = validations().lock().unwrap();
        for path in failed {
            validated.insert(
                path,
                Validation::Failed(format!("cargo_overlay_validation_failed: {error}")),
            );
        }
    }
}

pub(in crate::isolation) fn validated_owner(path: &Path) -> io::Result<Owner> {
    let current = Owner::read(path)?;
    let validated = validations().lock().unwrap();
    match validated.get(path) {
        Some(Validation::Ready(owner)) if *owner == current => Ok(current),
        Some(Validation::Ready(_)) => Err(io::Error::other(
            "cargo_overlay_lower_changed: restart ownership validation",
        )),
        Some(Validation::Failed(error)) => Err(io::Error::other(error.clone())),
        Some(Validation::Retrying(error)) => Err(io::Error::other(format!(
            "cargo_overlay_ownership_retrying: {error}"
        ))),
        _ => Err(io::Error::other(
            "cargo_overlay_ownership_pending: cache ownership validation has not completed",
        )),
    }
}

#[cfg(test)]
pub(in crate::isolation) fn validate(path: &Path) -> (usize, Duration) {
    let (entries, elapsed, _) = attempt(path);
    (entries, elapsed)
}

fn retry_validation(path: &Path, mut wait: impl FnMut(Duration)) {
    let mut delay = Duration::from_millis(50);
    loop {
        if !attempt(path).2 {
            return;
        }
        // A writer can temporarily remove a name or make a directory unavailable.
        // Never turn that race into a process-lifetime refusal or sleep with a lock.
        wait(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

fn attempt(path: &Path) -> (usize, Duration, bool) {
    let started = Instant::now();
    let mut entries = 0;
    let result = scan_with(path, &mut entries, |_, _| {});
    let elapsed = started.elapsed();
    let (validation, retry) = match result {
        Ok(owner) => (Validation::Ready(owner), false),
        Err(ScanError::Hard(error)) => (Validation::Failed(error), false),
        Err(ScanError::Transient(error)) => (Validation::Retrying(error.to_string()), true),
    };
    tracing::info!(path = %path.display(), entries, elapsed_ms = elapsed.as_millis(), retry, "Cargo overlay startup ownership validation finished");
    validations()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), validation);
    (entries, elapsed, retry)
}

#[derive(Debug)]
enum ScanError {
    Hard(String),
    Transient(io::Error),
}

impl From<io::Error> for ScanError {
    fn from(error: io::Error) -> Self {
        Self::Transient(error)
    }
}

struct Frame {
    directory: Directory,
    entries: Entries,
    diagnostic_path: PathBuf,
}

impl Frame {
    fn new(directory: Directory, diagnostic_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            entries: directory.entries()?,
            directory,
            diagnostic_path,
        })
    }
}

fn scan_with(
    root: &Path,
    count: &mut usize,
    mut visit: impl FnMut(&Path, bool),
) -> Result<Owner, ScanError> {
    let pinned = Directory::open(root)?;
    let owner = Owner::from_stat(&pinned.stat()?);
    if owner.uid != unsafe { libc::geteuid() } {
        return Err(ScanError::Hard(format!(
            "cargo_overlay_fetch_owner_mismatch: {} has uid {}, trusted fetch uid is {}",
            root.display(),
            owner.uid,
            unsafe { libc::geteuid() }
        )));
    }
    let mut stack = vec![Frame::new(pinned, root.to_path_buf())?];
    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.entries.next(&frame.directory)? else {
            if frame.directory.stat()?.st_nlink == 0 {
                return Err(io::Error::other("cache directory vanished while scanning").into());
            }
            stack.pop();
            continue;
        };
        *count += 1;
        let path = frame.diagnostic_path.join(&entry.name);
        if entry.stat.st_uid != owner.uid {
            return Err(ScanError::Hard(format!(
                "cargo_overlay_mixed_ownership: {} has uid {}, expected {}",
                path.display(),
                entry.stat.st_uid,
                owner.uid
            )));
        }
        match entry.stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child = frame.directory.child(&entry.name)?;
                let metadata = child.stat()?;
                if metadata.st_uid != owner.uid {
                    return Err(ScanError::Hard(format!(
                        "cargo_overlay_mixed_ownership: {} changed owner",
                        path.display()
                    )));
                }
                if !tree_fs::same_inode(&metadata, &entry.stat) || metadata.st_nlink == 0 {
                    return Err(io::Error::other("cache directory changed while opening it").into());
                }
                // The callback observes an already pinned directory, never a
                // pathname that traversal will later resolve or follow.
                visit(&path, true);
                stack.push(Frame::new(child, path)?);
            }
            libc::S_IFREG | libc::S_IFLNK => visit(&path, false),
            _ => {
                return Err(ScanError::Hard(format!(
                    "cargo_overlay_unsafe_lower: {} is not a cache file",
                    path.display()
                )))
            }
        }
    }
    if Owner::read(root)? != owner {
        return Err(io::Error::other(
            "cargo_overlay_lower_changed: root changed during ownership validation",
        )
        .into());
    }
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unvalidated_lower_refuses_network_overlay() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-overlay-pending");
        let error = validated_owner(scratch.path()).unwrap_err();
        assert!(error
            .to_string()
            .contains("cargo_overlay_ownership_pending"));
    }

    #[test]
    fn validated_owner_lookup_does_not_rewalk_descendants() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-overlay-owner");
        let lower = scratch.path().join("registry");
        let child = lower.join("cache");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("crate"), "cached bytes").unwrap();
        assert_eq!(validate(&lower).0, 2);
        fs::set_permissions(&child, fs::Permissions::from_mode(0o000)).unwrap();
        let result = validated_owner(&lower);
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap().uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn replacing_validated_root_refuses_network_overlay() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-overlay-replaced");
        let lower = scratch.path().join("registry");
        fs::create_dir(&lower).unwrap();
        validate(&lower);
        fs::rename(&lower, scratch.path().join("old-registry")).unwrap();
        fs::create_dir(&lower).unwrap();
        let error = validated_owner(&lower).unwrap_err();
        assert!(error.to_string().contains("cargo_overlay_lower_changed"));
    }

    #[test]
    fn pinned_directory_replaced_by_symlink_never_visits_its_target() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-owner-symlink-race");
        let root = scratch.path().join("root");
        let queued = root.join("queued");
        let outside = scratch.path().join("outside");
        fs::create_dir_all(&queued).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(queued.join("original"), "inside").unwrap();
        fs::write(outside.join("must-not-visit"), "outside").unwrap();
        let mut moved = false;
        let mut visited = Vec::new();
        scan_with(&root, &mut 0, |path, pinned| {
            visited.push(path.file_name().unwrap().to_owned());
            if path == queued && pinned && !moved {
                fs::rename(&queued, scratch.path().join("parked")).unwrap();
                std::os::unix::fs::symlink(&outside, &queued).unwrap();
                moved = true;
            }
        })
        .unwrap();
        assert!(moved);
        assert!(visited.iter().any(|name| name == "original"));
        assert!(!visited.iter().any(|name| name == "must-not-visit"));
    }

    #[test]
    fn transient_scan_error_is_retried_with_backoff_and_recovers() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-owner-retry");
        let root = scratch.path().join("not-yet-created");
        let mut waits = Vec::new();
        retry_validation(&root, |delay| {
            waits.push(delay);
            assert!(matches!(
                validations().lock().unwrap().get(&root),
                Some(Validation::Retrying(_))
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("entry"), "ready").unwrap();
        });
        assert_eq!(waits, [Duration::from_millis(50)]);
        validated_owner(&root).unwrap();
    }

    #[test]
    fn unsafe_cache_entry_is_a_hard_refusal_without_retry() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-owner-hard");
        let fifo =
            std::ffi::CString::new(scratch.path().join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        retry_validation(scratch.path(), |_| panic!("hard refusal must not retry"));
        assert!(validated_owner(scratch.path())
            .unwrap_err()
            .to_string()
            .contains("cargo_overlay_unsafe_lower"));
    }
}
