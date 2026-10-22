//! Helper units: transient systemd services in which the trusted daemon runs
//! the steps of sibling snapshot mode that must see a sibling's snapshot at
//! the sibling's canonical path P instead of whatever checkout is on the host
//! at P. These are the Cargo prefetch of a job's workspace, which resolves
//! `../<sibling>` path dependencies, and the JavaScript install of a new
//! snapshot, whose local dependencies may name P absolutely.
//!
//! Each helper runs as the daemon's own account in a private mount namespace.
//! The parent directory of every mounted path is replaced by an empty
//! read-only tmpfs, and only the declared paths are bound into it, so the
//! helper sees the same tree at P that a job will see and cannot read the host
//! checkout at P at all. Directories the helper itself needs (the runner
//! state, Cargo and rustup homes, the executables on PATH) stay visible at
//! their own paths.
//!
//! Each helper unit has its own time limit (`RuntimeMaxSec`), and stopping it
//! kills its whole cgroup, ending with SIGKILL for processes that ignore
//! SIGTERM. Before it returns, setup waits for the unit to become inactive,
//! whether the helper succeeded, failed or ran out of time, and fails if it
//! cannot confirm that state.
//!
//! Units are named `motor-snaphelper-<instance>-<uuid>.service`, outside the
//! `motor-job-*` pattern that the runner lists as job units. Instance names
//! may themselves contain `-`, so startup stops a unit only when the part
//! after this instance's prefix is exactly a UUID: the runner `main` never
//! stops a helper of the runner `main-2`.

use crate::engine::Config;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Replaces the `systemd-run` invocation of a helper in tests. It receives
/// the arguments that follow `systemd-run`.
#[cfg(test)]
pub(crate) type Launcher = std::sync::Arc<dyn Fn(&[String]) -> io::Result<Output> + Send + Sync>;

pub(crate) const UNIT_PREFIX: &str = "motor-snaphelper-";

/// Directory under the instance state that holds each running helper's
/// environment file.
const ENVIRONMENT_DIRECTORY: &str = "snapshot-helpers";

/// How long systemd waits after SIGTERM before it sends SIGKILL to whatever
/// is left in a stopping helper's cgroup.
const STOP_TIMEOUT: &str = "2s";

/// How much longer than a helper's own limit the daemon waits for
/// `systemd-run` to return before it stops the unit itself. systemd enforces
/// the limit; this only covers a `systemd-run` that never returns.
#[cfg(target_os = "linux")]
const LAUNCH_GRACE: Duration = Duration::from_secs(30);

/// How long setup waits for a stopped helper unit to become inactive.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Captured diagnostics are cut to this many bytes.
#[cfg(target_os = "linux")]
const DIAGNOSTIC_BYTES: usize = 16 * 1024;

/// What a helper unit runs. Each kind has its own configured time limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Helper {
    CargoPrefetch,
    JavascriptInstall,
}

impl Helper {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::CargoPrefetch => "Cargo prefetch",
            Self::JavascriptInstall => "JavaScript install",
        }
    }

    /// The runner option that sets this helper's limit.
    pub(crate) fn setting(self) -> &'static str {
        match self {
            Self::CargoPrefetch => "--snapshot-prefetch-timeout-s",
            Self::JavascriptInstall => "--snapshot-javascript-timeout-s",
        }
    }

    /// The configured limit for one run of this helper.
    pub(crate) fn limit(self, config: &Config) -> Duration {
        match self {
            Self::CargoPrefetch => config.snapshot_prefetch_timeout,
            Self::JavascriptInstall => config.snapshot_javascript_timeout,
        }
    }
}

/// The mount namespace of one helper. Every bind destination's parent
/// directory is hidden behind an empty read-only tmpfs, so the helper sees
/// only the bound paths there.
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    read_only: Vec<(PathBuf, PathBuf)>,
    read_write: Vec<(PathBuf, PathBuf)>,
    /// Paths that stay visible at their own location even when a hidden
    /// directory covers them, with whether they stay writable.
    kept: Vec<(PathBuf, bool)>,
}

impl View {
    /// The view of the Cargo prefetch for workspace W: W itself, and each
    /// sibling's snapshot S at the sibling's canonical path P and, when W's
    /// parent is not P's parent, at the alias `<parent of W>/<name of P>`
    /// where a job sees it too. All read-only.
    pub(crate) fn prefetch(workspace: &Path, siblings: &[(PathBuf, PathBuf)]) -> io::Result<Self> {
        let parent = workspace
            .parent()
            .ok_or_else(|| io::Error::other("workspace has no parent directory"))?;
        let mut view = Self::default().read_only(workspace, workspace);
        for (snapshot, path) in siblings {
            view = view.read_only(snapshot, path);
            let alias = parent.join(
                path.file_name()
                    .ok_or_else(|| io::Error::other("sibling path has no name"))?,
            );
            if &alias != path {
                view = view.read_only(snapshot, &alias);
            }
        }
        Ok(view)
    }

    /// The view of the JavaScript install of a snapshot being built at
    /// `snapshot`: the snapshot at the sibling's canonical path, writable so
    /// the install can create `node_modules`.
    pub(crate) fn javascript(snapshot: &Path, path: &Path) -> Self {
        Self::default().read_write(snapshot, path)
    }

    pub(crate) fn read_only(mut self, source: &Path, destination: &Path) -> Self {
        self.read_only
            .push((source.to_path_buf(), destination.to_path_buf()));
        self
    }

    pub(crate) fn read_write(mut self, source: &Path, destination: &Path) -> Self {
        self.read_write
            .push((source.to_path_buf(), destination.to_path_buf()));
        self
    }

    /// Keeps `path` visible at its own location if a hidden directory covers
    /// it.
    pub(crate) fn keep(mut self, path: &Path, writable: bool) -> Self {
        self.kept.push((path.to_path_buf(), writable));
        self
    }

    /// The parent directories replaced by an empty tmpfs. A directory inside
    /// another hidden one is already hidden by it.
    fn hidden(&self) -> BTreeSet<PathBuf> {
        let parents: BTreeSet<PathBuf> = self
            .read_only
            .iter()
            .chain(&self.read_write)
            .filter_map(|(_, destination)| destination.parent().map(Path::to_path_buf))
            .filter(|parent| parent.parent().is_some())
            .collect();
        parents
            .iter()
            .filter(|parent| {
                !parents
                    .iter()
                    .any(|other| other != *parent && parent.starts_with(other))
            })
            .cloned()
            .collect()
    }

    fn bound(&self, path: &Path) -> bool {
        self.read_only
            .iter()
            .chain(&self.read_write)
            .any(|(_, destination)| path.starts_with(destination))
    }

    /// Maps a host path inside a bind source to the path the helper sees.
    fn inside(&self, path: &Path) -> PathBuf {
        for (source, destination) in self.read_write.iter().chain(&self.read_only) {
            if let Ok(rest) = path.strip_prefix(source) {
                return if rest.as_os_str().is_empty() {
                    destination.clone()
                } else {
                    destination.join(rest)
                };
            }
        }
        path.to_path_buf()
    }

    /// The `systemd-run` properties that build this view.
    fn properties(&self) -> io::Result<Vec<String>> {
        let hidden = self.hidden();
        let mut read_only = Vec::new();
        let mut read_write = Vec::new();
        for (source, destination) in &self.read_only {
            read_only.push(bind(source, destination)?);
        }
        for (source, destination) in &self.read_write {
            read_write.push(bind(source, destination)?);
        }
        let mut kept = BTreeSet::new();
        for (path, writable) in &self.kept {
            if hidden.iter().any(|directory| path.starts_with(directory))
                && !self.bound(path)
                && kept.insert(path.clone())
            {
                // A path that does not exist cannot be bound; it was not
                // visible to the command on the host either.
                if !path.exists() {
                    continue;
                }
                let list = if *writable {
                    &mut read_write
                } else {
                    &mut read_only
                };
                list.push(bind(path, path)?);
            }
        }
        let mut properties = Vec::new();
        for directory in &hidden {
            properties.push(format!(
                "--property=TemporaryFileSystem={}:ro",
                word(directory)?
            ));
        }
        if !read_only.is_empty() {
            properties.push(format!(
                "--property=BindReadOnlyPaths={}",
                read_only.join(" ")
            ));
        }
        if !read_write.is_empty() {
            properties.push(format!("--property=BindPaths={}", read_write.join(" ")));
        }
        Ok(properties)
    }
}

/// `motor-snaphelper-<instance>-<id>.service`.
pub(crate) fn unit_name(instance: &str, id: Uuid) -> String {
    format!("{UNIT_PREFIX}{instance}-{id}.service")
}

/// Whether `unit` is a helper unit of runner `instance`: the prefix, the
/// instance, `-`, and a remainder that is exactly a UUID in the form the
/// runner writes. A helper of instance `main-2` is not one of `main`'s,
/// because `2-<uuid>` is not a UUID.
pub(crate) fn belongs_to(instance: &str, unit: &str) -> bool {
    let Some(id) = unit
        .strip_prefix(UNIT_PREFIX)
        .and_then(|rest| rest.strip_prefix(instance))
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".service"))
    else {
        return false;
    };
    Uuid::try_parse(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

/// Runs `command` in a helper unit with the mount namespace `view`, and
/// returns once the unit is no longer active. Fails when the command fails,
/// when the unit exceeds `limit`, and when the unit cannot be confirmed
/// inactive; a time-out names the helper and its limit.
///
/// The unit gets the environment the command would get as a child of the
/// daemon: the daemon's environment with the command's own changes applied.
/// It is passed through a file readable only by the daemon, never on the
/// command line or as unit properties, which any local account can read. A
/// command that cleared its environment with `env_clear` is not supported.
pub(crate) fn run(
    config: &Config,
    helper: Helper,
    limit: Duration,
    command: &Command,
    view: &View,
) -> io::Result<()> {
    let id = Uuid::now_v7();
    let unit = unit_name(&config.instance, id);
    let environment = environment_path(config, id);
    let arguments = write_environment(&environment, command)
        .and_then(|()| arguments(config, &unit, limit, command, view, &environment));
    let started = Instant::now();
    let launched = arguments.and_then(|arguments| {
        crate::setup_log::phase(helper.label(), || launch(config, &arguments, limit))
    });
    // Whatever happened above, the unit must be inactive before setup
    // continues and releases anything the helper was using.
    let ended = settle(config, &unit);
    if let Err(error) = fs::remove_file(&environment) {
        if error.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %environment.display(), %error, "cannot remove snapshot helper environment");
        }
    }
    let ended = ended.map_err(|error| {
        io::Error::other(format!(
            "{} helper unit {unit} could not be confirmed stopped: {error}",
            helper.label()
        ))
    })?;
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{} helper unit {unit} exceeded its time limit of {} s ({}); systemd stopped it \
                 and every process it started",
                helper.label(),
                limit.as_secs_f64(),
                helper.setting()
            ),
        )
    };
    // systemd also reports Result=timeout when processes left behind by a
    // main process that exited ignore SIGTERM and need SIGKILL. Only a unit
    // that ran for its whole limit ran out of time.
    let ran_out = ended.result.as_deref() == Some("timeout") && started.elapsed() >= limit;
    match launched {
        _ if ran_out => Err(timed_out()),
        Ok(Launched::DeadlineExpired) => Err(timed_out()),
        Ok(Launched::Exited(output)) if output.status.success() => Ok(()),
        Ok(Launched::Exited(_)) if ended.main_succeeded => {
            tracing::warn!(
                unit,
                result = ended.result.as_deref().unwrap_or_default(),
                "snapshot helper succeeded, but systemd had to kill processes it left behind"
            );
            Ok(())
        }
        Ok(Launched::Exited(output)) => Err(io::Error::other(format!(
            "{} helper unit {unit} failed ({}{}): {}",
            helper.label(),
            output.status,
            ended
                .result
                .as_deref()
                .map(|result| format!(", result {result}"))
                .unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr)
        ))),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{} helper unit {unit}: {error}", helper.label()),
        )),
    }
}

/// Stops every helper unit this runner instance left behind, for example
/// when it was killed during a setup, and removes their environment files.
/// Units of other instances on the same host are left alone, even when their
/// instance name starts with this one's.
pub(crate) fn stop_stale(config: &Config) -> io::Result<()> {
    let pattern = format!("{UNIT_PREFIX}{}-*", config.instance);
    let output = crate::backend::systemctl(
        config,
        &[
            "list-units",
            "--all",
            "--type=service",
            "--no-legend",
            "--plain",
            "--no-pager",
            &pattern,
        ],
    )?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "list snapshot helper units: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    for unit in listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| belongs_to(&config.instance, unit))
    {
        tracing::warn!(unit, "stopping a snapshot helper left by an earlier run");
        settle(config, unit)
            .map_err(|error| io::Error::other(format!("stop stale helper {unit}: {error}")))?;
    }
    let directory = config.state_dir().join(ENVIRONMENT_DIRECTORY);
    match fs::read_dir(&directory) {
        Ok(entries) => {
            for entry in entries {
                fs::remove_file(entry?.path())?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn environment_path(config: &Config, id: Uuid) -> PathBuf {
    config
        .state_dir()
        .join(ENVIRONMENT_DIRECTORY)
        .join(format!("{id}.env"))
}

/// The environment a child of the daemon running `command` would get.
fn environment(command: &Command) -> BTreeMap<OsString, OsString> {
    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => environment.insert(name.to_os_string(), value.to_os_string()),
            None => environment.remove(name),
        };
    }
    environment
}

/// Writes the command's environment as a systemd environment file, private
/// to the daemon. Each value is double-quoted, which is literal in such a
/// file except for the four characters escaped here. Variables whose name
/// or value systemd cannot carry are left out, as systemd would drop them.
fn write_environment(path: &Path, command: &Command) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("helper environment has no directory"))?;
    fs::create_dir_all(directory)?;
    let mut text = String::new();
    for (name, value) in environment(command) {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        let valid_name = name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !valid_name || value.contains('\0') {
            continue;
        }
        text.push_str(name);
        text.push_str("=\"");
        for character in value.chars() {
            if matches!(character, '"' | '\\' | '`' | '$') {
                text.push('\\');
            }
            text.push(character);
        }
        text.push_str("\"\n");
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// Resolves the command's program the way a child of the daemon would find
/// it, so the unit runs the same executable.
fn program(command: &Command) -> io::Result<PathBuf> {
    let program = Path::new(command.get_program());
    if program.is_absolute() {
        return Ok(program.to_path_buf());
    }
    if program.components().count() > 1 {
        return Err(io::Error::other(format!(
            "helper program {} is a relative path",
            program.display()
        )));
    }
    let path = environment(command)
        .remove(OsStr::new("PATH"))
        .unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(program))
        .find(|candidate| {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(candidate)
                .is_ok_and(|found| found.is_file() && found.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} not found on PATH", program.display()),
            )
        })
}

/// The arguments after `systemd-run` that run `command` as unit `unit`.
fn arguments(
    config: &Config,
    unit: &str,
    limit: Duration,
    command: &Command,
    view: &View,
    environment_file: &Path,
) -> io::Result<Vec<String>> {
    let program = program(command)?;
    let directory = command
        .get_current_dir()
        .ok_or_else(|| io::Error::other("helper command has no working directory"))?;
    let path = environment(command)
        .remove(OsStr::new("PATH"))
        .unwrap_or_default();
    // The program, the directories on PATH, the runner state (which holds
    // the helpers' caches and the Cargo prefetch's working directory) and
    // the working directory must stay reachable inside the view.
    let mut view = view.clone().keep(&config.state_dir(), true);
    if let Some(parent) = program.parent() {
        view = view.keep(parent, false);
    }
    for directory in std::env::split_paths(&path).filter(|d| d.is_absolute()) {
        view = view.keep(&directory, false);
    }
    view = view.keep(directory, true);
    let mut arguments = vec![
        "--wait".to_string(),
        "--pipe".into(),
        "--quiet".into(),
        "--service-type=exec".into(),
        // Arguments are passed as they are; nothing in them is a variable.
        "--expand-environment=no".into(),
        format!("--unit={unit}"),
        // The daemon's own account. SAFETY: geteuid and getegid have no
        // preconditions and cannot fail.
        format!("--uid={}", unsafe { libc::geteuid() }),
        format!("--gid={}", unsafe { libc::getegid() }),
        format!(
            "--property=WorkingDirectory={}",
            word(&view.inside(directory))?
        ),
        format!("--property=EnvironmentFile={}", word(environment_file)?),
        format!("--property=RuntimeMaxSec={}ms", limit.as_millis().max(1)),
        "--property=KillMode=control-group".into(),
        "--property=SendSIGKILL=yes".into(),
        format!("--property=TimeoutStopSec={STOP_TIMEOUT}"),
        "--property=UMask=0022".into(),
    ];
    arguments.extend(view.properties()?);
    arguments.push("--".into());
    arguments.push(
        program
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF-8 helper program"))?
            .to_string(),
    );
    for argument in command.get_args() {
        arguments.push(
            argument
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF-8 helper argument"))?
                .to_string(),
        );
    }
    Ok(arguments)
}

enum Launched {
    Exited(Output),
    /// `systemd-run` had not returned well after the unit's own limit.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    DeadlineExpired,
}

fn launch(config: &Config, arguments: &[String], limit: Duration) -> io::Result<Launched> {
    #[cfg(test)]
    if let Some(launcher) = &config.snapshot_helper_launcher {
        return launcher(arguments).map(Launched::Exited);
    }
    let _ = config;
    #[cfg(target_os = "linux")]
    {
        use std::{io::Read, process::Stdio};
        let mut child = crate::privilege::command("/usr/bin/systemd-run")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0; 8192];
            while let Ok(n) = stderr.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let room = DIAGNOSTIC_BYTES.saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..n.min(room)]);
            }
            let _ = sender.send(captured);
        });
        let deadline = Instant::now() + limit + LAUNCH_GRACE;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Launched::DeadlineExpired);
            }
            std::thread::park_timeout(Duration::from_millis(20));
        };
        // The unit has ended, so nothing holds the pipe open for long.
        let stderr = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        Ok(Launched::Exited(Output {
            status,
            stdout: Vec::new(),
            stderr,
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (arguments, limit);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "snapshot helper units need an isolated Linux runner",
        ))
    }
}

/// How a helper unit ended, as far as systemd still knew when it was
/// settled. A unit that ended cleanly may already be unloaded.
#[derive(Default)]
struct Ended {
    /// systemd's `Result` for the unit.
    result: Option<String>,
    /// Whether the unit's main process exited with status 0.
    main_succeeded: bool,
}

/// Waits until `unit` is inactive, stopping it and then killing its whole
/// cgroup if it is still running, and unloads it if it failed.
fn settle(config: &Config, unit: &str) -> io::Result<Ended> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    let mut stopping = false;
    loop {
        let output = crate::backend::systemctl(
            config,
            &[
                "show",
                "--property=LoadState",
                "--property=ActiveState",
                "--property=Result",
                "--property=ExecMainCode",
                "--property=ExecMainStatus",
                unit,
            ],
        )?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "inspect {unit}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let property = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(str::to_string)
        };
        let load = property("LoadState");
        let active = property("ActiveState");
        if load.as_deref() == Some("not-found") {
            return Ok(Ended::default());
        }
        // ExecMainCode holds waitid's CLD_* code: CLD_EXITED for an exit.
        let ended = Ended {
            result: property("Result"),
            main_succeeded: property("ExecMainCode") == Some(libc::CLD_EXITED.to_string())
                && property("ExecMainStatus").as_deref() == Some("0"),
        };
        match active.as_deref() {
            Some("inactive") => return Ok(ended),
            Some("failed") => {
                let reset = crate::backend::systemctl(config, &["reset-failed", unit])?;
                if !reset.status.success()
                    && !String::from_utf8_lossy(&reset.stderr).contains("not loaded")
                {
                    tracing::warn!(unit, stderr = %String::from_utf8_lossy(&reset.stderr), "cannot unload failed snapshot helper");
                }
                return Ok(ended);
            }
            Some(_) => {}
            None => return Err(io::Error::other(format!("inspect {unit}: no ActiveState"))),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{unit} is still {} after {SETTLE_TIMEOUT:?}",
                    active.unwrap_or_default()
                ),
            ));
        }
        if !stopping {
            stopping = true;
            for args in [
                ["stop", "--no-block", unit].as_slice(),
                ["kill", "--kill-whom=all", "--signal=KILL", unit].as_slice(),
            ] {
                let output = crate::backend::systemctl(config, args)?;
                if !output.status.success() {
                    tracing::warn!(unit, stderr = %String::from_utf8_lossy(&output.stderr), "cannot stop snapshot helper");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A path as one word of a systemd path list: separators, whitespace,
/// quotes and specifiers are escaped, so a repository path cannot add or
/// change mounts.
fn word(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF-8 mount path"))?;
    if !Path::new(path).is_absolute() {
        return Err(io::Error::other(format!("non-absolute mount path {path}")));
    }
    let mut result = String::new();
    for byte in path.bytes() {
        match byte {
            b'%' => result.push_str("%%"),
            b'/' | b'.' | b'_' | b'-' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                result.push(byte as char)
            }
            _ => result.push_str(&format!("\\x{byte:02x}")),
        }
    }
    Ok(result)
}

fn bind(source: &Path, destination: &Path) -> io::Result<String> {
    Ok(format!("{}:{}", word(source)?, word(destination)?))
}

#[cfg(test)]
#[path = "helper_tests.rs"]
mod tests;
