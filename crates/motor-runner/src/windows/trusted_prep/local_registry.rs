use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
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
const MAX_INDEX_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_BYTES: usize = 1024 * 1024;

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

/// Fetch only hard-coded allowlisted HTTPS host/path pairs; redirects are disabled
/// so an HTTP response cannot send the runner to another host.
fn download_allowlisted(host: &str, path: &str) -> io::Result<Vec<u8>> {
    let (expected_host, size_limit) = match (host, path) {
        ("index.crates.io", "/config.json" | "/cf/g-/cfg-if") => (host, MAX_INDEX_BYTES),
        ("static.crates.io", "/crates/cfg-if/cfg-if-1.0.4.crate") => (host, MAX_ARCHIVE_BYTES),
        _ => return Err(invalid("local-registry download target is not allowlisted")),
    };
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
    if status != 200 {
        return Err(io::Error::other(format!(
            "unexpected HTTP status {status} from https://{expected_host}{path}; redirects are refused"
        )));
    }

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

fn write_verified_archive(bytes: &[u8], destination: &Path) -> io::Result<String> {
    let checksum = format!("{:x}", Sha256::digest(bytes));
    if checksum != CFG_IF_CHECKSUM {
        return Err(invalid(format!(
            "cfg-if archive checksum mismatch: expected {CFG_IF_CHECKSUM}, got {checksum}"
        )));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(checksum)
}

fn verify_index_checksum(bytes: &[u8]) -> io::Result<()> {
    let mut matching_records = 0;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: Value = serde_json::from_slice(line).map_err(invalid)?;
        if record["name"] == "cfg-if" && record["vers"] == "1.0.4" {
            matching_records += 1;
            if record["cksum"].as_str() != Some(CFG_IF_CHECKSUM) {
                return Err(invalid(
                    "cfg-if 1.0.4 sparse-index checksum differs from Cargo.lock",
                ));
            }
        }
    }
    if matching_records != 1 {
        return Err(invalid(format!(
            "expected one cfg-if 1.0.4 sparse-index record, found {matching_records}"
        )));
    }
    Ok(())
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

fn write_report(path: &Path, report: &Value) -> io::Result<()> {
    super::launch::write_diagnostic_update(path, report)
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
    let config_bytes = download_allowlisted("index.crates.io", "/config.json")?;
    let index_bytes = download_allowlisted("index.crates.io", "/cf/g-/cfg-if")?;
    let archive_bytes =
        download_allowlisted("static.crates.io", "/crates/cfg-if/cfg-if-1.0.4.crate")?;
    let index_config: Value = serde_json::from_slice(&config_bytes).map_err(invalid)?;
    let download_base = index_config["dl"]
        .as_str()
        .ok_or_else(|| invalid("crates.io index config has no download URL"))?;
    if !download_base.starts_with("https://static.crates.io/")
        && download_base != "https://static.crates.io"
    {
        return Err(invalid(
            "crates.io index config names a non-allowlisted download host",
        ));
    }
    verify_index_checksum(&index_bytes)?;
    let downloaded_archive_sha256 = format!("{:x}", Sha256::digest(&archive_bytes));
    if downloaded_archive_sha256 != CFG_IF_CHECKSUM {
        return Err(invalid(format!(
            "cfg-if download checksum mismatch before local-registry writes: expected {CFG_IF_CHECKSUM}, got {downloaded_archive_sha256}"
        )));
    }

    let local_registry = stage.join("local-registry");
    let index = local_registry.join("index");
    let entry = index.join("cf/g-/cfg-if");
    fs::create_dir_all(entry.parent().unwrap())?;
    fs::write(index.join("config.json"), &config_bytes)?;
    fs::write(&entry, &index_bytes)?;
    let archive = local_registry.join("cfg-if-1.0.4.crate");
    let verified_archive_sha256 = write_verified_archive(&archive_bytes, &archive)?;

    let normal_home = stage.join("fetch-temp/local-registry-home");
    fs::create_dir_all(&normal_home)?;
    fs::write(
        normal_home.join("config.toml"),
        source_config(&local_registry, "motor-local"),
    )?;
    let target = stage.join("cargo-target/local-registry");
    let temp = stage.join("fetch-temp");
    let fetch = run_cargo(cargo, rustc, fixture, &normal_home, &target, &temp, "fetch");
    let lock_unchanged_after_fetch = lockfile_matches(&lock_path, &lock_before);
    let mut report = json!({
        "schema": 1,
        "downloaded_sources": {
            "index_config": {
                "url": "https://index.crates.io/config.json",
                "bytes": config_bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(&config_bytes))
            },
            "crate_index_entry": {
                "url": "https://index.crates.io/cf/g-/cfg-if",
                "registry_path": "index/cf/g-/cfg-if",
                "bytes": index_bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(&index_bytes)),
                "cfg_if_1_0_4_checksum_matches_lockfile": true
            },
            "crate_archive": {
                "url": "https://static.crates.io/crates/cfg-if/cfg-if-1.0.4.crate",
                "registry_path": "cfg-if-1.0.4.crate",
                "bytes": archive_bytes.len(),
                "sha256": verified_archive_sha256,
                "checksum_matches_lockfile": true,
                "checksum_verified_before_local_registry_write": true
            }
        },
        "local_registry": local_registry,
        "cargo_home_before_commands": {"path": normal_home, "files": ["config.toml"]},
        "cargo_fetch_locked_offline": fetch,
        "lockfile_unchanged_after_fetch": lock_unchanged_after_fetch,
        "cargo_build_locked_offline": null,
        "lockfile_unchanged_after_build": null,
        "checksum_negative_controls": {},
        "repository_source_override_test": null
    });
    write_report(report_path, &report)?;

    let build = run_cargo(cargo, rustc, fixture, &normal_home, &target, &temp, "build");
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
    let refused = write_verified_archive(&changed_archive, &refused_path);
    let runner_refused_before_write = refused.is_err() && !refused_path.exists();

    let corrupt_registry = stage.join("corrupt-local-registry");
    fs::create_dir_all(corrupt_registry.join("index/cf/g-"))?;
    fs::write(corrupt_registry.join("index/config.json"), &config_bytes)?;
    fs::write(corrupt_registry.join("index/cf/g-/cfg-if"), &index_bytes)?;
    fs::write(
        corrupt_registry.join("cfg-if-1.0.4.crate"),
        &changed_archive,
    )?;
    let corrupt_home = stage.join("fetch-temp/corrupt-registry-home");
    fs::create_dir_all(&corrupt_home)?;
    fs::write(
        corrupt_home.join("config.toml"),
        source_config(&corrupt_registry, "motor-local-corrupt"),
    )?;
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
    fs::write(empty_registry.join("index/config.json"), &config_bytes)?;
    let repository_config_dir = fixture.join(".cargo");
    fs::create_dir_all(&repository_config_dir)?;
    fs::write(
        repository_config_dir.join("config.toml"),
        source_config(&empty_registry, "repository-override"),
    )?;
    let override_home = stage.join("fetch-temp/repository-override-home");
    fs::create_dir_all(&override_home)?;
    fs::write(
        override_home.join("config.toml"),
        source_config(&local_registry, "motor-local"),
    )?;
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
        report["cargo_fetch_locked_offline"]["success"] == true
            && report["cargo_build_locked_offline"]["success"] == true
            && report["lockfile_unchanged_after_fetch"] == true
            && report["lockfile_unchanged_after_build"] == true
            && report["checksum_negative_controls"]["runner_refused_before_write"] == true
            && report["checksum_negative_controls"]["cargo_rejected_changed_archive_checksum"]
                == true
            && report["repository_source_override_test"]["check_started"] == true
            && report["repository_source_override_test"]["lockfile_unchanged"] == true
    );
    write_report(report_path, &report)?;
    Ok(report)
}
