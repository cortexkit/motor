use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

mod exit_tests;
mod network_tests;
mod plane_tests;
mod security_tests;
#[cfg(feature = "linux-service-tests")]
mod service_cleanup;
mod watcher_tests;
#[cfg(feature = "linux-service-tests")]
use service_cleanup::ServiceResources;

#[test]
fn absent_job_stop_and_adopt_are_idempotent_without_exit_evidence() {
    let base = scratch();
    let jobs = machine(&base);
    let id = start().job_id;
    jobs.stop(&id).unwrap();
    let reply = serde_json::to_value(jobs.status(&id).unwrap().reply).unwrap();
    assert_eq!(reply["known"], false);
    assert_eq!(reply["running"], false);
    assert!(reply.get("exit_report").is_none());
    // Readers that ignore additive fields can still confirm that no VM
    // holds the workspace; no exit receipt is synthesized.
    assert_eq!(reply["result"], "job");
    assert!(jobs.records().is_empty());
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn absent_job_requires_empty_cgroup_and_no_unreconciled_jail() {
    let base = scratch();
    let jobs = machine(&base);
    let id = start().job_id;
    let group = jobs.group(&id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), "123\n").unwrap();
    assert!(jobs.stop(&id).is_err());
    assert!(jobs.status(&id).is_err());
    std::fs::write(group.join("cgroup.procs"), "").unwrap();
    std::fs::create_dir_all(jobs.boundary(&id)).unwrap();
    assert!(jobs.stop(&id).is_err());
    assert!(jobs.status(&id).is_err());
    std::fs::remove_dir_all(jobs.boundary(&id)).unwrap();
    jobs.stop(&id).unwrap();
    assert_eq!(
        serde_json::to_value(jobs.status(&id).unwrap().reply).unwrap()["known"],
        false
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn balloon_refuses_target_above_recorded_ram_before_opening_any_process_or_socket() {
    let base = scratch();
    let jobs = machine(&base);
    let record = record();
    let id = record.start.job_id.clone();
    let amount = record.start.memory_mib + 1;
    jobs.insert_record(record);
    assert!(jobs
        .balloon(&id, amount)
        .err()
        .unwrap()
        .0
        .contains("target exceeds guest RAM"));
    assert!(jobs.balloon("unknown", 0).is_err());
    assert!(jobs.memory("unknown").is_err());
    std::fs::remove_dir_all(base).unwrap();
}

/// Firecracker answers 400 InvalidID to any resource ID (in the path or the
/// body) with characters other than ASCII alphanumerics and underscores. A
/// hyphenated credential drive ID once failed every credentialed job start.
#[test]
fn every_boot_request_uses_ids_firecracker_accepts() {
    let valid =
        |id: &str| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let nonce = "0123456789abcdef0123456789abcdef";
    for kind in [
        job::VmKind::Job,
        job::VmKind::Fill,
        job::VmKind::BaseBuild,
        job::VmKind::Recovery,
    ] {
        let mut start = start();
        start.kind = kind;
        for nonce in [Some(nonce), None] {
            for (path, body) in boot_requests(&start, Some(17), nonce, 4 << 30).unwrap() {
                for resource in ["/drives/", "/pmem/", "/network-interfaces/"] {
                    if let Some(id) = path.strip_prefix(resource) {
                        assert!(valid(id), "{kind:?}: invalid Firecracker ID in path {path}");
                    }
                }
                for key in ["drive_id", "iface_id", "id"] {
                    if let Some(id) = body[key].as_str() {
                        assert!(
                            valid(id),
                            "{kind:?}: invalid Firecracker {key} {id} in {path}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn credential_boot_is_read_only_nonce_bound_and_absent_for_unprovisioned_vms() {
    let nonce = "0123456789abcdef0123456789abcdef";
    let requests = boot_requests(&start(), Some(17), Some(nonce), 4 << 30).unwrap();
    let drive = requests
        .iter()
        .find(|request| request.0 == "/drives/remote_cache")
        .unwrap();
    assert_eq!(
        drive.1,
        json!({"drive_id":"remote_cache","path_on_host":"/remote-cache.img","is_root_device":false,"is_read_only":true})
    );
    let encoded: String = runner_args(&start(), 4 << 30, true, None)
        .unwrap()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        requests[1].1["boot_args"],
        format!("{JOB_BOOT_ARGS} motor.runner_args={encoded} motor.cache_nonce={nonce}")
    );
    let plain = boot_requests(&start(), Some(17), None, 4 << 30).unwrap();
    assert!(!plain
        .iter()
        .any(|request| request.0 == "/drives/remote_cache"));
    assert!(!plain[1].1["boot_args"]
        .as_str()
        .unwrap()
        .contains("motor.cache_nonce"));
    let mut recovery_start = start();
    recovery_start.kind = job::VmKind::Recovery;
    let recovery = boot_requests(&recovery_start, Some(17), Some(nonce), 4 << 30).unwrap();
    assert_eq!(
        recovery[1].1["boot_args"],
        format!("{JOB_BOOT_ARGS} motor.recovery=1")
    );
    assert!(!recovery.iter().any(|r| r.0 == "/drives/remote_cache"));
    assert!(!recovery
        .iter()
        .any(|request| request.0.starts_with("/network-interfaces/")));
    assert!(!requests.iter().any(|request| request.0.contains("mmds")));
}

#[test]
#[cfg(feature = "linux-root-tests")]
fn isolated_credential_image_is_removed_on_restart_and_release_before_uid_reuse() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = scratch();
    let mut jobs = machine(&base);
    jobs.settings = Some(JobVms {
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
        uid_base: 600001,
        uid_count: 1,
        memory_max_bytes: 1048576,
    });
    std::fs::create_dir_all(&jobs.records_path).unwrap();
    let mut record = record();
    record.uid = 600001;
    let disk_dir = jobs.workspaces.join(&record.start.workspace_disk_id);
    std::fs::create_dir_all(&disk_dir).unwrap();
    let disk = disk_dir.join("workspace.img");
    std::fs::write(&disk, "workspace-data-not-credentials").unwrap();
    let metadata = std::fs::metadata(&disk).unwrap();
    record.disk_device = metadata.dev();
    record.disk_inode = metadata.ino();
    fs::chown(&File::open(&disk).unwrap(), record.uid).unwrap();
    let id = record.start.job_id.clone();
    // The production jail parent is created once at helper startup.
    std::fs::create_dir_all(&jobs.jail).unwrap();
    super::super::create_boundary(&jobs.boundary(&id)).unwrap();
    cache::tests::install_fixture(&jobs.cache_store, &record.start.account);
    let nonce = cache::stage(
        &jobs.cache_store,
        &record.start.account,
        job::VmKind::Job,
        &File::open(jobs.boundary(&id)).unwrap(),
        record.uid,
    )
    .unwrap()
    .unwrap();
    let bytes = std::fs::read(jobs.boundary(&id).join(cache::IMAGE)).unwrap();
    let offset = motor_protocol::REMOTE_CACHE_DRIVE_MAGIC.len();
    let header: String = bytes[offset..offset + 16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(header, nonce);
    assert!(
        boot_requests(&record.start, Some(17), Some(&nonce), 4 << 30).unwrap()[1].1["boot_args"]
            .as_str()
            .unwrap()
            .ends_with(&format!("motor.cache_nonce={header}"))
    );
    jobs.adopt_record(record.clone()).unwrap();
    assert!(jobs.allocate_uid(jobs.settings.as_ref().unwrap()).is_err());
    jobs.save(&record).unwrap();
    let mut restarted = machine(&base);
    restarted.settings = jobs.settings.clone();
    restarted.reconcile_records(vec![record.clone()]).unwrap();
    assert!(!restarted.boundary(&id).join(cache::IMAGE).exists());
    assert!(restarted
        .allocate_uid(restarted.settings.as_ref().unwrap())
        .is_err());
    // Release retries cleanup even after startup already removed the image.
    restarted.stop(&id).unwrap();
    assert!(!restarted.boundary(&id).exists());
    assert_eq!(
        restarted
            .allocate_uid(restarted.settings.as_ref().unwrap())
            .unwrap(),
        record.uid
    );
    assert_eq!(
        std::fs::read_to_string(&disk).unwrap(),
        "workspace-data-not-credentials"
    );
    let saved = std::fs::read_to_string(restarted.records_path.join(format!("{id}.json"))).unwrap();
    let reply = serde_json::to_string(&restarted.status(&id).unwrap().reply).unwrap();
    for text in [saved, reply] {
        assert!(!text.contains("SECRET"));
        assert!(!text.contains("remote-cache"));
        assert!(!text.contains(&nonce));
    }
    std::fs::remove_dir_all(base).unwrap();
}

fn start() -> Start {
    Start {
        cache_write_blocked: false,
        kind: job::VmKind::Job,
        job_id: job::test_id(1),
        account: "account-1".into(),
        workspace_disk_id: "disk-1".into(),
        image_generation_id: "generation-1".into(),
        vcpus: 7,
        memory_mib: 1536,
        burst_mib: Some(512),
    }
}

pub(super) fn record() -> Record {
    Record::new(start(), unsafe { libc::geteuid() }, 8, 99, None)
}

fn start_record(jobs: &LinuxJobs, start: Start, uid: u32, tap_index: Option<u8>) -> Record {
    let workspace = jobs.workspaces.join(&start.workspace_disk_id);
    std::fs::create_dir_all(&workspace).unwrap();
    let disk = File::create(workspace.join("workspace.img")).unwrap();
    let meta = disk.metadata().unwrap();
    Record::new(start, uid, meta.dev(), meta.ino(), tap_index)
}

fn restart_start(base: &Path) -> Start {
    // UUIDv7 fixtures use a process-local timestamp. Every helper incarnation
    // must use the original start rather than generate another job identity.
    serde_json::from_reader(File::open(base.join("start.json")).unwrap()).unwrap()
}

#[test]
fn legacy_job_records_have_no_clean_fill_exit_evidence() {
    let mut value = serde_json::to_value(record()).unwrap();
    value.as_object_mut().unwrap().remove("fill_exit_success");
    let restored: Record = serde_json::from_value(value).unwrap();
    assert_eq!(restored.fill_exit_success, None);
    assert!(serde_json::to_value(restored)
        .unwrap()
        .get("fill_exit_success")
        .is_none());
}

#[test]
fn fill_process_exit_success_is_durable_and_failure_never_becomes_success() {
    for (program, success) in [("/bin/true", true), ("/bin/false", false)] {
        let base = scratch();
        let jobs = machine(&base);
        std::fs::create_dir_all(&jobs.records_path).unwrap();
        let mut record = record();
        record.start.kind = job::VmKind::Fill;
        let id = record.start.job_id.clone();
        jobs.insert_record(record);
        jobs.cell(&id).unwrap().0.lock().child = Some(Command::new(program).spawn().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while jobs.cell(&id).unwrap().0.lock().child.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture child did not exit"
            );
            jobs.reap();
            std::thread::sleep(Duration::from_millis(5));
        }
        let durable: Record = serde_json::from_slice(
            &std::fs::read(jobs.records_path.join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(durable.fill_exit_success, Some(success));
        std::fs::remove_dir_all(base).unwrap();
    }
}

pub(super) fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = cortexkit_test_support::scratch_root().join(format!(
        "helper-job-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

pub(super) fn machine(base: &Path) -> LinuxJobs {
    machine_with_config(
        base,
        &serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap(),
    )
}

pub(in crate::linux) fn empty_exit_fixture(base: &Path) -> LinuxJobs {
    let jobs = exit_tests::fixture(base);
    jobs.save(&record()).unwrap();
    jobs.insert_record(record());
    exit_tests::empty_group(&jobs, 3);
    jobs
}

pub(super) fn machine_with_config(base: &Path, config: &Config) -> LinuxJobs {
    LinuxJobs {
        cache_write_grants: std::sync::Arc::new(crate::cache_write::Grants::new(config)),
        settings: None,
        groups: base.join("groups"),
        records_path: base.join("records"),
        jail: base.join("jail"),
        cache_store: base.join("remote-cache"),
        workspaces: base.join("workspaces"),
        admission: Mutex::new(Lock::A, ()),
        registry: Mutex::new(Lock::R, BTreeMap::new()),
        next_prune: std::sync::Mutex::new(None),
        network: Box::<network_tests::Fake>::default(),
        hooks: std::sync::Mutex::new(TestHooks::default()),
        remove_test_group: Some(|path| std::fs::remove_dir_all(path)),
    }
}

struct Vmm {
    child: Child,
    base: PathBuf,
}
impl Vmm {
    fn new() -> Self {
        let base = scratch();
        let binary = base.join("firecracker");
        std::fs::copy("/bin/sleep", &binary).unwrap();
        let child = Command::new(binary)
            .arg0("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        // spawn can return while the child is still inside execve: the kernel
        // wakes a vfork-style parent before it renames the child, so `comm`
        // briefly still shows the test's name. The helper itself checks the
        // name only after the VMM's API socket is up.
        let comm = format!("/proc/{}/comm", child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&comm).unwrap().trim() != "firecracker" {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture never became firecracker"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { child, base }
    }
}
impl Drop for Vmm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[test]
fn job_boot_command_line_tracks_the_tenant_image_and_selects_per_job_mode() {
    let image_builder = include_str!("../../../../../script/tenant-image/build.py");
    let definition = image_builder
        .split_once("BOOT_ARGS = (")
        .unwrap()
        .1
        .split_once(")\n")
        .unwrap()
        .0;
    let tenant_args: String = definition
        .split('"')
        .enumerate()
        .filter_map(|(index, text)| (index % 2 == 1).then_some(text))
        .collect();
    assert!(!tenant_args.is_empty());
    let requests = boot_requests(&start(), Some(17), None, 4 << 30).unwrap();
    let encoded: String = runner_args(&start(), 4 << 30, false, None)
        .unwrap()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        requests[1].1["boot_args"],
        format!("{tenant_args} motor.per_job=1 page_reporting.page_reporting_order=5 motor.runner_args={encoded}")
    );
}

fn fixed_path_settings(roots: serde_json::Value) -> JobVms {
    serde_json::from_value(json!({
        "uid_base": 500000, "uid_count": 128, "memory_max_bytes": 4096,
        "fixed_path_repositories": roots
    }))
    .unwrap()
}

fn disk_registry(start: &Start) -> Workspace {
    Workspace {
        workspace_key_hash: None,
        account: start.account.clone(),
        workspace_disk_id: start.workspace_disk_id.clone(),
        role: if start.kind == job::VmKind::Fill {
            job::WorkspaceRole::Fill
        } else {
            job::WorkspaceRole::Job
        },
    }
}

fn decoded_runner_args(line: &str) -> Option<String> {
    let hex = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("motor.runner_args="))?;
    Some(
        String::from_utf8(
            hex.as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect(),
        )
        .unwrap(),
    )
}

#[test]
fn disabled_fixed_paths_preserve_the_boot_line_bytes() {
    let legacy = concat!(
        "--backend host --tenant-vm --job-network veth --state-root /tenant/state ",
        "--max-weight 7 --rustc-slots 7 ",
        "--return-chunked-max-bytes 134217728 --return-budget-bytes 2147483648 ",
        "--sccache-size-gib 1 --free-space-floor-bytes 429496729 ",
        "--free-space-eviction-bytes 1503238553 ",
        "--workspace-root /Users --owned-workspace-root /Users ",
        "--workspace-root /home --owned-workspace-root /home"
    );
    let encoded: String = legacy.bytes().map(|byte| format!("{byte:02x}")).collect();
    let expected = format!(
        "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro init=/sbin/motor-init net.ifnames=0 systemd.unified_cgroup_hierarchy=1 motor.per_job=1 page_reporting.page_reporting_order=5 motor.runner_args={encoded}"
    );
    let mut shape = start();
    shape.account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
    let registry = disk_registry(&shape);
    let absent: JobVms = serde_json::from_value(json!({
        "uid_base": 500000, "uid_count": 128, "memory_max_bytes": 4096
    }))
    .unwrap();
    for settings in [
        absent,
        fixed_path_settings(json!({})),
        fixed_path_settings(json!({shape.account.clone(): []})),
    ] {
        let requests =
            recorded_boot_requests(&shape, Some(17), None, 4 << 30, &registry, &settings).unwrap();
        assert_eq!(requests[1].1["boot_args"], expected);
    }
}

#[test]
fn fixed_path_policy_is_shared_by_job_and_fill_but_not_recovery_or_other_accounts() {
    let account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let other = "org:01ARZ3NDEKTSV4RRFFQ69G5FAV";
    for roots in [vec!["/repo/a", "/repo/b"], vec!["*"]] {
        let settings = fixed_path_settings(json!({account: roots, other: ["/other"]}));
        for kind in [job::VmKind::Job, job::VmKind::Fill, job::VmKind::Recovery] {
            let mut shape = start();
            shape.account = account.into();
            shape.kind = kind;
            let registry = disk_registry(&shape);
            let requests =
                recorded_boot_requests(&shape, Some(17), None, 4 << 30, &registry, &settings)
                    .unwrap();
            let line = requests[1].1["boot_args"].as_str().unwrap();
            if kind == job::VmKind::Recovery {
                assert_eq!(line, format!("{JOB_BOOT_ARGS} motor.recovery=1"));
                continue;
            }
            let text = decoded_runner_args(line).unwrap();
            let words: Vec<_> = text.split_whitespace().collect();
            assert!(words
                .windows(2)
                .any(|pair| pair == ["--account-subject", account]));
            for root in &roots {
                assert!(words
                    .windows(2)
                    .any(|pair| pair == ["--fixed-path-repository", root]));
            }
            assert!(!text.contains("/other"));
            assert!(!text.contains(other));
            assert_eq!(line.ends_with(" motor.fill=1"), kind == job::VmKind::Fill);
            let early = early_boot_command_line(
                &shape,
                4 << 30,
                fixed_path_policy(&settings, &registry, kind),
            )
            .unwrap();
            let actual = recorded_boot_requests(
                &shape,
                Some(17),
                Some(&"0".repeat(32)),
                4 << 30,
                &registry,
                &settings,
            )
            .unwrap();
            assert_eq!(actual[1].1["boot_args"], early);

            shape.account = other.into();
            assert!(
                recorded_boot_requests(&shape, None, None, 4 << 30, &registry, &settings).is_err()
            );
            let requests = recorded_boot_requests(
                &shape,
                None,
                None,
                4 << 30,
                &disk_registry(&shape),
                &settings,
            )
            .unwrap();
            let text = decoded_runner_args(requests[1].1["boot_args"].as_str().unwrap()).unwrap();
            assert!(text.contains("--fixed-path-repository /other"));
            assert!(!text.contains(account));
            for root in &roots {
                assert!(!text.split_whitespace().any(|word| word == *root));
            }
        }
        let mut unconfigured = start();
        unconfigured.account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
        let requests = recorded_boot_requests(
            &unconfigured,
            None,
            None,
            4 << 30,
            &disk_registry(&unconfigured),
            &settings,
        )
        .unwrap();
        let text = decoded_runner_args(requests[1].1["boot_args"].as_str().unwrap()).unwrap();
        assert!(!text.contains("--account-subject"));
        assert!(!text.contains("--fixed-path-repository"));
    }
}

#[test]
fn configured_fixed_paths_are_included_in_the_early_boot_budget() {
    let mut shape = start();
    shape.account = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
    for kind in [job::VmKind::Job, job::VmKind::Fill] {
        shape.kind = kind;
        let registry = disk_registry(&shape);
        let settings =
            fixed_path_settings(json!({shape.account.clone(): [format!("/{}", "x".repeat(1024))]}));
        assert!(early_boot_command_line(
            &shape,
            4 << 30,
            fixed_path_policy(&settings, &registry, kind)
        )
        .unwrap_err()
        .0
        .contains("command-line budget"));
        assert!(recorded_boot_requests(&shape, None, None, 4 << 30, &registry, &settings).is_err());
    }
}

#[test]
fn fixed_path_validation_worst_case_bounds_real_job_and_fill_boot_lines() {
    let account = "acct:01KXJJHK4V6DB42W1YAN2XD9QN";
    let nonce = "0".repeat(REMOTE_CACHE_DRIVE_NONCE_BYTES * 2);
    for ceiling in [Some(4 << 30), Some(1024 << 30), Some(u64::MAX - 511), None] {
        let mut settings = fixed_path_settings(json!({
            account: ["/Users/ufukaltinok/Work/Projects/CortexKit/motor"]
        }));
        settings.workspace_disk_max_bytes = ceiling;
        let roots = &settings.fixed_path_repositories[account];
        let worst = worst_case_fixed_path_boot_command_line(&settings, account, roots).unwrap();
        let maximum_disk = ceiling.unwrap_or(u64::MAX - 511);
        let mut reached_worst = false;
        for bytes in [512, 4 << 30, maximum_disk] {
            for vcpus in [1, 9, 10, 32] {
                for kind in [job::VmKind::Job, job::VmKind::Fill] {
                    for cache_nonce in [None, Some(nonce.as_str())] {
                        let mut shape = start();
                        shape.account = account.into();
                        shape.vcpus = vcpus;
                        shape.kind = kind;
                        let registry = disk_registry(&shape);
                        let mut requests = recorded_boot_requests(
                            &shape,
                            Some(17),
                            cache_nonce,
                            bytes,
                            &registry,
                            &settings,
                        )
                        .unwrap();
                        authorize_job_write_boot(
                            &mut requests,
                            &shape,
                            cache_nonce,
                            kind == job::VmKind::Job && cache_nonce.is_some(),
                        )
                        .unwrap();
                        let actual = requests[1].1["boot_args"].as_str().unwrap();
                        assert!(
                            worst.len() >= actual.len(),
                            "validation reserved {} bytes, start needs {} (ceiling={ceiling:?}, disk={bytes}, vcpus={vcpus}, kind={kind:?}, remote_cache={})",
                            worst.len(),
                            actual.len(),
                            cache_nonce.is_some(),
                        );
                        reached_worst |= worst.len() == actual.len();
                    }
                }
            }
        }
        assert!(
            reached_worst,
            "maximum settings must reach the reserved length"
        );
    }
}

#[test]
fn runner_policy_matches_provisioning_for_small_large_and_cache_enabled_disks() {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/tenant-provision/provision.py");
    for disk in [4 << 30, 1024_u64 << 30, u64::MAX - 511] {
        for cache in [false, true] {
            let output = Command::new("python3").args(["-c", r#"
import importlib.util, sys
spec = importlib.util.spec_from_file_location('provision', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
print(module.runner_args(dict(vcpu_ceiling=4, disk_quota_bytes=int(sys.argv[2]), remote_cache=sys.argv[3] == 'true')))
"#]).arg(&script).arg(disk.to_string()).arg(cache.to_string()).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let mut shape = start();
            shape.vcpus = 4;
            let actual = runner_args(&shape, disk, cache, None).unwrap();
            assert_eq!(
                actual.split_whitespace().collect::<Vec<_>>(),
                std::str::from_utf8(&output.stdout)
                    .unwrap()
                    .split_whitespace()
                    .collect::<Vec<_>>()
            );
            let boot = boot_requests(
                &shape,
                Some(17),
                cache.then_some("0123456789abcdef0123456789abcdef"),
                disk,
            )
            .unwrap();
            let line = boot[1].1["boot_args"].as_str().unwrap();
            assert!(line.len() < 1792);
            assert_eq!(line.matches("motor.runner_args=").count(), 1);
        }
    }
}

#[test]
fn runner_boot_refuses_invalid_shape_disk_and_oversized_command_line() {
    for disk in [0, 1, 511, 513] {
        assert!(boot_requests(&start(), Some(17), None, disk).is_err());
    }
    let mut invalid = start();
    invalid.vcpus = 0;
    assert!(boot_requests(&invalid, Some(17), None, 4 << 30).is_err());
    assert!(boot_requests(&start(), Some(17), Some(&"a".repeat(2048)), 4 << 30).is_err());
    assert!(checked_boot_command_line("a".repeat(JOB_BOOT_CMDLINE_MAX_BYTES)).is_err());
    assert!(checked_boot_command_line("a".repeat(JOB_BOOT_CMDLINE_MAX_BYTES - 1)).is_ok());
}

#[test]
fn cache_nonce_requires_exact_lowercase_hex_before_any_boot_request() {
    for nonce in [
        String::new(),
        "0".repeat(31),
        "0".repeat(33),
        "A".repeat(32),
        "g".repeat(32),
        format!("+{}", "0".repeat(31)),
        "é".repeat(16),
        format!("{} injected=1", "0".repeat(21)),
    ] {
        for kind in [job::VmKind::Job, job::VmKind::Recovery] {
            let mut shape = start();
            shape.kind = kind;
            assert!(boot_requests(&shape, Some(17), Some(&nonce), 4 << 30)
                .unwrap_err()
                .0
                .contains("invalid job cache nonce"));
        }
    }
}

#[test]
fn early_boot_budget_uses_the_same_checked_line_with_credentials_reserved() {
    let nonce = "0".repeat(REMOTE_CACHE_DRIVE_NONCE_BYTES * 2);
    for vcpus in [1, 32] {
        for bytes in [4 << 30, u64::MAX - 511] {
            let mut shape = start();
            shape.vcpus = vcpus;
            let early = early_boot_command_line(&shape, bytes, None).unwrap();
            let actual = boot_requests(&shape, Some(17), Some(&nonce), bytes).unwrap();
            assert_eq!(actual[1].1["boot_args"], early);
            assert!(boot_command_line(&shape, None, bytes, None).unwrap().len() < early.len());
            assert!(early.len() < JOB_BOOT_CMDLINE_MAX_BYTES);
        }
    }
}

#[test]
fn jailer_enters_the_job_namespace_before_starting_firecracker() {
    let netns = namespace::path(&start().job_id).unwrap();
    let command = jailer_command(
        Path::new("/binaries"),
        Path::new("/jail"),
        &netns,
        &start().job_id,
        500000,
    );
    let args: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
    let end = args.iter().position(|s| *s == "--").unwrap();
    let option = args.iter().position(|s| *s == "--netns").unwrap();
    assert!(option < end);
    assert_eq!(args[option + 1], netns.to_str().unwrap());
    assert_eq!(args[end + 1..], ["--api-sock", "/api.sock"]);
    assert!(!args.contains(&"--cgroup-version"));
}

#[test]
fn cold_boot_uses_request_size_and_exactly_the_three_job_images() {
    let requests = boot_requests(&start(), Some(17), None, 4 << 30).unwrap();
    let paths: Vec<_> = requests.iter().map(|r| r.0).collect();
    assert_eq!(
        paths,
        [
            "/machine-config",
            "/boot-source",
            "/drives/root",
            "/pmem/toolchain",
            "/drives/workspace",
            "/vsock",
            "/balloon",
            "/network-interfaces/eth0",
            "/actions"
        ]
    );
    assert_eq!(
        requests[0].1,
        json!({"vcpu_count":7,"mem_size_mib":1536,"smt":false})
    );
    assert_eq!(
        requests[2].1,
        json!({"drive_id":"root","path_on_host":"/rootfs.ext4","is_root_device":true,"is_read_only":true})
    );
    assert_eq!(
        requests[3].1,
        json!({"id":"toolchain","path_on_host":"/toolchain.ext4","read_only":true})
    );
    assert_eq!(
        requests[4].1,
        json!({"drive_id":"workspace","path_on_host":"/workspace.img","is_root_device":false,"is_read_only":false})
    );
    assert_eq!(
        requests[6].1,
        json!({"amount_mib":0,"deflate_on_oom":true,"free_page_reporting":true,"stats_polling_interval_s":0})
    );
    assert_eq!(
        requests[7].1,
        json!({"iface_id":"eth0","host_dev_name":"mtap17","guest_mac":"02:fc:00:00:11:02"})
    );
    assert!(requests[1].1["boot_args"]
        .as_str()
        .unwrap()
        .contains("net.ifnames=0"));
    assert_eq!(requests[8].1, json!({"action_type":"InstanceStart"}));
    let mut different = start();
    different.vcpus = 2;
    different.memory_mib = 8192;
    assert_eq!(
        boot_requests(&different, Some(18), None, 4 << 30).unwrap()[0].1,
        json!({"vcpu_count":2,"mem_size_mib":8192,"smt":false})
    );
}

#[test]
fn attached_disk_is_refused_by_identity_and_inode_until_released() {
    let base = scratch();
    let mut jobs = machine(&base);
    jobs.settings = Some(JobVms {
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
        uid_base: 500000,
        uid_count: 1,
        memory_max_bytes: 1048576,
    });
    jobs.adopt_record(record()).unwrap();
    let mut next = start();
    next.job_id = job::test_id(2);
    let error = jobs
        .handle(
            Operation::Start(next),
            Peer {
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
            },
        )
        .err()
        .unwrap();
    assert!(error.0.contains("already attached"), "{}", error.0);
    assert!(jobs
        .disk_available(&start(), 10, 11)
        .unwrap_err()
        .0
        .contains("already attached"));
    let mut alias = start();
    alias.workspace_disk_id = "alias".into();
    alias.account = "other".into();
    assert!(jobs.disk_available(&alias, 8, 99).is_err());
    jobs.disk_available(&alias, 8, 100).unwrap();
    let mut released = jobs.records()[&start().job_id].clone();
    released.released = true;
    jobs.insert_record(released);
    jobs.disk_available(&start(), 8, 99).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn helper_restart_re_adopts_a_live_vmm_without_killing_it() {
    let mut vmm = Vmm::new();
    let old = machine(&vmm.base);
    let group = old.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    old.adopt_record(record()).unwrap();
    let durable = serde_json::to_vec(old.records().get(&start().job_id).unwrap()).unwrap();
    // Dropping the old helper's child handles must not signal their processes.
    let child = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let child_pid = child.id();
    old.cell(&start().job_id).unwrap().0.lock().child = Some(child);
    drop(old);
    let restarted = machine(&vmm.base);
    restarted
        .adopt_record(serde_json::from_slice(&durable).unwrap())
        .unwrap();
    assert!(matches!(restarted.status(&start().job_id).unwrap().reply,
        Reply::Job { known: true, running: true, vmm_pid: Some(pid), exit_report: None, tap_index: None } if pid == vmm.child.id()));
    assert!(vmm.child.try_wait().unwrap().is_none());
    assert_eq!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
    unsafe {
        libc::kill(child_pid as i32, libc::SIGKILL);
        libc::waitpid(child_pid as i32, std::ptr::null_mut(), 0);
    }
}

#[test]
fn saved_start_record_reconciles_a_live_vmm_across_processes() {
    const TEST: &str =
        "linux::job::tests::saved_start_record_reconciles_a_live_vmm_across_processes";
    if let Some(base) = std::env::var_os("MOTOR_VM_JOB_RECORD_TEST_ROOT") {
        let base = PathBuf::from(base);
        let start = restart_start(&base);
        let mut restarted = machine(&base);
        let paths: Vec<_> = std::fs::read_dir(&restarted.records_path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(paths.len(), 1);
        let saved: Record = serde_json::from_reader(File::open(&paths[0]).unwrap()).unwrap();
        saved.start.validate().unwrap();
        assert_eq!(
            paths[0].file_stem().unwrap().to_str().unwrap(),
            saved.start.job_id
        );
        restarted.settings = Some(JobVms {
            workspace_disk_max_bytes: Some(1073741824),
            fixed_path_repositories: Default::default(),
            uid_base: saved.uid,
            uid_count: 1,
            memory_max_bytes: 268435456,
        });
        // The root service test covers trusted-file loading. This portable test
        // feeds the saved JSON into the same reconciliation used after loading.
        restarted.reconcile_records(vec![saved]).unwrap();
        let expected: u32 = std::fs::read_to_string(base.join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        let reply = restarted.status(&start.job_id).unwrap().reply;
        assert!(
            matches!(reply,
                Reply::Job { known: true, running: true, vmm_pid: Some(pid), exit_report: None, tap_index: Some(17) } if pid == expected),
            "expected the saved job's live VMM {expected}, got {reply:?}"
        );
        return;
    }

    let mut vmm = Vmm::new();
    let jobs = machine(&vmm.base);
    let mut start = start();
    // A distinct suffix makes regenerating the default start fail even if both
    // test processes happen to initialize their UUID timestamp in the same ms.
    start.job_id = job::test_id(42);
    tenant::durable_write(
        &vmm.base.join("start.json"),
        &serde_json::to_vec(&start).unwrap(),
    )
    .unwrap();
    let record = start_record(&jobs, start.clone(), unsafe { libc::geteuid() }, Some(17));
    std::fs::create_dir_all(&jobs.records_path).unwrap();
    jobs.save(&record).unwrap();
    let group = jobs.group(&start.job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    std::fs::write(vmm.base.join("pid"), vmm.child.id().to_string()).unwrap();
    drop(jobs);

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("MOTOR_VM_JOB_RECORD_TEST_ROOT", &vmm.base)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fresh helper process failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(vmm.child.try_wait().unwrap().is_none());
}

#[test]
fn adoption_refuses_a_cgroup_with_the_wrong_uid() {
    let vmm = Vmm::new();
    let jobs = machine(&vmm.base);
    let group = jobs.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    let mut wrong = record();
    wrong.uid += 1;
    assert!(jobs
        .adopt_record(wrong)
        .unwrap_err()
        .0
        .contains("identity mismatch"));
}

#[test]
fn job_vmm_oom_score_is_read_back_above_host_and_helper() {
    let vmm = Vmm::new();
    let host = read_oom_score(std::process::id()).unwrap();
    let helper = read_oom_score(std::process::id()).unwrap();
    set_job_oom_score(vmm.child.id(), host, helper).unwrap();
    let actual = std::fs::read_to_string(format!("/proc/{}/oom_score_adj", vmm.child.id()))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(actual > host && actual > helper);
    assert!(check_oom_order(1000, helper).is_err());
    assert!(check_oom_order(host, 1000).is_err());
}

#[test]
fn job_child_is_placed_before_exec() {
    let base = scratch();
    let path = base.join("procs-written-before-exec");
    let procs = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let mut command = Command::new("/bin/sleep");
    command
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || super::super::raw_place(procs.as_raw_fd()));
    }
    let child = command.spawn().unwrap();
    let _vmm = Vmm { child, base };
    // spawn returns only after the exec handshake. Placement must have happened
    // before the guest executable could process even one request.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "0");
}

#[test]
fn final_oom_evidence_is_not_invented() {
    let base = scratch();
    assert!(read_oom_kill(&base).is_err());
    std::fs::write(base.join("memory.events"), "oom 7\noom_kill 3\n").unwrap();
    assert_eq!(read_oom_kill(&base).unwrap(), 3);
    std::fs::write(base.join("memory.events"), "oom 7\n").unwrap();
    assert!(read_oom_kill(&base).is_err());
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(feature = "linux-service-tests")]
fn slice_state(point: &str) {
    let output = Command::new("systemctl")
        .args([
            "show",
            "motorvmtest.slice",
            "-p",
            "ActiveState,SubState,Delegate",
        ])
        .output()
        .unwrap();
    eprintln!(
        "{point}: systemctl show motorvmtest.slice: status={}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "linux-service-tests")]
fn child_placement(point: &str, pid: u32) {
    eprintln!(
        "{point}: pid={pid} cgroup={:?}",
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
    );
}

#[cfg(feature = "linux-service-tests")]
fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Runs outside remote jobs as root: it creates a disposable systemd service
/// with the shipped helper's kill/delegation settings and a separate test slice.
/// Both service incarnations read one serialized Start and the durable record
/// uses the production start-record constructor; no KVM or guest image is needed.
/// The shared scratch root and its ancestors must be root-owned, non-writable
/// by other users, and traversable by the VM uid in 600000..600254; do not use
/// a directory below /root. On a Linux build server with systemd 254+ as PID 1,
/// a writable cgroup v2 hierarchy and a C compiler, run:
///
/// ```sh
/// mkdir -p target/scratch
/// cc -O2 -Wall -Wextra -Werror crates/motor-vm-helper/fixtures/job-firecracker.c -o target/scratch/job-firecracker
/// TMPDIR=/var/lib/motor-root-tests \
/// MOTOR_VM_TEST_JOB_FIRECRACKER="$PWD/target/scratch/job-firecracker" \
/// cargo nextest run -p motor-vm-helper --features linux-service-tests \
///   -E 'test(=linux::job::tests::isolated_job_vmm_survives_helper_service_restart)' --no-capture
/// ```
#[test]
#[cfg(feature = "linux-service-tests")]
fn isolated_job_vmm_survives_helper_service_restart() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    const TEST: &str = "linux::job::tests::isolated_job_vmm_survives_helper_service_restart";
    const SLICE: &str = "/sys/fs/cgroup/motorvmtest.slice";
    let phase = std::env::var("MOTOR_VM_JOB_TEST_PHASE");
    let base = if phase.is_ok() {
        PathBuf::from(std::env::var_os("MOTOR_VM_JOB_TEST_ROOT").unwrap())
    } else {
        let base = scratch();
        tenant::durable_write(
            &base.join("start.json"),
            &serde_json::to_vec(&start()).unwrap(),
        )
        .unwrap();
        base
    };
    let start = || restart_start(&base);
    if let Ok(phase) = phase {
        let mut jobs = machine(&base);
        jobs.remove_test_group = None;
        jobs.groups = SLICE.into();
        jobs.settings = Some(JobVms {
            workspace_disk_max_bytes: Some(1073741824),
            fixed_path_repositories: Default::default(),
            uid_base: 600000,
            uid_count: 254,
            memory_max_bytes: 268435456,
        });
        if phase == "start" {
            trusted_directory(&jobs.records_path).unwrap();
            let uid = jobs.allocate_uid(jobs.settings.as_ref().unwrap()).unwrap();
            let record = start_record(&jobs, start(), uid, None);
            let group = jobs.group(&record.start.job_id);
            cgroup::create_job(&group, 1).unwrap();
            let procs = OpenOptions::new()
                .write(true)
                .open(group.join("cgroup.procs"))
                .unwrap();
            let binary = base.join("firecracker");
            let mut command = Command::new(binary);
            command
                .arg("300")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear();
            unsafe {
                command.pre_exec(move || {
                    super::super::raw_place(procs.as_raw_fd())?;
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(uid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            // This no-KVM fixture has the same lifetime, uid and placement as
            // jailer's Firecracker child; the test needs no guest image.
            jobs.save(&record).unwrap();
            let child = command.spawn().unwrap();
            child_placement("inner immediately after move", child.id());
            set_job_oom_score(
                child.id(),
                read_oom_score(std::process::id()).unwrap(),
                read_oom_score(std::process::id()).unwrap(),
            )
            .unwrap();
            tenant::durable_write(&base.join("pid"), child.id().to_string().as_bytes()).unwrap();
            let placement =
                std::fs::read_to_string(format!("/proc/{}/cgroup", child.id())).unwrap();
            assert!(placement.contains("/motorvmtest.slice/job-"), "{placement}");
            assert!(
                !placement.contains("motor-vm-helper-job-test.service"),
                "{placement}"
            );
            jobs.adopt_record(record).unwrap();
            jobs.cell(&start().job_id).unwrap().0.lock().child = Some(child);
            assert_eq!(
                std::fs::read_to_string(group.join("memory.max"))
                    .unwrap()
                    .trim(),
                "max"
            );
            assert_eq!(
                std::fs::read_to_string(group.join("memory.high"))
                    .unwrap()
                    .trim(),
                "max"
            );
            assert!(std::fs::read_to_string("/proc/self/cgroup")
                .unwrap()
                .contains("/motor-vm-helper-job-test.service/helper"));
            wait_for_file(&base.join("allow-helper-exit"));
            let cell = jobs.cell(&start().job_id).unwrap();
            let mut state = cell.0.lock();
            let child = state.child.as_mut().unwrap();
            let status = child.try_wait().unwrap();
            eprintln!("inner before exit: child try_wait={status:?}");
            assert!(
                status.is_none(),
                "job VMM fixture exited before the helper: {status:?}"
            );
            slice_state("inner before service exit");
            child_placement("inner immediately before service exit", child.id());
        } else {
            jobs.reconcile().unwrap();
            let expected: u32 = std::fs::read_to_string(base.join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            let reply = jobs.status(&start().job_id).unwrap().reply;
            assert!(
                matches!(reply,
                    Reply::Job { known: true, running: true, vmm_pid: Some(pid), exit_report: None, tap_index: None } if pid == expected),
                "after the helper restart, expected the running job VMM {expected}, got {reply:?}"
            );
            assert_eq!(read_oom_score(expected).unwrap(), JOB_OOM_SCORE);
        }
        return;
    }
    // Ancestors of helper-owned records must already be root-controlled. The
    // operator must set a root-controlled, uid-traversable shared scratch root
    // such as TMPDIR=/var/lib/motor-root-tests for this service restart test.
    // A path below /root prevents the dropped-uid child from executing.
    for ancestor in base.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor).unwrap()).unwrap();
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Compile fixtures/job-firecracker.c on the build server and name its path.
    // Copying a multicall /bin/sleep under a different name can exit immediately
    // because its applet dispatch uses argv[0], not the supplied sleep interval.
    let fixture = std::env::var_os("MOTOR_VM_TEST_JOB_FIRECRACKER").expect(
        "set MOTOR_VM_TEST_JOB_FIRECRACKER to the compiled fixtures/job-firecracker.c executable",
    );
    std::fs::copy(fixture, base.join("firecracker")).unwrap();
    let run = |args: &[&str]| {
        assert!(Command::new("systemctl")
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    run(&["start", "motorvmtest.slice"]);
    run(&[
        "set-property",
        "--runtime",
        "motorvmtest.slice",
        "MemoryMax=268435456",
    ]);
    cgroup::enable(Path::new(SLICE)).unwrap();
    let group = Path::new(SLICE).join(format!("job-{}", start().job_id));
    assert!(!group.exists(), "leftover job service-test cgroup");
    let _resources = ServiceResources::new(base.clone(), group);
    let unit = include_str!("../../../systemd/motor-vm-helper.service");
    let properties: Vec<_> = ["Delegate=", "DelegateSubgroup=", "KillMode="]
        .iter()
        .map(|prefix| {
            format!(
                "--property={}",
                unit.lines().find(|line| line.starts_with(prefix)).unwrap()
            )
        })
        .collect();
    for phase in ["start", "adopt"] {
        slice_state(&format!("outer before {phase} service"));
        let mut service = Command::new("systemd-run")
            .args([
                "--wait",
                "--pipe",
                "--collect",
                "--unit=motor-vm-helper-job-test",
            ])
            .args(&properties)
            .arg(format!("--setenv=MOTOR_VM_JOB_TEST_PHASE={phase}"))
            .arg(format!(
                "--setenv=MOTOR_VM_JOB_TEST_ROOT={}",
                base.display()
            ))
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .spawn()
            .unwrap();
        wait_for_file(&base.join("pid"));
        let pid: u32 = std::fs::read_to_string(base.join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        // A pidfd observes exit without confusing a reused pid for the VMM.
        // waitid may report ECHILD because systemd, not this test, is its reaper.
        use std::os::fd::FromRawFd;
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        assert!(
            pidfd >= 0,
            "pidfd_open: {}",
            std::io::Error::last_os_error()
        );
        let pidfd = unsafe { File::from_raw_fd(pidfd) };
        if phase == "start" {
            child_placement("outer while inner helper is alive", pid);
            slice_state("outer immediately before allowing inner helper exit");
            std::fs::write(base.join("allow-helper-exit"), "1").unwrap();
        }
        assert!(service.wait().unwrap().success());
        slice_state(&format!("outer after {phase} service exit"));
        child_placement("outer after service exit", pid);
        let mut event = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut event, 1, 0) } >= 0);
        eprintln!("outer pidfd exit readiness: {}", event.revents);
        if event.revents != 0 {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    pidfd.as_raw_fd() as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                eprintln!(
                    "outer child exit: code={} status={}",
                    info.si_code,
                    unsafe { info.si_status() }
                );
            } else {
                eprintln!(
                    "outer cannot reap child exit status: {}",
                    std::io::Error::last_os_error()
                );
            }
            let journal = Command::new("journalctl")
                .args([
                    "--no-pager",
                    "-n",
                    "30",
                    "-u",
                    "motorvmtest.slice",
                    "-u",
                    "motor-vm-helper-job-test.service",
                ])
                .output()
                .unwrap();
            eprintln!(
                "job test unit journal:\n{}",
                String::from_utf8_lossy(&journal.stdout)
            );
            eprintln!(
                "job cgroup memory.events after exit: {:?}",
                std::fs::read_to_string(
                    Path::new(SLICE).join(format!("job-{}/memory.events", start().job_id))
                )
            );
        }
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        let placement = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        assert!(placement.contains("/motorvmtest.slice/job-"));
        assert!(!placement.contains("motor-vm-helper-job-test.service"));
    }
    let group = Path::new(SLICE).join(format!("job-{}", start().job_id));
    std::fs::write(group.join("cgroup.kill"), "1").unwrap();
    tenant::wait_empty(&group).unwrap();
    // The helper was absent at exit. Startup must recover the real cgroup's
    // evidence, persist it with root ownership, and replay it until acked.
    let mut restarted = machine(&base);
    restarted.groups = SLICE.into();
    restarted.remove_test_group = None;
    restarted.settings = Some(JobVms {
        workspace_disk_max_bytes: Some(1073741824),
        fixed_path_repositories: Default::default(),
        uid_base: 600000,
        uid_count: 254,
        memory_max_bytes: 268435456,
    });
    restarted.reconcile().unwrap();
    assert!(!group.exists());
    let report = ExitReport {
        job_id: start().job_id,
        oom_kill: 0,
        egress_bytes: None,
    };
    assert!(
        matches!(restarted.handle(Operation::Exits, Peer { uid: 0, gid: 0 }).unwrap().reply,
        Reply::JobExits { exits } if exits == [report.clone()])
    );
    let path = restarted
        .records_path
        .join(format!("{}.json", start().job_id));
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.uid(), 0);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    restarted.acknowledge_exit(report.clone()).unwrap();
    restarted.registry.lock().clear();
    restarted.reconcile().unwrap();
    assert!(
        matches!(restarted.handle(Operation::Exits, Peer { uid: 0, gid: 0 }).unwrap().reply,
        Reply::JobExits { exits } if exits.is_empty())
    );
    restarted.acknowledge_exit(report).unwrap();
}
#[test]
fn fill_boot_authority_comes_from_the_root_disk_role_not_the_requested_start_kind() {
    let mut start = start();
    start.kind = job::VmKind::Fill;
    let mut requested = boot_requests(&start, Some(7), None, 4u64 << 30).unwrap();
    let boot = |requests: &[(&str, Value)]| {
        requests
            .iter()
            .find(|(path, _)| *path == "/boot-source")
            .unwrap()
            .1["boot_args"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(!boot(&requested)
        .split_whitespace()
        .any(|word| word == "motor.fill=1"));
    authorize_fill_boot(&mut requested, job::WorkspaceRole::Job).unwrap();
    assert!(!boot(&requested)
        .split_whitespace()
        .any(|word| word == "motor.fill=1"));
    authorize_fill_boot(&mut requested, job::WorkspaceRole::Fill).unwrap();
    assert!(boot(&requested)
        .split_whitespace()
        .any(|word| word == "page_reporting.page_reporting_order=5"));
    assert!(boot(&requested)
        .split_whitespace()
        .any(|word| word == "motor.fill=1"));
}

#[test]
fn replayed_token_after_helper_restart_cannot_start_a_second_write_vm() {
    let base = scratch();
    let mut record = record();
    record.start.account = crate::cache_write::tests::ACCOUNT.into();
    let id = record.start.job_id.clone();
    let account = record.start.account.clone();
    crate::cache_write::tests::with_fresh_token(&id, &account, |config, token| {
        for tombstone in [false, true] {
            let first = machine_with_config(&base, config);
            first.cache_write_grants.admit(token).unwrap();
            std::fs::create_dir_all(&first.records_path).unwrap();
            record.released = tombstone;
            record.exit_acknowledged = tombstone;
            record.oom_kill = tombstone.then_some(0);
            first.save(&record).unwrap();
            first.insert_record(record.clone());
            first.cache_write_grants.consume(&id);
            assert!(first.status(&id).is_ok());
            assert!(first.cache_write_grants.get(&id).is_none());
            assert!(!first.boundary(&id).exists());
            let path = first.records_path.join(format!("{id}.json"));
            drop(first);
            let mut restarted = machine_with_config(&base, config);
            restarted.settings = Some(fixed_path_settings(json!({})));
            let saved = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            restarted.adopt_record(saved).unwrap();
            restarted.cache_write_grants.admit(token).unwrap();
            assert!(restarted.cache_write_grants.get(&id).is_some());
            let error = restarted
                .start(record.start.clone(), Peer { uid: 0, gid: 0 })
                .err()
                .unwrap();
            assert_eq!(error.0, "job id already booted; use adopt_job");
            assert!(!restarted.boundary(&id).exists());
        }
    });
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn write_boot_flag_requires_job_role_and_staged_credentials_without_granting_fill() {
    let start = start();
    let nonce = "ab".repeat(REMOTE_CACHE_DRIVE_NONCE_BYTES);
    let mut requests = boot_requests(&start, Some(1), Some(&nonce), 4096).unwrap();
    authorize_job_write_boot(&mut requests, &start, Some(&nonce), true).unwrap();
    let boot = requests
        .iter()
        .find(|(path, _)| *path == "/boot-source")
        .unwrap()
        .1["boot_args"]
        .as_str()
        .unwrap();
    assert!(boot
        .split_whitespace()
        .any(|part| part == "motor.cache_write=1"));
    assert!(!boot.contains("motor.fill=1"));
    assert!(!boot.contains("motor.recovery=1"));
    assert_eq!(
        requests
            .iter()
            .find(|(path, _)| *path == "/drives/remote_cache")
            .unwrap()
            .1["is_read_only"],
        true
    );
    assert_eq!(start.kind, job::VmKind::Job);
    for case in ["missing-key", "no-disk", "block", "fill", "recovery"] {
        let mut start = start.clone();
        match case {
            "block" => start.cache_write_blocked = true,
            "fill" => start.kind = job::VmKind::Fill,
            "recovery" => start.kind = job::VmKind::Recovery,
            _ => {}
        }
        let nonce = (case != "no-disk").then_some(nonce.as_str());
        let mut requests = boot_requests(&start, Some(1), nonce, 4096).unwrap();
        let write = case != "missing-key";
        let result = authorize_job_write_boot(&mut requests, &start, nonce, write);
        if case == "missing-key" {
            result.unwrap();
        } else {
            assert!(result.is_err(), "{case}");
        }
        let boot = requests
            .iter()
            .find(|(path, _)| *path == "/boot-source")
            .unwrap()
            .1["boot_args"]
            .as_str()
            .unwrap();
        assert!(!boot.contains("motor.cache_write=1"), "{case}");
    }
}

#[test]
fn start_retry_before_durable_intent_reuses_grant_for_write_boot_configuration() {
    let base = scratch();
    let mut start = start();
    start.account = crate::cache_write::tests::ACCOUNT.into();
    crate::cache_write::tests::with_fresh_token(&start.job_id, &start.account, |config, token| {
        let jobs = machine_with_config(&base, config);
        jobs.cache_write_grants.admit(token).unwrap();
        for _ in 0..2 {
            let error = jobs
                .start(start.clone(), Peer { uid: 0, gid: 0 })
                .err()
                .unwrap();
            assert_eq!(error.0, "job VM settings are required");
            assert!(!jobs
                .records_path
                .join(format!("{}.json", start.job_id))
                .exists());
            let grant = jobs.cache_write_grants.get(&start.job_id).unwrap().clone();
            let mut disk = disk_registry(&start);
            disk.workspace_key_hash = Some(grant.workspace_key_hash.clone());
            assert!(grant.permits(&start, &disk));
            let nonce = "ab".repeat(REMOTE_CACHE_DRIVE_NONCE_BYTES);
            let mut requests = boot_requests(&start, Some(1), Some(&nonce), 4096).unwrap();
            authorize_job_write_boot(
                &mut requests,
                &start,
                Some(&nonce),
                grant.permits(&start, &disk),
            )
            .unwrap();
            let boot = requests
                .iter()
                .find(|(path, _)| *path == "/boot-source")
                .unwrap()
                .1["boot_args"]
                .as_str()
                .unwrap();
            assert!(boot.contains("motor.cache_write=1"));
        }
    });
    std::fs::remove_dir_all(base).unwrap();
}
