use super::*;
use motor_host::jobs::{images::Images, Resources};
use motor_vm_helper::images::{Artifact, Manifest};

const TOOLCHAIN: u64 = 8 * 1024 * 1024;

fn manifest() -> Manifest {
    let artifact = |bytes| Artifact {
        bytes,
        sha256: "a".repeat(64),
    };
    Manifest {
        kernel: artifact(4096),
        rootfs: artifact(1024 * 1024),
        toolchain: artifact(TOOLCHAIN),
        hello: json!({"type":"hello", "version":1}),
        guest_init: json!({}),
    }
}

fn publish(h: &mut Host, id: &str) {
    h.begin_image_publication(id, manifest(), AT).unwrap();
    h.image_artifacts_ready(id).unwrap();
    let accounts = h.snapshot.per_job.images[id]
        .accounts
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for account in accounts {
        h.image_account_ready(id, &account).unwrap();
    }
    h.finish_image_publication(id).unwrap();
}

#[test]
fn generations_pin_at_claim_across_publication_gc_attachment_and_restart() {
    let root = scratch();
    let storage = Storage::new(FLOOR + 20 * CAP);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    publish(&mut h, "gen-1");
    h.claim(job(JOB), AT).unwrap();
    h.claim(job(JOB2), AT).unwrap();
    let execution = h.snapshot.per_job.executions[JOB].clone();
    assert_eq!(execution.image_generation.as_deref(), Some("gen-1"));
    let draining = h.snapshot.draining;
    publish(&mut h, "gen-2");
    assert_eq!(h.snapshot.draining, draining);
    assert!(h.begin_image_gc("gen-1").is_err());
    assert!(h.begin_image_gc("gen-2").is_err());
    drop(h);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    assert!(h.begin_image_gc("gen-1").is_err());
    assert!(h.begin_job_boot(JOB).unwrap().is_some());
    let host = Arc::new(Mutex::new(h));
    let resources = Images::new(host.clone(), storage.clone());
    // Publication and a GC attempt happened after claim but before the helper
    // receives this assignment. The helper must attach the original files.
    let execution = host.lock().unwrap().snapshot.per_job.executions[JOB].clone();
    assert_eq!(
        resources.acquire(&execution).unwrap().image_generation_id,
        "gen-1"
    );
    {
        let mut h = host.lock().unwrap();
        h.job_boot_failed(JOB, AT + 1).unwrap();
        assert!(h.begin_image_gc("gen-1").is_err());
        h.job_stopped(JOB, false).unwrap();
        assert!(h.begin_image_gc("gen-1").is_err());
        h.job_stopped(JOB2, false).unwrap();
        h.begin_image_gc("gen-1").unwrap();
    }
    drop(resources);
    drop(host);
    let mut h = durable_host(root.path(), 10_000);
    assert!(h.snapshot.per_job.images["gen-1"].deleting);
    h.finish_image_gc("gen-1").unwrap();
    assert!(h.snapshot.per_job.images["gen-1"].removed);
    assert!(h.begin_image_publication("gen-1", manifest(), AT).is_err());
    enable(&mut h, &storage);
    h.claim(job(JOB3), AT).unwrap();
    assert_eq!(
        h.snapshot.per_job.executions[JOB3]
            .image_generation
            .as_deref(),
        Some("gen-2")
    );
}

#[test]
fn missing_account_copy_is_reserved_once_in_the_claim_transaction() {
    let root = scratch();
    let mut h = durable_host(root.path(), 10_000);
    let storage = Storage::new(FLOOR + 10 * CAP);
    enable(&mut h, &storage);
    publish(&mut h, "gen-1");
    storage
        .free
        .store(FLOOR + CAP + JOB_BYTES + TOOLCHAIN - 1, Ordering::SeqCst);
    full(h.claim(job(JOB), AT));
    assert!(h.snapshot.per_job.images["gen-1"].accounts.is_empty());
    storage.free.fetch_add(1, Ordering::SeqCst);
    h.claim(job(JOB), AT).unwrap();
    assert!(!h.snapshot.per_job.images["gen-1"].accounts[&job(JOB).sub]);
    drop(h);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    full(h.claim(job(JOB2), AT));
    storage.free.fetch_add(JOB_BYTES, Ordering::SeqCst);
    h.claim(job(JOB2), AT).unwrap();
    assert_eq!(h.snapshot.per_job.images["gen-1"].accounts.len(), 1);
    let mut foreign = other(JOB3, 8);
    foreign.sub = "other-account".into();
    storage.free.fetch_add(CAP + JOB_BYTES, Ordering::SeqCst);
    full(h.claim(foreign.clone(), AT));
    assert_eq!(h.snapshot.per_job.images["gen-1"].accounts.len(), 1);
    storage.free.fetch_add(TOOLCHAIN, Ordering::SeqCst);
    h.claim(foreign, AT).unwrap();
    assert_eq!(h.snapshot.per_job.images["gen-1"].accounts.len(), 2);
}

#[test]
fn publication_and_admissions_respect_shared_base_growth_and_maximum_replay() {
    let mut h = memory_host(10_000);
    let storage = Storage::new(FLOOR + 10 * CAP);
    enable(&mut h, &storage);
    publish(&mut h, "gen-1");
    h.claim(job(JOB), AT).unwrap();
    let first = disk(&h, JOB);
    // A fully populated reflink base still promises the whole cap for writes;
    // allocated shared extents are not exclusive and cannot reduce that promise.
    storage.exclusive.lock().unwrap().insert(first.clone(), 0);
    let publication = manifest().publication_bytes(1).unwrap();
    storage.free.store(
        FLOOR + CAP + JOB_BYTES + TOOLCHAIN + publication - 1,
        Ordering::SeqCst,
    );
    let before = h.snapshot.clone();
    assert!(h.begin_image_publication("gen-2", manifest(), AT).is_err());
    assert_eq!(h.snapshot, before);
    storage.free.fetch_add(1, Ordering::SeqCst);
    h.begin_image_publication("gen-2", manifest(), AT).unwrap();
    assert!(h.finish_image_publication("gen-2").is_err());
    full(h.claim(job(JOB2), AT));
    assert_eq!(
        h.snapshot.per_job.disks[&first]
            .storage
            .as_ref()
            .unwrap()
            .growth_reservation_bytes,
        CAP
    );
    assert_eq!(
        h.snapshot.per_job.executions[JOB].replay_reservation_bytes,
        Some(REPLAY_RESERVATION_BYTES)
    );
    assert_eq!(
        h.snapshot.per_job.images["gen-2"]
            .accounts
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![job(JOB).sub]
    );
    h.abort_image_publication("gen-2").unwrap();
    storage
        .free
        .store(FLOOR + CAP + 2 * JOB_BYTES + TOOLCHAIN, Ordering::SeqCst);
    h.claim(job(JOB2), AT).unwrap();
}

#[test]
fn interrupted_publication_never_replaces_current_and_keeps_its_space_promise() {
    let root = scratch();
    let storage = Storage::new(FLOOR + 20 * CAP);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    publish(&mut h, "gen-1");
    h.claim(job(JOB), AT).unwrap();
    h.begin_image_publication("gen-2", manifest(), AT).unwrap();
    assert!(h.begin_image_gc("gen-2").is_err());
    drop(h);
    let mut h = durable_host(root.path(), 10_000);
    enable(&mut h, &storage);
    h.claim(job(JOB2), AT).unwrap();
    assert_eq!(
        h.snapshot.per_job.executions[JOB2]
            .image_generation
            .as_deref(),
        Some("gen-1")
    );
    assert!(h.snapshot.per_job.images["gen-2"].pending);
    assert!(h.finish_image_publication("gen-2").is_err());
    h.abort_image_publication("gen-2").unwrap();
    assert!(h.snapshot.per_job.images["gen-1"].current);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root and a disposable loop-mounted XFS filesystem"]
fn isolated_tight_host_space_stops_admissions_and_publication_before_reflink_overwrites() {
    use std::{
        fs::File,
        io::Write,
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
        path::{Path, PathBuf},
        process::Command,
    };
    assert_eq!(unsafe { libc::geteuid() }, 0);
    fn free(path: &Path) -> u64 {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        assert_eq!(
            unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) },
            0
        );
        let stat = unsafe { stat.assume_init() };
        stat.f_bavail * stat.f_frsize
    }
    fn fill(path: &Path, mut bytes: u64) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        let byte = if path.file_name().unwrap() == "workspace" {
            2u8
        } else {
            1u8
        };
        let block = vec![byte; 1024 * 1024];
        while bytes > 0 {
            let n = bytes.min(block.len() as u64) as usize;
            file.write_all(&block[..n]).unwrap();
            bytes -= n as u64;
        }
        file.sync_all().unwrap();
    }
    struct Mounted(PathBuf);
    impl Drop for Mounted {
        fn drop(&mut self) {
            assert!(Command::new("/usr/bin/umount")
                .arg(&self.0)
                .status()
                .unwrap()
                .success());
        }
    }
    struct Space {
        root: PathBuf,
        ordinary: Arc<Storage>,
    }
    impl Control for Space {
        fn available_bytes(&self) -> anyhow::Result<u64> {
            Ok(free(&self.root))
        }
        fn workspace_usage(&self, _: &str, _: &str) -> anyhow::Result<Option<u64>> {
            Ok(Some(0))
        }
        fn capacity(&self) -> anyhow::Result<Capacity> {
            let mut capacity = self.ordinary.capacity()?;
            capacity.storage_margin_bytes = 16 * 1024 * 1024;
            Ok(capacity)
        }
        fn adopt(&self, s: &str) -> anyhow::Result<bool> {
            self.ordinary.adopt(s)
        }
        fn wake(&self, s: &str, g: Option<u64>) -> anyhow::Result<u64> {
            self.ordinary.wake(s, g)
        }
        fn quota(&self, s: &str, n: u32) -> anyhow::Result<()> {
            self.ordinary.quota(s, n)
        }
        fn relay(&self, s: &str, p: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
            self.ordinary.relay(s, p)
        }
        fn admin(&self, s: &str, v: &Value) -> anyhow::Result<Value> {
            self.ordinary.admin(s, v)
        }
        fn suspend(&self, s: &str) -> anyhow::Result<()> {
            self.ordinary.suspend(s)
        }
    }
    let scratch = scratch();
    let image = scratch.path().join("host.xfs");
    File::create(&image)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    assert!(Command::new("/usr/sbin/mkfs.xfs")
        .args(["-q", "-f", "-m", "reflink=1"])
        .arg(&image)
        .status()
        .unwrap()
        .success());
    let root = scratch.path().join("host");
    std::fs::create_dir(&root).unwrap();
    assert!(Command::new("/usr/bin/mount")
        .args(["-o", "loop"])
        .arg(&image)
        .arg(&root)
        .status()
        .unwrap()
        .success());
    let _mounted = Mounted(root.clone());
    const SMALL_CAP: u64 = 16 * 1024 * 1024;
    fill(&root.join("base"), SMALL_CAP);
    let clone = File::create(root.join("workspace")).unwrap();
    let base = File::open(root.join("base")).unwrap();
    assert_eq!(
        unsafe {
            libc::ioctl(
                clone.as_raw_fd(),
                0x40049409 as libc::c_ulong,
                base.as_raw_fd(),
            )
        },
        0
    );
    clone.sync_all().unwrap();
    let control = Arc::new(Space {
        root: root.clone(),
        ordinary: Storage::new(0),
    });
    let mut h = memory_host(10_000);
    publish(&mut h, "gen-1");
    if let motor_host::config::HostSettings::PerJob(s) = &mut h.settings {
        s.workspace_disk_cap_bytes = SMALL_CAP;
    }
    h.set_job_storage(control.clone());
    h.enforce_storage();
    h.observe_storage(
        control.capacity().map_err(|e| e.to_string()),
        control.available_bytes().map_err(|e| e.to_string()),
    );
    let threshold = control.capacity().unwrap().storage_margin_bytes;
    let desired = threshold + SMALL_CAP + JOB_BYTES + TOOLCHAIN + 8 * 1024 * 1024;
    fill(&root.join("other-used-space"), free(&root) - desired);
    h.claim(job(JOB), AT).unwrap();
    full(h.claim(job(JOB2), AT));
    full(h.claim(other(JOB3, 9), AT));
    assert!(h.begin_image_publication("gen-2", manifest(), AT).is_err());
    // Consume every byte promised to this claim, including maximum retained
    // output replay, returned-file chunks and queued upload, then overwrite
    // every shared base block.
    fill(&root.join("account-toolchain"), TOOLCHAIN);
    h.image_account_ready("gen-1", &job(JOB).sub).unwrap();
    fill(&root.join("replay-and-upload"), JOB_BYTES);
    fill(&root.join("workspace"), SMALL_CAP);
    assert!(free(&root) >= threshold);
    assert_eq!(
        std::fs::read(root.join("base")).unwrap(),
        vec![1u8; SMALL_CAP as usize]
    );
}
