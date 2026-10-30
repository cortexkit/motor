use super::*;
use std::os::unix::fs::PermissionsExt;

#[path = "cargo_overlay_ownership.rs"]
pub(super) mod ownership;
#[path = "cargo_overlay_seed.rs"]
mod seed;
#[path = "cargo_overlay_fs.rs"]
mod tree_fs;

pub(super) const LAYERS: &str = "/motor-cargo-overlay";
const SETUP: &str = include_str!("cargo-overlay-setup.py");
struct Target {
    pid: u32,
    cgroup: String,
    unit: String,
}

struct Cache {
    name: &'static str,
    source: PathBuf,
    owner: ownership::Owner,
    validation_root: PathBuf,
    target: PathBuf,
}

pub(super) struct Plan {
    caches: Vec<Cache>,
    job_uid: u32,
    job_gid: u32,
    guard: (u64, u64, u32),
}

impl Plan {
    // Exercise the private cache seed without mounting it or looking up the job account.
    #[cfg(test)]
    pub(in crate::isolation) fn seed_for_test(
        shared: &Path,
        private: &Path,
        commits: &[String],
    ) -> io::Result<()> {
        seed::prepare(shared, private, commits)
    }

    pub(super) fn prepare(
        runtime: &Path,
        shared: &Path,
        cargo: &Path,
        read_only: &mut Vec<String>,
        read_write: &mut Vec<String>,
        commits: &[String],
    ) -> io::Result<Self> {
        let mut caches = Vec::new();
        let registry = shared.join("registry");
        let source = registry.join("cache");
        match ownership::validated_owner(&source) {
            Ok(owner) => caches.push(Cache {
                name: "archives",
                owner,
                validation_root: source.clone(),
                source,
                target: cargo.join("registry/cache"),
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
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
        let guard = tree_fs::Directory::open(&layers)?.stat()?;
        let private = layers.join("private");
        seed::prepare(shared, &private, commits)?;
        prepare_job_home(&private)?;
        // Cargo rewrites indexes, repairs source trees and mutates Git metadata.
        // None may be a live lower. Only archive bytes are append-only: trusted
        // fetch never rewrites a nonempty .crate, and automatic GC is disabled.
        for name in ["registry", "git"] {
            let old = bind(&shared.join(name), &cargo.join(name))?;
            read_only.retain(|entry| entry != &old);
        }
        for (name, target) in [
            ("index", "registry/index"),
            ("src", "registry/src"),
            ("git", "git"),
        ] {
            read_write.push(bind(&private.join(name), &cargo.join(target))?);
        }
        read_write.push(bind(&private.join("cache"), &cargo.join("registry/cache"))?);
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
                &cache.source,
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
            guard: (guard.st_dev, guard.st_ino, guard.st_uid),
        })
    }

    pub(super) fn setup(&self, id: Uuid) -> io::Result<Duration> {
        self.setup_target(Self::target(id)?)
    }

    fn target(id: Uuid) -> io::Result<Target> {
        let output = systemctl(&[
            "show",
            "--property=MainPID",
            "--property=ControlGroup",
            &unit(id),
        ])?;
        let text = String::from_utf8_lossy(&output.stdout);
        let value = |key: &str| text.lines().find_map(|line| line.strip_prefix(key));
        let pid = value("MainPID=")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid != 0)
            .ok_or_else(|| {
                io::Error::other("cargo_overlay_target_refused: unit has no main task")
            })?;
        let cgroup = value("ControlGroup=")
            .filter(|group| group.starts_with('/') && *group != "/")
            .ok_or_else(|| {
                io::Error::other("cargo_overlay_target_refused: unit has no control group")
            })?
            .to_owned();
        if !output.status.success() {
            return Err(io::Error::other(
                "cargo_overlay_target_refused: cannot inspect unit",
            ));
        }
        Ok(Target {
            pid,
            cgroup,
            unit: unit(id),
        })
    }

    #[cfg(test)]
    pub(in crate::isolation) fn setup_with_pid_for_test(
        &self,
        id: Uuid,
        pid: u32,
    ) -> io::Result<Duration> {
        let mut target = Self::target(id)?;
        target.pid = pid;
        self.setup_target(target)
    }

    fn setup_target(&self, target: Target) -> io::Result<Duration> {
        let started = Instant::now();
        let mut mounts = vec![vec!["--make-rprivate".to_owned(), "/".to_owned()]];
        for cache in &self.caches {
            ownership::validated_owner(&cache.validation_root)?;
            if ownership::Owner::observed(&cache.source)? != cache.owner {
                return Err(io::Error::other(
                    "cargo_overlay_lower_changed: archive root changed before mount",
                ));
            }
            let base = format!("{LAYERS}/{}", cache.name);
            mounts.push(vec![
                "--internal-only".into(),
                "--no-mtab".into(),
                "--bind".into(),
                "--read-only".into(),
                "-o".into(),
                lower_mapping(cache.owner.uid, cache.owner.gid, self.job_uid, self.job_gid),
                format!("{base}/lower"),
                format!("{base}/mapped-lower"),
            ]);
            mounts.push(vec![
                "--internal-only".into(), "--no-mtab".into(), "-t".into(), "overlay".into(), "-o".into(),
                format!("lowerdir={base}/mapped-lower,upperdir={base}/upper,workdir={base}/work,index=off,nosuid,nodev,metacopy=off,redirect_dir=nofollow"),
                "overlay".into(), cache.target.to_string_lossy().into_owned(),
            ]);
        }
        let plan = serde_json::json!({ "pid": target.pid, "unit": target.unit, "cgroup": target.cgroup, "guard": self.guard, "mounts": mounts });
        // Bootstrap in the trusted daemon namespace. The helper opens pidfd,
        // proc and namespace handles before entering the verified unit, then
        // uses only those pinned handles for the entire mount sequence.
        let output = crate::systemd::output(
            crate::privilege::command("/usr/bin/nsenter")
                .args([
                    "--mount=/proc/self/ns/mnt",
                    "--",
                    "/usr/bin/python3",
                    "-I",
                    "-c",
                    SETUP,
                ])
                .arg(plan.to_string()),
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
        Ok(started.elapsed())
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
        let created = tree_fs::Directory::open(Path::new(&home))
            .and_then(|home| home.ensure_child(std::ffi::OsStr::new("registry")))
            .and_then(|registry| registry.ensure_child(std::ffi::OsStr::new("cache")));
        if let Err(error) = created {
            tracing::warn!(%error, "cannot initialize archive lower root");
        }
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

    #[test]
    fn wrong_unit_cgroup_is_refused_before_any_mount() {
        let output = Command::new("/usr/bin/python3").args(["-I", "-c", SETUP])
            .arg(serde_json::json!({"pid": std::process::id(), "unit": "not-the-gated-unit.service", "cgroup": "/not-the-gated-unit", "guard": [0,0,0], "mounts": []}).to_string())
            .output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("cgroup does not match the gated unit"));
    }
}
