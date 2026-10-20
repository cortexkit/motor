use super::*;
use sha2::{Digest, Sha256};

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
    let mut file = fs::File::open(&baseline)?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    if format!("{:x}", digest.finalize()) != vm.parent_sha256.to_ascii_lowercase() {
        return Err(invalid("Hyper-V parent SHA-256 mismatch"));
    }
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
    super::vm::checked(super::vm::control(config, "Validate", None, &[])?)?;
    Ok(())
}
pub(super) fn grant_mirrors(_config: &Config) -> io::Result<()> {
    Ok(())
}
