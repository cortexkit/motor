use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "is_false")]
    pub hosted_runners: bool,
    /// Main-checkout repository identities whose ordinary jobs use hosted leases.
    /// Workspace paths are deliberately not matched: linked worktrees share the
    /// resolved repository_root of their main checkout. Windows and macOS stay direct.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosted_repositories: Vec<String>,
    /// Move only proven capacity-permit waits to hosted runners. Defaults are
    /// omitted so older clients with a closed Config schema can still load it.
    #[serde(skip_serializing_if = "is_false")]
    pub spill_to_hosted: bool,
    /// Compare sampled build-server jobs against hosted runners without routing
    /// caller traffic there. Omit defaults for older closed config schemas.
    #[serde(skip_serializing_if = "ShadowConfig::is_default")]
    pub shadow_to_hosted: ShadowConfig,
    /// Capacity-permit wait before attempting spill, in seconds (default 45).
    #[serde(skip_serializing_if = "is_default_spill_after")]
    pub spill_after_s: u64,
    #[serde(skip_serializing_if = "is_default_spill_max_vcpus")]
    pub spill_max_vcpus: u32,
    /// The build server's broker host id. A spilled lease asks the broker to
    /// avoid this host, so a job leaving the busy build server is never placed
    /// back on it. Unset sends no host to avoid. Omitted when unset so older
    /// clients with a closed Config schema can still load the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spill_avoid_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broker_url: Option<String>,
    #[serde(skip_serializing_if = "BrokerTokenSource::is_default")]
    pub broker_token_source: BrokerTokenSource,
    pub runner_instance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_runner_instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_runner_state_root: Option<String>,
    /// Ordered Windows endpoints. An empty list keeps the legacy instance pair.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub windows_runners: Vec<WindowsRunner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macos_runner: Option<MacosRunner>,
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
    /// of jobs their route started or whose secret they hold. Off by default;
    /// when off, ck-motor refuses direct binds on these routes.
    // Omitted from serialized configs at its default so
    // a config written by this version still loads in an older ck-motor, whose
    // Config rejects unknown fields. scripts/mixed-version.py checks that the
    // pinned production ck-motor can load a config written by this version.
    #[serde(skip_serializing_if = "is_false")]
    pub admit_direct_exec: bool,
    /// The longest timeout, in seconds, a direct `exec.run` may ask for. A
    /// disconnected caller may never return with its secret, so every direct
    /// job must end on its own: ck-motor refuses a direct
    /// run with no timeout or a longer one before anything is uploaded.
    #[serde(skip_serializing_if = "is_default_direct_timeout")]
    pub direct_exec_max_timeout_secs: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerTokenSource {
    #[default]
    CkAccount,
    Fake,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShadowConfig {
    pub enabled: bool,
    pub sample_rate: f64,
    pub max_concurrent: usize,
}
impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_rate: 0.1,
            max_concurrent: 2,
        }
    }
}
impl ShadowConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
impl BrokerTokenSource {
    fn is_default(&self) -> bool {
        *self == Self::CkAccount
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_spill_after(value: &u64) -> bool {
    *value == 45
}
fn is_default_spill_max_vcpus(value: &u32) -> bool {
    *value == 8
}

fn is_default_direct_timeout(value: &u64) -> bool {
    *value == DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS
}

/// The default for [`Config::direct_exec_max_timeout_secs`]: two hours.
pub const DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS: u64 = 7200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsRunner {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(skip_serializing_if = "is_ssh_port")]
    pub port: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub instance: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub state_root: String,
    #[serde(skip_serializing_if = "is_one")]
    pub max_concurrent: usize,
}

fn is_ssh_port(port: &u16) -> bool {
    *port == 22
}
fn is_one(count: &usize) -> bool {
    *count == 1
}
impl Default for WindowsRunner {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            host_key: String::new(),
            instance: String::new(),
            state_root: String::new(),
            max_concurrent: 1,
        }
    }
}
impl WindowsRunner {
    pub(crate) fn target(&self) -> RunnerTarget {
        RunnerTarget {
            instance: self.instance.clone(),
            state_root: Some(self.state_root.clone()),
            windows: Some(self.clone()),
            macos: None,
        }
    }
    fn validate(&self) -> io::Result<()> {
        let plain = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        if !plain(&self.name)
            || !plain(&self.host)
            || !plain(&self.user)
            || self.host.starts_with('-')
            || self.user.starts_with('-')
            || self.port == 0
            || self.instance.is_empty()
            || !self
                .instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || !self.state_root.starts_with('/')
            || !self
                .state_root
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            || self.max_concurrent == 0
            || self.max_concurrent > tokio::sync::Semaphore::MAX_PERMITS
            || self.host_key.contains(['\r', '\n'])
            || ssh_key::PublicKey::from_openssh(&self.host_key).is_err()
        {
            return Err(io::Error::other(
                "invalid Windows runner endpoint, instance, state root, host key or concurrency",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MacosRunner {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(skip_serializing_if = "is_ssh_port")]
    pub port: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub instance: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub state_root: String,
}
impl Default for MacosRunner {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            host_key: String::new(),
            instance: String::new(),
            state_root: String::new(),
        }
    }
}
impl MacosRunner {
    fn target(&self) -> RunnerTarget {
        RunnerTarget {
            instance: self.instance.clone(),
            state_root: Some(self.state_root.clone()),
            windows: None,
            macos: Some(self.clone()),
        }
    }
    fn validate(&self) -> io::Result<()> {
        if self.host.is_empty()
            || self.host.starts_with('-')
            || !self
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || self.port == 0
            || self.user != "motor"
            || self.instance.is_empty()
            || !self
                .instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || !self.state_root.starts_with('/')
            || !self
                .state_root
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            || self.host_key.contains(['\r', '\n'])
            || ssh_key::PublicKey::from_openssh(&self.host_key).is_err()
        {
            return Err(io::Error::other(
                "invalid macOS runner endpoint, motor user, instance, state root or host key",
            ));
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        Self {
            hosted_runners: false,
            hosted_repositories: Vec::new(),
            spill_to_hosted: false,
            shadow_to_hosted: ShadowConfig::default(),
            spill_after_s: 45,
            spill_max_vcpus: 8,
            spill_avoid_host: None,
            broker_url: None,
            broker_token_source: BrokerTokenSource::CkAccount,
            runner_instance: "main".into(),
            windows_runner_instance: None,
            windows_runner_state_root: None,
            windows_runners: Vec::new(),
            macos_runner: None,
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
        if let Some(runner) = &self.macos_runner {
            runner.validate()?;
            if runner.instance == self.runner_instance
                || Some(&runner.instance) == self.windows_runner_instance.as_ref()
                || self
                    .windows_runners
                    .iter()
                    .any(|windows| windows.instance == runner.instance)
            {
                return Err(io::Error::other(
                    "macos_runner instance must differ from Linux and Windows instances",
                ));
            }
        }
        let mut names = std::collections::HashSet::new();
        for runner in &self.windows_runners {
            runner.validate()?;
            if !names.insert(&runner.name) {
                return Err(io::Error::other("Windows runner names must be unique"));
            }
        }
        if !self.shadow_to_hosted.sample_rate.is_finite()
            || !(0.0..=1.0).contains(&self.shadow_to_hosted.sample_rate)
            || self.shadow_to_hosted.max_concurrent == 0
        {
            return Err(io::Error::other(
                "shadow sample_rate must be 0..1 and max_concurrent must be positive",
            ));
        }
        if self.runner_instance == "main" && self.broker_token_source == BrokerTokenSource::Fake {
            return Err(io::Error::other(
                "fake broker is forbidden for runner_instance main",
            ));
        }
        if let Some(url) = &self.broker_url {
            let url = reqwest::Url::parse(url).map_err(io::Error::other)?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (url.scheme() != "https"
                    && !(self.broker_token_source == BrokerTokenSource::Fake
                        && url.scheme() == "http"
                        && url.host_str().is_some_and(|h| {
                            h == "localhost"
                                || h.parse::<std::net::IpAddr>()
                                    .is_ok_and(|ip| ip.is_loopback())
                        })))
            {
                return Err(io::Error::other(
                    "broker_url requires HTTPS (loopback HTTP is allowed only for fake)",
                ));
            }
        }
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
        if self
            .hosted_repositories
            .iter()
            .any(|p| !Path::new(p).is_absolute())
        {
            return Err(io::Error::other(
                "hosted_repositories must be absolute repository roots",
            ));
        }
        if self
            .spill_avoid_host
            .as_deref()
            .is_some_and(|host| !motor_lease::valid_name(host))
        {
            return Err(io::Error::other(
                "spill_avoid_host must be 1-64 ASCII letters, digits, '.', '_' or '-'",
            ));
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
                windows: None,
                macos: None,
            }),
            Some(Platform::Windows) if !self.windows_runners.is_empty() => {
                Ok(self.windows_runners[0].target())
            }
            Some(Platform::Windows) => match (
                &self.windows_runner_instance,
                &self.windows_runner_state_root,
            ) {
                (Some(instance), Some(root)) => Ok(RunnerTarget {
                    instance: instance.clone(),
                    state_root: Some(root.clone()),
                    windows: None,
                    macos: None,
                }),
                _ => Err(io::Error::other(
                    "requested platform windows is not configured",
                )),
            },
            Some(Platform::Macos) => self
                .macos_runner
                .as_ref()
                .map(MacosRunner::target)
                .ok_or_else(|| io::Error::other("requested platform macos is not configured")),
            Some(platform) => Err(io::Error::other(format!(
                "requested platform {platform:?} is unsupported"
            ))),
        }
    }

    pub(crate) fn stored_target(&self, instance: Option<&str>) -> io::Result<RunnerTarget> {
        if let Some(runner) = &self.macos_runner {
            if instance == Some(runner.instance.as_str()) {
                return Ok(runner.target());
            }
        }
        if instance.is_none() || instance == Some(self.runner_instance.as_str()) {
            return self.target(None);
        }
        if instance == self.windows_runner_instance.as_deref() {
            return Ok(RunnerTarget {
                instance: instance.unwrap().into(),
                state_root: self.windows_runner_state_root.clone(),
                windows: None,
                macos: None,
            });
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
    pub windows: Option<WindowsRunner>,
    pub macos: Option<MacosRunner>,
}
