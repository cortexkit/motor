//! Cache identity, fail-closed storage decisions, and fresh-disk initialization.

use std::io;
use std::time::Instant;

pub const CACHE_DISK_SERIAL: &str = "motor-cache";

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
    Mbr,
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
    pub native_attributes_ms: u64,
    pub powershell_ms: u64,
    pub volume_ms: u64,
}

fn check_disk(storage: &mut impl CacheStorage, number: u32) -> io::Result<DiskLayout> {
    if storage.serial()? != CACHE_DISK_SERIAL {
        return Err(io::Error::other("cache disk serial changed"));
    }
    let windows = storage.windows_disks()?;
    if windows.is_empty() || windows.contains(&number) {
        return Err(io::Error::other("cache disk may hold running Windows"));
    }
    let layout = storage.layout()?;
    // Windows BIOS system disks are MBR; UEFI system disks contain an EFI
    // partition. Accept only the GPT data/MSR layout created for our caches.
    let allowed = match &layout {
        DiskLayout::Raw => true,
        DiskLayout::Mbr => false,
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
    let raw = matches!(check_disk(storage, number)?, DiskLayout::Raw);
    let started = Instant::now();
    storage.make_writable()?;
    let native_attributes_ms = started.elapsed().as_millis() as u64;
    let started = Instant::now();
    let mut powershell_ms = 0;
    if raw {
        storage.initialize_raw()?;
        powershell_ms = started.elapsed().as_millis() as u64;
        if !matches!(check_disk(storage, number)?, DiskLayout::Gpt(_)) {
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
        native_attributes_ms,
        powershell_ms,
        volume_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn mount_script(number: u32) -> String {
    format!(
        r#"$ErrorActionPreference='Stop';
$d=Get-Disk -Number {number};
if($d.IsBoot -or $d.IsSystem) {{ throw 'cache disk is a boot or system disk' }};
if($d.SerialNumber.Trim() -cne 'motor-cache') {{ throw 'cache disk serial changed' }};
if($d.IsOffline -or $d.IsReadOnly) {{ throw 'cache disk is not online and writable' }};
if($d.PartitionStyle -ne 'RAW') {{ throw 'only a RAW cache disk may be initialized' }};
$d | Initialize-Disk -PartitionStyle GPT -PassThru | New-Partition -UseMaximumSize | Format-Volume -FileSystem NTFS -NewFileSystemLabel '{label}' -Confirm:$false | Out-Null"#,
        label = crate::CACHE_LABEL,
    )
}

#[cfg(test)]
mod tests {
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
        let mut fake = FakeStorage {
            windows_disks: vec![0, 7],
            ..Default::default()
        };
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.mutations, 0);
    }

    #[test]
    fn native_cache_rejects_mbr_before_mutation() {
        let mut fake = FakeStorage {
            layout: DiskLayout::Mbr,
            ..Default::default()
        };
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.mutations, 0);
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
