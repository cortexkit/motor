use super::*;
use cortexkit_test_support::ScratchDir;

fn kernel_mask(dir: &Path, mask: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("cpuset.cpus.effective"), mask).unwrap();
}

fn online_topology() -> CpuTopology {
    CpuTopology::new((0..4).map(|i| vec![i, i + 8]).collect()).unwrap()
}

#[test]
fn realized_job_slice_limits_placement_and_recomputes_daemon_reservation() {
    let root = ScratchDir::new("motor-job-cpuset");
    kernel_mask(root.path(), "0-3,8-11\n");
    kernel_mask(&root.join(MOTOR_SLICE).join(JOB_SLICE), "1,9,11\n");
    let effective = job_slice_cpus(root.path(), |_| panic!("realized slice has a mask")).unwrap();
    let topology = effective.restrict(online_topology(), None).unwrap();
    assert_eq!(topology.claim_limit(), 3);
    assert_eq!(topology.place(3, std::iter::empty()), [1, 9, 11]);
}

#[test]
fn inactive_job_slice_inherits_nearest_realized_ancestor() {
    let root = ScratchDir::new("motor-ancestor-cpuset");
    kernel_mask(root.path(), "0-3,8-11\n");
    kernel_mask(&root.join(MOTOR_SLICE), "2-3,10-11\n");
    let effective = job_slice_cpus(root.path(), |unit| {
        assert_eq!(unit, JOB_SLICE);
        Ok(String::new())
    })
    .unwrap();
    let topology = effective.restrict(online_topology(), None).unwrap();
    assert_eq!(topology.place(4, std::iter::empty()), [2, 3, 10, 11]);
}

#[test]
fn unrealized_slices_still_intersect_their_configured_allowed_cpus() {
    let root = ScratchDir::new("motor-configured-cpuset");
    kernel_mask(root.path(), "0-3,8-11\n");
    let effective = job_slice_cpus(root.path(), |unit| {
        Ok(match unit {
            MOTOR_SLICE => "1-3 9-11\n",
            JOB_SLICE => "0-2 8-10\n",
            _ => panic!("unexpected unit {unit}"),
        }
        .into())
    })
    .unwrap();
    let topology = effective
        .restrict(online_topology(), Some(&[1, 9]))
        .unwrap();
    assert_eq!(topology.place(4, std::iter::empty()), [1, 2, 9, 10]);

    let error = effective
        .restrict(online_topology(), Some(&[0]))
        .err()
        .unwrap();
    assert!(error.to_string().contains("--linux-cpus names CPU 0"));
    assert!(error
        .to_string()
        .contains("motor.slice AllowedCPUs=1-3 9-11"));
}

#[test]
fn missing_kernel_root_mask_does_not_fall_back_to_online_cpus() {
    let root = ScratchDir::new("motor-missing-cpuset");
    let error = job_slice_cpus(root.path(), |_| Ok(String::new()))
        .err()
        .unwrap();
    assert!(error.to_string().contains("cpuset.cpus.effective"));
}
