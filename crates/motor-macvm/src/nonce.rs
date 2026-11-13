//! Authenticated per-boot metadata on a root-only, read-only raw disk.
//!
//! The disk holds two fixed-size regions. The first is the V2 block that every
//! agent version reads: the nonce and the share map. The build extension follows
//! it; only an agent that mounts the job's build disk reads it. An older agent
//! reads exactly the V2 block, boots, and is refused at Hello for its version.
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const NONCE_HEADER: &str = "MOTOR-NONCE-V2\n";
/// Size of the V2 block, which older agents read in one aligned read.
pub const NONCE_DISK_SIZE: usize = 17 * 4096;
pub const BUILD_EXTENSION_HEADER: &str = "MOTOR-BUILD-V1\n";
/// Room for the most return paths a request may carry (64 of at most 4096
/// bytes), even if every byte needs a six-byte JSON escape.
pub const BUILD_EXTENSION_SIZE: usize = 400 * 4096;
/// Size of the whole nonce disk: the V2 block, then the build extension.
pub const NONCE_IMAGE_SIZE: usize = NONCE_DISK_SIZE + BUILD_EXTENSION_SIZE;
const MAX_MAP_BYTES: usize = 64 * 1024;
const MAX_MOUNTS: usize = 64;
/// The most return paths one request may carry.
pub const MAX_BUILD_RETURNS: usize = 64;
/// The longest return path, in bytes, as for any requested return path.
pub const MAX_BUILD_RETURN_BYTES: usize = 4096;
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
/// What the guest agent needs for the job's build disk besides finding it:
/// the returned files it must copy out of `target/` before unmounting it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    /// Requested return paths that name something under the checkout's
    /// `target/`, relative to `target/`, in request order. Each is `/`-separated
    /// with no empty, `.` or `..` component.
    pub returns: Vec<String>,
    /// The caller's byte cap for all of the job's returned files together.
    pub return_max_bytes: u64,
}
impl BuildConfig {
    pub fn validate(&self) -> io::Result<()> {
        if self.returns.len() > MAX_BUILD_RETURNS {
            return Err(invalid("build extension lists more than 64 return paths"));
        }
        for path in &self.returns {
            if path.is_empty()
                || path.len() > MAX_BUILD_RETURN_BYTES
                || path.bytes().any(|b| b == 0)
                || path.split('/').any(|c| matches!(c, "" | "." | ".."))
            {
                return Err(invalid(format!("invalid build return path {path:?}")));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootConfig {
    pub nonce: String,
    pub mounts: Vec<Mount>,
    pub build: BuildConfig,
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
pub fn nonce_bytes(nonce: &str, mounts: &[Mount], build: &BuildConfig) -> io::Result<Vec<u8>> {
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
    build.validate()?;
    let extension = serde_json::to_vec(build)?;
    if BUILD_EXTENSION_HEADER.len() + 4 + extension.len() > BUILD_EXTENSION_SIZE {
        return Err(invalid("build extension exceeds its fixed size"));
    }
    bytes.extend_from_slice(BUILD_EXTENSION_HEADER.as_bytes());
    bytes.extend_from_slice(&(extension.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&extension);
    bytes.resize(NONCE_IMAGE_SIZE, 0);
    Ok(bytes)
}

/// Raw devices require an aligned buffer, length and offset. Never probe their
/// header with a short read or retry a partial read at an unaligned offset.
/// The whole image is read at once, so the build extension needs no second read.
pub fn read_boot_candidate(mut input: impl Read) -> io::Result<Option<BootConfig>> {
    // A heap buffer: the image is too large for a worker thread's stack.
    let mut buffer = vec![0u8; NONCE_IMAGE_SIZE + 4096];
    let offset = buffer.as_ptr().align_offset(4096);
    let image = &mut buffer[offset..offset + NONCE_IMAGE_SIZE];
    let count = loop {
        match input.read(image) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    };
    if !image[..count].starts_with(NONCE_HEADER.as_bytes()) {
        return Ok(None);
    }
    if count != NONCE_IMAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short nonce disk: it has no build extension",
        ));
    }
    decode_boot_config(image).map(Some)
}

pub fn read_boot_config(input: impl Read) -> io::Result<BootConfig> {
    read_boot_candidate(input)?.ok_or_else(|| invalid("invalid nonce disk header"))
}

/// Parse the complete fixed-size image, including every byte of zero padding.
pub fn decode_boot_config(bytes: &[u8]) -> io::Result<BootConfig> {
    if bytes.len() != NONCE_IMAGE_SIZE {
        return Err(invalid("invalid nonce disk extent"));
    }
    let (bytes, extension) = bytes.split_at(NONCE_DISK_SIZE);
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
    Ok(BootConfig {
        nonce,
        mounts,
        build: decode_build_extension(extension)?,
    })
}

/// The extension is all or nothing: a wrong header, length, JSON document,
/// path or non-zero padding byte refuses the whole boot configuration.
fn decode_build_extension(bytes: &[u8]) -> io::Result<BuildConfig> {
    let start = BUILD_EXTENSION_HEADER.len() + 4;
    if bytes.len() != BUILD_EXTENSION_SIZE || !bytes.starts_with(BUILD_EXTENSION_HEADER.as_bytes())
    {
        return Err(invalid("invalid build extension header"));
    }
    let length = u32::from_be_bytes(bytes[start - 4..start].try_into().unwrap()) as usize;
    if length == 0 || length > BUILD_EXTENSION_SIZE - start {
        return Err(invalid("invalid build extension length"));
    }
    let end = start + length;
    let build: BuildConfig = serde_json::from_slice(&bytes[start..end]).map_err(invalid)?;
    build.validate()?;
    if bytes[end..].iter().any(|b| *b != 0) {
        return Err(invalid("invalid build extension padding"));
    }
    Ok(build)
}

/// Creates a new disk, never truncating an existing job's boot metadata. The
/// build extension lists no returned files.
pub fn create_nonce_disk(path: &Path, nonce: &str, mounts: &[Mount]) -> io::Result<()> {
    create_job_nonce_disk(path, nonce, mounts, &BuildConfig::default())
}

/// Creates a new disk, never truncating an existing job's boot metadata.
pub fn create_job_nonce_disk(
    path: &Path,
    nonce: &str,
    mounts: &[Mount],
    build: &BuildConfig,
) -> io::Result<()> {
    let bytes = nonce_bytes(nonce, mounts, build)?;
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
                || out.len() != NONCE_IMAGE_SIZE
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
            image: nonce_bytes(NONCE, &[], &BuildConfig::default()).unwrap(),
            reads: 0,
        };
        assert_eq!(read_boot_config(&mut disk).unwrap().nonce, NONCE);
        assert_eq!(disk.reads, 1);
    }
    #[test]
    fn complete_image_rejects_nonzero_tail_and_extra_extent() {
        let mut bytes = nonce_bytes(NONCE, &[], &BuildConfig::default()).unwrap();
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
        let bytes = nonce_bytes(NONCE, &mounts, &BuildConfig::default()).unwrap();
        assert_eq!(bytes.len(), NONCE_IMAGE_SIZE);
        assert_eq!(
            read_boot_config(&bytes[..]).unwrap(),
            BootConfig {
                nonce: NONCE.into(),
                mounts: mounts.to_vec(),
                build: BuildConfig::default(),
            }
        );
        let mut old = bytes;
        old[b"MOTOR-NONCE-V".len()] = b'1';
        assert!(read_boot_config(&old[..]).is_err());
    }
    #[test]
    fn v2_map_bounds_and_corruption_fail_closed() {
        let valid = nonce_bytes(NONCE, &[], &BuildConfig::default()).unwrap();
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
        assert!(nonce_bytes(NONCE, &vec![mount.clone(); 65], &BuildConfig::default()).is_err());
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
        assert!(nonce_bytes(NONCE, &[oversized], &BuildConfig::default()).is_err());
        assert!(read_boot_config(&valid[..valid.len() - 1]).is_err());
    }
    fn build() -> BuildConfig {
        BuildConfig {
            returns: vec!["debug/app".into(), "doc/index.html".into()],
            return_max_bytes: 4096,
        }
    }
    #[test]
    fn build_extension_roundtrips_after_an_unchanged_v2_block() {
        let bytes = nonce_bytes(NONCE, &[], &build()).unwrap();
        assert_eq!(read_boot_config(&bytes[..]).unwrap().build, build());
        // An agent that predates the extension reads only the V2 block, which is
        // byte-identical whatever the extension holds, so it reaches Hello.
        assert_eq!(
            bytes[..NONCE_DISK_SIZE],
            nonce_bytes(NONCE, &[], &BuildConfig::default()).unwrap()[..NONCE_DISK_SIZE]
        );
        let mut largest = BuildConfig {
            returns: vec!["\u{1}".repeat(MAX_BUILD_RETURN_BYTES); MAX_BUILD_RETURNS],
            return_max_bytes: u64::MAX,
        };
        let bytes = nonce_bytes(NONCE, &[], &largest).unwrap();
        assert_eq!(read_boot_config(&bytes[..]).unwrap().build, largest);
        largest.returns.push("one-too-many".into());
        assert!(nonce_bytes(NONCE, &[], &largest).is_err());
    }
    #[test]
    fn malformed_build_extension_refuses_the_boot_configuration() {
        let valid = nonce_bytes(NONCE, &[], &build()).unwrap();
        let start = NONCE_DISK_SIZE + BUILD_EXTENSION_HEADER.len();
        // Header, JSON and trailing padding bytes.
        for at in [NONCE_DISK_SIZE, start + 4, NONCE_IMAGE_SIZE - 1] {
            let mut bad = valid.clone();
            bad[at] = b'!';
            assert!(read_boot_config(&bad[..]).is_err(), "byte {at}");
        }
        for length in [0, (BUILD_EXTENSION_SIZE - BUILD_EXTENSION_HEADER.len() - 3) as u32] {
            let mut bad = valid.clone();
            bad[start..start + 4].copy_from_slice(&length.to_be_bytes());
            assert!(read_boot_config(&bad[..]).is_err(), "length {length}");
        }
        // A length that stops inside the JSON document is not half-read.
        let mut short = valid.clone();
        let length = u32::from_be_bytes(short[start..start + 4].try_into().unwrap());
        short[start..start + 4].copy_from_slice(&(length - 1).to_be_bytes());
        assert!(read_boot_config(&short[..]).is_err());
        // An unknown field is refused rather than ignored.
        let mut extra = valid.clone();
        let json = br#"{"returns":[],"return_max_bytes":1,"other":1}"#;
        extra[start..start + 4].copy_from_slice(&(json.len() as u32).to_be_bytes());
        extra[start + 4..start + 4 + json.len()].copy_from_slice(json);
        extra[start + 4 + json.len()..].fill(0);
        assert!(read_boot_config(&extra[..]).is_err());
        // A disk written before the extension existed is too short.
        assert!(read_boot_config(&valid[..NONCE_DISK_SIZE]).is_err());
    }
    #[test]
    fn build_return_paths_must_be_relative_and_normal() {
        for path in [
            "", "/abs", "a//b", "a/", "./a", "a/../b", "..", "a\0b",
        ] {
            let config = BuildConfig {
                returns: vec![path.into()],
                return_max_bytes: 1,
            };
            assert!(nonce_bytes(NONCE, &[], &config).is_err(), "{path:?}");
            let mut bytes = nonce_bytes(NONCE, &[], &BuildConfig::default()).unwrap();
            let json = serde_json::to_vec(&config).unwrap();
            let start = NONCE_DISK_SIZE + BUILD_EXTENSION_HEADER.len();
            bytes[start..start + 4].copy_from_slice(&(json.len() as u32).to_be_bytes());
            bytes[start + 4..start + 4 + json.len()].copy_from_slice(&json);
            assert!(read_boot_config(&bytes[..]).is_err(), "{path:?}");
        }
        let long = BuildConfig {
            returns: vec!["x".repeat(MAX_BUILD_RETURN_BYTES + 1)],
            return_max_bytes: 1,
        };
        assert!(nonce_bytes(NONCE, &[], &long).is_err());
    }
}
