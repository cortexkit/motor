//! Cold boots use ordinary guest RAM, not the legacy userfaultfd snapshot pool.
use super::{cgroup, fs, process, tenant};
use crate::{
    config::{Config, JobVms},
    job::{self, ExitReport, Operation, Start, Workspace},
    Error, Peer, Reply, Response, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const JOB_OOM_SCORE: i32 = 1000;

#[derive(Debug, Clone, Deserialize, Serialize)]
// Rollbacks must be able to read records written by a newer helper.
struct Record {
    start: Start,
    uid: u32,
    disk_device: u64,
    disk_inode: u64,
    released: bool,
    oom_kill: Option<u64>,
    #[serde(default)]
    exit_acknowledged: bool,
}

impl Record {
    fn pending_exit(&self) -> Option<ExitReport> {
        self.oom_kill
            .filter(|_| !self.exit_acknowledged)
            .map(|oom_kill| ExitReport {
                job_id: self.start.job_id.clone(),
                oom_kill,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStep {
    Captured,
    Recorded,
    Removed,
    Delivery,
}

pub struct LinuxJobs {
    settings: Option<JobVms>,
    groups: PathBuf,
    records_path: PathBuf,
    jail: PathBuf,
    records: BTreeMap<String, Record>,
    children: BTreeMap<String, Child>,
    #[cfg(test)]
    exit_fault: Option<ExitStep>,
    #[cfg(test)]
    remove_test_group: Option<fn(&Path) -> std::io::Result<()>>,
}

impl LinuxJobs {
    pub fn new(config: &Config) -> Self {
        Self {
            settings: config.job_vms.clone(),
            groups: cgroup::JOB_SLICE.into(),
            records_path: job::RECORDS.into(),
            jail: Path::new(super::BUILD_ROOT).join("jail/jobs"),
            records: BTreeMap::new(),
            children: BTreeMap::new(),
            #[cfg(test)]
            exit_fault: None,
            #[cfg(test)]
            remove_test_group: None,
        }
    }

    fn group(&self, id: &str) -> PathBuf {
        self.groups.join(format!("job-{id}"))
    }
    fn boundary(&self, id: &str) -> PathBuf {
        self.jail.join(id)
    }
    fn root(&self, id: &str) -> PathBuf {
        self.boundary(id).join(format!("firecracker/job-{id}/root"))
    }
    fn save(&self, record: &Record) -> Result<()> {
        tenant::durable_write(
            &self
                .records_path
                .join(format!("{}.json", record.start.job_id)),
            &serde_json::to_vec(record).map_err(|e| Error(e.to_string()))?,
        )
    }

    pub fn reconcile(&mut self) -> Result<()> {
        if !self.records_path.try_exists()? {
            return Ok(());
        }
        fs::trusted(
            &self.records_path,
            &std::fs::symlink_metadata(&self.records_path)?,
        )?;
        for entry in std::fs::read_dir(&self.records_path)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let record: Record = serde_json::from_reader(fs::trusted_file(&path)?)
                .map_err(|e| Error(format!("invalid job record: {e}")))?;
            record.start.validate()?;
            if path.file_stem().and_then(|p| p.to_str()) != Some(&record.start.job_id) {
                return Err(Error("job record identity mismatch".into()));
            }
            let settings = self
                .settings
                .as_ref()
                .ok_or_else(|| Error("job VM settings required to adopt jobs".into()))?;
            if !(settings.uid_base..settings.uid_base + settings.uid_count).contains(&record.uid) {
                return Err(Error("job record uid outside configured range".into()));
            }
            self.adopt_record(record)?;
        }
        self.collect_exits()
    }

    fn adopt_record(&mut self, record: Record) -> Result<()> {
        // A trusted, fresh cgroup identifies the process even if the helper
        // died between spawning jailer and recording its pid. Never signal a
        // stale saved pid or move an unrelated process into this group.
        if !record.released && self.pid(&record)?.is_some() && record.oom_kill.is_some() {
            return Err(Error("exited job record has a running VMM".into()));
        }
        if record.exit_acknowledged && record.oom_kill.is_none() {
            return Err(Error("job acknowledgement has no exit evidence".into()));
        }
        self.records.insert(record.start.job_id.clone(), record);
        Ok(())
    }

    fn pid(&self, record: &Record) -> Result<Option<u32>> {
        if record.released {
            return Ok(None);
        }
        let path = self.group(&record.start.job_id);
        let text = match std::fs::read_to_string(path.join("cgroup.procs")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let pids: Vec<_> = text.split_whitespace().collect();
        if pids.len() > 1 {
            return Err(Error("unexpected processes in job VM cgroup".into()));
        }
        let Some(pid) = pids.first() else {
            return Ok(None);
        };
        let pid = pid
            .parse::<u32>()
            .map_err(|_| Error("invalid job VMM pid".into()))?;
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let comm = match std::fs::read_to_string(proc.join("comm")) {
            Ok(comm) => comm,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if comm.trim() != "firecracker" || effective_uid(pid)? != record.uid {
            return Err(Error("job cgroup process identity mismatch".into()));
        }
        Ok(Some(pid))
    }

    fn reap(&mut self) -> Result<()> {
        let mut ended = Vec::new();
        for (id, child) in &mut self.children {
            if child.try_wait()?.is_some() {
                ended.push(id.clone());
            }
        }
        for id in ended {
            self.children.remove(&id);
        }
        Ok(())
    }

    pub fn collect_exits(&mut self) -> Result<()> {
        self.reap()?;
        let ids: Vec<_> = self.records.keys().cloned().collect();
        let mut result = Ok(());
        for id in ids {
            if let Err(error) = self.collect_exit(&id) {
                eprintln!("job {id} exit evidence: {error}");
                result = Err(error);
            }
        }
        result
    }

    fn collect_exit(&mut self, id: &str) -> Result<()> {
        let group = self.group(id);
        if !group.try_exists()? {
            // A reboot can erase cgroups. No count is better than an invented 0.
            return Ok(());
        }
        if !std::fs::read_to_string(group.join("cgroup.procs"))?
            .trim()
            .is_empty()
        {
            return Ok(());
        }
        let mut record = self.records.get(id).unwrap().clone();
        if record.oom_kill.is_none() {
            record.oom_kill = Some(read_oom_kill(&group)?);
            self.exit_checkpoint(ExitStep::Captured)?;
            self.save(&record)?;
            self.records.insert(id.into(), record);
            self.exit_checkpoint(ExitStep::Recorded)?;
        }
        self.remove_empty_group(&group)?;
        self.exit_checkpoint(ExitStep::Removed)
    }

    fn remove_empty_group(&self, group: &Path) -> Result<()> {
        // Unit fixtures model cgroup control files with ordinary files. Only
        // deletion is replaced; emptiness and evidence checks run unchanged.
        #[cfg(test)]
        if let Some(remove) = self.remove_test_group {
            return Ok(remove(group)?);
        }
        Ok(std::fs::remove_dir(group)?)
    }

    fn exit_checkpoint(&mut self, step: ExitStep) -> Result<()> {
        #[cfg(test)]
        if self.exit_fault == Some(step) {
            self.exit_fault = None;
            return Err(Error(format!("injected exit fault at {step:?}")));
        }
        let _ = step;
        Ok(())
    }

    fn acknowledge_exit(&mut self, report: ExitReport) -> Result<Response> {
        let mut record = self
            .records
            .get(&report.job_id)
            .cloned()
            .ok_or_else(|| Error("unknown job VM".into()))?;
        if record.oom_kill != Some(report.oom_kill) {
            return Err(Error(
                "job exit count does not match durable evidence".into(),
            ));
        }
        if !record.exit_acknowledged {
            record.exit_acknowledged = true;
            self.save(&record)?;
            self.records.insert(report.job_id, record);
        }
        Ok(Response::new(Reply::Ok))
    }

    pub fn handle(&mut self, op: Operation, peer: Peer) -> Result<Response> {
        op.validate()?;
        // Invalid acknowledgements must not change any evidence or cgroups.
        if let Operation::AckExit(report) = op {
            return self.acknowledge_exit(report);
        }
        self.collect_exits()?;
        match op {
            Operation::Start(start) => self.start(start, peer),
            Operation::Adopt(id) => self.status(&id),
            Operation::Stop(id) => {
                self.stop(&id)?;
                self.status(&id)
            }
            Operation::Relay(id, port) => {
                let record = self
                    .records
                    .get(&id)
                    .ok_or_else(|| Error("unknown job VM".into()))?;
                if self.pid(record)?.is_none() {
                    return Err(Error("job VM is not running".into()));
                }
                Ok(Response {
                    reply: Reply::Relay { fd_count: 1 },
                    fds: vec![tenant::connect_relay(&self.root(&id), port)?],
                })
            }
            Operation::Exits => {
                let exits = self
                    .records
                    .values()
                    .filter_map(Record::pending_exit)
                    .collect();
                self.exit_checkpoint(ExitStep::Delivery)?;
                Ok(Response::new(Reply::JobExits { exits }))
            }
            Operation::AckExit(_) => unreachable!(),
        }
    }

    fn status(&self, id: &str) -> Result<Response> {
        let record = self
            .records
            .get(id)
            .ok_or_else(|| Error("unknown job VM".into()))?;
        let pid = self.pid(record)?;
        Ok(Response::new(Reply::Job {
            running: pid.is_some(),
            vmm_pid: pid,
            exit_report: record.pending_exit(),
        }))
    }

    fn disk_available(&self, start: &Start, device: u64, inode: u64) -> Result<()> {
        if self.records.values().any(|r| {
            !r.released
                && ((r.start.account == start.account
                    && r.start.workspace_disk_id == start.workspace_disk_id)
                    || (r.disk_device == device && r.disk_inode == inode))
        }) {
            return Err(Error(
                "workspace disk already attached or awaiting cleanup".into(),
            ));
        }
        Ok(())
    }

    fn allocate_uid(&self, settings: &JobVms) -> Result<u32> {
        // Uids are separate from tenant VMMs, legacy slots, and host accounts.
        // Never reuse one while its previous jail still owns disk resources.
        for uid in settings.uid_base..settings.uid_base + settings.uid_count {
            if self.records.values().any(|r| !r.released && r.uid == uid) {
                continue;
            }
            process::refuse_passwd_entries(uid, 1)?;
            let mut occupied = false;
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().parse::<u32>().is_ok() {
                    match effective_uid(entry.file_name().to_string_lossy().parse().unwrap()) {
                        Ok(actual) => occupied |= actual == uid,
                        Err(_) if !entry.path().exists() => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            if !occupied {
                return Ok(uid);
            }
        }
        Err(Error("job VMM uid capacity exhausted".into()))
    }

    fn start(&mut self, start: Start, peer: Peer) -> Result<Response> {
        start.validate()?;
        let settings = self
            .settings
            .clone()
            .ok_or_else(|| Error("job VM settings are required".into()))?;
        if self.records.contains_key(&start.job_id) {
            return Err(Error("job id already booted; use adopt_job".into()));
        }
        // Keep a start for an already attached identity out of all filesystem
        // preparation, including opening a disk owned by its running VMM.
        self.disk_available(&start, 0, 0)?;
        if self.group(&start.job_id).try_exists()? || self.boundary(&start.job_id).try_exists()? {
            return Err(Error(
                "stale job cgroup or jail requires reconciliation".into(),
            ));
        }
        let workspace = Path::new(job::WORKSPACES).join(&start.workspace_disk_id);
        let registry: Workspace =
            serde_json::from_reader(fs::trusted_file(&workspace.join("registry.json"))?)
                .map_err(|e| Error(format!("invalid workspace registry: {e}")))?;
        if registry.account != start.account
            || registry.workspace_disk_id != start.workspace_disk_id
        {
            return Err(Error("workspace disk identity or account mismatch".into()));
        }
        let disk = fs::trusted_file(&workspace.join("workspace.img"))?;
        let meta = disk.metadata()?;
        self.disk_available(&start, meta.dev(), meta.ino())?;
        if unsafe { libc::flock(disk.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error("workspace disk already attached".into()));
        }
        let image = Path::new(job::IMAGES).join(&start.image_generation_id);
        let inputs = [
            (fs::trusted_file(&image.join("kernel"))?, "kernel", true),
            (
                fs::trusted_file(&image.join("rootfs.ext4"))?,
                "rootfs.ext4",
                true,
            ),
            (
                fs::trusted_file(
                    &image
                        .join("accounts")
                        .join(&start.account)
                        .join("toolchain.ext4"),
                )?,
                "toolchain.ext4",
                true,
            ),
        ];
        for (file, _, _) in &inputs {
            if file.metadata()?.mode() & 0o004 == 0 {
                return Err(Error("job images must be readable by jailed VMMs".into()));
            }
        }
        let binaries = Path::new("/usr/local/lib/firecracker/1.17.0");
        for name in ["jailer", "firecracker"] {
            fs::trusted_file(&binaries.join(name))?;
            let output = Command::new(binaries.join(name))
                .arg("--version")
                .output()?;
            if !output.status.success()
                || !String::from_utf8_lossy(&output.stdout).contains("1.17.0")
            {
                return Err(Error(
                    "job VMs require Firecracker and jailer 1.17.0".into(),
                ));
            }
        }
        let uid = self.allocate_uid(&settings)?;
        let host_score = host_oom_score(peer.uid)?;
        let helper_score = read_oom_score(std::process::id())?;
        check_oom_order(host_score, helper_score)?;
        cgroup::prepare_job_slice(settings.memory_max_bytes)?;
        for path in [&self.records_path, &self.jail] {
            trusted_directory(path)?;
        }
        let record = Record {
            start: start.clone(),
            uid,
            disk_device: meta.dev(),
            disk_inode: meta.ino(),
            released: false,
            oom_kill: None,
            exit_acknowledged: false,
        };
        // The intent is durable before any jail or VMM can open the disk. A
        // crash in setup keeps it reserved until explicit stop has cleaned up.
        self.save(&record)?;
        self.records.insert(start.job_id.clone(), record);
        let id = &start.job_id;
        let boundary = self.boundary(id);
        let root = self.root(id);
        let group = self.group(id);
        let launch = (|| -> Result<()> {
            cgroup::create_job(&group, start.vcpus)?;
            super::create_boundary(&boundary)?;
            std::fs::create_dir_all(&root)?;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
            fs::chown(&File::open(&root)?, uid)?;
            for (file, name, readonly) in &inputs {
                tenant::bind(file, &root.join(name), *readonly)?;
            }
            fs::chown(&disk, uid)?;
            tenant::bind(&disk, &root.join("workspace.img"), false)?;
            let procs = OpenOptions::new()
                .write(true)
                .open(group.join("cgroup.procs"))?;
            let mut command = Command::new(binaries.join("jailer"));
            command
                .args(["--id", &format!("job-{id}"), "--exec-file"])
                .arg(binaries.join("firecracker"))
                .args([
                    "--uid",
                    &uid.to_string(),
                    "--gid",
                    &uid.to_string(),
                    "--chroot-base-dir",
                ])
                .arg(&boundary)
                .args(["--", "--api-sock", "/api.sock"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .env_clear();
            unsafe {
                command.pre_exec(move || super::raw_place(procs.as_raw_fd()));
            }
            let child = command.spawn()?;
            let pid = child.id();
            self.children.insert(id.clone(), child);
            // A /proc/fd alias avoids Unix-domain socket path length limits.
            let root_fd = File::open(&root)?;
            let short_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
            drop(super::wait_socket(
                &short_root.join("api.sock"),
                Duration::from_secs(10),
            )?);
            set_job_oom_score(pid, host_score, helper_score)?;
            for (path, body) in boot_requests(&start) {
                tenant::api(&short_root, "PUT", path, &body)?;
            }
            Ok(())
        })();
        if let Err(error) = launch {
            if let Err(cleanup) = self.stop(id) {
                return Err(Error(format!("{error}; job cleanup failed: {cleanup}")));
            }
            return Err(error);
        }
        self.status(id)
    }

    fn stop(&mut self, id: &str) -> Result<()> {
        let record = self
            .records
            .get(id)
            .cloned()
            .ok_or_else(|| Error("unknown job VM".into()))?;
        if record.released {
            return Ok(());
        }
        let group = self.group(id);
        if group.try_exists()? {
            std::fs::write(group.join("cgroup.kill"), "1")?;
            if let Some(mut child) = self.children.remove(id) {
                child.wait()?;
            }
            tenant::wait_empty(&group)?;
        }
        self.collect_exit(id)?;
        let mut record = self.records.get(id).unwrap().clone();
        tenant::cleanup_boundary(&self.boundary(id))?;
        let disk_path = Path::new(job::WORKSPACES)
            .join(&record.start.workspace_disk_id)
            .join("workspace.img");
        let disk = open_owned_disk(&disk_path, &record)?;
        disk.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        fs::chown(&disk, 0)?;
        record.released = true;
        self.save(&record)?;
        self.records.insert(id.into(), record);
        Ok(())
    }
}

fn trusted_directory(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if ancestor.try_exists()? {
            fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
        }
    }
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    for ancestor in path.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
    }
    Ok(())
}

fn open_owned_disk(path: &Path, record: &Record) -> Result<File> {
    for ancestor in path.parent().unwrap().ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
    }
    let disk = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = disk.metadata()?;
    if !meta.is_file()
        || ![0, record.uid].contains(&meta.uid())
        || (meta.dev(), meta.ino()) != (record.disk_device, record.disk_inode)
    {
        return Err(Error("job workspace inode or owner changed".into()));
    }
    Ok(disk)
}

fn read_oom_kill(group: &Path) -> Result<u64> {
    std::fs::read_to_string(group.join("memory.events"))?
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill "))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Error("missing final job oom_kill count".into()))
}

fn read_oom_score(pid: u32) -> Result<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))?
        .trim()
        .parse()
        .map_err(|_| Error("invalid oom_score_adj".into()))
}

fn effective_uid(pid: u32) -> Result<u32> {
    // /proc/<pid>'s inode can be root-owned for a non-dumpable process even
    // when its effective uid is unprivileged. Read credentials, not ownership.
    std::fs::read_to_string(format!("/proc/{pid}/status"))?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| Error("missing process credentials".into()))
}

fn check_oom_order(host_score: i32, helper_score: i32) -> Result<()> {
    if JOB_OOM_SCORE <= host_score || JOB_OOM_SCORE <= helper_score {
        return Err(Error(
            "job VMM OOM score must exceed host and helper".into(),
        ));
    }
    Ok(())
}

fn host_oom_score(uid: u32) -> Result<i32> {
    let mut highest = -1000;
    // SO_PEERCRED authenticates the host uid. Checking all its processes also
    // covers a host in the middle of restarting, without trusting a caller pid.
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        match effective_uid(pid) {
            Ok(actual) if actual == uid => match read_oom_score(pid) {
                Ok(score) => highest = highest.max(score),
                Err(_) if !entry.path().exists() => {}
                Err(e) => return Err(e),
            },
            Ok(_) => {}
            Err(_) if !entry.path().exists() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(highest)
}

fn set_job_oom_score(pid: u32, host_score: i32, helper_score: i32) -> Result<()> {
    check_oom_order(host_score, helper_score)?;
    if std::fs::read_to_string(format!("/proc/{pid}/comm"))?.trim() != "firecracker" {
        return Err(Error("expected job firecracker process".into()));
    }
    std::fs::write(
        format!("/proc/{pid}/oom_score_adj"),
        JOB_OOM_SCORE.to_string(),
    )?;
    if read_oom_score(pid)? != JOB_OOM_SCORE {
        return Err(Error("job VMM OOM score readback mismatch".into()));
    }
    Ok(())
}

/// Cold-boot API with no snapshot load, userfaultfd, or hugepages. Only the root,
/// toolchain and workspace images are attached; caller source bundles travel
/// over the existing runner channel rather than on a fourth drive.
fn boot_requests(start: &Start) -> Vec<(&'static str, Value)> {
    vec![
        (
            "/machine-config",
            json!({"vcpu_count":start.vcpus,"mem_size_mib":start.memory_mib,"smt":false}),
        ),
        (
            "/boot-source",
            json!({"kernel_image_path":"/kernel","boot_args":"console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro"}),
        ),
        (
            "/drives/root",
            json!({"drive_id":"root","path_on_host":"/rootfs.ext4","is_root_device":true,"is_read_only":true}),
        ),
        (
            "/pmem/toolchain",
            json!({"id":"toolchain","path_on_host":"/toolchain.ext4","read_only":true}),
        ),
        (
            "/drives/workspace",
            json!({"drive_id":"workspace","path_on_host":"/workspace.img","is_root_device":false,"is_read_only":false}),
        ),
        ("/vsock", json!({"guest_cid":3,"uds_path":"/vsock.sock"})),
        (
            "/balloon",
            json!({"amount_mib":0,"deflate_on_oom":true,"free_page_reporting":true,"stats_polling_interval_s":0}),
        ),
        ("/actions", json!({"action_type":"InstanceStart"})),
    ]
}

#[cfg(test)]
mod tests;
