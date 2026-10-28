//! Helper units: transient systemd services in which the trusted daemon runs
//! the steps of sibling snapshot mode that must see a sibling's snapshot at
//! the sibling's canonical path P instead of whatever checkout is on the host
//! at P. These are the Cargo prefetch of a job's workspace, which resolves
//! `../<sibling>` path dependencies, and the JavaScript install of a new
//! snapshot, whose local dependencies may name P absolutely.
//!
//! Each helper runs with the runner's own identity: the unit's user and group
//! are the daemon's effective uid and gid (`--uid`/`--gid`), the identity
//! with which the in-place Cargo prefetch and JavaScript install run today,
//! as plain children of the daemon. systemd gives the unit that user's
//! supplementary groups from the account database. The mount namespace is
//! the helper's own: the parent directory of every mounted path is replaced
//! by an empty read-only tmpfs, and only the declared paths are bound into it, so
//! the helper sees the same tree at P that a job will see and cannot read
//! the host checkout at P at all. Directories the helper itself needs (the
//! runner state, Cargo and rustup homes, the executables on PATH) stay
//! visible at their own paths.
//!
//! A helper succeeds only when systemd reports that the unit's main process
//! exited with status 0 and the unit's result is `success`. The result alone
//! is not enough: systemd counts a main process killed by SIGTERM during a
//! stop as a clean end, so a helper stopped from outside, for example by
//! the startup cleanup of a restarted runner, would otherwise pass as
//! successful.
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
/// environment file and the file its output goes to.
const ENVIRONMENT_DIRECTORY: &str = "snapshot-helpers";

/// How long systemd waits after SIGTERM before it sends SIGKILL to whatever
/// is left in a stopping helper's cgroup.
const STOP_TIMEOUT: &str = "2s";

/// How much longer than a helper's own limit the daemon waits for its main
/// process to end before it stops the unit itself. systemd enforces the
/// limit; this only covers a unit whose limit somehow did not fire.
const END_GRACE: Duration = Duration::from_secs(30);

/// How long `systemd-run` may take to start a helper unit.
#[cfg(target_os = "linux")]
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long after asking systemd to stop a helper the runner sends SIGKILL
/// to its cgroup itself, if the unit is still not inactive. systemd itself
/// escalates to SIGKILL after `STOP_TIMEOUT`.
const KILL_AFTER: Duration = Duration::from_secs(4);

/// How long setup waits for a stopped helper unit to become inactive.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Captured diagnostics are cut to this many bytes.
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
        Self::prefetch_tree(workspace, workspace, siblings)
    }

    /// [`Self::prefetch`] for a workspace stored at host path `source` and
    /// seen by its jobs at `workspace`: the view binds `source` there, and
    /// sibling aliases sit beside `workspace`, as in the job's own unit.
    pub(crate) fn prefetch_tree(
        source: &Path,
        workspace: &Path,
        siblings: &[(PathBuf, PathBuf)],
    ) -> io::Result<Self> {
        let parent = workspace
            .parent()
            .ok_or_else(|| io::Error::other("workspace has no parent directory"))?;
        let mut view = Self::default().read_only(source, workspace);
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

    fn prepare_mount_points(&self, config: &Config) -> io::Result<()> {
        for directory in self.hidden() {
            // systemd sets up mounts as root, before switching to the helper's
            // user. Create the tmpfs parents as the runner so systemd cannot
            // leave root-owned directories in the caller tree. Bind destinations
            // beneath these parents are created inside the private tmpfs only.
            let mapped = config.caller_roots.iter().any(|root| {
                let caller = Path::new(&root.caller_prefix);
                directory.starts_with(caller) || caller.starts_with(&directory)
            });
            let prepared = if mapped {
                // A mapped caller spelling is not its host storage path. Never
                // create directories there; only mount over an existing parent.
                fs::metadata(&directory).and_then(|metadata| {
                    if metadata.is_dir() {
                        Ok(())
                    } else {
                        Err(io::Error::other("mount parent is not a directory"))
                    }
                })
            } else {
                fs::create_dir_all(&directory)
            };
            prepared.map_err(|error| {
                crate::workspace_access::Unreadable::wrap(
                    &directory,
                    "prepare snapshot helper mount parent",
                    error,
                )
            })?;
        }
        Ok(())
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

    /// The `systemd-run` properties that build this view. `tenant_resolver`
    /// says whether the tenant attachment installed the daemon's private
    /// resolver, as [`TenantResolver`] works out.
    fn properties(&self, tenant_resolver: &TenantResolver) -> io::Result<Vec<String>> {
        let hidden = self.hidden();
        let mut read_only = Vec::new();
        let mut read_write = Vec::new();
        if let Some(resolver) = tenant_resolver.resolver()? {
            // systemd starts helpers outside the daemon's private mount
            // namespace, where the tenant attachment binds the daemon's
            // resolver over /etc/resolv.conf. The VM's own resolver is empty,
            // so a trusted preparation unit needs the same bind to resolve.
            // Only /etc/resolv.conf is bound: the rest of /etc must not be
            // hidden as if it were a checkout.
            read_only.push(bind(&resolver, Path::new("/etc/resolv.conf"))?);
        }
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

/// Where a helper unit of this runner gets its resolver.
///
/// A tenant VM's own `/etc/resolv.conf` is empty: attachment keeps it empty and
/// the daemon runs with the attachment's resolver bound over it. Helpers are
/// separate units outside that mount namespace, so they need the bind too, from
/// the private file attachment writes. Boat runs as `--tenant-vm` without a
/// tenant attachment (it is a single-tenant sandbox, not a tenant VM), so its
/// helpers keep the sandbox's own resolver and must not be given a bind for a
/// file nobody installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TenantResolver {
    /// The host's own name service: a profile that is not a tenant VM, or
    /// Boat, whose sandbox supplies a resolver and which no attachment serves.
    Host,
    /// A tenant attachment has installed the private resolver in `directory`.
    Attached { directory: PathBuf },
    /// A tenant VM whose attachment has not (yet) installed the resolver in
    /// `directory`: the runner started while the tenant was attached but
    /// before the file was written, or the file has been removed since. A
    /// private resolver is expected here, so leaving the empty VM resolver in
    /// place would only surface later as a fetch that cannot resolve.
    Missing { directory: PathBuf },
}

impl TenantResolver {
    pub(crate) fn new(config: &Config) -> Self {
        Self::under(
            config.tenant_vm,
            config.boat_profile,
            Path::new(crate::tenant::DNS_MARKER),
            Path::new(crate::tenant::DNS_DIRECTORY),
        )
    }

    /// [`Self::new`], reading the attachment's marker from `marker` and the
    /// private resolver from `directory`, so tests need not touch `/run`.
    ///
    /// A runner that is not a tenant VM, and Boat, which runs as one without a
    /// tenant attachment, keep the host's name service. Any other tenant VM is
    /// [`Self::Attached`] only when the marker holds a generation, and is
    /// [`Self::Missing`] when the marker is empty or unreadable.
    fn under(tenant_vm: bool, boat_profile: bool, marker: &Path, directory: &Path) -> Self {
        if !tenant_vm || boat_profile {
            return Self::Host;
        }
        match fs::read_to_string(marker) {
            Ok(generation) if !generation.trim().is_empty() => Self::Attached {
                directory: directory.to_path_buf(),
            },
            Ok(_) | Err(_) => Self::Missing {
                directory: directory.to_path_buf(),
            },
        }
    }

    /// The private resolver a helper must bind, when it needs one. Whether the
    /// tenant has one is decided by the attachment's marker, never by whether
    /// this file exists, so a missing file here is an error rather than "no
    /// tenant".
    fn resolver(&self) -> io::Result<Option<PathBuf>> {
        match self {
            Self::Host => Ok(None),
            Self::Attached { directory } => {
                let resolver = directory.join("resolv.conf");
                match fs::symlink_metadata(&resolver) {
                    Ok(metadata) if metadata.is_file() => Ok(Some(resolver)),
                    _ => Err(missing(directory)),
                }
            }
            Self::Missing { directory } => Err(missing(directory)),
        }
    }
}

/// The error for a tenant whose private resolver the attachment does not
/// provide, naming the directory and file a helper would need.
fn missing(directory: &Path) -> io::Error {
    io::Error::other(format!(
        "the tenant's private resolver is missing: a --tenant-vm runner needs {}/resolv.conf, \
         which tenant attachment installs; the VM's own resolver is empty, so dependency \
         preparation cannot resolve online until the tenant is attached again",
        directory.display()
    ))
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
    helper_id(instance, unit).is_some()
}

/// The UUID in the name of a helper unit of runner `instance`, as
/// `belongs_to` reads it.
fn helper_id(instance: &str, unit: &str) -> Option<Uuid> {
    let id = unit
        .strip_prefix(UNIT_PREFIX)
        .and_then(|rest| rest.strip_prefix(instance))
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".service"))?;
    Uuid::try_parse(id)
        .ok()
        .filter(|parsed| parsed.hyphenated().to_string() == id)
}

/// Runs `command` in a helper unit with the mount namespace `view`, and
/// returns once the unit is no longer active. Succeeds only when systemd
/// reports that the unit's main process exited with status 0 and the unit's
/// result is `success`. A main process killed by a signal, including the
/// SIGTERM of a stop from outside the helper (which systemd counts as a clean
/// end of the unit), fails the helper. So does a unit that ran past `limit`,
/// with an error naming the helper and its limit, and a unit that cannot be
/// confirmed inactive.
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
    let files = Files::new(config, id);
    let started = Instant::now();
    let observed = files
        .create(command)
        .and_then(|()| {
            arguments(
                config,
                &unit,
                limit,
                command,
                view,
                &files,
                TenantResolver::new(config),
            )
        })
        .and_then(|arguments| {
            view.prepare_mount_points(config)?;
            crate::setup_log::phase(helper.label(), || {
                start(config, &arguments)?;
                observe(config, &unit, limit + END_GRACE)
            })
        });
    // Whatever happened above, the unit must be inactive before setup
    // continues and releases anything the helper was using.
    let settled = settle(config, &unit);
    let output = files.output();
    files.remove();
    let settled = settled.map_err(|error| {
        io::Error::other(format!(
            "{} helper unit {unit} could not be confirmed stopped: {error}",
            helper.label()
        ))
    })?;
    let observed = observed.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{} helper unit {unit}: {error}{output}", helper.label()),
        )
    })?;
    match classify(&observed, started.elapsed(), limit) {
        Verdict::Succeeded => {
            if settled
                .result
                .as_deref()
                .is_some_and(|result| result != "success")
            {
                tracing::warn!(
                    unit,
                    result = settled.result.as_deref().unwrap_or_default(),
                    "snapshot helper succeeded, but systemd had to kill processes it left behind"
                );
            }
            Ok(())
        }
        Verdict::TimedOut => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{} helper unit {unit} exceeded its time limit of {} s ({}); systemd stopped it \
                 and every process it started{output}",
                helper.label(),
                limit.as_secs_f64(),
                helper.setting()
            ),
        )),
        Verdict::Failed(reason) => Err(io::Error::other(format!(
            "{} helper unit {unit} failed: {reason}{output}",
            helper.label()
        ))),
    }
}

/// Stops every helper unit this runner left behind, for example when it was
/// killed during a setup, and removes their files. Units of other instances
/// on the same host are left alone, even when their instance name starts
/// with this one's. So are units of another runner that uses the same
/// instance name with a different state directory, as test fixtures do: a
/// helper's files are written to the state directory before its unit
/// starts and removed only once it has stopped, so a unit this runner
/// started always has them there, and the daemon lock on that directory
/// means no other live runner uses it.
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
    for (unit, id) in listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|unit| helper_id(&config.instance, unit).map(|id| (unit, id)))
    {
        if !Files::new(config, id).exist() {
            tracing::info!(
                unit,
                "leaving a snapshot helper of another runner's state directory alone"
            );
            continue;
        }
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

/// A helper's private files under `<state>/snapshot-helpers`: its
/// environment, and the file systemd appends its output to. Both belong to
/// the daemon and are readable only by it.
struct Files {
    environment: PathBuf,
    output: PathBuf,
}

impl Files {
    fn new(config: &Config, id: Uuid) -> Self {
        let directory = config.state_dir().join(ENVIRONMENT_DIRECTORY);
        Self {
            environment: directory.join(format!("{id}.env")),
            output: directory.join(format!("{id}.log")),
        }
    }

    /// Whether either file is present, as from the moment the helper's unit
    /// may start until it has stopped.
    fn exist(&self) -> bool {
        fs::symlink_metadata(&self.environment).is_ok()
            || fs::symlink_metadata(&self.output).is_ok()
    }

    fn create(&self, command: &Command) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        write_environment(&self.environment, command)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.output)?;
        Ok(())
    }

    /// The end of the helper's output, as a suffix for an error message.
    fn output(&self) -> String {
        use std::io::{Read, Seek, SeekFrom};
        let read = || -> io::Result<Vec<u8>> {
            let mut file = fs::File::open(&self.output)?;
            let length = file.metadata()?.len();
            file.seek(SeekFrom::Start(
                length.saturating_sub(DIAGNOSTIC_BYTES as u64),
            ))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        match read() {
            Ok(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => {
                format!("; output: {}", String::from_utf8_lossy(&bytes).trim())
            }
            _ => String::new(),
        }
    }

    fn remove(&self) {
        for path in [&self.environment, &self.output] {
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %error, "cannot remove snapshot helper file");
                }
            }
        }
    }
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
    files: &Files,
    tenant_resolver: TenantResolver,
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
    let output = files
        .output
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF-8 helper output path"))?;
    let mut arguments = vec![
        "--quiet".to_string(),
        "--service-type=exec".into(),
        // A main process that exits successfully leaves the unit active and
        // loaded, so its exit can be read. Without this a successful unit
        // is unloaded at once, like one that a stop killed.
        "--property=RemainAfterExit=yes".into(),
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
        format!("--property=EnvironmentFile={}", word(&files.environment)?),
        format!("--property=StandardOutput=append:{output}"),
        format!("--property=StandardError=append:{output}"),
        format!("--property=RuntimeMaxSec={}ms", limit.as_millis().max(1)),
        "--property=KillMode=control-group".into(),
        "--property=SendSIGKILL=yes".into(),
        format!("--property=TimeoutStopSec={STOP_TIMEOUT}"),
        "--property=UMask=0022".into(),
    ];
    arguments.extend(crate::systemd::JOB_MEMORY_PROPERTIES.map(str::to_owned));
    arguments.extend(view.properties(&tenant_resolver)?);
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

/// What `observe` saw of a helper unit.
#[derive(Debug)]
enum Observed {
    /// The unit's main process has ended and systemd still holds the unit.
    Ended(MainExit),
    /// The unit was unloaded before its end could be read. Only a unit that
    /// became inactive is unloaded, and a helper's own successful end keeps
    /// it active, so the unit was stopped from outside the helper.
    Gone,
    /// The unit was still running well after its own limit.
    Overran,
}

/// The unit properties that say how its main process ended.
#[derive(Debug, Default)]
struct MainExit {
    /// systemd's `Result` for the unit.
    result: Option<String>,
    /// `ExecMainCode`: waitid's CLD_* code for the main process.
    code: Option<i32>,
    /// `ExecMainStatus`: the exit status, or the signal for CLD_KILLED and
    /// CLD_DUMPED.
    status: Option<i32>,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Succeeded,
    TimedOut,
    Failed(String),
}

/// Decides how a helper ended. Success needs the main process to have
/// exited with status 0 and the unit's result to be `success`: systemd also
/// reports `success` for a unit whose main process a stop killed with
/// SIGTERM, so the result alone proves nothing.
fn classify(observed: &Observed, elapsed: Duration, limit: Duration) -> Verdict {
    let exit = match observed {
        Observed::Overran => return Verdict::TimedOut,
        Observed::Gone => {
            return Verdict::Failed(
                "the unit was stopped and unloaded from outside the helper before its main \
                 process's exit could be read"
                    .into(),
            )
        }
        Observed::Ended(exit) => exit,
    };
    let result = exit.result.as_deref().unwrap_or("unknown");
    // A stop from outside that needed SIGKILL also reports `timeout`. It
    // counts as a time-out only once the time since `run` started the helper
    // has reached the limit; a stop before that is a failure.
    if result == "timeout" && elapsed >= limit {
        return Verdict::TimedOut;
    }
    let main = match (exit.code, exit.status) {
        (Some(libc::CLD_EXITED), Some(0)) if result == "success" => return Verdict::Succeeded,
        (Some(libc::CLD_EXITED), Some(status)) => {
            format!("its main process exited with status {status}")
        }
        (Some(libc::CLD_KILLED), Some(signal)) => {
            format!(
                "its main process was killed by signal {}",
                signal_name(signal)
            )
        }
        (Some(libc::CLD_DUMPED), Some(signal)) => format!(
            "its main process dumped core on signal {}",
            signal_name(signal)
        ),
        (code, status) => format!(
            "its main process reported no exit (ExecMainCode={}, ExecMainStatus={})",
            code.map(|code| code.to_string()).unwrap_or_default(),
            status.map(|status| status.to_string()).unwrap_or_default()
        ),
    };
    Verdict::Failed(format!("{main}; unit result {result}"))
}

/// `SIGTERM (15)` for the common signals, the number for the others.
fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        _ => return signal.to_string(),
    };
    format!("{name} ({signal})")
}

/// Starts the helper unit. `systemd-run` returns once the main process has
/// been started; `observe` then waits for it to end.
fn start(config: &Config, arguments: &[String]) -> io::Result<()> {
    #[cfg(test)]
    if let Some(launcher) = &config.snapshot_helper_launcher {
        return started(launcher(arguments)?);
    }
    let _ = config;
    #[cfg(target_os = "linux")]
    {
        crate::systemd::read_job_memory_budget()?;
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
            let _ = stderr
                .by_ref()
                .take(DIAGNOSTIC_BYTES as u64)
                .read_to_end(&mut captured);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            let _ = sender.send(captured);
        });
        let deadline = Instant::now() + START_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("systemd-run did not start the unit within {START_TIMEOUT:?}"),
                ));
            }
            std::thread::park_timeout(Duration::from_millis(10));
        };
        let stderr = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        started(Output {
            status,
            stdout: Vec::new(),
            stderr,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = arguments;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "snapshot helper units need an isolated Linux runner",
        ))
    }
}

fn started(output: Output) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "systemd-run could not start it ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// The unit properties `observe` and `settle` read.
struct UnitState {
    load: Option<String>,
    active: Option<String>,
    sub: Option<String>,
    exit: MainExit,
}

fn unit_state(config: &Config, unit: &str) -> io::Result<UnitState> {
    let output = crate::backend::systemctl(
        config,
        &[
            "show",
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
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
    let number = |name: &str| property(name).and_then(|value| value.parse().ok());
    Ok(UnitState {
        load: property("LoadState"),
        active: property("ActiveState"),
        sub: property("SubState"),
        exit: MainExit {
            result: property("Result"),
            code: number("ExecMainCode"),
            status: number("ExecMainStatus"),
        },
    })
}

/// Waits until the unit's main process has ended, or until `deadline` after
/// the start. Units run with `RemainAfterExit=yes`, so after a main process
/// that exited successfully the unit stays loaded and active, in the
/// `exited` state, and its exit can be read; anything the main process left
/// running stays in its cgroup until `settle` stops the unit.
fn observe(config: &Config, unit: &str, deadline: Duration) -> io::Result<Observed> {
    let deadline = Instant::now() + deadline;
    let mut delay = Duration::from_millis(10);
    loop {
        let state = unit_state(config, unit)?;
        if state.load.as_deref() == Some("not-found") {
            return Ok(Observed::Gone);
        }
        match (state.active.as_deref(), state.sub.as_deref()) {
            (Some("active"), Some("exited")) | (Some("failed" | "inactive"), _) => {
                return Ok(Observed::Ended(state.exit))
            }
            (None, _) => return Err(io::Error::other(format!("inspect {unit}: no ActiveState"))),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok(Observed::Overran);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(250));
    }
}

/// How a helper unit was left once it was inactive.
struct Settled {
    /// systemd's `Result` for the unit, unless it was already unloaded.
    result: Option<String>,
}

/// Waits until `unit` is inactive or failed; for a failed unit it then runs
/// `reset-failed`, so systemd can unload it. A unit still active is stopped, which sends SIGTERM to its whole cgroup and SIGKILL
/// after `STOP_TIMEOUT`; if it is still not inactive a little later, the
/// runner sends SIGKILL to the cgroup itself.
fn settle(config: &Config, unit: &str) -> io::Result<Settled> {
    let started = Instant::now();
    let mut stop_requested: Option<Instant> = None;
    let mut killed = false;
    loop {
        let state = unit_state(config, unit)?;
        if state.load.as_deref() == Some("not-found") {
            return Ok(Settled { result: None });
        }
        match state.active.as_deref() {
            Some("inactive") => {
                return Ok(Settled {
                    result: state.exit.result,
                })
            }
            Some("failed") => {
                let reset = crate::backend::systemctl(config, &["reset-failed", unit])?;
                if !reset.status.success()
                    && !String::from_utf8_lossy(&reset.stderr).contains("not loaded")
                {
                    tracing::warn!(unit, stderr = %String::from_utf8_lossy(&reset.stderr), "cannot unload failed snapshot helper");
                }
                return Ok(Settled {
                    result: state.exit.result,
                });
            }
            Some(_) => {}
            None => return Err(io::Error::other(format!("inspect {unit}: no ActiveState"))),
        }
        if started.elapsed() >= SETTLE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{unit} is still {} after {SETTLE_TIMEOUT:?}",
                    state.active.unwrap_or_default()
                ),
            ));
        }
        let command: Option<&[&str]> = if stop_requested.is_none() {
            stop_requested = Some(Instant::now());
            Some(&["stop", "--no-block", unit])
        } else if !killed && stop_requested.is_some_and(|at| at.elapsed() >= KILL_AFTER) {
            killed = true;
            Some(&["kill", "--kill-whom=all", "--signal=KILL", unit])
        } else {
            None
        };
        if let Some(command) = command {
            let output = crate::backend::systemctl(config, command)?;
            if !output.status.success() {
                tracing::warn!(unit, stderr = %String::from_utf8_lossy(&output.stderr), "cannot stop snapshot helper");
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
