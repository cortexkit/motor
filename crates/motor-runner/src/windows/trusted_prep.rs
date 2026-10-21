//! Credential-free fetch process. Only SYSTEM decrypts the batch account secret.
use super::*;
use std::{
    collections::BTreeMap,
    mem::zeroed,
    ptr::{null, null_mut},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE},
    Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB},
    Security::{LogonUserW, LOGON32_LOGON_BATCH, LOGON32_PROVIDER_DEFAULT},
};
mod launch;
mod lowbox;
struct Handle(HANDLE);
impl Handle {
    fn close(&mut self) -> io::Result<()> {
        if !self.0.is_null() {
            checked(unsafe { CloseHandle(self.0) })?;
            self.0 = null_mut();
        }
        Ok(())
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
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

fn token(root: &Path) -> io::Result<Handle> {
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
    // The installer generates an ASCII hex secret. Avoid an additional String
    // allocation containing a password that cannot be erased before it drops.
    let mut password: Vec<u16> = secret
        .iter()
        .map(|byte| u16::from(*byte))
        .chain(Some(0))
        .collect();
    for byte in secret {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
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
        unsafe {
            std::ptr::write_volatile(unit, 0);
        }
    }
    checked(success)?;
    Ok(Handle(result))
}

/// Fetch-only batch child with a private desktop and loaded user profile.
/// Only controlled staging stdio handles are inherited; writes require the
/// logon SID granted inside staging. Suspend until Job Object assignment.
pub(super) fn fetch(config: &Config, command: &mut Command, cwd: &Path) -> io::Result<()> {
    run_as_batch(config, command, cwd, true)
}
fn run_as_batch(
    config: &Config,
    command: &mut Command,
    cwd: &Path,
    restrict: bool,
) -> io::Result<()> {
    launch::run(config, command, cwd, restrict).map(|_| ())
}

pub(super) fn launch_entry(arguments: &[String]) -> io::Result<()> {
    launch::entry(arguments)
}
pub(super) fn access_check_entry(arguments: &[String]) -> io::Result<()> {
    launch::access_check_entry(arguments)
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
    for (index, (url, mirror)) in routes.iter().enumerate() {
        let copy = staging.join("fetch-git-deps").join(index.to_string());
        copy_tree(mirror, &copy)?;
        result.insert(url.clone(), copy);
    }
    Ok(result)
}
fn copy_manifests(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            copy_manifests(&entry.path(), &target.join(entry.file_name()))?;
        } else if matches!(
            entry.file_name().to_str(),
            Some("Cargo.toml" | "Cargo.lock")
        ) {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        } else if entry.path().extension().is_some_and(|s| s == "rs") {
            // Cargo discovers targets while fetching; empty placeholders retain
            // that layout without exposing source code to the prep account.
            fs::write(target.join(entry.file_name()), b"")?;
        }
    }
    Ok(())
}
pub(super) fn prepare_fetch(
    config: &Config,
    staging: &Path,
    command: &mut Command,
) -> io::Result<()> {
    use fs2::FileExt;
    use sha2::{Digest, Sha256};
    let vm = config.windows.as_ref().unwrap();
    // VM execution can overlap, but one batch account must never retain grants
    // to two jobs. Serialize fetch, copy the result, then revoke the job grants.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(vm.layout_root.join("prep.lock"))?;
    lock.lock_exclusive()?;
    let cargo = vm.layout_root.join("prep-toolchain/bin/cargo.exe");
    let hash = format!("{:x}", Sha256::digest(fs::read(&cargo)?));
    if hash != vm.prep_cargo_sha256.to_ascii_lowercase() {
        return Err(invalid("standalone prep cargo SHA-256 mismatch"));
    }
    let manifests = staging.join("fetch-manifests");
    copy_manifests(&staging.join("m"), &manifests.join("m"))?;
    let cache = config.state_dir().join("prep-cargo");
    fs::create_dir_all(&cache)?;
    // The child never gets write rights to the persistent host cache. SYSTEM
    // seeds/publishes only selected cache directories under the serialized lock.
    let fetch_cache = staging.join("fetch-temp/cargo-home");
    fs::create_dir_all(&fetch_cache)?;
    for name in [
        "registry/index",
        "registry/cache",
        "git/db",
        "git/checkouts",
    ] {
        let source = cache.join(name);
        if source.exists() {
            launch::check_path(&vm.layout_root, &source)?;
            copy_tree(&source, &fetch_cache.join(name))?;
        }
    }
    let mut fetch_command = Command::new(&cargo);
    fetch_command.env_clear();
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            fetch_command.env(key, value);
        }
    }
    let mut manifest_arg = false;
    for arg in command.get_args() {
        if manifest_arg {
            let manifest = Path::new(arg).strip_prefix(staging).map_err(invalid)?;
            fetch_command.arg(manifests.join(manifest));
            manifest_arg = false;
        } else {
            fetch_command.arg(arg);
            manifest_arg = arg == "--manifest-path";
        }
    }
    fetch_command
        .env("CARGO_HOME", &fetch_cache)
        .env("SYSTEMROOT", r"C:\Windows")
        .env("TEMP", staging.join("fetch-temp"))
        .env("TMP", staging.join("fetch-temp"))
        .env("RUSTC", vm.layout_root.join("prep-toolchain/bin/rustc.exe"))
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "");
    fs::create_dir_all(staging.join("fetch-temp"))?;
    super::vm::checked(super::vm::control(
        config,
        "GrantPrep",
        None,
        &["-Staging".into(), staging.to_string_lossy().into()],
    )?)?;
    let fetched = (|| {
        fetch(config, &mut fetch_command, staging)?;
        for name in [
            "registry/index",
            "registry/cache",
            "git/db",
            "git/checkouts",
        ] {
            let source = fetch_cache.join(name);
            if source.exists() {
                launch::check_path(&vm.layout_root, &source)?;
                launch::check_target(&vm.layout_root, &cache.join(name))?;
                launch::check_target(&vm.layout_root, &staging.join("cargo-home").join(name))?;
                copy_tree(&source, &cache.join(name))?;
                copy_tree(&source, &staging.join("cargo-home").join(name))?;
            }
        }
        Ok(())
    })();
    super::vm::checked(super::vm::control(
        config,
        "RevokePrep",
        None,
        &["-Staging".into(), staging.to_string_lossy().into()],
    )?)?;
    fetched
}

/// Executes the same restricted batch launch used for Cargo. An unrestricted
/// batch-token control must first prove that the outside test path is writable.
pub(super) fn qualify(config: &Config) -> io::Result<()> {
    let root = &config.windows.as_ref().unwrap().layout_root;
    let stage = root.join("staging").join(Uuid::now_v7().to_string());
    fs::create_dir_all(stage.join("fetch-manifests"))?;
    fs::create_dir_all(stage.join("fetch-temp"))?;
    fs::create_dir_all(config.state_dir().join("prep-cargo"))?;
    let credential = root.join("prep-secret.dpapi");
    // Open as SYSTEM without reading bytes: the real protected file must exist.
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
        for (mode, restricted) in [("base-control", false), ("restricted", true)] {
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
            let started = Instant::now();
            eprintln!("prep qualification batch probe mode={mode} starting");
            run_as_batch(config, &mut command, &stage, restricted)?;
            eprintln!(
                "prep qualification batch probe mode={mode} completed elapsed_ms={}",
                started.elapsed().as_millis()
            );
            let bytes = fs::read(&output)?;
            fs::write(root.join(format!("evidence/prep-{mode}.json")), &bytes)?;
            let proof: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
            let expected = if restricted { "denied" } else { "allowed" };
            let expected_sids = if restricted {
                serde_json::json!([proof["logon_sid"]])
            } else {
                serde_json::json!([])
            };
            if proof["outside_write"] != expected
                || proof["allowed_write"] != "allowed"
                || (restricted && proof["credential_read"] != "denied")
                || proof["profile_write"] != expected
                || proof["profile_environment"] != true
                || proof["window_station_visible"] != false
                || proof["restricting_sids"] != expected_sids
            {
                return Err(invalid(
                    "prep qualification did not exercise the expected native access checks",
                ));
            }
        }
        // The account/file probe does not exercise Cargo's loader imports.
        // Run the actual pinned executable through exactly the same broker.
        let cargo = root.join("prep-toolchain/bin/cargo.exe");
        use sha2::{Digest, Sha256};
        if format!("{:x}", Sha256::digest(fs::read(&cargo)?))
            != config
                .windows
                .as_ref()
                .unwrap()
                .prep_cargo_sha256
                .to_ascii_lowercase()
        {
            return Err(invalid("qualification prep cargo SHA-256 mismatch"));
        }
        let mut version = Command::new(&cargo);
        version
            .arg("--version")
            .env_clear()
            .env("SYSTEMROOT", r"C:\Windows")
            .env("CARGO_HOME", stage.join("fetch-temp/cargo-version"));
        let report = launch::run(config, &version, &stage, true)?;
        let stdout = report["stdout"].as_str().unwrap_or("").trim();
        if !stdout.starts_with("cargo ") {
            return Err(invalid(
                "native cargo --version fixture returned no Cargo version",
            ));
        }
        eprintln!("PASS native-trusted-fetch-cargo-version {stdout} status={} profile-loaded private-desktop write-restricted", report["exit_status"].as_str().unwrap_or("missing"));
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

/// Child-side fixture: direct file operations avoid shell redirection or NUL
/// permissions masquerading as a denied credential read or outside write.
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
    if matches!(mode, "rng-process-prng" | "rng-bcrypt-gen-random") {
        return launch::rng_probe(mode, output);
    }
    if mode == "lowbox" {
        return lowbox::probe(credential, outside, output);
    }
    let profile =
        std::env::var_os("USERPROFILE").ok_or_else(|| invalid("prep probe has no USERPROFILE"))?;
    let profile = PathBuf::from(profile).join(format!("motor-prep-probe-{}.txt", Uuid::now_v7()));
    let temp =
        PathBuf::from(std::env::var_os("TEMP").ok_or_else(|| invalid("prep probe has no TEMP"))?);
    let write_root = PathBuf::from(
        std::env::var_os("MOTOR_PREP_WRITE_ROOT")
            .ok_or_else(|| invalid("prep probe has no staging write root"))?,
    );
    if temp != write_root
        || std::env::var_os("TMP").map(PathBuf::from).as_ref() != Some(&write_root)
    {
        return Err(invalid("prep TEMP/TMP must point inside staging"));
    }
    let temp_probe = temp.join(format!("temp-probe-{}.txt", Uuid::now_v7()));
    fs::write(&temp_probe, b"staging temp write")?;
    fs::remove_file(&temp_probe)?;
    let mut proof = match mode {
        "diagnostic-writes" => {
            // Record allowed writes as evidence too; diagnostic completion is
            // never a permission-boundary pass. Remove every controlled canary.
            fn attempt(path: &Path) -> io::Result<&'static str> {
                match fs::write(path, b"diagnostic controlled write") {
                    Ok(()) => {
                        fs::remove_file(path)?;
                        Ok("allowed")
                    }
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok("denied"),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok("directory-absent"),
                    Err(error) => Err(error),
                }
            }
            let root = credential
                .parent()
                .ok_or_else(|| invalid("diagnostic credential root missing"))?;
            let stage = write_root
                .parent()
                .ok_or_else(|| invalid("diagnostic stage missing"))?;
            let mut surfaces = serde_json::Map::new();
            for (name, directory) in [
                ("persistent_prep_cache", root.join("state/ally/prep-cargo")),
                ("service_home", root.join("service-home")),
                ("fetch_manifests", stage.join("fetch-manifests")),
                ("prep_toolchain", root.join("prep-toolchain/bin")),
                (
                    "profile_local_temp",
                    profile.parent().unwrap().join("AppData/Local/Temp"),
                ),
            ] {
                let path = directory.join(format!("motor-write-diag-{}.txt", Uuid::now_v7()));
                let outcome = attempt(&path)?;
                surfaces.insert(
                    name.into(),
                    serde_json::json!({"path":path,"write":outcome}),
                );
            }
            serde_json::json!({"outside_write":attempt(outside)?,"profile_write":attempt(&profile)?,"allowed_write":"allowed", "temp_tmp_inside_staging":true, "write_surfaces":surfaces})
        }
        "base-control" => {
            fs::write(outside, b"unrestricted batch write control")?;
            fs::remove_file(outside)?;
            fs::write(&profile, b"unrestricted profile write control")?;
            fs::remove_file(&profile)?;
            serde_json::json!({"outside_write":"allowed","allowed_write":"allowed","profile_write":"allowed"})
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
            match fs::write(&profile, b"must be denied") {
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
                _ => {
                    let _ = fs::remove_file(&profile);
                    return Err(invalid(
                        "restricted fetch token must not write the loaded profile",
                    ));
                }
            }
            serde_json::json!({"credential_read":"denied","outside_write":"denied","allowed_write":"allowed","profile_write":"denied"})
        }
        _ => return Err(invalid("unknown prep-probe mode")),
    };
    let context = launch::probe_context()?;
    proof
        .as_object_mut()
        .unwrap()
        .extend(context.as_object().unwrap().clone());
    fs::write(output, serde_json::to_vec(&proof).map_err(invalid)?)
}

/// Diagnostic-only AppContainer launch cells; see `lowbox`.
pub(super) fn lowbox_entry(config: Config) -> io::Result<()> {
    lowbox::entry(config)
}
/// Diagnostic collection only: failed cells are retained, never treated as a
/// successful preparation or permission-policy qualification.
pub(super) fn matrix_entry(config: Config) -> io::Result<()> {
    use fs2::FileExt;
    use sha2::{Digest, Sha256};
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    launch::require_system()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(io::Error::other)?;
    let status = manager
        .open_service("MotorRunner", ServiceAccess::QUERY_STATUS)
        .and_then(|service| service.query_status())
        .map_err(io::Error::other)?;
    if status.current_state != ServiceState::Stopped {
        return Err(invalid(
            "Stop MotorRunner before the trusted-fetch diagnostic matrix",
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
    let id = Uuid::now_v7();
    let stage = root.join("staging").join(id.to_string());
    fs::create_dir_all(stage.join("fetch-manifests"))?;
    fs::create_dir_all(stage.join("fetch-temp"))?;
    fs::create_dir_all(config.state_dir().join("prep-cargo"))?;
    let probe = stage.join("fetch-manifests/probe.exe");
    fs::copy(std::env::current_exe()?, &probe)?;
    let fixture = stage.join("fetch-manifests/matrix-fixture");
    fs::create_dir_all(fixture.join("src"))?;
    fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname=\"trusted-fetch-matrix\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )?;
    fs::write(
        fixture.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"trusted-fetch-matrix\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(fixture.join("src/lib.rs"), "")?;
    let cargo = root.join("prep-toolchain/bin/cargo.exe");
    let cargo_sha256 = format!("{:x}", Sha256::digest(fs::read(&cargo)?));
    if cargo_sha256 != vm.prep_cargo_sha256.to_ascii_lowercase() {
        return Err(invalid("matrix cargo SHA-256 mismatch"));
    }
    let rustc = root.join("prep-toolchain/bin/rustc.exe");
    let toolchain_identity = serde_json::json!({"cargo_sha256":cargo_sha256,"cargo_pin":vm.prep_cargo_sha256,
        "rustc_sha256":format!("{:x}",Sha256::digest(fs::read(&rustc)?)),"source":"official prebuilt Rust components, not rustup proxies",
        "rustup_executable_present":root.join("prep-toolchain/bin/rustup.exe").exists()});
    let outside = PathBuf::from(r"C:\Windows\Temp").join(format!("motor-prep-matrix-{id}.txt"));
    let report_path = root
        .join("evidence")
        .join(format!("prep-launch-matrix-{id}.json"));
    let mut cells = Vec::new();
    let mut deferred_cleanup = false;
    let result = (|| {
        super::vm::checked(super::vm::control(
            config,
            "GrantPrep",
            None,
            &["-Staging".into(), stage.to_string_lossy().into()],
        )?)?;
        use launch::DiagnosticPolicy::{
            DefaultDacl, DefaultDaclBCryptRng, DefaultDaclCargoTwice, DefaultDaclProbeFirst,
            DefaultDaclProcessPrng, DefaultDaclRestrictedCode, DefaultDaclSafeCwdPair, Everyone,
            ExplicitDesktopWrites, FetchDefaultDaclRestrictedCode, None as Production,
            RestrictedCode,
        };
        for (index, name, program, restricted, policy) in [
            (1, "probe-base", &probe, false, Production),
            (2, "probe-write-restricted", &probe, true, Production),
            (3, "cargo-version-base", &cargo, false, Production),
            (
                4,
                "cargo-version-write-restricted",
                &cargo,
                true,
                Production,
            ),
            (
                5,
                "rustc-version-write-restricted",
                &rustc,
                true,
                Production,
            ),
            (
                6,
                "cargo-version-logon-and-everyone-diagnostic-only",
                &cargo,
                true,
                Everyone,
            ),
            (
                7,
                "cargo-version-explicit-desktop-write-mask-diagnostic-only",
                &cargo,
                true,
                ExplicitDesktopWrites,
            ),
            (
                8,
                "cargo-version-logon-default-dacl-diagnostic-only",
                &cargo,
                true,
                DefaultDacl,
            ),
            (
                9,
                "cargo-version-logon-and-restricted-code-diagnostic-only",
                &cargo,
                true,
                RestrictedCode,
            ),
            (
                10,
                "cargo-version-logon-restricted-code-default-dacl-diagnostic-only",
                &cargo,
                true,
                DefaultDaclRestrictedCode,
            ),
            (
                11,
                "cargo-fetch-logon-restricted-code-default-dacl-diagnostic-only",
                &cargo,
                true,
                FetchDefaultDaclRestrictedCode,
            ),
            (
                12,
                "default-dacl-runner-first-then-cargo",
                &cargo,
                true,
                DefaultDaclProbeFirst,
            ),
            (
                13,
                "default-dacl-cargo-twice-same-token",
                &cargo,
                true,
                DefaultDaclCargoTwice,
            ),
            (
                14,
                "default-dacl-cargo-cwd-pair-same-token",
                &cargo,
                true,
                DefaultDaclSafeCwdPair,
            ),
            (
                15,
                "default-dacl-probe-ProcessPrng-safe-cwd",
                &probe,
                true,
                DefaultDaclProcessPrng,
            ),
            (
                16,
                "default-dacl-probe-BCryptGenRandom-safe-cwd",
                &probe,
                true,
                DefaultDaclBCryptRng,
            ),
        ] {
            let mut command = Command::new(program);
            command.env_clear().env("SYSTEMROOT", r"C:\Windows").env(
                "CARGO_HOME",
                stage.join(format!("fetch-temp/cargo-{index}")),
            );
            command.env("RUSTC", &rustc);
            if index <= 2 || index >= 15 {
                command
                    .arg("prep-probe")
                    .arg(if index == 15 {
                        "rng-process-prng"
                    } else if index == 16 {
                        "rng-bcrypt-gen-random"
                    } else if restricted {
                        "restricted"
                    } else {
                        "base-control"
                    })
                    .arg(root.join("prep-secret.dpapi"))
                    .arg(&outside)
                    .arg(stage.join(format!("fetch-temp/matrix-probe-{index}.json")));
            } else if index == 11 {
                command
                    .args(["fetch", "--locked", "--offline", "--manifest-path"])
                    .arg(fixture.join("Cargo.toml"));
            } else {
                command.arg("--version");
            }
            let outcome = launch::diagnostic(config, &command, &stage, restricted, policy);
            let report = match outcome {
                Ok(report) => report,
                Err(error) => {
                    serde_json::json!({"phase":"diagnostic-launch-error", "error":error.to_string()})
                }
            };
            eprintln!(
                "DIAG trusted-fetch-matrix cell={index} name={name} exit={} token={} desktop={}",
                report["exit_status"].as_str().unwrap_or("not-created"),
                report["launch"]["token"],
                report["launch"]["lp_desktop"]
            );
            let deferred = report.get("cleanup_deferred").is_some();
            if index >= 15 {
                eprintln!(
                    "DIAG direct-rng cell={index} result={}",
                    report["rng_probe"]
                );
            }
            let boundary = report["write_probe"]["proof"].as_object().map(|proof| {
                report["write_probe"]["exit_code"] == 0
                    && proof
                        .get("outside_write")
                        .is_some_and(|value| value == "denied")
                    && proof
                        .get("profile_write")
                        .is_some_and(|value| value == "denied")
                    && proof
                        .get("allowed_write")
                        .is_some_and(|value| value == "allowed")
                    && proof
                        .get("write_surfaces")
                        .and_then(serde_json::Value::as_object)
                        .is_some_and(|surfaces| {
                            surfaces.values().all(|surface| {
                                surface["write"] == "denied"
                                    || surface["write"] == "directory-absent"
                            })
                        })
            });
            cells.push(serde_json::json!({"cell":index,"name":name,"diagnostic_policy":policy,"report":report,
                "staging_write_probe_passed":boundary,
                "note":if index == 7 { "Production already grants logon SID GENERIC_ALL; this repeats with explicit full write masks to test mapping, not a new principal" } else { "" }}));
            launch::write_diagnostic_update(
                &report_path,
                &serde_json::json!({"diagnostic_only":true,"production_policy_unchanged":true,"cells":cells,
                    "toolchain_identity":toolchain_identity}),
            )?;
            if deferred {
                deferred_cleanup = true;
                return Err(invalid(
                    "matrix stopped after an unreaped child; preserve launch reports",
                ));
            }
        }
        Ok(())
    })();
    let revoke = super::vm::checked(super::vm::control(
        config,
        "RevokePrep",
        None,
        &["-Staging".into(), stage.to_string_lossy().into()],
    )?);
    if outside.exists() {
        let _ = fs::remove_file(&outside);
    }
    let cleanup = if deferred_cleanup {
        Ok(())
    } else {
        remove_staging(config, &stage)
    };
    result?;
    revoke?;
    cleanup?;
    println!(
        "DIAG trusted-fetch-matrix collected {} cells; report={}; this is not a qualification pass",
        cells.len(),
        report_path.display()
    );
    Ok(())
}
