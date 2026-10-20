//! Credential-free fetch process. Only SYSTEM decrypts the batch account secret.
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
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
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

fn token(root: &Path, restrict: bool) -> io::Result<Handle> {
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
    let base = Handle(result);
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
        // A write must pass the ordinary account DACL and this SID check. Broad
        // Users/Authenticated Users grants on host temp directories do not grant
        // the restricting SID, unlike the explicit per-job/cache ACLs.
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
        Ok(Handle(restricted))
    }
}

/// No inherited handles, no profile, no repository working directory, no shell.
/// Suspending before Job Object assignment closes the child escape race.
pub(super) fn fetch(config: &Config, command: &mut Command, cwd: &Path) -> io::Result<()> {
    run_as_batch(config, command, cwd, true)
}
fn run_as_batch(
    config: &Config,
    command: &mut Command,
    cwd: &Path,
    restrict: bool,
) -> io::Result<()> {
    let root = &config.windows.as_ref().unwrap().layout_root;
    let token = token(root, restrict)?;
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
        let job = Handle(CreateJobObjectW(null(), null()));
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
        let process = Handle(info.hProcess);
        let thread = Handle(info.hThread);
        if AssignProcessToJobObject(job.0, process.0) == 0 {
            let error = io::Error::last_os_error();
            TerminateProcess(process.0, 1);
            WaitForSingleObject(process.0, 5000);
            return Err(error);
        }
        if ResumeThread(thread.0) == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + Duration::from_secs(600);
        while WaitForSingleObject(process.0, 50) != WAIT_OBJECT_0 {
            if Instant::now() >= deadline {
                return Err(invalid("Windows trusted preparation exceeded 600 seconds"));
            }
        }
        let mut code = 0;
        checked(GetExitCodeProcess(process.0, &mut code))?;
        if code != 0 {
            return Err(invalid(format!(
                "Windows trusted cargo fetch exited {code}"
            )));
        }
    }
    Ok(())
}
pub(super) fn remove_staging(_config: &Config, path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    crate::filesystem::identity(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
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
        .env("CARGO_HOME", &cache)
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
            let source = cache.join(name);
            if source.exists() {
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
            run_as_batch(config, &mut command, &stage, restricted)?;
            let bytes = fs::read(&output)?;
            fs::write(root.join(format!("evidence/prep-{mode}.json")), &bytes)?;
            let proof: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
            let expected = if restricted { "denied" } else { "allowed" };
            if proof["outside_write"] != expected
                || proof["allowed_write"] != "allowed"
                || (restricted && proof["credential_read"] != "denied")
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
