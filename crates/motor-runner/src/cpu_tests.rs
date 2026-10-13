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
