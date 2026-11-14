use crate::{config::SlotConfig, Error, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const CONTROLLERS: &str = "+cpu +cpuset +memory +io";
pub const JOB_SLICE: &str = "/sys/fs/cgroup/motorvm.slice";

/// A separate systemd slice is not collected when the helper service stops.
/// Its aggregate ceiling protects host services; individual VM leaves stay
/// unbounded so accounting for one VM cannot reclaim another VM's pmem pages.
pub fn prepare_job_slice(memory_max_bytes: u64) -> Result<()> {
    #[cfg(test)]
    if let Some(result) = TEST_JOB_SLICE.with(|fixture| {
        fixture
            .borrow()
            .as_ref()
            .map(|(path, systemctl)| prepare_job_slice_at(path, memory_max_bytes, systemctl))
    }) {
        return result;
    }
    prepare_job_slice_at(Path::new(JOB_SLICE), memory_max_bytes, |args| {
        let output = std::process::Command::new("systemctl")
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(Error(format!(
                "systemctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    })
}

fn prepare_job_slice_at(
    path: &Path,
    memory_max_bytes: u64,
    systemctl: impl Fn(&[String]) -> Result<()>,
) -> Result<()> {
    prepare_job_slice_inner(path, memory_max_bytes, systemctl).map_err(|error| {
        Error(format!(
            "job slice {} setup failed: {error}",
            path.display()
        ))
    })
}

fn prepare_job_slice_inner(
    path: &Path,
    memory_max_bytes: u64,
    systemctl: impl Fn(&[String]) -> Result<()>,
) -> Result<()> {
    if memory_max_bytes == 0 {
        return Err(Error("job slice memory ceiling is required".into()));
    }
    let ceiling = memory_max_bytes.to_string();
    let configured = match fs::symlink_metadata(path) {
        Ok(meta) => {
            super::fs::trusted(path, &meta)?;
            fs::read_to_string(path.join("memory.max"))?.trim() == ceiling
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    // The runtime property disappears on reboot. Repair it before admission,
    // without rewriting an already configured slice on every readiness probe.
    if !configured {
        systemctl(&["start".into(), "motorvm.slice".into()])?;
        systemctl(&[
            "set-property".into(),
            "--runtime".into(),
            "motorvm.slice".into(),
            format!("MemoryMax={memory_max_bytes}"),
        ])?;
    }
    super::fs::trusted(path, &fs::symlink_metadata(path)?)?;
    let actual = fs::read_to_string(path.join("memory.max"))?;
    if actual.trim() != ceiling {
        return Err(Error(format!(
            "job slice memory ceiling readback mismatch: expected {ceiling}, got {}",
            actual.trim()
        )));
    }
    enable(path)
}

#[cfg(test)]
type TestJobSlice = (PathBuf, Box<dyn Fn(&[String]) -> Result<()>>);

#[cfg(test)]
thread_local! {
    static TEST_JOB_SLICE: std::cell::RefCell<Option<TestJobSlice>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_job_slice<T>(
    path: PathBuf,
    systemctl: impl Fn(&[String]) -> Result<()> + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_JOB_SLICE.with(|fixture| *fixture.borrow_mut() = None);
        }
    }
    TEST_JOB_SLICE.with(|fixture| {
        assert!(fixture.borrow().is_none());
        *fixture.borrow_mut() = Some((path, Box::new(systemctl)));
    });
    let _reset = Reset;
    run()
}

pub fn create_job(path: &Path, vcpus: u32) -> Result<()> {
    // create_dir, not create_dir_all: old OOM evidence is never a new boot.
    fs::create_dir(path)?;
    for (name, value) in job_limits(vcpus) {
        fs::write(path.join(name), value)?;
    }
    Ok(())
}

fn job_limits(vcpus: u32) -> [(&'static str, String); 1] {
    [("cpu.max", format!("{} 100000", u64::from(vcpus) * 100000))]
}
pub struct Cgroups {
    pub root: PathBuf,
}
impl Cgroups {
    pub fn discover() -> Result<Self> {
        let text = fs::read_to_string("/proc/self/cgroup")?;
        let relative = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| Error("cgroup v2 is required".into()))?;
        let leaf = Path::new(relative);
        if leaf.file_name().and_then(|n| n.to_str()) != Some("helper") || relative.contains("..") {
            return Err(Error("helper must run in DelegateSubgroup=helper".into()));
        }
        let root = Path::new("/sys/fs/cgroup").join(
            leaf.parent()
                .ok_or_else(|| Error("missing delegation root".into()))?
                .strip_prefix("/")
                .map_err(|_| Error("invalid cgroup path".into()))?,
        );
        Ok(Self { root })
    }
    pub fn enable(&self) -> Result<()> {
        enable(&self.root)?;
        let slots = self.root.join("slots");
        match fs::create_dir(&slots) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        enable(&slots)
    }
    pub fn slot(&self, n: usize) -> PathBuf {
        self.root.join(format!("slots/slot-{n}"))
    }
    pub fn create(&self, n: usize, config: &SlotConfig) -> Result<()> {
        let path = self.slot(n);
        fs::create_dir(&path)?;
        // No controller is enabled in a leaf that holds loader/Firecracker.
        for (name, value) in [
            ("cpuset.mems", config.mems.clone()),
            ("cpuset.cpus", config.cpus.clone()),
            ("cpu.max", config.cpu_max.clone()),
            ("memory.max", config.memory_max.to_string()),
            ("io.weight", config.io_weight.to_string()),
        ] {
            fs::write(path.join(name), value)?;
        }
        if let Some(value) = &config.io_max {
            fs::write(path.join("io.max"), value)?;
        }
        Ok(())
    }
    pub fn kill(&self, n: usize) -> Result<()> {
        let path = self.slot(n);
        if !path.try_exists()? {
            return Ok(());
        }
        fs::write(path.join("cgroup.kill"), "1")?;
        Ok(())
    }
    pub fn remove(&self, n: usize) -> Result<()> {
        remove(&self.slot(n))
    }
}
pub fn enable(path: &Path) -> Result<()> {
    if !fs::read_to_string(path.join("cgroup.procs"))?
        .trim()
        .is_empty()
    {
        return Err(Error(format!(
            "cannot enable controllers: {} holds a process",
            path.display()
        )));
    }
    let available = fs::read_to_string(path.join("cgroup.controllers"))?;
    for controller in ["cpu", "cpuset", "memory", "io"] {
        if !available.split_whitespace().any(|c| c == controller) {
            return Err(Error(format!("missing delegated controller {controller}")));
        }
    }
    let enabled = fs::read_to_string(path.join("cgroup.subtree_control"))?;
    if !["cpu", "cpuset", "memory", "io"]
        .iter()
        .all(|controller| enabled.split_whitespace().any(|c| c == *controller))
    {
        fs::write(path.join("cgroup.subtree_control"), CONTROLLERS)?;
    }
    Ok(())
}
fn remove(path: &Path) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove(&entry.path())?;
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn already_enabled_controllers_are_not_rewritten() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-slice-idempotent");
        fs::write(scratch.join("cgroup.procs"), "").unwrap();
        fs::write(scratch.join("cgroup.controllers"), "cpu cpuset memory io").unwrap();
        fs::write(
            scratch.join("cgroup.subtree_control"),
            "cpu cpuset memory io\n",
        )
        .unwrap();
        enable(&scratch).unwrap();
        enable(&scratch).unwrap();
        assert_eq!(
            fs::read_to_string(scratch.join("cgroup.subtree_control")).unwrap(),
            "cpu cpuset memory io\n"
        );
    }

    #[test]
    fn job_leaf_is_new_and_does_not_set_memory_limits() {
        let path =
            cortexkit_test_support::scratch_root().join(format!("job-leaf-{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        create_job(&path, 7).unwrap();
        assert_eq!(
            fs::read_to_string(path.join("cpu.max")).unwrap(),
            "700000 100000"
        );
        assert!(!path.join("memory.max").exists());
        assert!(!path.join("memory.high").exists());
        assert!(create_job(&path, 2).is_err());
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn populated_delegation_root_is_refused_before_controller_write() {
        let path = cortexkit_test_support::scratch_root()
            .join(format!("vm-cgroup-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("cgroup.procs"), "123\n").unwrap();
        fs::write(path.join("cgroup.subtree_control"), "unchanged").unwrap();
        assert!(enable(&path).unwrap_err().0.contains("holds a process"));
        assert_eq!(
            fs::read_to_string(path.join("cgroup.subtree_control")).unwrap(),
            "unchanged"
        );
        fs::remove_dir_all(path).unwrap();
    }
}
