use super::*;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

pub(super) fn load(path: &Path) -> io::Result<VmConfig> {
    let vm: VmConfig = toml::from_str(&fs::read_to_string(path)?).map_err(invalid)?;
    if vm.layout_root != Path::new(r"C:\ProgramData\MotorRunner")
        || vm.baseline.is_empty()
        || !vm
            .baseline
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_ .".contains(&b))
        || Path::new(&vm.baseline).components().count() != 1
        || vm.memory_mib != 8192
        || vm.cpu_set != [0, 1, 2, 3]
        || !(1..=2).contains(&vm.concurrent_jobs)
        || vm.toolchain_version.trim().is_empty()
        || vm.parent_sha256.len() != 64
        || !vm.parent_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || vm.prep_cargo_sha256.len() != 64
        || !vm.prep_cargo_sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("invalid Hyper-V configuration: dedicated root, sealed baseline hashes, 4 processors, 8192 MiB and at most two slots required; deploy one slot until native concurrency qualification is approved"));
    }
    let baseline = vm.baseline_path();
    let metadata = fs::symlink_metadata(&baseline)?;
    if !metadata.is_file() || !metadata.permissions().readonly() {
        return Err(invalid(
            "Hyper-V parent must be a sealed read-only regular file",
        ));
    }
    // Startup Validate hashes and records the parent. Configuration loading reads no bytes.
    Ok(vm)
}
pub(super) fn validate(config: &Config) -> io::Result<()> {
    let vm = config.windows.as_ref().unwrap();
    if config.state_root != vm.layout_root.join("state")
        || config.instance != "ally"
        || config.backend != crate::BackendKind::WindowsVm
    {
        return Err(invalid("native Windows runner requires instance ally, windows-vm backend and the installed state root"));
    }
    let parent = vm.baseline_path();
    let mut lifetime = config.windows_runtime.parent_guard.lock().unwrap();
    if lifetime.as_ref().is_some_and(|(path, _)| path != &parent) {
        return Err(invalid("Windows runtime already guards a different parent"));
    }
    // Acquire before validation. Store only after success; an error drops the
    // local guard even if callers retain another Config clone.
    let guard = if lifetime.is_none() {
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ: Hyper-V readers coexist; writes/deletes do not.
            .custom_flags(0x00200000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(&parent)
            .map_err(|error| {
                invalid(format!("Cannot guard parent before verification: {error}"))
            })?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_attributes() & 0x400 != 0
            || !metadata.permissions().readonly()
        {
            return Err(invalid(
                "Parent guard requires a sealed non-reparse regular file",
            ));
        }
        Some(file)
    } else {
        None
    };
    let reuse_record = config.windows_runtime.reuses_parent_verification();
    if reuse_record {
        use std::io::Write;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "WAIT parent image verification using the saved SHA-256 record, file identity and ChangeTime")?;
        stdout.flush()?;
    } else {
        tracing::info!("validating parent image (sha256, ~70 s)");
    }
    let mut validation_args = vec![
        "-Parent".into(),
        vm.baseline_path().to_string_lossy().into(),
        "-ParentSha256".into(),
        vm.parent_sha256.clone(),
    ];
    if reuse_record {
        validation_args.push("-ReuseVerifiedParent".into());
    }
    let validation = super::vm::checked_validation(super::vm::control(
        config,
        "Validate",
        None,
        &validation_args,
    )?)?;
    if reuse_record {
        use std::io::Write;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "RESULT parent image verification record_reused=true file_identity_matches=true hash_duration_ms=0")?;
        stdout.flush()?;
    } else {
        tracing::info!(
            validation = %String::from_utf8_lossy(&validation.stdout).trim(),
            "parent image SHA-256 validation result"
        );
    }
    if let Some(file) = guard {
        *lifetime = Some((parent, file));
    }
    Ok(())
}
pub(super) fn ensure_parent_guard(config: &Config) -> io::Result<()> {
    let vm = config
        .windows
        .as_ref()
        .ok_or_else(|| invalid("Windows VM configuration is missing"))?;
    let parent = vm.baseline_path();
    let lifetime = config.windows_runtime.parent_guard.lock().unwrap();
    let (guarded_path, file) = lifetime.as_ref().ok_or_else(|| {
        invalid("Windows parent verification guard is missing; restart the service to re-verify")
    })?;
    if guarded_path != &parent {
        return Err(invalid(
            "Windows parent verification guard is for a different parent",
        ));
    }
    let metadata = file.metadata().map_err(|error| {
        invalid(format!(
            "Windows parent verification guard was lost; restart the service: {error}"
        ))
    })?;
    if !metadata.is_file()
        || metadata.file_attributes() & 0x400 != 0
        || !metadata.permissions().readonly()
    {
        return Err(invalid(
            "Windows parent verification guard no longer identifies a sealed regular file",
        ));
    }
    Ok(())
}
pub(super) fn grant_mirrors(_config: &Config) -> io::Result<()> {
    Ok(())
}
