use crate::{invalid, Layout, MAX_HANDSHAKE};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};

fn handshake(stream: &mut UnixStream) -> io::Result<(Vec<u8>, OwnedFd)> {
    let mut bytes = vec![0u8; MAX_HANDSHAKE];
    // Aligned storage is required by CMSG_FIRSTHDR/CMSG_NXTHDR.
    let mut control = [0usize; 16];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<i32>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(
                        *libc::CMSG_DATA(cmsg).cast::<i32>().add(i),
                    ));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || fds.len() != 1 {
        return Err(invalid(
            "expected exactly one fd in complete UFFD handshake",
        ));
    }
    bytes.truncate(n as usize);
    // SOCK_STREAM may split the JSON even though SCM_RIGHTS arrived in one message.
    loop {
        match serde_json::from_slice::<Vec<crate::Region>>(&bytes) {
            Ok(_) => break,
            Err(e) if e.is_eof() && bytes.len() < MAX_HANDSHAKE => {
                let mut chunk = [0; 4096];
                let available = chunk.len().min(MAX_HANDSHAKE - bytes.len());
                let n = stream.read(&mut chunk[..available])?;
                if n == 0 {
                    return Err(invalid("incomplete UFFD handshake"));
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(e) => return Err(invalid(&e.to_string())),
        }
    }
    Ok((bytes, fds.pop().unwrap()))
}

struct Mapping {
    pointer: *mut libc::c_void,
    size: usize,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pointer, self.size);
        }
    }
}
#[repr(C)]
struct UffdCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copied: i64,
}

fn copy_fault(
    layout: &Layout,
    pointer: *mut libc::c_void,
    address: u64,
    mut ioctl: impl FnMut(&mut UffdCopy) -> io::Result<()>,
) -> io::Result<()> {
    let copy = layout.locate(address)?;
    let mut request = UffdCopy {
        dst: copy.destination,
        src: pointer as u64 + copy.offset,
        len: copy.length,
        mode: 0,
        copied: 0,
    };
    match ioctl(&mut request) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        Err(e) => Err(e),
        Ok(()) if request.copied == copy.length as i64 => Ok(()),
        Ok(()) => Err(invalid("incomplete UFFDIO_COPY")),
    }
}

pub fn run(memory: File, listener: UnixListener) -> io::Result<()> {
    let metadata = memory.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("memory fd must be a regular file"));
    }
    let size = usize::try_from(metadata.len()).map_err(|_| invalid("memory too large"))?;
    let (mut connection, _) = listener.accept()?;
    let (bytes, uffd) = handshake(&mut connection)?;
    drop(connection);
    drop(listener);
    let layout = Layout::parse(&bytes, metadata.len())?;
    let pointer = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            memory.as_raw_fd(),
            0,
        )
    };
    if pointer == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let mapping = Mapping { pointer, size };
    let mut fd = libc::pollfd {
        fd: uffd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        if unsafe { libc::poll(&mut fd, 1, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(invalid("userfaultfd closed"));
        }
        let mut messages = [0u8; 32 * 128];
        let n = unsafe { libc::read(fd.fd, messages.as_mut_ptr().cast(), messages.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(
                e.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(e);
        }
        if n == 0 || n % 32 != 0 {
            return Err(invalid("partial or empty uffd_msg"));
        }
        for message in messages[..n as usize].as_chunks::<32>().0 {
            let flags = u64::from_ne_bytes(message[8..16].try_into().unwrap());
            if message[0] != 0x12 || flags & !1 != 0 {
                return Err(invalid("only missing-page faults are supported"));
            }
            let address = u64::from_ne_bytes(message[16..24].try_into().unwrap());
            copy_fault(&layout, mapping.pointer, address, |request| {
                // _IOWR(0xaa, 3, struct uffdio_copy), on x86_64 and aarch64 Linux.
                if unsafe { libc::ioctl(fd.fd, 0xc028aa03u32 as _, request) } < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            })?;
        }
    }
}

/// Exercise the actual UFFDIO_COPY path over an anonymous registered region.
/// This needs /dev/userfaultfd or permission for user-mode-only userfaultfd.
/// The slot's hugepage-backed, demand-fault latency is measured separately by
/// VM qualification; this measures the loader's 2 MiB copy syscall in isolation.
pub fn benchmark_copies(iterations: usize) -> io::Result<std::time::Duration> {
    use crate::{Region, PAGE_SIZE};
    if iterations == 0 {
        return Err(invalid("zero benchmark iterations"));
    }
    let raw = unsafe {
        libc::syscall(
            libc::SYS_userfaultfd,
            libc::O_CLOEXEC | libc::O_NONBLOCK | 1,
        )
    };
    let uffd = if raw >= 0 {
        unsafe { OwnedFd::from_raw_fd(raw as _) }
    } else {
        let device = File::open("/dev/userfaultfd")?;
        let fd = unsafe {
            libc::ioctl(
                device.as_raw_fd(),
                0xaa00u32 as _,
                libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { OwnedFd::from_raw_fd(fd) }
    };
    let mut api = [0xaau64, 0, 0];
    if unsafe { libc::ioctl(uffd.as_raw_fd(), 0xc018aa3fu32 as _, api.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = 2 * PAGE_SIZE as usize;
    let pointer = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if pointer == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let mapping = Mapping { pointer, size };
    let base = (mapping.pointer as u64 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    // uffdio_register: range(start,len), mode=MISSING, returned ioctls.
    let mut register = [base, PAGE_SIZE, 1, 0];
    if unsafe { libc::ioctl(uffd.as_raw_fd(), 0xc020aa00u32 as _, register.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let source = vec![0x5au8; PAGE_SIZE as usize];
    let layout = Layout::new(
        vec![Region {
            base_host_virt_addr: base,
            offset: 0,
            size: PAGE_SIZE,
            page_size: PAGE_SIZE,
        }],
        PAGE_SIZE,
    )?;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        copy_fault(
            &layout,
            source.as_ptr() as *mut _,
            base + PAGE_SIZE - 1,
            |r| {
                if unsafe { libc::ioctl(uffd.as_raw_fd(), 0xc028aa03u32 as _, r) } < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            },
        )?;
        let copied = unsafe { std::slice::from_raw_parts(base as *const u8, PAGE_SIZE as usize) };
        if copied != source {
            return Err(invalid("UFFD copy bytes differ"));
        }
        if unsafe { libc::madvise(base as *mut _, PAGE_SIZE as usize, libc::MADV_DONTNEED) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Region, PAGE_SIZE};
    use std::{io::Write, thread};
    fn send(stream: UnixStream, fds: &[i32], bytes: &[u8]) {
        let mut control = [0usize; 16];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as _) } as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as _) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                std::mem::size_of_val(fds),
            );
            assert_eq!(
                libc::sendmsg(stream.as_raw_fd(), &msg, 0),
                bytes.len() as isize
            );
        }
    }
    #[test]
    fn handshake_accepts_one_fd_and_fragmented_json_rejects_extra_fds() {
        let file = File::open("/dev/null").unwrap();
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let fd = file.as_raw_fd();
        let worker = thread::spawn(move || {
            send(sender.try_clone().unwrap(), &[fd], b"[");
            let mut sender = sender;
            sender.write_all(b"]").unwrap();
        });
        let (bytes, received) = handshake(&mut receiver).unwrap();
        assert_eq!(bytes, b"[]");
        assert!(
            unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0
        );
        worker.join().unwrap();
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        send(sender, &[file.as_raw_fd(), file.as_raw_fd()], b"[]");
        assert!(handshake(&mut receiver).is_err());
    }
    #[test]
    fn ioctl_uses_two_mib_and_checks_partial_copies() {
        let layout = Layout::new(
            vec![Region {
                base_host_virt_addr: 4 * PAGE_SIZE,
                offset: 0,
                size: 2 * PAGE_SIZE,
                page_size: PAGE_SIZE,
            }],
            2 * PAGE_SIZE,
        )
        .unwrap();
        let pointer = (16 * PAGE_SIZE) as *mut libc::c_void;
        copy_fault(&layout, pointer, 6 * PAGE_SIZE - 1, |r| {
            assert_eq!(r.dst, 5 * PAGE_SIZE);
            assert_eq!(r.src, 17 * PAGE_SIZE);
            assert_eq!(r.len, PAGE_SIZE);
            assert_eq!(r.mode, 0);
            r.copied = PAGE_SIZE as i64;
            Ok(())
        })
        .unwrap();
        assert!(copy_fault(&layout, pointer, 4 * PAGE_SIZE, |r| {
            r.copied = 4096;
            Ok(())
        })
        .is_err());
        copy_fault(&layout, pointer, 4 * PAGE_SIZE, |_| {
            Err(io::Error::from_raw_os_error(libc::EEXIST))
        })
        .unwrap();
        assert!(copy_fault(&layout, pointer, 6 * PAGE_SIZE, |_| panic!(
            "out-of-bounds copy reached ioctl"
        ))
        .is_err());
    }
}

#[cfg(all(test, feature = "linux-uffd-tests"))]
#[test]
fn real_uffd_copy_matches_every_byte_of_two_mib() {
    benchmark_copies(4).unwrap();
}

#[cfg(all(test, feature = "linux-sandbox-tests"))]
#[test]
fn static_loader_runs_in_empty_chroot_under_helper_seccomp() {
    use cortexkit_test_support::ScratchDir;
    use std::{
        ffi::CString,
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Command, Stdio},
    };
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "sandbox gate requires root inside a disposable container"
    );
    let exe = File::open(
        std::env::var("MOTOR_VM_LOADER_BIN")
            .expect("set MOTOR_VM_LOADER_BIN to a static Linux binary"),
    )
    .unwrap();
    // Leave room for the shared scratch-root prefix within sockaddr_un's path limit.
    let scratch = ScratchDir::new("mvl");
    let root = scratch.path().join("empty");
    std::fs::create_dir(&root).unwrap();
    let memory = File::create(scratch.path().join("memory")).unwrap();
    memory.set_len(crate::PAGE_SIZE).unwrap();
    drop(memory);
    let memory = File::open(scratch.path().join("memory")).unwrap();
    let listener = UnixListener::bind(scratch.path().join("uffd.sock")).unwrap();
    let exec_fd = exe.as_raw_fd();
    let mem_fd = memory.as_raw_fd();
    let listen_fd = listener.as_raw_fd();
    let root = CString::new(root.as_os_str().as_encoded_bytes()).unwrap();
    let argv = [
        CString::new("motor-vm-loader").unwrap(),
        CString::new("--memory-fd").unwrap(),
        CString::new(mem_fd.to_string()).unwrap(),
        CString::new("--listen-fd").unwrap(),
        CString::new(listen_fd.to_string()).unwrap(),
    ];
    let ptrs: Vec<_> = argv
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let argv_address = ptrs.as_ptr() as usize;
    let filter = motor_vm_helper::linux::process::loader_program(exec_fd);
    let mut cmd = Command::new("/unused");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_clear();
    unsafe {
        cmd.pre_exec(move || {
            for fd in [mem_fd, listen_fd] {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::chroot(root.as_ptr()) != 0
                || libc::chdir(c"/".as_ptr()) != 0
                || libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(200000) != 0
                || libc::setuid(200000) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            {
                return Err(io::Error::last_os_error());
            }
            motor_vm_helper::linux::process::install_loader_filter(&filter)?;
            let env = [std::ptr::null::<libc::c_char>()];
            libc::syscall(
                libc::SYS_execveat,
                exec_fd,
                c"".as_ptr(),
                argv_address as *const *const libc::c_char,
                env.as_ptr(),
                libc::AT_EMPTY_PATH,
            );
            Err(io::Error::last_os_error())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop((argv, ptrs, exe, memory, listener));
    let mut connection = UnixStream::connect(scratch.path().join("uffd.sock")).unwrap();
    let file = File::open("/dev/null").unwrap();
    let bytes = format!(
        "[{{\"base_host_virt_addr\":{},\"offset\":0,\"size\":{},\"page_size\":{}}}]",
        crate::PAGE_SIZE,
        crate::PAGE_SIZE,
        crate::PAGE_SIZE
    );
    let mut control = [0usize; 4];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
    let sent = unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        *libc::CMSG_DATA(c).cast::<i32>() = file.as_raw_fd();
        libc::sendmsg(connection.as_raw_fd(), &msg, libc::MSG_NOSIGNAL)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("sandbox loader timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.signal(), None, "sandbox killed loader: {stderr}");
    assert_eq!(sent, bytes.len() as isize);
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contains("partial or empty uffd_msg"),
        "did not reach UFFD read loop: {stderr}"
    );
    // Keep the socket live until the loader completes its receive/read path.
    let _ = &mut connection;
}
