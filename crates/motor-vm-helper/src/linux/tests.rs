use super::*;
use crate::{Identity, Peer};
use std::os::unix::fs::symlink;
#[cfg(feature = "linux-service-tests")]
use std::os::unix::{fs::MetadataExt, process::CommandExt};

fn scratch(name: &str) -> PathBuf {
    let p = cortexkit_test_support::scratch_root()
        .join(format!("vm-helper-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn trust_refusal_names_the_exact_path_owner_and_mode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let path = scratch("trust-diagnostic");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    let error = fs::trusted(&path, &metadata).unwrap_err().0;
    assert!(error.contains(&format!("path={}", path.display())));
    assert!(error.contains(&format!("uid={}", metadata.uid())));
    assert!(error.contains(&format!("gid={}", metadata.gid())));
    assert!(error.contains("mode=0777"));
    assert!(error.contains("symlink=false"));
    std::fs::remove_dir(path).unwrap();
}

#[test]
fn trust_refusal_names_the_symlink_not_its_target() {
    let root = scratch("trust-link");
    let path = root.join("loader");
    symlink("real-loader", &path).unwrap();
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    let error = fs::trusted(&path, &metadata).unwrap_err().0;
    assert!(error.contains(&format!("path={}", path.display())));
    assert!(error.contains("symlink=true"));
    assert!(error.contains("mode=0777"));
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(root).unwrap();
}
fn peer() -> Peer {
    Peer {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    }
}
fn identity() -> Identity {
    Identity {
        instance: "test".into(),
        job_id: uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap(),
    }
}
fn job_path(base: &Path) -> PathBuf {
    base.join(format!("test/jobs/{}", identity().job_id))
}

fn capacity_machine(base: &Path) -> LinuxMachine {
    let mut config: crate::config::Config =
        serde_json::from_str(include_str!("../../fixtures/helper.json")).unwrap();
    config.hugepages_per_slot = 4;
    let tenants =
        crate::tenant::Controller(tenant::LinuxTenants::new(&config, base.join("cgroups")));
    LinuxMachine {
        config,
        base: File::open(base).unwrap(),
        base_path: base.into(),
        cgroups: cgroup::Cgroups {
            root: base.join("cgroups"),
        },
        running: BTreeMap::new(),
        tenants,
    }
}

// The capacity gate is the one start() uses. Only the privileged continuation
// is replaced: it creates real directories and a marker without needing KVM,
// mounts, root-owned snapshots, cgroup delegation or reserved hugepages.
fn capacity_preparation(machine: &LinuxMachine) -> Result<()> {
    std::fs::create_dir_all(machine.boundary(0).parent().unwrap())?;
    create_boundary(&machine.boundary(0))?;
    std::fs::create_dir_all(machine.root(0))?;
    std::fs::create_dir_all(machine.cgroups.slot(0))?;
    std::fs::write(machine.base_path.join("prepared"), "prepared")?;
    Ok(())
}

#[test]
fn hugepage_shortage_refuses_before_vm_preparation() {
    let base = scratch("hugepage-shortage");
    let machine = capacity_machine(&base);
    let reads = std::cell::Cell::new(0);
    let result = machine.prepare(
        || {
            reads.set(reads.get() + 1);
            Ok(3)
        },
        || capacity_preparation(&machine),
    );
    let entries = std::fs::read_dir(&base).unwrap().count();
    let jail = machine.boundary(0).exists();
    let chroot = machine.root(0).exists();
    let cgroup = machine.cgroups.slot(0).exists();
    let prepared = base.join("prepared").exists();
    std::fs::remove_dir_all(base).unwrap();
    assert_eq!(
        result.unwrap_err().0,
        "vm_capacity: not enough HugePages_Free"
    );
    assert_eq!(reads.get(), 1);
    assert_eq!(
        entries, 0,
        "shortage must not create any preparation output"
    );
    assert!(!jail && !chroot && !cgroup && !prepared);
}

#[test]
fn sufficient_hugepages_proceed_with_vm_preparation() {
    for free in [4, 5] {
        let base = scratch(&format!("hugepage-sufficient-{free}"));
        let machine = capacity_machine(&base);
        let reads = std::cell::Cell::new(0);
        machine
            .prepare(
                || {
                    reads.set(reads.get() + 1);
                    Ok(free)
                },
                || capacity_preparation(&machine),
            )
            .unwrap();
        assert_eq!(reads.get(), 1);
        assert!(machine.boundary(0).is_dir());
        assert!(machine.root(0).is_dir());
        assert!(machine.cgroups.slot(0).is_dir());
        assert_eq!(
            std::fs::read_to_string(base.join("prepared")).unwrap(),
            "prepared"
        );
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[test]
fn unreadable_hugepage_pool_refuses_before_vm_preparation() {
    let base = scratch("hugepage-unreadable");
    let machine = capacity_machine(&base);
    let result = machine.prepare(
        || Err(Error("HugePages_Free missing".into())),
        || capacity_preparation(&machine),
    );
    let entries = std::fs::read_dir(&base).unwrap().count();
    std::fs::remove_dir_all(base).unwrap();
    assert_eq!(result.unwrap_err().0, "HugePages_Free missing");
    assert_eq!(entries, 0, "an unreadable pool must not use a fallback");
}

#[test]
fn openat2_rejects_symlink_images_and_job_directory_swaps() {
    let base = scratch("symlinks");
    let job = job_path(&base);
    std::fs::create_dir_all(&job).unwrap();
    let canary = base.join("canary");
    std::fs::create_dir(&canary).unwrap();
    std::fs::write(canary.join("secret"), "untouched").unwrap();
    symlink(canary.join("secret"), job.join("tree.in")).unwrap();
    let fd = File::open(&base).unwrap();
    let pinned = fs::JobDir::open(&fd, &identity(), peer()).unwrap();
    assert!(pinned.source("tree.in", 9).is_err());
    std::fs::rename(&job, job.with_extension("old")).unwrap();
    symlink(&canary, &job).unwrap();
    assert!(pinned.verify().is_err());
    assert!(fs::JobDir::open(&fd, &identity(), peer()).is_err());
    assert!(pinned
        .export(&[("tree.out", File::open(canary.join("secret")).unwrap())])
        .is_err());
    assert!(!canary.join("tree.out").exists());
    assert_eq!(
        std::fs::read_to_string(canary.join("secret")).unwrap(),
        "untouched"
    );
    std::fs::remove_dir_all(base).unwrap();
}
#[test]
fn images_must_be_regular_and_match_snapshot_record_size() {
    let base = scratch("sizes");
    let job = job_path(&base);
    std::fs::create_dir_all(job.join("tree.in")).unwrap();
    let fd = File::open(&base).unwrap();
    let pinned = fs::JobDir::open(&fd, &identity(), peer()).unwrap();
    assert!(pinned.source("tree.in", 1).is_err());
    std::fs::write(job.join("target.in"), "abcd").unwrap();
    assert!(pinned
        .source("target.in", 3)
        .unwrap_err()
        .0
        .contains("vm_image_size"));
    assert_eq!(
        pinned
            .source("target.in", 4)
            .unwrap()
            .metadata()
            .unwrap()
            .len(),
        4
    );
    std::fs::remove_dir_all(base).unwrap();
}
#[test]
fn output_symlink_is_refused_without_touching_its_target() {
    let base = scratch("output");
    let job = job_path(&base);
    std::fs::create_dir_all(&job).unwrap();
    let canary = base.join("canary");
    std::fs::write(&canary, "untouched").unwrap();
    symlink(&canary, job.join("tree.out")).unwrap();
    let fd = File::open(&base).unwrap();
    let pinned = fs::JobDir::open(&fd, &identity(), peer()).unwrap();
    assert!(pinned
        .export(&[("tree.out", File::open(&canary).unwrap())])
        .is_err());
    assert_eq!(std::fs::read_to_string(&canary).unwrap(), "untouched");
    std::fs::remove_dir_all(base).unwrap();
}
#[test]
fn filesystem_credentials_are_restored_on_error() {
    let before = peer();
    let _ = fs::as_peer(before, || Err::<(), _>(Error("intentional refusal".into())));
    assert_eq!(unsafe { libc::setfsuid(u32::MAX) } as u32, before.uid);
    assert_eq!(unsafe { libc::setfsgid(u32::MAX) } as u32, before.gid);
}

#[test]
fn fresh_boundary_is_private_and_existing_jail_is_refused() {
    use std::os::unix::fs::MetadataExt;
    let base = scratch("private-jail");
    let jail = base.join("slot-0");
    create_boundary(&jail).unwrap();
    assert_eq!(std::fs::metadata(&jail).unwrap().mode() & 0o777, 0o700);
    std::fs::write(jail.join("canary"), "untouched").unwrap();
    assert!(create_boundary(&jail).is_err());
    assert_eq!(
        std::fs::read_to_string(jail.join("canary")).unwrap(),
        "untouched"
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn beneath_refuses_parent_traversal() {
    let base = scratch("traversal");
    std::fs::create_dir(base.join("dir")).unwrap();
    std::fs::write(base.join("canary"), "untouched").unwrap();
    let dir = File::open(base.join("dir")).unwrap();
    assert!(fs::beneath(dir.as_raw_fd(), "../canary", libc::O_RDONLY, 0).is_err());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn intermediate_job_symlink_is_refused() {
    let base = scratch("intermediate-symlink");
    let canary = base.join("canary");
    std::fs::create_dir_all(canary.join(identity().job_id.to_string())).unwrap();
    std::fs::create_dir(base.join("test")).unwrap();
    symlink("../canary", base.join("test/jobs")).unwrap();
    let fd = File::open(&base).unwrap();
    assert!(fs::JobDir::open(&fd, &identity(), peer()).is_err());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn peer_credentials_and_agent_fd_transfer_use_real_unix_sockets() {
    let (mut helper, runner) = UnixStream::pair().unwrap();
    assert_eq!(server::peer(&helper).unwrap().uid, peer().uid);
    let (agent, mut guest) = UnixStream::pair().unwrap();
    server::send_response(
        &mut helper,
        crate::Response {
            reply: crate::Reply::Started { fd_count: 1 },
            fds: vec![agent.into()],
        },
    )
    .unwrap();
    let mut bytes = [0u8; 256];
    let mut control = [0usize; 16];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    use std::os::fd::FromRawFd;
    let (n, fd) = unsafe {
        let n = libc::recvmsg(runner.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        assert!(n > 0);
        assert_eq!(msg.msg_flags & libc::MSG_CTRUNC, 0);
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        assert!(!cmsg.is_null());
        assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
        (n as usize, *(libc::CMSG_DATA(cmsg).cast::<i32>()))
    };
    assert!(std::str::from_utf8(&bytes[..n])
        .unwrap()
        .contains("\"fd_count\":1"));
    let mut received = unsafe { UnixStream::from_raw_fd(fd) };
    guest.write_all(b"agent").unwrap();
    let mut response = [0; 5];
    received.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"agent");
}

#[test]
#[cfg(feature = "linux-root-tests")]
fn beneath_refuses_cross_device_mount() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = scratch("cross-device");
    let mount = base.join("mounted");
    std::fs::create_dir(&mount).unwrap();
    let target = CString::new(mount.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(
        unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                target.as_ptr(),
                c"tmpfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        },
        0
    );
    std::fs::write(mount.join("canary"), "untouched").unwrap();
    let dir = File::open(&base).unwrap();
    let refused = fs::beneath(dir.as_raw_fd(), "mounted/canary", libc::O_RDONLY, 0).is_err();
    fs::unmount(&mount).unwrap();
    std::fs::remove_dir_all(base).unwrap();
    assert!(refused);
}
#[test]
#[cfg(feature = "linux-root-tests")]
fn foreign_owned_image_is_refused_under_caller_credentials() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = scratch("ownership");
    let job = job_path(&base);
    std::fs::create_dir_all(&job).unwrap();
    // Use a real unprivileged uid, not root masquerading as an authorised caller.
    let uid = 200001;
    for p in [&base, &base.join("test"), &base.join("test/jobs"), &job] {
        let file = File::open(p).unwrap();
        fs::chown(&file, uid).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(job.join("tree.in"), "abcd").unwrap();
    std::fs::set_permissions(job.join("tree.in"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let fd = File::open(&base).unwrap();
    let pinned = fs::JobDir::open(&fd, &identity(), Peer { uid, gid: uid }).unwrap();
    assert!(pinned
        .source("tree.in", 4)
        .unwrap_err()
        .0
        .contains("owned by caller"));
    assert_eq!(unsafe { libc::setfsuid(u32::MAX) }, 0);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
#[cfg(feature = "linux-root-tests")]
fn unreadable_caller_image_cannot_use_root_dac_override() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = scratch("unreadable-caller-image");
    let job = job_path(&base);
    std::fs::create_dir_all(&job).unwrap();
    let uid = 200001;
    for path in [&base, &base.join("test"), &base.join("test/jobs"), &job] {
        fs::chown(&File::open(path).unwrap(), uid).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let image = job.join("tree.in");
    std::fs::write(&image, "abcd").unwrap();
    fs::chown(&File::open(&image).unwrap(), uid).unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0)).unwrap();
    let fd = File::open(&base).unwrap();
    let pinned = fs::JobDir::open(&fd, &identity(), Peer { uid, gid: uid }).unwrap();
    assert!(pinned.source("tree.in", 4).is_err());
    assert_eq!(unsafe { libc::setfsuid(u32::MAX) }, 0);
    std::fs::remove_dir_all(base).unwrap();
}

/// Runs the actual test executable in a transient systemd service. Assertions
/// inspect the kernel's cgroup tree and /proc, not a simulated filesystem.
#[test]
#[cfg(feature = "linux-service-tests")]
fn cgroup_placement_service() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    if std::env::var_os("MOTOR_VM_HELPER_IN_SERVICE").is_none() {
        // Run with the shipped unit's properties, not a second copy that could
        // stay green while the production service loses its delegation leaf.
        let unit = include_str!("../../systemd/motor-vm-helper.service");
        let properties: Vec<_> = ["Delegate=", "DelegateSubgroup=", "KillMode="]
            .iter()
            .map(|prefix| {
                let line = unit.lines().find(|line| line.starts_with(prefix)).unwrap();
                format!("--property={line}")
            })
            .collect();
        let status = Command::new("systemd-run")
            .args([
                "--wait",
                "--pipe",
                "--collect",
                "--unit=motor-vm-helper-placement-test",
                "--setenv=MOTOR_VM_HELPER_IN_SERVICE=1",
            ])
            .args(properties)
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "linux::tests::cgroup_placement_service",
                "--nocapture",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let groups = cgroup::Cgroups::discover().unwrap();
    assert!(std::fs::read_to_string(groups.root.join("cgroup.procs"))
        .unwrap()
        .trim()
        .is_empty());
    let leaf = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    assert!(leaf.trim().ends_with("/helper"));
    groups.enable().unwrap();
    let config = crate::config::SlotConfig {
        cpus: std::fs::read_to_string(groups.root.join("cpuset.cpus.effective"))
            .unwrap()
            .trim()
            .into(),
        mems: std::fs::read_to_string(groups.root.join("cpuset.mems.effective"))
            .unwrap()
            .trim()
            .into(),
        cpu_max: "100000 100000".into(),
        memory_max: 134217728,
        io_weight: 100,
        io_max: None,
    };
    groups.create(0, &config).unwrap();
    let procs = OpenOptions::new()
        .write(true)
        .open(groups.slot(0).join("cgroup.procs"))
        .unwrap();
    let mut child = Command::new("/bin/sleep");
    unsafe {
        child.pre_exec(move || {
            raw_place(procs.as_raw_fd())?;
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(200000) != 0
                || libc::setuid(200000) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.arg("60").spawn().unwrap();
    let placement = std::fs::read_to_string(format!("/proc/{}/cgroup", child.id())).unwrap();
    assert!(placement.trim().ends_with("/slots/slot-0"));
    assert!(
        std::fs::read_to_string(groups.slot(0).join("cgroup.subtree_control"))
            .unwrap()
            .trim()
            .is_empty()
    );
    for parent in [&groups.root, &groups.root.join("slots")] {
        assert!(std::fs::read_to_string(parent.join("cgroup.procs"))
            .unwrap()
            .trim()
            .is_empty());
        let controllers = std::fs::read_to_string(parent.join("cgroup.subtree_control")).unwrap();
        for name in ["cpu", "cpuset", "memory", "io"] {
            assert!(controllers.split_whitespace().any(|n| n == name));
        }
    }
    // Exercise the real startup barrier against an on-disk jail, a populated
    // slot cgroup, and an escaped slot-uid process in the helper leaf.
    let base = scratch("reconcile-service");
    let jail = base.join("jail/slot-0/firecracker/slot-0/root");
    std::fs::create_dir_all(&jail).unwrap();
    std::fs::write(jail.join("stale"), "stale").unwrap();
    let mut escaped = Command::new("/bin/sleep")
        .arg("60")
        .uid(200000)
        .gid(200000)
        .spawn()
        .unwrap();
    let mut settings: crate::config::Config =
        serde_json::from_str(include_str!("../../fixtures/helper.json")).unwrap();
    settings.slots = vec![config];
    let machine = LinuxMachine {
        config: settings,
        base: File::open(&base).unwrap(),
        base_path: base.clone(),
        cgroups: groups,
        running: BTreeMap::new(),
    };
    let mut helper = crate::Helper::new(machine, 1, vec![1000]).unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(!escaped.wait().unwrap().success());
    assert!(!jail.exists());
    assert!(!helper.machine.cgroups.slot(0).exists());
    let connection = helper
        .connect(Peer {
            uid: 1000,
            gid: 1000,
        })
        .unwrap();
    let response = helper
        .handle(
            connection,
            crate::Request::Acquire {
                instance: "a".into(),
                job_id: identity().job_id.to_string(),
            },
        )
        .unwrap();
    assert!(matches!(
        response.reply,
        crate::Reply::Acquired { slot: Some(0) }
    ));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
#[cfg(feature = "linux-service-tests")]
fn official_jailer_creates_documented_root() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let jailer = std::env::var_os("MOTOR_VM_TEST_JAILER")
        .expect("set MOTOR_VM_TEST_JAILER to the official jailer");
    let fixture = std::env::var_os("MOTOR_VM_TEST_FIRECRACKER")
        .expect("set MOTOR_VM_TEST_FIRECRACKER to the static no-KVM fixture named firecracker");
    let base = scratch("jailer-layout");
    // The fixture writes /root-observed inside its root and exits. Verify the
    // artifact produced by jailer, not just the helper's path calculation.
    let status = Command::new(jailer)
        .args([
            "--id",
            "slot-0",
            "--uid",
            "200000",
            "--gid",
            "200000",
            "--chroot-base-dir",
        ])
        .arg(&base)
        .arg("--exec-file")
        .arg(fixture)
        .status()
        .unwrap();
    assert!(status.success());
    let root = base.join("firecracker/slot-0/root");
    assert!(root.join("firecracker").is_file());
    let observed = std::fs::read_to_string(root.join("root-observed")).unwrap();
    assert_eq!(observed, "uid=200000 gid=200000 root=/\n");
    assert_eq!(
        std::fs::metadata(root.join("root-observed")).unwrap().uid(),
        200000
    );
    std::fs::remove_dir_all(base).unwrap();
}
