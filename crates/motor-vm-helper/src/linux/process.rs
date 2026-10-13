use crate::{Error, Result};
use std::{
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};

pub fn refuse_passwd_entries(base: u32, count: usize) -> Result<()> {
    for uid in base..base + count as u32 {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        let mut buffer = vec![0u8; 65536];
        let error = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error).into());
        }
        if !found.is_null() {
            return Err(Error(format!(
                "slot uid {uid} overlaps an existing passwd entry"
            )));
        }
    }
    Ok(())
}
fn has_uid(pid: u32, uid: u32) -> Result<bool> {
    let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if status.lines().any(|line| line.starts_with("State:\tZ")) {
        // A zombie cannot execute or retain a VM/mount. Direct children are
        // reaped by the helper; orphan zombies belong to the host's reaper.
        return Ok(false);
    }
    Ok(status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .is_some_and(|line| line.split_whitespace().any(|u| u.parse::<u32>() == Ok(uid))))
}
pub fn kill_uid(uid: u32) -> Result<()> {
    // pidfds avoid signalling an unrelated process if a numeric pid is reused.
    // Repeat after waiting because a process might have forked during the scan.
    for _ in 0..16 {
        let mut victims = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if !has_uid(pid, uid)? {
                continue;
            }
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(e.into());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            if !has_uid(pid, uid)? {
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
                    return Err(e.into());
                }
            }
            victims.push(fd);
        }
        if victims.is_empty() {
            return Ok(());
        }
        for fd in victims {
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut poll, 1, 5000) };
            if result <= 0 {
                return Err(Error("slot uid process did not exit after SIGKILL".into()));
            }
        }
    }
    Err(Error(
        "slot uid processes keep appearing during cleanup".into(),
    ))
}
pub fn place_here(procs: &Path) -> io::Result<()> {
    fs::write(procs, std::process::id().to_string())
}

/// A static loader is executed by fd after chroot. Its runtime needs no path
/// access, networking, process creation, or privilege-changing syscalls.
pub fn loader_program(exec_fd: i32) -> Vec<libc::sock_filter> {
    const LD_W_ABS: u16 = 0x20;
    const JMP_JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k, jt, jf| libc::sock_filter {
        code: JMP_JEQ_K,
        jt,
        jf,
        k,
    };
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc00000b7;
    let mut filter = vec![
        stmt(LD_W_ABS, 4),
        jump(arch, 1, 0),
        stmt(RET_K, 0x80000000),
        stmt(LD_W_ABS, 0),
    ];
    let allowed = [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_lseek,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mprotect,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_ioctl,
        libc::SYS_recvmsg,
        libc::SYS_sendmsg,
        libc::SYS_recvfrom,
        libc::SYS_sendto,
        libc::SYS_accept4,
        libc::SYS_ppoll,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_futex,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_clock_gettime,
        libc::SYS_getrandom,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_set_tid_address,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_sched_getaffinity,
        libc::SYS_prlimit64,
        libc::SYS_fcntl,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    for syscall in allowed {
        filter.push(jump(syscall as u32, 0, 1));
        filter.push(stmt(RET_K, 0x7fff0000));
    }
    #[cfg(target_arch = "x86_64")]
    for syscall in [libc::SYS_arch_prctl, libc::SYS_poll, libc::SYS_epoll_wait] {
        filter.push(jump(syscall as u32, 0, 1));
        filter.push(stmt(RET_K, 0x7fff0000));
    }
    // One exec is necessary to enter the static loader. Its executable fd is
    // CLOEXEC, so the inherited policy cannot be used to execute it again.
    filter.extend([
        jump(libc::SYS_execveat as u32, 0, 3),
        stmt(LD_W_ABS, 16),
        jump(exec_fd as u32, 0, 1),
        stmt(RET_K, 0x7fff0000),
        stmt(RET_K, 0x80000000),
    ]);
    filter
}
pub fn install_loader_filter(filter: &[libc::sock_filter]) -> io::Result<()> {
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
