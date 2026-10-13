//! Record the filesystem changes made while filling the compiler cache without
//! enumerating its existing entries.
use crate::isolation::{self, WriterAccount};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

/// Begin at writer reservation, hold through unit shutdown, then finish while
/// still holding the cache fence. Overflow or a removed watch fails the fill;
/// unreadable entries must never be advertised as a successful fill.
pub struct CacheFill {
    events: File,
    watches: BTreeMap<i32, PathBuf>,
    owner: u32,
}

impl CacheFill {
    pub fn begin(config: &crate::engine::Config) -> io::Result<Self> {
        let account = config
            .writer_account
            .ok_or_else(|| io::Error::other("dependency writer account is unavailable"))?;
        Self::begin_at(&config.sccache_dir(), account)
    }

    pub(crate) fn begin_at(fleet: &Path, account: WriterAccount) -> io::Result<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fill = Self {
            events: unsafe { File::from_raw_fd(fd) },
            watches: BTreeMap::new(),
            owner: account.uid,
        };
        fill.watch(fleet)?;
        // sccache 0.18.0 shards hexadecimal keys under <first>/<second>/<key>.
        // Probe only those 272 directories, never enumerate their old entries.
        // A newly created shard is reported by its already-watched parent and
        // only that new subtree is scanned after the writer has stopped.
        for first in "0123456789abcdef".chars() {
            let parent = fleet.join(first.to_string());
            fill.watch_if_present(&parent)?;
            for second in "0123456789abcdef".chars() {
                fill.watch_if_present(&parent.join(second.to_string()))?;
            }
        }
        Ok(fill)
    }

    fn watch_if_present(&mut self, path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(_) => self.watch(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn watch(&mut self, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != self.owner {
            return Err(io::Error::other(
                "cache watch requires a writer-owned physical directory",
            ));
        }
        let name = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let mask = libc::IN_CREATE
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_ONLYDIR
            | libc::IN_DONT_FOLLOW;
        let wd = unsafe { libc::inotify_add_watch(self.events.as_raw_fd(), name.as_ptr(), mask) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.watches.insert(wd, path.to_path_buf());
        Ok(())
    }

    pub fn finish(self) -> io::Result<()> {
        self.finish_with(isolation::repair_writer_cache_path)
    }

    fn finish_with(
        mut self,
        mut repair: impl FnMut(&Path, bool) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut changed = BTreeMap::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let length = match self.events.read(&mut buffer) {
                Ok(0) => return Err(io::Error::other("cache change stream closed unexpectedly")),
                Ok(length) => length,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "read cache change events: {error}"
                    )))
                }
            };
            self.record_events(&buffer[..length], &mut changed)?;
        }
        let owner = self.owner;
        drop(self);
        let mut pending: Vec<_> = changed.into_iter().collect();
        let mut seen = BTreeSet::new();
        while let Some((path, new_subtree)) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue, // tempfiles may have been renamed
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "inspect cache entry {}: {error}",
                        path.display()
                    )))
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.uid() != owner || (!metadata.is_file() && !metadata.is_dir()) {
                return Err(io::Error::other(
                    "changed cache entry is not owned by the writer",
                ));
            }
            // sccache 0.18.0 persists 0600 NamedTempFiles. That creation mode
            // masks inherited named-user ACLs, so repair the changed entries
            // after unit shutdown, inside the cache fence, before publication.
            repair(&path, metadata.is_dir()).map_err(|e| {
                io::Error::other(format!("repair cache ACL {}: {e}", path.display()))
            })?;
            // Secure directory modes can mask inherited daemon ACLs too. Repair
            // each new directory before enumerating its children.
            if metadata.is_dir() && new_subtree {
                for entry in fs::read_dir(&path).map_err(|e| {
                    io::Error::other(format!("scan new cache shard {}: {e}", path.display()))
                })? {
                    pending.push((entry?.path(), true));
                }
            }
        }
        Ok(())
    }

    fn record_events(&self, bytes: &[u8], changed: &mut BTreeMap<PathBuf, bool>) -> io::Result<()> {
        let mut offset = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while offset + header <= bytes.len() {
            let event = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<libc::inotify_event>())
            };
            let end = offset + header + event.len as usize;
            if end > bytes.len() {
                return Err(io::Error::other("incomplete cache change event"));
            }
            if event.mask
                & (libc::IN_Q_OVERFLOW
                    | libc::IN_DELETE_SELF
                    | libc::IN_MOVE_SELF
                    | libc::IN_IGNORED)
                != 0
            {
                return Err(io::Error::other(
                    "cache change tracking lost events; fill cannot be published",
                ));
            }
            if event.len != 0 {
                let directory = self
                    .watches
                    .get(&event.wd)
                    .ok_or_else(|| io::Error::other("unknown cache watch"))?;
                let name = &bytes[offset + header..end];
                let nul = name
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| io::Error::other("invalid cache event name"))?;
                let path = directory.join(std::ffi::OsStr::from_bytes(&name[..nul]));
                let new_subtree = event.mask & libc::IN_ISDIR != 0
                    && event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0;
                *changed.entry(path).or_insert(false) |= new_subtree;
            }
            offset = end;
        }
        if offset != bytes.len() {
            return Err(io::Error::other("incomplete cache change event header"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
