//! The helper's read-only credential drive is read only by guest root, before
//! job code starts. The payload and provisioning files never touch /tenant.
use super::*;
use motor_protocol::{
    REMOTE_CACHE_DRIVE_MAGIC, REMOTE_CACHE_DRIVE_MAX_BYTES, REMOTE_CACHE_DRIVE_NONCE_BYTES,
    REMOTE_CACHE_NONCE_CMDLINE_KEY,
};
use std::{
    io::Read,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use zeroize::Zeroizing;

const DIRECTORY: &str = "/run/motor/cache";
type Nonce = [u8; REMOTE_CACHE_DRIVE_NONCE_BYTES];

fn invalid() -> io::Error {
    io::Error::other("remote_cache_invalid_drive")
}

fn expected_nonce(cmdline: &str) -> io::Result<Option<Nonce>> {
    let mut found = None;
    for word in cmdline.split_whitespace() {
        let Some((key, value)) = word.split_once('=') else {
            continue;
        };
        if key != REMOTE_CACHE_NONCE_CMDLINE_KEY {
            continue;
        }
        if found.is_some()
            || value.len() != REMOTE_CACHE_DRIVE_NONCE_BYTES * 2
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        let mut nonce = [0; REMOTE_CACHE_DRIVE_NONCE_BYTES];
        for (index, byte) in nonce.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }
        found = Some(nonce);
    }
    Ok(found)
}

fn matches_header(mut reader: impl Read, expected: &Nonce) -> io::Result<bool> {
    let mut magic = vec![0; REMOTE_CACHE_DRIVE_MAGIC.len()];
    let mut nonce = [0; REMOTE_CACHE_DRIVE_NONCE_BYTES];
    match reader
        .read_exact(&mut magic)
        .and_then(|()| reader.read_exact(&mut nonce))
    {
        Ok(()) => Ok(magic == REMOTE_CACHE_DRIVE_MAGIC && &nonce == expected),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(_) => Err(invalid()),
    }
}

fn discover(
    cmdline: &str,
    sys_block: &Path,
    devices: &Path,
) -> io::Result<Option<(PathBuf, Nonce)>> {
    let Some(nonce) = expected_nonce(cmdline)? else {
        return Ok(None);
    };
    let mut matched = None;
    // Firecracker's serial comes from backing-file metadata, not drive_id.
    // A per-start nonce binds a read-only payload to the trusted boot arguments
    // without depending on device letters or Firecracker's internal ID format.
    for entry in fs::read_dir(sys_block).map_err(|_| invalid())? {
        let entry = entry.map_err(|_| invalid())?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| name.starts_with("vd")) {
            continue;
        }
        let read_only = fs::read_to_string(entry.path().join("ro")).map_err(|_| invalid())?;
        if read_only.strip_suffix('\n').unwrap_or(&read_only) != "1" {
            continue;
        }
        let path = devices.join(name);
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| invalid())?;
        if !matches_header(&mut file, &nonce)? {
            continue;
        }
        if matched.is_some() {
            return Err(invalid());
        }
        matched = Some((path, nonce));
    }
    Ok(matched)
}

fn load_discovered(
    cmdline: &str,
    sys_block: &Path,
    devices: &Path,
) -> io::Result<Option<RemoteCacheRequest>> {
    discover(cmdline, sys_block, devices)
        .map_err(|_| {
            per_job::failure(
                "remote_cache_invalid_drive",
                "credential drive nonce selection failed",
            )
        })?
        .map(|(device, nonce)| load(&device, &nonce))
        .transpose()
}

fn decode(mut reader: impl Read, nonce: &Nonce) -> io::Result<RemoteCacheRequest> {
    if !matches_header(&mut reader, nonce)? {
        return Err(invalid());
    }
    let mut size = [0; 4];
    reader.read_exact(&mut size).map_err(|_| invalid())?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > REMOTE_CACHE_DRIVE_MAX_BYTES {
        return Err(invalid());
    }
    let mut bytes = Zeroizing::new(vec![0; size]);
    reader.read_exact(&mut bytes).map_err(|_| invalid())?;
    let value = SecretJson::new(serde_json::from_slice(&bytes).map_err(|_| invalid())?);
    let request = RemoteCacheRequest::parse(value.value()).ok_or_else(invalid)?;
    crate::remote_cache::provisioning::validate_injected(&request)?;
    Ok(request)
}

fn load(device: &Path, nonce: &Nonce) -> io::Result<RemoteCacheRequest> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(device)
        .map_err(|_| invalid())?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_block_device()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
    {
        return Err(invalid());
    }
    decode(file, nonce)
}

fn write_set(directory: &Path, request: RemoteCacheRequest) -> io::Result<()> {
    let owner = unsafe { libc::geteuid() };
    let parent = directory.parent().ok_or_else(invalid)?;
    paths::Directory::open(parent, owner)?;
    let staging = parent.join(format!(".cache-injection-{}", Uuid::now_v7()));
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| {
        for (name, bytes, mode) in [
            ("config.json", Some(request.config), 0o444),
            ("read-key", Some(request.read_key), 0o400),
            ("write-key", request.write_key, 0o400),
        ] {
            if let Some(bytes) = bytes {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(staging.join(name))?;
                if unsafe { libc::fchown(file.as_raw_fd(), owner, libc::getegid()) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                file.set_permissions(fs::Permissions::from_mode(mode))?;
                file.write_all(bytes.as_bytes())?;
            }
        }
        if directory.exists() {
            paths::Directory::open(directory, owner)?;
            fs::remove_dir_all(directory)?;
        }
        fs::rename(&staging, directory)
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(staging);
    }
    result
}

pub(super) fn install() -> io::Result<()> {
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    let Some(request) = load_discovered(&cmdline, Path::new("/sys/block"), Path::new("/dev"))?
    else {
        return Ok(());
    };
    fs::create_dir_all("/run/motor")?;
    paths::Directory::absolute(Path::new("/run/motor"), 0)?;
    let directory = std::ffi::CString::new("/run/motor").unwrap();
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(directory.as_ptr(), &mut stat) } != 0
        // f_type is signed on glibc and unsigned on musl; compare as u64.
        || stat.f_type as u64 != libc::TMPFS_MAGIC as u64
    {
        return Err(io::Error::other("remote_cache_requires_tmpfs"));
    }
    write_set(Path::new(DIRECTORY), request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_cache::provisioning::{HostRole, Layout, Provisioning};
    use cortexkit_test_support::ScratchDir;
    const NONCE: Nonce = [0x42; REMOTE_CACHE_DRIVE_NONCE_BYTES];

    fn cmdline(nonce: &Nonce) -> String {
        format!(
            "quiet {REMOTE_CACHE_NONCE_CMDLINE_KEY}={}",
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    fn payload(write: bool) -> Vec<u8> {
        let config = json!({"generation":"1", "tenant":"acct:test", "endpoint":"https://cache.example.com",
            "region":"test", "bucket":"motor-cache", "prefix":"tenant/acct-test/", "storage_class":"STANDARD", "enabled":true});
        let key =
            json!({"generation":"1", "access_key_id":"READ-ID", "secret_access_key":"READ-SECRET"});
        let mut value = json!({"op":"set_remote_cache", "config":config.to_string(), "read_key":key.to_string()});
        if write {
            value["write_key"] = json!(json!({"generation":"1", "access_key_id":"WRITE-ID", "secret_access_key":"WRITE-SECRET"}).to_string());
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut drive = REMOTE_CACHE_DRIVE_MAGIC.to_vec();
        drive.extend_from_slice(&NONCE);
        drive.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        drive.extend(bytes);
        drive.resize(drive.len().next_multiple_of(512), 0);
        drive
    }

    fn sys_device(root: &Path, name: &str, read_only: &str) {
        let device = root.join(name);
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("ro"), read_only).unwrap();
    }

    fn assert_identity_refusal(error: io::Error) {
        let typed = error
            .get_ref()
            .unwrap()
            .downcast_ref::<per_job::BootFailure>()
            .unwrap();
        assert_eq!(typed.code, "remote_cache_invalid_drive");
    }

    #[test]
    fn credential_payload_with_wrong_nonce_is_ignored() {
        let scratch = ScratchDir::new("cache-drive-wrong-nonce");
        let sys_block = scratch.join("sys-block");
        let devices = scratch.join("dev");
        fs::create_dir_all(&devices).unwrap();
        sys_device(&sys_block, "vdc", "1\n");
        fs::write(devices.join("vdc"), payload(true)).unwrap();
        assert!(load_discovered(
            &cmdline(&[0x99; REMOTE_CACHE_DRIVE_NONCE_BYTES]),
            &sys_block,
            &devices
        )
        .unwrap()
        .is_none());
        assert!(!scratch.join("cache").exists());
    }

    #[test]
    fn writable_device_with_matching_nonce_is_ignored_without_reading_it() {
        let scratch = ScratchDir::new("cache-drive-writable");
        let sys_block = scratch.join("sys-block");
        let devices = scratch.join("dev");
        fs::create_dir_all(&devices).unwrap();
        sys_device(&sys_block, "vdb", "0\n");
        fs::write(devices.join("vdb"), payload(true)).unwrap();
        assert!(load_discovered(&cmdline(&NONCE), &sys_block, &devices)
            .unwrap()
            .is_none());
        fs::remove_file(devices.join("vdb")).unwrap();
        assert!(
            load_discovered(&cmdline(&NONCE), &sys_block, &devices)
                .unwrap()
                .is_none(),
            "a writable device must not even be opened"
        );
    }

    #[test]
    fn two_read_only_devices_with_matching_nonce_are_refused_before_payload_decode() {
        let scratch = ScratchDir::new("cache-drive-duplicate-nonce");
        let sys_block = scratch.join("sys-block");
        let devices = scratch.join("dev");
        fs::create_dir_all(&devices).unwrap();
        for name in ["vdb", "vdd"] {
            sys_device(&sys_block, name, "1\n");
            fs::write(devices.join(name), payload(false)).unwrap();
        }
        assert_identity_refusal(
            load_discovered(&cmdline(&NONCE), &sys_block, &devices)
                .err()
                .unwrap(),
        );
    }

    #[test]
    fn unique_read_only_nonce_selects_the_device_independently_of_attach_order() {
        let scratch = ScratchDir::new("cache-drive-selected-nonce");
        let sys_block = scratch.join("sys-block");
        let devices = scratch.join("dev");
        fs::create_dir_all(&devices).unwrap();
        sys_device(&sys_block, "vdc", "0\n");
        sys_device(&sys_block, "vdd", "1\n");
        fs::write(devices.join("vdc"), payload(true)).unwrap();
        fs::write(devices.join("vdd"), payload(false)).unwrap();
        assert_eq!(
            discover(&cmdline(&NONCE), &sys_block, &devices).unwrap(),
            Some((devices.join("vdd"), NONCE))
        );
    }

    #[test]
    fn missing_cmdline_nonce_disables_cache_without_reading_any_device() {
        let scratch = ScratchDir::new("cache-drive-no-boot-nonce");
        let sys_block = scratch.join("sys-block");
        let devices = scratch.join("dev");
        fs::create_dir_all(&devices).unwrap();
        sys_device(&sys_block, "vdd", "1\n");
        fs::write(devices.join("vdd"), payload(false)).unwrap();
        assert!(
            load_discovered("quiet motor.per_job=1", &sys_block, &devices)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(&sys_block).unwrap();
        assert!(load_discovered("quiet", &sys_block, &devices)
            .unwrap()
            .is_none());
        assert_identity_refusal(
            load_discovered("motor.cache_nonce=malformed", &sys_block, &devices)
                .err()
                .unwrap(),
        );
        assert_identity_refusal(
            load_discovered(
                &format!("{} {}", cmdline(&NONCE), cmdline(&NONCE)),
                &sys_block,
                &devices,
            )
            .err()
            .unwrap(),
        );
    }

    #[test]
    fn injected_read_only_and_fill_sets_have_runner_owner_exact_modes_and_parse() {
        let scratch = ScratchDir::new("cache-drive");
        let directory = scratch.join("cache");
        for write in [true, false] {
            let request = decode(payload(write).as_slice(), &NONCE).unwrap();
            write_set(&directory, request).unwrap();
            for (name, mode) in [("config.json", 0o444), ("read-key", 0o400)] {
                let metadata = fs::metadata(directory.join(name)).unwrap();
                assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
                assert_eq!(metadata.mode() & 0o777, mode);
            }
            assert_eq!(directory.join("write-key").exists(), write);
            if write {
                let metadata = fs::metadata(directory.join("write-key")).unwrap();
                assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
                assert_eq!(metadata.mode() & 0o777, 0o400);
            }
            let mut provisioning =
                Provisioning::new(Layout::new(directory.clone(), HostRole::TenantVm));
            provisioning.reread();
            assert!(provisioning.admit().is_some());
            assert_eq!(provisioning.active().unwrap().write_key.is_some(), write);
        }
    }

    #[test]
    fn absent_drive_disables_cache_without_writing_credentials() {
        let scratch = ScratchDir::new("cache-drive-absent");
        let sys_block = scratch.join("sys-block");
        fs::create_dir_all(&sys_block).unwrap();
        assert!(
            load_discovered(&cmdline(&NONCE), &sys_block, &scratch.join("missing-dev"))
                .unwrap()
                .is_none()
        );
        assert!(!scratch.join("cache").exists());
        let layout = Layout::new(scratch.join("cache"), HostRole::TenantVm);
        let mut provisioning = Provisioning::new(layout);
        provisioning.reread();
        assert!(provisioning.admit().is_none());
        let mut config = crate::Config::new(scratch.to_path_buf(), "main");
        config.tenant_vm = true;
        config.per_job = true;
        assert_eq!(
            Layout::for_config(&config).directory(),
            Path::new(DIRECTORY)
        );
    }

    #[test]
    fn drive_parser_rejects_malformed_truncated_and_oversized_payloads_without_secrets() {
        let mut bad = payload(false);
        bad[0] = b'x';
        assert!(decode(bad.as_slice(), &NONCE).is_err());
        for size in [0, REMOTE_CACHE_DRIVE_MAX_BYTES + 1] {
            let mut bad = REMOTE_CACHE_DRIVE_MAGIC.to_vec();
            bad.extend_from_slice(&NONCE);
            bad.extend_from_slice(&(size as u32).to_le_bytes());
            assert!(decode(bad.as_slice(), &NONCE).is_err());
        }
        let mut bad = payload(false);
        bad.truncate(30);
        let error = decode(bad.as_slice(), &NONCE).err().unwrap().to_string();
        assert_eq!(error, "remote_cache_invalid_drive");
        let mut malformed = REMOTE_CACHE_DRIVE_MAGIC.to_vec();
        let bytes =
            br#"{"op":"set_remote_cache","config":"READ-SECRET","read_key":"WRITE-SECRET"}"#;
        malformed.extend_from_slice(&NONCE);
        malformed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        malformed.extend_from_slice(bytes);
        assert_eq!(
            decode(malformed.as_slice(), &NONCE)
                .err()
                .unwrap()
                .to_string(),
            "remote_cache_invalid_injected_set"
        );
    }
}
