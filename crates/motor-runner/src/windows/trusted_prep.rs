//! Windows job preparation fetches lockfile-selected registry artifacts as SYSTEM.
//! Cargo runs only in the offline guest or in explicitly selected diagnostics.
use super::*;
use std::{
    collections::BTreeMap,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE, WAIT_OBJECT_0},
    Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB},
    Security::{
        CreateRestrictedToken, GetTokenInformation, LogonUserW, TokenUser, DISABLE_MAX_PRIVILEGE,
        LOGON32_LOGON_BATCH, LOGON32_PROVIDER_DEFAULT, SID_AND_ATTRIBUTES, TOKEN_USER,
        WRITE_RESTRICTED,
    },
    System::{JobObjects::*, Threading::*},
};
mod local_registry;
mod system;

const LOCAL_REGISTRY_CACHE_DIRECTORY: &str = "local-registry-cache";
fn checked(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

struct BatchHandle(HANDLE);
impl Drop for BatchHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn batch_token(root: &Path, restrict: bool) -> io::Result<BatchHandle> {
    let mut bytes = fs::read(root.join("prep-secret.dpapi"))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { zeroed() };
    unsafe {
        checked(CryptUnprotectData(
            &input,
            null_mut(),
            null(),
            null(),
            null(),
            0,
            &mut output,
        ))?;
    }
    let secret = unsafe { std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize) };
    let mut password: Vec<u16> = secret
        .iter()
        .map(|byte| u16::from(*byte))
        .chain(Some(0))
        .collect();
    for byte in secret {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    unsafe { LocalFree(output.pbData.cast()) };
    let mut result = null_mut();
    let success = unsafe {
        LogonUserW(
            wide("motor-prep").as_ptr(),
            wide(".").as_ptr(),
            password.as_ptr(),
            LOGON32_LOGON_BATCH,
            LOGON32_PROVIDER_DEFAULT,
            &mut result,
        )
    };
    for unit in &mut password {
        unsafe { std::ptr::write_volatile(unit, 0) };
    }
    checked(success)?;
    let base = BatchHandle(result);
    if !restrict {
        return Ok(base);
    }
    let mut required = 0;
    unsafe { GetTokenInformation(base.0, TokenUser, null_mut(), 0, &mut required) };
    let mut user = vec![0u64; (required as usize).div_ceil(8)];
    unsafe {
        checked(GetTokenInformation(
            base.0,
            TokenUser,
            user.as_mut_ptr().cast(),
            required,
            &mut required,
        ))?;
        let sid = (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let restriction = SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0,
        };
        let mut restricted = null_mut();
        // For each permitted write, the account's Windows security identifier
        // must be allowed by both the file's normal access list and the restricted token.
        checked(CreateRestrictedToken(
            base.0,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0,
            null(),
            0,
            null(),
            1,
            &restriction,
            &mut restricted,
        ))?;
        Ok(BatchHandle(restricted))
    }
}

/// Starts one isolated `motor-prep` batch process and waits for its exit.
fn run_as_batch(
    config: &Config,
    command: &mut Command,
    cwd: &Path,
    restrict: bool,
    timeout: Duration,
) -> io::Result<()> {
    let root = &config.windows.as_ref().unwrap().layout_root;
    let token = batch_token(root, restrict)?;
    let program = command
        .get_program()
        .to_str()
        .ok_or_else(|| invalid("non-UTF8 prep program"))?;
    let mut args = vec![motor_winagent::quote_windows_arg(program)];
    for arg in command.get_args() {
        args.push(motor_winagent::quote_windows_arg(
            arg.to_str()
                .ok_or_else(|| invalid("non-UTF8 prep argument"))?,
        ));
    }
    let mut line = wide(&args.join(" "));
    let mut env = BTreeMap::new();
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            env.insert(
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }
    }
    let environment = motor_winagent::environment::encode_block(&env)?;
    unsafe {
        let job = BatchHandle(CreateJobObjectW(null(), null()));
        if job.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 32;
        checked(SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ))?;
        let mut startup: STARTUPINFOW = zeroed();
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        let mut info: PROCESS_INFORMATION = zeroed();
        checked(CreateProcessAsUserW(
            token.0,
            wide(program).as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            wide(&cwd.to_string_lossy()).as_ptr(),
            &startup,
            &mut info,
        ))?;
        let process = BatchHandle(info.hProcess);
        let thread = BatchHandle(info.hThread);
        if AssignProcessToJobObject(job.0, process.0) == 0 {
            let error = io::Error::last_os_error();
            TerminateProcess(process.0, 1);
            WaitForSingleObject(process.0, 5000);
            return Err(error);
        }
        if ResumeThread(thread.0) == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + timeout;
        while WaitForSingleObject(process.0, 50) != WAIT_OBJECT_0 {
            if Instant::now() >= deadline {
                return Err(invalid(format!(
                    "Windows trusted preparation exceeded {} seconds",
                    timeout.as_secs()
                )));
            }
        }
        let mut code = 0;
        checked(GetExitCodeProcess(process.0, &mut code))?;
        if code != 0 {
            return Err(invalid(format!(
                "Windows trusted preparation process exited {code}"
            )));
        }
    }
    Ok(())
}

pub(super) fn log_phase(progress_path: &Path, phase: &str) -> io::Result<()> {
    use std::io::Write;

    let timestamp_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let entry = serde_json::json!({"timestamp_unix_ms": timestamp_unix_ms, "phase": phase});
    let bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(progress_path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;

    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "DIAG local-registry phase={phase} timestamp_unix_ms={timestamp_unix_ms}"
    );
    let _ = stdout.flush();
    Ok(())
}

fn lock_exclusive_bounded(lock: &fs::File, operation: &str) -> io::Result<()> {
    use fs2::FileExt;

    let timeout = std::time::Duration::from_secs(30);
    let started = std::time::Instant::now();
    loop {
        match FileExt::try_lock_exclusive(lock) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out waiting 30 seconds for {operation}"),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn remove_staging(_config: &Config, path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_attributes() & 0x400 != 0
        || fs::symlink_metadata(target)
            .is_ok_and(|metadata| metadata.file_attributes() & 0x400 != 0)
    {
        return Err(invalid("reparse point in trusted preparation cache"));
    }
    crate::filesystem::identity(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if let Ok(existing) = fs::metadata(target) {
            if existing.permissions().readonly() {
                let attributes = existing.file_attributes()
                    & !windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY;
                checked(unsafe {
                    windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(
                        wide(&target.to_string_lossy()).as_ptr(),
                        if attributes == 0 {
                            windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL
                        } else {
                            attributes
                        },
                    )
                })?;
            }
        }
        fs::copy(source, target)?;
    } else {
        return Err(invalid("special file in trusted preparation input"));
    }
    Ok(())
}
pub(super) fn copy_routes(
    staging: &Path,
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<BTreeMap<String, PathBuf>> {
    let mut result = BTreeMap::new();
    let destination = staging.join("cargo-home/git-mirrors");
    fs::create_dir_all(&destination)?;
    for (index, (url, mirror)) in routes.iter().enumerate() {
        let copy = destination.join(index.to_string());
        copy_tree(mirror, &copy)?;
        result.insert(url.clone(), copy);
    }
    Ok(result)
}

const OFFLINE_CARGO_MANIFEST: &str = "[package]\nname = \"motor-runner-offline-cache\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncfg-if = \"=1.0.4\"\n";
const OFFLINE_CARGO_LOCK: &str = "version = 4\n\n[[package]]\nname = \"cfg-if\"\nversion = \"1.0.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"9330f8b2ff13f34540b44e946ef35111825727b38d33286ef986142615121801\"\n\n[[package]]\nname = \"motor-runner-offline-cache\"\nversion = \"0.1.0\"\ndependencies = [\n \"cfg-if\",\n]\n";

fn offline_cargo_layout(cargo_home: &Path) -> serde_json::Value {
    use sha2::{Digest, Sha256};

    const EXPECTED_CHECKSUM: &str =
        "9330f8b2ff13f34540b44e946ef35111825727b38d33286ef986142615121801";

    fn files_below(root: &Path) -> (Vec<PathBuf>, Vec<serde_json::Value>) {
        let mut pending = vec![root.to_path_buf()];
        let mut files = Vec::new();
        let mut errors = Vec::new();
        while let Some(directory) = pending.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    errors.push(serde_json::json!({
                        "path": directory,
                        "error": error.to_string()
                    }));
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        errors.push(serde_json::json!({
                            "path": directory,
                            "error": error.to_string()
                        }));
                        continue;
                    }
                };
                let path = entry.path();
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => pending.push(path),
                    Ok(kind) if kind.is_file() => files.push(path),
                    Ok(_) => {}
                    Err(error) => errors.push(serde_json::json!({
                        "path": path,
                        "error": error.to_string()
                    })),
                }
            }
        }
        files.sort();
        (files, errors)
    }

    fn relative(root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn hex_evidence(bytes: &[u8]) -> String {
        bytes
            .iter()
            .take(65_536)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    let index_root = cargo_home.join("registry/index");
    let archive_root = cargo_home.join("registry/cache");
    let (index_files, mut discovery_errors) = files_below(&index_root);
    let (archive_files, archive_errors) = files_below(&archive_root);
    discovery_errors.extend(archive_errors);
    let sparse_entries = index_files
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| name == "cfg-if")
                && path
                    .components()
                    .any(|part| part.as_os_str() == ".cache")
        })
        .map(|path| match fs::read(path) {
            Ok(bytes) => serde_json::json!({
                "path": relative(cargo_home, path),
                "readable": true,
                "bytes": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(&bytes)),
                "contents_hex_up_to_64_kib": hex_evidence(&bytes),
                "contents_truncated": bytes.len() > 65_536,
                "prefix_escaped_first_96_bytes": format!("{:?}", String::from_utf8_lossy(&bytes[..bytes.len().min(96)]))
            }),
            Err(error) => serde_json::json!({
                "path": relative(cargo_home, path),
                "readable": false,
                "read_error": error.to_string()
            }),
        })
        .collect::<Vec<_>>();
    let registry_config = index_files
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "config.json"))
        .map(|path| match fs::read_to_string(path) {
            Ok(contents) => serde_json::json!({
                "path": relative(cargo_home, path),
                "contents": contents
            }),
            Err(error) => serde_json::json!({
                "path": relative(cargo_home, path),
                "read_error": error.to_string()
            }),
        })
        .collect::<Vec<_>>();
    let archives = archive_files
        .iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == "cfg-if-1.0.4.crate")
        })
        .map(|path| match fs::read(path) {
            Ok(bytes) => {
                let checksum = format!("{:x}", Sha256::digest(bytes));
                let matches = checksum == EXPECTED_CHECKSUM;
                serde_json::json!({
                    "path": relative(cargo_home, path),
                    "sha256": checksum,
                    "checksum_matches_lockfile": matches
                })
            }
            Err(error) => serde_json::json!({
                "path": relative(cargo_home, path),
                "sha256": null,
                "checksum_matches_lockfile": false,
                "read_error": error.to_string()
            }),
        })
        .collect::<Vec<_>>();
    let archive_ready = archives
        .iter()
        .any(|entry| entry["checksum_matches_lockfile"] == true);
    serde_json::json!({
        "cargo_home": cargo_home,
        "file_discovery_errors": discovery_errors,
        "registry_config_files": registry_config,
        "sparse_index_cache_entries_named_cfg_if": sparse_entries,
        "crate_archives_named_cfg_if_1_0_4": archives,
        "sparse_index_entry_present": !sparse_entries.is_empty(),
        "locked_crate_archive_present_and_verified": archive_ready,
        "lockfile_crates_io_dependencies": [{
            "name": "cfg-if", "version": "1.0.4", "sha256": EXPECTED_CHECKSUM
        }],
        "sparse_cache_format_evidence": "Cargo-created bytes are reported as found; read failures are explicit and no cache bytes are synthesized"
    })
}

fn copy_cargo_cache(
    source: &Path,
    destination: &Path,
    include_index: bool,
    include_sparse_cache: bool,
) -> io::Result<()> {
    for subtree in ["registry/cache", "registry/index"] {
        if subtree == "registry/index" && !include_index {
            continue;
        }
        let from = source.join(subtree);
        if !from.is_dir() {
            continue;
        }
        for path in {
            let mut paths = vec![from.clone()];
            let mut files = Vec::new();
            while let Some(directory) = paths.pop() {
                for entry in fs::read_dir(directory)? {
                    let entry = entry?;
                    let path = entry.path();
                    let kind = entry.file_type()?;
                    if kind.is_dir() {
                        paths.push(path);
                    } else if kind.is_file() {
                        files.push(path);
                    }
                }
            }
            files
        } {
            let relative = path.strip_prefix(source).map_err(invalid)?;
            if subtree == "registry/index"
                && !include_sparse_cache
                && !path.file_name().is_some_and(|name| name == "config.json")
            {
                continue;
            }
            let target = destination.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, target)?;
        }
    }
    Ok(())
}

fn run_offline_cargo(
    cargo: &Path,
    rustc: &Path,
    fixture: &Path,
    cargo_home: &Path,
    stage: &Path,
) -> serde_json::Value {
    let mut command = Command::new(cargo);
    command
        .current_dir(fixture)
        .env_clear()
        .env("SYSTEMROOT", r"C:\Windows")
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_REGISTRIES_CRATES_IO_PROTOCOL", "sparse")
        .env("RUSTC", rustc)
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .env("TEMP", stage.join("fetch-temp"))
        .env("TMP", stage.join("fetch-temp"))
        .args(["fetch", "--locked", "--offline", "--manifest-path"])
        .arg(fixture.join("Cargo.toml"));
    match crate::native_process::output_with_timeout(
        &mut command,
        std::time::Duration::from_secs(20),
        || false,
    ) {
        Ok(output) => serde_json::json!({
            "process_started": true,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "network_mode": "offline; CARGO_NET_OFFLINE=true"
        }),
        Err(error) => serde_json::json!({
            "process_started": null,
            "completed": false,
            "success": false,
            "error": error.to_string(),
            "network_mode": "offline; CARGO_NET_OFFLINE=true"
        }),
    }
}

pub(super) fn prepare_local_registry(
    config: &Config,
    staging: &Path,
    lockfile: &str,
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    system::require_system()?;
    let vm = config.windows.as_ref().unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(vm.layout_root.join("prep.lock"))?;
    lock_exclusive_bounded(&lock, "Windows local-registry cache lock")?;
    let cache = config.state_dir().join(LOCAL_REGISTRY_CACHE_DIRECTORY);
    let cargo_home = staging.join("cargo-home");
    let stats = local_registry::prepare_for_job(&cache, staging, &cargo_home, lockfile, routes)?;
    tracing::info!(
        index_files = stats.index_files_downloaded.len(),
        crate_archives = stats.archives_downloaded.len(),
        bytes_downloaded = stats.downloaded_bytes,
        "prepared Windows local Cargo registry"
    );
    Ok(())
}

pub(super) fn offline_cargo_entry(config: Config) -> io::Result<()> {
    use fs2::FileExt;
    use sha2::{Digest, Sha256};
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    system::require_system()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(io::Error::other)?;
    let status = manager
        .open_service("MotorRunner", ServiceAccess::QUERY_STATUS)
        .and_then(|service| service.query_status())
        .map_err(io::Error::other)?;
    if status.current_state != ServiceState::Stopped {
        return Err(invalid(
            "Stop MotorRunner before the offline Cargo cache diagnostic",
        ));
    }
    let runner = crate::Runner::open(config)?;
    let config = runner.config();
    let vm = config.windows.as_ref().unwrap();
    let root = &vm.layout_root;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("prep.lock"))?;
    lock.lock_exclusive()?;

    let cargo = root.join("prep-toolchain/bin/cargo.exe");
    let rustc = root.join("prep-toolchain/bin/rustc.exe");
    let cargo_sha256 = format!("{:x}", Sha256::digest(fs::read(&cargo)?));
    if cargo_sha256 != vm.prep_cargo_sha256.to_ascii_lowercase() {
        return Err(invalid("offline diagnostic prep cargo SHA-256 mismatch"));
    }
    let rustc_sha256 = format!("{:x}", Sha256::digest(fs::read(&rustc)?));

    let id = Uuid::now_v7();
    let stage = root.join("staging").join(id.to_string());
    let fixture = stage.join("fetch-manifests/offline-cargo");
    let seed_home = stage.join("fetch-temp/seed-cargo-home");
    let report_path = root
        .join("evidence")
        .join(format!("prep-offline-cargo-{id}.json"));
    let mut report = serde_json::json!({
        "diagnostic_only": true,
        "production_policy_unchanged": true,
        "actor": "SYSTEM runner; Cargo runs only as SYSTEM in this diagnostic",
        "restricted_motor_prep_cargo_launch_attempted": false,
        "offline_child_network_mode": {"cargo_argument": "--offline", "CARGO_NET_OFFLINE": "true"},
        "fixture": {"manifest": fixture.join("Cargo.toml"), "lockfile": fixture.join("Cargo.lock"), "dependency": "cfg-if 1.0.4 from crates.io"},
        "toolchain": {"cargo": cargo, "cargo_sha256": cargo_sha256, "rustc": rustc,
            "rustc_sha256": rustc_sha256, "cargo_pin": vm.prep_cargo_sha256},
        "seed_cargo_home": seed_home,
        "cells": {},
        "staging": stage,
        "staging_removed": false
    });
    system::write_diagnostic_update(&report_path, &report)?;

    let result: io::Result<()> = (|| {
        fs::create_dir_all(fixture.join("src"))?;
        fs::create_dir_all(&seed_home)?;
        fs::create_dir_all(stage.join("fetch-temp"))?;
        fs::write(fixture.join("Cargo.toml"), OFFLINE_CARGO_MANIFEST)?;
        fs::write(fixture.join("Cargo.lock"), OFFLINE_CARGO_LOCK)?;
        fs::write(fixture.join("src/lib.rs"), "pub fn offline_fixture() {}\n")?;

        let mut prefetch = Command::new(&cargo);
        prefetch
            .current_dir(&fixture)
            .env_clear()
            .env("SYSTEMROOT", r"C:\Windows")
            .env("CARGO_HOME", &seed_home)
            .env("CARGO_NET_OFFLINE", "false")
            .env("CARGO_REGISTRIES_CRATES_IO_PROTOCOL", "sparse")
            .env("CARGO_HTTP_TIMEOUT", "20")
            .env("RUSTC", &rustc)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "")
            .env("TEMP", stage.join("fetch-temp"))
            .env("TMP", stage.join("fetch-temp"))
            .args(["fetch", "--locked", "--manifest-path"])
            .arg(fixture.join("Cargo.toml"));
        let prefetch = crate::native_process::output_with_timeout(
            &mut prefetch,
            std::time::Duration::from_secs(45),
            || false,
        );
        report["cells"]["system_locked_prefetch"] = match prefetch {
            Ok(output) => serde_json::json!({
                "process_started": true,
                "success": output.status.success(),
                "exit_code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "network_mode": "SYSTEM Cargo fetch; no --offline flag"
            }),
            Err(error) => serde_json::json!({
                "process_started": null,
                "completed": false,
                "success": false,
                "error": error.to_string(),
                "network_mode": "SYSTEM Cargo fetch; no --offline flag"
            }),
        };
        report["observed_seed_layout"] = offline_cargo_layout(&seed_home);
        system::write_diagnostic_update(&report_path, &report)?;

        let cases = [
            ("crate_archive_only", false, false),
            ("archive_plus_registry_config_no_sparse_cache", true, false),
            ("complete_cargo_seed", true, true),
        ];
        for (name, include_index, include_sparse_cache) in cases {
            let home = stage.join("fetch-temp").join(name);
            fs::create_dir_all(&home)?;
            copy_cargo_cache(&seed_home, &home, include_index, include_sparse_cache)?;
            let layout = offline_cargo_layout(&home);
            let outcome = run_offline_cargo(&cargo, &rustc, &fixture, &home, &stage);
            report["cells"][name] = serde_json::json!({
                "layout": layout,
                "offline_fetch": outcome
            });
            system::write_diagnostic_update(&report_path, &report)?;
        }
        Ok(())
    })();

    let cleanup = remove_staging(config, &stage);
    report["staging_removed"] = serde_json::json!(cleanup.is_ok() && !stage.exists());
    report["staging_leftover"] = if stage.exists() {
        serde_json::json!(stage)
    } else {
        serde_json::Value::Null
    };
    if let Err(error) = &result {
        report["error"] = serde_json::json!(error.to_string());
    }
    system::write_diagnostic_update(&report_path, &report)?;
    println!(
        "DIAG offline-cargo-cache report={} staging_removed={}",
        report_path.display(),
        report["staging_removed"]
    );
    result?;
    cleanup
}

/// SYSTEM-only proof for building a Cargo local registry from direct downloads.
pub(super) fn local_registry_entry(config: Config) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let root = config
        .state_root
        .parent()
        .ok_or_else(|| invalid("native Windows state root has no parent"))?;
    if root != Path::new(r"C:\ProgramData\MotorRunner") {
        return Err(invalid(
            "local-registry diagnostic requires the installed runner root",
        ));
    }
    let id = Uuid::now_v7();
    let stage = root.join("staging").join(id.to_string());
    let fixture = stage.join("fetch-manifests/offline-cargo");
    let report_path = root
        .join("evidence")
        .join(format!("prep-local-registry-{id}.json"));
    let progress_path = root
        .join("evidence")
        .join(format!("prep-local-registry-{id}.progress.jsonl"));
    let mut report = serde_json::json!({
        "diagnostic_only": true,
        "production_policy_unchanged": true,
        "actor": "SYSTEM runner; pinned Cargo runs only in this disposable proof",
        "phase_log": progress_path,
        "fixture": {"manifest": fixture.join("Cargo.toml"), "lockfile": fixture.join("Cargo.lock"), "dependency": "cfg-if 1.0.4 from crates.io"},
        "toolchain": null,
        "staging": stage,
        "staging_removed": false
    });
    log_phase(&progress_path, "diagnostic-start")?;
    system::write_diagnostic_update(&report_path, &report)?;

    let result: io::Result<()> = (|| {
        log_phase(&progress_path, "verify-system-token")?;
        system::require_system()?;
        if config.instance != "ally"
            || config.backend != crate::BackendKind::WindowsVm
            || config.state_root != root.join("state")
        {
            return Err(invalid(
                "local-registry diagnostic requires the installed ally Windows runner configuration",
            ));
        }

        log_phase(&progress_path, "load-pinned-vm-configuration")?;
        // Opening a Runner would rerun Hyper-V Validate and hash the large parent
        // even though this diagnostic only exercises the local Cargo registry.
        let vm_config_path = config
            .vm_config
            .as_deref()
            .ok_or_else(|| invalid("local-registry diagnostic requires --vm-config"))?;
        let vm = super::VmConfig::load(vm_config_path)?;
        if vm.layout_root.as_path() != root {
            return Err(invalid(
                "local-registry VM configuration names a different runner root",
            ));
        }

        log_phase(&progress_path, "query-runner-service-state")?;
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(io::Error::other)?;
        let status = manager
            .open_service("MotorRunner", ServiceAccess::QUERY_STATUS)
            .and_then(|service| service.query_status())
            .map_err(io::Error::other)?;
        if status.current_state != ServiceState::Stopped {
            return Err(invalid(
                "Stop MotorRunner before the local-registry Cargo proof",
            ));
        }

        log_phase(&progress_path, "open-preparation-lock")?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("prep.lock"))?;
        log_phase(&progress_path, "wait-for-preparation-lock")?;
        lock_exclusive_bounded(&lock, "local-registry proof prep.lock")?;

        let cargo = root.join("prep-toolchain/bin/cargo.exe");
        let rustc = root.join("prep-toolchain/bin/rustc.exe");
        log_phase(&progress_path, "hash-pinned-cargo-executable")?;
        let cargo_sha256 = format!("{:x}", Sha256::digest(fs::read(&cargo)?));
        if cargo_sha256 != vm.prep_cargo_sha256.to_ascii_lowercase() {
            return Err(invalid("local-registry proof prep cargo SHA-256 mismatch"));
        }
        log_phase(&progress_path, "hash-pinned-rustc-executable")?;
        let rustc_sha256 = format!("{:x}", Sha256::digest(fs::read(&rustc)?));

        report["toolchain"] = serde_json::json!({
            "cargo": cargo,
            "cargo_sha256": cargo_sha256,
            "cargo_pin": vm.prep_cargo_sha256,
            "rustc": rustc,
            "rustc_sha256": rustc_sha256
        });
        system::write_diagnostic_update(&report_path, &report)?;

        log_phase(&progress_path, "create-disposable-cargo-fixture")?;
        fs::create_dir_all(fixture.join("src"))?;
        fs::create_dir_all(stage.join("fetch-temp"))?;
        fs::write(fixture.join("Cargo.toml"), OFFLINE_CARGO_MANIFEST)?;
        fs::write(fixture.join("Cargo.lock"), OFFLINE_CARGO_LOCK)?;
        fs::write(fixture.join("src/lib.rs"), "pub fn offline_fixture() {}\n")?;

        log_phase(&progress_path, "run-local-registry-cargo-proof")?;
        let proof = local_registry::prove(
            &cargo,
            &rustc,
            &fixture,
            &stage,
            &report_path,
            &progress_path,
        )?;
        let passed = proof["proof_passed"] == true;
        report["local_registry_proof"] = proof;
        if !passed {
            return Err(invalid(
                "local-registry Cargo proof failed; inspect its evidence report",
            ));
        }
        Ok(())
    })();

    let cleanup_phase = log_phase(&progress_path, "cleanup-disposable-staging");
    let cleanup = remove_staging(&config, &stage);
    if let Err(error) = cleanup_phase {
        report["progress_log_error"] = serde_json::json!(error.to_string());
    }
    report["staging_removed"] = serde_json::json!(cleanup.is_ok() && !stage.exists());
    report["staging_leftover"] = if stage.exists() {
        serde_json::json!(stage)
    } else {
        serde_json::Value::Null
    };
    if let Err(error) = &result {
        report["error"] = serde_json::json!(error.to_string());
    }
    if let Err(error) = &cleanup {
        report["cleanup_error"] = serde_json::json!(error.to_string());
    }
    system::write_diagnostic_update(&report_path, &report)?;
    log_phase(&progress_path, "diagnostic-finished")?;
    println!(
        "DIAG local-registry report={} progress={} staging_removed={}",
        report_path.display(),
        progress_path.display(),
        report["staging_removed"]
    );
    result?;
    cleanup
}

/// Tests normal batch file access and verifies the restricted token cannot
/// read the credential or write outside staging.
pub(super) fn qualify(config: &Config) -> io::Result<()> {
    let root = &config.windows.as_ref().unwrap().layout_root;
    let stage = root.join("staging").join(Uuid::now_v7().to_string());
    fs::create_dir_all(stage.join("fetch-manifests"))?;
    fs::create_dir_all(stage.join("fetch-temp"))?;
    let credential = root.join("prep-secret.dpapi");
    drop(fs::File::open(&credential)?);
    let outside = PathBuf::from(r"C:\Windows\Temp")
        .join(format!("motor-prep-outside-{}.txt", Uuid::now_v7()));
    let probe = stage.join("fetch-manifests/probe.exe");
    fs::copy(std::env::current_exe()?, &probe)?;
    let result = (|| {
        super::vm::checked(super::vm::control(
            config,
            "GrantPrep",
            None,
            &["-Staging".into(), stage.to_string_lossy().into()],
        )?)?;
        for (mode, restrict) in [("base-control", false), ("restricted", true)] {
            let output = stage.join(format!("fetch-temp/{mode}.json"));
            let mut command = Command::new(&probe);
            command
                .arg("prep-probe")
                .arg(mode)
                .arg(&credential)
                .arg(&outside)
                .arg(&output)
                .env_clear()
                .env("SYSTEMROOT", r"C:\Windows");
            run_as_batch(
                config,
                &mut command,
                &stage,
                restrict,
                Duration::from_secs(600),
            )?;
            let bytes = fs::read(&output)?;
            fs::write(root.join(format!("evidence/prep-{mode}.json")), &bytes)?;
            let proof: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
            let expected = if restrict { "denied" } else { "allowed" };
            if proof["outside_write"] != expected
                || proof["allowed_write"] != "allowed"
                || (restrict && proof["credential_read"] != "denied")
            {
                return Err(invalid(
                    "prep qualification did not exercise the expected native access checks",
                ));
            }
        }
        if outside.exists() {
            return Err(invalid("prep qualification left an outside file"));
        }
        Ok(())
    })();
    if outside.exists() {
        fs::remove_file(&outside)?;
    }
    remove_staging(config, &stage)?;
    result
}

/// The child uses direct file operations so shell redirection failures cannot
/// be mistaken for denied credential reads or outside writes.
pub(crate) fn probe(arguments: &[String]) -> io::Result<()> {
    if arguments.len() != 4 {
        return Err(invalid(
            "prep-probe requires mode, credential, outside and result paths",
        ));
    }
    let mode = arguments[0].as_str();
    let credential = Path::new(&arguments[1]);
    let outside = Path::new(&arguments[2]);
    let output = Path::new(&arguments[3]);
    let proof = match mode {
        "base-control" => {
            fs::write(outside, b"unrestricted batch write control")?;
            fs::remove_file(outside)?;
            serde_json::json!({"outside_write":"allowed","allowed_write":"allowed"})
        }
        "restricted" => {
            match fs::File::open(credential) {
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
                _ => {
                    return Err(invalid(
                        "restricted batch token did not deny opening the real runner credential",
                    ))
                }
            }
            match fs::write(outside, b"must be denied") {
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
                _ => {
                    return Err(invalid(
                        "restricted batch token did not deny the controlled outside write",
                    ))
                }
            }
            serde_json::json!({"credential_read":"denied","outside_write":"denied","allowed_write":"allowed"})
        }
        _ => return Err(invalid("unknown prep-probe mode")),
    };
    fs::write(output, serde_json::to_vec(&proof).map_err(invalid)?)
}
