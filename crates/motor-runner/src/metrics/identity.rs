use ed25519_dalek::SigningKey;
use rand::RngCore;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

#[cfg(unix)]
mod native {
    use super::*;
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };

    pub fn directory(path: &Path) -> io::Result<File> {
        let absolute = std::path::absolute(path)?;
        if absolute
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        {
            return Err(io::Error::other(
                "monitor storage cannot contain parent-directory components",
            ));
        }
        let mut directory = File::open("/")?;
        for part in absolute.components() {
            let std::path::Component::Normal(name) = part else {
                continue;
            };
            let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
            let parent = directory.as_raw_fd();
            if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } != 0
                && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe {
                libc::openat(
                    parent,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            directory.sync_all()?;
            directory = unsafe { File::from_raw_fd(fd) };
        }
        let meta = directory.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "monitor directory must be owned and mode 0700",
            ));
        }
        Ok(directory)
    }

    pub fn open(parent: &File, path: &Path, create: bool) -> io::Result<File> {
        let name = CString::new(
            path.file_name()
                .ok_or_else(|| io::Error::other("missing metrics filename"))?
                .as_bytes(),
        )
        .map_err(io::Error::other)?;
        let flags = libc::O_RDWR
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "monitor file must be a private, owned, singly-linked regular file",
            ));
        }
        Ok(file)
    }
}

#[cfg(windows)]
#[path = "identity_windows.rs"]
mod native;

fn open(path: &Path, create: bool) -> io::Result<(File, File)> {
    let parent = native::directory(
        path.parent()
            .ok_or_else(|| io::Error::other("metrics file needs a parent"))?,
    )?;
    let file = native::open(&parent, path, create)?;
    Ok((parent, file))
}

pub(super) fn key(path: &Path) -> io::Result<SigningKey> {
    let (parent, mut file) = match open(path, true) {
        Ok((parent, mut file)) => {
            let mut seed = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut seed);
            file.write_all(&seed)?;
            file.sync_all()?;
            sync_directory(&parent)?;
            file.rewind()?;
            (parent, file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open(path, false)?,
        Err(error) => return Err(error),
    };
    let _ = parent;
    let mut seed = Vec::new();
    (&mut file).take(33).read_to_end(&mut seed)?;
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| io::Error::other("monitor key must contain a raw 32-byte Ed25519 seed"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn sync_directory(directory: &File) -> io::Result<()> {
    #[cfg(unix)]
    directory.sync_all()?;
    // Windows files are flushed individually; directory handles cannot be
    // passed to FlushFileBuffers. Creation uses a protected, service-only
    // discretionary access control list (DACL).
    #[cfg(windows)]
    let _ = directory;
    Ok(())
}

pub(super) struct Counter {
    file: File,
    last: u64,
    failed: bool,
}

impl Counter {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let (parent, mut file) = match open(path, true) {
            Ok(pair) => pair,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open(path, false)?,
            Err(error) => return Err(error),
        };
        fs2::FileExt::try_lock_exclusive(&file)?;
        file.sync_all()?;
        sync_directory(&parent)?;
        if file.metadata()?.len() % 8 != 0 {
            return Err(io::Error::other(
                "incomplete monitor counter journal; refusing counter reuse",
            ));
        }
        let mut last = 0;
        loop {
            let mut bytes = [0; 8];
            if file.read(&mut bytes[..1])? == 0 {
                break;
            }
            file.read_exact(&mut bytes[1..])?;
            let value = u64::from_le_bytes(bytes);
            if value <= last || value > motor_lease::MAX_SAFE_INTEGER {
                return Err(io::Error::other("invalid monitor counter journal"));
            }
            last = value;
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            last,
            failed: false,
        })
    }

    pub(super) fn reserve(&mut self, now: u64) -> io::Result<u64> {
        if self.failed {
            return Err(io::Error::other("monitor counter persistence failed"));
        }
        let value = self
            .last
            .checked_add(1)
            .ok_or_else(|| io::Error::other("monitor counter exhausted"))?
            .max(now.saturating_mul(1000));
        if value > motor_lease::MAX_SAFE_INTEGER {
            return Err(io::Error::other("monitor counter exhausted"));
        }
        // A partial append is fatal for this process and the next startup;
        // no request can use an unflushed or ambiguously persisted counter.
        self.failed = true;
        self.file.write_all(&value.to_le_bytes())?;
        self.file.sync_all()?;
        self.last = value;
        self.failed = false;
        Ok(value)
    }
}
