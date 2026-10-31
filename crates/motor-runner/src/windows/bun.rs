//! Host-only registry policy and verified, immutable tarball storage.

#[cfg(windows)]
use motor_winagent::bun::protection;
#[cfg(test)]
use motor_winagent::bun::Integrity;
use motor_winagent::bun::{self, Manifest, Package, Project, Refusal, RefusalKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(all(test, unix))]
use std::io::Write;
use std::time::{Duration, Instant};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};
use url::Url;
mod store;
pub(super) use store::Lease;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RegistryConfig {
    pub scopes: BTreeMap<String, Scope>,
    pub cas_quota_bytes: u64,
}
impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            scopes: BTreeMap::new(),
            cas_quota_bytes: store::DEFAULT_QUOTA,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    pub registry: String,
    pub credential_file: Option<PathBuf>,
    #[serde(default)]
    pub repositories: Vec<String>,
}

fn fail(kind: RefusalKind, project: &str, package: &Package, origin: &str) -> Refusal {
    let mut refusal = bun::refuse(kind, project, &package.name, &package.version);
    refusal.registry_origin = origin.into();
    refusal
}

fn bounded_file(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(io::Error::other("not a bounded regular file"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("reparse file"));
        }
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("file exceeds limit"));
    }
    Ok(bytes)
}

fn registry_url(text: &str) -> Option<Url> {
    let url = Url::parse(text).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.port_or_known_default() == Some(443)
        && url.path().ends_with('/'))
    .then_some(url)
}

struct FetchTarget<'a> {
    url: Url,
    origin: String,
    credential_file: Option<&'a Path>,
    provenance: store::Provenance,
}

fn resolve<'a>(
    config: &'a RegistryConfig,
    repository: &str,
    project: &str,
    row: &bun::LockedPackage,
) -> Result<FetchTarget<'a>, Refusal> {
    let package = &row.package;
    if !bun::valid_name(&package.name)
        || package.version.len() > 256
        || !package
            .version
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
        || !package
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
        || !package.integrity.valid()
    {
        return Err(bun::refuse(RefusalKind::BunLockInvalid, project, "", ""));
    }
    let scope = package
        .name
        .strip_prefix('@')
        .and_then(|_| package.name.split('/').next());
    let configured = scope.and_then(|scope| config.scopes.get(scope));
    let registry = configured.map_or("https://registry.npmjs.org/", |scope| {
        scope.registry.as_str()
    });
    let registry = registry_url(registry)
        .ok_or_else(|| fail(RefusalKind::BunRegistryUnauthorized, project, package, ""))?;
    let origin = registry.origin().ascii_serialization();
    if row.url.contains(['%', '\\']) || row.url.chars().any(char::is_control) {
        return Err(fail(
            RefusalKind::BunRegistryUnauthorized,
            project,
            package,
            &origin,
        ));
    }
    let credential_file = if let Some(configured) = configured {
        // Grants are checked even on a verified cache hit, so revocation does not
        // depend on registry availability or on removing shared cached bytes.
        if configured.credential_file.is_none() {
            let mut refusal = fail(
                RefusalKind::BunCredentialsUnavailable,
                project,
                package,
                &origin,
            );
            refusal.package = scope.unwrap().into();
            return Err(refusal);
        }
        if !configured.repositories.iter().any(|r| r == repository) {
            return Err(fail(
                RefusalKind::BunRegistryUnauthorized,
                project,
                package,
                &origin,
            ));
        }
        configured.credential_file.as_deref()
    } else {
        None
    };
    let basename = package.name.rsplit('/').next().unwrap();
    let expected = registry
        .join(&format!(
            "{}/-/{}-{}.tgz",
            package.name, basename, package.version
        ))
        .map_err(|_| {
            fail(
                RefusalKind::BunRegistryUnauthorized,
                project,
                package,
                &origin,
            )
        })?;
    let url = if row.url.is_empty() {
        Some(expected.clone())
    } else {
        Url::parse(&row.url).ok()
    }
    .ok_or_else(|| fail(RefusalKind::BunSourceUnsupported, project, package, &origin))?;
    if url.scheme() != "https"
        || url.origin() != registry.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != expected.path()
        || url.path().contains('%')
        || url.path().contains('\\')
    {
        return Err(fail(
            RefusalKind::BunRegistryUnauthorized,
            project,
            package,
            &origin,
        ));
    }
    Ok(FetchTarget {
        provenance: store::Provenance {
            registry: registry.to_string(),
            origin: origin.clone(),
            scope: scope.map(str::to_owned),
            credentialed: credential_file.is_some(),
        },
        url,
        origin,
        credential_file,
    })
}

fn credential(path: &Path) -> io::Result<String> {
    #[cfg(windows)]
    protection::check(path, 0)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("credential file is not host protected"));
        }
    }
    let bytes = bounded_file(path, 8192)?;
    let token = String::from_utf8(bytes).map_err(|_| io::Error::other("invalid credential"))?;
    let token = token.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(io::Error::other("invalid credential"));
    }
    Ok(token.into())
}

fn credential_path(root: &Path, path: &Path) -> io::Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| io::Error::other("credential outside host store"))?;
    let mut current = root.to_path_buf();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(io::Error::other("invalid credential path"));
            }
            current.push(component.as_os_str());
        }
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other("credential path contains a link"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::other("unprotected credential path"));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other("credential path contains a reparse point"));
            }
            protection::check(&current, 0)?;
        }
    }
    if !path.is_file() {
        return Err(io::Error::other("credential is not a file"));
    }
    Ok(())
}

fn fetch(target: &FetchTarget<'_>) -> Result<Vec<u8>, RefusalKind> {
    // No other layer reads the token. Neither the HTTP client's diagnostic text
    // nor headers are propagated into a job artifact or refusal.
    let token = target
        .credential_file
        .map(credential)
        .transpose()
        .map_err(|_| RefusalKind::BunCredentialsUnavailable)?;
    #[cfg(windows)]
    {
        super::trusted_prep::download_bun(
            target.url.host_str().unwrap(),
            target.url.path(),
            token.as_deref(),
        )
        .map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                RefusalKind::BunRegistryUnauthorized
            } else {
                RefusalKind::BunFetchUnavailable
            }
        })
    }
    #[cfg(not(windows))]
    {
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| RefusalKind::BunFetchUnavailable)?;
        let mut request = client.get(target.url.clone());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|_| RefusalKind::BunFetchUnavailable)?;
        fetch_status(response.status().as_u16())?;
        if response
            .content_length()
            .is_some_and(|n| n > bun::MAX_BLOB_BYTES)
        {
            return Err(RefusalKind::BunFetchUnavailable);
        }
        let mut bytes = Vec::new();
        response
            .take(bun::MAX_BLOB_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| RefusalKind::BunFetchUnavailable)?;
        if bytes.len() as u64 > bun::MAX_BLOB_BYTES {
            return Err(RefusalKind::BunFetchUnavailable);
        }
        Ok(bytes)
    }
}

pub(super) fn fetch_status(status: u16) -> Result<(), RefusalKind> {
    match status {
        200 => Ok(()),
        401 | 403 | 404 => Err(RefusalKind::BunRegistryUnauthorized),
        _ => Err(RefusalKind::BunFetchUnavailable),
    }
}

fn protected_directory(path: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::other("invalid Bun store directory"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other("reparse Bun directory"));
            }
        }
    }
    fs::create_dir_all(path)?;
    #[cfg(windows)]
    protection::check(path, 0)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
fn verified_blob(
    cache: &Path,
    integrity: &Integrity,
    download: impl FnMut() -> Result<Vec<u8>, RefusalKind>,
) -> Result<Vec<u8>, RefusalKind> {
    let source = store::Provenance {
        registry: "https://registry.npmjs.org/".into(),
        origin: "https://registry.npmjs.org".into(),
        scope: None,
        credentialed: false,
    };
    store::get(
        cache,
        &source,
        integrity,
        store::DEFAULT_QUOTA,
        &mut Lease::default(),
        download,
    )
}

fn discover(path: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other("link in Bun snapshot"));
    }
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            if entry
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("node_modules"))
            {
                continue;
            }
            discover(&entry, files)?;
        }
    } else if metadata.is_file() {
        files.push(path.into());
    } else {
        return Err(io::Error::other("special file in Bun snapshot"));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedLock {
    digest: String,
    rows: Vec<bun::LockedPackage>,
    locals: Vec<String>,
}
fn parse_lock(project: &str, bytes: &[u8]) -> io::Result<ParsedLock> {
    let invalid = || bun::refuse(RefusalKind::BunLockInvalid, project, "", "");
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let rows = bun::read_lock(project, text)?;
    let value = bun::text_json(text).map_err(|_| invalid())?;
    let mut locals = Vec::new();
    for row in value["packages"].as_object().ok_or_else(invalid)?.values() {
        let resolution = row[0].as_str().ok_or_else(invalid)?;
        let at = resolution[usize::from(resolution.starts_with('@'))..]
            .find('@')
            .ok_or_else(invalid)?
            + usize::from(resolution.starts_with('@'));
        for prefix in ["file:", "link:", "workspace:", "portal:"] {
            if let Some(local) = resolution[at + 1..].strip_prefix(prefix) {
                locals.push(local.to_owned());
            }
        }
    }
    Ok(ParsedLock {
        digest: hex_sha256(bytes),
        rows,
        locals,
    })
}

/// On Linux the credential-free motor-prep systemd service parses these inputs
/// with a 2 GiB memory limit before the host fetcher handles their bounded output.
#[cfg(any(unix, test))]
pub(super) fn write_parsed_locks(staging: &Path) -> io::Result<()> {
    let mut files = Vec::new();
    discover(&staging.join("m"), &mut files)?;
    let mut parsed = BTreeMap::new();
    let mut input_bytes = 0usize;
    for lock in files
        .iter()
        .filter(|path| path.file_name().is_some_and(|n| n == "bun.lock"))
    {
        let bytes = bounded_file(lock, bun::MAX_LOCK_BYTES as u64)?;
        input_bytes = input_bytes.saturating_add(bytes.len());
        if input_bytes > bun::MAX_LOCK_BYTES {
            return Err(bun::refuse(RefusalKind::BunLockInvalid, "", "", "").into());
        }
        let project = lock
            .parent()
            .unwrap()
            .strip_prefix(staging)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        parsed.insert(project.clone(), parse_lock(&project, &bytes)?);
    }
    let bytes = serde_json::to_vec(&parsed)?;
    if bytes.len() > 2 * bun::MAX_LOCK_BYTES {
        return Err(bun::refuse(RefusalKind::BunLockInvalid, "", "", "").into());
    }
    fs::write(staging.join("bun-parsed.json"), bytes)
}

fn validate_local(
    project: &str,
    directory: &Path,
    checkout: &Path,
    locals: &[String],
    files: &[PathBuf],
) -> Result<(), Refusal> {
    let fail = || bun::refuse(RefusalKind::BunSourceUnsupported, project, "", "");
    for local in locals {
        let target = super::local_path::resolve(directory, &[checkout.to_path_buf()], local)
            .map_err(|_| fail())?;
        if !files.contains(&target.join("package.json")) {
            return Err(fail());
        }
    }
    Ok(())
}

fn validate_registry_inputs(
    project: &str,
    directory: &Path,
    registries: &RegistryConfig,
) -> Result<(), Refusal> {
    let invalid = || bun::refuse(RefusalKind::BunRegistryUnauthorized, project, "", "");
    let config_path = directory.join("bunfig.toml");
    if config_path.exists() {
        let bytes =
            bounded_file(&config_path, bun::MAX_LOCK_BYTES as u64).map_err(|_| invalid())?;
        let config: toml::Value =
            toml::from_str(std::str::from_utf8(&bytes).map_err(|_| invalid())?)
                .map_err(|_| invalid())?;
        if let Some(install) = config.get("install") {
            if let Some(default) = install.get("registry") {
                if default.as_str() != Some("https://registry.npmjs.org/") {
                    return Err(invalid());
                }
            }
            if let Some(scopes) = install.get("scopes") {
                for (scope, value) in scopes.as_table().ok_or_else(invalid)? {
                    let scope = format!("@{}", scope.trim_start_matches('@'));
                    let Some(policy) = registries.scopes.get(&scope) else {
                        return Err(bun::refuse(
                            RefusalKind::BunCredentialsUnavailable,
                            project,
                            &scope,
                            "",
                        ));
                    };
                    // URL-only mappings may be committed, but credentials belong
                    // exclusively to host configuration and the fetcher's file.
                    let url = value
                        .as_str()
                        .or_else(|| {
                            let table = value.as_table()?;
                            (table.len() == 1)
                                .then(|| table.get("url")?.as_str())
                                .flatten()
                        })
                        .ok_or_else(invalid)?;
                    if registry_url(url) != registry_url(&policy.registry) {
                        return Err(invalid());
                    }
                }
            }
        }
    }
    let npmrc = directory.join(".npmrc");
    if npmrc.exists() {
        let bytes = bounded_file(&npmrc, bun::MAX_LOCK_BYTES as u64).map_err(|_| invalid())?;
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|s| !s.starts_with(['#', ';']))
        {
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                if key == "registry" && value.trim() == "https://registry.npmjs.org/" {
                    continue;
                }
                if key.to_ascii_lowercase().contains("auth")
                    || key.contains("registry")
                    || key.contains("password")
                    || key.contains("username")
                {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

/// On Linux this broker runs after the credential-free motor-prep systemd unit
/// exits; on Windows it follows trusted Cargo preparation in the SYSTEM runner.
/// It reads bounded lock inputs without starting repository executables or scripts.
pub(super) fn prepare(
    config: &crate::engine::Config,
    staging: &Path,
    repository: &str,
    checkouts: &[PathBuf],
) -> io::Result<Lease> {
    #[cfg(target_os = "linux")]
    let parsed = Some(
        match bounded_file(
            &staging.join("bun-parsed.json"),
            (2 * bun::MAX_LOCK_BYTES) as u64,
        ) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, ParsedLock>>(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        },
    );
    #[cfg(not(target_os = "linux"))]
    let parsed = None;
    prepare_impl(
        config,
        staging,
        repository,
        checkouts,
        fetch,
        parsed.as_ref(),
    )
}

#[cfg(test)]
fn prepare_with_fetch(
    config: &crate::engine::Config,
    staging: &Path,
    repository: &str,
    checkouts: &[PathBuf],
    download: impl FnMut(&FetchTarget<'_>) -> Result<Vec<u8>, RefusalKind>,
) -> io::Result<Lease> {
    prepare_impl(config, staging, repository, checkouts, download, None)
}

fn prepare_impl(
    config: &crate::engine::Config,
    staging: &Path,
    repository: &str,
    checkouts: &[PathBuf],
    mut download: impl FnMut(&FetchTarget<'_>) -> Result<Vec<u8>, RefusalKind>,
    parsed: Option<&BTreeMap<String, ParsedLock>>,
) -> io::Result<Lease> {
    let mut lease = Lease::default();
    let registries = &config.windows.as_ref().unwrap().bun;
    let cache = config.state_dir().join("bun-cas");
    let mut files = Vec::new();
    discover(&staging.join("m"), &mut files)?;
    let mut locks = std::collections::BTreeSet::new();
    for checkout in checkouts {
        let paths = files
            .iter()
            .filter_map(|path| path.strip_prefix(checkout).ok().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        let project = checkout
            .strip_prefix(staging)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if paths
            .iter()
            .any(|p| p.file_name().is_some_and(|p| p == "bun.lockb"))
        {
            return Err(bun::refuse(RefusalKind::BunLockInvalid, &project, "", "").into());
        }
        for install in crate::javascript::discover_javascript_installs(&paths)
            .map_err(|_| bun::refuse(RefusalKind::BunLockInvalid, &project, "", ""))?
        {
            if install.lockfile == "bun.lock" {
                locks.insert(checkout.join(install.directory).join("bun.lock"));
            }
        }
    }
    let mut projects = Vec::new();
    let mut input_bytes = 0usize;
    for lock in &locks {
        let directory = lock.parent().unwrap();
        let project = directory
            .strip_prefix(staging)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let invalid = || bun::refuse(RefusalKind::BunLockInvalid, &project, "", "");
        if !files.contains(&directory.join("package.json")) {
            return Err(invalid().into());
        }
        let bytes = bounded_file(lock, bun::MAX_LOCK_BYTES as u64).map_err(|_| invalid())?;
        input_bytes = input_bytes.saturating_add(bytes.len());
        if input_bytes > bun::MAX_LOCK_BYTES {
            return Err(invalid().into());
        }
        let owned;
        let parsed = if let Some(parsed) = parsed {
            let entry = parsed.get(&project).ok_or_else(invalid)?;
            if entry.digest != hex_sha256(&bytes) {
                return Err(invalid().into());
            }
            entry
        } else {
            owned = parse_lock(&project, &bytes)?;
            &owned
        };
        let rows = parsed.rows.clone();
        let checkout = checkouts
            .iter()
            .filter(|root| directory.starts_with(root))
            .max_by_key(|p| p.components().count())
            .ok_or_else(invalid)?;
        validate_local(&project, directory, checkout, &parsed.locals, &files)?;
        validate_registry_inputs(&project, directory, registries)?;
        // Resolve and authorize every row before the first download, including
        // optional/disabled rows and rows whose integrity-keyed cached blob is already present.
        for row in &rows {
            resolve(registries, repository, &project, row)?;
        }
        let package_json =
            bounded_file(&directory.join("package.json"), bun::MAX_LOCK_BYTES as u64)
                .map_err(|_| invalid())?;
        let metadata: serde_json::Value =
            serde_json::from_slice(&package_json).map_err(|_| invalid())?;
        let name = metadata
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let version = metadata
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        if !name.is_empty() && !bun::valid_name(&name) {
            return Err(invalid().into());
        }
        projects.push((
            project,
            hex_sha256(&bytes),
            hex_sha256(&package_json),
            name,
            version,
            rows,
        ));
    }
    if projects.is_empty() {
        return Ok(lease);
    }
    protected_directory(&cache)?;
    let seed = staging.join("bun-seed");
    protected_directory(&seed)?;
    let mut manifest = Manifest {
        format: 1,
        bun_version: bun::BUN_VERSION.into(),
        projects: Vec::new(),
        registries: BTreeMap::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut seed_bytes = 0u64;
    for (project, lock_sha256, manifest_sha256, name, version, rows) in projects {
        let mut packages = Vec::new();
        for row in rows {
            let target = resolve(registries, repository, &project, &row)?;
            if let Some(scope) = row
                .package
                .name
                .split('/')
                .next()
                .filter(|s| s.starts_with('@'))
            {
                if let Some(policy) = registries.scopes.get(scope) {
                    let registry = registry_url(&policy.registry).ok_or_else(|| {
                        fail(
                            RefusalKind::BunRegistryUnauthorized,
                            &project,
                            &row.package,
                            "",
                        )
                    })?;
                    manifest
                        .registries
                        .insert(scope.into(), registry.to_string());
                }
            }
            if let Some(path) = target.credential_file {
                let credentials = config.state_dir().join("bun-credentials");
                if credential_path(&credentials, path).is_err() {
                    return Err(fail(
                        RefusalKind::BunCredentialsUnavailable,
                        &project,
                        &row.package,
                        &target.origin,
                    )
                    .into());
                }
            }
            if Instant::now() >= deadline {
                return Err(fail(
                    RefusalKind::BunFetchUnavailable,
                    &project,
                    &row.package,
                    &target.origin,
                )
                .into());
            }
            let mut unused = Lease::default();
            let reference = if row.package.windows {
                &mut lease
            } else {
                &mut unused
            };
            let bytes = store::get(
                &cache,
                &target.provenance,
                &row.package.integrity,
                registries.cas_quota_bytes,
                reference,
                || download(&target),
            )
            .map_err(|kind| fail(kind, &project, &row.package, &target.origin))?;
            let dest = seed.join(row.package.integrity.blob_path());
            if row.package.windows && !dest.exists() {
                seed_bytes += bytes.len() as u64;
                if seed_bytes > 8 * 1024 * 1024 * 1024 {
                    return Err(fail(
                        RefusalKind::BunFetchUnavailable,
                        &project,
                        &row.package,
                        &target.origin,
                    )
                    .into());
                }
                fs::create_dir_all(dest.parent().unwrap())?;
                fs::write(dest, bytes)?;
            }
            packages.push(row.package);
        }
        manifest.projects.push(Project {
            path: project,
            lock_sha256,
            manifest_sha256,
            name,
            version,
            packages,
        });
    }
    fs::write(seed.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
    Ok(lease)
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod guest_tests;
#[cfg(test)]
mod security_tests;
#[cfg(test)]
mod tests;
