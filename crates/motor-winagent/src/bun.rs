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
mod preparation;
pub use expansion::ExpansionBudget;
pub use preparation::{DiagnosticTail, finish_install, skip_project, skip_unprepared};
#[cfg(windows)]
pub mod protection;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub project: String,
    pub package: String,
    pub version: String,
    pub registry_origin: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}: project={} package={} version={} registry={}",
            self.kind, self.project, self.package, self.version, self.registry_origin
        )
    }
}
impl std::error::Error for Refusal {}
impl Refusal {
    pub fn warning(&self) -> String {
        format!(
            "bun install skipped for {}: {:?} package={}@{}\n",
            self.project, self.kind, self.package, self.version
        )
    }
    pub fn registry(mut self, registries: &BTreeMap<String, String>) -> Self {
        if let Some(registry) = self
            .package
            .split('/')
            .next()
            .and_then(|scope| registries.get(scope))
        {
            if let Ok(url) = url::Url::parse(registry) {
                if url.scheme() == "https" {
                    self.registry_origin = url.origin().ascii_serialization();
                }
            }
        }
        self
    }
    pub fn validate(&self) -> io::Result<()> {
        for field in [
            &self.project,
            &self.package,
            &self.version,
            &self.registry_origin,
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
            String::new()
        },
        registry_origin: if package.is_empty() {
            String::new()
        } else {
            "https://registry.npmjs.org".into()
        },
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
            if let Some(value) = metadata.get(field) {
                if !value.is_string()
                    && !value
                        .as_array()
                        .is_some_and(|a| a.iter().all(Value::is_string))
                {
                    return Err(invalid());
                }
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
    let reachable = graph::reachable(&lock, rows).map_err(|_| invalid())?;
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
    for entry in raw.entries()?.raw(true) {
        let mut entry = entry?;
        let metadata = matches!(
            entry.header().entry_type().as_byte(),
            b'L' | b'K' | b'x' | b'g'
        );
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
            if matches!(entry.header().entry_type().as_byte(), b'x' | b'g')
                && [
                    b"GNU.sparse".as_slice(),
                    b"SCHILY.realsize".as_slice(),
                    b" size=".as_slice(),
                ]
                .iter()
                .any(|key| bytes.windows(key.len()).any(|part| part == *key))
            {
                return Err(io::Error::other("unsupported npm size or sparse metadata"));
            }
        } else {
            io::copy(&mut entry, &mut io::sink())?;
        }
    }
    io::copy(&mut raw.into_inner(), &mut io::sink())?;
    fn relative(path: &str, directory: bool) -> io::Result<Option<&str>> {
        let path = path.trim_end_matches('/');
        let (prefix, rest) = path
            .split_once('/')
            .map_or((path, None), |(p, r)| (p, Some(r)));
        crate::validate_component(prefix)?;
        match rest {
            Some(rest) => Ok(Some(rest)),
            None if directory => Ok(None),
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
                if previous != prefix || was_file || file {
                    return Err(io::Error::other("npm path collision"));
                }
            }
        }
        if !files.insert(relative.to_owned()) || files.len() > 100_000 {
            return Err(io::Error::other("duplicate npm member or entry limit"));
        }
    }
    if !files.contains("package.json") {
        return Err(io::Error::other("npm package.json missing"));
    }
    std::fs::create_dir_all(destination)?;
    for entry in
        tar::Archive::new(budget.reader(flate2::read::GzDecoder::new(bytes), false)).entries()?
    {
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
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(dest)?;
        } else {
            std::fs::create_dir_all(dest.parent().unwrap())?;
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)?;
            io::copy(&mut entry, &mut out)?;
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

fn source_projects(
    source: &std::path::Path,
    budget: &ExpansionBudget,
) -> Result<std::collections::BTreeSet<String>, Refusal> {
    fn walk(
        path: &std::path::Path,
        source: &std::path::Path,
        projects: &mut std::collections::BTreeSet<String>,
        budget: &ExpansionBudget,
    ) -> Result<(), Refusal> {
        let invalid = || refuse(RefusalKind::BunProjectionInvalid, "", "", "");
        for entry in std::fs::read_dir(path).map_err(|_| invalid())? {
            budget.check().map_err(|_| invalid())?;
            let entry = entry.map_err(|_| invalid())?;
            if is_node_modules(&entry.file_name()) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|_| invalid())?;
            if crate::is_reparse(&metadata) {
                return Err(invalid());
            }
            if metadata.is_dir() {
                walk(&entry.path(), source, projects, budget)?;
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
                    return Err(refuse(RefusalKind::BunLockInvalid, &project, "", ""));
                }
                projects.insert(project);
            }
        }
        Ok(())
    }
    let mut projects = Default::default();
    walk(source, source, &mut projects, budget)?;
    Ok(projects)
}

fn clean_install_inputs(directory: &std::path::Path, budget: &ExpansionBudget) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        budget.check()?;
        let entry = entry?;
        // A committed node_modules could make Bun accept unverified files as an
        // existing install. Only the verified seed may supply installed packages.
        if is_node_modules(&entry.file_name()) {
            return Err(io::Error::other("pre-existing node_modules in Bun project"));
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if crate::is_reparse(&metadata) {
            return Err(io::Error::other("reparse point in Bun project"));
        }
        if metadata.is_dir() {
            clean_install_inputs(&entry.path(), budget)?;
        }
    }
    Ok(())
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
    let invalid = || refuse(RefusalKind::BunProjectionInvalid, "", "", "");
    budget.check().map_err(|_| invalid())?;
    let input_projects = source_projects(source, budget)?;
    let bytes = match read_bounded(&seed.join("manifest.json"), MAX_LOCK_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(project) = input_projects.first() {
                return Err(refuse(RefusalKind::BunCacheMissing, project, "", ""));
            }
            return Ok(None);
        }
        Err(_) => return Err(invalid()),
    };
    let mut manifest: Manifest = serde_json::from_value(
        text_json(std::str::from_utf8(&bytes).map_err(|_| invalid())?).map_err(|_| invalid())?,
    )
    .map_err(|_| invalid())?;
    if manifest.format != 1 || manifest.bun_version != BUN_VERSION {
        return Err(invalid());
    }
    if manifest.projects.is_empty() || manifest.projects.len() > 1000 {
        return Err(invalid());
    }
    if manifest
        .projects
        .iter()
        .map(|project| project.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        != input_projects
    {
        return Err(invalid());
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
    if seed.join("blobs").exists() {
        for algorithm in std::fs::read_dir(seed.join("blobs")).map_err(|_| invalid())? {
            let algorithm = algorithm.map_err(|_| invalid())?;
            for blob in std::fs::read_dir(algorithm.path()).map_err(|_| invalid())? {
                let blob = blob.map_err(|_| invalid())?;
                let relative = blob
                    .path()
                    .strip_prefix(seed)
                    .map_err(|_| invalid())?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !expected_blobs.contains(&relative) {
                    return Err(invalid());
                }
            }
        }
    }
    // A fresh cache cannot contain stale or attacker-supplied junctions. It is
    // intentionally writable because Bun applies patches and creates temp files.
    std::fs::create_dir(cache).map_err(|_| invalid())?;
    let mut cleanup = DisposableCache {
        path: cache,
        keep: false,
    };
    let mut keys = BTreeMap::<String, Integrity>::new();
    let mut projects = std::collections::BTreeSet::new();
    for project in &mut manifest.projects {
        let invalid = || {
            refuse(
                RefusalKind::BunProjectionInvalid,
                &project.path,
                &project.name,
                &project.version,
            )
        };
        crate::validate_archive_path(&project.path).map_err(|_| invalid())?;
        let relative = if project.path == "m" {
            ""
        } else {
            project.path.strip_prefix("m/").ok_or_else(invalid)?
        };
        clean_install_inputs(&source.join(relative), budget).map_err(|_| invalid())?;
        if !projects.insert(project.path.to_uppercase()) {
            return Err(invalid());
        }
        let lock = read_bounded(
            &source.join(relative).join("bun.lock"),
            MAX_LOCK_BYTES as u64,
        )
        .map_err(|_| invalid())?;
        if hex(&Sha256::digest(&lock)) != project.lock_sha256 {
            return Err(invalid());
        }
        let package_json = read_bounded(
            &source.join(relative).join("package.json"),
            MAX_LOCK_BYTES as u64,
        )
        .map_err(|_| invalid())?;
        if hex(&Sha256::digest(&package_json)) != project.manifest_sha256 {
            return Err(invalid());
        }
        let locked = read_lock(
            &project.path,
            std::str::from_utf8(&lock).map_err(|_| invalid())?,
        )?;
        let expected: Vec<_> = locked.into_iter().map(|p| p.package).collect();
        if serde_json::to_value(expected).map_err(|_| invalid())?
            != serde_json::to_value(&project.packages).map_err(|_| invalid())?
        {
            return Err(invalid());
        }
        if let Some(refusal) = &project.skipped {
            refusal.validate().map_err(|_| invalid())?;
            if refusal.kind.is_security() || refusal.project != project.path {
                return Err(invalid());
            }
            continue;
        }
        for package in &project.packages {
            let fail = || projection_error(&project.path, package).registry(&manifest.registries);
            if !package.integrity.valid() {
                return Err(fail());
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
                            .registry(&manifest.registries),
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
                .registry(&manifest.registries));
            }
            if !package.windows {
                continue;
            }
            let key = cache_key(package, &manifest.registries).map_err(|_| fail())?;
            if let Some(previous) = keys.insert(key.to_uppercase(), package.integrity.clone()) {
                if previous != package.integrity {
                    return Err(fail());
                }
                continue;
            }
            let destination = cache.join(&key);
            expand_tarball_with_budget(&bytes, &destination, budget).map_err(|_| fail())?;
            let metadata: Value = serde_json::from_slice(
                &read_bounded(&destination.join("package.json"), MAX_LOCK_BYTES as u64)
                    .map_err(|_| fail())?,
            )
            .map_err(|_| fail())?;
            if metadata["name"] != package.name || metadata["version"] != package.version {
                return Err(fail());
            }
        }
    }
    manifest.projects.sort_by(|a, b| {
        (a.path.split('/').count(), &a.path).cmp(&(b.path.split('/').count(), &b.path))
    });
    cleanup.keep = true;
    Ok(Some(manifest))
}

/// A successful offline install may omit optional packages. Check identities and
/// declared bin targets independently instead of treating exit zero as completeness.
pub fn verify_installed(project: &Project, directory: &std::path::Path) -> Result<(), Refusal> {
    fn walk_modules(
        path: &std::path::Path,
        root: &std::path::Path,
        seen: &mut std::collections::BTreeSet<std::path::PathBuf>,
        installed: &mut Vec<(Value, std::path::PathBuf)>,
    ) -> io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(root) {
            return Err(io::Error::other(refuse(
                RefusalKind::BunProjectionInvalid,
                "",
                "",
                "",
            )));
        }
        if !seen.insert(canonical) {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".bun" {
                for store in std::fs::read_dir(entry.path())? {
                    walk_modules(&store?.path().join("node_modules"), root, seen, installed)?;
                }
            } else if name.starts_with('@') {
                for package in std::fs::read_dir(entry.path())? {
                    collect_package(&package?.path(), root, seen, installed)?;
                }
            } else if !name.starts_with('.') && entry.path().is_dir() {
                collect_package(&entry.path(), root, seen, installed)?;
            }
        }
        Ok(())
    }
    fn collect_package(
        path: &std::path::Path,
        root: &std::path::Path,
        seen: &mut std::collections::BTreeSet<std::path::PathBuf>,
        installed: &mut Vec<(Value, std::path::PathBuf)>,
    ) -> io::Result<()> {
        if !path.canonicalize()?.starts_with(root) {
            return Err(io::Error::other(refuse(
                RefusalKind::BunProjectionInvalid,
                "",
                "",
                "",
            )));
        }
        if let Ok(bytes) = read_bounded(&path.join("package.json"), MAX_LOCK_BYTES as u64) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                installed.push((value, path.into()));
            }
        }
        walk_modules(&path.join("node_modules"), root, seen, installed)
    }
    let mut installed = Vec::new();
    let modules = directory.join("node_modules");
    if modules.exists() {
        // Isolated linker junctions stay inside the checkout; inspect each real
        // directory once and never traverse out to an unrelated disk or share.
        let root = directory
            .canonicalize()
            .map_err(|_| refuse(RefusalKind::BunOfflineInstallFailed, &project.path, "", ""))?;
        walk_modules(&modules, &root, &mut Default::default(), &mut installed).map_err(
            |error| {
                refuse(
                    if error.get_ref().is_some_and(|error| error.is::<Refusal>()) {
                        RefusalKind::BunProjectionInvalid
                    } else {
                        RefusalKind::BunOfflineInstallFailed
                    },
                    &project.path,
                    "",
                    "",
                )
            },
        )?;
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
            .ok_or_else(fail)?;
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
                crate::validate_component(component)
                    .map_err(|_| projection_error(&project.path, package))?;
            }
            if !root.join(bin).is_file() {
                return Err(fail());
            }
        }
        if let Some(bin_dir) = &package.bin_dir {
            for component in bin_dir.strip_prefix("./").unwrap_or(bin_dir).split('/') {
                crate::validate_component(component)
                    .map_err(|_| projection_error(&project.path, package))?;
            }
            if !root.join(bin_dir).is_dir() {
                return Err(fail());
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
