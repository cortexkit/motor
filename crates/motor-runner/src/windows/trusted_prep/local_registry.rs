use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
    time::Duration,
};

type HInternet = *mut c_void;
const WINHTTP_OPTION_REDIRECT_POLICY: u32 = 88;
const WINHTTP_OPTION_REDIRECT_POLICY_NEVER: u32 = 0;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_QUERY_CONTENT_LENGTH: u32 = 5;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const ERROR_WINHTTP_HEADER_NOT_FOUND: u32 = 12150;
const MAX_CRATE_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MAX_CRATE_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;

#[link(name = "winhttp", kind = "raw-dylib")]
unsafe extern "system" {
    fn WinHttpOpen(
        user_agent: *const u16,
        access_type: u32,
        proxy_name: *const u16,
        proxy_bypass: *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpSetTimeouts(
        handle: HInternet,
        resolve_timeout: i32,
        connect_timeout: i32,
        send_timeout: i32,
        receive_timeout: i32,
    ) -> i32;
    fn WinHttpSetOption(handle: HInternet, option: u32, buffer: *mut c_void, length: u32) -> i32;
    fn WinHttpConnect(server: HInternet, name: *const u16, port: u16, reserved: u32) -> HInternet;
    fn WinHttpOpenRequest(
        connection: HInternet,
        verb: *const u16,
        object_name: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpSendRequest(
        request: HInternet,
        headers: *const u16,
        headers_length: u32,
        optional: *const c_void,
        optional_length: u32,
        total_length: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: HInternet, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: HInternet,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_length: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(
        request: HInternet,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: HInternet) -> i32;
}

struct Internet(HInternet);
impl Drop for Internet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn winhttp_error(call: &str) -> io::Error {
    io::Error::other(format!("{call}: {}", io::Error::last_os_error()))
}

fn handle(raw: HInternet, call: &str) -> io::Result<Internet> {
    if raw.is_null() {
        Err(winhttp_error(call))
    } else {
        Ok(Internet(raw))
    }
}

fn valid_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn valid_crate_version(version: &str) -> bool {
    !version.is_empty()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
}

fn crates_io_index_path(name: &str) -> io::Result<String> {
    if !valid_crate_name(name) {
        return Err(invalid(format!("invalid crates.io package name: {name}")));
    }
    let path = match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{}", &name[..1], name),
        _ => format!("{}/{}/{}", &name[..2], &name[2..4], name),
    };
    Ok(format!("/{path}"))
}

fn allowlisted_download_limit(host: &str, path: &str) -> Option<usize> {
    if host == "index.crates.io" {
        if path == "/config.json" {
            return Some(MAX_CRATE_INDEX_BYTES);
        }
        let name = path.rsplit('/').next()?;
        if crates_io_index_path(name).ok().as_deref() == Some(path) {
            return Some(MAX_CRATE_INDEX_BYTES);
        }
    }
    if host == "static.crates.io" {
        let parts = path.split('/').collect::<Vec<_>>();
        if parts.len() == 4
            && parts[0].is_empty()
            && parts[1] == "crates"
            && valid_crate_name(parts[2])
            && parts[3].strip_suffix(".crate").is_some_and(|filename| {
                filename
                    .strip_prefix(&format!("{}-", parts[2]))
                    .is_some_and(valid_crate_version)
            })
        {
            return Some(MAX_CRATE_ARCHIVE_BYTES);
        }
    }
    None
}

fn require_success_status(status: u32) -> io::Result<()> {
    if status == 200 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "unexpected HTTP status {status}; redirects and non-success responses are refused"
        )))
    }
}

/// Fetch only hard-coded allowlisted HTTPS host/path pairs; redirects are disabled
/// so an HTTP response cannot send the runner to another host.
fn download_allowlisted(host: &str, path: &str, size_limit: usize) -> io::Result<Vec<u8>> {
    let maximum = allowlisted_download_limit(host, path)
        .ok_or_else(|| invalid("local-registry download target is not allowlisted"))?;
    if size_limit == 0 || size_limit > maximum {
        return Err(invalid(
            "local-registry download size limit exceeds its host policy",
        ));
    }
    let expected_host = host;
    let user_agent = wide("MotorRunner local-registry qualification");
    let session = handle(
        unsafe { WinHttpOpen(user_agent.as_ptr(), 0, null(), null(), 0) },
        "WinHttpOpen",
    )?;
    checked(unsafe { WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 10_000) })?;
    let mut redirect_policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
    checked(unsafe {
        WinHttpSetOption(
            session.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&mut redirect_policy as *mut u32).cast(),
            std::mem::size_of_val(&redirect_policy) as u32,
        )
    })?;
    let host_wide = wide(expected_host);
    let connection = handle(
        unsafe { WinHttpConnect(session.0, host_wide.as_ptr(), 443, 0) },
        "WinHttpConnect",
    )?;
    let get = wide("GET");
    let path_wide = wide(path);
    let request = handle(
        unsafe {
            WinHttpOpenRequest(
                connection.0,
                get.as_ptr(),
                path_wide.as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;
    checked(unsafe { WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) })?;
    checked(unsafe { WinHttpReceiveResponse(request.0, null_mut()) })?;

    let mut status = 0u32;
    let mut status_size = std::mem::size_of_val(&status) as u32;
    checked(unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut status_size,
            null_mut(),
        )
    })?;
    require_success_status(status).map_err(|error| {
        io::Error::other(format!("{} from https://{expected_host}{path}", error))
    })?;

    let mut declared_size = 0u32;
    let mut size_bytes = std::mem::size_of_val(&declared_size) as u32;
    let has_size = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut declared_size as *mut u32).cast(),
            &mut size_bytes,
            null_mut(),
        )
    } != 0;
    if !has_size {
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if error != ERROR_WINHTTP_HEADER_NOT_FOUND {
            return Err(io::Error::other(format!(
                "WinHttpQueryHeaders(Content-Length) failed with Win32 {error}"
            )));
        }
    } else if declared_size as usize > size_limit {
        return Err(invalid(format!(
            "local-registry response exceeds the {size_limit}-byte limit"
        )));
    }

    let mut bytes = Vec::with_capacity(if has_size { declared_size as usize } else { 0 });
    let mut chunk = [0u8; 8192];
    loop {
        let mut count = 0u32;
        checked(unsafe {
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut count,
            )
        })?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count as usize) > size_limit {
            return Err(invalid(format!(
                "local-registry response exceeds the {size_limit}-byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk[..count as usize]);
    }
    if has_size && bytes.len() != declared_size as usize {
        return Err(io::Error::other(format!(
            "incomplete HTTPS response from {expected_host}: declared {declared_size} bytes, received {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

const CFG_IF_CHECKSUM: &str = "9330f8b2ff13f34540b44e946ef35111825727b38d33286ef986142615121801";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RegistryPackage {
    pub name: String,
    pub version: String,
    pub checksum: String,
}

pub(super) fn locked_registry_packages(lockfile: &str) -> io::Result<Vec<RegistryPackage>> {
    const CRATES_IO_SOURCES: [&str; 2] = [
        "registry+https://github.com/rust-lang/crates.io-index",
        "sparse+https://index.crates.io/",
    ];
    let value: toml::Value = toml::from_str(lockfile).map_err(invalid)?;
    let mut packages = BTreeMap::new();
    for package in value
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(source) = package.get("source").and_then(toml::Value::as_str) else {
            continue;
        };
        if source.starts_with("git+") {
            continue;
        }
        if !CRATES_IO_SOURCES.contains(&source) {
            return Err(invalid(format!(
                "unsupported registry dependency source in Cargo.lock: {source}"
            )));
        }
        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid(format!("registry package has no name: {source}")))?;
        let version = package
            .get("version")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid(format!("registry package {name} has no version")))?;
        let checksum = package
            .get("checksum")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid(format!("registry package {name} {version} has no checksum")))?
            .to_ascii_lowercase();
        if !valid_crate_name(name)
            || !valid_crate_version(version)
            || checksum.len() != 64
            || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid(format!(
                "invalid crates.io lock entry: {name} {version}"
            )));
        }
        let key = (name.to_owned(), version.to_owned());
        if packages
            .insert(key.clone(), checksum.clone())
            .is_some_and(|previous| previous != checksum)
        {
            return Err(invalid(format!(
                "conflicting lock checksums for crates.io package {} {}",
                key.0, key.1
            )));
        }
    }
    Ok(packages
        .into_iter()
        .map(|((name, version), checksum)| RegistryPackage {
            name,
            version,
            checksum,
        })
        .collect())
}

fn verify_archive_checksum(bytes: &[u8], expected_checksum: &str) -> io::Result<String> {
    let checksum = format!("{:x}", Sha256::digest(bytes));
    if checksum != expected_checksum {
        return Err(invalid(format!(
            "crate archive checksum mismatch: expected {expected_checksum}, got {checksum}"
        )));
    }
    Ok(checksum)
}

fn write_verified_archive(
    bytes: &[u8],
    expected_checksum: &str,
    destination: &Path,
) -> io::Result<String> {
    let checksum = verify_archive_checksum(bytes, expected_checksum)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(checksum)
}

fn source_config(local_registry: &Path, source_name: &str) -> String {
    let path = local_registry
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "[source.crates-io]\nreplace-with = \"{source_name}\"\n\n[source.{source_name}]\nlocal-registry = \"{path}\"\n"
    )
}

fn run_cargo(
    cargo: &Path,
    rustc: &Path,
    fixture: &Path,
    cargo_home: &Path,
    target_dir: &Path,
    temp: &Path,
    operation: &str,
) -> Value {
    let mut command = Command::new(cargo);
    command
        .current_dir(fixture)
        .env_clear()
        .env("SYSTEMROOT", r"C:\Windows")
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTC", rustc)
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .env("CARGO_TARGET_DIR", target_dir)
        .env("TEMP", temp)
        .env("TMP", temp)
        .args([operation, "--locked", "--offline", "--manifest-path"])
        .arg(fixture.join("Cargo.toml"));
    match crate::native_process::output_with_timeout(&mut command, Duration::from_secs(20), || {
        false
    }) {
        Ok(output) => cargo_result(output),
        Err(error) => json!({
            "process_started": null,
            "success": false,
            "error": error.to_string(),
            "operation": operation
        }),
    }
}

fn cargo_result(output: std::process::Output) -> Value {
    json!({
        "process_started": true,
        "success": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)
    })
}

fn lockfile_matches(path: &Path, original: &[u8]) -> bool {
    fs::read(path).is_ok_and(|current| current == original)
}

fn verify_locked_index_records(
    bytes: &[u8],
    name: &str,
    packages: &[RegistryPackage],
) -> io::Result<bool> {
    let mut records = BTreeMap::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: Value = serde_json::from_slice(line).map_err(invalid)?;
        if record["name"].as_str() != Some(name) {
            return Err(invalid(format!(
                "sparse-index record name mismatch for {name}"
            )));
        }
        let version = record["vers"]
            .as_str()
            .ok_or_else(|| invalid(format!("sparse-index record for {name} has no version")))?;
        let checksum = record["cksum"].as_str().ok_or_else(|| {
            invalid(format!(
                "sparse-index record for {name} {version} has no checksum"
            ))
        })?;
        if records
            .insert(version.to_owned(), checksum.to_owned())
            .is_some_and(|previous| previous != checksum)
        {
            return Err(invalid(format!(
                "duplicate sparse-index version for {name} {version}"
            )));
        }
    }
    for package in packages {
        match records.get(&package.version) {
            Some(checksum) if checksum == &package.checksum => {}
            Some(_) => {
                return Err(invalid(format!(
                    "{name} {} sparse-index checksum differs from Cargo.lock",
                    package.version
                )))
            }
            None => return Ok(false),
        }
    }
    Ok(true)
}

fn write_report(path: &Path, report: &Value) -> io::Result<()> {
    super::system::write_diagnostic_update(path, report)
}

fn reject_reparse_under(root: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    let relative = path.strip_prefix(root).map_err(invalid)?;
    let mut current = root.to_path_buf();
    if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_attributes() & 0x400 != 0)
    {
        return Err(invalid(format!(
            "reparse point in runner root: {}",
            current.display()
        )));
    }
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(invalid(
                "local-registry path contains a non-normal component",
            ));
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_attributes() & 0x400 != 0 => {
                return Err(invalid(format!(
                    "reparse point in local-registry path: {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_new_file(root: &Path, relative: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let target = root.join(relative);
    reject_reparse_under(root, &target)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid("local-registry file has no parent"))?;
    fs::create_dir_all(parent)?;
    reject_reparse_under(root, &target)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&target)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(target)
}

fn valid_crates_io_config(bytes: &[u8]) -> io::Result<()> {
    let config: Value = serde_json::from_slice(bytes).map_err(invalid)?;
    let download = config["dl"]
        .as_str()
        .ok_or_else(|| invalid("crates.io index config has no download URL"))?;
    if !download.starts_with("https://static.crates.io/") && download != "https://static.crates.io"
    {
        return Err(invalid(
            "crates.io index config names a non-allowlisted download host",
        ));
    }
    Ok(())
}

fn write_job_cargo_config(cargo_home: &Path, has_crates_io: bool) -> io::Result<()> {
    let source = if has_crates_io {
        "[source.crates-io]\nreplace-with = \"motor-local\"\n\n[source.motor-local]\nlocal-registry = \"local-registry\"\n\n"
    } else {
        ""
    };
    let config = format!("{source}[net]\ngit-fetch-with-cli = true\n");
    write_new_file(cargo_home, Path::new("config.toml"), config.as_bytes())?;
    write_new_file(cargo_home, Path::new("gitconfig"), b"")?;
    Ok(())
}

fn write_git_rewrites(cargo_home: &Path, routes: &BTreeMap<String, PathBuf>) -> io::Result<()> {
    let mut mappings = Vec::new();
    for (url, mirror) in routes {
        let parsed = url::Url::parse(url).map_err(invalid)?;
        if !matches!(parsed.scheme(), "https" | "http")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(invalid(format!("unsupported Git bundle source URL: {url}")));
        }
        let relative = mirror.strip_prefix(cargo_home).map_err(|_| {
            invalid(format!(
                "Git bundle path is outside Cargo home: {}",
                mirror.display()
            ))
        })?;
        if relative.components().next()
            != Some(std::path::Component::Normal("git-mirrors".as_ref()))
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid(format!(
                "invalid Git bundle path: {}",
                relative.display()
            )));
        }
        reject_reparse_under(cargo_home, mirror)?;
        mappings.push(json!({
            "url": url,
            "mirror": relative.to_string_lossy().replace('\\', "/")
        }));
    }
    let value = json!({"schema": 1, "routes": mappings});
    write_new_file(
        cargo_home,
        Path::new("git-rewrites.json"),
        &serde_json::to_vec(&value).map_err(io::Error::other)?,
    )?;
    Ok(())
}

#[derive(Default)]
pub(super) struct FetchStats {
    pub config_downloaded: bool,
    pub index_files_downloaded: Vec<String>,
    pub archives_downloaded: Vec<String>,
    pub downloaded_bytes: u64,
    pub downloads: Vec<Value>,
}

impl FetchStats {
    pub(super) fn total_downloads(&self) -> usize {
        usize::from(self.config_downloaded)
            + self.index_files_downloaded.len()
            + self.archives_downloaded.len()
    }
}

fn write_new_staged_file(staging: &Path, relative: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let target = staging.join(relative);
    reject_reparse_under(staging, &target)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid("staged local-registry file has no parent"))?;
    fs::create_dir_all(parent)?;
    reject_reparse_under(staging, &target)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&target)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(target)
}

fn publish_cached_file(
    staging: &Path,
    cache_root: &Path,
    relative: &Path,
    bytes: &[u8],
    replace: bool,
) -> io::Result<()> {
    // The stage and persistent cache are SYSTEM/Administrators-only, and the
    // caller holds the cache lock. Fetched bytes are validated before a
    // create-new staging write, then published into the cache by same-volume rename.
    let target = cache_root.join(relative);
    reject_reparse_under(cache_root, &target)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid("cached local-registry file has no parent"))?;
    fs::create_dir_all(parent)?;
    reject_reparse_under(cache_root, &target)?;
    if target.exists() && !replace {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "local-registry cache entry already exists: {}",
                target.display()
            ),
        ));
    }
    let temporary = write_new_staged_file(
        staging,
        &PathBuf::from("fetch-temp").join(format!("local-registry-{}.tmp", uuid::Uuid::now_v7())),
        bytes,
    )?;
    reject_reparse_under(cache_root, &target)?;
    crate::filesystem::rename(&temporary, &target)
}

fn write_cache_file(
    staging: &Path,
    cache_root: &Path,
    relative: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    publish_cached_file(staging, cache_root, relative, bytes, true)
}

fn copy_cache_file(
    cache_root: &Path,
    source_relative: &Path,
    cargo_home: &Path,
    target_relative: &Path,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let source = cache_root.join(source_relative);
    reject_reparse_under(cache_root, &source)?;
    let metadata = fs::symlink_metadata(&source)?;
    if !metadata.is_file() {
        return Err(invalid(format!(
            "local-registry cache entry is not a regular file: {}",
            source.display()
        )));
    }
    let target = cargo_home.join(target_relative);
    reject_reparse_under(cargo_home, &target)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    reject_reparse_under(cargo_home, &target)?;
    let mut input = fs::File::open(&source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&target)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()
}

pub(super) fn prepare_for_job(
    cache_root: &Path,
    staging: &Path,
    cargo_home: &Path,
    lockfile: &str,
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<FetchStats> {
    let packages = locked_registry_packages(lockfile)?;
    let cache_parent = cache_root
        .parent()
        .ok_or_else(|| invalid("local-registry cache has no parent"))?;
    let staging_parent = staging
        .parent()
        .ok_or_else(|| invalid("job staging has no parent"))?;
    reject_reparse_under(staging_parent, staging)?;
    reject_reparse_under(cache_parent, cache_root)?;
    fs::create_dir_all(cache_root.join("index"))?;
    fs::create_dir_all(cargo_home)?;
    reject_reparse_under(cache_parent, cache_root)?;
    reject_reparse_under(staging, cargo_home)?;

    let mut stats = FetchStats::default();
    if !packages.is_empty() {
        let config_relative = PathBuf::from("index/config.json");
        let config_path = cache_root.join(&config_relative);
        let cached_config = match fs::read(&config_path) {
            Ok(bytes) if valid_crates_io_config(&bytes).is_ok() => Some(bytes),
            Ok(_) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let config_bytes = if let Some(bytes) = cached_config {
            bytes
        } else {
            let bytes =
                download_allowlisted("index.crates.io", "/config.json", MAX_CRATE_INDEX_BYTES)?;
            valid_crates_io_config(&bytes)?;
            write_cache_file(staging, cache_root, &config_relative, &bytes)?;
            stats.config_downloaded = true;
            stats.downloaded_bytes += bytes.len() as u64;
            stats.downloads.push(json!({
                "url": "https://index.crates.io/config.json",
                "bytes": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(&bytes))
            }));
            bytes
        };
        let job_registry = cargo_home.join("local-registry");
        reject_reparse_under(cargo_home, &job_registry)?;
        fs::create_dir_all(job_registry.join("index"))?;
        write_new_file(&job_registry, Path::new("index/config.json"), &config_bytes)?;

        let mut by_name: BTreeMap<String, Vec<RegistryPackage>> = BTreeMap::new();
        for package in &packages {
            by_name
                .entry(package.name.clone())
                .or_default()
                .push(package.clone());
        }
        for (name, locked) in by_name {
            let index_relative =
                PathBuf::from("index").join(crates_io_index_path(&name)?.trim_start_matches('/'));
            let index_path = cache_root.join(&index_relative);
            reject_reparse_under(cache_root, &index_path)?;
            let cached_index = match fs::read(&index_path) {
                Ok(bytes)
                    if verify_locked_index_records(&bytes, &name, &locked).unwrap_or(false) =>
                {
                    Some(bytes)
                }
                Ok(_) => None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let index_bytes = if let Some(bytes) = cached_index {
                bytes
            } else {
                let path = crates_io_index_path(&name)?;
                let bytes = download_allowlisted("index.crates.io", &path, MAX_CRATE_INDEX_BYTES)?;
                if !verify_locked_index_records(&bytes, &name, &locked)? {
                    return Err(invalid(format!(
                        "crates.io index for {name} lacks a version locked by Cargo.lock"
                    )));
                }
                write_cache_file(staging, cache_root, &index_relative, &bytes)?;
                stats.index_files_downloaded.push(name.clone());
                stats.downloaded_bytes += bytes.len() as u64;
                stats.downloads.push(json!({
                    "url": format!("https://index.crates.io{path}"),
                    "crate": name,
                    "bytes": bytes.len(),
                    "sha256": format!("{:x}", Sha256::digest(&bytes))
                }));
                bytes
            };
            let job_index_relative =
                PathBuf::from("index").join(crates_io_index_path(&name)?.trim_start_matches('/'));
            write_new_file(&job_registry, &job_index_relative, &index_bytes)?;

            for package in locked {
                let filename = format!("{}-{}.crate", package.name, package.version);
                let archive_relative = PathBuf::from(&filename);
                let archive_path = cache_root.join(&archive_relative);
                let sidecar_relative = PathBuf::from(".sha256").join(format!("{filename}.sha256"));
                let sidecar_path = cache_root.join(&sidecar_relative);
                reject_reparse_under(cache_root, &archive_path)?;
                reject_reparse_under(cache_root, &sidecar_path)?;
                let cached = if archive_path.is_file() {
                    let recorded = fs::read_to_string(&sidecar_path)
                        .ok()
                        .map(|value| value.trim().to_owned());
                    if recorded.as_deref() == Some(package.checksum.as_str()) {
                        true
                    } else {
                        let existing = fs::read(&archive_path)?;
                        let actual = format!("{:x}", Sha256::digest(&existing));
                        if actual == package.checksum {
                            write_cache_file(
                                staging,
                                cache_root,
                                &sidecar_relative,
                                format!("{}\n", package.checksum).as_bytes(),
                            )?;
                            true
                        } else {
                            false
                        }
                    }
                } else {
                    false
                };
                if !cached {
                    let path = format!(
                        "/crates/{}/{}-{}.crate",
                        package.name, package.name, package.version
                    );
                    let bytes =
                        download_allowlisted("static.crates.io", &path, MAX_CRATE_ARCHIVE_BYTES)?;
                    let checksum =
                        verify_archive_checksum(&bytes, &package.checksum).map_err(|_| {
                            invalid(format!(
                                "{} {} archive checksum mismatch before writing",
                                package.name, package.version
                            ))
                        })?;
                    write_cache_file(staging, cache_root, &archive_relative, &bytes)?;
                    write_cache_file(
                        staging,
                        cache_root,
                        &sidecar_relative,
                        format!("{}\n", checksum).as_bytes(),
                    )?;
                    stats.archives_downloaded.push(filename.clone());
                    stats.downloaded_bytes += bytes.len() as u64;
                    stats.downloads.push(json!({
                        "url": format!("https://static.crates.io{path}"),
                        "crate": filename,
                        "bytes": bytes.len(),
                        "sha256": checksum,
                        "checksum_matches_lockfile": true
                    }));
                }
                copy_cache_file(
                    cache_root,
                    &archive_relative,
                    &job_registry,
                    &archive_relative,
                )?;
            }
        }
    }

    write_job_cargo_config(cargo_home, !packages.is_empty())?;
    write_git_rewrites(cargo_home, routes)?;
    Ok(stats)
}

pub(super) fn prove(
    cargo: &Path,
    rustc: &Path,
    fixture: &Path,
    stage: &Path,
    report_path: &Path,
) -> io::Result<Value> {
    let lock_path = fixture.join("Cargo.lock");
    let lock_before = fs::read(&lock_path)?;
    let lockfile = std::str::from_utf8(&lock_before).map_err(invalid)?;
    let proof_cache = stage.join("proof-cache");
    let first_home = stage.join("fetch-temp/local-registry-first-job");
    let second_home = stage.join("fetch-temp/local-registry-second-job");
    let no_git_routes = BTreeMap::new();
    let first_stats = prepare_for_job(&proof_cache, stage, &first_home, lockfile, &no_git_routes)?;
    let second_stats =
        prepare_for_job(&proof_cache, stage, &second_home, lockfile, &no_git_routes)?;
    let local_registry = second_home.join("local-registry");
    let index_config = fs::read(local_registry.join("index/config.json"))?;
    let index_entry = fs::read(local_registry.join("index/cf/g-/cfg-if"))?;
    let archive_path = local_registry.join("cfg-if-1.0.4.crate");
    let archive_bytes = fs::read(&archive_path)?;
    let archive_sha256 = format!("{:x}", Sha256::digest(&archive_bytes));
    if archive_sha256 != CFG_IF_CHECKSUM {
        return Err(invalid(
            "prepared local-registry archive does not match Cargo.lock",
        ));
    }

    let target = stage.join("cargo-target/local-registry");
    let temp = stage.join("fetch-temp");
    let fetch = run_cargo(cargo, rustc, fixture, &second_home, &target, &temp, "fetch");
    let lock_unchanged_after_fetch = lockfile_matches(&lock_path, &lock_before);
    let mut report = json!({
        "schema": 1,
        "persistent_cache": proof_cache,
        "local_registry": local_registry,
        "first_job_downloads": {
            "total": first_stats.total_downloads(),
            "config": first_stats.config_downloaded,
            "index_files": first_stats.index_files_downloaded,
            "archives": first_stats.archives_downloaded,
            "bytes": first_stats.downloaded_bytes,
            "sources": first_stats.downloads
        },
        "second_job_downloads": {
            "total": second_stats.total_downloads(),
            "config": second_stats.config_downloaded,
            "index_files": second_stats.index_files_downloaded,
            "archives": second_stats.archives_downloaded,
            "bytes": second_stats.downloaded_bytes,
            "sources": second_stats.downloads
        },
        "cargo_home_before_commands": {
            "path": second_home,
            "runner_config": "config.toml",
            "source_config_relative_path": "local-registry",
            "cfg_if_index_config_bytes": index_config.len(),
            "cfg_if_index_record_bytes": index_entry.len(),
            "cfg_if_index_record_sha256": format!("{:x}", Sha256::digest(&index_entry)),
            "cfg_if_archive_bytes": archive_bytes.len(),
            "cfg_if_archive_sha256": archive_sha256,
            "cfg_if_checksum_matches_lockfile": true
        },
        "cargo_fetch_locked_offline": fetch,
        "lockfile_unchanged_after_fetch": lock_unchanged_after_fetch,
        "cargo_build_locked_offline": null,
        "lockfile_unchanged_after_build": null,
        "checksum_negative_controls": {},
        "network_policy_negative_controls": {
            "disallowed_host_refused_before_request": allowlisted_download_limit("downloads.invalid", "/cf/g-/cfg-if").is_none(),
            "redirect_status_refused": require_success_status(302).is_err()
        },
        "repository_source_override_test": null
    });
    write_report(report_path, &report)?;

    let build = run_cargo(cargo, rustc, fixture, &second_home, &target, &temp, "build");
    let lock_unchanged_after_build = lockfile_matches(&lock_path, &lock_before);
    report["cargo_build_locked_offline"] = build;
    report["lockfile_unchanged_after_build"] = json!(lock_unchanged_after_build);
    write_report(report_path, &report)?;

    let mut changed_archive = archive_bytes.clone();
    if changed_archive.is_empty() {
        return Err(invalid("downloaded cfg-if archive is empty"));
    }
    changed_archive[0] ^= 1;
    let refused_path = stage.join("checksum-refused-before-write.crate");
    let refused = write_verified_archive(&changed_archive, CFG_IF_CHECKSUM, &refused_path);
    let runner_refused_before_write = refused.is_err() && !refused_path.exists();

    let corrupt_home = stage.join("fetch-temp/corrupt-registry-home");
    fs::create_dir_all(&corrupt_home)?;
    let corrupt_registry = corrupt_home.join("local-registry");
    super::copy_tree(&local_registry, &corrupt_registry)?;
    fs::write(
        corrupt_registry.join("cfg-if-1.0.4.crate"),
        &changed_archive,
    )?;
    write_job_cargo_config(&corrupt_home, true)?;
    write_git_rewrites(&corrupt_home, &no_git_routes)?;
    let cargo_rejected_corrupt_archive = run_cargo(
        cargo,
        rustc,
        fixture,
        &corrupt_home,
        &stage.join("cargo-target/corrupt-registry"),
        &temp,
        "fetch",
    );
    let cargo_checksum_text = format!(
        "{}\n{}",
        cargo_rejected_corrupt_archive["stdout"]
            .as_str()
            .unwrap_or(""),
        cargo_rejected_corrupt_archive["stderr"]
            .as_str()
            .unwrap_or("")
    )
    .to_ascii_lowercase();
    let cargo_rejected_checksum = cargo_rejected_corrupt_archive["success"] == false
        && cargo_checksum_text.contains("checksum");
    report["checksum_negative_controls"] = json!({
        "altered_archive_sha256": format!("{:x}", Sha256::digest(&changed_archive)),
        "runner_refused_before_write": runner_refused_before_write,
        "runner_write_error": refused.err().map(|error| error.to_string()),
        "rejected_path_exists": refused_path.exists(),
        "cargo_bypassed_runner": cargo_rejected_corrupt_archive,
        "cargo_rejected_changed_archive_checksum": cargo_rejected_checksum
    });
    write_report(report_path, &report)?;

    let empty_registry = stage.join("empty-local-registry");
    fs::create_dir_all(empty_registry.join("index"))?;
    fs::write(empty_registry.join("index/config.json"), &index_config)?;
    let repository_config_dir = fixture.join(".cargo");
    fs::create_dir_all(&repository_config_dir)?;
    fs::write(
        repository_config_dir.join("config.toml"),
        source_config(&empty_registry, "repository-override"),
    )?;
    let override_home = stage.join("fetch-temp/repository-override-home");
    fs::create_dir_all(&override_home)?;
    super::copy_tree(&local_registry, &override_home.join("local-registry"))?;
    write_job_cargo_config(&override_home, true)?;
    write_git_rewrites(&override_home, &no_git_routes)?;
    let override_result = run_cargo(
        cargo,
        rustc,
        fixture,
        &override_home,
        &stage.join("cargo-target/repository-override"),
        &temp,
        "fetch",
    );
    let repository_config_check_started = override_result["process_started"] == true;
    let repository_override_selected = override_result["success"] == false;
    fs::remove_dir_all(&repository_config_dir)?;
    let lock_unchanged_after_override = lockfile_matches(&lock_path, &lock_before);
    report["repository_source_override_test"] = json!({
        "repository_config_path": repository_config_dir.join("config.toml"),
        "cargo_home_config_path": override_home.join("config.toml"),
        "home_config_source": "motor-local",
        "repository_config_source": "repository-override",
        "cargo_result": override_result,
        "check_started": repository_config_check_started,
        "repository_config_was_selected": repository_override_selected,
        "lockfile_unchanged": lock_unchanged_after_override
    });
    report["proof_passed"] = json!(
        first_stats.total_downloads() == 3
            && second_stats.total_downloads() == 0
            && report["cargo_fetch_locked_offline"]["success"] == true
            && report["cargo_build_locked_offline"]["success"] == true
            && report["lockfile_unchanged_after_fetch"] == true
            && report["lockfile_unchanged_after_build"] == true
            && report["checksum_negative_controls"]["runner_refused_before_write"] == true
            && report["checksum_negative_controls"]["cargo_rejected_changed_archive_checksum"]
                == true
            && report["network_policy_negative_controls"]["disallowed_host_refused_before_request"]
                == true
            && report["network_policy_negative_controls"]["redirect_status_refused"] == true
            && report["repository_source_override_test"]["check_started"] == true
            && report["repository_source_override_test"]["lockfile_unchanged"] == true
    );
    write_report(report_path, &report)?;
    Ok(report)
}
