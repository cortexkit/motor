use super::{parse_list, CpuTopology, RESERVE_MIN_CORES};
use std::{fs, io, path::Path};

const JOB_SLICE: &str = "motor-jobs.slice";
const MOTOR_SLICE: &str = "motor.slice";

struct Limit {
    cpus: Vec<u32>,
    source: String,
}

pub(super) struct Effective(Vec<Limit>);

impl Effective {
    fn push(&mut self, list: &str, source: String) -> io::Result<()> {
        let cpus = parse_list(list).map_err(|error| {
            io::Error::other(format!("cannot read CPU set from {source}: {error}"))
        })?;
        self.0.push(Limit { cpus, source });
        Ok(())
    }

    pub(super) fn restrict(
        &self,
        mut topology: CpuTopology,
        explicit: Option<&[u32]>,
    ) -> io::Result<CpuTopology> {
        for Limit { cpus, source } in &self.0 {
            let contains = |cpu: &u32| cpus.binary_search(cpu).is_ok();
            if let Some(cpu) = explicit.into_iter().flatten().find(|cpu| !contains(cpu)) {
                return Err(io::Error::other(format!(
                    "--linux-cpus names CPU {cpu}, which is unavailable according to {source}"
                )));
            }
            for core in &mut topology.cores {
                core.retain(contains);
            }
            topology.cores.retain(|core| !core.is_empty());
            if topology.cores.is_empty() {
                return Err(io::Error::other(format!(
                    "no online CPUs remain for Linux jobs after applying {source}"
                )));
            }
        }
        topology.cores.sort_unstable();
        topology.reserved = usize::from(topology.cores.len() >= RESERVE_MIN_CORES);
        Ok(topology)
    }
}

pub(super) fn read(config: &crate::Config) -> io::Result<Effective> {
    if config.isolate_jobs {
        // PID 1 starts job units in its system slice, not in the daemon's
        // cgroup. The daemon's scheduler affinity may differ from theirs.
        return job_slice_cpus(Path::new("/sys/fs/cgroup"), |unit| {
            let output = crate::backend::systemctl(
                config,
                &["show", "--property=AllowedCPUs", "--value", unit],
            )?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "cannot query {unit} AllowedCPUs: {}",
                    String::from_utf8_lossy(&output.stderr).trim(),
                )));
            }
            String::from_utf8(output.stdout).map_err(io::Error::other)
        });
    }
    let source = "/proc/thread-self/status Cpus_allowed_list";
    let status = fs::read_to_string("/proc/thread-self/status")?;
    let list = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .ok_or_else(|| io::Error::other(format!("missing {source}")))?;
    let mut effective = Effective(Vec::new());
    effective.push(list, source.into())?;
    Ok(effective)
}

fn job_slice_cpus(
    root: &Path,
    mut allowed_cpus: impl FnMut(&str) -> io::Result<String>,
) -> io::Result<Effective> {
    let motor = root.join(MOTOR_SLICE);
    let jobs = motor.join(JOB_SLICE);
    let levels = [&jobs, &motor, root];
    let mut effective = Effective(Vec::new());
    for (depth, dir) in levels.into_iter().enumerate() {
        let mask = dir.join("cpuset.cpus.effective");
        match fs::read_to_string(&mask) {
            Ok(list) => {
                effective.push(&list, mask.display().to_string())?;
                // An inactive slice has no kernel mask yet. Its configured
                // restrictions still apply when systemd starts the job unit.
                for unit in [JOB_SLICE, MOTOR_SLICE][..depth].iter().rev() {
                    let list = allowed_cpus(unit)?;
                    if !list.trim().is_empty() {
                        // systemctl prints ranges separated by spaces, unlike sysfs.
                        let normalized = list.split_whitespace().collect::<Vec<_>>().join(",");
                        effective
                            .push(&normalized, format!("{unit} AllowedCPUs={}", list.trim()))?;
                    }
                }
                return Ok(effective);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && *dir != *root => (),
            Err(error) => {
                return Err(io::Error::other(format!(
                    "cannot read {}: {error}",
                    mask.display()
                )))
            }
        }
    }
    unreachable!("the cgroup root either has an effective CPU mask or reports an error")
}

#[cfg(test)]
mod tests;
