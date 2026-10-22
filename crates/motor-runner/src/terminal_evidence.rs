//! Bounded, best-effort evidence collected before an ended unit is reset.
use super::*;

#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutionEvidence {
    pub(crate) output_storage_full: bool,
    peak: Option<u64>,
    limit: Option<u64>,
    max_events: Option<u64>,
    oom: Option<u64>,
    oom_kill: Option<u64>,
    main_code: Option<i32>,
    main_status: Option<i32>,
    result: Option<String>,
    workspace: VolumeEvidence,
    state: VolumeEvidence,
}

#[derive(Clone, Debug, Default)]
struct VolumeEvidence {
    bytes: Option<u64>,
    inodes: Option<u64>,
}

impl VolumeEvidence {
    fn read(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
                return Self::default();
            };
            let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } == 0 {
                #[allow(clippy::useless_conversion)]
                return Self {
                    bytes: Some(
                        u64::from(stats.f_bavail).saturating_mul(u64::from(stats.f_frsize)),
                    ),
                    inodes: Some(u64::from(stats.f_favail)),
                };
            }
        }
        let _ = path;
        Self::default()
    }
}

fn value<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".into())
}

impl ExecutionEvidence {
    #[cfg(any(target_os = "linux", test))]
    fn unit(&mut self, text: &str, cgroups: &Path) {
        let property = |name: &str| text.lines().find_map(|line| line.strip_prefix(name));
        self.main_code = property("ExecMainCode=").and_then(|v| v.parse().ok());
        self.main_status = property("ExecMainStatus=").and_then(|v| v.parse().ok());
        self.result = property("Result=").map(str::to_owned);
        let peak = property("MemoryPeak=")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v != u64::MAX);
        self.peak = self.peak.into_iter().chain(peak).max();
        self.limit = property("MemoryMax=")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v != u64::MAX)
            .or(self.limit);
        if let Some(group) = property("ControlGroup=") {
            let relative = Path::new(group.trim_start_matches('/'));
            if relative.as_os_str().is_empty()
                || relative
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return;
            }
            let group = cgroups.join(relative);
            let number = |name| {
                fs::read_to_string(group.join(name))
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
            };
            self.peak = self.peak.into_iter().chain(number("memory.peak")).max();
            self.limit = number("memory.max").or(self.limit);
            if let Ok(events) = fs::read_to_string(group.join("memory.events")) {
                let count = |key| {
                    events
                        .lines()
                        .find_map(|line| line.strip_prefix(key))
                        .and_then(|v| v.trim().parse().ok())
                };
                self.max_events = count("max ").or(self.max_events);
                self.oom = count("oom ").or(self.oom);
                self.oom_kill = count("oom_kill ").or(self.oom_kill);
            }
        }
    }

    pub(crate) fn oom_killed(&self) -> bool {
        self.result.as_deref() == Some("oom-kill") || self.oom_kill.is_some_and(|n| n > 0)
    }

    pub(crate) fn disk_full(&self) -> bool {
        self.output_storage_full
            || [&self.workspace, &self.state]
                .iter()
                .any(|v| v.bytes == Some(0) || v.inodes == Some(0))
    }

    fn detail(&self) -> String {
        let hit = if self.max_events.is_some_and(|n| n > 0)
            || self
                .peak
                .zip(self.limit)
                .is_some_and(|(peak, limit)| peak >= limit)
        {
            Some(true)
        } else {
            self.max_events.map(|_| false)
        };
        format!("memory.peak={} memory.max={} memory.max_hit={} memory.events[last_available,max={},oom={},oom_kill={}] ExecMainCode={} ExecMainStatus={} workspace[free_bytes={},free_inodes={}] runner_state[free_bytes={},free_inodes={}]",
            value(self.peak), value(self.limit), value(hit), value(self.max_events), value(self.oom), value(self.oom_kill),
            value(self.main_code), value(self.main_status), value(self.workspace.bytes), value(self.workspace.inodes), value(self.state.bytes), value(self.state.inodes))
    }
}

impl Runner {
    pub(crate) fn execution_evidence(&self, job: &Job) -> ExecutionEvidence {
        let mut evidence = job.execution_evidence.clone().unwrap_or_default();
        #[cfg(any(target_os = "linux", test))]
        if !self.core.config.windows_instance()
            && (self.core.config.isolate_jobs || self.core.config.boat_profile)
        {
            if let Some(unit) = job.process.and_then(|p| p.unit) {
                #[cfg(test)]
                let configured = backend::memory_max(job.weight, self.core.config.memory_floor_gib);
                #[cfg(not(test))]
                let configured = backend::memory_max(job.weight);
                evidence.limit = evidence.limit.or(Some(
                    self.core.config.boat_memory_max_bytes.unwrap_or(configured),
                ));
                let unit = self.core.backend.unit_name(&self.core.config, unit);
                if let Ok(output) = backend::systemctl(&self.core.config, &[
                    "show", "--property=ControlGroup,MemoryPeak,MemoryMax,Result,ExecMainCode,ExecMainStatus", &unit,
                ]) {
                    if output.status.success() {
                        evidence.unit(&String::from_utf8_lossy(&output.stdout), Path::new("/sys/fs/cgroup"));
                    }
                }
            }
        }
        evidence.workspace = VolumeEvidence::read(Path::new(job.key()));
        evidence.state = VolumeEvidence::read(&self.core.trees.state);
        evidence
    }

    pub(crate) fn explained_terminal(
        &self,
        job: &Job,
        rule: &str,
        reason: &str,
        evidence: &ExecutionEvidence,
    ) -> String {
        let floor = self.core.config.free_space_floor_bytes;
        let mut withdrawal = String::new();
        if self.core.config.boat_profile {
            if evidence.state.bytes.is_some_and(|free| free < floor) {
                withdrawal =
                    format!(" withdrawal=disk_below_floor volume=runner_state floor_bytes={floor}");
            }
            // The endpoint and runner share this root-owned runtime directory.
            // Only retain a withdrawal observed during this job, not a previous lease.
            if let Ok(file) =
                crate::filesystem::read_nofollow(Path::new("/run/motor/endpoint-withdrawal.json"))
            {
                let mut bytes = Vec::new();
                let _ = file.take(4097).read_to_end(&mut bytes);
                if let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if record["at_ms"]
                        .as_u64()
                        .is_some_and(|at| at >= job.started_ms.unwrap_or(u64::MAX))
                    {
                        if let Some(rule) = record["rule"].as_str() {
                            withdrawal = format!(
                                " withdrawal={}",
                                rule.chars().take(160).collect::<String>()
                            );
                        }
                    }
                }
            }
        }
        let reason: String = reason.chars().take(512).collect();
        self.safe_refusal_detail(
            format!("rule={rule}; {}{withdrawal}; {reason}", evidence.detail()),
            job.request.iter().flat_map(|r| r.env.values().cloned()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_evidence_keeps_last_cgroup_counters_when_unit_vanishes() {
        let scratch = cortexkit_test_support::ScratchDir::new("terminal-evidence");
        let group = scratch.path().join("motor-job.service");
        fs::create_dir(&group).unwrap();
        for (name, contents) in [
            ("memory.peak", "12500000000"),
            ("memory.max", "12500000000"),
            (
                "memory.events",
                "low 0\nhigh 0\nmax 53\noom 2\noom_kill 1\noom_group_kill 0\n",
            ),
        ] {
            fs::write(group.join(name), contents).unwrap();
        }
        let mut evidence = ExecutionEvidence::default();
        evidence.unit(
            "ControlGroup=/motor-job.service\nExecMainCode=0\nExecMainStatus=0\n",
            scratch.path(),
        );
        fs::remove_dir_all(&group).unwrap();
        evidence.unit(
            "ControlGroup=\nMemoryPeak=0\nExecMainCode=2\nExecMainStatus=9\nResult=oom-kill\n",
            scratch.path(),
        );
        assert!(evidence.oom_killed());
        let detail = evidence.detail();
        assert!(detail.contains("memory.peak=12500000000"), "{detail}");
        assert!(detail.contains("memory.max_hit=true"), "{detail}");
        assert!(detail.contains("max=53,oom=2,oom_kill=1"), "{detail}");
        assert!(
            detail.contains("ExecMainCode=2 ExecMainStatus=9"),
            "{detail}"
        );
    }
}
