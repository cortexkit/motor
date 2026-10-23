//! Descriptor-relative host tree operations. Guest links are copied, never followed.
use super::Control;
use crate::storage::invalid;
#[cfg(not(target_os = "macos"))]
use std::os::unix::fs::PermissionsExt;
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Component, Path},
};

pub(super) struct Dir(File);
fn c(value: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(invalid)
}
fn cv(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
impl Dir {
    pub(super) fn sync_all(&self) -> io::Result<()> {
        for name in self.names()? {
            match self.kind(&name)? {
                libc::S_IFDIR => self.child(&name, false)?.sync_all()?,
                libc::S_IFREG => self.file(&name, false)?.sync_all()?,
                _ => {}
            }
        }
        self.0.sync_all()
    }
    pub(super) fn publish(
        &self,
        temporary: &std::ffi::OsStr,
        store: &std::ffi::OsStr,
    ) -> io::Result<()> {
        let from = c(temporary)?;
        let to = c(store)?;
        if self.contains(store)? {
            self.child(store, false)?;
            #[cfg(target_os = "macos")]
            cv(unsafe {
                libc::renameatx_np(
                    self.0.as_raw_fd(),
                    from.as_ptr(),
                    self.0.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_SWAP,
                )
            })?;
            #[cfg(target_os = "linux")]
            cv(unsafe {
                libc::renameat2(
                    self.0.as_raw_fd(),
                    from.as_ptr(),
                    self.0.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            })?;
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            return Err(invalid("atomic target exchange is unsupported"));
        } else {
            cv(unsafe {
                libc::renameat(
                    self.0.as_raw_fd(),
                    from.as_ptr(),
                    self.0.as_raw_fd(),
                    to.as_ptr(),
                )
            })?;
        }
        self.0.sync_all()
    }
    pub(super) fn open(path: &Path, create: bool) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("host tree must be absolute"));
        }
        let fd = cv(unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        let mut dir = Self(unsafe { File::from_raw_fd(fd) });
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => dir = dir.child(name, create)?,
                _ => return Err(invalid("non-normal host path")),
            }
        }
        Ok(dir)
    }
    pub(super) fn child(&self, name: &std::ffi::OsStr, create: bool) -> io::Result<Self> {
        let name = c(name)?;
        let open = || unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        let mut fd = open();
        if fd < 0 {
            let error = io::Error::last_os_error();
            if !create || error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
            let result = unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o700) };
            if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                cv(result)?;
            }
            fd = open();
        }
        Ok(Self(unsafe { File::from_raw_fd(cv(fd)?) }))
    }
    pub(super) fn file(&self, name: &std::ffi::OsStr, create: bool) -> io::Result<File> {
        let name = c(name)?;
        let flags = if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        };
        let fd = cv(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600,
            )
        })?;
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(invalid("host file is not regular"));
        }
        Ok(file)
    }
    pub(super) fn names(&self) -> io::Result<Vec<OsString>> {
        let fd = cv(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
        unsafe {
            libc::closedir(stream);
        }
        names.sort();
        Ok(names)
    }
    pub(super) fn kind(&self, name: &std::ffi::OsStr) -> io::Result<libc::mode_t> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        cv(unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                c(name)?.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        Ok(unsafe { stat.assume_init() }.st_mode & libc::S_IFMT)
    }
    pub(super) fn contains(&self, name: &std::ffi::OsStr) -> io::Result<bool> {
        match self.kind(name) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub(super) fn link(&self, name: &std::ffi::OsStr) -> io::Result<OsString> {
        let mut buffer = vec![0; 65536];
        let n = unsafe {
            libc::readlinkat(
                self.0.as_raw_fd(),
                c(name)?.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize == buffer.len() {
            return Err(invalid("symlink target too long"));
        }
        buffer.truncate(n as usize);
        Ok(OsString::from_vec(buffer))
    }
    pub(super) fn remove(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        let kind = match self.kind(name) {
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if kind == libc::S_IFDIR {
            let child = self.child(name, false)?;
            for entry in child.names()? {
                child.remove(&entry)?;
            }
        }
        cv(unsafe {
            libc::unlinkat(
                self.0.as_raw_fd(),
                c(name)?.as_ptr(),
                if kind == libc::S_IFDIR {
                    libc::AT_REMOVEDIR
                } else {
                    0
                },
            )
        })?;
        Ok(())
    }
    pub(super) fn text(&self, name: &std::ffi::OsStr) -> io::Result<String> {
        let mut text = String::new();
        self.file(name, false)?.read_to_string(&mut text)?;
        Ok(text)
    }
    pub(super) fn copy_inputs(&self, to: &Dir, control: &Control) -> io::Result<()> {
        for name in self.names()? {
            control.check()?;
            let text = name
                .to_str()
                .ok_or_else(|| invalid("non-UTF8 prep input"))?;
            if matches!(
                text,
                ".git"
                    | "node_modules"
                    | "target"
                    | ".cargo"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
                    | ".npmrc"
                    | "bunfig.toml"
            ) {
                continue;
            }
            let kind = self.kind(&name)?;
            if kind == libc::S_IFDIR {
                self.child(&name, false)?
                    .copy_inputs(&to.child(&name, true)?, control)?;
            } else if matches!(
                text,
                "Cargo.toml" | "Cargo.lock" | "package.json" | "bun.lock" | "bun.lockb"
            ) || text.ends_with(".rs")
            {
                if kind != libc::S_IFREG {
                    return Err(invalid(format!("symlink or special prep input: {text}")));
                }
                let mut output = to.file(&name, true)?;
                if !text.ends_with(".rs") {
                    let mut source = self.file(&name, false)?;
                    let mut buffer = [0; 65536];
                    loop {
                        control.check()?;
                        let n = source.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        output.write_all(&buffer[..n])?;
                    }
                }
                output.flush()?;
            }
        }
        Ok(())
    }
    pub(super) fn clone_into(&self, to: &Dir, control: &Control) -> io::Result<()> {
        for name in self.names()? {
            control.check()?;
            match self.kind(&name)? {
                libc::S_IFDIR => self
                    .child(&name, false)?
                    .clone_into(&to.child(&name, true)?, control)?,
                libc::S_IFREG => {
                    let source = self.file(&name, false)?;
                    #[cfg(target_os = "macos")]
                    {
                        unsafe extern "C" {
                            fn fclonefileat(
                                src: libc::c_int,
                                dst: libc::c_int,
                                name: *const libc::c_char,
                                flags: u32,
                            ) -> libc::c_int;
                        }
                        cv(unsafe {
                            fclonefileat(
                                source.as_raw_fd(),
                                to.0.as_raw_fd(),
                                c(&name)?.as_ptr(),
                                0,
                            )
                        })?;
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        let output = to.file(&name, true)?;
                        #[cfg(target_os = "linux")]
                        cv(unsafe {
                            libc::ioctl(
                                output.as_raw_fd(),
                                0x40049409 as libc::c_ulong,
                                source.as_raw_fd(),
                            )
                        })?;
                        cv(unsafe {
                            libc::fchmod(
                                output.as_raw_fd(),
                                source.metadata()?.permissions().mode(),
                            )
                        })?;
                        #[cfg(not(target_os = "linux"))]
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "clonefile requires macOS or Linux reflinks",
                        ));
                    }
                }
                libc::S_IFLNK => {
                    cv(unsafe {
                        libc::symlinkat(
                            c(&self.link(&name)?)?.as_ptr(),
                            to.0.as_raw_fd(),
                            c(&name)?.as_ptr(),
                        )
                    })?;
                }
                _ => {
                    return Err(invalid(format!(
                        "special file in clone: {}",
                        name.to_string_lossy()
                    )))
                }
            }
        }
        Ok(())
    }
}

pub(super) fn read(path: &Path) -> io::Result<String> {
    Dir::open(path.parent().ok_or_else(|| invalid("file parent"))?, false)?
        .text(path.file_name().ok_or_else(|| invalid("file name"))?)
}
pub(super) fn remove(path: &Path) -> io::Result<()> {
    Dir::open(
        path.parent().ok_or_else(|| invalid("remove parent"))?,
        false,
    )?
    .remove(path.file_name().ok_or_else(|| invalid("remove name"))?)
}
