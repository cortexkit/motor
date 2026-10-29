use super::*;
use std::os::unix::fs::PermissionsExt;

#[path = "cargo_overlay_ownership.rs"]
pub(super) mod ownership;

pub(super) const LAYERS: &str = "/motor-cargo-overlay";

struct Cache {
    name: &'static str,
    source: PathBuf,
    owner: ownership::Owner,
}

pub(super) struct Plan {
    caches: Vec<Cache>,
    job_uid: u32,
    job_gid: u32,
}

impl Plan {
    pub(super) fn prepare(
        runtime: &Path,
        shared: &Path,
        cargo: &Path,
        read_only: &mut Vec<String>,
        read_write: &mut Vec<String>,
    ) -> io::Result<Self> {
        let mut caches = Vec::new();
        for name in ["registry", "git"] {
            let source = shared.join(name);
            if read_only.contains(&bind(&source, &cargo.join(name))?) {
                caches.push(Cache {
                    name,
                    owner: ownership::validated_owner(&source)?,
                    source,
                });
            }
        }
        let job_uid = account_uid(USER)?
            .ok_or_else(|| io::Error::other("cargo_overlay_account_missing: motor-job"))?;
        let group = crate::systemd::output(Command::new("/usr/bin/id").args(["-g", USER]))?;
        let job_gid = String::from_utf8_lossy(&group.stdout)
            .trim()
            .parse::<u32>()
            .map_err(|_| io::Error::other("cargo_overlay_account_missing: motor-job group"))?;
        let layers = runtime.join("cargo-overlay");
        fs::create_dir(&layers)?;
        fs::set_permissions(&layers, fs::Permissions::from_mode(0o700))?;
        for cache in &caches {
            for name in ["lower", "mapped-lower", "upper", "work"] {
                fs::create_dir_all(layers.join(cache.name).join(name))?;
            }
            // Only the lower bind is idmapped. The plain upper/work directories
            // use the job's real IDs, including metadata copied up by overlayfs.
            checked(
                crate::privilege::command("/usr/bin/chown")
                    .args(["--", &format!("{job_uid}:{job_gid}")])
                    .args([
                        layers.join(cache.name).join("upper"),
                        layers.join(cache.name).join("work"),
                    ]),
            )?;
            read_only.push(bind(
                &shared.join(cache.name),
                &Path::new(LAYERS).join(cache.name).join("lower"),
            )?);
        }
        // The backing directories are outside /motor-home. This root-only bind
        // lets the trusted namespace helper reach them, never job code.
        read_write.push(bind(&layers, Path::new(LAYERS))?);
        Ok(Self {
            caches,
            job_uid,
            job_gid,
        })
    }

    pub(super) fn setup(&self, id: Uuid) -> io::Result<Duration> {
        let started = Instant::now();
        for cache in &self.caches {
            if ownership::validated_owner(&cache.source)? != cache.owner {
                return Err(io::Error::other(
                    "cargo_overlay_lower_changed: owner changed before mount",
                ));
            }
        }
        let output = systemctl(&["show", "--property=MainPID", "--value", &unit(id)])?;
        let pid = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid != 0)
            .ok_or_else(|| {
                io::Error::other("cargo_overlay_mount_refused: job has no main process")
            })?;
        self.mount(pid, &["--make-rprivate".into(), "/".into()])?;
        for cache in &self.caches {
            let base = format!("{LAYERS}/{}", cache.name);
            let mapping =
                lower_mapping(cache.owner.uid, cache.owner.gid, self.job_uid, self.job_gid);
            self.mount(
                pid,
                &[
                    "--internal-only".into(),
                    "--no-mtab".into(),
                    "--bind".into(),
                    "--read-only".into(),
                    "-o".into(),
                    mapping,
                    format!("{base}/lower"),
                    format!("{base}/mapped-lower"),
                ],
            )?;
            let options = format!(
                "lowerdir={base}/mapped-lower,upperdir={base}/upper,workdir={base}/work,index=off,nosuid,nodev",
            );
            self.mount(
                pid,
                &[
                    "--internal-only".into(),
                    "--no-mtab".into(),
                    "-t".into(),
                    "overlay".into(),
                    "-o".into(),
                    options,
                    "overlay".into(),
                    format!("/motor-home/cargo/{}", cache.name),
                ],
            )?;
        }
        Ok(started.elapsed())
    }

    fn mount(&self, pid: u32, arguments: &[String]) -> io::Result<()> {
        // Enter PID as well as mount/root so mount(8)'s /proc/self operations
        // resolve in the job's private procfs. Only the external helper runs
        // the mount arguments; the waiting job process remains unprivileged.
        let output = crate::systemd::output(
            crate::privilege::command("/usr/bin/nsenter")
                .args([
                    "--target",
                    &pid.to_string(),
                    "--mount",
                    "--pid",
                    "--root",
                    "--wd=/",
                    "/usr/bin/mount",
                ])
                .args(arguments),
        )?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "cargo_overlay_mount_refused: kernel={}: {}: {}",
                fs::read_to_string("/proc/sys/kernel/osrelease")
                    .unwrap_or_else(|_| "unknown".into())
                    .trim(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            )));
        }
        Ok(())
    }
}

fn lower_mapping(source_uid: u32, source_gid: u32, job_uid: u32, job_gid: u32) -> String {
    // Keep every other ID representable: shared default ACLs also name the job
    // and writer accounts. An owner-only map can fail while copying those ACLs.
    let mut mappings = id_swap('u', source_uid, job_uid);
    mappings.extend(id_swap('g', source_gid, job_gid));
    format!("X-mount.idmap={}", mappings.join(" "))
}

fn id_swap(kind: char, source: u32, job: u32) -> Vec<String> {
    if source == job {
        return vec![format!("{kind}:0:0:{}", u32::MAX)];
    }
    // mount(8) takes visible mount ID first, backing filesystem ID second.
    let mut mappings = vec![
        format!("{kind}:{job}:{source}:1"),
        format!("{kind}:{source}:{job}:1"),
    ];
    let mut next = 0_u64;
    for excluded in [
        u64::from(source.min(job)),
        u64::from(source.max(job)),
        u64::from(u32::MAX),
    ] {
        if excluded > next {
            mappings.push(format!("{kind}:{next}:{next}:{}", excluded - next));
        }
        next = excluded + 1;
    }
    mappings
}

pub(crate) fn start_validation() {
    if let Some(home) = host_inputs::environment("CARGO_HOME") {
        ownership::start(Path::new(&home));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_idmap_presents_backing_owner_as_job_identity() {
        assert_eq!(
            lower_mapping(0, 0, 999, 999),
            "X-mount.idmap=u:999:0:1 u:0:999:1 u:1:1:998 u:1000:1000:4294966295 g:999:0:1 g:0:999:1 g:1:1:998 g:1000:1000:4294966295",
        );
        assert_eq!(id_swap('u', 44, 44), ["u:0:0:4294967295"]);
        assert_eq!(
            id_swap('u', 1000, 999),
            [
                "u:999:1000:1",
                "u:1000:999:1",
                "u:0:0:999",
                "u:1001:1001:4294966294",
            ]
        );
    }
}
