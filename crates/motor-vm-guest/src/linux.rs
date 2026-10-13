use crate::{
    fetch::Policy, invalid, read_frame, write_frame, Frame, Mode, Start, Termination, CARGO_HOME,
    OUTPUT_CHUNK, TARGET,
};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File},
    io::{self, Read},
    net::{Shutdown, TcpListener},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const TREE: &str = "/run/motor-vm/tree";
const FETCH_CWD: &str = "/run/motor-vm/fetch";
pub const IMAGE_CARGO: &str = "/opt/motor/bin/cargo";
pub const IMAGE_GIT: &str = "/usr/bin/git";
fn cvt(n: i32) -> io::Result<()> {
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| invalid("NUL in path"))
}

fn vsock_socket() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    cvt(fd)?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn address(cid: u32, port: u32) -> libc::sockaddr_vm {
    let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    address.svm_family = libc::AF_VSOCK as u16;
    address.svm_cid = cid;
    address.svm_port = port;
    address
}
pub fn connect_proxy(port: u32) -> io::Result<File> {
    let fd = vsock_socket()?;
    let address = address(libc::VMADDR_CID_HOST, port);
    cvt(unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_vm).cast(),
            std::mem::size_of_val(&address) as _,
        )
    })?;
    Ok(File::from(fd))
}

const MAX_RELAY_CONNECTIONS: usize = 512;

#[cfg(all(test, feature = "linux-root-tests"))]
#[path = "root_tests.rs"]
mod root_tests;

struct RelayConnection {
    sockets: [OwnedFd; 2],
    upload: thread::JoinHandle<io::Result<u64>>,
    download: thread::JoinHandle<io::Result<u64>>,
}
impl RelayConnection {
    fn finished(&self) -> bool {
        self.upload.is_finished() && self.download.is_finished()
    }
    fn shutdown(&self) {
        for fd in &self.sockets {
            unsafe {
                libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR);
            }
        }
    }
    fn join(self) -> io::Result<()> {
        let upload = self
            .upload
            .join()
            .map_err(|_| invalid("relay copy worker panicked"))
            .and_then(|result| result.map(|_| ()));
        let download = self
            .download
            .join()
            .map_err(|_| invalid("relay copy worker panicked"))
            .and_then(|result| result.map(|_| ()));
        upload.and(download)
    }
}
struct Relay {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<io::Result<()>>>,
}
impl Relay {
    fn start(port: u32) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 3128))?;
        listener.set_nonblocking(true)?;
        Self::start_with(listener, move || connect_proxy(port))
    }
    fn start_with(
        listener: TcpListener,
        connect: impl Fn() -> io::Result<File> + Send + 'static,
    ) -> io::Result<Self> {
        Self::start_with_capacity(listener, connect, MAX_RELAY_CONNECTIONS)
    }
    fn start_with_capacity(
        listener: TcpListener,
        connect: impl Fn() -> io::Result<File> + Send + 'static,
        capacity: usize,
    ) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut connections = Vec::<RelayConnection>::new();
            let mut transfer_error = None;
            let result = (|| {
                while !stopped.load(Ordering::Acquire) {
                    // Release completed transports and their descriptors before checking capacity.
                    let mut i = 0;
                    while i < connections.len() {
                        if connections[i].finished() {
                            if let Err(error) = connections.swap_remove(i).join() {
                                transfer_error.get_or_insert(error);
                            }
                        } else {
                            i += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((tcp, _)) => {
                            if connections.len() >= capacity {
                                return Err(invalid("proxy connection bound exceeded"));
                            }
                            let vsock = connect()?;
                            let sockets = [tcp.try_clone()?.into(), vsock.try_clone()?.into()];
                            let mut tcp_read = tcp.try_clone()?;
                            let mut vsock_write = vsock.try_clone()?;
                            let upload = thread::spawn(move || {
                                let result = io::copy(&mut tcp_read, &mut vsock_write);
                                unsafe {
                                    libc::shutdown(vsock_write.as_raw_fd(), libc::SHUT_WR);
                                    if result.is_err() {
                                        libc::shutdown(tcp_read.as_raw_fd(), libc::SHUT_RDWR);
                                        libc::shutdown(vsock_write.as_raw_fd(), libc::SHUT_RDWR);
                                    }
                                }
                                result
                            });
                            let download = thread::spawn(move || {
                                let mut tcp = tcp;
                                let mut vsock = vsock;
                                let result = io::copy(&mut vsock, &mut tcp);
                                let _ = tcp.shutdown(Shutdown::Write);
                                if result.is_err() {
                                    let _ = tcp.shutdown(Shutdown::Both);
                                    unsafe {
                                        libc::shutdown(vsock.as_raw_fd(), libc::SHUT_RDWR);
                                    }
                                }
                                result
                            });
                            connections.push(RelayConnection {
                                sockets,
                                upload,
                                download,
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            })();
            for connection in &connections {
                connection.shutdown();
            }
            for connection in connections {
                if let Err(error) = connection.join() {
                    transfer_error.get_or_insert(error);
                }
            }
            result.and(transfer_error.map_or(Ok(()), Err))
        });
        Ok(Self {
            stop,
            thread: Some(worker),
        })
    }
    fn finish(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| invalid("relay panicked"))?
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.finish();
        }
    }
}

pub fn restore_clock_entropy(start: &Start) -> io::Result<()> {
    let time = libc::timespec {
        tv_sec: start.realtime_seconds,
        tv_nsec: start.realtime_nanoseconds as _,
    };
    cvt(unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) })?;
    #[repr(C)]
    struct Entropy {
        bits: i32,
        size: i32,
        bytes: [u8; 32],
    }
    let random = File::options().read(true).write(true).open("/dev/random")?;
    let entropy = Entropy {
        bits: 256,
        size: 32,
        bytes: start.entropy,
    };
    // RNDADDENTROPY credits the supplied host randomness; RNDRESEEDCRNG prevents
    // restored snapshots from continuing with an identical CRNG state.
    cvt(unsafe { libc::ioctl(random.as_raw_fd(), 0x40085203u32 as _, &entropy) })?;
    cvt(unsafe { libc::ioctl(random.as_raw_fd(), 0x5207u32 as _, 0) })
}

fn mkdir_no_symlinks(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::from("/");
    for part in path.components().skip(1) {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => return Err(invalid("mount path is not a real directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
struct Mounts(Vec<PathBuf>);
impl Mounts {
    fn mount(&mut self, source: &Path, target: &Path, bind: bool) -> io::Result<()> {
        mkdir_no_symlinks(target)?;
        let source = cstr(source)?;
        let dest = cstr(target)?;
        let (ty, flags) = if bind {
            (std::ptr::null(), libc::MS_BIND)
        } else {
            (c"ext4".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV)
        };
        cvt(unsafe { libc::mount(source.as_ptr(), dest.as_ptr(), ty, flags, std::ptr::null()) })?;
        self.0.push(target.to_owned());
        Ok(())
    }
    fn finish(&mut self) -> io::Result<()> {
        for mount in self.0.iter().rev() {
            let file = File::open(mount)?;
            cvt(unsafe { libc::syncfs(file.as_raw_fd()) })?;
            // The sync descriptor pins this mount; close it before attempting a strict unmount.
            drop(file);
            cvt(unsafe { libc::umount2(cstr(mount)?.as_ptr(), 0) })?;
        }
        self.0.clear();
        Ok(())
    }
}
impl Drop for Mounts {
    fn drop(&mut self) {
        for mount in self.0.iter().rev() {
            if let Ok(path) = cstr(mount) {
                unsafe {
                    libc::umount2(path.as_ptr(), 0);
                }
            }
        }
    }
}

fn environment(start: &Start) -> BTreeMap<String, String> {
    let mut env = match start.mode {
        Mode::Job { .. } => start.env.clone(),
        Mode::Fetch { .. } => BTreeMap::new(),
    };
    for key in [
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "TZ",
        "RUSTUP_HOME",
    ] {
        env.remove(key);
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_owned(), value);
        }
    }
    env.insert("CARGO_HOME".into(), CARGO_HOME.into());
    env.insert("CARGO_TARGET_DIR".into(), TARGET.into());
    env.insert("CARGO_BUILD_JOBS".into(), start.vcpus.to_string());
    env.remove("RUSTC_WRAPPER");
    env.remove("RUSTC_WORKSPACE_WRAPPER");
    env.insert(
        "CARGO_NET_OFFLINE".into(),
        if start.proxy_port.is_some() {
            "false"
        } else {
            "true"
        }
        .into(),
    );
    if start.proxy_port.is_some() {
        env.insert("CARGO_HTTP_PROXY".into(), "http://127.0.0.1:3128".into());
    } else {
        env.remove("CARGO_HTTP_PROXY");
    }
    env
}

fn command(start: &Start) -> io::Result<(Command, Option<Policy>)> {
    let mut env = environment(start);
    let (mut command, policy) = match &start.mode {
        Mode::Job { command: argv, cwd } => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(cwd);
            (command, None)
        }
        Mode::Fetch {
            manifest,
            git_directories,
        } => {
            mkdir_no_symlinks(Path::new(FETCH_CWD))?;
            let manifest = fs::canonicalize(manifest)?;
            let lock_path = manifest
                .parent()
                .ok_or_else(|| invalid("manifest parent"))?
                .join("Cargo.lock");
            let config = fs::read("/etc/motor-vm/cargo-config.toml")?;
            fs::write(Path::new(CARGO_HOME).join("config.toml"), &config)?;
            let policy =
                Policy::from_lock(&fs::read_to_string(lock_path)?, git_directories, config)?;
            env.insert("CARGO_NET_GIT_FETCH_WITH_CLI".into(), "true".into());
            env.insert("GIT_CONFIG_COUNT".into(), git_directories.len().to_string());
            for (i, (source, directory)) in git_directories.iter().enumerate() {
                let url = source
                    .trim_start_matches("git+")
                    .split(['?', '#'])
                    .next()
                    .unwrap();
                env.insert(
                    format!("GIT_CONFIG_KEY_{i}"),
                    format!("url.file://{CARGO_HOME}/git/db/{directory}.insteadOf"),
                );
                env.insert(format!("GIT_CONFIG_VALUE_{i}"), url.to_owned());
            }
            (fetch_command(&manifest), Some(policy))
        }
    };
    command
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            cvt(libc::setgroups(0, std::ptr::null()))?;
            cvt(libc::setgid(1000))?;
            cvt(libc::setuid(1000))?;
            cvt(libc::setsid())?;
            cvt(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))
        });
    }
    Ok((command, policy))
}

fn fetch_command(manifest: &Path) -> Command {
    let mut command = Command::new(IMAGE_CARGO);
    command
        .current_dir(FETCH_CWD)
        .args(["fetch", "--locked", "--manifest-path"])
        .arg(manifest);
    command
}

fn has_job_uid(pid: i32) -> io::Result<bool> {
    let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(s) => s,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .is_some_and(|l| l.split_whitespace().any(|u| u == "1000")))
}
fn kill_jobs() -> io::Result<()> {
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if !has_job_uid(pid)? {
            continue;
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(e);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if !has_job_uid(pid)? {
            continue;
        }
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } < 0
        {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(e);
            }
        }
    }
    Ok(())
}
fn reap_jobs() -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        kill_jobs()?;
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        if Instant::now() >= deadline {
            return Err(invalid("guest processes did not reap"));
        }
        if pid == 0 {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Poll both pipes in one supervisor thread. The child can never hold a stream
/// lock while waiting for its other pipe to drain.
pub fn supervise(
    child: &mut Child,
    stream: &mut impl io::Write,
    mut cleanup: impl FnMut() -> io::Result<()>,
) -> io::Result<Termination> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("missing stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| invalid("missing stderr"))?;
    let mut fds = [stdout.as_raw_fd(), stderr.as_raw_fd()].map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    for fd in &fds {
        let flags = unsafe { libc::fcntl(fd.fd, libc::F_GETFL) };
        cvt(flags)?;
        cvt(unsafe { libc::fcntl(fd.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    }
    let mut status = None;
    while status.is_none() || fds.iter().any(|f| f.fd >= 0) {
        if status.is_none() {
            if let Some(s) = child.try_wait()? {
                status = Some(s);
                cleanup()?;
            }
        }
        let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, 20) };
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
        for (i, fd) in fds.iter_mut().enumerate() {
            if fd.fd < 0 || fd.revents == 0 {
                continue;
            }
            let mut bytes = vec![0; OUTPUT_CHUNK];
            let read = if i == 0 {
                stdout.read(&mut bytes)
            } else {
                stderr.read(&mut bytes)
            };
            match read {
                Ok(0) => fd.fd = -1,
                Ok(n) => {
                    bytes.truncate(n);
                    write_frame(
                        stream,
                        &if i == 0 {
                            Frame::Stdout { bytes }
                        } else {
                            Frame::Stderr { bytes }
                        },
                    )?;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    }
    let status = status.unwrap();
    if let Some(code) = status.code() {
        Ok(Termination::Exit { code })
    } else if let Some(signal) = status.signal() {
        Ok(Termination::Signal { signal })
    } else {
        Err(invalid("invalid wait status"))
    }
}

fn run(start: Start, stream: &mut File) -> io::Result<()> {
    start.validate()?;
    restore_clock_entropy(&start)?;
    let mut mounts = Mounts(Vec::new());
    mounts.mount(Path::new("/dev/vdb"), Path::new(TREE), false)?;
    mounts.mount(Path::new("/dev/vdc"), Path::new(TARGET), false)?;
    mounts.mount(Path::new("/dev/vdd"), Path::new(CARGO_HOME), false)?;
    for tree in &start.trees {
        let source = Path::new(TREE).join(&tree.subdirectory);
        if fs::canonicalize(&source)? != source {
            return Err(invalid("tree source contains symlink"));
        }
        mounts.mount(&source, &tree.destination, true)?;
    }
    let mut relay = start.proxy_port.map(Relay::start).transpose()?;
    let (mut command, policy) = command(&start)?;
    let mut child = command.spawn()?;
    let result = supervise(&mut child, stream, reap_jobs);
    // A broken host stream is also an abnormal end: kill the child before any
    // cleanup, and never emit a clean status on that path.
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    reap_jobs()?;
    if let Some(relay) = &mut relay {
        relay.finish()?;
    }
    let termination = result?;
    complete(termination, stream, |termination| {
        if let Some(policy) = policy {
            if termination != &(Termination::Exit { code: 0 }) {
                return Err(invalid("cargo fetch failed"));
            }
            policy.validate(Path::new(CARGO_HOME), Path::new(IMAGE_GIT))?;
        }
        mounts.finish()
    })
}

fn complete(
    termination: Termination,
    stream: &mut impl io::Write,
    finish: impl FnOnce(&Termination) -> io::Result<()>,
) -> io::Result<()> {
    finish(&termination)?;
    write_frame(
        stream,
        &Frame::Status {
            termination,
            clean: true,
        },
    )
}

pub fn serve(port: u32) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(invalid("guest supervisor must be root"));
    }
    cvt(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) })?;
    let listener = vsock_socket()?;
    let address = address(libc::VMADDR_CID_ANY, port);
    cvt(unsafe {
        libc::bind(
            listener.as_raw_fd(),
            (&address as *const libc::sockaddr_vm).cast(),
            std::mem::size_of_val(&address) as _,
        )
    })?;
    cvt(unsafe { libc::listen(listener.as_raw_fd(), 1) })?;
    // Snapshot here: data drives remain unmounted until a start frame arrives.
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    cvt(fd)?;
    let mut stream = unsafe { File::from_raw_fd(fd) };
    let start = read_frame(&mut stream)?;
    run(start, &mut stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn start() -> Start {
        Start {
            realtime_seconds: 1,
            realtime_nanoseconds: 0,
            entropy: [9; 32],
            trees: vec![crate::TreeMount {
                subdirectory: "workspace".into(),
                destination: "/job/workspace".into(),
            }],
            env: BTreeMap::new(),
            vcpus: 2,
            proxy_port: None,
            mode: Mode::Job {
                command: vec!["/bin/sh".into()],
                cwd: "/job/workspace".into(),
            },
        }
    }
    #[test]
    fn streams_are_separate_and_wait_status_preserves_exit_and_signal() {
        for (script, expected) in [
            (
                "printf out; printf err >&2; exit 3",
                Termination::Exit { code: 3 },
            ),
            ("kill -TERM $$", Termination::Signal { signal: 15 }),
        ] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", script])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut wire = Vec::new();
            let mut cleaned = 0;
            assert_eq!(
                supervise(&mut child, &mut wire, || {
                    cleaned += 1;
                    Ok(())
                })
                .unwrap(),
                expected
            );
            assert_eq!(cleaned, 1);
            let mut reader = wire.as_slice();
            let mut out = Vec::new();
            let mut err = Vec::new();
            while !reader.is_empty() {
                match read_frame(&mut reader).unwrap() {
                    Frame::Stdout { bytes } => out.extend(bytes),
                    Frame::Stderr { bytes } => err.extend(bytes),
                    _ => panic!("premature status"),
                }
            }
            if matches!(expected, Termination::Exit { .. }) {
                assert_eq!(out, b"out");
                assert_eq!(err, b"err");
            }
        }
    }
    #[test]
    fn clean_status_requires_successful_sync_unmount_and_fetch_validation() {
        let mut wire = Vec::new();
        assert!(
            complete(Termination::Exit { code: 0 }, &mut wire, |_| Err(invalid(
                "unmount failed"
            )))
            .is_err()
        );
        assert!(wire.is_empty());
        complete(Termination::Exit { code: 3 }, &mut wire, |_| Ok(())).unwrap();
        assert_eq!(
            read_frame::<Frame>(&mut wire.as_slice()).unwrap(),
            Frame::Status {
                termination: Termination::Exit { code: 3 },
                clean: true
            }
        );
    }
    #[test]
    fn relay_creates_one_transport_per_tcp_connection_and_preserves_half_close() {
        use std::{
            io::Write, net::TcpStream, os::unix::net::UnixStream, sync::atomic::AtomicUsize,
        };
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut relay = Relay::start_with(listener, move || {
            count.fetch_add(1, Ordering::Relaxed);
            let (guest, mut host) = UnixStream::pair()?;
            thread::spawn(move || {
                let mut bytes = Vec::new();
                host.read_to_end(&mut bytes).unwrap();
                host.write_all(&bytes).unwrap();
                host.shutdown(Shutdown::Write).unwrap();
            });
            Ok(File::from(OwnedFd::from(guest)))
        })
        .unwrap();
        for bytes in [b"first".as_slice(), b"second".as_slice()] {
            let mut tcp = TcpStream::connect(address).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            tcp.write_all(bytes).unwrap();
            tcp.shutdown(Shutdown::Write).unwrap();
            let mut reply = Vec::new();
            tcp.read_to_end(&mut reply).unwrap();
            assert_eq!(reply, bytes);
        }
        relay.finish().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn relay_reset_mid_transfer_prevents_clean_fetch_status() {
        use std::{io::Write, net::TcpStream};
        let upstream = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let (mut peer, _) = upstream.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut byte = [0];
            peer.read_exact(&mut byte).unwrap();
            peer.write_all(b"partial response").unwrap();
            // Closing with zero linger sends a real TCP reset, not a clean EOF.
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            cvt(unsafe {
                libc::setsockopt(
                    peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    (&linger as *const libc::linger).cast(),
                    std::mem::size_of_val(&linger) as _,
                )
            })
            .unwrap();
        });
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let mut relay = Relay::start_with(listener, move || {
            Ok(File::from(OwnedFd::from(TcpStream::connect(
                upstream_address,
            )?)))
        })
        .unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"request").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        peer.join().unwrap();
        let mut wire = Vec::new();
        let error =
            complete(Termination::Exit { code: 0 }, &mut wire, |_| relay.finish()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        assert!(
            wire.is_empty(),
            "a failed transfer must never publish a clean status"
        );
    }

    #[test]
    fn relay_accepts_more_than_512_sequential_connections() {
        use std::{io::Write, net::TcpStream, os::unix::net::UnixStream};
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let descriptors = || fs::read_dir("/proc/self/fd").unwrap().count();
        let before = descriptors();
        let mut relay = Relay::start_with(listener, || {
            let (guest, mut host) = UnixStream::pair()?;
            thread::spawn(move || {
                let mut bytes = Vec::new();
                host.read_to_end(&mut bytes).unwrap();
                host.write_all(&bytes).unwrap();
                host.shutdown(Shutdown::Write).unwrap();
            });
            Ok(File::from(OwnedFd::from(guest)))
        })
        .unwrap();
        for i in 0..520u32 {
            let mut client = TcpStream::connect(address).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client.write_all(&i.to_be_bytes()).unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).unwrap();
            assert_eq!(reply, i.to_be_bytes(), "connection {i}");
        }
        assert!(
            descriptors() <= before + 16,
            "completed connections retained descriptors"
        );
        relay.finish().unwrap();
    }

    #[test]
    fn relay_enforces_concurrent_connection_bound() {
        use std::{
            net::TcpStream,
            os::unix::net::UnixStream,
            sync::{mpsc, Mutex},
        };
        assert_eq!(MAX_RELAY_CONNECTIONS, 512);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let hosts = Arc::new(Mutex::new(Vec::new()));
        let retained = hosts.clone();
        let (accepted, events) = mpsc::channel();
        // Exercise the same admission code with four live connections rather than 1024 copy threads.
        let mut relay = Relay::start_with_capacity(
            listener,
            move || {
                let (guest, host) = UnixStream::pair()?;
                retained.lock().unwrap().push(host);
                accepted.send(()).unwrap();
                Ok(File::from(OwnedFd::from(guest)))
            },
            4,
        )
        .unwrap();
        let mut clients = Vec::new();
        for _ in 0..4 {
            clients.push(TcpStream::connect(address).unwrap());
            events.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let mut excess = TcpStream::connect(address).unwrap();
        excess
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(excess.read(&mut [0]).unwrap(), 0);
        assert_eq!(
            relay.finish().unwrap_err().to_string(),
            "proxy connection bound exceeded"
        );
        assert_eq!(hosts.lock().unwrap().len(), 4);
    }
    #[test]
    fn environment_forces_guest_paths_offline_and_removes_wrappers() {
        let mut s = start();
        s.env = BTreeMap::from([
            ("CARGO_TARGET_DIR".into(), "/host/target".into()),
            ("RUSTC_WRAPPER".into(), "/host/wrapper".into()),
            ("CARGO_NET_OFFLINE".into(), "false".into()),
            ("HOME".into(), "/host/home".into()),
        ]);
        let env = environment(&s);
        assert_eq!(env["CARGO_TARGET_DIR"], TARGET);
        assert_eq!(env["CARGO_NET_OFFLINE"], "true");
        assert_eq!(env["CARGO_BUILD_JOBS"], "2");
        assert!(!env.contains_key("RUSTC_WRAPPER"));
        assert_ne!(env.get("HOME").map(String::as_str), Some("/host/home"));
    }
    #[test]
    fn fetch_uses_image_cargo_and_ignores_request_environment() {
        let mut s = start();
        s.mode = Mode::Fetch {
            manifest: "/job/workspace/Cargo.toml".into(),
            git_directories: BTreeMap::new(),
        };
        s.proxy_port = Some(53);
        s.env
            .insert("RUSTUP_TOOLCHAIN".into(), "/workspace/evil".into());
        s.env.insert("GIT_CONFIG_COUNT".into(), "999".into());
        let env = environment(&s);
        assert!(!env.contains_key("RUSTUP_TOOLCHAIN"));
        assert!(!env.contains_key("GIT_CONFIG_COUNT"));
        assert_eq!(env["CARGO_NET_OFFLINE"], "false");
        assert_eq!(env["CARGO_HTTP_PROXY"], "http://127.0.0.1:3128");
        assert!(Path::new(IMAGE_CARGO).is_absolute());
        assert_eq!(
            fetch_command(Path::new("/job/workspace/Cargo.toml")).get_program(),
            IMAGE_CARGO
        );
    }
}
