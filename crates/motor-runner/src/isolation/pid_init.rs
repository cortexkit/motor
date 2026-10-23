//! The init process of a Boat job's PID namespace.
//!
//! Build-server job units set `PrivatePIDs=yes`, so each job has its own PID
//! namespace: PID 1 is the runner's bash wrapper, the job is PID 2, and `/proc`
//! shows only that namespace. Boat jobs use this init on every systemd version
//! (including 255, which rejects PrivatePIDs), so the namespace and quota
//! inheritance filter share one bootstrap. The unit execs the installed runner
//! as root instead of the wrapper. It
//!
//! 1. creates the PID namespace and forks its PID 1;
//! 2. in PID 1, mounts a `/proc` for the new namespace, carrying over the
//!    read-only and inaccessible submounts systemd made for
//!    `ProtectKernelTunables=yes`;
//! 3. drops to the job account with no capabilities, joins a fresh session
//!    keyring and installs a seccomp filter that blocks new PID namespaces
//!    again, matching `RestrictNamespaces=yes`, and prevents clearing quota
//!    inheritance through filesystem-attribute setters;
//! 4. execs the unchanged wrapper as PID 1.
//!
//! The first process stays outside the namespace as the unit's main process
//! and forwards PID 1's exit code or terminating signal. The supervisor never
//! dumps another core: a child's core-dumped status becomes killed-by-signal
//! with the same signal number, not the same core-dumped bit.
//!
//! A failure before the wrapper starts writes one line starting with
//! [`SETUP_FAILURE_PREFIX`] to [`SETUP_ERROR_FILE`] in the job's output
//! directory and exits [`SETUP_FAILED`]. The engine turns that into a
//! refused-before-start job instead of a job exit code.
use std::{
    ffi::{CString, OsString},
    fmt, fs, io,
    os::unix::ffi::OsStringExt,
    path::Path,
};

/// The Boat recipe installs the runner here (`script/boat-image/install.py`).
/// It lies under `/usr`, which every job root binds read-only from the host, so
/// the unit can exec it without another bind.
pub(crate) const INSTALLED_RUNNER: &str = "/usr/local/bin/motor-runner";
/// The runner's command-line mode that runs [`run`].
pub(crate) const MODE: &str = "job-pid-init";
/// systemd's own exit status for a failed namespace setup (`EXIT_NAMESPACE`).
pub(crate) const SETUP_FAILED: i32 = 226;
/// No trusted command status exists if the supervisor itself cannot wait.
pub(crate) const SUPERVISOR_FAILED: i32 = 227;
/// File receiving a setup failure line or trusted pre-exec JSON telemetry. The
/// runner creates it root-owned 0600 before the unit starts; only the init,
/// still root, can open it, and it is closed before the wrapper runs.
pub(crate) const SETUP_ERROR_FILE: &str = "setup-error";
/// Allocated before launch so reporting does not need another block on a full disk.
pub(crate) const REPORT_BYTES: usize = 4096;
/// Start of the line the init writes to [`SETUP_ERROR_FILE`] on failure.
pub(crate) const SETUP_FAILURE_PREFIX: &str = "motor: job PID namespace setup failed: ";
/// The capabilities the unit keeps for the init. The init empties the bounding
/// set before the wrapper starts.
pub(crate) const UNIT_CAPABILITIES: &str = "CAP_SYS_ADMIN CAP_SETUID CAP_SETGID CAP_SETPCAP";

const SETUP_ERROR_PATH: &str = "/motor-output/setup-error";
/// Where the new `/proc` is mounted before it replaces the old one. `/dev/shm`
/// is the unit's own fresh tmpfs, writable by root.
const STAGING: &str = "/dev/shm/.motor-pid-init";

/// The job account's numeric identity, resolved by the runner on the host. The
/// init does no name lookups: the IDs are exactly what systemd's `User=` and
/// `Group=` would apply, including the supplementary groups from the account's
/// group membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobIdentity {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) groups: Vec<u32>,
    /// Protected proc mounts measured in an independent systemd reference unit.
    pub(crate) proc_protections: Vec<(String, bool)>,
}

impl JobIdentity {
    /// Resolve `user` with `id(1)`, which reports the same initgroups list that
    /// systemd applies for `User=`.
    pub(crate) fn resolve(user: &str) -> io::Result<Self> {
        let number = |flag: &str| -> io::Result<String> {
            let output = crate::systemd::output(
                std::process::Command::new("/usr/bin/id").args([flag, user]),
            )?;
            if !output.status.success() {
                return Err(io::Error::other(format!("id {flag} {user} failed")));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        let parse = |text: &str| -> io::Result<u32> {
            text.parse()
                .map_err(|_| io::Error::other(format!("id reported a non-numeric id: {text:?}")))
        };
        let identity = Self {
            uid: parse(&number("-u")?)?,
            gid: parse(&number("-g")?)?,
            groups: number("-G")?
                .split_whitespace()
                .map(parse)
                .collect::<io::Result<_>>()?,
            proc_protections: reference_proc_protections(user)?,
        };
        if identity.uid == 0 || identity.gid == 0 || identity.groups.contains(&0) {
            return Err(io::Error::other(format!(
                "{user} must not be root or in group 0"
            )));
        }
        Ok(identity)
    }
}

/// The unit's command line: the installed runner in [`MODE`], the identity,
/// then the wrapper's own argv, which the init execs unchanged.
pub(crate) fn exec_start(identity: &JobIdentity, wrapper: &[&str]) -> Vec<String> {
    let groups = identity
        .groups
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    [
        INSTALLED_RUNNER.to_owned(),
        MODE.to_owned(),
        identity.uid.to_string(),
        identity.gid.to_string(),
        groups,
        serde_json::to_string(&identity.proc_protections).expect("proc protection list is JSON"),
        "--".to_owned(),
    ]
    .into_iter()
    .chain(wrapper.iter().map(|arg| (*arg).to_owned()))
    .collect()
}

/// Environment names glibc's dynamic loader or allocator reads. The init
/// starts as root with the job's environment, so none of these may reach it.
/// The request allowlist (`motor_protocol::allowed_env`) already keeps them
/// out; this check does not depend on that.
pub(crate) fn refuse_loader_environment<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> io::Result<()> {
    let refused: Vec<_> = names
        .into_iter()
        .filter(|name| {
            name.starts_with("LD_")
                || name.starts_with("MALLOC_")
                || matches!(*name, "GLIBC_TUNABLES" | "GCONV_PATH" | "LOCPATH")
        })
        .collect();
    if refused.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "a Boat job's PID namespace init starts as root and must not receive {}",
            refused.join(", ")
        )))
    }
}

/// Refuse to serve Boat jobs unless this runner is the installed binary the
/// job units exec, owned by root and not writable by anyone else along its
/// path. Otherwise a unit could run a different build than the daemon, or one
/// a non-root user could replace.
pub(crate) fn check_installed_runner() -> io::Result<()> {
    check_runner_path(&std::env::current_exe()?, Path::new(INSTALLED_RUNNER))
}

fn check_runner_path(running: &Path, installed: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    for path in installed.ancestors() {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("job PID namespace init {}: {error}", path.display()),
            )
        })?;
        let kind_ok = if path == installed {
            metadata.is_file() && metadata.mode() & 0o111 != 0
        } else {
            metadata.is_dir()
        };
        if !kind_ok || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other(format!(
                "unsafe path to the job PID namespace init {}: uid={} mode={:04o}; it must be a root-owned file in root-owned directories that only root can write",
                path.display(),
                metadata.uid(),
                metadata.mode() & 0o7777
            )));
        }
    }
    if running.canonicalize()? != installed {
        return Err(io::Error::other(format!(
            "--boat-profile on systemd before 257 runs each job's PID namespace init from {}; this runner is {}. Run the installed runner",
            installed.display(), running.display()
        )));
    }
    Ok(())
}

/// What the init was asked to do, parsed from its command line.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    identity: JobIdentity,
    argv: Vec<CString>,
}

fn parse_args(args: Vec<OsString>) -> io::Result<Plan> {
    let invalid = |what: &str| io::Error::other(format!("invalid {MODE} arguments: {what}"));
    let mut args = args.into_iter();
    let mut number = |what: &str| -> io::Result<u32> {
        args.next()
            .and_then(|arg| arg.to_str().and_then(|text| text.parse().ok()))
            .ok_or_else(|| invalid(what))
    };
    let uid = number("uid")?;
    let gid = number("gid")?;
    let groups = args.next().ok_or_else(|| invalid("groups"))?;
    let groups = groups.to_str().ok_or_else(|| invalid("groups"))?;
    let groups = if groups.is_empty() {
        Vec::new()
    } else {
        groups
            .split(',')
            .map(|group| group.parse().map_err(|_| invalid("groups")))
            .collect::<io::Result<_>>()?
    };
    let expected = args.next().ok_or_else(|| invalid("proc protections"))?;
    let proc_protections: Vec<(String, bool)> = serde_json::from_str(
        expected
            .to_str()
            .ok_or_else(|| invalid("proc protections"))?,
    )
    .map_err(|_| invalid("proc protections"))?;
    validate_proc_protections(&proc_protections)?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err(invalid("missing -- before the wrapper command"));
    }
    let argv = args
        .map(|arg| CString::new(arg.into_vec()).map_err(|_| invalid("NUL in wrapper argument")))
        .collect::<io::Result<Vec<_>>>()?;
    if argv
        .first()
        .is_none_or(|program| !program.as_bytes().starts_with(b"/"))
    {
        return Err(invalid("the wrapper must be an absolute path"));
    }
    if uid == 0 || gid == 0 || groups.contains(&0) {
        return Err(invalid("the job identity must not be root"));
    }
    Ok(Plan {
        identity: JobIdentity {
            uid,
            gid,
            groups,
            proc_protections,
        },
        argv,
    })
}

/// Where a setup failure is reported: the root-only setup-error file when the
/// unit binds it, otherwise stderr.
struct Report {
    fd: libc::c_int,
    owned: bool,
}

impl Report {
    fn open() -> Self {
        let path = CString::new(SETUP_ERROR_PATH).unwrap();
        // SAFETY: path is a valid NUL-terminated string.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        Self {
            fd: if fd < 0 { libc::STDERR_FILENO } else { fd },
            owned: fd >= 0,
        }
    }

    fn fail(&self, step: &str, error: impl fmt::Display) -> ! {
        let line = format!("{SETUP_FAILURE_PREFIX}{step}: {error}\n");
        self.write(&line);
        unsafe { libc::_exit(SETUP_FAILED) }
    }

    fn write(&self, line: &str) {
        // SAFETY: the buffer is valid for its length. Retry interrupted and
        // partial writes so the engine can recognize the setup failure line.
        let mut block = [b' '; REPORT_BYTES];
        let bytes = if self.owned {
            // Preserve the allocated block and erase any earlier telemetry.
            // A newline is copied only with a complete report, never a torn prefix.
            let n = line.len().min(REPORT_BYTES);
            block[..n].copy_from_slice(&line.as_bytes()[..n]);
            &block[..]
        } else {
            line.as_bytes()
        };
        unsafe {
            if self.owned {
                libc::lseek(self.fd, 0, libc::SEEK_SET);
            }
            let mut written = 0;
            while written < bytes.len() {
                let n = libc::write(
                    self.fd,
                    bytes.as_ptr().add(written).cast(),
                    bytes.len() - written,
                );
                if n > 0 {
                    written += n as usize;
                } else if n == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
        }
    }

    fn check(&self, step: &str, result: libc::c_long) {
        if result < 0 {
            self.fail(step, io::Error::last_os_error());
        }
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

/// The runner's `job-pid-init` mode. Never returns: it ends by exec'ing the
/// wrapper (in the namespace's PID 1) or by exiting with the wrapper's status
/// (in the unit's main process).
pub fn run(args: Vec<OsString>) -> ! {
    let report = Report::open();
    let plan = parse_args(args).unwrap_or_else(|error| report.fail("arguments", error));
    if !cfg!(target_arch = "x86_64") {
        report.fail("architecture", "the namespace filter supports only x86_64");
    }
    // SAFETY: plain libc calls on locally owned values. The process is
    // single-threaded (main dispatches this mode before starting any runtime),
    // so the child of fork may run ordinary Rust code.
    unsafe {
        // Build-server PID 1 is a namespace init and so ignores every signal
        // from outside except SIGKILL and SIGSTOP. This process stands where
        // systemd sees that PID 1; blocking all signals gives it the same
        // reaction to the unit's SIGTERM, which then reaches only the job.
        let mut all = std::mem::zeroed::<libc::sigset_t>();
        libc::sigfillset(&mut all);
        libc::sigprocmask(libc::SIG_BLOCK, &all, std::ptr::null_mut());
        // PID 1 sees an outside parent as PPid 0 even after reparenting. A
        // pidfd is therefore needed in addition to the ordinary PPid check.
        let parent_fd = libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) as libc::c_int;
        report.check("open supervisor pidfd", parent_fd.into());
        report.check(
            "create PID namespace",
            libc::unshare(libc::CLONE_NEWPID).into(),
        );
        match libc::fork() {
            -1 => report.fail("start namespace init", io::Error::last_os_error()),
            0 => namespace_init(&plan, &report, parent_fd),
            child => {
                // Only the pre-exec child may authenticate a setup failure.
                drop(report);
                libc::close(parent_fd);
                supervise(child)
            }
        }
    }
}

unsafe fn supervise(child: libc::pid_t) -> ! {
    if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
        libc::_exit(SUPERVISOR_FAILED);
    }
    let mut status = 0;
    loop {
        if libc::waitpid(child, &mut status, 0) == child {
            forward(status);
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            libc::_exit(SUPERVISOR_FAILED);
        }
    }
}

/// How the unit's main process must end so systemd records PID 1's status.
#[derive(Debug, PartialEq, Eq)]
enum Forward {
    Exit(i32),
    Signal { signal: i32, core_dumped: bool },
}

fn forwarded(status: libc::c_int) -> Forward {
    if libc::WIFEXITED(status) {
        Forward::Exit(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Forward::Signal {
            signal: libc::WTERMSIG(status),
            core_dumped: libc::WCOREDUMP(status),
        }
    } else {
        // waitpid without WUNTRACED reports only exits and signal deaths.
        Forward::Exit(SETUP_FAILED)
    }
}

fn forward(status: libc::c_int) -> ! {
    // SAFETY: plain libc calls; this process only exits from here.
    unsafe {
        match forwarded(status) {
            Forward::Exit(code) => libc::_exit(code),
            Forward::Signal { signal, .. } => {
                // A child core is never duplicated by this privileged parent.
                // Its signal is preserved, but the parent's core bit is clear.
                if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
                    libc::_exit(SUPERVISOR_FAILED);
                }
                if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                    libc::signal(signal, libc::SIG_DFL);
                }
                let mut set = std::mem::zeroed::<libc::sigset_t>();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, signal);
                libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                libc::kill(libc::getpid(), signal);
                // Only a signal whose default action does not terminate gets
                // here; report it the way a shell would.
                libc::_exit(128 + signal)
            }
        }
    }
}

fn path(text: &str) -> CString {
    CString::new(text).unwrap()
}

/// PID 1 of the new namespace, still root and still in the unit's mount
/// namespace. Never returns: it execs the wrapper or exits with
/// [`SETUP_FAILED`].
unsafe fn namespace_init(plan: &Plan, report: &Report, parent_fd: libc::c_int) -> ! {
    let null = std::ptr::null::<libc::c_char>();
    // If the unit's main process dies, so does the namespace. Should it die
    // before this call, systemd stops the unit and kills its whole cgroup.
    report.check(
        "set parent-death signal",
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong).into(),
    );
    separate_session().unwrap_or_else(|error| report.fail("create job session", error));
    // systemd's default MountFlags=shared leaves these shared inside the unit,
    // and the kernel refuses to move a mount whose parent is shared. Both are
    // the unit's private API mounts; nothing outside the unit receives them.
    for (step, target, flags) in [
        (
            "make /proc private",
            "/proc",
            libc::MS_REC | libc::MS_PRIVATE,
        ),
        ("make /dev/shm private", "/dev/shm", libc::MS_PRIVATE),
    ] {
        let target = path(target);
        report.check(
            step,
            libc::mount(null, target.as_ptr(), null, flags, std::ptr::null()).into(),
        );
    }
    let staging = path(STAGING);
    report.check(
        "create proc staging directory",
        libc::mkdir(staging.as_ptr(), 0o700).into(),
    );
    // The same options systemd uses for ProtectProc=invisible. A proc mount
    // shows the PID namespace of the task that mounts it: this one.
    let (proc_type, options) = (path("proc"), path("hidepid=invisible"));
    report.check(
        "mount the namespace's proc",
        libc::mount(
            proc_type.as_ptr(),
            staging.as_ptr(),
            proc_type.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            options.as_ptr().cast(),
        )
        .into(),
    );
    let table = fs::read_to_string(format!("{STAGING}/self/mountinfo"))
        .unwrap_or_else(|error| report.fail("read the mount table", error));
    let submounts =
        proc_submounts(&table).unwrap_or_else(|error| report.fail("find /proc submounts", error));
    // These are what systemd mounted for ProtectKernelTunables=yes. Their
    // contents do not depend on the PID namespace, so they move as they are.
    for submount in &submounts {
        let source = path(submount);
        let target = path(&format!("{STAGING}{}", &submount["/proc".len()..]));
        report.check(
            &format!("move {submount}"),
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                null,
                libc::MS_MOVE,
                std::ptr::null(),
            )
            .into(),
        );
    }
    let proc_path = path("/proc");
    report.check(
        "move the namespace's proc onto /proc",
        libc::mount(
            staging.as_ptr(),
            proc_path.as_ptr(),
            null,
            libc::MS_MOVE,
            std::ptr::null(),
        )
        .into(),
    );
    report.check(
        "remove proc staging directory",
        libc::rmdir(staging.as_ptr()).into(),
    );
    let table = fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_else(|error| report.fail("read final proc mounts", error));
    verify_proc_protections(&table, &plan.identity.proc_protections)
        .unwrap_or_else(|error| report.fail("verify protected proc mounts", error));
    let parent = libc::getppid();
    drop_privileges(&plan.identity, report);
    arm_parent_death_signal(parent, parent_fd).unwrap_or_else(|error| {
        report.fail("re-arm parent-death signal after credential drop", error)
    });
    libc::close(parent_fd);
    // The unit's session keyring was created for root. Give the job its own.
    report.check(
        "join a new session keyring",
        libc::syscall(libc::SYS_keyctl, KEYCTL_JOIN_SESSION_KEYRING, null),
    );
    let program = namespace_filter(libc::EPERM as u16);
    let filter = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    report.check(
        "install the namespace filter",
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &filter as *const SockFprog,
        )
        .into(),
    );
    // systemd starts services with default dispositions and no blocked
    // signals. Rust's runtime ignores SIGPIPE and the main process blocked
    // everything before forking; exec would carry both into the job.
    for signal in 1..=libc::SIGRTMAX() {
        if signal != libc::SIGKILL && signal != libc::SIGSTOP {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
    let mut none = std::mem::zeroed::<libc::sigset_t>();
    libc::sigemptyset(&mut none);
    libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    let mut argv: Vec<*const libc::c_char> = plan.argv.iter().map(|arg| arg.as_ptr()).collect();
    argv.push(std::ptr::null());
    // Root-only telemetry is written exclusively by trusted pre-exec code.
    // The host probe reads it; the job cannot forge it or inherit its fd.
    let mut death_signal = 0i32;
    report.check(
        "read parent-death signal",
        libc::prctl(libc::PR_GET_PDEATHSIG, &mut death_signal).into(),
    );
    let fds = exec_fds().unwrap_or_else(|error| report.fail("inspect exec descriptors", error));
    report.write(&format!("{}\n", serde_json::json!({"pdeathsig":death_signal, "uid":libc::getuid(), "gid":libc::getgid(),
        "sid":libc::getsid(0), "pgid":libc::getpgrp(), "ppid":libc::getppid(), "exec_fds":fds, "proc_submounts":proc_protections(&table).unwrap()})));
    libc::execv(argv[0], argv.as_ptr());
    report.fail("exec the wrapper", io::Error::last_os_error())
}

unsafe fn arm_parent_death_signal(parent: libc::pid_t, parent_fd: libc::c_int) -> io::Result<()> {
    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut watch = libc::pollfd {
        fd: parent_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = libc::poll(&mut watch, 1, 0);
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::getppid() != parent || ready != 0 {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

unsafe fn separate_session() -> io::Result<()> {
    if libc::setsid() < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn exec_fds() -> io::Result<Vec<i32>> {
    let mut kept = Vec::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        if let Some(fd) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                kept.push(fd);
            }
        }
    }
    kept.sort_unstable();
    Ok(kept)
}

const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;
const PR_CAP_AMBIENT: libc::c_int = 47;
const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
const PR_CAPBSET_DROP: libc::c_int = 24;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_SECCOMP: libc::c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Become the job account with nothing extra, as systemd's `User=`, `Group=`,
/// empty `CapabilityBoundingSet=` and `NoNewPrivileges=yes` leave a job.
unsafe fn drop_privileges(identity: &JobIdentity, report: &Report) {
    // Emptying the bounding set needs CAP_SETPCAP, so it comes first.
    for capability in 0..64 {
        if libc::prctl(PR_CAPBSET_DROP, capability as libc::c_ulong) != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            report.fail("empty the capability bounding set", error);
        }
    }
    report.check(
        "set supplementary groups",
        libc::setgroups(identity.groups.len(), identity.groups.as_ptr()).into(),
    );
    let (uid, gid) = (identity.uid, identity.gid);
    report.check("set group ids", libc::setresgid(gid, gid, gid).into());
    // Changing every user id away from 0 clears the permitted and effective
    // capability sets.
    report.check("set user ids", libc::setresuid(uid, uid, uid).into());
    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    report.check(
        "clear capabilities",
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut CapHeader,
            data.as_ptr(),
        ),
    );
    report.check(
        "clear ambient capabilities",
        libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0).into(),
    );
    report.check(
        "set no_new_privs",
        libc::prctl(PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0).into(),
    );
    // Verify rather than trust each step.
    let (mut r, mut e, mut s) = (0, 0, 0);
    libc::getresuid(&mut r, &mut e, &mut s);
    let (mut rg, mut eg, mut sg) = (0, 0, 0);
    libc::getresgid(&mut rg, &mut eg, &mut sg);
    report.check(
        "read capabilities",
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut CapHeader,
            data.as_mut_ptr(),
        ),
    );
    let capabilities = data
        .iter()
        .any(|set| set.effective != 0 || set.permitted != 0 || set.inheritable != 0);
    if [r, e, s] != [uid; 3] || [rg, eg, sg] != [gid; 3] || capabilities {
        report.fail(
            "verify credentials",
            format!("uids {r}/{e}/{s} gids {rg}/{eg}/{sg} capabilities left {capabilities}"),
        );
    }
}

/// The direct submounts of the topmost `/proc` mount in a mountinfo table.
fn proc_submounts(table: &str) -> io::Result<Vec<String>> {
    let entries: Vec<(u64, u64, String)> = table
        .lines()
        .map(|line| {
            let mut fields = line.split(' ');
            let id = fields.next().and_then(|f| f.parse().ok());
            let parent = fields.next().and_then(|f| f.parse().ok());
            let point = fields.nth(2).map(unescape);
            match (id, parent, point) {
                (Some(id), Some(parent), Some(point)) => Ok((id, parent, point)),
                _ => Err(io::Error::other(format!(
                    "unreadable mountinfo line {line:?}"
                ))),
            }
        })
        .collect::<io::Result<_>>()?;
    let at_proc: Vec<_> = entries
        .iter()
        .filter(|(_, _, point)| point == "/proc")
        .collect();
    // Stacked mounts on one mount point: the top one is not any other's parent.
    let top = at_proc
        .iter()
        .find(|(id, _, _)| !at_proc.iter().any(|(_, parent, _)| parent == id))
        .map(|(id, _, _)| *id)
        .ok_or_else(|| io::Error::other("no /proc mount in the mount table"))?;
    Ok(entries
        .into_iter()
        .filter(|(_, parent, point)| *parent == top && point.starts_with("/proc/"))
        .map(|(_, _, point)| point)
        .collect())
}

/// Measure the same independent systemd policy used by the live Boat probe.
/// The manager, rather than a copied list of paths, defines the expected set.
fn reference_proc_protections(user: &str) -> io::Result<Vec<(String, bool)>> {
    let mut command = crate::privilege::command("/usr/bin/systemd-run");
    command.args([
        "--quiet",
        "--wait",
        "--pipe",
        "--collect",
        "--service-type=exec",
    ]);
    for property in [
        format!("User={user}"),
        format!("Group={user}"),
        "NoNewPrivileges=yes".into(),
        "CapabilityBoundingSet=".into(),
        "RestrictNamespaces=yes".into(),
        "ProtectProc=invisible".into(),
        "ProtectKernelTunables=yes".into(),
        "ProtectKernelModules=yes".into(),
        "ProtectControlGroups=yes".into(),
        "PrivateDevices=yes".into(),
        "PrivateNetwork=yes".into(),
        "PrivateTmp=yes".into(),
        "ProtectSystem=strict".into(),
        "ProtectHome=tmpfs".into(),
        "MountAPIVFS=yes".into(),
        "KeyringMode=private".into(),
    ] {
        command.arg(format!("--property={property}"));
    }
    command.args(["/bin/cat", "/proc/self/mountinfo"]);
    let output = crate::systemd::output(&mut command)?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "cannot measure systemd proc protections: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let protections = proc_protections(&String::from_utf8_lossy(&output.stdout))?;
    validate_proc_protections(&protections)?;
    Ok(protections)
}

fn proc_protections(table: &str) -> io::Result<Vec<(String, bool)>> {
    let mut protections = Vec::new();
    for line in table.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 6 {
            return Err(io::Error::other("invalid mountinfo entry"));
        }
        let point = unescape(fields[4]);
        if point.starts_with("/proc/") {
            protections.push((point, fields[5].split(',').any(|flag| flag == "ro")));
        }
    }
    protections.sort();
    Ok(protections)
}

fn validate_proc_protections(expected: &[(String, bool)]) -> io::Result<()> {
    if !expected
        .iter()
        .any(|(point, ro)| point == "/proc/sys" && *ro)
        || expected
            .iter()
            .any(|(point, _)| !point.starts_with("/proc/") || point.contains('\0'))
    {
        return Err(io::Error::other(
            "invalid reference proc protections: read-only /proc/sys required",
        ));
    }
    Ok(())
}

fn verify_proc_protections(table: &str, expected: &[(String, bool)]) -> io::Result<()> {
    validate_proc_protections(expected)?;
    let mut expected = expected.to_vec();
    expected.sort();
    let actual = proc_protections(table)?;
    if actual != expected {
        return Err(io::Error::other(format!("proc protections differ from systemd reference: expected {expected:?}, actual {actual:?}")));
    }
    Ok(())
}

/// mountinfo writes space, tab, newline and backslash as `\ooo` octal.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|digits| bytes[i] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)));
        if let Some(digits) = octal {
            out.push(
                digits
                    .iter()
                    .fold(0u8, |n, d| n.wrapping_mul(8) + (d - b'0')),
            );
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One classic-BPF instruction, laid out as the kernel's `struct sock_filter`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// The kernel's `struct sock_fprog`.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_ALU_AND_K: u16 = 0x54;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
/// x32 system calls are x86_64-arch calls with this bit set in the number.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
/// Offsets into the kernel's `struct seccomp_data`. Arguments are 64-bit; the
/// low 32 bits come first on little-endian x86. Every flag tested here is in
/// those low bits.
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
const DATA_ARG0_LOW: u32 = 16;
const DATA_ARG1_LOW: u32 = 24;
const CLONE_NEWPID: u32 = 0x2000_0000;
const X86_64_CLONE: u32 = 56;
const X86_64_UNSHARE: u32 = 272;
const X86_64_SETNS: u32 = 308;
const I386_CLONE: u32 = 120;
const I386_UNSHARE: u32 = 310;
const I386_SETNS: u32 = 346;
const CLONE3: u32 = 435;
// Linux UAPI include/uapi/asm-generic/unistd.h and x86 syscall_{64,32}.tbl:
// file_setattr is common syscall 469; x32 adds X32_SYSCALL_BIT.
const FILE_SETATTR: u32 = 469;

/// A step of the filter, with jumps to named labels.
enum Step {
    Load(u32),
    And(u32),
    JumpIfEqual(u32, &'static str, &'static str),
    JumpIfAnyBit(u32, &'static str, &'static str),
    Return(u32),
    Label(&'static str),
}

/// The seccomp program that, stacked on systemd's `RestrictNamespaces=pid`
/// filter, leaves exactly what `RestrictNamespaces=yes` enforces. It returns
/// `errno` for `unshare` and `clone` whose flags include CLONE_NEWPID, and for
/// `setns` whose namespace type is CLONE_NEWPID or 0 (any type, which systemd
/// refuses too). It covers x86_64, x32 and i386 system call numbers and allows
/// other supported-ABI calls, which systemd's filter still governs. clone3's
/// pointed-to arguments cannot be inspected by classic BPF, so return ENOSYS
/// explicitly and let libc fall back to clone. file_setattr also returns ENOSYS:
/// its path-based attribute setters must fall back to the refused ioctls rather
/// than bypassing project inheritance protection. file_getattr remains allowed.
/// Unknown architectures are killed
/// rather than trusting an inherited filter to cover an unrecognized ABI.
/// The engine passes EPERM, the errno systemd's filter uses; tests pass a
/// distinct value to see which refusals come from this filter.
///
/// seccompiler's high-level filter targets one architecture and kills on an
/// architecture mismatch; it has no i386 target. This small multi-ABI filter
/// handles i386 and x32 namespace calls explicitly, failing closed otherwise.
/// Filesystem-attribute setters are also denied: an inode owner can otherwise
/// clear project inheritance and create files outside the Boat job's quota.
/// Attribute getters and unrelated ioctl requests remain available.
/// Tests exercise both arms of every BPF jump.
fn namespace_filter(errno: u16) -> Vec<SockFilter> {
    namespace_filter_with_setattr_errno(errno, libc::ENOSYS as u16)
}

// A distinct setter errno lets tests prove interception even on kernels that
// predate file_setattr and would return ENOSYS without any filter.
fn namespace_filter_with_setattr_errno(errno: u16, setter_errno: u16) -> Vec<SockFilter> {
    use Step::*;
    let steps = [
        Load(DATA_ARCH),
        JumpIfEqual(AUDIT_ARCH_X86_64, "x86_64", "other_arch"),
        Label("other_arch"),
        JumpIfEqual(AUDIT_ARCH_I386, "i386", "kill"),
        Label("x86_64"),
        Load(DATA_NR),
        And(!X32_SYSCALL_BIT),
        JumpIfEqual(CLONE3, "enosys", "x86_64_file_attr"),
        Label("x86_64_file_attr"),
        JumpIfEqual(FILE_SETATTR, "file_setattr_denied", "x86_64_calls"),
        Label("x86_64_calls"),
        JumpIfEqual(X86_64_UNSHARE, "flags", "x86_64_not_unshare"),
        Label("x86_64_not_unshare"),
        JumpIfEqual(X86_64_CLONE, "flags", "x86_64_not_clone"),
        Label("x86_64_not_clone"),
        JumpIfEqual(X86_64_SETNS, "nstype", "x86_64_not_setns"),
        Label("x86_64_not_setns"),
        JumpIfEqual(16, "ioctl", "x32_ioctl"),
        Label("x32_ioctl"),
        JumpIfEqual(514, "ioctl", "allow"),
        Label("i386"),
        Load(DATA_NR),
        JumpIfEqual(CLONE3, "enosys", "i386_file_attr"),
        Label("i386_file_attr"),
        JumpIfEqual(FILE_SETATTR, "file_setattr_denied", "i386_calls"),
        Label("i386_calls"),
        JumpIfEqual(I386_UNSHARE, "flags", "i386_not_unshare"),
        Label("i386_not_unshare"),
        JumpIfEqual(I386_CLONE, "flags", "i386_not_clone"),
        Label("i386_not_clone"),
        JumpIfEqual(I386_SETNS, "nstype", "i386_not_setns"),
        Label("i386_not_setns"),
        JumpIfEqual(54, "ioctl", "allow"),
        Label("ioctl"),
        Load(DATA_ARG1_LOW),
        JumpIfEqual(0x401c_5820, "deny", "not_fssetxattr"),
        Label("not_fssetxattr"),
        JumpIfEqual(0x4008_6602, "deny", "not_setflags64"),
        Label("not_setflags64"),
        JumpIfEqual(0x4004_6602, "deny", "allow"),
        Label("flags"),
        Load(DATA_ARG0_LOW),
        JumpIfAnyBit(CLONE_NEWPID, "deny", "allow"),
        Label("nstype"),
        Load(DATA_ARG1_LOW),
        JumpIfEqual(0, "deny", "nstype_not_any"),
        Label("nstype_not_any"),
        JumpIfAnyBit(CLONE_NEWPID, "deny", "allow"),
        Label("deny"),
        Return(SECCOMP_RET_ERRNO | u32::from(errno)),
        Label("enosys"),
        Return(SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
        Label("file_setattr_denied"),
        Return(SECCOMP_RET_ERRNO | u32::from(setter_errno)),
        Label("kill"),
        Return(SECCOMP_RET_KILL_PROCESS),
        Label("allow"),
        Return(SECCOMP_RET_ALLOW),
    ];
    assemble(&steps)
}

fn assemble(steps: &[Step]) -> Vec<SockFilter> {
    let mut labels = std::collections::BTreeMap::new();
    let mut index = 0usize;
    for step in steps {
        match step {
            Step::Label(name) => {
                assert!(
                    labels.insert(*name, index).is_none(),
                    "duplicate label {name}"
                );
            }
            _ => index += 1,
        }
    }
    let mut program = Vec::with_capacity(index);
    let offset = |from: usize, name: &str| -> u8 {
        let target = labels[name];
        assert!(target > from, "jump to {name} must go forward");
        u8::try_from(target - from - 1).expect("jump fits in eight bits")
    };
    for step in steps {
        let at = program.len();
        let insn = |code, jt, jf, k| SockFilter { code, jt, jf, k };
        program.push(match *step {
            Step::Label(_) => continue,
            Step::Load(offset) => insn(BPF_LD_W_ABS, 0, 0, offset),
            Step::And(mask) => insn(BPF_ALU_AND_K, 0, 0, mask),
            Step::JumpIfEqual(k, yes, no) => {
                insn(BPF_JMP_JEQ_K, offset(at, yes), offset(at, no), k)
            }
            Step::JumpIfAnyBit(k, yes, no) => {
                insn(BPF_JMP_JSET_K, offset(at, yes), offset(at, no), k)
            }
            Step::Return(value) => insn(BPF_RET_K, 0, 0, value),
        });
    }
    program
}

#[cfg(test)]
#[path = "pid_init_tests.rs"]
mod tests;
