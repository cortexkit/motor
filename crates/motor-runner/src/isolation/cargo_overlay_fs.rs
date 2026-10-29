use std::{
    ffi::{CStr, CString, OsStr, OsString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Component, Path},
};

pub(super) struct Directory(File);

pub(super) struct Entry {
    pub name: OsString,
    pub stat: libc::stat,
}

impl Directory {
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("cache directory must be absolute"));
        }
        let root = CString::new("/").unwrap();
        let mut directory = Self(open_at(
            libc::AT_FDCWD,
            &root,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?);
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => directory = directory.child(name)?,
                _ => return Err(io::Error::other("unsafe cache directory component")),
            }
        }
        Ok(directory)
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn stat(&self) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(self.fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    pub fn child(&self, name: &OsStr) -> io::Result<Self> {
        Ok(Self(open_at(
            self.fd(),
            &name_c(name)?,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?))
    }

    pub fn ensure_child(&self, name: &OsStr) -> io::Result<Self> {
        let name_c = name_c(name)?;
        if unsafe { libc::mkdirat(self.fd(), name_c.as_ptr(), 0o755) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        self.child(name)
    }

    pub fn file(&self, name: &OsStr) -> io::Result<File> {
        open_at(self.fd(), &name_c(name)?, libc::O_RDONLY | libc::O_NONBLOCK)
    }

    pub fn stat_at(&self, name: &OsStr) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe {
            libc::fstatat(
                self.fd(),
                name_c(name)?.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    pub fn entries(&self) -> io::Result<Entries> {
        let fd = open_at(self.fd(), c".", libc::O_RDONLY | libc::O_DIRECTORY)?.into_raw_fd();
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        Ok(Entries(stream))
    }

    pub fn read_link(&self, name: &OsStr) -> io::Result<OsString> {
        let name = name_c(name)?;
        let mut bytes = vec![0; 256];
        loop {
            let size = unsafe {
                libc::readlinkat(
                    self.fd(),
                    name.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if size < 0 {
                return Err(io::Error::last_os_error());
            }
            if (size as usize) < bytes.len() {
                bytes.truncate(size as usize);
                return Ok(OsString::from_vec(bytes));
            }
            if bytes.len() >= 65536 {
                return Err(io::Error::other("cache symlink is too long"));
            }
            bytes.resize(bytes.len() * 2, 0);
        }
    }
}

fn name_c(name: &OsStr) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.as_bytes().contains(&b'/') {
        return Err(io::Error::other("unsafe cache entry name"));
    }
    CString::new(name.as_bytes()).map_err(io::Error::other)
}

fn open_at(parent: RawFd, name: &CStr, flags: i32) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) struct Entries(*mut libc::DIR);

impl Entries {
    pub fn next(&mut self, directory: &Directory) -> io::Result<Option<Entry>> {
        loop {
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = OsString::from_vec(bytes.to_vec());
            let stat = directory.stat_at(&name)?;
            return Ok(Some(Entry { name, stat }));
        }
    }
}

impl Drop for Entries {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

pub(super) fn same_inode(a: &libc::stat, b: &libc::stat) -> bool {
    (a.st_dev, a.st_ino, a.st_mode & libc::S_IFMT) == (b.st_dev, b.st_ino, b.st_mode & libc::S_IFMT)
}
