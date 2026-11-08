//! The helper's read-only credential drive is read only by guest root, before
//! job code starts. The payload and provisioning files never touch /tenant.
use super::*;
use motor_protocol::{REMOTE_CACHE_DRIVE_MAGIC, REMOTE_CACHE_DRIVE_MAX_BYTES};
use std::{
    io::Read,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use zeroize::Zeroizing;

const DEVICE: &str = "/dev/vdc";
const DIRECTORY: &str = "/run/motor/cache";

fn invalid() -> io::Error {
    io::Error::other("remote_cache_invalid_drive")
}

fn decode(mut reader: impl Read) -> io::Result<RemoteCacheRequest> {
    let mut magic = vec![0; REMOTE_CACHE_DRIVE_MAGIC.len()];
    reader.read_exact(&mut magic).map_err(|_| invalid())?;
    if magic != REMOTE_CACHE_DRIVE_MAGIC {
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

fn load(device: &Path) -> io::Result<Option<RemoteCacheRequest>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(device)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(invalid()),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_block_device()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
    {
        return Err(invalid());
    }
    decode(file).map(Some)
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
    let Some(request) = load(Path::new(DEVICE))? else {
        return Ok(());
    };
    fs::create_dir_all("/run/motor")?;
    paths::Directory::absolute(Path::new("/run/motor"), 0)?;
    let directory = std::ffi::CString::new("/run/motor").unwrap();
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(directory.as_ptr(), &mut stat) } != 0
        || stat.f_type != libc::TMPFS_MAGIC
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
        drive.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        drive.extend(bytes);
        drive.resize(drive.len().next_multiple_of(512), 0);
        drive
    }

    #[test]
    fn injected_read_only_and_fill_sets_have_runner_owner_exact_modes_and_parse() {
        let scratch = ScratchDir::new("cache-drive");
        let directory = scratch.join("cache");
        for write in [true, false] {
            let request = decode(payload(write).as_slice()).unwrap();
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
        assert!(load(&scratch.join("missing-device")).unwrap().is_none());
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
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
        assert!(decode(bad.as_slice()).is_err());
        for size in [0, REMOTE_CACHE_DRIVE_MAX_BYTES + 1] {
            let mut bad = REMOTE_CACHE_DRIVE_MAGIC.to_vec();
            bad.extend_from_slice(&(size as u32).to_le_bytes());
            assert!(decode(bad.as_slice()).is_err());
        }
        let mut bad = payload(false);
        bad.truncate(30);
        let error = decode(bad.as_slice()).err().unwrap().to_string();
        assert_eq!(error, "remote_cache_invalid_drive");
        let mut malformed = REMOTE_CACHE_DRIVE_MAGIC.to_vec();
        let bytes =
            br#"{"op":"set_remote_cache","config":"READ-SECRET","read_key":"WRITE-SECRET"}"#;
        malformed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        malformed.extend_from_slice(bytes);
        assert_eq!(
            decode(malformed.as_slice()).err().unwrap().to_string(),
            "remote_cache_invalid_injected_set"
        );
    }
}
