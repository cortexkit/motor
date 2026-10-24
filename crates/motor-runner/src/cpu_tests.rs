use super::*;
use cortexkit_test_support::ScratchDir;

fn topology() -> CpuTopology {
    CpuTopology::new((0..4).map(|cpu| vec![cpu, cpu + 4]).collect()).unwrap()
}

#[test]
fn topology_reads_online_siblings_and_reserves_cpu_zero_core() {
    let scratch = ScratchDir::new("motor-cpu-topology");
    std::fs::write(scratch.join("online"), "0-3,8-10\n").unwrap();
    for cpu in [0, 1, 2, 3, 8, 9, 10] {
        let root = scratch.join(format!("cpu{cpu}/topology"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("thread_siblings_list"),
            format!("{},{}\n", cpu % 8, cpu % 8 + 8),
        )
        .unwrap();
    }
    let topology = CpuTopology::from_sysfs(scratch.path()).unwrap();
    assert_eq!(topology.capacity(), 5);
    assert_eq!(topology.place(5, std::iter::empty()), [1, 2, 3, 9, 10]);
    assert!(parse_list("3-1").is_err());
    assert!(parse_list("").is_err());
    assert!(CpuTopology::new(vec![vec![0, 1], vec![1, 2]]).is_err());
}

#[test]
fn even_claims_use_whole_pairs_without_overlap_while_free() {
    let topology = topology();
    let first = topology.place(2, std::iter::empty());
    assert_eq!(first, [1, 5]);
    let second = topology.place(2, std::iter::once(first.as_slice()));
    assert_eq!(second, [2, 6]);
    let third = topology.place(2, [first.as_slice(), second.as_slice()].into_iter());
    assert_eq!(third, [3, 7]);
    assert_eq!(topology.capacity(), 6);
}

#[test]
fn odd_claims_prefer_an_already_split_pair() {
    let topology = topology();
    let first = topology.place(1, std::iter::empty());
    assert_eq!(first, [1]);
    assert_eq!(
        topology.place(3, std::iter::once(first.as_slice())),
        [2, 5, 6]
    );
    assert_eq!(topology.place(1, std::iter::once(first.as_slice())), [5]);
}

#[test]
fn fragmented_even_claim_uses_free_threads_before_overlap() {
    let topology = topology();
    assert_eq!(
        topology.place(2, std::iter::once([1, 2, 3].as_slice())),
        [5, 6]
    );
}

#[test]
fn overlap_uses_least_claimed_cpus_only_after_free_cpus() {
    let topology = topology();
    let all = [1, 2, 3, 5, 6, 7];
    let busy_pair = [1, 5];
    assert_eq!(
        topology.place(2, [all.as_slice(), busy_pair.as_slice()].into_iter()),
        [2, 6]
    );
    // Exhaust the five free CPUs before the single unavoidable overlap.
    assert_eq!(topology.place(6, std::iter::once([1].as_slice())), all);
    assert_eq!(format_list(&all), "1,2,3,5,6,7");
}

#[test]
fn small_hosts_keep_every_cpu_allocatable() {
    // A 2-vCPU CI runner is one core with two threads: reserving it would
    // leave no CPU for any job.
    let one_core = CpuTopology::new(vec![vec![0, 1]]).unwrap();
    assert_eq!(one_core.capacity(), 2);
    assert_eq!(one_core.place(2, std::iter::empty()), [0, 1]);
    let three_cores = CpuTopology::new((0..3).map(|cpu| vec![cpu, cpu + 3]).collect()).unwrap();
    assert_eq!(three_cores.capacity(), 6);
    // From four cores up, the core holding CPU 0 is left for daemon setup.
    let build_server = CpuTopology::new((0..16).map(|cpu| vec![cpu, cpu + 16]).collect()).unwrap();
    assert_eq!(build_server.capacity(), 30);
    assert!(!build_server.place(30, std::iter::empty()).contains(&0));
}

#[test]
fn explicit_single_job_sandbox_uses_all_eight_vcpus() {
    for cores in [
        (0..8).map(|cpu| vec![cpu]).collect(),
        (0..4).map(|cpu| vec![cpu, cpu + 4]).collect(),
    ] {
        let default = CpuTopology::new(cores).unwrap();
        assert!(default.capacity() < 8);
        let sandbox = default.without_daemon_reservation();
        assert_eq!(sandbox.capacity(), 8);
        for request in 1..=8 {
            assert_eq!(
                sandbox.place(request, std::iter::empty()).len(),
                request as usize
            );
        }
    }
    assert!(crate::Config::new("/unused", "main").reserve_daemon_core);
}

fn windows_topology(probe: ActivityProbe) -> CpuTopology {
    let mut topology = CpuTopology::new((0..16).map(|cpu| vec![cpu, cpu + 16]).collect())
        .unwrap()
        .avoid_windows(&[12, 13, 14, 15], PathBuf::from("/windows/slot.lock"))
        .unwrap();
    topology.set_windows_probe(probe);
    topology
}

#[test]
fn active_windows_excludes_siblings_and_overlaps_only_remaining_cpus() {
    let topology = windows_topology(Arc::new(|| Ok(true)));
    let remaining: Vec<_> = (1..12).chain(17..28).collect();
    let placement = topology.place_with_status(30, std::iter::empty());
    assert_eq!(placement.windows_active, Some(true));
    assert_eq!(
        placement.avoided_cpus,
        [12, 13, 14, 15, 28, 29, 30, 31].into_iter().collect()
    );
    assert_eq!(topology.capacity(), 30);
    assert_eq!(topology.place(30, std::iter::empty()), remaining);
    assert_eq!(
        topology.place(30, std::iter::once(remaining.as_slice())),
        remaining
    );
    assert_eq!(
        topology.place(4, [remaining.as_slice(), [1, 17].as_slice()].into_iter()),
        [2, 3, 18, 19]
    );
    assert_eq!(topology.place(3, std::iter::empty()), [1, 2, 17]);
}

#[test]
fn inactive_windows_keeps_all_cpus_available() {
    let topology = windows_topology(Arc::new(|| Ok(false)));
    let placement = topology.place_with_status(30, std::iter::empty());
    assert_eq!(placement.windows_active, Some(false));
    assert!(placement.avoided_cpus.is_empty());
    assert_eq!(
        topology.place(30, std::iter::empty()),
        (1..16).chain(17..32).collect::<Vec<_>>()
    );
}

#[test]
fn failed_windows_check_fails_open_and_rate_limits_warnings() {
    let topology = windows_topology(Arc::new(|| Err(io::Error::other("lock table unreadable"))));
    let placement = topology.place_with_status(30, std::iter::empty());
    assert_eq!(placement.windows_active, None);
    assert!(placement.avoided_cpus.is_empty());
    assert_eq!(
        topology.place(30, std::iter::empty()),
        (1..16).chain(17..32).collect::<Vec<_>>()
    );
    let windows = topology.windows.as_ref().unwrap();
    let last = windows.last_warning.lock().unwrap().unwrap();
    assert!(!windows.warning_due(last + Duration::from_secs(59)));
    assert!(windows.warning_due(last + Duration::from_secs(60)));
    assert!(!windows.warning_due(last + Duration::from_secs(61)));
}

#[test]
fn windows_reservation_expands_actual_sibling_groups_not_cpu_number_offsets() {
    let mut topology = CpuTopology::new(vec![vec![0, 8], vec![2, 19], vec![5, 31], vec![7, 23]])
        .unwrap()
        .avoid_windows(&[19], PathBuf::from("/windows/slot.lock"))
        .unwrap();
    topology.set_windows_probe(Arc::new(|| Ok(true)));
    assert_eq!(topology.place(6, std::iter::empty()), [5, 7, 23, 31]);
    assert_eq!(topology.capacity(), 6);
}

#[test]
fn windows_covering_all_allocatable_cpus_fails_open() {
    let mut topology = topology()
        .avoid_windows(&[1, 2, 3], PathBuf::from("/windows/slot.lock"))
        .unwrap();
    topology.set_windows_probe(Arc::new(|| Ok(true)));
    assert_eq!(topology.place(6, std::iter::empty()), [1, 2, 3, 5, 6, 7]);
    assert!(topology
        .windows
        .as_ref()
        .unwrap()
        .last_warning
        .lock()
        .unwrap()
        .is_some());
}

const LINUX_CPUS: [u32; 8] = [12, 13, 14, 15, 28, 29, 30, 31];

fn build_server() -> CpuTopology {
    CpuTopology::new((0..16).map(|cpu| vec![cpu, cpu + 16]).collect()).unwrap()
}

#[test]
fn unconfined_claim_limit_is_the_allocatable_cpu_count() {
    let topology = build_server();
    assert_eq!(topology.capacity(), 30);
    assert_eq!(topology.claim_limit(), 30);
}

#[test]
fn linux_cpu_list_confines_placement_and_admits_claims_up_to_max_weight() {
    let topology = build_server().confine(&LINUX_CPUS, 16).unwrap();
    // The listed threads are all allocatable: no daemon core is kept back.
    assert_eq!(topology.capacity(), 8);
    assert_eq!(topology.claim_limit(), 16);
    for claim in [12, 16] {
        assert_eq!(topology.place(claim, std::iter::empty()), LINUX_CPUS);
    }
    let four = topology.place(4, std::iter::empty());
    assert_eq!(four, [12, 13, 28, 29]);
    assert_eq!(
        topology.place(4, std::iter::once(four.as_slice())),
        [14, 15, 30, 31]
    );
    // A partially listed core contributes only its listed thread.
    let partial = build_server().confine(&[0, 12], 4).unwrap();
    assert_eq!(partial.capacity(), 2);
    assert_eq!(partial.place(4, std::iter::empty()), [0, 12]);
    for (cpus, limit) in [(&[][..], 16), (&[12, 32][..], 16), (&LINUX_CPUS[..], 0)] {
        assert!(
            build_server().confine(cpus, limit).is_err(),
            "{cpus:?} {limit}"
        );
    }
}

#[derive(Clone, Default)]
struct LogBytes(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn confined_linux_cpus_share_windows_threads_while_the_slot_is_active() {
    // The listed threads are exactly the Windows cores and their siblings, so
    // an active Windows slot would leave Linux jobs nothing: avoidance fails
    // open with a warning and the job still runs on the listed threads.
    let topology = windows_topology(Arc::new(|| Ok(true)))
        .confine(&LINUX_CPUS, 16)
        .unwrap();
    let logs = LogBytes::default();
    let placement = tracing::subscriber::with_default(
        motor_protocol::logging::LineSubscriber::new("[runner]", logs.clone()),
        || topology.place_with_status(4, std::iter::empty()),
    );
    assert_eq!(placement.windows_active, Some(true));
    assert!(placement.avoided_cpus.is_empty());
    assert_eq!(placement.cpus, [12, 13, 28, 29]);
    assert!(topology.windows_fail_open_warned());
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("Windows CPU avoidance unavailable")
            && logs.contains("Windows CPUs cover every allocatable CPU"),
        "{logs}"
    );
}

#[test]
fn lock_table_matches_device_inode_and_only_granted_exclusive_flocks() {
    let entry = "1: FLOCK ADVISORY WRITE 101 08:1a:1234 0 EOF\n";
    assert!(flock_is_held(entry, 8, 26, 1234).unwrap());
    assert!(!flock_is_held(entry, 8, 25, 1234).unwrap());
    assert!(!flock_is_held(entry, 9, 26, 1234).unwrap());
    assert!(!flock_is_held(entry, 8, 26, 1235).unwrap());
    for entry in [
        "1: -> FLOCK ADVISORY WRITE 101 08:1a:1234 0 EOF\n",
        "1: POSIX ADVISORY WRITE 101 08:1a:1234 0 EOF\n",
        "1: OFDLCK ADVISORY WRITE -1 08:1a:1234 0 EOF\n",
        "1: FLOCK ADVISORY READ 101 08:1a:1234 0 EOF\n",
        "",
    ] {
        assert!(!flock_is_held(entry, 8, 26, 1234).unwrap(), "{entry}");
    }
    for entry in [
        "garbage",
        "1: FLOCK ADVISORY WRITE 101 bad 0 EOF",
        "1: FLOCK ADVISORY WRITE 101 08:1a:no 0 EOF",
    ] {
        assert!(flock_is_held(entry, 8, 26, 1234).is_err(), "{entry}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn slot_observation_restats_inode_and_fails_on_unreadable_or_malformed_table() {
    use std::os::unix::fs::MetadataExt;

    let scratch = ScratchDir::new("motor-slot-observation");
    let slot = scratch.join("slot.lock");
    let locks = scratch.join("locks");
    assert!(!slot_active_from(&slot, &locks).unwrap());
    std::fs::write(&slot, "").unwrap();
    assert!(slot_active_from(&slot, &locks).is_err());
    let metadata = std::fs::metadata(&slot).unwrap();
    std::fs::write(
        &locks,
        format!(
            "1: FLOCK ADVISORY WRITE 101 {:x}:{:x}:{} 0 EOF\n",
            libc::major(metadata.dev()),
            libc::minor(metadata.dev()),
            metadata.ino()
        ),
    )
    .unwrap();
    assert!(slot_active_from(&slot, &locks).unwrap());
    std::fs::rename(&slot, scratch.join("old-slot.lock")).unwrap();
    std::fs::write(&slot, "").unwrap();
    assert!(!slot_active_from(&slot, &locks).unwrap());
    std::fs::write(&locks, "malformed").unwrap();
    assert!(slot_active_from(&slot, &locks).is_err());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires operator validation of the host PID namespace and /proc/locks visibility"]
fn isolated_windows_slot_observation_tracks_flock_and_recreated_inode() {
    use fs2::FileExt;

    let scratch = ScratchDir::new("motor-real-slot-observation");
    let slot = scratch.join("slot.lock");
    let file = std::fs::File::create(&slot).unwrap();
    assert!(!windows_slot_active(&slot).unwrap());
    file.try_lock_exclusive().unwrap();
    let started = Instant::now();
    assert!(windows_slot_active(&slot).unwrap());
    assert!(started.elapsed() < Duration::from_secs(1));
    FileExt::unlock(&file).unwrap();
    assert!(!windows_slot_active(&slot).unwrap());
    file.try_lock_exclusive().unwrap();
    std::fs::rename(&slot, scratch.join("old-slot.lock")).unwrap();
    let replacement = std::fs::File::create(&slot).unwrap();
    assert!(!windows_slot_active(&slot).unwrap());
    replacement.try_lock_exclusive().unwrap();
    assert!(windows_slot_active(&slot).unwrap());
    FileExt::unlock(&replacement).unwrap();
    assert!(!windows_slot_active(&slot).unwrap());
}

#[cfg(target_os = "linux")]
#[test]
// Not named isolated_*: the operator lane runs every ignored isolated_ test,
// and this one needs a Windows job VM running at the time, so it is run by hand.
#[ignore = "requires a running Windows job VM; set MOTOR_TEST_WINDOWS_SLOT_LOCK and MOTOR_TEST_WINDOWS_CPUS to the Linux runner's flag values"]
fn windows_vm_running_linux_placement_avoids_its_cores() {
    let slot =
        PathBuf::from(std::env::var_os("MOTOR_TEST_WINDOWS_SLOT_LOCK").expect("slot-lock path"));
    let cpus =
        parse_list(&std::env::var("MOTOR_TEST_WINDOWS_CPUS").expect("Windows CPU list")).unwrap();
    let units = std::process::Command::new("systemctl")
        .args([
            "list-units",
            "--all",
            "--no-legend",
            "--plain",
            "--state=active",
            "motor-windows-windows-*.service",
        ])
        .output()
        .unwrap();
    assert!(
        units.status.success(),
        "{}",
        String::from_utf8_lossy(&units.stderr)
    );
    assert!(
        String::from_utf8_lossy(&units.stdout)
            .lines()
            .any(|line| line.contains("motor-windows-windows-")),
        "start a Windows job VM before running this test"
    );
    assert!(
        windows_slot_active(&slot).unwrap(),
        "the Linux runner must see the QEMU slot's flock in its PID namespace"
    );
    let topology = CpuTopology::read()
        .unwrap()
        .avoid_windows(&cpus, slot)
        .unwrap();
    let excluded = &topology.windows.as_ref().unwrap().cpus;
    let remaining = topology.place(topology.capacity(), std::iter::empty());
    assert!(!remaining.is_empty());
    assert!(remaining.iter().all(|cpu| !excluded.contains(cpu)));
    assert!(topology
        .cores
        .iter()
        .take(topology.reserved)
        .flatten()
        .all(|cpu| !remaining.contains(cpu)));
}
