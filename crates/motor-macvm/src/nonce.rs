//! Authenticated per-boot metadata on a root-only, read-only raw disk.
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const NONCE_HEADER: &str = "MOTOR-NONCE-V2\n";
pub const NONCE_DISK_SIZE: usize = 17 * 4096;
const MAX_MAP_BYTES: usize = 64 * 1024;
const MAX_MOUNTS: usize = 64;
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub tag: String,
    pub destination: PathBuf,
    pub read_only: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootConfig {
    pub nonce: String,
    pub mounts: Vec<Mount>,
}
pub(super) fn validate_nonce(nonce: &str) -> io::Result<()> {
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "nonce must contain exactly 32 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}
pub fn nonce_bytes(nonce: &str, mounts: &[Mount]) -> io::Result<Vec<u8>> {
    validate_nonce(nonce)?;
    if mounts.len() > MAX_MOUNTS {
        return Err(invalid("nonce mount map exceeds 64 entries"));
    }
    let map = serde_json::to_vec(mounts)?;
    if map.len() > MAX_MAP_BYTES {
        return Err(invalid("nonce mount map exceeds 64 KiB"));
    }
    let mut bytes = format!("{NONCE_HEADER}{nonce}\n").into_bytes();
    bytes.extend_from_slice(&(map.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&map);
    bytes.resize(NONCE_DISK_SIZE, 0);
    Ok(bytes)
}
#[repr(align(4096))]
struct RawImage([u8; NONCE_DISK_SIZE]);

/// Raw devices require an aligned buffer, length and offset. Never probe their
/// header with a short read or retry a partial read at an unaligned offset.
pub fn read_boot_candidate(mut input: impl Read) -> io::Result<Option<BootConfig>> {
    let mut image = Box::new(RawImage([0; NONCE_DISK_SIZE]));
    let count = loop {
        match input.read(&mut image.0) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    };
    if !image.0[..count].starts_with(NONCE_HEADER.as_bytes()) {
        return Ok(None);
    }
    if count != NONCE_DISK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short nonce disk",
        ));
    }
    decode_boot_config(&image.0).map(Some)
}

pub fn read_boot_config(input: impl Read) -> io::Result<BootConfig> {
    read_boot_candidate(input)?.ok_or_else(|| invalid("invalid nonce disk header"))
}

/// Parse the complete fixed-size image, including every byte of zero padding.
pub fn decode_boot_config(bytes: &[u8]) -> io::Result<BootConfig> {
    if bytes.len() != NONCE_DISK_SIZE {
        return Err(invalid("invalid nonce disk extent"));
    }
    let prefix = &bytes[..NONCE_HEADER.len() + 33 + 4];
    if !prefix.starts_with(NONCE_HEADER.as_bytes()) || prefix[NONCE_HEADER.len() + 32] != b'\n' {
        return Err(invalid("invalid nonce disk header"));
    }
    let nonce = std::str::from_utf8(&prefix[NONCE_HEADER.len()..NONCE_HEADER.len() + 32])
        .map_err(invalid)?
        .to_string();
    validate_nonce(&nonce)?;
    let length = u32::from_be_bytes(prefix[prefix.len() - 4..].try_into().unwrap()) as usize;
    if length == 0 || length > MAX_MAP_BYTES {
        return Err(invalid("invalid nonce mount map length"));
    }
    let end = prefix.len() + length;
    let mounts: Vec<Mount> = serde_json::from_slice(&bytes[prefix.len()..end]).map_err(invalid)?;
    if mounts.len() > MAX_MOUNTS {
        return Err(invalid("nonce mount map exceeds 64 entries"));
    }
    if bytes[end..].iter().any(|b| *b != 0) {
        return Err(invalid("invalid nonce disk padding"));
    }
    Ok(BootConfig { nonce, mounts })
}
/// Creates a new disk, never truncating an existing job's boot metadata.
pub fn create_nonce_disk(path: &Path, nonce: &str, mounts: &[Mount]) -> io::Result<()> {
    let bytes = nonce_bytes(nonce, mounts)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut disk = options.open(path)?;
    disk.write_all(&bytes)?;
    disk.sync_all()?;
    let mut permissions = disk.metadata()?.permissions();
    permissions.set_readonly(true);
    disk.set_permissions(permissions)?;
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| invalid("nonce disk has no parent"))?,
    )?
    .sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    struct SectorDevice {
        image: Vec<u8>,
        reads: usize,
    }
    impl Read for SectorDevice {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads != 1
                || out.len() != NONCE_DISK_SIZE
                || !(out.as_ptr() as usize).is_multiple_of(4096)
            {
                return Err(io::Error::from_raw_os_error(22));
            }
            out.copy_from_slice(&self.image);
            Ok(out.len())
        }
    }
    #[test]
    fn raw_nonce_device_is_read_once_with_sector_aligned_io() {
        let mut disk = SectorDevice {
            image: nonce_bytes(NONCE, &[]).unwrap(),
            reads: 0,
        };
        assert_eq!(read_boot_config(&mut disk).unwrap().nonce, NONCE);
        assert_eq!(disk.reads, 1);
    }
    #[test]
    fn complete_image_rejects_nonzero_tail_and_extra_extent() {
        let mut bytes = nonce_bytes(NONCE, &[]).unwrap();
        bytes[NONCE_DISK_SIZE - 1] = 1;
        assert!(decode_boot_config(&bytes).is_err());
        bytes[NONCE_DISK_SIZE - 1] = 0;
        bytes.push(1);
        assert!(decode_boot_config(&bytes).is_err());
    }
    #[test]
    fn v2_mount_map_roundtrips_and_rejects_v1() {
        let mounts = [Mount {
            tag: "workspace".into(),
            destination: "/Users/test/repo".into(),
            read_only: false,
        }];
        let bytes = nonce_bytes(NONCE, &mounts).unwrap();
        assert_eq!(bytes.len(), NONCE_DISK_SIZE);
        assert_eq!(
            read_boot_config(&bytes[..]).unwrap(),
            BootConfig {
                nonce: NONCE.into(),
                mounts: mounts.to_vec()
            }
        );
        let mut old = bytes;
        old[b"MOTOR-NONCE-V".len()] = b'1';
        assert!(read_boot_config(&old[..]).is_err());
    }
    #[test]
    fn v2_map_bounds_and_corruption_fail_closed() {
        let valid = nonce_bytes(NONCE, &[]).unwrap();
        for at in [
            0,
            NONCE_HEADER.len(),
            NONCE_HEADER.len() + 32,
            NONCE_DISK_SIZE - 1,
        ] {
            let mut bad = valid.clone();
            bad[at] = b'!';
            assert!(read_boot_config(&bad[..]).is_err(), "byte {at}");
        }
        for length in [0, 65537, u32::MAX] {
            let mut bad = valid.clone();
            let at = NONCE_HEADER.len() + 33;
            bad[at..at + 4].copy_from_slice(&length.to_be_bytes());
            assert!(read_boot_config(&bad[..]).is_err());
        }
        let mount = Mount {
            tag: "x".into(),
            destination: "/Users/a".into(),
            read_only: true,
        };
        assert!(nonce_bytes(NONCE, &vec![mount.clone(); 65]).is_err());
        let mut bad_map = valid.clone();
        let json = serde_json::to_vec(&vec![mount.clone(); 65]).unwrap();
        let at = NONCE_HEADER.len() + 33;
        bad_map[at..at + 4].copy_from_slice(&(json.len() as u32).to_be_bytes());
        bad_map[at + 4..at + 4 + json.len()].copy_from_slice(&json);
        assert!(read_boot_config(&bad_map[..]).is_err());
        let mut invalid_json = valid.clone();
        invalid_json[at + 4] = b'!';
        assert!(read_boot_config(&invalid_json[..]).is_err());
        let oversized = Mount {
            destination: format!("/Users/{}", "x".repeat(MAX_MAP_BYTES)).into(),
            ..mount
        };
        assert!(nonce_bytes(NONCE, &[oversized]).is_err());
        assert!(read_boot_config(&valid[..valid.len() - 1]).is_err());
    }
}
