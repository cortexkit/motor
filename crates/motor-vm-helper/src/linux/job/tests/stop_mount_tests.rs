//! Root-only: stop releases a job whose jail root holds the production bind
//! mounts while a later job is running, inside the private mount namespace
//! that the isolated job tests use. No KVM, jailer, cgroup or network.
use super::*;

/// Mount points at or below `boundary`, as listed by one mountinfo file.
fn mounts_under(mountinfo: &str, boundary: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(mountinfo)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(crate::linux::unescape_mount)
        .filter(|path| path.starts_with(boundary))
        .collect()
}

/// Builds the jail root as production start does (`LinuxJobs::start`): the
/// boundary, a uid-owned root, read-only image binds and a writable disk
/// bind. Then starts a stand-in VMM blocked on its stdin pipe in its own
/// copied mount namespace with slave propagation, as the jailer makes one.
/// Unlike the jailer, it never pivots away from the copied tree, so it keeps
/// a copy of every jail mount that exists when it starts.
fn launch(jobs: &LinuxJobs, images: &[(File, &str)], n: u32) -> Start {
    let mut start = start();
    start.job_id = job::test_id(n.into());
    start.workspace_disk_id = format!("stop-mount-{n}");
    let uid = 600000 + n;
    let record = start_record(jobs, start.clone(), uid, None);
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            jobs.workspaces
                .join(&start.workspace_disk_id)
                .join("workspace.img"),
        )
        .unwrap();
    let admission = jobs.admission.lock();
    jobs.launch_record(admission, record, |state| {
        let boundary = jobs.boundary(&start.job_id);
        let root = jobs.root(&start.job_id);
        crate::linux::create_boundary(&boundary)?;
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        fs::chown(&File::open(&root)?, uid)?;
        for (file, name) in images {
            tenant::bind(file, &root.join(name), true)?;
        }
        fs::chown(&disk, uid)?;
        tenant::bind(&disk, &root.join("workspace.img"), false)?;
        let child = Command::new("/usr/bin/unshare")
            .args(["--mount", "--propagation", "slave", "/bin/sh", "-c"])
            .arg("read -r _")
            .stdin(Stdio::piped())
            .spawn()?;
        state.child = Some(child);
        Ok(())
    })
    .unwrap();
    start
}

fn kill_child(jobs: &LinuxJobs, id: &str) {
    let cell = jobs.cell(id).unwrap();
    let mut state = cell.0.lock();
    state.child.as_mut().unwrap().kill().unwrap();
}

/// Checks only what stop must release. Collection without a cgroup is the
/// same code path the cgroup-less unit fixtures use.
fn assert_released(jobs: &LinuxJobs, start: &Start) {
    assert!(!jobs.boundary(&start.job_id).exists());
    assert_eq!(jobs.view(&start.job_id).unwrap().state, State::Released);
    let durable: Record = serde_json::from_reader(
        File::open(jobs.records_path.join(format!("{}.json", start.job_id))).unwrap(),
    )
    .unwrap();
    assert!(durable.released);
    let disk = std::fs::metadata(
        jobs.workspaces
            .join(&start.workspace_disk_id)
            .join("workspace.img"),
    )
    .unwrap();
    assert_eq!((disk.uid(), disk.mode() & 0o777), (0, 0o600));
}

/// Run as the root operator outside remote jobs. Needs mount, unshare and a
/// root-owned shared scratch root; no KVM, Firecracker or jailer.
#[test]
fn isolated_stop_releases_jail_binds_while_a_later_job_runs() {
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as the root operator");
    let _isolation = namespace::root_tests::Fixture::new();
    let base = scratch();
    let jobs = machine(&base);
    for path in [&jobs.jail, &jobs.records_path, &jobs.workspaces] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let image_dir = base.join("images");
    std::fs::create_dir(&image_dir).unwrap();
    let images: Vec<_> = ["kernel", "rootfs.ext4", "toolchain.ext4"]
        .into_iter()
        .map(|name| {
            let path = image_dir.join(name);
            std::fs::write(&path, name).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            (File::open(&path).unwrap(), name)
        })
        .collect();
    let first = launch(&jobs, &images, 201);
    let second = launch(&jobs, &images, 202);
    kill_child(&jobs, &first.job_id);
    jobs.collect_exit(&first.job_id).unwrap();
    assert_eq!(jobs.view(&first.job_id).unwrap().state, State::Exited);
    // The binds exist in this thread's mount namespace. Name both views so a
    // failure shows which mount table the cleanup could have read.
    let boundary = jobs.boundary(&first.job_id);
    eprintln!(
        "stop_mounts pid={} tid={} thread_view={:?} process_view={:?}",
        std::process::id(),
        unsafe { libc::gettid() },
        mounts_under("/proc/thread-self/mountinfo", &boundary),
        mounts_under("/proc/self/mountinfo", &boundary),
    );
    jobs.stop(&first.job_id).unwrap();
    assert_released(&jobs, &first);
    assert!(mounts_under("/proc/thread-self/mountinfo", &boundary).is_empty());
    kill_child(&jobs, &second.job_id);
    jobs.stop(&second.job_id).unwrap();
    assert_released(&jobs, &second);
    drop(images);
    std::fs::remove_dir_all(base).unwrap();
}
