//! Job paths are resolved only with the caller's filesystem credentials. Keep
//! dirfds alive through a whole operation: a rename cannot redirect an open fd.
use crate::{Error, Identity, Peer, Result};
use std::{
    ffi::CString,
    fs::{File, Metadata},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::Path,
};

pub const RESOLVE: u64 = 0x08 | 0x04 | 0x01; // BENEATH | NO_SYMLINKS | NO_XDEV
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn cstr(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).map_err(|_| Error("NUL in filesystem path".into()))
}
pub fn beneath(dir: RawFd, name: &str, flags: i32, mode: u32) -> Result<File> {
    let name = cstr(name.as_bytes())?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: mode as u64,
        resolve: RESOLVE,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}
struct FsCredentials {
    uid: u32,
    gid: u32,
}
impl FsCredentials {
    fn enter(peer: Peer) -> Result<Self> {
        let gid = unsafe { libc::setfsgid(peer.gid) } as u32;
        let uid = unsafe { libc::setfsuid(peer.uid) } as u32;
        let guard = Self { uid, gid };
        let actual_uid = unsafe { libc::setfsuid(u32::MAX) } as u32;
        let actual_gid = unsafe { libc::setfsgid(u32::MAX) } as u32;
        if actual_uid != peer.uid || actual_gid != peer.gid {
            drop(guard);
            return Err(Error("cannot assume caller filesystem credentials".into()));
        }
        Ok(guard)
    }
}
impl Drop for FsCredentials {
    fn drop(&mut self) {
        unsafe {
            libc::setfsuid(self.uid);
            libc::setfsgid(self.gid);
        }
    }
}
pub fn as_peer<T>(peer: Peer, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = FsCredentials::enter(peer)?;
    action()
}

/// The base is root-owned; instance/jobs/job directories may be caller-owned.
/// Every component below the base, including the final image, uses openat2.
pub struct JobDir<'a> {
    base: &'a File,
    dir: File,
    relative: String,
    peer: Peer,
}
impl<'a> JobDir<'a> {
    pub fn open(base: &'a File, identity: &Identity, peer: Peer) -> Result<Self> {
        crate::validate_instance(&identity.instance)?;
        let relative = format!("{}/jobs/{}", identity.instance, identity.job_id);
        let dir = as_peer(peer, || {
            let dir = beneath(
                base.as_raw_fd(),
                &relative,
                libc::O_PATH | libc::O_DIRECTORY,
                0,
            )?;
            if dir.metadata()?.uid() != peer.uid {
                return Err(Error("job directory is not owned by caller".into()));
            }
            Ok(dir)
        })?;
        Ok(Self {
            base,
            dir,
            relative,
            peer,
        })
    }
    pub fn verify(&self) -> Result<()> {
        as_peer(self.peer, || {
            let current = beneath(
                self.base.as_raw_fd(),
                &self.relative,
                libc::O_PATH | libc::O_DIRECTORY,
                0,
            )?;
            let a = self.dir.metadata()?;
            let b = current.metadata()?;
            if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
                return Err(Error("job directory changed during operation".into()));
            }
            Ok(())
        })
    }
    pub fn clone_into(&self, source: &File, dest: &File) -> Result<()> {
        as_peer(self.peer, || {
            self.verify()?;
            reflink(source, dest)?;
            self.verify()
        })
    }
    pub fn source(&self, name: &str, size: u64) -> Result<File> {
        as_peer(self.peer, || {
            self.verify()?;
            let file = beneath(
                self.dir.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_NONBLOCK,
                0,
            )?;
            let meta = file.metadata()?;
            if !meta.is_file() || meta.uid() != self.peer.uid {
                return Err(Error("image must be a regular file owned by caller".into()));
            }
            if meta.len() != size {
                return Err(Error(format!(
                    "vm_image_size: {name}: expected {size}, got {}",
                    meta.len()
                )));
            }
            self.verify()?;
            Ok(file)
        })
    }
    pub fn export(&self, images: &[(&str, File)]) -> Result<()> {
        // Create every output exclusively before copying any data. On any error,
        // remove only the names this operation created through the pinned dirfd.
        as_peer(self.peer, || {
            self.verify()?;
            let mut outputs = Vec::new();
            let result = (|| {
                for (name, _) in images {
                    self.verify()?;
                    let file = beneath(
                        self.dir.as_raw_fd(),
                        name,
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )?;
                    outputs.push((*name, file));
                }
                for ((_, source), (_, dest)) in images.iter().zip(&outputs) {
                    self.verify()?;
                    reflink(source, dest)?;
                    dest.sync_all()?;
                }
                self.verify()
            })();
            if result.is_err() {
                for (name, _) in &outputs {
                    let name = cstr(name.as_bytes())?;
                    if unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
            }
            result
        })
    }
}
pub fn reflink(source: &File, dest: &File) -> Result<()> {
    // There is deliberately no byte-copy fallback: images and snapshots are on
    // the configured XFS volume and the performance contract depends on CoW.
    if unsafe {
        libc::ioctl(
            dest.as_raw_fd(),
            0x40049409 as libc::c_ulong,
            source.as_raw_fd(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn trusted_file(path: &Path) -> Result<File> {
    // Config paths are root-controlled, but reject writable ancestors as well:
    // ownership of a final inode does not prevent replacing its directory entry.
    let mut current = std::path::PathBuf::from("/");
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            current.push(name);
            let meta = std::fs::symlink_metadata(&current).map_err(|error| {
                Error(format!(
                    "cannot inspect helper input {}: {error}",
                    current.display()
                ))
            })?;
            trusted(&current, &meta)?;
        } else if !matches!(component, std::path::Component::RootDir) {
            return Err(Error(format!(
                "trusted path must be absolute without traversal: {}",
                path.display()
            )));
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            Error(format!(
                "cannot open trusted helper input {}: {error}",
                path.display()
            ))
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error(format!(
            "trusted input is not a regular file: path={} uid={} gid={} mode={:04o}",
            path.display(),
            metadata.uid(),
            metadata.gid(),
            metadata.mode() & 0o7777
        )));
    }
    Ok(file)
}
pub fn trusted(path: &Path, meta: &Metadata) -> Result<()> {
    if meta.uid() != 0 || meta.mode() & 0o022 != 0 || meta.file_type().is_symlink() {
        return Err(Error(format!(
            "helper input or directory must be root-owned, non-writable and not a symlink: path={} uid={} gid={} mode={:04o} symlink={}",
            path.display(), meta.uid(), meta.gid(), meta.mode() & 0o7777, meta.file_type().is_symlink()
        )));
    }
    Ok(())
}
pub fn chown(file: &File, uid: u32) -> Result<()> {
    if unsafe { libc::fchown(file.as_raw_fd(), uid, uid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn bind_readonly(source: &File, target: &Path) -> Result<()> {
    let source = cstr(format!("/proc/self/fd/{}", source.as_raw_fd()).as_bytes())?;
    let target = cstr(target.as_os_str().as_bytes())?;
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND
                | libc::MS_REMOUNT
                | libc::MS_RDONLY
                | libc::MS_NOSUID
                | libc::MS_NODEV
                | libc::MS_NOEXEC,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn unmount(path: &Path) -> Result<()> {
    let path = cstr(path.as_os_str().as_bytes())?;
    if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
        let err = std::io::Error::last_os_error();
        if !matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOENT)) {
            return Err(err.into());
        }
    }
    Ok(())
}
