use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

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
            for name in ["lower", "upper", "work"] {
                fs::create_dir_all(layers.join(cache.name).join(name))?;
            }
            // The upper's root supplies the merged root's ownership. All other
            // entries retain their validated lower ownership until copy-up.
            let upper = layers.join(cache.name).join("upper");
            let metadata = fs::metadata(&upper)?;
            if metadata.uid() != cache.owner.uid || metadata.gid() != cache.owner.gid {
                checked(
                    crate::privilege::command("/usr/bin/chown")
                        .args(["--", &format!("{}:{}", cache.owner.uid, cache.owner.gid)])
                        .arg(&upper),
                )?;
            }
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
            let options = format!(
                "lowerdir={base}/lower,upperdir={base}/upper,workdir={base}/work,index=off,nosuid,nodev,X-mount.idmap=u:{}:{}:1 g:{}:{}:1",
                cache.owner.uid, self.job_uid, cache.owner.gid, self.job_gid,
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
                "cargo_overlay_mount_refused: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            )));
        }
        Ok(())
    }
}

pub(crate) fn start_validation() {
    if let Some(home) = host_inputs::environment("CARGO_HOME") {
        ownership::start(Path::new(&home));
    }
}
