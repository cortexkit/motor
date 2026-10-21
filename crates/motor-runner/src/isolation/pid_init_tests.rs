use super::*;
use std::collections::BTreeSet;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn exec_start_round_trips_through_the_init_arguments() {
    let identity = JobIdentity {
        uid: 997,
        gid: 996,
        groups: vec![996, 4242],
    };
    let wrapper = [
        "/bin/bash",
        "-c",
        "IFS= read -r -n 1 || exit 125",
        "motor-job",
        "echo 'a b'",
    ];
    let command = exec_start(&identity, &wrapper);
    assert_eq!(
        command[..6],
        [INSTALLED_RUNNER, MODE, "997", "996", "996,4242", "--"]
    );
    let plan = parse_args(command[2..].iter().map(OsString::from).collect()).unwrap();
    assert_eq!(plan.identity, identity);
    assert_eq!(
        plan.argv,
        wrapper.map(|arg| CString::new(arg).unwrap()).to_vec()
    );
    // An account with no supplementary groups passes an empty list.
    let bare = JobIdentity {
        uid: 5,
        gid: 6,
        groups: Vec::new(),
    };
    let command = exec_start(&bare, &["/bin/true"]);
    assert_eq!(command[4], "");
    assert_eq!(
        parse_args(command[2..].iter().map(OsString::from).collect())
            .unwrap()
            .identity,
        bare
    );
}

#[test]
fn init_arguments_refuse_root_and_malformed_commands() {
    for (args, why) in [
        (os(&["0", "5", "5", "--", "/bin/true"]), "root uid"),
        (os(&["5", "0", "5", "--", "/bin/true"]), "root gid"),
        (os(&["5", "5", "5,0", "--", "/bin/true"]), "group 0"),
        (os(&["5", "5", "5", "/bin/true"]), "missing --"),
        (os(&["5", "5", "5", "--"]), "no wrapper"),
        (os(&["5", "5", "5", "--", "bash"]), "relative wrapper"),
        (
            os(&["five", "5", "5", "--", "/bin/true"]),
            "non-numeric uid",
        ),
        (
            os(&["5", "5", "5,x", "--", "/bin/true"]),
            "non-numeric group",
        ),
        (os(&["5", "5"]), "truncated"),
    ] {
        assert!(parse_args(args).is_err(), "{why} must be refused");
    }
}

#[test]
fn exit_and_signal_statuses_forward_unchanged() {
    // waitpid status encoding: exit code in bits 8-15, signal in 0-6, core 7.
    for code in [0, 1, 23, 125, 137, 226, 255] {
        assert_eq!(forwarded(code << 8), Forward::Exit(code));
    }
    for signal in [libc::SIGKILL, libc::SIGTERM, libc::SIGSEGV] {
        assert_eq!(
            forwarded(signal),
            Forward::Signal {
                signal,
                core_dumped: false
            }
        );
        assert_eq!(
            forwarded(signal | 0x80),
            Forward::Signal {
                signal,
                core_dumped: true
            }
        );
    }
}

#[test]
fn supervisor_reproduces_real_exit_and_sigkill_wait_status() {
    for status in [
        0,
        23 << 8,
        125 << 8,
        137 << 8,
        226 << 8,
        libc::SIGKILL,
        libc::SIGTERM,
    ] {
        // SAFETY: the forked child only changes signal state and exits; it
        // neither allocates nor touches the test harness after fork.
        let observed = unsafe {
            let child = libc::fork();
            assert!(child >= 0);
            if child == 0 {
                forward(status);
            }
            let mut observed = 0;
            assert_eq!(libc::waitpid(child, &mut observed, 0), child);
            observed
        };
        assert_eq!(observed, status, "supervisor must preserve {status:#x}");
    }
}

#[test]
fn proc_submounts_are_the_direct_children_of_the_top_proc_mount() {
    // A job root's mounts: the old /proc, its protected submounts (one with a
    // nested mount of its own), the new proc at the staging directory, and a
    // name with an escaped space that only looks like /proc.
    let table = "\
20 1 0:5 / / ro - ext4 /dev/vda rw
25 20 0:22 / /proc rw,nosuid,nodev,noexec - proc proc rw,hidepid=invisible
26 25 0:22 /sys /proc/sys ro,nosuid,nodev,noexec - proc proc rw,hidepid=invisible
27 26 0:40 / /proc/sys/fs/binfmt_misc rw - autofs systemd-1 rw
28 25 0:22 /sysrq-trigger /proc/sysrq-trigger ro - proc proc rw
29 25 0:6 /null /proc/kcore ro - devtmpfs udev rw
30 20 0:41 / /dev/shm rw - tmpfs tmpfs rw
31 30 0:42 / /dev/shm/.motor-pid-init rw - proc proc rw,hidepid=invisible
32 20 0:43 / /proc\\040x rw - tmpfs tmpfs rw
";
    assert_eq!(
        proc_submounts(table).unwrap(),
        ["/proc/sys", "/proc/sysrq-trigger", "/proc/kcore"]
    );
    // A second proc stacked on /proc: only the top one's children count.
    let stacked = format!(
        "{table}40 25 0:50 / /proc rw - proc proc rw\n41 40 0:50 /sys /proc/sys ro - proc proc rw\n"
    );
    assert_eq!(proc_submounts(&stacked).unwrap(), ["/proc/sys"]);
    assert!(proc_submounts("20 1 0:5 / / ro - ext4 /dev/vda rw\n").is_err());
    assert!(proc_submounts("garbage\n").is_err());
    assert_eq!(unescape("/a\\040b\\134c\\011"), "/a b\\c\t");
    assert_eq!(unescape("/trailing\\04"), "/trailing\\04");
}

#[test]
fn a_runner_outside_the_installed_path_refuses_to_serve_boat_jobs() {
    // The test binary is not the installed runner.
    let error = check_installed_runner().unwrap_err().to_string();
    assert!(error.contains(INSTALLED_RUNNER), "{error}");
}

#[test]
fn a_missing_installed_runner_refuses_to_serve_boat_jobs() {
    let missing = cortexkit_test_support::scratch_root()
        .join(format!("missing-runner-{}", uuid::Uuid::now_v7()));
    let error = check_runner_path(&std::env::current_exe().unwrap(), &missing).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(
        error.to_string().contains(missing.to_str().unwrap()),
        "{error}"
    );
}

#[test]
fn loader_and_allocator_variables_are_refused() {
    assert!(refuse_loader_environment(["PATH", "HOME", "RUST_LOG", "CARGO_TERM_COLOR"]).is_ok());
    for name in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LD_AUDIT",
        "GLIBC_TUNABLES",
        "GCONV_PATH",
        "LOCPATH",
        "MALLOC_ARENA_MAX",
    ] {
        let error = refuse_loader_environment(["PATH", name]).unwrap_err();
        assert!(error.to_string().contains(name), "{error}");
    }
}

/// An errno the filter returns in tests, so a refusal from this filter is
/// told apart from the kernel's or another filter's EPERM.
const TEST_ERRNO: u16 = libc::EUCLEAN as u16;
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;

/// Run a classic-BPF seccomp program the way the kernel does, for the opcodes
/// the filter uses, recording every instruction executed.
fn evaluate(
    program: &[SockFilter],
    arch: u32,
    nr: u32,
    args: [u64; 2],
    seen: &mut BTreeSet<usize>,
    branches: &mut BTreeSet<(usize, usize)>,
) -> u32 {
    let mut data = [0u8; 64];
    data[0..4].copy_from_slice(&nr.to_ne_bytes());
    data[4..8].copy_from_slice(&arch.to_ne_bytes());
    for (i, arg) in args.iter().enumerate() {
        data[16 + 8 * i..24 + 8 * i].copy_from_slice(&arg.to_ne_bytes());
    }
    let (mut a, mut pc) = (0u32, 0usize);
    loop {
        seen.insert(pc);
        let insn = program[pc];
        let jump = |taken: bool| pc + 1 + usize::from(if taken { insn.jt } else { insn.jf });
        let next = match insn.code {
            BPF_LD_W_ABS => {
                let at = insn.k as usize;
                a = u32::from_ne_bytes(data[at..at + 4].try_into().unwrap());
                pc + 1
            }
            BPF_ALU_AND_K => {
                a &= insn.k;
                pc + 1
            }
            BPF_JMP_JEQ_K => jump(a == insn.k),
            BPF_JMP_JSET_K => jump(a & insn.k != 0),
            BPF_RET_K => return insn.k,
            code => panic!("unexpected opcode {code:#x} at {pc}"),
        };
        if matches!(insn.code, BPF_JMP_JEQ_K | BPF_JMP_JSET_K) {
            branches.insert((pc, next));
        }
        pc = next;
        assert!(pc < program.len(), "jump past the end of the program");
    }
}

#[test]
fn namespace_filter_refuses_exactly_new_pid_namespaces_on_every_path() {
    let program = namespace_filter(TEST_ERRNO);
    let deny = SECCOMP_RET_ERRNO | u32::from(TEST_ERRNO);
    let pid = u64::from(CLONE_NEWPID);
    let net = 0x4000_0000_u64;
    let sigchld = 17_u64;
    let x32 = X32_SYSCALL_BIT;
    let x86 = AUDIT_ARCH_X86_64;
    let i386 = AUDIT_ARCH_I386;
    let mut seen = BTreeSet::new();
    let mut branches = BTreeSet::new();
    for (name, arch, nr, args, refused) in [
        ("x86_64 unshare pid", x86, X86_64_UNSHARE, [pid, 0], true),
        (
            "x86_64 unshare pid+net",
            x86,
            X86_64_UNSHARE,
            [pid | net, 0],
            true,
        ),
        ("x86_64 unshare net", x86, X86_64_UNSHARE, [net, 0], false),
        (
            "x86_64 clone pid",
            x86,
            X86_64_CLONE,
            [pid | sigchld, 0],
            true,
        ),
        ("x86_64 clone", x86, X86_64_CLONE, [sigchld, 0], false),
        ("x86_64 setns pid", x86, X86_64_SETNS, [3, pid], true),
        ("x86_64 setns any", x86, X86_64_SETNS, [3, 0], true),
        ("x86_64 setns net", x86, X86_64_SETNS, [3, net], false),
        ("x86_64 other call", x86, 39, [pid, pid], false),
        (
            "x86_64 high argument bits",
            x86,
            X86_64_UNSHARE,
            [pid | 1 << 40, 0],
            true,
        ),
        ("x32 unshare pid", x86, X86_64_UNSHARE | x32, [pid, 0], true),
        ("x32 clone pid", x86, X86_64_CLONE | x32, [pid, 0], true),
        ("x32 setns any", x86, X86_64_SETNS | x32, [3, 0], true),
        ("x32 other call", x86, 39 | x32, [pid, 0], false),
        ("i386 unshare pid", i386, I386_UNSHARE, [pid, 0], true),
        ("i386 unshare net", i386, I386_UNSHARE, [net, 0], false),
        ("i386 clone pid", i386, I386_CLONE, [pid, 0], true),
        ("i386 clone", i386, I386_CLONE, [sigchld, 0], false),
        ("i386 setns pid", i386, I386_SETNS, [3, pid], true),
        ("i386 setns any", i386, I386_SETNS, [3, 0], true),
        ("i386 setns net", i386, I386_SETNS, [3, net], false),
        // i386 numbers 272 and 56 are other calls; only x86_64 uses them for
        // unshare and clone.
        ("i386 x86_64 numbers", i386, X86_64_UNSHARE, [pid, 0], false),
        ("i386 other call", i386, 20, [pid, 0], false),
        // Any other architecture is left to systemd's filter.
        (
            "aarch64 unshare pid",
            AUDIT_ARCH_AARCH64,
            97,
            [pid, 0],
            false,
        ),
    ] {
        let result = evaluate(&program, arch, nr, args, &mut seen, &mut branches);
        let expected = if refused { deny } else { SECCOMP_RET_ALLOW };
        assert_eq!(result, expected, "{name}");
    }
    let every: BTreeSet<_> = (0..program.len()).collect();
    assert_eq!(
        seen, every,
        "every instruction and jump target must be exercised"
    );
    let every_branch: BTreeSet<_> = program
        .iter()
        .enumerate()
        .filter(|(_, insn)| matches!(insn.code, BPF_JMP_JEQ_K | BPF_JMP_JSET_K))
        .flat_map(|(pc, insn)| {
            [
                (pc, pc + 1 + usize::from(insn.jt)),
                (pc, pc + 1 + usize::from(insn.jf)),
            ]
        })
        .collect();
    assert_eq!(branches, every_branch, "both arms of every jump must run");
    // The production program differs only in the errno it returns.
    let production = namespace_filter(libc::EPERM as u16);
    assert_eq!(production.len(), program.len());
    for (real, tested) in production.iter().zip(&program) {
        if tested.k == deny {
            assert_eq!(real.k, SECCOMP_RET_ERRNO | libc::EPERM as u32);
        } else {
            assert_eq!(real, tested);
        }
    }
}

/// Install the filter in a forked child and make real system calls. The child
/// exits 0 when every call behaves as expected, or with the number of the
/// first unexpected one. x32 and i386 calls run in grandchildren: a kernel
/// without that emulation, or another seccomp filter that kills foreign
/// architectures (systemd's `SystemCallArchitectures=native`, which Boat's
/// tenant units set), ends them with a signal instead of returning. Those
/// paths are then covered only by the interpreter test above.
#[cfg(target_arch = "x86_64")]
#[test]
fn installed_filter_refuses_new_pid_namespaces_in_a_real_process() {
    const X32_UNAVAILABLE: i32 = 1 << 5;
    const I386_UNAVAILABLE: i32 = 1 << 6;
    let program = namespace_filter(TEST_ERRNO);
    let filter = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    // SAFETY: the child runs only system calls and _exit; nothing allocates
    // after fork.
    let status = unsafe {
        let child = libc::fork();
        assert!(child >= 0);
        if child == 0 {
            if libc::prctl(PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) != 0 {
                libc::_exit(30);
            }
            if libc::prctl(
                PR_SET_SECCOMP,
                SECCOMP_MODE_FILTER,
                &filter as *const SockFprog,
            ) != 0
            {
                libc::_exit(31);
            }
            let ours = libc::c_int::from(TEST_ERRNO);
            let refused_here =
                |result: libc::c_long| result == -1 && *libc::__errno_location() == ours;
            // errno belongs to the most recent syscall, not to its saved
            // return value. Capture both before evaluating the next call.
            let observed = |result: libc::c_long| (result, *libc::__errno_location());
            let pid = libc::c_long::from(CLONE_NEWPID);
            let checks = [
                (observed(libc::syscall(libc::SYS_unshare, pid)), true),
                (
                    observed(libc::syscall(libc::SYS_unshare, libc::CLONE_NEWUTS)),
                    false,
                ),
                (
                    observed(libc::syscall(
                        libc::SYS_clone,
                        pid | libc::CLONE_THREAD as libc::c_long,
                        0,
                        0,
                        0,
                        0,
                    )),
                    true,
                ),
                // CLONE_THREAD without CLONE_SIGHAND is EINVAL, so nothing forks.
                (
                    observed(libc::syscall(
                        libc::SYS_clone,
                        libc::CLONE_THREAD,
                        0,
                        0,
                        0,
                        0,
                    )),
                    false,
                ),
                (observed(libc::syscall(libc::SYS_setns, -1, pid)), true),
                (observed(libc::syscall(libc::SYS_setns, -1, 0)), true),
                (
                    observed(libc::syscall(libc::SYS_setns, -1, libc::CLONE_NEWNET)),
                    false,
                ),
            ];
            for (index, ((result, errno), refused)) in checks.into_iter().enumerate() {
                if (result == -1 && errno == ours) != refused {
                    libc::_exit(index as libc::c_int + 1);
                }
            }
            // Runs `check` in a grandchild: Some(passed), or None when the
            // grandchild was killed before it could answer.
            let isolated = |check: &dyn Fn() -> bool| -> Option<bool> {
                let grandchild = libc::fork();
                if grandchild == 0 {
                    libc::_exit(if check() { 0 } else { 1 });
                }
                let mut status = 0;
                libc::waitpid(grandchild, &mut status, 0);
                libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status) == 0)
            };
            let x32 = isolated(&|| {
                let x32_unshare = libc::c_long::from(X86_64_UNSHARE | X32_SYSCALL_BIT);
                refused_here(libc::syscall(x32_unshare, pid))
            });
            let i386 = isolated(&|| {
                let call = |nr: u32, arg: u32| -> i32 {
                    let result: i32;
                    std::arch::asm!(
                        "push rbx",
                        "mov ebx, {arg:e}",
                        "int 0x80",
                        "pop rbx",
                        arg = in(reg) arg,
                        inlateout("eax") nr as i32 => result,
                        out("r8") _, out("r9") _, out("r10") _, out("r11") _,
                    );
                    result
                };
                call(I386_UNSHARE, CLONE_NEWPID) == -ours
                    && call(I386_UNSHARE, libc::CLONE_NEWUTS as u32) != -ours
            });
            if x32 == Some(false) {
                libc::_exit(20);
            }
            if i386 == Some(false) {
                libc::_exit(21);
            }
            libc::_exit(
                if x32.is_none() { X32_UNAVAILABLE } else { 0 }
                    | if i386.is_none() { I386_UNAVAILABLE } else { 0 },
            );
        }
        let mut status = 0;
        assert_eq!(libc::waitpid(child, &mut status, 0), child);
        status
    };
    assert!(
        libc::WIFEXITED(status),
        "filter test child died: {status:#x}"
    );
    let code = libc::WEXITSTATUS(status);
    assert_eq!(
        code & !(X32_UNAVAILABLE | I386_UNAVAILABLE),
        0,
        "filter test check {code} behaved unexpectedly"
    );
    for (bit, name) in [(X32_UNAVAILABLE, "x32"), (I386_UNAVAILABLE, "i386")] {
        if code & bit != 0 {
            eprintln!(
                "{name} system calls cannot run here; that path is checked by the interpreter only"
            );
        }
    }
}
