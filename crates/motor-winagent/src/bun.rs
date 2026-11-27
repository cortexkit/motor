//! Versioned, credential-free Bun seed shared by the Windows hosts and guest.

use std::collections::BTreeMap;
use std::io;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
mod expansion;
mod graph;
mod hash;
mod install_roots;
mod preparation;
mod timing;
pub use expansion::ExpansionBudget;
pub use preparation::{
    DiagnosticTail, InstallEvent, finish_install, skip_project, skip_unprepared, wait_install,
};
pub use timing::PreparationTimings;
#[cfg(windows)]
pub mod protection;
pub mod windows_path;

pub const BUN_VERSION: &str = "1.4.4";
pub const CACHE_DIR: &str = "C:\\motor-job\\bun-cache";
pub const MAX_LOCK_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;

/// Use archive validation's Unicode case folding for the reserved directory name.
pub fn is_node_modules(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().to_uppercase() == "NODE_MODULES"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusalKind {
    BunLockInvalid,
    BunSourceUnsupported,
    BunIntegrityMissing,
    BunIntegrityMismatch,
    BunCredentialsUnavailable,
    BunRegistryUnauthorized,
    BunFetchUnavailable,
    BunCacheMissing,
    BunProjectionInvalid,
    BunOfflineInstallFailed,
}

impl RefusalKind {
    /// Invalid or untrusted inputs are refusals; availability is only a convenience.
    pub fn is_security(self) -> bool {
        !matches!(
            self,
            Self::BunCredentialsUnavailable
                | Self::BunFetchUnavailable
                | Self::BunCacheMissing
                | Self::BunOfflineInstallFailed
        )
    }
}

/// Fixed diagnostics cross the trust boundary; never serialize host I/O errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Check {
    Lock,
    Source,
    Integrity,
    Registry,
    Credentials,
    Download,
    FetchDeadline,
    CacheLock,
    CacheRead,
    CacheProvenance,
    CacheProtection,
    CacheQuota,
    CachePublish,
    SeedWrite,
    SeedBudget,
    SeedManifest,
    SeedBlobs,
    Projection,
    Identity,
    RegistryConfig,
    WorkingDirectory,
    ProcessStart,
    ExitPoll,
    JobTerminate,
    JobWaitEmpty,
    InstallCleanup,
    Cancelled,
    InstallDeadline,
    PipeRead,
    PipesDisconnected,
    NonzeroExit,
    InstalledTree,
    InstalledPackage,
    InstalledBin,
    InstalledBinDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissingPackage {
    pub name: String,
    pub version: String,
}

fn missing_empty(packages: &[MissingPackage]) -> bool {
    packages.is_empty()
}

fn detail_empty(detail: &str) -> bool {
    detail.is_empty()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub project: String,
    pub package: String,
    /// Boxed like `registry_origin` and `detail` so a refusal stays small enough
    /// to return by value from every preparation step.
    pub version: Box<str>,
    /// Boxed like `detail` so a refusal stays small enough to return by value.
    pub registry_origin: Box<str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
    /// The failed condition and its checkout-relative path, never a host I/O
    /// error message. Boxing keeps refusals small enough to return by value.
    #[serde(default, skip_serializing_if = "detail_empty")]
    pub detail: Box<str>,
    #[serde(default, skip_serializing_if = "missing_empty")]
    pub missing: Box<[MissingPackage]>,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: project={} package={} version={} registry={} check={:?}",
            self.kind, self.project, self.package, self.version, self.registry_origin, self.check
        )?;
        // A refused job must name the condition and path that refused it, so
        // the detail is part of the message rather than a separate field.
        if !self.detail.is_empty() {
            write!(f, " detail={}", self.detail)?;
        }
        Ok(())
    }
}
impl std::error::Error for Refusal {}
impl Refusal {
    pub fn warning(&self) -> String {
        let identity = if self.package.is_empty() {
            String::new()
        } else if self.version.is_empty() {
            format!(" package={}", self.package)
        } else {
            format!(" package={}@{}", self.package, self.version)
        };
        let missing = if self.missing.is_empty() {
            String::new()
        } else {
            format!(
                " missing=[{}]",
                self.missing
                    .iter()
                    .map(|p| format!("{}@{}", p.name, p.version))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let check = self
            .check
            .map(|check| format!("{check:?}"))
            .unwrap_or_else(|| "Unspecified".into());
        let detail = if self.detail.is_empty() {
            String::new()
        } else {
            format!(" detail={}", self.detail)
        };
        format!(
            "bun install skipped for {}: {:?}{} check={}{}{}\n",
            self.project, self.kind, identity, check, missing, detail
        )
    }
    /// Say which check failed and what it found, as a path relative to the job
    /// checkout. Refusals raised outside `check` keep it empty.
    pub fn detail(mut self, detail: impl Into<Box<str>>) -> Self {
        self.detail = detail.into();
        self
    }
    pub fn at(mut self, check: Check) -> Self {
        self.check = Some(check);
        if self.detail.is_empty()
            && matches!(
                self.kind,
                RefusalKind::BunProjectionInvalid | RefusalKind::BunOfflineInstallFailed
            )
        {
            self.detail = format!("{check:?} failed at . (project working directory)").into();
        }
        self
    }
    pub fn missing_packages(mut self, packages: impl IntoIterator<Item = MissingPackage>) -> Self {
        self.missing = packages.into_iter().take(5).collect();
        self
    }
    pub fn registry(mut self, registries: &BTreeMap<String, String>) -> Self {
        if let Some(registry) = self
            .package
            .split('/')
            .next()
            .and_then(|scope| registries.get(scope))
            && let Ok(url) = url::Url::parse(registry)
            && url.scheme() == "https"
        {
            self.registry_origin = url.origin().ascii_serialization().into();
        }
        self
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.missing.len() > 5
            || self.missing.iter().any(|p| {
                !valid_name(&p.name)
                    || p.name.len() > 214
                    || p.version.is_empty()
                    || p.version.len() > 256
                    || !p
                        .version
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
            })
        {
            return Err(io::Error::other("invalid Bun missing package diagnostics"));
        }
        for field in [
            &self.project,
            &self.package,
            &*self.version,
            &*self.registry_origin,
            &*self.detail,
        ] {
            if field.len() > 4096 || field.chars().any(char::is_control) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Bun refusal",
                ));
            }
        }
        if !self.registry_origin.is_empty() {
            let host = self
                .registry_origin
                .strip_prefix("https://")
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid registry origin")
                })?;
            if host.is_empty() || host.contains(['/', '@', '?', '#', '\\']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid registry origin",
                ));
            }
        }
        Ok(())
    }
}
impl From<Refusal> for io::Error {
    fn from(value: Refusal) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, value)
    }
}

pub fn refuse(kind: RefusalKind, project: &str, package: &str, version: &str) -> Refusal {
    Refusal {
        kind,
        project: project.into(),
        package: package.into(),
        version: if version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
        {
            version.into()
        } else {
            Box::default()
        },
        registry_origin: if package.is_empty() {
            Box::default()
        } else {
            "https://registry.npmjs.org".into()
        },
        check: Some(match kind {
            RefusalKind::BunLockInvalid => Check::Lock,
            RefusalKind::BunSourceUnsupported => Check::Source,
            RefusalKind::BunIntegrityMissing | RefusalKind::BunIntegrityMismatch => {
                Check::Integrity
            }
            RefusalKind::BunCredentialsUnavailable => Check::Credentials,
            RefusalKind::BunRegistryUnauthorized => Check::Registry,
            RefusalKind::BunFetchUnavailable => Check::Download,
            RefusalKind::BunCacheMissing => Check::SeedBlobs,
            RefusalKind::BunProjectionInvalid => Check::Projection,
            RefusalKind::BunOfflineInstallFailed => Check::InstalledTree,
        }),
        detail: Box::default(),
        missing: Box::default(),
    }
}

/// The digest identifies compressed bytes, not a tar reader's interpretation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integrity {
    pub algorithm: String,
    pub hex: String,
}
impl Integrity {
    pub fn parse(sri: &str) -> Option<Self> {
        let (algorithm, encoded) = sri.split_once('-')?;
        let length = match algorithm {
            "sha256" => 32,
            "sha512" => 64,
            _ => return None,
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        (bytes.len() == length).then(|| Self {
            algorithm: algorithm.into(),
            hex: hex(&bytes),
        })
    }
    pub fn valid(&self) -> bool {
        let length = match self.algorithm.as_str() {
            "sha256" => 64,
            "sha512" => 128,
            _ => return false,
        };
        self.hex.len() == length
            && self
                .hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
    pub fn verify(&self, bytes: &[u8]) -> bool {
        self.valid()
            && match self.algorithm.as_str() {
                "sha256" => hex(&Sha256::digest(bytes)) == self.hex,
                "sha512" => hex(&Sha512::digest(bytes)) == self.hex,
                _ => false,
            }
    }
    pub fn blob_path(&self) -> String {
        format!("blobs/{}/{}", self.algorithm, self.hex)
    }
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub placement: String,
    pub name: String,
    pub version: String,
    pub integrity: Integrity,
    pub windows: bool,
    pub bins: Vec<String>,
    pub bin_dir: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockedPackage {
    pub package: Package,
    pub url: String,
}

fn platform(metadata: &Value, field: &str, target: &str) -> bool {
    let Some(value) = metadata.get(field) else {
        return true;
    };
    let values: Vec<_> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => return false,
    };
    let recognized = |name| match field {
        "os" => [
            "aix", "android", "darwin", "freebsd", "linux", "openbsd", "sunos", "win32",
        ]
        .contains(&name),
        _ => [
            "arm", "arm64", "ia32", "loong64", "mips", "mips64", "ppc", "ppc64", "riscv64", "s390",
            "s390x", "x64",
        ]
        .contains(&name),
    };
    let (mut added, mut removed, mut positive, mut negative, mut wildcard, mut unknown) =
        (false, false, false, false, false, false);
    for value in values {
        if value == "any" {
            wildcard = true;
            continue;
        }
        let name = value.strip_prefix('!').unwrap_or(value);
        if recognized(name) {
            wildcard = false;
            unknown = false;
            if value.starts_with('!') {
                negative = true;
                removed |= name == target;
            } else {
                positive = true;
                added |= name == target;
            }
        } else if !value.starts_with('!') {
            unknown = true;
        }
    }
    !removed && (wildcard || added || (!positive && (negative || !unknown)))
}

/// Bun text locks are JSON with trailing commas. Only remove commas outside strings.
pub fn text_json(text: &str) -> Result<Value, serde_json::Error> {
    let mut bytes = text.as_bytes().to_vec();
    let (mut string, mut escape) = (false, false);
    for i in 0..bytes.len() {
        let b = bytes[i];
        if string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                string = false;
            }
        } else if b == b'"' {
            string = true;
        } else if b == b',' {
            let next = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                bytes[i] = b' ';
            }
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = UniqueValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueValue>()? {
                    if values.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(UniqueValue(Value::Object(values)))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<UniqueValue>()? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.into())))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value)))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|n| UniqueValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

pub fn valid_name(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    name.len() <= 214
        && (parts.len() == 1 || (parts.len() == 2 && parts[0].starts_with('@')))
        && parts.iter().all(|part| {
            let part = part.strip_prefix('@').unwrap_or(part);
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

fn resolution_parts(resolution: &str) -> Option<(&str, &str)> {
    let start = usize::from(resolution.starts_with('@'));
    let at = resolution[start..].find('@')? + start;
    Some((&resolution[..at], &resolution[at + 1..]))
}

/// Every registry row is returned, including disabled and foreign-platform rows.
pub fn read_lock(project: &str, text: &str) -> Result<Vec<LockedPackage>, Refusal> {
    let invalid = || refuse(RefusalKind::BunLockInvalid, project, "", "");
    if text.len() > MAX_LOCK_BYTES {
        return Err(invalid());
    }
    let lock = text_json(text).map_err(|_| invalid())?;
    read_lock_value(project, &lock)
}

fn read_lock_value(project: &str, lock: &Value) -> Result<Vec<LockedPackage>, Refusal> {
    let invalid = || refuse(RefusalKind::BunLockInvalid, project, "", "");
    if !matches!(
        lock.get("lockfileVersion").and_then(Value::as_u64),
        Some(1 | 2)
    ) {
        return Err(invalid());
    }
    let rows = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    if rows.len() > 100_000 {
        return Err(invalid());
    }
    let mut packages = Vec::new();
    for (placement, row) in rows {
        let row = row.as_array().ok_or_else(invalid)?;
        let resolution = row.first().and_then(Value::as_str).ok_or_else(invalid)?;
        let (name, version) = resolution_parts(resolution).ok_or_else(invalid)?;
        if !valid_name(name) {
            return Err(invalid());
        }
        let fail = |kind| refuse(kind, project, name, version);
        if version.starts_with("file:")
            || version.starts_with("portal:")
            || version.starts_with("workspace:")
            || version.starts_with("link:")
        {
            // Containment is checked against the tracked snapshot by the host.
            continue;
        }
        if !version.bytes().next().is_some_and(|b| b.is_ascii_digit())
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
        {
            return Err(fail(RefusalKind::BunSourceUnsupported));
        }
        let url = row
            .get(1)
            .and_then(Value::as_str)
            .ok_or_else(|| fail(RefusalKind::BunSourceUnsupported))?;
        if !url.is_empty() && !url.starts_with("https://") {
            return Err(fail(RefusalKind::BunSourceUnsupported));
        }
        let metadata = row.get(2).filter(|v| v.is_object()).ok_or_else(invalid)?;
        for field in ["os", "cpu", "libc"] {
            if let Some(value) = metadata.get(field)
                && !value.is_string()
                && !value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string))
            {
                return Err(invalid());
            }
        }
        let sri = row
            .get(3)
            .and_then(Value::as_str)
            .ok_or_else(|| fail(RefusalKind::BunIntegrityMissing))?;
        let integrity =
            Integrity::parse(sri).ok_or_else(|| fail(RefusalKind::BunIntegrityMissing))?;
        if row.len() != 4 {
            return Err(invalid());
        }
        let bins = match &metadata["bin"] {
            Value::String(s) => vec![s.clone()],
            Value::Object(map) => map
                .values()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        packages.push(LockedPackage {
            package: Package {
                placement: placement.clone(),
                name: name.into(),
                version: version.into(),
                integrity,
                windows: platform(metadata, "os", "win32") && platform(metadata, "cpu", "x64"),
                bins,
                bin_dir: metadata
                    .get("binDir")
                    .filter(|_| metadata.get("bin").is_none())
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            url: url.into(),
        });
    }
    let reachable = graph::reachable(lock, rows).map_err(|_| invalid())?;
    for row in &mut packages {
        row.package.windows &= reachable.contains(&row.package.placement);
    }
    Ok(packages)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub path: String,
    pub lock_sha256: String,
    pub manifest_sha256: String,
    pub name: String,
    pub version: String,
    pub packages: Vec<Package>,
    pub skipped: Option<Refusal>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub bun_version: String,
    pub projects: Vec<Project>,
    pub registries: BTreeMap<String, String>,
}

pub const SEED_DIR: &str = "C:\\motor-bun-seed";

pub(crate) fn validate_seed_member(path: &str, directory: bool) -> io::Result<()> {
    let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
    let valid = match parts.as_slice() {
        ["bun-seed"] | ["bun-seed", "blobs"] => directory,
        ["bun-seed", "blobs", "sha256" | "sha512"] => directory,
        ["bun-seed", "manifest.json"] => !directory,
        ["bun-seed", "blobs", algorithm, digest] => {
            !directory
                && Integrity {
                    algorithm: (*algorithm).into(),
                    hex: (*digest).into(),
                }
                .valid()
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::other("invalid Bun seed namespace member"))
    }
}

/// The unpatched npm cache key matches Bun 1.4.4's package lookup convention.
/// Patched entries are derived by Bun in the disposable cache, never in the seed.
pub fn cache_key(package: &Package, registries: &BTreeMap<String, String>) -> io::Result<String> {
    let invalid = || io::Error::other("unsupported Bun cache version or registry");
    let version = package.version.trim_start_matches(['v', '=', ' ']);
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(v, b)| (v, Some(b)));
    let mut core = version.splitn(3, '.');
    let major = core
        .next()
        .ok_or_else(invalid)?
        .parse::<u64>()
        .map_err(|_| invalid())?;
    let minor = core
        .next()
        .ok_or_else(invalid)?
        .parse::<u64>()
        .map_err(|_| invalid())?;
    let rest = core.next().ok_or_else(invalid)?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let patch = rest[..end].parse::<u64>().map_err(|_| invalid())?;
    let pre = rest[end..].strip_prefix('-').unwrap_or(&rest[end..]);
    let mut key = format!("{}@{major}.{minor}.{patch}", package.name);
    if !pre.is_empty() {
        key.push_str(&format!("-{:016x}", hash::wyhash11(pre.as_bytes())));
    }
    if let Some(build) = build {
        key.push_str(&format!("+{:016X}", hash::wyhash11(build.as_bytes())));
    }
    let scope = package
        .name
        .split('/')
        .next()
        .filter(|name| name.starts_with('@'));
    if let Some(registry) = scope.and_then(|scope| registries.get(scope)) {
        let url = url::Url::parse(registry).map_err(|_| invalid())?;
        let host = url.host_str().ok_or_else(invalid)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        let visible = if host.len() > 32 { &host[..12] } else { host };
        key.push_str(&format!("@@{visible}"));
        if visible != host || url.port().is_some() || url.path().len() > 1 {
            key.push_str(&format!(
                "__{:016x}",
                hash::wyhash11(url.as_str().as_bytes())
            ));
        }
    }
    key.push_str("@@@1");
    for component in key.split('/') {
        crate::validate_component(component)?;
    }
    Ok(key)
}

/// Only the seed namespace is extracted with service privileges. Checkout members
/// remain inert here and are later extracted under the job's impersonation token.
pub fn unpack_seed(
    reader: &mut (impl io::Read + io::Seek),
    seed: &std::path::Path,
) -> io::Result<()> {
    crate::validate_tree(reader)?;
    #[cfg(windows)]
    protection::check(seed, 0xA01200A9)?; // GENERIC_READ/EXECUTE plus FILE_GENERIC_READ/EXECUTE.
    let metadata = std::fs::symlink_metadata(seed)?;
    if !metadata.is_dir()
        || crate::is_reparse(&metadata)
        || std::fs::read_dir(seed)?.next().is_some()
    {
        return Err(io::Error::other(
            "Bun seed root must be a fresh physical baseline directory",
        ));
    }
    for entry in tar::Archive::new(reader).entries()? {
        let mut entry = entry?;
        let raw = entry.path_bytes();
        let path = std::str::from_utf8(&raw).map_err(|_| io::Error::other("non-UTF8 seed path"))?;
        let Some(relative) = path.trim_end_matches('/').strip_prefix("bun-seed/") else {
            continue;
        };
        let components: Vec<_> = relative.split('/').collect();
        let mut dest = seed.to_path_buf();
        for (index, component) in components.iter().enumerate() {
            dest.push(component);
            if index + 1 < components.len() || entry.header().entry_type().is_dir() {
                crate::safe_directory(&dest)?;
            }
        }
        if entry.header().entry_type().is_file() {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)?;
            io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}

/// Splits pax extended-header data into `(key, value)` records, each written
/// as `<decimal length> <key>=<value>\n` with the length counting the whole
/// record. Anything malformed is refused rather than skipped.
fn pax_records(mut data: &[u8]) -> io::Result<Vec<(&[u8], &[u8])>> {
    let invalid = || io::Error::other("malformed npm pax header");
    let mut records = Vec::new();
    while !data.is_empty() {
        let space = data.iter().position(|b| *b == b' ').ok_or_else(invalid)?;
        let length: usize = std::str::from_utf8(&data[..space])
            .ok()
            .and_then(|n| n.parse().ok())
            .filter(|n| *n > space + 1 && *n <= data.len())
            .ok_or_else(invalid)?;
        let record = &data[space + 1..length];
        let record = record.strip_suffix(b"\n").ok_or_else(invalid)?;
        let equals = record.iter().position(|b| *b == b'=').ok_or_else(invalid)?;
        records.push((&record[..equals], &record[equals + 1..]));
        data = &data[length..];
    }
    Ok(records)
}

fn projection_error(project: &str, package: &Package) -> Refusal {
    refuse(
        RefusalKind::BunProjectionInvalid,
        project,
        &package.name,
        &package.version,
    )
}

/// Preflight the full tar before writing: implicit directory prefixes also take
/// part in collision detection, and neither links nor tar permissions are used.
pub fn expand_tarball(bytes: &[u8], destination: &std::path::Path) -> io::Result<()> {
    expand_tarball_with_budget(bytes, destination, &ExpansionBudget::default())
}
pub fn expand_tarball_with_budget(
    bytes: &[u8],
    destination: &std::path::Path,
    budget: &ExpansionBudget,
) -> io::Result<()> {
    let mut raw = tar::Archive::new(budget.reader(flate2::read::GzDecoder::new(bytes), true));
    // A pax `size` record overrides the next member's header size. npm's tar
    // writes one beside every long `path` record, equal to the header size, so
    // an equal size is accepted; a differing one would make this raw pass and
    // the later pax-aware passes read different member lengths.
    let mut pax_size: Option<u64> = None;
    for entry in raw.entries()?.raw(true) {
        let mut entry = entry?;
        let kind = entry.header().entry_type().as_byte();
        let metadata = matches!(kind, b'L' | b'K' | b'x' | b'g');
        budget.entry(metadata.then_some(entry.size()))?;
        if !metadata
            && !entry.header().entry_type().is_file()
            && !entry.header().entry_type().is_dir()
        {
            return Err(io::Error::other("npm link or special file"));
        }
        if metadata {
            use std::io::Read;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if matches!(kind, b'x' | b'g') {
                for (key, value) in pax_records(&bytes)? {
                    match key {
                        b"size" if kind == b'x' => {
                            let size = std::str::from_utf8(value)
                                .ok()
                                .and_then(|v| v.parse::<u64>().ok())
                                .ok_or_else(|| io::Error::other("invalid npm pax size"))?;
                            pax_size = Some(size);
                        }
                        _ if key == b"size"
                            || key.starts_with(b"GNU.sparse")
                            || key == b"SCHILY.realsize" =>
                        {
                            return Err(io::Error::other(
                                "unsupported npm size or sparse metadata",
                            ));
                        }
                        _ => {}
                    }
                }
            }
        } else {
            if pax_size
                .take()
                .is_some_and(|size| size != entry.header().size().unwrap_or(u64::MAX))
            {
                return Err(io::Error::other("npm pax size differs from its header"));
            }
            io::copy(&mut entry, &mut io::sink())?;
        }
    }
    if pax_size.is_some() {
        return Err(io::Error::other("npm pax size without a member"));
    }
    io::copy(&mut raw.into_inner(), &mut io::sink())?;
    // `.` components name the same folder and are dropped: several published
    // packages (agent-base 7.1.4 and its proxy-agent siblings) pack
    // `package/./dist/index.js` beside an identical `package/dist/index.js`.
    // `..` and every other invalid component stay refused.
    fn relative(path: &str, directory: bool) -> io::Result<Option<String>> {
        let path = path.trim_end_matches('/');
        let (prefix, rest) = path
            .split_once('/')
            .map_or((path, None), |(p, r)| (p, Some(r)));
        crate::validate_component(prefix)?;
        let rest = rest.map(|rest| {
            rest.split('/')
                .filter(|component| *component != ".")
                .collect::<Vec<_>>()
                .join("/")
        });
        match rest {
            Some(rest) if !rest.is_empty() => Ok(Some(rest)),
            _ if directory => Ok(None),
            _ => Err(io::Error::other("npm member has no archive prefix")),
        }
    }
    let mut names = BTreeMap::<String, (String, bool)>::new();
    let mut files = std::collections::BTreeSet::new();
    for entry in
        tar::Archive::new(budget.reader(flate2::read::GzDecoder::new(bytes), false)).entries()?
    {
        let entry = entry?;
        let raw = entry.path_bytes();
        let path = std::str::from_utf8(&raw).map_err(|_| io::Error::other("non-UTF8 npm path"))?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(io::Error::other("npm link or special file"));
        }
        let Some(relative) = relative(path, kind.is_dir())? else {
            continue;
        };
        let mut prefix = String::new();
        for component in relative.split('/') {
            crate::validate_component(component)?;
            if component.encode_utf16().count() > 255 || relative.encode_utf16().count() > 32_700 {
                return Err(io::Error::other("Bun Windows path length"));
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let file = kind.is_file() && prefix == relative;
            if !names.contains_key(&prefix.to_uppercase()) {
                budget.path(prefix.len())?;
            }
            if let Some((previous, was_file)) =
                names.insert(prefix.to_uppercase(), (prefix.clone(), file))
            {
                // The same file named twice is checked byte for byte when
                // extracted; any other overlap (case, file over folder) is not.
                let same_file = previous == prefix && was_file && file;
                if !same_file && (previous != prefix || was_file || file) {
                    return Err(io::Error::other("npm path collision"));
                }
            }
        }
        files.insert(relative);
        if files.len() > 100_000 {
            return Err(io::Error::other("npm entry limit"));
        }
    }
    if !files.contains("package.json") {
        return Err(io::Error::other("npm package.json missing"));
    }
    std::fs::create_dir_all(destination)?;
    // The validated index includes implicit parents and repeated directory headers.
    // Create each directory once, not once per file (costly CreateFile probes on NTFS).
    for (path, file) in names.values() {
        if !file {
            budget.check()?;
            std::fs::create_dir_all(destination.join(path))?;
        }
    }
    for entry in
        tar::Archive::new(budget.reader(flate2::read::GzDecoder::new(bytes), false)).entries()?
    {
        use std::io::Read as _;
        let mut entry = entry?;
        let path = entry.path_bytes();
        let Some(relative) = relative(
            std::str::from_utf8(&path).unwrap(),
            entry.header().entry_type().is_dir(),
        )?
        else {
            continue;
        };
        let dest = destination.join(relative);
        if entry.header().entry_type().is_file() {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)
            {
                Ok(mut out) => {
                    io::copy(&mut entry, &mut out)?;
                }
                // A repeated member was written earlier from this archive;
                // keep it only if this copy is byte-identical.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let mut again = Vec::new();
                    entry.read_to_end(&mut again)?;
                    if std::fs::read(&dest)? != again {
                        return Err(io::Error::other("npm duplicate member differs"));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn read_bounded(path: &std::path::Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let file = crate::open_source_file(path)?;
    if file.metadata()?.len() > limit {
        return Err(io::Error::other("Bun file limit"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("Bun file limit"));
    }
    Ok(bytes)
}

/// Directories holding a lockfile, keyed by project path, each with the
/// enclosing project whose own lockfile lists it as a workspace member.
pub fn source_projects(
    source: &std::path::Path,
    budget: &ExpansionBudget,
) -> Result<std::collections::BTreeMap<String, Option<String>>, Refusal> {
    fn walk(
        path: &std::path::Path,
        source: &std::path::Path,
        projects: &mut std::collections::BTreeMap<String, Option<String>>,
        budget: &ExpansionBudget,
    ) -> Result<(), Refusal> {
        let invalid = |step, path: &std::path::Path| {
            refuse(RefusalKind::BunProjectionInvalid, "", "", "")
                .detail(path_detail(step, source, path))
        };
        for entry in std::fs::read_dir(path)
            .map_err(|_| invalid("open source for Bun project discovery", path))?
        {
            budget
                .check()
                .map_err(|_| invalid("Bun project discovery deadline", path))?;
            let entry = entry
                .map_err(|_| invalid("read directory entry for Bun project discovery", path))?;
            if is_node_modules(&entry.file_name()) {
                continue;
            }
            let entry_path = entry.path();
            let metadata = std::fs::symlink_metadata(&entry_path).map_err(|_| {
                invalid(
                    "inspect source entry for Bun project discovery",
                    &entry_path,
                )
            })?;
            if crate::is_reparse(&metadata) {
                return Err(invalid("reparse point in Bun source", &entry_path));
            }
            if metadata.is_dir() {
                walk(&entry_path, source, projects, budget)?;
            } else if entry.file_name() == "bun.lock" || entry.file_name() == "bun.lockb" {
                let relative = path
                    .strip_prefix(source)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let project = if relative.is_empty() {
                    "m".into()
                } else {
                    format!("m/{relative}")
                };
                if entry.file_name() == "bun.lockb" {
                    return Err(refuse(RefusalKind::BunLockInvalid, &project, "", "")
                        .detail("unsupported binary lockfile bun.lockb"));
                }
                projects.insert(project, None);
            }
        }
        Ok(())
    }
    let mut projects = Default::default();
    walk(source, source, &mut projects, budget)?;
    let paths: Vec<String> = projects.keys().cloned().collect();
    // Bun stops at the nearest ancestor manifest declaring workspaces, even
    // when it does not include this directory. A nearer standalone lockfile
    // does not stop that search. Host and guest share this ownership decision.
    for project in &paths {
        let member = source.join(project.strip_prefix("m/").unwrap_or(""));
        let mut ancestor = member.parent();
        while let Some(directory) = ancestor.filter(|path| path.starts_with(source)) {
            budget.check().map_err(|_| {
                refuse(RefusalKind::BunProjectionInvalid, project, "", "").detail(path_detail(
                    "Bun workspace discovery deadline",
                    source,
                    directory,
                ))
            })?;
            let manifest = directory.join("package.json");
            let declares_workspaces = if manifest.is_file() {
                let invalid = |step| {
                    refuse(RefusalKind::BunLockInvalid, project, "", "")
                        .detail(path_detail(step, source, &manifest))
                };
                let bytes = read_bounded(&manifest, MAX_LOCK_BYTES as u64)
                    .map_err(|_| invalid("read enclosing workspace manifest"))?;
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| invalid("parse enclosing workspace manifest"))?;
                value.get("workspaces").is_some()
            } else {
                false
            };
            if declares_workspaces {
                let relative = directory
                    .strip_prefix(source)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let owner = if relative.is_empty() {
                    "m".into()
                } else {
                    format!("m/{relative}")
                };
                if paths.contains(&owner) && lists_member(source, directory, &member, budget)? {
                    projects.insert(project.clone(), Some(owner));
                }
                break;
            }
            ancestor = directory.parent();
        }
    }
    Ok(projects)
}

/// Frozen installs use the resolved workspace table in the enclosing lockfile.
/// Reading its exact member paths avoids implementing a second, subtly different
/// version of Bun's workspace glob expansion. Bun checks manifest drift during
/// the frozen install; an unused member lockfile must not be parsed as a project.
fn lists_member(
    source: &std::path::Path,
    directory: &std::path::Path,
    member: &std::path::Path,
    budget: &ExpansionBudget,
) -> Result<bool, Refusal> {
    let invalid = |step, path: &std::path::Path| {
        refuse(RefusalKind::BunProjectionInvalid, "", "", "")
            .detail(path_detail(step, source, path))
    };
    let lock_path = directory.join("bun.lock");
    budget
        .check()
        .map_err(|_| invalid("workspace membership check deadline", &lock_path))?;
    let bytes = read_bounded(&lock_path, MAX_LOCK_BYTES as u64)
        .map_err(|_| invalid("read enclosing workspace lockfile", &lock_path))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("decode enclosing workspace lockfile as UTF-8", &lock_path))?;
    let lock: Value =
        text_json(text).map_err(|_| invalid("parse enclosing workspace lockfile", &lock_path))?;
    let relative = member
        .strip_prefix(directory)
        .map_err(|_| {
            invalid(
                "workspace member is not under its enclosing project",
                member,
            )
        })?
        .to_str()
        .ok_or_else(|| invalid("decode workspace member path as UTF-8", member))?
        .replace('\\', "/");
    let Some(name) = lock
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(&relative))
        .and_then(|workspace| workspace.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return Ok(false);
    };
    let manifest = member.join("package.json");
    let bytes = read_bounded(&manifest, MAX_LOCK_BYTES as u64)
        .map_err(|_| invalid("read workspace member manifest", &manifest))?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("parse workspace member manifest", &manifest))?;
    Ok(manifest.get("name").and_then(Value::as_str) == Some(name))
}

/// Report a lockfile that an enclosing workspace project already installs. Bun
/// reads only the enclosing lockfile there, so the member's own file is unused.
pub fn workspace_member_warning(project: &str, owner: &str) -> String {
    format!(
        "bun.lock unused at {}/bun.lock: inside workspace member of {}; Bun installs it from there\n",
        project.strip_prefix("m/").unwrap_or(project),
        owner.strip_prefix("m/").unwrap_or(owner),
    )
}

fn path_detail(step: &str, root: &std::path::Path, path: &std::path::Path) -> String {
    detail_location(step, &installed_relative(root, path))
}

fn detail_location(step: &str, relative: &str) -> String {
    let mut detail = format!("{step} at ");
    for c in relative.chars() {
        if detail.len() + c.len_utf8() > 4096 {
            break;
        }
        detail.push(if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            '?'
        } else {
            c
        });
    }
    detail
}

pub fn prepare_cache(
    seed: &std::path::Path,
    source: &std::path::Path,
    cache: &std::path::Path,
) -> Result<Option<Manifest>, Refusal> {
    prepare_cache_with_budget(seed, source, cache, &ExpansionBudget::default())
}

struct DisposableCache<'a> {
    path: &'a std::path::Path,
    keep: bool,
}
impl Drop for DisposableCache<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(self.path);
        }
    }
}

pub fn prepare_cache_with_budget(
    seed: &std::path::Path,
    source: &std::path::Path,
    cache: &std::path::Path,
    budget: &ExpansionBudget,
) -> Result<Option<Manifest>, Refusal> {
    let invalid = |check, detail: &str| {
        refuse(RefusalKind::BunProjectionInvalid, "", "", "")
            .at(check)
            .detail(detail)
    };
    budget.check().map_err(|_| {
        invalid(
            Check::SeedBudget,
            "cache preparation deadline before scanning source root (m/)",
        )
    })?;
    // Members are prepared by the enclosing project, so the host publishes no
    // manifest row of their own.
    let input_projects: std::collections::BTreeSet<String> = source_projects(source, budget)?
        .into_iter()
        .filter(|(_, owner)| owner.is_none())
        .map(|(project, _)| project)
        .collect();
    let bytes = match read_bounded(&seed.join("manifest.json"), MAX_LOCK_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(project) = input_projects.first() {
                return Err(refuse(RefusalKind::BunCacheMissing, project, "", "")
                    .at(Check::SeedManifest)
                    .detail("missing bun-seed/manifest.json"));
            }
            return Ok(None);
        }
        Err(_) => return Err(invalid(Check::SeedManifest, "read bun-seed/manifest.json")),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        invalid(
            Check::SeedManifest,
            "decode bun-seed/manifest.json as UTF-8",
        )
    })?;
    let json = text_json(text)
        .map_err(|_| invalid(Check::SeedManifest, "parse bun-seed/manifest.json as JSON"))?;
    let mut manifest: Manifest = serde_json::from_value(json)
        .map_err(|_| invalid(Check::SeedManifest, "decode bun-seed/manifest.json fields"))?;
    if manifest.format != 1 {
        return Err(invalid(
            Check::SeedManifest,
            "unsupported bun-seed/manifest.json format",
        ));
    }
    if manifest.bun_version != BUN_VERSION {
        return Err(invalid(
            Check::SeedManifest,
            "unsupported bun-seed/manifest.json bun_version",
        ));
    }
    if manifest.projects.is_empty() || manifest.projects.len() > 1000 {
        return Err(invalid(
            Check::SeedManifest,
            "bun-seed/manifest.json projects count outside 1..=1000",
        ));
    }
    if manifest
        .projects
        .iter()
        .map(|project| project.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        != input_projects
    {
        return Err(invalid(
            Check::SeedManifest,
            "bun-seed/manifest.json projects[].path does not match source lockfiles",
        ));
    }
    let expected_blobs: std::collections::BTreeSet<_> = manifest
        .projects
        .iter()
        .flat_map(|project| {
            project
                .packages
                .iter()
                .filter(|p| p.windows)
                .map(|package| package.integrity.blob_path())
        })
        .collect();
    let blobs = seed.join("blobs");
    let blob_error = |step, path: &std::path::Path| {
        refuse(RefusalKind::BunProjectionInvalid, "", "", "")
            .at(Check::SeedBlobs)
            .detail(detail_location(
                step,
                &format!("bun-seed/{}", installed_relative(seed, path)),
            ))
    };
    if blobs.exists() {
        for algorithm in
            std::fs::read_dir(&blobs).map_err(|_| blob_error("list blob algorithms", &blobs))?
        {
            let algorithm =
                algorithm.map_err(|_| blob_error("read blob algorithm entry", &blobs))?;
            let directory = algorithm.path();
            for blob in std::fs::read_dir(&directory)
                .map_err(|_| blob_error("list blobs for algorithm", &directory))?
            {
                let blob = blob.map_err(|_| blob_error("read blob entry", &directory))?;
                let relative = blob
                    .path()
                    .strip_prefix(seed)
                    .map_err(|_| blob_error("blob path is not relative to the seed", &directory))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !expected_blobs.contains(&relative) {
                    return Err(blob_error(
                        "blob is not listed in the manifest",
                        &blob.path(),
                    ));
                }
            }
        }
    }
    // A fresh cache cannot contain stale or attacker-supplied junctions. It is
    // intentionally writable because Bun applies patches and creates temp files.
    std::fs::create_dir(cache)
        .map_err(|_| invalid(Check::CachePublish, "create fresh bun-cache directory"))?;
    let mut cleanup = DisposableCache {
        path: cache,
        keep: false,
    };
    let mut keys = BTreeMap::<String, Integrity>::new();
    let mut projects = std::collections::BTreeSet::new();
    for project in &mut manifest.projects {
        let invalid = |detail: &str| {
            refuse(
                RefusalKind::BunProjectionInvalid,
                &project.path,
                &project.name,
                &project.version,
            )
            .detail(detail)
        };
        crate::validate_archive_path(&project.path).map_err(|_| {
            invalid("invalid archive path in bun-seed/manifest.json projects[].path")
        })?;
        let relative = if project.path == "m" {
            ""
        } else {
            project.path.strip_prefix("m/").ok_or_else(|| {
                invalid("path outside m/ in bun-seed/manifest.json projects[].path")
            })?
        };
        if !projects.insert(project.path.to_uppercase()) {
            return Err(invalid(
                "duplicate path in bun-seed/manifest.json projects[].path",
            ));
        }
        let directory = source.join(relative);
        let lock = read_bounded(&directory.join("bun.lock"), MAX_LOCK_BYTES as u64)
            .map_err(|_| invalid("read bun.lock"))?;
        if hex(&Sha256::digest(&lock)) != project.lock_sha256 {
            return Err(invalid(
                "bun.lock digest does not match projects[].lock_sha256",
            ));
        }
        let package_json = read_bounded(&directory.join("package.json"), MAX_LOCK_BYTES as u64)
            .map_err(|_| invalid("read package.json"))?;
        if hex(&Sha256::digest(&package_json)) != project.manifest_sha256 {
            return Err(invalid(
                "package.json digest does not match projects[].manifest_sha256",
            ));
        }
        let text = std::str::from_utf8(&lock).map_err(|_| invalid("decode bun.lock as UTF-8"))?;
        let lock = text_json(text).map_err(|_| {
            refuse(RefusalKind::BunLockInvalid, &project.path, "", "")
                .detail("parse bun.lock as JSON")
        })?;
        let locked = read_lock_value(&project.path, &lock).map_err(|refusal| {
            refusal.detail("validate locked package rows and workspaces in bun.lock")
        })?;
        install_roots::check_inputs(source, &directory, &lock, budget)
            .map_err(|detail| invalid(&detail))?;
        let expected: Vec<_> = locked.into_iter().map(|p| p.package).collect();
        if serde_json::to_value(expected)
            .map_err(|_| invalid("encode expected packages from bun.lock"))?
            != serde_json::to_value(&project.packages)
                .map_err(|_| invalid("encode bun-seed/manifest.json projects[].packages"))?
        {
            return Err(invalid(
                "bun-seed/manifest.json projects[].packages does not match bun.lock",
            ));
        }
        if let Some(refusal) = &project.skipped {
            refusal
                .validate()
                .map_err(|_| invalid("invalid bun-seed/manifest.json projects[].skipped fields"))?;
            if refusal.kind.is_security() {
                return Err(invalid(
                    "security refusal in bun-seed/manifest.json projects[].skipped.kind",
                ));
            }
            if refusal.project != project.path {
                return Err(invalid(
                    "bun-seed/manifest.json projects[].skipped.project does not match projects[].path",
                ));
            }
            continue;
        }
        // Do not expand a prefix of an incomplete project. Bun's offline lookup
        // can otherwise reach only direct dependencies and fail on a later edge.
        let missing: Vec<_> = project
            .packages
            .iter()
            .filter(|p| p.windows)
            .filter(|p| !seed.join(p.integrity.blob_path()).is_file())
            .take(5)
            .map(|p| MissingPackage {
                name: p.name.clone(),
                version: p.version.clone(),
            })
            .collect();
        if let Some(first) = missing.first() {
            project.skipped = Some(
                refuse(
                    RefusalKind::BunCacheMissing,
                    &project.path,
                    &first.name,
                    &first.version,
                )
                .at(Check::SeedBlobs)
                .detail("required Windows package missing from bun-seed/blobs")
                .registry(&manifest.registries)
                .missing_packages(missing),
            );
            continue;
        }
        let mut created_keys = Vec::new();
        for package in &project.packages {
            let fail = || projection_error(&project.path, package).registry(&manifest.registries);
            if !package.integrity.valid() {
                return Err(
                    fail().detail("invalid bun-seed/manifest.json projects[].packages[].integrity")
                );
            }
            if !package.windows {
                continue;
            }
            let bytes =
                match read_bounded(&seed.join(package.integrity.blob_path()), MAX_BLOB_BYTES) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        project.skipped = Some(
                            refuse(
                                RefusalKind::BunCacheMissing,
                                &project.path,
                                &package.name,
                                &package.version,
                            )
                            .at(Check::CacheRead)
                            .detail(format!("read bun-seed/{}", package.integrity.blob_path()))
                            .registry(&manifest.registries)
                            .missing_packages([MissingPackage {
                                name: package.name.clone(),
                                version: package.version.clone(),
                            }]),
                        );
                        break;
                    }
                };
            if !package.integrity.verify(&bytes) {
                return Err(refuse(
                    RefusalKind::BunIntegrityMismatch,
                    &project.path,
                    &package.name,
                    &package.version,
                )
                .registry(&manifest.registries)
                .detail(format!(
                    "verify digest of bun-seed/{}",
                    package.integrity.blob_path()
                )));
            }
            let key = cache_key(package, &manifest.registries)
                .map_err(|_| fail().detail("derive Bun cache key from bun-seed/manifest.json package and registries fields"))?;
            if let Some(previous) = keys.insert(key.to_uppercase(), package.integrity.clone()) {
                if previous != package.integrity {
                    return Err(fail().detail(detail_location(
                        "conflicting integrity for cache entry",
                        &format!("bun-cache/{key}"),
                    )));
                }
                continue;
            }
            let destination = cache.join(&key);
            expand_tarball_with_budget(&bytes, &destination, budget).map_err(|_| {
                fail().detail(format!(
                    "expand archive from bun-seed/{}",
                    package.integrity.blob_path()
                ))
            })?;
            let metadata_path = format!("bun-cache/{key}/package.json");
            let metadata: Value = serde_json::from_slice(
                &read_bounded(&destination.join("package.json"), MAX_LOCK_BYTES as u64).map_err(
                    |_| {
                        fail().detail(detail_location(
                            "read expanded package metadata",
                            &metadata_path,
                        ))
                    },
                )?,
            )
            .map_err(|_| {
                fail().detail(detail_location(
                    "parse expanded package metadata",
                    &metadata_path,
                ))
            })?;
            if metadata["name"] != package.name {
                return Err(fail().detail(detail_location(
                    "expanded package name mismatch",
                    &metadata_path,
                )));
            }
            if metadata["version"] != package.version {
                return Err(fail().detail(detail_location(
                    "expanded package version mismatch",
                    &metadata_path,
                )));
            }
            created_keys.push(key);
        }
        // A bounded read can still fail after the existence preflight. Preserve
        // entries shared with earlier complete projects, not this failed prefix.
        if project.skipped.is_some() {
            for key in created_keys {
                std::fs::remove_dir_all(cache.join(&key)).map_err(|_| {
                    invalid(&detail_location(
                        "remove partial cache entry",
                        &format!("bun-cache/{key}"),
                    ))
                })?;
                keys.remove(&key.to_uppercase());
                if let Some((scope, _)) = key.split_once('/') {
                    let directory = cache.join(scope);
                    if std::fs::read_dir(&directory)
                        .map_err(|_| {
                            invalid(&detail_location(
                                "list partial cache scope",
                                &format!("bun-cache/{scope}"),
                            ))
                        })?
                        .next()
                        .is_none()
                    {
                        std::fs::remove_dir(directory).map_err(|_| {
                            invalid(&detail_location(
                                "remove empty cache scope",
                                &format!("bun-cache/{scope}"),
                            ))
                        })?;
                    }
                }
            }
        }
    }
    manifest.projects.sort_by(|a, b| {
        (a.path.split('/').count(), &a.path).cmp(&(b.path.split('/').count(), &b.path))
    });
    cleanup.keep = true;
    Ok(Some(manifest))
}

/// The job checkout an installed tree must stay inside. Every path a refusal
/// names is relative to it, so a live report says where the install left its
/// tree.
struct Directory {
    directory: std::path::PathBuf,
    root: std::path::PathBuf,
}

/// An install check that failed: either it left the checkout, or it could not
/// be read. The caller keeps the distinction in the refusal kind.
enum Containment {
    Outside(String),
    Unreadable(String),
}

impl Containment {
    fn into_refusal(self, project: &Project) -> Refusal {
        match self {
            Containment::Outside(detail) => {
                refuse(RefusalKind::BunProjectionInvalid, &project.path, "", "").detail(format!(
                    "Projection: installed tree leaves the checkout at {detail}"
                ))
            }
            Containment::Unreadable(detail) => {
                refuse(RefusalKind::BunOfflineInstallFailed, &project.path, "", "").detail(format!(
                    "InstalledTree: installed tree is unreadable at {detail}"
                ))
            }
        }
    }
}

impl Directory {
    fn new(directory: &std::path::Path, project: &str) -> Result<Self, Refusal> {
        let root = directory.canonicalize().map_err(|_| {
            refuse(RefusalKind::BunOfflineInstallFailed, project, "", "")
                .at(Check::InstalledTree)
                .detail("InstalledTree: checkout is unreadable at .")
        })?;
        Ok(Self {
            directory: directory.into(),
            root,
        })
    }

    /// Resolve `path`, refusing a link whose target leaves the checkout.
    fn inside(&self, path: &std::path::Path) -> Result<std::path::PathBuf, Containment> {
        let relative = self.relative(path);
        let canonical = path
            .canonicalize()
            .map_err(|_| Containment::Unreadable(relative.clone()))?;
        if !canonical.starts_with(&self.root) {
            return Err(Containment::Outside(relative));
        }
        Ok(canonical)
    }

    fn failed(&self, path: &std::path::Path, error: io::Error) -> Containment {
        let _ = error;
        Containment::Unreadable(self.relative(path))
    }

    fn relative(&self, path: &std::path::Path) -> String {
        installed_relative(&self.directory, path)
    }
}

/// Install walkers retain lexical entry paths so diagnostics name the link
/// encountered in the checkout, not its resolved target or a guest disk path.
fn installed_relative(directory: &std::path::Path, path: &std::path::Path) -> String {
    let relative = path
        .strip_prefix(directory)
        .unwrap_or(std::path::Path::new("."));
    if relative.as_os_str().is_empty() {
        return ".".into();
    }
    relative.to_string_lossy().replace('\\', "/")
}

/// A successful offline install may omit optional packages. Check identities and
/// declared bin targets independently instead of treating exit zero as completeness.
pub fn verify_installed(project: &Project, directory: &std::path::Path) -> Result<(), Refusal> {
    fn walk_modules(
        path: &std::path::Path,
        root: &Directory,
        seen: &mut std::collections::BTreeSet<std::path::PathBuf>,
        installed: &mut Vec<(Value, std::path::PathBuf)>,
    ) -> Result<(), Containment> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(root.failed(path, error)),
        }
        let canonical = root.inside(path)?;
        if !seen.insert(canonical) {
            return Ok(());
        }
        for entry in std::fs::read_dir(path).map_err(|error| root.failed(path, error))? {
            let entry = entry.map_err(|error| root.failed(path, error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let target = entry.path();
            root.inside(&target)?;
            if name == ".bun" {
                for store in
                    std::fs::read_dir(&target).map_err(|error| root.failed(&target, error))?
                {
                    let store = store.map_err(|error| root.failed(&target, error))?;
                    root.inside(&store.path())?;
                    let modules = if store.file_name() == "node_modules" {
                        store.path()
                    } else {
                        store.path().join("node_modules")
                    };
                    walk_modules(&modules, root, seen, installed)?;
                }
            } else if name.starts_with('@') {
                for package in
                    std::fs::read_dir(&target).map_err(|error| root.failed(&target, error))?
                {
                    let package = package.map_err(|error| root.failed(&target, error))?;
                    collect_package(&package.path(), root, seen, installed)?;
                }
            } else if !name.starts_with('.') && target.is_dir() {
                collect_package(&target, root, seen, installed)?;
            }
        }
        Ok(())
    }
    fn collect_package(
        path: &std::path::Path,
        root: &Directory,
        seen: &mut std::collections::BTreeSet<std::path::PathBuf>,
        installed: &mut Vec<(Value, std::path::PathBuf)>,
    ) -> Result<(), Containment> {
        root.inside(path)?;
        if let Ok(bytes) = read_bounded(&path.join("package.json"), MAX_LOCK_BYTES as u64)
            && let Ok(value) = serde_json::from_slice(&bytes)
        {
            installed.push((value, path.into()));
        }
        walk_modules(&path.join("node_modules"), root, seen, installed)
    }
    let mut installed = Vec::new();
    let boundary = Directory::new(directory, &project.path)?;
    let invalid = || refuse(RefusalKind::BunProjectionInvalid, &project.path, "", "");
    let roots = install_roots::Roots::read(directory, &project.lock_sha256)
        .map_err(|step| invalid().detail(format!("verify install roots: {step}")))?;
    let roots = roots.existing(directory).map_err(|path| {
        invalid().detail(path_detail(
            "verify install root directory",
            directory,
            &path,
        ))
    })?;
    let mut seen = Default::default();
    // A test fixture may contain a package with a locked name and version that
    // Bun never installed. Only the node_modules folders Bun creates count: the
    // project root's and each workspace member's, followed through the links
    // Bun places there to its shared package store.
    for root in roots {
        walk_modules(
            &root.join("node_modules"),
            &boundary,
            &mut seen,
            &mut installed,
        )
        .map_err(|error| error.into_refusal(project))?;
    }
    for package in project.packages.iter().filter(|p| p.windows) {
        let fail = || {
            refuse(
                RefusalKind::BunOfflineInstallFailed,
                &project.path,
                &package.name,
                &package.version,
            )
        };
        let (metadata, root) = installed
            .iter()
            .find(|(metadata, _)| {
                metadata["name"] == package.name && metadata["version"] == package.version
            })
            .ok_or_else(|| {
                fail().at(Check::InstalledPackage).detail(format!(
                    "InstalledPackage: {}@{} is not installed at node_modules",
                    package.name, package.version
                ))
            })?;
        let bins: Vec<&str> = match &metadata["bin"] {
            Value::String(s) => vec![s],
            Value::Object(map) => map.values().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for bin in bins
            .into_iter()
            .chain(package.bins.iter().map(String::as_str))
        {
            let bin = bin.strip_prefix("./").unwrap_or(bin);
            for component in bin.split('/') {
                crate::validate_component(component).map_err(|_| {
                    projection_error(&project.path, package).detail(format!(
                        "Projection: invalid bin target at {}",
                        installed_relative(directory, &root.join(bin))
                    ))
                })?;
            }
            if !root.join(bin).is_file() {
                return Err(fail().at(Check::InstalledBin).detail(format!(
                    "InstalledBin: {} is missing at {}",
                    package.name,
                    installed_relative(directory, &root.join(bin))
                )));
            }
        }
        if let Some(bin_dir) = &package.bin_dir {
            for component in bin_dir.strip_prefix("./").unwrap_or(bin_dir).split('/') {
                crate::validate_component(component).map_err(|_| {
                    projection_error(&project.path, package).detail(format!(
                        "Projection: invalid bin directory at {}",
                        installed_relative(directory, &root.join(bin_dir))
                    ))
                })?;
            }
            if !root.join(bin_dir).is_dir() {
                return Err(fail().at(Check::InstalledBinDirectory).detail(format!(
                    "InstalledBinDirectory: {} is missing at {}",
                    package.name,
                    installed_relative(directory, &root.join(bin_dir))
                )));
            }
        }
    }
    Ok(())
}

pub fn write_registry_config(
    profile: &std::path::Path,
    registries: &BTreeMap<String, String>,
) -> io::Result<()> {
    if registries.is_empty() {
        return Ok(());
    }
    let mut config = String::from("[install.scopes]\n");
    for (scope, registry) in registries {
        if !valid_name(&format!("{scope}/package")) || !scope.starts_with('@') {
            return Err(io::Error::other("invalid registry scope"));
        }
        let url = url::Url::parse(registry).map_err(io::Error::other)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(io::Error::other("invalid registry URL"));
        }
        config.push_str(&format!(
            "{} = {}\n",
            serde_json::to_string(scope)?,
            serde_json::to_string(registry)?
        ));
    }
    std::fs::write(profile.join(".bunfig.toml"), config)
}
