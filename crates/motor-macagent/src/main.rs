#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("motor-macagent runs only in a macOS job VM");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    guest::run().await
}

#[cfg(target_os = "macos")]
mod guest {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};
    use tokio::io::unix::AsyncFd;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    pub async fn run() -> io::Result<()> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "agent must read nonce as root",
            ));
        }
        let mut nonce_device = None;
        let mut port = motor_macagent::VSOCK_PORT;
        let mut args = std::env::args_os().skip(1);
        while let Some(arg) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| io::Error::other("missing option value"))?;
            match arg.to_str() {
                Some("--nonce-device") => nonce_device = Some(PathBuf::from(value)),
                Some("--port") => {
                    port = value
                        .to_str()
                        .and_then(|s| s.parse().ok())
                        .filter(|p| *p != 0)
                        .ok_or_else(|| io::Error::other("invalid vsock port"))?;
                }
                _ => return Err(io::Error::other("unknown option")),
            }
        }
        let boot = if let Some(path) = nonce_device {
            motor_macagent::read_boot_config(open_disk(&path)?)?
        } else {
            let mut disks = Vec::new();
            for entry in std::fs::read_dir("/dev")? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(suffix) = name.to_str().and_then(|n| n.strip_prefix("rdisk")) else {
                    continue;
                };
                if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let Ok(disk) = open_disk(&entry.path()) else {
                    continue;
                };
                disks.push(disk);
            }
            motor_macagent::find_boot_config(disks)?
        };
        motor_macagent::mounts::mount_native(&boot.mounts)?;
        let stream = Vsock::connect(port).await?;
        motor_macagent::serve_guest(
            stream,
            boot.nonce,
            PathBuf::from(motor_macagent::CARGO_HOME),
        )
        .await
    }

    fn open_disk(path: &Path) -> io::Result<std::fs::File> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.uid() != 0 || !metadata.file_type().is_char_device() {
            return Err(io::Error::other("nonce must be a root-owned raw disk"));
        }
        let job_gid = unsafe {
            let job = libc::getpwnam(c"motorjob".as_ptr());
            if job.is_null() || (*job).pw_uid == 0 || (*job).pw_gid == 0 {
                return Err(io::Error::other("unprivileged motorjob account missing"));
            }
            (*job).pw_gid
        };
        let readable = |metadata: &std::fs::Metadata| {
            metadata.mode() & 0o004 != 0
                || (metadata.gid() == job_gid && metadata.mode() & 0o040 != 0)
        };
        if readable(&metadata) {
            return Err(io::Error::other("motorjob can read the raw nonce device"));
        }
        if path.parent() == Some(Path::new("/dev"))
            && let Some(block_name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix('r'))
        {
            let block = std::fs::symlink_metadata(Path::new("/dev").join(block_name))?;
            if block.uid() != 0 || !block.file_type().is_block_device() || readable(&block) {
                return Err(io::Error::other(
                    "nonce block device is not protected from motorjob",
                ));
            }
        }
        Ok(file)
    }

    struct Vsock(AsyncFd<OwnedFd>);
    impl Vsock {
        async fn connect(port: u32) -> io::Result<Self> {
            let raw = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let yes: libc::c_int = 1;
            if unsafe {
                libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) != 0
                    || libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK) != 0
                    || libc::setsockopt(
                        raw,
                        libc::SOL_SOCKET,
                        libc::SO_NOSIGPIPE,
                        (&yes as *const libc::c_int).cast(),
                        std::mem::size_of_val(&yes) as _,
                    ) != 0
            } {
                return Err(io::Error::last_os_error());
            }
            let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
            address.svm_len = std::mem::size_of_val(&address) as _;
            address.svm_family = libc::AF_VSOCK as _;
            address.svm_port = port;
            address.svm_cid = libc::VMADDR_CID_HOST;
            let result = unsafe {
                libc::connect(
                    raw,
                    (&address as *const libc::sockaddr_vm).cast(),
                    std::mem::size_of_val(&address) as _,
                )
            };
            if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(io::Error::last_os_error());
            }
            let socket = Self(AsyncFd::new(fd)?);
            if result != 0 {
                let ready = socket.0.writable().await?;
                let mut error: libc::c_int = 0;
                let mut length = std::mem::size_of_val(&error) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        raw,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&mut error as *mut libc::c_int).cast(),
                        &mut length,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error));
                }
                // Keep the write readiness: the connected socket is writable,
                // and with edge-triggered kqueue no new event arrives while it
                // stays writable, so clearing it here stalled the first write.
                drop(ready);
            }
            Ok(socket)
        }
    }

    impl AsyncRead for Vsock {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                let mut ready = ready!(self.0.poll_read_ready(cx))?;
                let bytes = buf.initialize_unfilled();
                match ready.try_io(|fd| {
                    let count = unsafe {
                        libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len())
                    };
                    if count < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(count as usize)
                    }
                }) {
                    Ok(result) => {
                        buf.advance(result?);
                        return Poll::Ready(Ok(()));
                    }
                    Err(_) => continue,
                }
            }
        }
    }

    impl AsyncWrite for Vsock {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            loop {
                let mut ready = ready!(self.0.poll_write_ready(cx))?;
                match ready.try_io(|fd| {
                    let count =
                        unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
                    if count < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(count as usize)
                    }
                }) {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            let result = unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_WR) };
            Poll::Ready(if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            })
        }
    }
}
