use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::MetadataExt,
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
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() {
            return Err(io::Error::other(
                "cargo_overlay_unsafe_lower: not a physical directory",
            ));
        }
        Ok(Self {
            uid: metadata.uid(),
            gid: metadata.gid(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone)]
enum Validation {
    Pending,
    Ready(Owner),
    Failed(String),
}

static VALIDATED: OnceLock<Mutex<BTreeMap<PathBuf, Validation>>> = OnceLock::new();

fn validations() -> &'static Mutex<BTreeMap<PathBuf, Validation>> {
    VALIDATED.get_or_init(Mutex::default)
}

/// The daemon and its snapshot helpers create cache entries as the daemon UID.
/// Validate old entries once, without blocking readiness or holding runner state.
/// Later job setup only checks the identity of these two trusted cache roots.
pub(in crate::isolation) fn start(home: &Path) {
    let mut pending = Vec::new();
    {
        let mut validated = validations().lock().unwrap();
        for name in ["registry", "git"] {
            let path = home.join(name);
            if !validated.contains_key(&path) {
                validated.insert(path.clone(), Validation::Pending);
                pending.push(path);
            }
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
                validate(&path);
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
        _ => Err(io::Error::other(
            "cargo_overlay_ownership_pending: cache ownership validation has not completed",
        )),
    }
}

pub(in crate::isolation) fn validate(path: &Path) -> (usize, Duration) {
    let started = Instant::now();
    let mut entries = 0;
    let result = scan(path, &mut entries);
    let elapsed = started.elapsed();
    let validation = match result {
        Ok(owner) => Validation::Ready(owner),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "Cargo overlay ownership validation refused network cache writes");
            Validation::Failed(error.to_string())
        }
    };
    tracing::info!(path = %path.display(), entries, elapsed_ms = elapsed.as_millis(), "Cargo overlay startup ownership validation finished");
    validations()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), validation);
    (entries, elapsed)
}

fn scan(root: &Path, entries: &mut usize) -> io::Result<Owner> {
    let owner = Owner::read(root)?;
    // Both direct fetches and systemd snapshot helpers use the trusted daemon's
    // UID. A different root owner would make future fetches introduce mixed IDs.
    if owner.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::other(format!(
            "cargo_overlay_fetch_owner_mismatch: {} has uid {}, trusted fetch uid is {}",
            root.display(),
            owner.uid,
            unsafe { libc::geteuid() }
        )));
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            *entries += 1;
            if metadata.uid() != owner.uid {
                return Err(io::Error::other(format!(
                    "cargo_overlay_mixed_ownership: {} has uid {}, expected {}",
                    path.display(),
                    metadata.uid(),
                    owner.uid
                )));
            }
            if metadata.is_dir() {
                directories.push(path);
            } else if !metadata.is_file() && !metadata.file_type().is_symlink() {
                return Err(io::Error::other(format!(
                    "cargo_overlay_unsafe_lower: {} is not a cache file",
                    path.display()
                )));
            }
        }
    }
    if Owner::read(root)? != owner {
        return Err(io::Error::other(
            "cargo_overlay_lower_changed: root changed during ownership validation",
        ));
    }
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
