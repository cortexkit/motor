//! Provisioning files for the remote compile cache and the generation that is
//! active on this host.
//!
//! Whoever delivers the cache keys (the operator on the build server and a
//! tenant VM, CLOUD's broker on Boat) writes `config.json` (mode 0444) and one
//! key file per key (mode 0400), each a JSON object carrying the same
//! `generation`. They live in `cache-provisioning/` under the runner's state
//! directory (created 0700 by the runner), or in `/run/motor/cache/` on Boat.
//! The build server always has a `write-key`; a tenant VM has one only when
//! its tenant fills its own cache, and Boat never has one.
//! Every file must be a regular file owned by the runner's account; a key file
//! must be readable by that account alone, so the job account (`motor-job`) and
//! the fill account (`motor-cache`) cannot open it.
//!
//! A generation is the config plus every key file the host expects, all with
//! one `generation` value. A new generation becomes active only when the whole
//! set parses and agrees; a half-updated set keeps the previous generation.
//! `enabled` is the one field that may change within a generation, and it is
//! taken from `config.json` at every re-read on its own.
use serde::Deserialize;
use std::fmt;
#[cfg(unix)]
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

/// Boat's broker installs the files here on tmpfs, never under /etc, because
/// Boat archives /etc in snapshots and swaps it late after a restore.
pub(crate) const BOAT_DIRECTORY: &str = "/run/motor/cache";
/// Directory under the runner's state directory on the build server and on a
/// tenant VM; it persists across reboots.
pub(crate) const STATE_DIRECTORY: &str = "cache-provisioning";
pub(crate) const CONFIG_FILE: &str = "config.json";
pub(crate) const CONFIG_MODE: u32 = 0o444;
pub(crate) const KEY_MODE: u32 = 0o400;
pub(crate) const DIRECTORY_MODE: u32 = 0o700;
/// Provisioning files are a few hundred bytes; anything larger is refused
/// rather than read into memory.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// The files are re-read at least this often even when nothing seems changed.
pub(crate) const REREAD_INTERVAL: Duration = Duration::from_secs(10);
/// How often file metadata is compared to notice a change between full
/// re-reads.
pub(crate) const WATCH_POLL: Duration = Duration::from_secs(1);

/// The host type decides where the files live and which keys it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostRole {
    /// Holds the read-only key for jobs and the read-write key for fills.
    BuildServer,
    /// Holds the read-only key, and may also hold a read-write key for its
    /// own fills. The store policy confines that key to this tenant's prefix.
    /// Without it the tenant never fills, but its jobs still read the cache.
    TenantVm,
    /// Holds only the read-only key, delivered by CLOUD's broker.
    Boat,
}

impl HostRole {
    pub(crate) fn of(config: &crate::Config) -> Self {
        if config.boat_profile {
            Self::Boat
        } else if config.tenant_vm {
            Self::TenantVm
        } else {
            Self::BuildServer
        }
    }
    /// The keys a complete generation must hold on this host.
    pub(crate) fn keys(self) -> &'static [KeyRole] {
        match self {
            Self::BuildServer => &[KeyRole::Read, KeyRole::Write],
            Self::TenantVm | Self::Boat => &[KeyRole::Read],
        }
    }
    /// Keys this host may hold but can run without. A missing optional key
    /// file is not part of the generation at all. A present one must parse and
    /// carry the generation's `generation`, just like a required key, so a
    /// half-written rotation still keeps the previous generation.
    pub(crate) fn optional_keys(self) -> &'static [KeyRole] {
        match self {
            Self::TenantVm => &[KeyRole::Write],
            Self::BuildServer | Self::Boat => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyRole {
    /// The read-only store user, used by the gateway's read endpoint.
    Read,
    /// The read-write store user, used only by the write endpoint for fills.
    Write,
}

impl KeyRole {
    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Self::Read => "read-key",
            Self::Write => "write-key",
        }
    }
}

/// Where this host's provisioning files are and who must own them.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    directory: PathBuf,
    role: HostRole,
    owner: u32,
}

#[cfg(unix)]
impl Layout {
    pub(crate) fn for_config(config: &crate::Config) -> Self {
        let role = HostRole::of(config);
        let directory = match role {
            HostRole::Boat => PathBuf::from(BOAT_DIRECTORY),
            HostRole::BuildServer | HostRole::TenantVm => config.state_dir().join(STATE_DIRECTORY),
        };
        Self::new(directory, role)
    }
    /// Files must be owned by the account the runner runs as.
    pub(crate) fn new(directory: PathBuf, role: HostRole) -> Self {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let owner = unsafe { libc::geteuid() };
        Self {
            directory,
            role,
            owner,
        }
    }
    /// Expects another owner, so tests can show files owned by a different
    /// account are refused without needing root to chown them.
    #[cfg(test)]
    pub(crate) fn with_owner(mut self, owner: u32) -> Self {
        self.owner = owner;
        self
    }
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn role(&self) -> HostRole {
        self.role
    }
    pub(crate) fn config_path(&self) -> PathBuf {
        self.directory.join(CONFIG_FILE)
    }
    pub(crate) fn key_path(&self, key: KeyRole) -> PathBuf {
        self.directory.join(key.file_name())
    }

    /// Creates the runner-owned directory with mode 0700, or restores that
    /// mode on an existing one, at runner startup. Boat's directory belongs to
    /// the broker, which creates it at every create and resume, so it is left
    /// alone here and only checked when the files are read.
    pub(crate) fn prepare(&self) -> io::Result<()> {
        if self.role == HostRole::Boat {
            return Ok(());
        }
        match fs::DirBuilder::new()
            .mode(DIRECTORY_MODE)
            .create(&self.directory)
        {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let metadata = fs::symlink_metadata(&self.directory)?;
        if !metadata.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                self.directory.display()
            )));
        }
        if metadata.uid() != self.owner {
            return Err(io::Error::other(format!(
                "{} is owned by uid {}, not the runner's uid {}",
                self.directory.display(),
                metadata.uid(),
                self.owner
            )));
        }
        fs::set_permissions(&self.directory, fs::Permissions::from_mode(DIRECTORY_MODE))
    }

    fn check_directory(&self) -> Result<(), String> {
        let metadata = fs::symlink_metadata(&self.directory)
            .map_err(|e| format!("{}: {e}", self.directory.display()))?;
        if !metadata.is_dir() {
            return Err(format!("{} is not a directory", self.directory.display()));
        }
        if metadata.uid() != self.owner {
            return Err(format!(
                "{} is owned by uid {}, not the runner's uid {}",
                self.directory.display(),
                metadata.uid(),
                self.owner
            ));
        }
        let mode = metadata.mode() & 0o7777;
        let acceptable = match self.role {
            // The broker owns this tmpfs directory's mode; it is enough that
            // nobody but the runner's account can replace files in it.
            HostRole::Boat => mode & 0o022 == 0,
            HostRole::BuildServer | HostRole::TenantVm => mode == DIRECTORY_MODE,
        };
        if !acceptable {
            return Err(format!(
                "{} has mode {mode:04o}, expected {}",
                self.directory.display(),
                match self.role {
                    HostRole::Boat => "no group or other write access".to_string(),
                    _ => format!("{DIRECTORY_MODE:04o}"),
                }
            ));
        }
        Ok(())
    }

    /// Reads and parses every file this host expects.
    pub(crate) fn read(&self) -> FileSet {
        if let Err(problem) = self.check_directory() {
            return FileSet {
                config: Err(problem.clone()),
                keys: self
                    .role
                    .keys()
                    .iter()
                    .map(|key| (*key, Err(problem.clone())))
                    .collect(),
            };
        }
        let config = read_file(&self.config_path(), CONFIG_MODE, self.owner)
            .and_then(|bytes| parse_config(&bytes));
        let read_key = |key: KeyRole| {
            read_file(&self.key_path(key), KEY_MODE, self.owner).and_then(|bytes| {
                parse_key(&bytes).map_err(|e| format!("{}: {e}", key.file_name()))
            })
        };
        let mut keys: Vec<_> = self
            .role
            .keys()
            .iter()
            .map(|key| (*key, read_key(*key)))
            .collect();
        for key in self.role.optional_keys() {
            // Only a file that does not exist at all leaves the key out; any
            // other problem with it makes the set incomplete.
            let absent = matches!(
                fs::symlink_metadata(self.key_path(*key)),
                Err(e) if e.kind() == io::ErrorKind::NotFound
            );
            if !absent {
                keys.push((*key, read_key(*key)));
            }
        }
        FileSet { config, keys }
    }

    /// Cheap metadata summary of the directory and every expected file, so a
    /// watcher notices a rename, rewrite or chmod without parsing anything.
    fn fingerprint(&self) -> Vec<Option<[i64; 8]>> {
        let mut paths = vec![self.directory.clone(), self.config_path()];
        paths.extend(
            self.role
                .keys()
                .iter()
                .chain(self.role.optional_keys())
                .map(|key| self.key_path(*key)),
        );
        paths
            .iter()
            .map(|path| {
                fs::symlink_metadata(path).ok().map(|m| {
                    [
                        m.dev() as i64,
                        m.ino() as i64,
                        m.size() as i64,
                        m.mtime(),
                        m.mtime_nsec(),
                        m.ctime(),
                        m.ctime_nsec(),
                        (m.mode() as i64) | ((m.uid() as i64) << 32),
                    ]
                })
            })
            .collect()
    }
}

/// Opens one provisioning file without following a symlink and checks it is a
/// regular file owned by `owner` with exactly `mode`.
#[cfg(unix)]
fn read_file(path: &Path, mode: u32, owner: u32) -> Result<Vec<u8>, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file = OpenOptions::new()
        .read(true)
        // O_NONBLOCK keeps a FIFO planted in place of a file from blocking
        // the re-read; it changes nothing for the regular file accepted below.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => format!("{name}: missing"),
            _ => format!("{name}: {e}"),
        })?;
    let metadata = file.metadata().map_err(|e| format!("{name}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("{name}: not a regular file"));
    }
    if metadata.uid() != owner {
        return Err(format!(
            "{name}: owned by uid {}, not the runner's uid {owner}",
            metadata.uid()
        ));
    }
    let actual = metadata.mode() & 0o7777;
    if actual != mode {
        return Err(format!("{name}: mode {actual:04o}, expected {mode:04o}"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name}: {e}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("{name}: larger than {MAX_FILE_BYTES} bytes"));
    }
    Ok(bytes)
}

/// The `generation` every file of one set carries: an opaque non-empty JSON
/// string, compared only for equality. A number or any other JSON type makes
/// the file invalid, so a file holding 1 never meets one holding "1".
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub(crate) struct Generation(pub(crate) String);

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// `config.json`, the same shape on every host type.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigFile {
    pub(crate) generation: Generation,
    /// The cache-tenant id, the tenant subject such as `acct:<ULID>`. It is
    /// not the Boat sandbox id in `/etc/motor/tenant/id`.
    pub(crate) tenant: String,
    pub(crate) endpoint: String,
    pub(crate) region: String,
    pub(crate) bucket: String,
    pub(crate) prefix: String,
    pub(crate) storage_class: String,
    pub(crate) enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    generation: Generation,
    access_key_id: String,
    secret_access_key: String,
}

/// One store user's credential. Debug output never shows the secret.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credential {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// A parsed key file: its generation and the credential it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyMaterial {
    pub(crate) generation: Generation,
    pub(crate) credential: Credential,
}

fn visible_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())
}

fn parse_config(bytes: &[u8]) -> Result<ConfigFile, String> {
    let config: ConfigFile =
        serde_json::from_slice(bytes).map_err(|e| format!("{CONFIG_FILE}: {e}"))?;
    if config.generation.0.is_empty() {
        return Err(format!("{CONFIG_FILE}: generation is empty"));
    }
    // The tenant id becomes part of an object-store key prefix, so it is held
    // to characters that cannot add a path segment or need escaping.
    if config.tenant.is_empty()
        || !config
            .tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        return Err(format!(
            "{CONFIG_FILE}: tenant must be non-empty and use only letters, digits, '.', '_', ':' and '-'"
        ));
    }
    let endpoint = url::Url::parse(&config.endpoint)
        .map_err(|e| format!("{CONFIG_FILE}: endpoint is not a URL: {e}"))?;
    // Bodies are sent as UNSIGNED-PAYLOAD, so only TLS protects what a fill
    // writes; a plain-http store could be fed altered objects on the way.
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(format!(
            "{CONFIG_FILE}: endpoint must be an https URL with a host and no credentials, query or fragment"
        ));
    }
    for (field, value) in [
        ("region", &config.region),
        ("bucket", &config.bucket),
        ("prefix", &config.prefix),
        ("storage_class", &config.storage_class),
    ] {
        if !visible_token(value) {
            return Err(format!(
                "{CONFIG_FILE}: {field} must be non-empty printable ASCII without spaces"
            ));
        }
    }
    Ok(config)
}

fn parse_key(bytes: &[u8]) -> Result<KeyMaterial, String> {
    // serde_json's messages can quote the offending value, which in a key file
    // may be the secret itself, so only the error category and position are
    // reported.
    let key: KeyFile = serde_json::from_slice(bytes).map_err(|e| {
        let category = match e.classify() {
            serde_json::error::Category::Io => "unreadable",
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                "not valid JSON"
            }
            serde_json::error::Category::Data => {
                "not an object of generation, access_key_id and secret_access_key"
            }
        };
        format!("{category} at line {} column {}", e.line(), e.column())
    })?;
    if key.generation.0.is_empty() {
        return Err("generation is empty".to_string());
    }
    if !visible_token(&key.access_key_id) || !visible_token(&key.secret_access_key) {
        return Err(
            "access_key_id and secret_access_key must be non-empty printable ASCII without spaces"
                .to_string(),
        );
    }
    Ok(KeyMaterial {
        generation: key.generation,
        credential: Credential {
            access_key_id: key.access_key_id,
            secret_access_key: key.secret_access_key,
        },
    })
}

/// What one read of the provisioning directory found, file by file. Errors
/// name the file and the problem, never a file's contents.
#[derive(Debug)]
pub(crate) struct FileSet {
    pub(crate) config: Result<ConfigFile, String>,
    pub(crate) keys: Vec<(KeyRole, Result<KeyMaterial, String>)>,
}

impl FileSet {
    /// The generation these files form, when every file parsed and they all
    /// carry one `generation`; otherwise why they do not form one.
    fn complete(&self) -> Result<ActiveGeneration, String> {
        let mut problems = Vec::new();
        if let Err(problem) = &self.config {
            problems.push(problem.clone());
        }
        for (_, parsed) in &self.keys {
            if let Err(problem) = parsed {
                problems.push(problem.clone());
            }
        }
        // A directory problem is reported once, not once per file.
        problems.dedup();
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let config = self.config.as_ref().unwrap();
        let mixed = self
            .keys
            .iter()
            .any(|(_, parsed)| parsed.as_ref().unwrap().generation != config.generation);
        if mixed {
            let mut generations = vec![format!("{CONFIG_FILE} {}", config.generation)];
            generations.extend(self.keys.iter().map(|(key, parsed)| {
                format!(
                    "{} {}",
                    key.file_name(),
                    parsed.as_ref().unwrap().generation
                )
            }));
            return Err(format!("generations differ ({})", generations.join(", ")));
        }
        let key = |role: KeyRole| {
            self.keys
                .iter()
                .find(|(key, _)| *key == role)
                .map(|(_, parsed)| parsed.as_ref().unwrap().credential.clone())
        };
        Ok(ActiveGeneration {
            generation: config.generation.clone(),
            tenant: config.tenant.clone(),
            endpoint: config.endpoint.clone(),
            region: config.region.clone(),
            bucket: config.bucket.clone(),
            prefix: config.prefix.clone(),
            storage_class: config.storage_class.clone(),
            read_key: key(KeyRole::Read).expect("every host role holds a read key"),
            write_key: key(KeyRole::Write),
        })
    }
}

/// The store prefix a cache tenant owns: `tenant/` plus the tenant id with
/// every `:` replaced by `-`, plus `/`; e.g. `acct:01K…` gives
/// `tenant/acct-01K…/`. Objects of a tool live under `<prefix><tool>/`.
pub(crate) fn derived_prefix(tenant: &str) -> String {
    format!("tenant/{}/", tenant.replace(':', "-"))
}

/// The store a generation addresses. Records of published fills belong to
/// one identity; a key rotation keeps it unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreIdentity {
    pub(crate) endpoint: String,
    pub(crate) bucket: String,
    pub(crate) prefix: String,
}

impl StoreIdentity {
    /// The same identity in the form the catalog records and compares.
    pub(crate) fn recorded(&self) -> crate::storage::StoreIdentity {
        crate::storage::encode_identity(self)
    }
}

/// A complete provisioning generation: the config without `enabled`, plus
/// every key the host holds. It never changes once active; a request signs
/// with the generation it was admitted under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveGeneration {
    pub(crate) generation: Generation,
    pub(crate) tenant: String,
    pub(crate) endpoint: String,
    pub(crate) region: String,
    pub(crate) bucket: String,
    pub(crate) prefix: String,
    pub(crate) storage_class: String,
    pub(crate) read_key: Credential,
    /// The build server always holds the read-write key; a tenant VM holds it
    /// only when its provisioning delivered one; Boat never does.
    pub(crate) write_key: Option<Credential>,
}

impl ActiveGeneration {
    pub(crate) fn identity(&self) -> StoreIdentity {
        StoreIdentity {
            endpoint: self.endpoint.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
        }
    }
}

/// Why the cache cannot be used; health reports it as `cache_unavailable`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// No complete set has ever been read, so there is nothing to fall back to.
    NoCompleteGeneration { detail: String },
    /// A complete new set names a prefix other than the one derived from its
    /// tenant. This guards against misconfiguration; the store policy is what
    /// confines each tenant.
    PrefixMismatch {
        generation: Generation,
        expected: String,
        found: String,
    },
    /// A complete set repeats the active `generation` with some field other
    /// than `enabled`, or a key, changed. Generations are immutable.
    GenerationReused { generation: Generation },
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCompleteGeneration { detail } => {
                write!(f, "no complete provisioning generation: {detail}")
            }
            Self::PrefixMismatch {
                generation,
                expected,
                found,
            } => write!(
                f,
                "prefix {found:?} of generation {generation} does not match the tenant's derived prefix {expected:?}"
            ),
            Self::GenerationReused { .. } => f.write_str("generation reused"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Available,
    Unavailable(Unavailable),
}

/// What one re-read changed, for whoever acts on switch and identity
/// transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reread {
    /// A new generation became active at this re-read.
    pub(crate) activated: bool,
    /// The store identity active before this re-read.
    pub(crate) previous_identity: Option<StoreIdentity>,
    /// The store identity active after this re-read.
    pub(crate) identity: Option<StoreIdentity>,
    /// `enabled` before this re-read; `None` if no config had parsed yet.
    pub(crate) previous_enabled: Option<bool>,
    /// `enabled` after this re-read; `None` until a config has parsed. An
    /// unparsable config leaves it unchanged.
    pub(crate) enabled: Option<bool>,
    pub(crate) status: Status,
}

/// The active generation and the latest `enabled`, updated by re-reads.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct Provisioning {
    layout: Layout,
    active: Option<Arc<ActiveGeneration>>,
    enabled: Option<bool>,
    status: Status,
}

#[cfg(unix)]
impl Provisioning {
    pub(crate) fn new(layout: Layout) -> Self {
        Self {
            layout,
            active: None,
            enabled: None,
            status: Status::Unavailable(Unavailable::NoCompleteGeneration {
                detail: "not read yet".to_string(),
            }),
        }
    }
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Reads the files and applies them.
    pub(crate) fn reread(&mut self) -> Reread {
        let files = self.layout.read();
        self.apply(files)
    }

    /// Applies one read of the files.
    pub(crate) fn apply(&mut self, files: FileSet) -> Reread {
        let previous_identity = self.active.as_ref().map(|active| active.identity());
        let previous_enabled = self.enabled;
        // `enabled` is the tenant's switch and follows any config that parses,
        // whether or not its generation is complete or already active.
        if let Ok(config) = &files.config {
            self.enabled = Some(config.enabled);
        }
        let mut activated = false;
        self.status = match files.complete() {
            Ok(candidate) => match &self.active {
                Some(active) if active.generation == candidate.generation => {
                    if **active == candidate {
                        Status::Available
                    } else {
                        Status::Unavailable(Unavailable::GenerationReused {
                            generation: candidate.generation,
                        })
                    }
                }
                _ => {
                    let expected = derived_prefix(&candidate.tenant);
                    if candidate.prefix == expected {
                        self.active = Some(Arc::new(candidate));
                        activated = true;
                        Status::Available
                    } else {
                        Status::Unavailable(Unavailable::PrefixMismatch {
                            generation: candidate.generation,
                            expected,
                            found: candidate.prefix,
                        })
                    }
                }
            },
            // A missing, unparsable or mixed set keeps the previous complete
            // generation; only with none at all is the cache unavailable.
            Err(detail) => match self.active {
                Some(_) => Status::Available,
                None => Status::Unavailable(Unavailable::NoCompleteGeneration { detail }),
            },
        };
        Reread {
            activated,
            previous_identity,
            identity: self.active.as_ref().map(|active| active.identity()),
            previous_enabled,
            enabled: self.enabled,
            status: self.status.clone(),
        }
    }

    /// The generation a request admitted now signs with: the active one while
    /// the cache is available and the tenant has it enabled, otherwise none,
    /// and the request runs uncached. A caller keeps the returned generation
    /// for the whole request, so a later activation never changes the key of a
    /// request already admitted.
    pub(crate) fn admit(&self) -> Option<Arc<ActiveGeneration>> {
        match (&self.status, self.enabled) {
            (Status::Available, Some(true)) => self.active.clone(),
            _ => None,
        }
    }
    pub(crate) fn status(&self) -> &Status {
        &self.status
    }
    /// The tenant's switch from the latest config that parsed; off until one
    /// has.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled == Some(true)
    }
    pub(crate) fn active(&self) -> Option<&Arc<ActiveGeneration>> {
        self.active.as_ref()
    }
}

/// Shared provisioning state with a background re-reader.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) struct Watch {
    inner: Arc<Mutex<Provisioning>>,
}

#[cfg(unix)]
impl Watch {
    pub(crate) fn new(layout: Layout) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Provisioning::new(layout))),
        }
    }
    pub(crate) fn reread(&self) -> Reread {
        self.inner.lock().unwrap().reread()
    }
    pub(crate) fn admit(&self) -> Option<Arc<ActiveGeneration>> {
        self.inner.lock().unwrap().admit()
    }
    pub(crate) fn status(&self) -> Status {
        self.inner.lock().unwrap().status().clone()
    }
    pub(crate) fn enabled(&self) -> bool {
        self.inner.lock().unwrap().enabled()
    }
    pub(crate) fn enabled_setting(&self) -> Option<bool> {
        self.inner.lock().unwrap().enabled
    }

    /// Re-reads the files whenever their metadata changes (checked every
    /// `poll`) and at least every `full`, handing each outcome to `report` in
    /// order. A `cache_unavailable` decision is therefore re-evaluated at every
    /// re-read. Runs until the task is dropped or aborted.
    pub(crate) async fn run(
        self,
        poll: Duration,
        full: Duration,
        mut report: impl FnMut(Reread) + Send,
    ) {
        let layout = self.inner.lock().unwrap().layout().clone();
        let mut fingerprint = None;
        let mut last_full: Option<Instant> = None;
        let mut tick = tokio::time::interval(poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let current = {
                let layout = layout.clone();
                tokio::task::spawn_blocking(move || layout.fingerprint())
                    .await
                    .ok()
            };
            let due = last_full.is_none_or(|at| at.elapsed() >= full);
            if !due && current.is_some() && current == fingerprint {
                continue;
            }
            let this = self.clone();
            let Ok(outcome) = tokio::task::spawn_blocking(move || this.reread()).await else {
                continue;
            };
            fingerprint = current;
            last_full = Some(Instant::now());
            report(outcome);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "provisioning_tests.rs"]
mod tests;
