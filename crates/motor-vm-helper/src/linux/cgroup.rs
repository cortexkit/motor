use crate::{config::SlotConfig, Error, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const CONTROLLERS: &str = "+cpu +cpuset +memory +io";
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
    fs::write(path.join("cgroup.subtree_control"), CONTROLLERS)?;
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
