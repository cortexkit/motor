use super::*;
use std::sync::mpsc;
use std::time::Instant;

mod boot;
mod fixture;

fn peer() -> Peer {
    Peer { uid: 0, gid: 0 }
}

fn reports(jobs: &LinuxJobs) -> Vec<ExitReport> {
    match jobs.handle(Operation::Exits, peer()).unwrap().reply {
        Reply::JobExits { exits } => exits,
        reply => panic!("unexpected exit poll: {reply:?}"),
    }
}

// Observe both read orders without making populated=0 an exit-proof condition.
// Missing files mean phase 2 has already removed the empty cgroup.
fn log_cgroup_signals(group: &Path, killed: Instant) {
    loop {
        let procs_first = std::fs::read_to_string(group.join("cgroup.procs"));
        let events_second = std::fs::read_to_string(group.join("cgroup.events"));
        let events_first = std::fs::read_to_string(group.join("cgroup.events"));
        let procs_second = std::fs::read_to_string(group.join("cgroup.procs"));
        eprintln!(
            "exit_signals timestamp={:?} since_kill_us={} procs_then_events=({procs_first:?}, {events_second:?}) events_then_procs=({events_first:?}, {procs_second:?})",
            SystemTime::now(), killed.elapsed().as_micros()
        );
        let empty = |procs: &std::io::Result<String>, events: &std::io::Result<String>| {
            matches!((procs, events), (Ok(procs), Ok(events))
                if procs.trim().is_empty() && events.lines().any(|line| line == "populated 0"))
        };
        if empty(&procs_first, &events_second) || empty(&procs_second, &events_first) {
            return;
        }
        if [&procs_first, &events_second, &events_first, &procs_second]
            .iter()
            .any(
                |value| matches!(value, Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            )
        {
            return;
        }
        if killed.elapsed() > Duration::from_secs(10) {
            eprintln!("exit_signals observation ended without both signals; informational only");
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn nft_objects() -> Result<BTreeSet<String>> {
    let output = Command::new("/usr/sbin/nft")
        .args(["-j", "list", "ruleset"])
        .output()?;
    if !output.status.success() {
        return Err(Error(format!(
            "list private nft objects: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let document: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| Error(error.to_string()))?;
    let mut names = BTreeSet::new();
    for object in document["nftables"]
        .as_array()
        .ok_or_else(|| Error("missing nftables array".into()))?
    {
        for kind in ["chain", "counter"] {
            if object[kind]["table"] == "motor_tenant" {
                if let Some(name) = object[kind]["name"].as_str() {
                    names.insert(name.into());
                }
            }
        }
    }
    Ok(names)
}

fn tap_objects(index: u8) -> BTreeSet<String> {
    ["input", "forward", "nat", "egress"]
        .map(|suffix| format!("tenant_mtap{index}_{suffix}"))
        .into()
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "collector phase 2 did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Run as the root operator outside remote jobs, with KVM, cgroup v2, systemd,
/// iproute2, nft and mount available. No live helper IPC or state is used.
/// Set TMPDIR to root-owned, UID-traversable shared scratch (not below /root).
/// Set MOTOR_VM_TEST_FIRECRACKER and MOTOR_VM_TEST_JAILER to the 1.17.0 binaries,
/// and MOTOR_VM_TEST_JOB_{KERNEL,ROOTFS,TOOLCHAIN} to absolute paths of the
/// kernel, rootfs.ext4 and toolchain.ext4 from one Job image generation emitted
/// by script/tenant-image/build.py build. Set MOTOR_VM_TEST_JOB_MEMORY_MIB to
/// that image's smallest bootable RAM size. Missing prerequisites fail, not skip.
/// The guests use two vCPUs each, no burst RAM and disposable sparse disks.
///
/// cargo nextest run -p motor-vm-helper --features linux-service-tests --lib \
///   -E 'test(=linux::job::tests::real_exit_tests::isolated_job_exit_is_reported_while_another_job_boots)' --no-capture
#[test]
fn isolated_job_exit_is_reported_while_another_job_boots() {
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as the root operator");
    let _isolation = namespace::root_tests::Fixture::new();
    let base = scratch();
    let slice = format!("motorvmexit{}.slice", std::process::id());
    let config: Config =
        serde_json::from_str(include_str!("../../../../fixtures/helper.json")).unwrap();
    let mut jobs = machine_with_config(&base, &config);
    jobs.groups = Path::new("/sys/fs/cgroup").join(&slice);
    jobs.remove_test_group = None;
    jobs.network = Box::new(fixture::PrivateNetwork {
        base: base.clone(),
        system: network::System(config),
    });
    let mut resources = fixture::Resources {
        jobs: Arc::new(jobs),
        base: base.clone(),
        slice,
        releases: Vec::new(),
        boot: None,
        observers: Vec::new(),
    };
    for ancestor in base.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor).unwrap()).unwrap();
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    let artifacts = Arc::new(boot::Artifacts::new(&base));
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("read/write /dev/kvm required");
    Arc::get_mut(&mut resources.jobs).unwrap().settings = Some(JobVms {
        workspace_disk_max_bytes: Some(4 << 30),
        fixed_path_repositories: Default::default(),
        uid_base: 600000,
        uid_count: 254,
        memory_max_bytes: (u64::from(artifacts.memory_mib) + 128) << 21,
    });
    // Hide persistent UFW files too: production removal must not discover a
    // live route grant even when this private netns reuses its numeric TAP slot.
    if Path::new("/etc/ufw").exists() {
        let empty = base.join("empty-ufw");
        std::fs::create_dir(&empty).unwrap();
        fixture::command(
            "/usr/bin/mount",
            &["--bind", empty.to_str().unwrap(), "/etc/ufw"],
        );
    }
    fixture::command("/usr/sbin/ip", &["link", "add", "public0", "type", "dummy"]);
    fixture::command("/usr/sbin/ip", &["link", "set", "public0", "up"]);
    fixture::command("systemctl", &["start", &resources.slice]);
    cgroup::enable(&resources.jobs.groups).unwrap();
    let jobs = resources.jobs.clone();
    let watcher = jobs.start_exit_watcher();
    let request = |n| {
        let mut request = start();
        request.job_id = job::test_id(n);
        request.workspace_disk_id = format!("real-exit-{n}");
        request
    };
    let first = request(101);
    let second = request(102);
    let third = request(103);
    let first_reply = boot::boot(&jobs, &artifacts, first.clone()).unwrap().reply;
    let pid = match first_reply {
        Reply::Job {
            running: true,
            vmm_pid: Some(pid),
            ..
        } => pid,
        reply => panic!("first real VM did not start: {reply:?}"),
    };
    boot::guest_ready(&jobs, &first.job_id);
    let index = jobs.view(&first.job_id).unwrap().record.tap_index.unwrap();
    assert!(namespace::path(&first.job_id).unwrap().exists());
    assert!(tap_objects(index).is_subset(&nft_objects().unwrap()));
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(ExitStep::Api, move || {
        entered.send(()).unwrap();
        released
            .recv_timeout(Duration::from_secs(180))
            .expect("release slow InstanceStart completion");
    });
    resources.releases.push(release);
    let starting = jobs.clone();
    let images = artifacts.clone();
    let other = second.clone();
    resources.boot = Some(std::thread::spawn(move || {
        boot::boot(&starting, &images, other)
    }));
    waiting
        .recv_timeout(Duration::from_secs(120))
        .expect("second VM must reach InstanceStart");
    boot::guest_ready(&jobs, &second.job_id);
    assert_eq!(jobs.view(&second.job_id).unwrap().state, State::Booting);
    let (phase2, paused) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(ExitStep::Recorded, move || {
        phase2.send(()).unwrap();
        released
            .recv_timeout(Duration::from_secs(30))
            .expect("release collector phase 2");
    });
    resources.releases.push(release);
    let killed = Instant::now();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let group = jobs.group(&first.job_id);
    resources.observers.push(std::thread::spawn(move || {
        log_cgroup_signals(&group, killed)
    }));
    let (reported, report) = mpsc::channel();
    let polling = jobs.clone();
    let id = first.job_id.clone();
    resources.observers.push(std::thread::spawn(move || {
        while killed.elapsed() < Duration::from_secs(1) {
            if let Some(exit) = reports(&polling).into_iter().find(|exit| exit.job_id == id) {
                drop(reported.send((killed.elapsed(), exit)));
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }));
    let (elapsed, report) = report
        .recv_timeout(Duration::from_secs(1))
        .expect("watcher must publish the killed VM's exit within 1 s while another VM boots");
    assert!(
        elapsed < Duration::from_secs(1),
        "report latency: {elapsed:?}"
    );
    assert_eq!(report.oom_kill, 0);
    assert!(
        report.egress_bytes.is_some(),
        "real nft counter must be measured"
    );
    assert_eq!(jobs.view(&second.job_id).unwrap().state, State::Booting);
    paused.recv_timeout(Duration::from_secs(1)).unwrap();
    for observer in resources.observers.drain(..) {
        observer.join().unwrap();
    }
    resources.releases.pop().unwrap().send(()).unwrap();
    wait_for(|| !jobs.view(&first.job_id).unwrap().cleanup_pending);
    assert!(!namespace::path(&first.job_id).unwrap().exists());
    assert!(tap_objects(index).is_disjoint(&nft_objects().unwrap()));
    let durable: Record = serde_json::from_reader(
        File::open(jobs.records_path.join(format!("{}.json", first.job_id))).unwrap(),
    )
    .unwrap();
    assert_eq!(durable.tap_index, None);
    assert!(
        !durable.released,
        "collection must not release the checkout disk"
    );
    assert_eq!(jobs.view(&second.job_id).unwrap().state, State::Booting);
    resources.releases.pop().unwrap().send(()).unwrap();
    assert!(matches!(
        resources
            .boot
            .take()
            .unwrap()
            .join()
            .unwrap()
            .unwrap()
            .reply,
        Reply::Job { running: true, .. }
    ));
    jobs.stop(&first.job_id).unwrap();
    jobs.stop(&second.job_id).unwrap();
    let reply = boot::boot(&jobs, &artifacts, third.clone()).unwrap().reply;
    assert!(
        matches!(reply, Reply::Job { running: true, tap_index: Some(reused), .. } if reused == index)
    );
    boot::guest_ready(&jobs, &third.job_id);
    drop(watcher);
}
