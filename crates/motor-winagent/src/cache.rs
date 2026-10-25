//! Cache identity, fail-closed storage decisions, and fresh-disk initialization.

use std::io;
use std::time::Instant;

pub const CACHE_DISK_SERIAL: &str = "motor-cache";
/// `MSFT_Disk.BusType` value for SCSI.
pub const HYPERV_SCSI_BUS_TYPE: u16 = 1;
/// `MSFT_Disk.BusType` value for SAS. Disks on a Generation 2 VM's synthetic
/// SCSI controller report this inside the guest: measured on the Ally on
/// 2026-10-25 as bus_type 10 for both the OS disk (LUN 0) and the cache
/// disk (LUN 1).
pub const HYPERV_SAS_BUS_TYPE: u16 = 10;

/// Bus types a Hyper-V job VM's synthetic SCSI disks may report. Anything
/// else (USB, SATA, NVMe, a virtual DVD) is not a disk this runner attached.
/// The two-disk LUN topology in `select_hyperv_cache_disk` stays the real
/// guard on which disk is the cache.
pub const HYPERV_ACCEPTED_BUS_TYPES: [u16; 2] = [HYPERV_SCSI_BUS_TYPE, HYPERV_SAS_BUS_TYPE];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HypervDisk {
    pub number: u32,
    pub lun: u32,
    pub bus_type: u16,
    pub boot: bool,
    pub system: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HypervStorageDisk {
    pub number: u32,
    pub bus_type: u16,
    pub boot: bool,
    pub system: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HypervDiskLocation {
    pub number: u32,
    pub lun: u32,
}

pub fn join_hyperv_disk_inventory(
    storage_disks: &[HypervStorageDisk],
    locations: &[HypervDiskLocation],
) -> io::Result<Vec<HypervDisk>> {
    let mut by_number = std::collections::BTreeMap::new();
    for disk in storage_disks {
        if by_number.insert(disk.number, disk).is_some() {
            return Err(io::Error::other("duplicate storage disk number"));
        }
    }
    locations
        .iter()
        .map(|location| {
            let disk = by_number
                .get(&location.number)
                .ok_or_else(|| io::Error::other("SCSI location has no matching storage disk"))?;
            Ok(HypervDisk {
                number: location.number,
                lun: location.lun,
                bus_type: disk.bus_type,
                boot: disk.boot,
                system: disk.system,
            })
        })
        .collect()
}

/// This runner expects the OS at SCSI LUN 0 and the cache at LUN 1. Require the
/// complete two-disk topology, not merely the absence of a virtio serial.
pub fn select_hyperv_cache_disk(disks: &[HypervDisk]) -> io::Result<u32> {
    let unexpected_bus: Vec<_> = disks
        .iter()
        .filter(|disk| !HYPERV_ACCEPTED_BUS_TYPES.contains(&disk.bus_type))
        .map(|disk| {
            format!(
                "disk_number={} lun={} bus_type={} boot={} system={}",
                disk.number, disk.lun, disk.bus_type, disk.boot, disk.system
            )
        })
        .collect();
    if !unexpected_bus.is_empty() {
        return Err(io::Error::other(format!(
            "unexpected Hyper-V bus type; expected SCSI (1) or SAS (10): {}",
            unexpected_bus.join(", ")
        )));
    }
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

/// GPT partition type GUIDs, written as one integer in their textual order
/// (the layout `GUID::from_u128` expects).
pub const BASIC_DATA_PARTITION: u128 = 0xebd0a0a2_b9e5_4433_87c0_68b6b72699c7;
pub const RESERVED_PARTITION: u128 = 0xe3c9e316_0b5c_4db8_817d_f92df00215ae;

/// Classify a GPT partition type. The native formatter writes
/// `BASIC_DATA_PARTITION`, and the layout reader uses this same mapping, so a
/// freshly initialized disk is read back as exactly one `BasicData` partition.
pub fn partition_kind(partition_type: u128) -> PartitionKind {
    match partition_type {
        BASIC_DATA_PARTITION => PartitionKind::BasicData,
        RESERVED_PARTITION => PartitionKind::Reserved,
        _ => PartitionKind::Other,
    }
}

/// Windows can describe a virgin disk as MBR with neither partitions nor a
/// disk signature. Only that exact empty shape is equivalent to RAW.
fn normalize(layout: DiskLayout) -> DiskLayout {
    match layout {
        DiskLayout::Mbr {
            partition_count: 0,
            signature: 0,
        } => DiskLayout::Raw,
        layout => layout,
    }
}

/// The formatter's own guard, rechecked on the held disk handle immediately
/// before the first write. A disk that already has any partition table,
/// including one left by an interrupted initialization, is never rewritten.
pub fn require_uninitialized(layout: DiskLayout) -> io::Result<()> {
    match normalize(layout) {
        DiskLayout::Raw => Ok(()),
        _ => Err(io::Error::other("only a RAW cache disk may be initialized")),
    }
}

/// GPT data partitions start and end on 1 MiB boundaries, as Windows' own
/// disk tools place them.
pub const PARTITION_ALIGNMENT: i64 = 1 << 20;
const MIN_CACHE_PARTITION: i64 = 1 << 30;

/// Place the single cache partition inside the usable range Windows reports
/// for a newly created GPT. Returns (starting offset, length) in bytes.
pub fn cache_partition(starting_usable: i64, usable_length: i64) -> io::Result<(i64, i64)> {
    let invalid = || io::Error::other("cache disk has no usable GPT range");
    if starting_usable <= 0 || usable_length <= 0 {
        return Err(invalid());
    }
    let end = starting_usable
        .checked_add(usable_length)
        .ok_or_else(invalid)?;
    let start = starting_usable
        .checked_add(PARTITION_ALIGNMENT - 1)
        .ok_or_else(invalid)?
        / PARTITION_ALIGNMENT
        * PARTITION_ALIGNMENT;
    let length = (end - start) / PARTITION_ALIGNMENT * PARTITION_ALIGNMENT;
    if length < MIN_CACHE_PARTITION {
        return Err(invalid());
    }
    Ok((start, length))
}

/// Volume GUID paths have exactly this shape: `\\?\Volume{` + 36-character
/// GUID + `}\`. Anything else is refused before it reaches a format command.
pub fn is_volume_guid_path(path: &str) -> bool {
    let Some(guid) = path
        .strip_prefix("\\\\?\\Volume{")
        .and_then(|rest| rest.strip_suffix("}\\"))
    else {
        return false;
    };
    guid.len() == 36
        && guid.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Everything the guest itself can read that names the cache disk, read once
/// at selection and re-read through the same open handle before every write.
///
/// A Hyper-V synthetic SCSI disk carries no serial or product id, so what it
/// does expose is where the device is wired and how big it is. That is enough
/// to refuse a disk swapped in behind an already-selected disk number; which
/// cache belongs to which job stays the host's decision, made in the runner's
/// own code before the disk is ever attached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskIdentity {
    /// Serial number the device reports, empty when the device reports none.
    pub serial: String,
    /// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` length in bytes: the reported disk
    /// size plus partition-table information.
    pub length_bytes: u64,
    /// `IOCTL_STORAGE_GET_DEVICE_NUMBER`: the disk number Windows assigned and
    /// the media type of the device behind it.
    pub device_number: u32,
    pub device_type: u32,
    /// `IOCTL_SCSI_GET_ADDRESS` for SCSI-family disks: the bus address the
    /// virtual controller put the disk at. `None` on a disk that address
    /// query does not describe, such as a non-SCSI device.
    pub scsi: Option<ScsiAddress>,
}

/// Where a SCSI-family disk sits on its controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScsiAddress {
    pub port: u8,
    pub path_id: u8,
    pub target_id: u8,
    pub lun: u8,
}

impl DiskIdentity {
    /// A disk that reports no serial has nothing on the device itself to read;
    /// selection there rests on the SCSI topology and the recheck below.
    pub fn has_no_serial(&self) -> bool {
        self.serial.is_empty()
    }
}

/// One bounded log line naming every identity value. The next run on new
/// hardware shows in the log what the guest could read.
/// Host-supplied content, so control characters are replaced.
pub fn disk_identity_log(identity: &DiskIdentity) -> String {
    fn clean(value: &str) -> String {
        value
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(64)
            .collect()
    }
    let scsi = match identity.scsi {
        Some(scsi) => format!(
            "port={} path={} target={} lun={}",
            scsi.port, scsi.path_id, scsi.target_id, scsi.lun
        ),
        None => "not-scsi".to_owned(),
    };
    format!(
        "Hyper-V cache disk identity: serial={:?} length_bytes={} device_number={} device_type={} scsi={}",
        clean(&identity.serial),
        identity.length_bytes,
        identity.device_number,
        identity.device_type,
        scsi,
    )
}

/// The first field that differs, so a refusal can name it. Every field is
/// compared, so one log line answers "what changed" without a second run.
fn difference(expected: &DiskIdentity, actual: &DiskIdentity) -> Option<&'static str> {
    if expected.serial != actual.serial {
        Some("serial")
    } else if expected.length_bytes != actual.length_bytes {
        Some("length_bytes")
    } else if expected.device_number != actual.device_number {
        Some("device_number")
    } else if expected.device_type != actual.device_type {
        Some("device_type")
    } else if expected.scsi != actual.scsi {
        Some("scsi_address")
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub struct CacheVolume {
    pub path: String,
    pub disks: Vec<u32>,
    pub filesystem: String,
    pub label: String,
}

/// Physical-disk queries and attribute updates use one held handle. Before
/// changing the layout, the formatter must recheck the serial and the RAW
/// layout through that handle, and confirm the disks running Windows exclude
/// this disk.
pub trait CacheStorage {
    fn serial(&mut self) -> io::Result<String>;
    fn identity(&mut self) -> io::Result<DiskIdentity>;
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
    /// Time spent initializing a RAW disk. The name is kept from the
    /// PowerShell formatter it replaced because it is part of the wire format.
    pub powershell_ms: u64,
    pub volume_ms: u64,
}

fn check_disk(
    storage: &mut impl CacheStorage,
    number: u32,
    expected: &DiskIdentity,
) -> io::Result<DiskLayout> {
    let actual = storage.identity()?;
    if let Some(field) = difference(expected, &actual) {
        return Err(io::Error::other(format!(
            "cache disk identity changed: {field} differs; {}",
            disk_identity_log(&actual)
        )));
    }
    let windows = storage.windows_disks()?;
    if windows.is_empty() || windows.contains(&number) {
        return Err(io::Error::other("cache disk may hold running Windows"));
    }
    let layout = normalize(storage.layout()?);
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

/// Prepare a cache disk whose virtio serial number names it. QEMU assigns
/// that serial, and the guest re-reads it before every write.
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
    let identity = DiskIdentity {
        serial: serial.to_owned(),
        ..DiskIdentity::default()
    };
    prepare_cache_with(storage, number, &identity)
}

/// `expected` is what pins the disk between selection and every later write:
/// it is re-read through the same open handle before each one. A disk swapped
/// under a running guest, or the same disk number naming a different device,
/// therefore cannot receive a cache write meant for this cache. Which cache
/// belongs to which job is the host's decision, made before it attaches one.
pub fn prepare_cache_with(
    storage: &mut impl CacheStorage,
    number: u32,
    expected: &DiskIdentity,
) -> io::Result<PreparedCache> {
    let raw = matches!(check_disk(storage, number, expected)?, DiskLayout::Raw);
    let started = Instant::now();
    storage.make_writable()?;
    let native_attributes_ms = started.elapsed().as_millis() as u64;
    let started = Instant::now();
    let mut powershell_ms = 0;
    if raw {
        storage.initialize_raw()?;
        powershell_ms = started.elapsed().as_millis() as u64;
        if !matches!(check_disk(storage, number, expected)?, DiskLayout::Gpt(_)) {
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

#[cfg(test)]
mod tests {
    #[test]
    fn hyperv_cache_selection_requires_exact_two_disk_scsi_topology() {
        use super::*;
        let os = || HypervDisk {
            number: 0,
            lun: 0,
            bus_type: HYPERV_SCSI_BUS_TYPE,
            boot: true,
            system: true,
        };
        let cache = || HypervDisk {
            number: 1,
            lun: 1,
            bus_type: HYPERV_SCSI_BUS_TYPE,
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
        let mut sas_cache = cache();
        sas_cache.bus_type = HYPERV_SAS_BUS_TYPE;
        assert_eq!(select_hyperv_cache_disk(&[os(), sas_cache]).unwrap(), 1);
        let mut sata = cache();
        sata.bus_type = 11;
        let failure = select_hyperv_cache_disk(&[os(), sata]).unwrap_err();
        assert!(failure.to_string().contains("disk_number=1"));
        assert!(failure.to_string().contains("bus_type=11"));
    }

    #[test]
    fn hyperv_inventory_joins_storage_flags_to_scsi_locations_by_disk_number() {
        let storage = [
            HypervStorageDisk {
                number: 0,
                bus_type: HYPERV_SCSI_BUS_TYPE,
                boot: true,
                system: true,
            },
            HypervStorageDisk {
                number: 1,
                bus_type: HYPERV_SCSI_BUS_TYPE,
                boot: false,
                system: false,
            },
        ];
        let locations = [
            HypervDiskLocation { number: 1, lun: 1 },
            HypervDiskLocation { number: 0, lun: 0 },
        ];
        let disks = join_hyperv_disk_inventory(&storage, &locations).unwrap();
        assert_eq!(select_hyperv_cache_disk(&disks).unwrap(), 1);

        // The inventory a real Hyper-V Generation 2 job VM reported on the Ally:
        // both disks on the synthetic SCSI controller show bus type SAS.
        let mut ally = storage;
        ally[0].bus_type = HYPERV_SAS_BUS_TYPE;
        ally[1].bus_type = HYPERV_SAS_BUS_TYPE;
        let joined = join_hyperv_disk_inventory(&ally, &locations).unwrap();
        assert_eq!(select_hyperv_cache_disk(&joined).unwrap(), 1);

        let mut usb = storage;
        usb[1].bus_type = 7;
        let joined = join_hyperv_disk_inventory(&usb, &locations).unwrap();
        let failure = select_hyperv_cache_disk(&joined).unwrap_err();
        assert!(failure.to_string().contains("disk_number=1"));
        assert!(failure.to_string().contains("bus_type=7"));

        assert!(
            join_hyperv_disk_inventory(&storage, &[HypervDiskLocation { number: 7, lun: 1 }])
                .is_err()
        );
        assert!(join_hyperv_disk_inventory(&[storage[0], storage[0]], &locations).is_err());
    }

    #[test]
    fn hyperv_selection_still_refuses_three_disks_boot_cache_and_missing_lun() {
        let os = HypervDisk {
            number: 0,
            lun: 0,
            bus_type: HYPERV_SCSI_BUS_TYPE,
            boot: true,
            system: true,
        };
        let cache = HypervDisk {
            number: 1,
            lun: 1,
            bus_type: HYPERV_SCSI_BUS_TYPE,
            boot: false,
            system: false,
        };
        let mut boot_cache = cache;
        boot_cache.boot = true;
        assert!(select_hyperv_cache_disk(&[os, cache, HypervDisk { number: 2, ..cache }]).is_err());
        assert!(select_hyperv_cache_disk(&[os, boot_cache]).is_err());
        assert!(select_hyperv_cache_disk(&[os, HypervDisk { lun: 2, ..cache }]).is_err());
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
        /// What the disk itself reports. Every fixture states it, so the two
        /// fields cannot drift apart into a fake that is neither device.
        identity: DiskIdentity,
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
                identity: DiskIdentity {
                    serial: CACHE_DISK_SERIAL.into(),
                    ..DiskIdentity::default()
                },
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
            identity: DiskIdentity {
                serial: "hyperv-stable-serial".into(),
                ..DiskIdentity::default()
            },
            ..Default::default()
        };
        assert!(prepare_cache_with_serial(&mut storage, 7, "different-serial").is_err());
        assert_eq!(storage.mutations, 0);
        assert!(prepare_cache_with_serial(&mut storage, 7, "").is_err());
        assert_eq!(storage.mutations, 0);
        assert!(prepare_cache_with_serial(&mut storage, 7, "hyperv-stable-serial").is_ok());
        assert_eq!(storage.mutations, 1);
    }

    // What the Ally's Hyper-V job VM reported: an empty serial. The old code
    // refused it before any write, which is the failure this identity path
    // exists to replace.
    fn nameless_hyperv_identity() -> DiskIdentity {
        DiskIdentity {
            serial: String::new(),
            length_bytes: 40 << 30,
            device_number: 7,
            device_type: 0,
            scsi: Some(ScsiAddress {
                port: 0,
                path_id: 0,
                target_id: 0,
                lun: 1,
            }),
        }
    }

    #[test]
    fn nameless_hyperv_disk_is_prepared_instead_of_refused_for_its_missing_serial() {
        let identity = nameless_hyperv_identity();
        assert!(identity.has_no_serial());
        let mut storage = FakeStorage {
            identity,
            ..Default::default()
        };
        assert!(prepare_cache_with(&mut storage, 7, &nameless_hyperv_identity()).is_ok());
        assert_eq!(storage.mutations, 1);
        // The serial-only virtio path keeps refusing this disk, so the two
        // identification paths still disagree exactly where they should: the
        // guest log then shows which one a job ran with.
        let mut serial_path = FakeStorage::default();
        assert!(prepare_cache_with_serial(&mut serial_path, 7, "").is_err());
        assert_eq!(serial_path.mutations, 0);
    }

    #[test]
    fn changed_hyperv_disk_identity_is_refused_before_any_cache_write() {
        let expected = nameless_hyperv_identity();
        let scsi = |lun| {
            Some(ScsiAddress {
                port: 0,
                path_id: 0,
                target_id: 0,
                lun,
            })
        };
        for change in [
            DiskIdentity {
                serial: "other-serial".into(),
                ..expected.clone()
            },
            DiskIdentity {
                length_bytes: (40 << 30) + 4096,
                ..expected.clone()
            },
            DiskIdentity {
                device_number: 8,
                ..expected.clone()
            },
            DiskIdentity {
                device_type: 7,
                ..expected.clone()
            },
            DiskIdentity {
                scsi: scsi(2),
                ..expected.clone()
            },
            DiskIdentity {
                scsi: None,
                ..expected.clone()
            },
        ] {
            let mut storage = FakeStorage {
                serial: change.serial.clone(),
                identity: change.clone(),
                ..Default::default()
            };
            let Err(failure) = prepare_cache_with(&mut storage, 7, &expected) else {
                panic!("{change:?} was accepted");
            };
            assert!(
                failure
                    .to_string()
                    .starts_with("cache disk identity changed:"),
                "{failure}"
            );
            assert_eq!(storage.mutations, 0, "{change:?} was written");
            assert_eq!(storage.formats, 0, "{change:?} was formatted");
        }
        // The unchanged disk still passes, so the loop above fails only on
        // the field each case changed.
        let mut storage = FakeStorage {
            identity: expected.clone(),
            ..Default::default()
        };
        assert!(prepare_cache_with(&mut storage, 7, &expected).is_ok());
        assert_eq!(storage.mutations, 1);
    }

    #[test]
    fn identity_log_names_every_field_the_guest_read() {
        let line = disk_identity_log(&nameless_hyperv_identity());
        assert_eq!(
            line,
            "Hyper-V cache disk identity: serial=\"\" length_bytes=42949672960 \
             device_number=7 device_type=0 scsi=port=0 path=0 target=0 lun=1"
        );
        assert!(
            disk_identity_log(&DiskIdentity {
                serial: "serial\nwith-control".into(),
                ..DiskIdentity::default()
            })
            .contains("serial=\"serial with-control\"")
        );
        assert!(
            disk_identity_log(&DiskIdentity::default()).ends_with("scsi=not-scsi"),
            "a non-SCSI disk must say so instead of printing an empty address"
        );
    }
    impl CacheStorage for FakeStorage {
        fn serial(&mut self) -> io::Result<String> {
            Ok(self.serial.clone())
        }
        fn identity(&mut self) -> io::Result<DiskIdentity> {
            Ok(self.identity.clone())
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
            require_uninitialized(self.layout.clone())?;
            self.formats += 1;
            self.layout = DiskLayout::Gpt(vec![partition_kind(BASIC_DATA_PARTITION)]);
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
        // Selection chose the disk number by its serial; the disk itself now
        // reports a different one.
        let mut fake = FakeStorage {
            serial: CACHE_DISK_SERIAL.into(),
            identity: DiskIdentity {
                serial: "motor-cache-other".into(),
                ..DiskIdentity::default()
            },
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
    fn formatter_refuses_every_initialized_layout_including_interrupted_ones() {
        for layout in [
            DiskLayout::Raw,
            DiskLayout::Mbr {
                partition_count: 0,
                signature: 0,
            },
        ] {
            require_uninitialized(layout).unwrap();
        }
        // An initialization stopped after the GPT was written (no partition),
        // after the partition was added (no NTFS yet), or any foreign layout.
        for layout in [
            DiskLayout::Gpt(vec![]),
            DiskLayout::Gpt(vec![PartitionKind::Reserved]),
            DiskLayout::Gpt(vec![PartitionKind::BasicData]),
            DiskLayout::Mbr {
                partition_count: 1,
                signature: 0,
            },
            DiskLayout::Mbr {
                partition_count: 0,
                signature: 0x1234,
            },
        ] {
            let error = require_uninitialized(layout.clone()).unwrap_err();
            assert_eq!(
                error.to_string(),
                "only a RAW cache disk may be initialized",
                "{layout:?}"
            );
        }
    }

    #[test]
    fn stranded_partition_without_ntfs_volume_is_refused_not_reformatted() {
        // GPT plus the data partition, but the format never ran: the guest
        // must fail (so the host discards the disk), never format it again.
        let mut fake = FakeStorage {
            layout: DiskLayout::Gpt(vec![partition_kind(BASIC_DATA_PARTITION)]),
            volumes: vec![],
            ..Default::default()
        };
        assert_eq!(
            prepare_cache(&mut fake, 7).err().unwrap().to_string(),
            "cache volume not found"
        );
        assert_eq!(fake.formats, 0);
        // A RAW (unformatted) volume on that partition is refused the same way.
        let mut fake = FakeStorage {
            layout: DiskLayout::Gpt(vec![partition_kind(BASIC_DATA_PARTITION)]),
            ..Default::default()
        };
        fake.volumes[0].filesystem = "RAW".into();
        fake.volumes[0].label = String::new();
        assert!(prepare_cache(&mut fake, 7).is_err());
        assert_eq!(fake.formats, 0);
    }

    #[test]
    fn natively_initialized_layout_passes_the_reuse_checks() {
        assert_eq!(
            partition_kind(BASIC_DATA_PARTITION),
            PartitionKind::BasicData
        );
        assert_eq!(partition_kind(RESERVED_PARTITION), PartitionKind::Reserved);
        // The EFI system partition type must never be accepted as a cache.
        assert_eq!(
            partition_kind(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b),
            PartitionKind::Other
        );
        // The native formatter writes one partition of BASIC_DATA_PARTITION
        // and formats it NTFS with CACHE_LABEL. The next job must reuse that
        // disk as-is, without initializing it again.
        let mut fake = FakeStorage {
            layout: DiskLayout::Gpt(vec![partition_kind(BASIC_DATA_PARTITION)]),
            ..Default::default()
        };
        let prepared = prepare_cache(&mut fake, 7).unwrap();
        assert!(!prepared.freshly_formatted);
        assert_eq!(fake.formats, 0);
    }

    #[test]
    fn cache_partition_is_aligned_inside_the_usable_gpt_range() {
        // A 40 GiB disk: Windows reports usable space after the 34-sector
        // primary GPT and before the 33-sector backup.
        let disk: i64 = 40 << 30;
        let (start, length) = cache_partition(17_408, disk - 17_408 - 16_896).unwrap();
        assert_eq!(start, 1 << 20);
        assert_eq!(start % PARTITION_ALIGNMENT, 0);
        assert_eq!(length % PARTITION_ALIGNMENT, 0);
        assert!(start + length <= disk - 16_896);
        assert_eq!(length, disk - (2 << 20));
        // Already aligned input keeps its start.
        assert_eq!(
            cache_partition(1 << 20, 4 << 30).unwrap(),
            (1 << 20, 4 << 30)
        );
        for (offset, length) in [(0, 4 << 30), (17_408, 0), (17_408, 1 << 20), (i64::MAX, 1)] {
            assert!(
                cache_partition(offset, length).is_err(),
                "{offset} {length}"
            );
        }
    }

    #[test]
    fn native_formatter_rechecks_identity_and_raw_before_its_first_write() {
        // initialize_raw runs only in the Windows guest. Fence its order: every
        // guard must run before the partition table changes.
        let source = include_str!("storage.rs");
        let body = source
            .split_once("fn initialize_raw(&mut self)")
            .unwrap()
            .1
            .split_once("fn volumes(&mut self)")
            .unwrap()
            .0;
        let first_write = body.find("IOCTL_DISK_CREATE_DISK").unwrap();
        // The identity the guest read at selection, re-read through the same
        // held handle: a disk swapped for another one is refused here.
        for guard in [
            "disk_identity(&self.disk",
            "self.windows_disks()?",
            "require_uninitialized(self.layout()?)",
        ] {
            let at = body
                .find(guard)
                .unwrap_or_else(|| panic!("missing {guard}"));
            assert!(at < first_write, "{guard} runs after the first write");
        }
        assert!(body.find("IOCTL_DISK_SET_DRIVE_LAYOUT_EX").unwrap() > first_write);
        assert!(body.contains("motor_winagent::cache_format::run(&volume, deadline)"));
        assert!(!body.contains("powershell"));
    }
}
