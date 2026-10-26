//! Descriptor-relative detachment keeps mapped deletions inside their host root,
//! even when a checked ancestor is replaced before the rename.
#[cfg(target_os = "linux")]
use super::{owner, startup};
use super::{CallerRoot, HostPath};
use crate::Config;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::{fs, io, sync::Arc};

pub(crate) fn roots(config: &Config) -> impl Iterator<Item = PathBuf> + '_ {
    config.caller_roots.iter().map(|root| {
        root.host_root
            .join(format!(".motor-trash-{}", config.instance))
    })
}

pub(crate) fn mapped_root<'a>(config: &'a Config, path: HostPath<'_>) -> Option<&'a CallerRoot> {
    config
        .caller_roots
        .iter()
        .find(|root| path.as_path().starts_with(&root.host_root))
}

#[cfg(target_os = "linux")]
pub(crate) fn validate(directory: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("caller-root trash must be runner-private"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) struct Retired {
    directory: Arc<fs::File>,
    name: std::ffi::OsString,
}

#[cfg(target_os = "linux")]
impl Retired {
    pub(crate) fn path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        // Cleanup helpers may run in subprocesses, where /proc/self names the
        // helper rather than the runner holding the descriptor.
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.directory.as_raw_fd()
        ))
        .join(&self.name)
    }

    pub(crate) fn remove(&self, isolate_jobs: bool, boat_profile: bool) -> io::Result<()> {
        if boat_profile {
            return crate::boat_quota::remove_retired_entry(&self.directory, &self.name);
        }
        let path = self.path();
        let metadata = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        };
        if !metadata.is_dir() {
            return fs::remove_file(path);
        }
        if isolate_jobs {
            crate::isolation::reclaim_runtime(&path)?;
        }
        fs::remove_dir_all(path)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn entries(config: &Config) -> io::Result<Vec<Retired>> {
    use std::os::fd::AsRawFd;
    let mut entries = Vec::new();
    for root in roots(config) {
        let directory = Arc::new(owner::directory(&root)?);
        validate(&directory)?;
        for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
            entries.push(Retired {
                directory: directory.clone(),
                name: entry?.file_name(),
            });
        }
    }
    Ok(entries)
}

#[cfg(target_os = "linux")]
pub(crate) fn garbage(config: &Config, path: HostPath<'_>) -> io::Result<Retired> {
    let path = path.as_path();
    let trash = roots(config)
        .find(|root| path.parent() == Some(root.as_path()))
        .ok_or_else(|| io::Error::other("not caller-root trash garbage"))?;
    let directory = owner::directory(&trash)?;
    validate(&directory)?;
    Ok(Retired {
        directory: Arc::new(directory),
        name: path.file_name().unwrap().to_owned(),
    })
}

/// The caller selects the mapped branch before calling this function. An absent
/// tree is idempotent; any other failure is returned rather than falling back
/// to the runner's state/trash directory or a sibling trash directory.
#[cfg(target_os = "linux")]
pub(crate) fn detach(
    config: &Config,
    root: &CallerRoot,
    path: HostPath<'_>,
) -> io::Result<Option<Retired>> {
    use std::{ffi::CString, os::fd::AsRawFd, os::unix::ffi::OsStrExt};
    let path = path.as_path();
    if path == root.host_root || !startup::canonical_spelling(&path.to_string_lossy()) {
        return Err(io::Error::other("invalid mapped deletion path"));
    }
    let relative = path
        .strip_prefix(&root.host_root)
        .map_err(io::Error::other)?;
    if relative.components().next().is_some_and(|part| {
        part.as_os_str() == format!(".motor-trash-{}", config.instance).as_str()
            || part.as_os_str() == ".motor-owner"
    }) {
        return Err(io::Error::other(
            "cannot discard caller-root private storage",
        ));
    }
    let host_root = owner::directory(&root.host_root)?;
    let parent = match owner::directory_at(&host_root, relative.parent().unwrap()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result?,
    };
    let name = CString::new(path.file_name().unwrap().as_bytes())?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK {
        return Err(io::Error::other("mapped deletion path is a symlink"));
    }
    let trash = PathBuf::from(format!(".motor-trash-{}", config.instance));
    let directory = owner::directory_at(&host_root, &trash)?;
    validate(&directory)?;
    let name_in_trash = uuid::Uuid::now_v7().to_string();
    let destination = CString::new(name_in_trash.as_str())?;
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            name.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // A durable rename precedes removing the catalog key. Interrupted cleanup
    // can therefore leave a record naming an absent tree, never the reverse.
    directory.sync_all()?;
    parent.sync_all()?;
    Ok(Some(Retired {
        directory: Arc::new(directory),
        name: name_in_trash.into(),
    }))
}
