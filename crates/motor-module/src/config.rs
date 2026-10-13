use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub runner_instance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_runner_instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_runner_state_root: Option<String>,
    pub worktree_roots: Vec<PathBuf>,
    pub queue_wait_limit_s: u64,
    pub ssh_config_override: Option<PathBuf>,
    pub inject_tree_hash_mismatch: bool,
    pub inject_snapshot_failure: bool,
    /// Relay outbound-network requests only when explicitly enabled. Omit the
    /// default so older ck-motor versions that reject unknown keys can load it.
    #[serde(skip_serializing_if = "is_false")]
    pub network_routing: bool,
    /// Admit plain local clients of the subc daemon (`Principal::Direct`, which
    /// is any process running as this user, agent shells included) for their
    /// own jobs: `exec.run`, and `exec.attach`, `exec.cancel` and `exec.status`
    /// of jobs their route started. Off by default; when off, ck-motor refuses
    /// direct binds on these routes.
    // Omitted from serialized configs at its default so
    // a config written by this version still loads in an older ck-motor, whose
    // Config rejects unknown fields. scripts/mixed-version.py runs the pinned
    // production ck-motor against a config written by this version to prove it.
    #[serde(skip_serializing_if = "is_false")]
    pub admit_direct_exec: bool,
    /// The longest timeout, in seconds, a direct `exec.run` may ask for. A
    /// direct caller that loses its connection can't re-attach to or cancel its
    /// job, so every direct job must end on its own: ck-motor refuses a direct
    /// run with no timeout or a longer one before anything is uploaded.
    #[serde(skip_serializing_if = "is_default_direct_timeout")]
    pub direct_exec_max_timeout_secs: u64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_direct_timeout(value: &u64) -> bool {
    *value == DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS
}

/// The default for [`Config::direct_exec_max_timeout_secs`]: two hours.
pub const DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS: u64 = 7200;

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        Self {
            runner_instance: "main".into(),
            windows_runner_instance: None,
            windows_runner_state_root: None,
            worktree_roots: vec![data.join("cortexkit/alfonso/worktrees")],
            queue_wait_limit_s: 300,
            ssh_config_override: None,
            inject_tree_hash_mismatch: false,
            inject_snapshot_failure: false,
            network_routing: false,
            admit_direct_exec: false,
            direct_exec_max_timeout_secs: DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let json = subc_jsonc::jsonc_to_json(&text).map_err(io::Error::other)?;
        let value: serde_json::Value = serde_json::from_str(&json).map_err(io::Error::other)?;
        let config: Self = serde_json::from_value(value.clone()).map_err(io::Error::other)?;
        if config.runner_instance == "main"
            && [
                "ssh_config_override",
                "inject_tree_hash_mismatch",
                "inject_snapshot_failure",
            ]
            .iter()
            .any(|name| value.get(*name).is_some())
        {
            return Err(io::Error::other(
                "test control fields are forbidden for runner_instance main",
            ));
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.runner_instance.is_empty()
            || !self
                .runner_instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(io::Error::other(
                "runner_instance must be a plain instance name",
            ));
        }
        if self.runner_instance == "main"
            && (self.ssh_config_override.is_some()
                || self.inject_snapshot_failure
                || self.inject_tree_hash_mismatch)
        {
            return Err(io::Error::other(
                "test controls are forbidden for runner_instance main",
            ));
        }
        match (
            &self.windows_runner_instance,
            &self.windows_runner_state_root,
        ) {
            (None, None) => {}
            (Some(instance), Some(root)) => {
                if instance.is_empty()
                    || !instance
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(io::Error::other(
                        "windows_runner_instance must be a plain instance name",
                    ));
                }
                if instance == &self.runner_instance {
                    return Err(io::Error::other(
                        "windows_runner_instance must differ from runner_instance",
                    ));
                }
                if !root.starts_with('/')
                    || !root
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
                {
                    return Err(io::Error::other("windows_runner_state_root must be absolute and contain only ASCII alphanumerics, /, ., _, -"));
                }
            }
            _ => return Err(io::Error::other(
                "windows_runner_instance and windows_runner_state_root must be configured together",
            )),
        }
        if self.worktree_roots.iter().any(|p| !p.is_absolute()) {
            return Err(io::Error::other("worktree_roots must be absolute"));
        }
        Ok(())
    }

    pub(crate) fn target(
        &self,
        platform: Option<&motor_protocol::Platform>,
    ) -> io::Result<RunnerTarget> {
        use motor_protocol::Platform;
        match platform {
            None | Some(Platform::Linux) => Ok(RunnerTarget {
                instance: self.runner_instance.clone(),
                state_root: None,
            }),
            Some(Platform::Windows) => match (
                &self.windows_runner_instance,
                &self.windows_runner_state_root,
            ) {
                (Some(instance), Some(root)) => Ok(RunnerTarget {
                    instance: instance.clone(),
                    state_root: Some(root.clone()),
                }),
                _ => Err(io::Error::other(
                    "requested platform windows is not configured",
                )),
            },
            Some(platform) => Err(io::Error::other(format!(
                "requested platform {platform:?} is unsupported"
            ))),
        }
    }

    pub(crate) fn stored_target(&self, instance: Option<&str>) -> io::Result<RunnerTarget> {
        if instance.is_none() || instance == Some(self.runner_instance.as_str()) {
            return self.target(None);
        }
        if instance == self.windows_runner_instance.as_deref() {
            return self.target(Some(&motor_protocol::Platform::Windows));
        }
        Err(io::Error::other(format!(
            "stored runner instance {} is no longer configured",
            instance.unwrap()
        )))
    }
}

#[derive(Clone)]
pub(crate) struct RunnerTarget {
    pub instance: String,
    pub state_root: Option<String>,
}
