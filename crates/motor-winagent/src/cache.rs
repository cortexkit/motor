//! Cache identity, fail-closed storage decisions, and fresh-disk initialization.

use std::io;
use std::time::Instant;

pub const CACHE_DISK_SERIAL: &str = "motor-cache";

#[derive(serde::Deserialize)]
pub struct HypervDisk {
    pub number: u32,
    pub lun: u32,
    pub boot: bool,
    pub system: bool,
}

pub fn parse_hyperv_disk_inventory(json: &str) -> Result<Vec<HypervDisk>, serde_json::Error> {
    serde_json::from_str(json)
}

/// This runner expects the OS at SCSI LUN 0 and the cache at LUN 1. Require the
/// complete two-disk topology, not merely the absence of a virtio serial.
pub fn select_hyperv_cache_disk(disks: &[HypervDisk]) -> io::Result<u32> {
    if disks.len() != 2
        || disks
            .iter()
            .filter(|d| d.lun == 0 && d.boot && d.system)
            .count()
            != 1
    {
        return Err(io::Error::other(
            "unexpected Hyper-V OS/cache disk topology",
        ));
    }
    let cache = disks
        .iter()
        .find(|d| d.lun == 1 && !d.boot && !d.system)
        .ok_or_else(|| {
            io::Error::other("Hyper-V cache must be the non-system disk at SCSI LUN 1")
        })?;
    if disks[0].number == disks[1].number {
        return Err(io::Error::other("duplicate Hyper-V disk number"));
    }
    Ok(cache.number)
}

pub fn select_cache_disk<'a>(disks: impl IntoIterator<Item = (u32, &'a str)>) -> io::Result<u32> {
    let mut matches = disks
        .into_iter()
        .filter(|(_, serial)| *serial == CACHE_DISK_SERIAL);
    let (number, _) = matches
        .next()
        .ok_or_else(|| io::Error::other("no disk has the cache serial"))?;
    if matches.next().is_some() {
        return Err(io::Error::other("multiple disks have the cache serial"));
    }
    Ok(number)
}

/// STORAGE_DEVICE_DESCRIPTOR uses byte offsets into its returned buffer. Reject
/// truncated or unterminated identifiers rather than guessing a physical disk.
pub fn storage_serial(descriptor: &[u8]) -> io::Result<&str> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid storage serial descriptor",
        )
    };
    if descriptor.len() < 36 {
        return Err(invalid());
    }
    let offset = u32::from_le_bytes(descriptor[24..28].try_into().unwrap()) as usize;
    if offset == 0 {
        return Ok("");
    }
    if offset < 36 || offset >= descriptor.len() {
        return Err(invalid());
    }
    let bytes = &descriptor[offset..];
    let end = bytes.iter().position(|b| *b == 0).ok_or_else(invalid)?;
    std::str::from_utf8(&bytes[..end])
        .map(str::trim)
        .map_err(|_| invalid())
}

#[derive(Clone, Debug)]
pub enum DiskLayout {
    Raw,
    Mbr {
        partition_count: u32,
        signature: u32,
    },
    Gpt(Vec<PartitionKind>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionKind {
    Reserved,
    BasicData,
    Other,
}

#[derive(Clone, Debug)]
pub struct CacheVolume {
    pub path: String,
    pub disks: Vec<u32>,
    pub filesystem: String,
    pub label: String,
}

/// Physical-disk queries and attribute updates use one held handle. A formatter
/// using the disk number must recheck identity before changing the layout.
pub trait CacheStorage {
    fn serial(&mut self) -> io::Result<String>;
    fn windows_disks(&mut self) -> io::Result<Vec<u32>>;
    fn layout(&mut self) -> io::Result<DiskLayout>;
    fn make_writable(&mut self) -> io::Result<()>;
    fn initialize_raw(&mut self) -> io::Result<()>;
    fn volumes(&mut self) -> io::Result<Vec<CacheVolume>>;
}

pub struct PreparedCache {
    pub volume: String,
    pub freshly_formatted: bool,
    pub native_attributes_ms: u64,
    pub powershell_ms: u64,
    pub volume_ms: u64,
}

fn check_disk(
    storage: &mut impl CacheStorage,
    number: u32,
    serial: &str,
) -> io::Result<DiskLayout> {
    if storage.serial()? != serial {
        return Err(io::Error::other("cache disk serial changed"));
    }
    let windows = storage.windows_disks()?;
    if windows.is_empty() || windows.contains(&number) {
        return Err(io::Error::other("cache disk may hold running Windows"));
    }
    // Windows can describe a virgin disk as MBR with neither partitions nor a
    // disk signature. Only that exact empty shape is equivalent to RAW.
    let layout = match storage.layout()? {
        DiskLayout::Mbr {
            partition_count: 0,
            signature: 0,
        } => DiskLayout::Raw,
        layout => layout,
    };
    // Windows BIOS system disks are MBR; UEFI system disks contain an EFI
    // partition. Accept only the GPT data/MSR layout created for our caches.
    let allowed = match &layout {
        DiskLayout::Raw => true,
        DiskLayout::Mbr { .. } => false,
        DiskLayout::Gpt(parts) => {
            parts
                .iter()
                .filter(|p| **p == PartitionKind::BasicData)
                .count()
                == 1
                && parts
                    .iter()
                    .filter(|p| **p == PartitionKind::Reserved)
                    .count()
                    <= 1
                && !parts.contains(&PartitionKind::Other)
        }
    };
    if !allowed {
        return Err(io::Error::other(format!(
            "cache disk has an unexpected partition layout: {layout:?}"
        )));
    }
    Ok(layout)
}

pub fn prepare_cache(storage: &mut impl CacheStorage, number: u32) -> io::Result<PreparedCache> {
    prepare_cache_with_serial(storage, number, CACHE_DISK_SERIAL)
}

pub fn prepare_cache_with_serial(
    storage: &mut impl CacheStorage,
    number: u32,
    serial: &str,
) -> io::Result<PreparedCache> {
    if serial.trim().is_empty() || serial.len() > 256 {
        return Err(io::Error::other("cache disk lacks a bounded stable serial"));
    }
    let raw = matches!(check_disk(storage, number, serial)?, DiskLayout::Raw);
    let started = Instant::now();
    storage.make_writable()?;
    let native_attributes_ms = started.elapsed().as_millis() as u64;
    let started = Instant::now();
    let mut powershell_ms = 0;
    if raw {
        storage.initialize_raw()?;
        powershell_ms = started.elapsed().as_millis() as u64;
        if !matches!(check_disk(storage, number, serial)?, DiskLayout::Gpt(_)) {
            return Err(io::Error::other("cache initialization did not create GPT"));
        }
    }
    let mut volumes = storage
        .volumes()?
        .into_iter()
        .filter(|v| v.disks.contains(&number));
    let volume = volumes
        .next()
        .ok_or_else(|| io::Error::other("cache volume not found"))?;
    if volumes.next().is_some()
        || volume.disks != [number]
        || !volume.filesystem.eq_ignore_ascii_case("NTFS")
        || volume.label != crate::CACHE_LABEL
    {
        return Err(io::Error::other(
            "cache is not the unique expected NTFS volume",
        ));
    }
    Ok(PreparedCache {
        volume: volume.path,
        freshly_formatted: raw,
        native_attributes_ms,
        powershell_ms,
        volume_ms: started.elapsed().as_millis() as u64,
    })
}

/// Updating an inheritable DACL can walk existing children as SYSTEM. A reused
/// disk is untrusted, so a missing grant requires discarding it, not repairing it.
pub fn cache_acl_update(has_grant: bool, freshly_formatted: bool) -> io::Result<bool> {
    match (has_grant, freshly_formatted) {
        (true, _) => Ok(false),
        (false, true) => Ok(true),
        (false, false) => Err(io::Error::other("cache_grant_missing")),
    }
}

/// Grant the OS mount folder before attaching any job-populated volume to it.
pub fn empty_mount_directory(path: &std::path::Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || crate::is_reparse(&metadata)
        || std::fs::read_dir(path)?.next().is_some()
    {
        return Err(io::Error::other(
            "cache mount folder must be an empty physical directory",
        ));
    }
    Ok(())
}

pub fn mount_script(number: u32) -> String {
    mount_script_with_serial(number, CACHE_DISK_SERIAL)
}

pub fn mount_script_with_serial(number: u32, serial: &str) -> String {
    let serial = serial.replace('\'', "''");
    format!(
        r#"$ErrorActionPreference='Stop';
$d=Get-Disk -Number {number};
if($d.IsBoot -or $d.IsSystem) {{ throw 'cache disk is a boot or system disk' }};
if($d.SerialNumber.Trim() -cne '{serial}') {{ throw 'cache disk serial changed' }};
if($d.IsOffline -or $d.IsReadOnly) {{ throw 'cache disk is not online and writable' }};
if($d.PartitionStyle -ne 'RAW') {{ throw 'only a RAW cache disk may be initialized' }};
$d | Initialize-Disk -PartitionStyle GPT -PassThru | New-Partition -UseMaximumSize | Format-Volume -FileSystem NTFS -NewFileSystemLabel '{label}' -Confirm:$false | Out-Null"#,
        label = crate::CACHE_LABEL,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn hyperv_cache_selection_requires_exact_two_disk_scsi_topology() {
        use super::*;
        let os = || HypervDisk {
            number: 0,
            lun: 0,
            boot: true,
            system: true,
        };
        let cache = || HypervDisk {
            number: 1,
            lun: 1,
            boot: false,
            system: false,
        };
        assert_eq!(select_hyperv_cache_disk(&[os(), cache()]).unwrap(), 1);
        assert_eq!(select_hyperv_cache_disk(&[cache(), os()]).unwrap(), 1);
        assert!(select_hyperv_cache_disk(&[cache()]).is_err());
        assert!(select_hyperv_cache_disk(&[os(), cache(), cache()]).is_err());
        let mut system_cache = cache();
        system_cache.system = true;
        assert!(select_hyperv_cache_disk(&[os(), system_cache]).is_err());
        let mut wrong_lun = cache();
        wrong_lun.lun = 2;
        assert!(select_hyperv_cache_disk(&[os(), wrong_lun]).is_err());
        let mut duplicate = cache();
        duplicate.number = 0;
        assert!(select_hyperv_cache_disk(&[os(), duplicate]).is_err());
    }

    #[test]
    fn hyperv_inventory_parser_requires_a_json_array() {
        let os = r#"{"number":0,"lun":0,"boot":true,"system":true}"#;
        let cache = r#"{"number":1,"lun":1,"boot":false,"system":false}"#;
        let many = format!("[{os},{cache}]");
        let disks = super::parse_hyperv_disk_inventory(&many).unwrap();
        assert_eq!(super::select_hyperv_cache_disk(&disks).unwrap(), 1);
        assert_eq!(super::parse_hyperv_disk_inventory("[]").unwrap().len(), 0);
        assert_eq!(
            super::parse_hyperv_disk_inventory(&format!("[{os}]"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            super::parse_hyperv_disk_inventory(&format!(
                "{{\"value\":[{os},{cache}],\"Count\":2}}"
            ))
            .is_err()
        );
    }

    #[test]
    fn hyperv_formatter_quotes_the_captured_serial_and_retains_raw_guard() {
        let script = super::mount_script_with_serial(1, "serial'quoted");
        assert!(script.contains("-cne 'serial''quoted'"));
        assert!(script.contains("$d.IsBoot -or $d.IsSystem"));
        assert!(script.contains("$d.PartitionStyle -ne 'RAW'"));
        assert_eq!(
            super::mount_script(1),
            super::mount_script_with_serial(1, super::CACHE_DISK_SERIAL)
        );
    }
    #[test]
    fn privileged_dismount_never_resolves_the_job_mount_folder() {
        // Dismount runs after the job can replace C:\t. Fence the actual
        // Windows implementation against reintroducing pathname-based cleanup.
        let source = include_str!("main.rs");
        let body = source
            .split_once("fn open_volume(&self)")
            .unwrap()
            .1
            .split_once("enum Event")
            .unwrap()
            .0;
        assert!(body.contains("self.open_volume()?"));
        assert!(body.contains("FSCTL_LOCK_VOLUME"));
        assert!(body.contains("FSCTL_DISMOUNT_VOLUME"));
        for forbidden in ["DeleteVolumeMountPoint", "wide(CACHE_MOUNT)", "std::fs::"] {
            assert!(
                !body.contains(forbidden),
                "dismount resolved a job path: {forbidden}"
            );
        }
    }

    #[test]
    fn reused_cache_missing_grant_never_updates_acl() {
        assert!(!super::cache_acl_update(true, false).unwrap());
        assert!(!super::cache_acl_update(true, true).unwrap());
        assert!(super::cache_acl_update(false, true).unwrap());
        assert_eq!(
            super::cache_acl_update(false, false)
                .unwrap_err()
                .to_string(),
            "cache_grant_missing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mount_folder_refuses_links_and_nonempty_directories() {
        let root = crate::tests::Scratch::new();
        let target = root.0.join("target");
        let mount = root.0.join("mount");
        super::empty_mount_directory(&target).unwrap();
        std::os::unix::fs::symlink(&target, &mount).unwrap();
        assert!(super::empty_mount_directory(&mount).is_err());
        std::fs::write(target.join("child"), "unchanged").unwrap();
        assert!(super::empty_mount_directory(&target).is_err());
        assert_eq!(std::fs::read(target.join("child")).unwrap(), b"unchanged");
    }
    use super::*;

    struct FakeStorage {
        serial: String,
        windows_disks: Vec<u32>,
        layout: DiskLayout,
        volumes: Vec<CacheVolume>,
        mutations: usize,
        formats: usize,
    }
    impl Default for FakeStorage {
        fn default() -> Self {
            Self {
                serial: CACHE_DISK_SERIAL.into(),
                windows_disks: vec![0],
                layout: DiskLayout::Gpt(vec![PartitionKind::Reserved, PartitionKind::BasicData]),
                volumes: vec![CacheVolume {
                    path: "\\\\?\\Volume{cache}\\".into(),
                    disks: vec![7],
                    filesystem: "NTFS".into(),
                    label: crate::CACHE_LABEL.into(),
                }],
                mutations: 0,
                formats: 0,
            }
        }
    }
    #[test]
    fn captured_hyperv_serial_is_rechecked_before_any_cache_write() {
        let mut storage = FakeStorage {
            serial: "hyperv-stable-serial".into(),
            ..Default::default()
        };
        assert!(prepare_cache_with_serial(&mut storage, 7, "different-serial").is_err());
        assert_eq!(storage.mutations, 0);
        assert!(prepare_cache_with_serial(&mut storage, 7, "").is_err());
        assert_eq!(storage.mutations, 0);
        assert!(prepare_cache_with_serial(&mut storage, 7, "hyperv-stable-serial").is_ok());
        assert_eq!(storage.mutations, 1);
    }
    impl CacheStorage for FakeStorage {
        fn serial(&mut self) -> io::Result<String> {
            Ok(self.serial.clone())
        }
        fn windows_disks(&mut self) -> io::Result<Vec<u32>> {
            Ok(self.windows_disks.clone())
        }
        fn layout(&mut self) -> io::Result<DiskLayout> {
            Ok(self.layout.clone())
        }
        fn make_writable(&mut self) -> io::Result<()> {
            self.mutations += 1;
            Ok(())
        }
        fn initialize_raw(&mut self) -> io::Result<()> {
            self.formats += 1;
            self.layout = DiskLayout::Gpt(vec![PartitionKind::Reserved, PartitionKind::BasicData]);
            Ok(())
        }
        fn volumes(&mut self) -> io::Result<Vec<CacheVolume>> {
            Ok(self.volumes.clone())
        }
    }

    #[test]
    fn native_cache_rejects_running_windows_disk_before_mutation() {
        for layout in [
            DiskLayout::Raw,
            DiskLayout::Mbr {
                partition_count: 0,
                signature: 0,
            },
            DiskLayout::Gpt(vec![PartitionKind::BasicData]),
        ] {
            let mut fake = FakeStorage {
                windows_disks: vec![0, 7],
                layout,
                ..Default::default()
            };
            assert!(prepare_cache(&mut fake, 7).is_err());
            assert_eq!(fake.mutations, 0);
            assert_eq!(fake.formats, 0);
        }
    }

    #[test]
    fn native_cache_rejects_partitioned_mbr_before_mutation() {
        let mut fake = FakeStorage {
            layout: DiskLayout::Mbr {
                partition_count: 1,
                signature: 0,
            },
            ..Default::default()
        };
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.mutations, 0);
        assert_eq!(fake.formats, 0);
    }

    #[test]
    fn native_cache_rejects_signed_empty_mbr_before_mutation() {
        let mut fake = FakeStorage {
            layout: DiskLayout::Mbr {
                partition_count: 0,
                signature: 0x1234,
            },
            ..Default::default()
        };
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.mutations, 0);
        assert_eq!(fake.formats, 0);
    }

    #[test]
    fn native_cache_initializes_zero_partition_zero_signature_mbr() {
        let mut fake = FakeStorage {
            layout: DiskLayout::Mbr {
                partition_count: 0,
                signature: 0,
            },
            ..Default::default()
        };
        let prepared = prepare_cache(&mut fake, 7).unwrap();
        assert_eq!(prepared.volume, fake.volumes[0].path);
        assert_eq!(fake.mutations, 1);
        assert_eq!(fake.formats, 1);
    }

    #[test]
    fn native_cache_rejects_unknown_partition_before_mutation() {
        let mut fake = FakeStorage {
            layout: DiskLayout::Gpt(vec![PartitionKind::Other, PartitionKind::BasicData]),
            ..Default::default()
        };
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.mutations, 0);
    }

    #[test]
    fn native_cache_revalidates_exact_serial_before_mutation() {
        let mut fake = FakeStorage {
            serial: "motor-cache-other".into(),
            ..Default::default()
        };
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.mutations, 0);
    }

    #[test]
    fn native_cache_rejects_two_candidate_volumes() {
        let mut fake = FakeStorage::default();
        fake.volumes.push(fake.volumes[0].clone());
        assert!(prepare_cache(&mut fake, 7).is_err());
    }

    #[test]
    fn native_cache_rejects_wrong_filesystem_or_label_or_spanned_volume() {
        for case in 0..3 {
            let mut fake = FakeStorage::default();
            match case {
                0 => fake.volumes[0].filesystem = "ReFS".into(),
                1 => fake.volumes[0].label = "other".into(),
                _ => fake.volumes[0].disks.push(0),
            }
            assert!(prepare_cache(&mut fake, 7).is_err(), "case {case}");
        }
    }

    #[test]
    fn native_cache_rejects_empty_or_multiple_data_partitions() {
        for partitions in [
            vec![],
            vec![PartitionKind::BasicData; 2],
            vec![PartitionKind::Reserved],
        ] {
            let mut fake = FakeStorage {
                layout: DiskLayout::Gpt(partitions),
                ..Default::default()
            };
            assert!(prepare_cache(&mut fake, 7).is_err());
            assert_eq!(fake.mutations, 0);
        }
    }

    #[test]
    fn native_cache_formats_only_raw_and_reuses_gpt_without_powershell() {
        for raw in [false, true] {
            let mut fake = FakeStorage::default();
            if raw {
                fake.layout = DiskLayout::Raw;
            }
            let prepared = prepare_cache(&mut fake, 7).unwrap();
            assert_eq!(prepared.volume, fake.volumes[0].path);
            assert_eq!(prepared.freshly_formatted, raw);
            assert_eq!(fake.mutations, 1);
            assert_eq!(fake.formats, usize::from(raw));
            if !raw {
                assert_eq!(prepared.powershell_ms, 0);
            }
        }
    }

    #[test]
    fn cache_disk_selection_requires_exactly_one_stable_identity() {
        assert_eq!(
            select_cache_disk([(0, "system"), (4, "motor-cache"), (7, "other")]).unwrap(),
            4
        );
        assert!(select_cache_disk([(0, "system"), (1, "other")]).is_err());
        assert!(select_cache_disk([(1, "motor-cache"), (2, "motor-cache")]).is_err());
        assert!(select_cache_disk([(1, "motor-cache-other")]).is_err());
    }

    #[test]
    fn cache_serial_descriptor_requires_a_complete_bounded_identifier() {
        let mut descriptor = vec![0; 64];
        descriptor[24..28].copy_from_slice(&36u32.to_le_bytes());
        descriptor[36..47].copy_from_slice(b"motor-cache");
        assert_eq!(storage_serial(&descriptor).unwrap(), CACHE_DISK_SERIAL);
        for offset in [1u32, 35, 64, u32::MAX] {
            descriptor[24..28].copy_from_slice(&offset.to_le_bytes());
            assert!(storage_serial(&descriptor).is_err(), "{offset}");
        }
        assert!(storage_serial(&descriptor[..24]).is_err());
        descriptor[24..28].copy_from_slice(&36u32.to_le_bytes());
        assert!(storage_serial(&descriptor[..47]).is_err());
        descriptor[24..28].fill(0);
        assert_eq!(storage_serial(&descriptor).unwrap(), "");
    }

    #[test]
    fn cache_powershell_is_only_a_serial_guarded_raw_formatter() {
        let script = mount_script(7);
        assert!(!script.contains("Set-Disk"));
        assert!(script.contains("$d.SerialNumber.Trim() -cne 'motor-cache'"));
        assert!(script.contains("if($d.PartitionStyle -ne 'RAW')"));
        assert!(script.find("$d.SerialNumber").unwrap() < script.find("Initialize-Disk").unwrap());
    }

    #[test]
    fn cache_mount_powershell_has_balanced_braces_and_preserves_disk_guards() {
        let script = mount_script(7);
        let mut depth = 0;
        for c in script.chars() {
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
            }
            assert!(depth >= 0, "unmatched closing brace");
        }
        assert_eq!(depth, 0);
        assert!(script.contains("$d=Get-Disk -Number 7;"));
        assert!(!script.contains("Get-Disk |"));
        assert!(script.contains("if($d.IsBoot -or $d.IsSystem)"));
        assert!(script.contains("if($d.PartitionStyle -ne 'RAW')"));
        assert!(script.contains("Format-Volume -FileSystem NTFS"));
        assert!(script.contains(crate::CACHE_LABEL));
        assert!(script.find("$d.IsSystem").unwrap() < script.find("Initialize-Disk").unwrap());
    }
}
