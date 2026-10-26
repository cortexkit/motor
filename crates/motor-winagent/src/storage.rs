//! Native queries for the sealed VM's RAW or GPT cache disk.

use motor_winagent::cache::{
    BASIC_DATA_PARTITION, CacheStorage, CacheVolume, DiskIdentity, DiskLayout, ScsiAddress,
    cache_partition, partition_kind, require_uninitialized, storage_serial,
};
use motor_winagent::cache_steps::{CacheReporter, CacheStep};
use std::{
    io,
    mem::{offset_of, size_of, zeroed},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
    Storage::FileSystem::*,
    Storage::IscsiDisc::{IOCTL_SCSI_GET_ADDRESS, SCSI_ADDRESS},
    System::{IO::DeviceIoControl, Ioctl::*, SystemInformation::GetWindowsDirectoryW},
};
use windows_sys::core::GUID;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn text(value: &[u16]) -> io::Result<String> {
    let end = value
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(|| invalid("unterminated storage name"))?;
    String::from_utf16(&value[..end]).map_err(|_| invalid("invalid storage name"))
}
fn win32(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct Handle(HANDLE);
impl Handle {
    fn open(
        path: &str,
        access: u32,
        reporter: &CacheReporter,
        step: CacheStep,
    ) -> io::Result<Self> {
        reporter.step(step, || {
            let handle = unsafe {
                CreateFileW(
                    wide(path).as_ptr(),
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        })
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Volumes(HANDLE);
impl Drop for Volumes {
    fn drop(&mut self) {
        unsafe {
            FindVolumeClose(self.0);
        }
    }
}

// Variable-length IOCTL responses are capped, length-checked, and copied rather
// than borrowing possibly unaligned trailing records from a byte buffer.
fn query(
    handle: &Handle,
    code: u32,
    input: &[u8],
    reporter: &CacheReporter,
    step: CacheStep,
) -> io::Result<Vec<u8>> {
    reporter.step(step, || {
        let mut output = vec![0u8; 64 * 1024];
        let mut returned = 0;
        unsafe {
            win32(DeviceIoControl(
                handle.0,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                null_mut(),
            ))?;
        }
        if returned as usize > output.len() {
            return Err(invalid("oversized storage response"));
        }
        output.truncate(returned as usize);
        Ok(output)
    })
}
fn value<T: Copy>(bytes: &[u8], offset: usize) -> io::Result<T> {
    let end = offset
        .checked_add(size_of::<T>())
        .ok_or_else(|| invalid("storage response overflow"))?;
    let bytes = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("truncated storage response"))?;
    // Only storage records returned by the trusted Windows kernel use this helper.
    Ok(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}
fn command(
    handle: &Handle,
    code: u32,
    input: &SET_DISK_ATTRIBUTES,
    reporter: &CacheReporter,
) -> io::Result<()> {
    reporter.step(CacheStep::SetAttributes, || {
        let mut returned = 0;
        unsafe {
            win32(DeviceIoControl(
                handle.0,
                code,
                (input as *const SET_DISK_ATTRIBUTES).cast(),
                size_of::<SET_DISK_ATTRIBUTES>() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            ))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "disk attribute IOCTL Version={} size={} mask={:#x}: {error}",
                        input.Version,
                        size_of::<SET_DISK_ATTRIBUTES>(),
                        input.AttributesMask
                    ),
                )
            })
        }
    })
}
fn disk_serial(disk: &Handle, reporter: &CacheReporter) -> io::Result<String> {
    // Request the standard storage-device descriptor, including the disk serial.
    let bytes = query(
        disk,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &[0; 12],
        reporter,
        CacheStep::DiskSerial,
    )?;
    storage_serial(&bytes).map(str::to_owned)
}
/// Everything the guest can read that names this disk, in one pass over the
/// same handle the cache writes go through. A Hyper-V synthetic disk reports
/// no serial, so the bus address and the reported size carry the identity.
fn disk_identity(disk: &Handle, reporter: &CacheReporter) -> io::Result<DiskIdentity> {
    let descriptor = query(
        disk,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &[0; 12],
        reporter,
        CacheStep::DiskSerial,
    )?;
    let geometry = query(
        disk,
        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
        &[],
        reporter,
        CacheStep::DiskGeometry,
    )?;
    let number: STORAGE_DEVICE_NUMBER = value(
        &query(
            disk,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            &[],
            reporter,
            CacheStep::DiskDeviceNumber,
        )?,
        0,
    )?;
    // DISK_GEOMETRY_EX starts with the 24-byte DISK_GEOMETRY, followed by
    // DiskSize, the disk's length in bytes. The buffer's own length only
    // reflects the partition information that follows, so it can't identify
    // a disk.
    let disk_size: i64 = value(&geometry, size_of::<DISK_GEOMETRY>())?;
    Ok(DiskIdentity {
        serial: storage_serial(&descriptor)?.to_owned(),
        length_bytes: u64::try_from(disk_size).map_err(|_| invalid("negative disk size"))?,
        device_number: number.DeviceNumber,
        device_type: number.DeviceType,
        scsi: scsi_address(disk, reporter)?,
    })
}
fn scsi_address(disk: &Handle, reporter: &CacheReporter) -> io::Result<Option<ScsiAddress>> {
    let mut address: SCSI_ADDRESS = unsafe { zeroed() };
    address.Length = size_of::<SCSI_ADDRESS>() as u32;
    let mut returned = 0;
    let completed = reporter.step(CacheStep::DiskScsiAddress, || unsafe {
        win32(DeviceIoControl(
            disk.0,
            IOCTL_SCSI_GET_ADDRESS,
            null(),
            0,
            (&mut address as *mut SCSI_ADDRESS).cast(),
            size_of::<SCSI_ADDRESS>() as u32,
            &mut returned,
            null_mut(),
        ))
    });
    if completed.is_err() {
        // A disk the SCSI address query does not describe, such as a
        // non-SCSI device, answers with an unsupported request rather than
        // an address. Any other failure still refuses the disk.
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code)
                if code == ERROR_INVALID_FUNCTION as i32 || code == ERROR_NOT_SUPPORTED as i32 =>
            {
                Ok(None)
            }
            // Keep the code: whether a disk has a SCSI address is decided on it.
            _ => Err(error),
        };
    }
    if returned as usize != size_of::<SCSI_ADDRESS>() {
        return Err(invalid("truncated SCSI address"));
    }
    Ok(Some(ScsiAddress {
        port: address.PortNumber,
        path_id: address.PathId,
        target_id: address.TargetId,
        lun: address.Lun,
    }))
}
/// Send one fixed-size input structure to a disk control code that returns
/// nothing. These calls change the partition table, so they only run after
/// the layout and identity checks have passed.
fn control<T>(handle: &Handle, code: u32, input: &T) -> io::Result<()> {
    let mut returned = 0;
    unsafe {
        win32(DeviceIoControl(
            handle.0,
            code,
            (input as *const T).cast(),
            size_of::<T>() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        ))
    }
    // The error is returned as the system reported it: rebuilding it from a
    // custom message here would erase the Windows code callers decide on.
}
/// A random (version 4) GUID for the new disk and partition identifiers.
/// Windows renames a disk whose GUID collides with another attached disk, so
/// the cache must never reuse a fixed value.
fn random_guid() -> io::Result<GUID> {
    let mut bytes = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "random GUID failed: NTSTATUS {status:#x}"
        )));
    }
    let mut value = u128::from_be_bytes(bytes);
    value = (value & !(0xf << 76)) | (0x4 << 76);
    value = (value & !(0x3 << 62)) | (0x2 << 62);
    Ok(GUID::from_u128(value))
}
fn guid_value(guid: GUID) -> u128 {
    (u128::from(guid.data1) << 96)
        | (u128::from(guid.data2) << 80)
        | (u128::from(guid.data3) << 64)
        | u128::from(u64::from_be_bytes(guid.data4))
}
/// Disk numbers of a volume's extents, or `None` for a volume that isn't
/// backed by disk extents at all. A Hyper-V job VM's virtual DVD is such a
/// volume: it answers `ERROR_INVALID_FUNCTION`. A volume with no disk extents
/// can't lie on the cache disk, so callers looking for volumes on a disk skip
/// it. The code is checked inside the step, because the step's error wrapping
/// drops the raw OS error.
fn extents(volume: &str, reporter: &CacheReporter) -> io::Result<Option<Vec<u32>>> {
    let handle = Handle::open(
        volume.trim_end_matches('\\'),
        0,
        reporter,
        CacheStep::VolumeOpen,
    )?;
    let bytes = reporter.step(CacheStep::VolumeExtents, || {
        let mut output = vec![0u8; 64 * 1024];
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle.0,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                null(),
                0,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_FUNCTION as i32) {
                return Ok(None);
            }
            return Err(error);
        }
        if returned as usize > output.len() {
            return Err(invalid("oversized storage response"));
        }
        output.truncate(returned as usize);
        Ok(Some(output))
    })?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let count: u32 = value(&bytes, 0)?;
    if count == 0 || count > 128 {
        return Err(invalid("unexpected volume extent count"));
    }
    (0..count as usize)
        .map(|i| {
            let extent: DISK_EXTENT = value(
                &bytes,
                offset_of!(VOLUME_DISK_EXTENTS, Extents) + i * size_of::<DISK_EXTENT>(),
            )?;
            Ok(extent.DiskNumber)
        })
        .collect::<io::Result<Vec<u32>>>()
        .map(Some)
}

pub struct WindowsStorage {
    #[cfg(feature = "hyperv-runner")]
    expected_identity: DiskIdentity,
    disk: Handle,
    number: u32,
    reporter: CacheReporter,
}
impl WindowsStorage {
    pub fn open(number: u32, reporter: &CacheReporter) -> io::Result<Self> {
        Ok(Self {
            #[cfg(feature = "hyperv-runner")]
            expected_identity: DiskIdentity::default(),
            disk: Handle::open(
                &format!("\\\\.\\PhysicalDrive{number}"),
                GENERIC_READ | GENERIC_WRITE,
                reporter,
                CacheStep::DiskOpen,
            )?,
            number,
            reporter: reporter.clone(),
        })
    }
    /// Read what the guest can see of this disk and pin it as the identity.
    /// `make_writable` and `initialize_raw` compare against it through the
    /// same held handle before writing, so a disk swapped for another one
    /// under the running guest is refused rather than written.
    #[cfg(feature = "hyperv-runner")]
    pub fn capture_identity(&mut self) -> io::Result<DiskIdentity> {
        let identity = disk_identity(&self.disk, &self.reporter)?;
        self.expected_identity = identity.clone();
        Ok(identity)
    }
    /// Whether the disk behind the held handle is still the one to write.
    /// Hyper-V disks report no serial, so the whole identity pinned at
    /// selection must match. On the QEMU runner the cache disk is the one
    /// carrying the fixed `motor-cache` serial, and only that is compared:
    /// its other fields were never pinned.
    fn identity_unchanged(&self, actual: &DiskIdentity) -> bool {
        #[cfg(feature = "hyperv-runner")]
        {
            motor_winagent::cache::CacheIdentity::Hyperv(&self.expected_identity)
                .difference(actual)
                .is_none()
        }
        #[cfg(not(feature = "hyperv-runner"))]
        {
            motor_winagent::cache::CacheIdentity::Serial(motor_winagent::cache::CACHE_DISK_SERIAL)
                .difference(actual)
                .is_none()
        }
    }
    fn attributes(&self) -> io::Result<u64> {
        let bytes = query(
            &self.disk,
            IOCTL_DISK_GET_DISK_ATTRIBUTES,
            &[],
            &self.reporter,
            CacheStep::GetAttributes,
        )?;
        let attributes: GET_DISK_ATTRIBUTES = value(&bytes, 0)?;
        if attributes.Version != size_of::<GET_DISK_ATTRIBUTES>() as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "disk attributes version {} instead of {}",
                    attributes.Version,
                    size_of::<GET_DISK_ATTRIBUTES>()
                ),
            ));
        }
        Ok(attributes.Attributes)
    }
    /// Volume GUID paths of the volumes that include this disk, each with the
    /// disk numbers of its extents. Uses only queries that also work on a
    /// volume without a file system.
    fn disk_volumes(&self) -> io::Result<Vec<(String, Vec<u32>)>> {
        let mut name = [0u16; 256];
        let raw = self.reporter.step(CacheStep::FirstVolume, || {
            let raw = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
            if raw == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(raw)
            }
        })?;
        let volumes = Volumes(raw);
        let mut found = Vec::new();
        for _ in 0..128 {
            let path = text(&name)?;
            if let Some(disks) = extents(&path, &self.reporter)?
                && disks.contains(&self.number)
            {
                found.push((path, disks));
            }
            let more = self.reporter.step(CacheStep::NextVolume, || {
                if unsafe { FindNextVolumeW(volumes.0, name.as_mut_ptr(), name.len() as u32) } != 0
                {
                    return Ok(true);
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    Ok(false)
                } else {
                    Err(error)
                }
            })?;
            if !more {
                return Ok(found);
            }
        }
        Err(invalid("too many volumes"))
    }
    fn scan_volumes(&self) -> io::Result<Vec<CacheVolume>> {
        let mut found = Vec::new();
        for (path, disks) in self.disk_volumes()? {
            let mut label = [0u16; 256];
            let mut filesystem = [0u16; 256];
            self.reporter
                .step(CacheStep::VolumeInformation, || unsafe {
                    win32(GetVolumeInformationW(
                        wide(&path).as_ptr(),
                        label.as_mut_ptr(),
                        label.len() as u32,
                        null_mut(),
                        null_mut(),
                        null_mut(),
                        filesystem.as_mut_ptr(),
                        filesystem.len() as u32,
                    ))
                })?;
            found.push(CacheVolume {
                path,
                disks,
                filesystem: text(&filesystem)?,
                label: text(&label)?,
            });
        }
        Ok(found)
    }
    /// Wait for Windows to publish the volume of the partition just written.
    /// It must be the only volume on this disk and lie entirely on it.
    fn new_volume(&self) -> io::Result<String> {
        self.reporter.step(CacheStep::VolumeDiscovery, || {
            motor_winagent::cache::wait_for_new_volume(
                Instant::now() + Duration::from_secs(5),
                self.number,
                &|| self.disk_volumes(),
            )
        })
    }
}

impl CacheStorage for WindowsStorage {
    fn serial(&mut self) -> io::Result<String> {
        disk_serial(&self.disk, &self.reporter)
    }

    fn identity(&mut self) -> io::Result<DiskIdentity> {
        disk_identity(&self.disk, &self.reporter)
    }

    fn windows_disks(&mut self) -> io::Result<Vec<u32>> {
        let mut directory = [0u16; 32768];
        let length = self.reporter.step(CacheStep::WindowsDirectory, || {
            let length =
                unsafe { GetWindowsDirectoryW(directory.as_mut_ptr(), directory.len() as u32) };
            if length == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(length)
            }
        })?;
        if length as usize >= directory.len() {
            return Err(invalid("Windows directory is too long"));
        }
        let mut mount = [0u16; 32768];
        let mut volume = [0u16; 256];
        self.reporter.step(CacheStep::WindowsMountPath, || unsafe {
            win32(GetVolumePathNameW(
                directory.as_ptr(),
                mount.as_mut_ptr(),
                mount.len() as u32,
            ))
        })?;
        self.reporter
            .step(CacheStep::WindowsVolumeName, || unsafe {
                win32(GetVolumeNameForVolumeMountPointW(
                    mount.as_ptr(),
                    volume.as_mut_ptr(),
                    volume.len() as u32,
                ))
            })?;
        // The Windows volume must resolve to real disks; without them the
        // cache disk can't be proven separate from running Windows.
        extents(&text(&volume)?, &self.reporter)?
            .ok_or_else(|| invalid("Windows volume reports no disk extents"))
    }

    fn layout(&mut self) -> io::Result<DiskLayout> {
        let bytes = query(
            &self.disk,
            IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
            &[],
            &self.reporter,
            CacheStep::DiskLayout,
        )?;
        let style: i32 = value(
            &bytes,
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionStyle),
        )?;
        let count: u32 = value(
            &bytes,
            offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionCount),
        )?;
        match style {
            PARTITION_STYLE_RAW if count == 0 => Ok(DiskLayout::Raw),
            PARTITION_STYLE_MBR => {
                let mbr: DRIVE_LAYOUT_INFORMATION_MBR =
                    value(&bytes, offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous))?;
                Ok(DiskLayout::Mbr {
                    partition_count: count,
                    signature: mbr.Signature,
                })
            }
            PARTITION_STYLE_GPT => {
                if count > 128 {
                    return Err(invalid("too many disk partitions"));
                }
                let mut parts = Vec::new();
                for i in 0..count as usize {
                    let part: PARTITION_INFORMATION_EX = value(
                        &bytes,
                        offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry)
                            + i * size_of::<PARTITION_INFORMATION_EX>(),
                    )?;
                    if part.PartitionStyle != PARTITION_STYLE_GPT
                        || part.PartitionLength <= 0
                        || part.PartitionNumber == 0
                    {
                        return Err(invalid("invalid GPT partition"));
                    }
                    let guid = unsafe { part.Anonymous.Gpt.PartitionType };
                    parts.push(partition_kind(guid_value(guid)));
                }
                Ok(DiskLayout::Gpt(parts))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported disk partition style {style}, count {count}"),
            )),
        }
    }

    fn make_writable(&mut self) -> io::Result<()> {
        if !self.identity_unchanged(&disk_identity(&self.disk, &self.reporter)?) {
            return Err(invalid("cache disk identity changed"));
        }
        let mask = DISK_ATTRIBUTE_OFFLINE | DISK_ATTRIBUTE_READ_ONLY;
        if self.attributes()? & mask != 0 {
            let mut attributes: SET_DISK_ATTRIBUTES = unsafe { zeroed() };
            attributes.Version = size_of::<SET_DISK_ATTRIBUTES>() as u32;
            attributes.Persist = true;
            attributes.AttributesMask = mask;
            command(
                &self.disk,
                IOCTL_DISK_SET_DISK_ATTRIBUTES,
                &attributes,
                &self.reporter,
            )?;
            query(
                &self.disk,
                IOCTL_DISK_UPDATE_PROPERTIES,
                &[],
                &self.reporter,
                CacheStep::UpdateProperties,
            )?;
        }
        if self.attributes()? & mask != 0 {
            return Err(invalid("cache disk is not online and writable"));
        }
        Ok(())
    }

    fn initialize_raw(&mut self) -> io::Result<()> {
        let reporter = self.reporter.clone();
        reporter.step(CacheStep::InitializeRaw, || {
            let deadline = Instant::now() + Duration::from_secs(30);
            // Recheck the identity and RAW layout through the held handle, and
            // confirm the disks running Windows exclude this disk, immediately
            // before the first write. A disk that already has a partition
            // table (including one left by an interrupted initialization) is
            // refused; the host then discards it.
            if !self.identity_unchanged(&disk_identity(&self.disk, &reporter)?) {
                return Err(invalid("cache disk identity changed"));
            }
            let windows = self.windows_disks()?;
            if windows.is_empty() || windows.contains(&self.number) {
                return Err(invalid("cache disk may hold running Windows"));
            }
            require_uninitialized(self.layout()?)?;

            let mut create: CREATE_DISK = unsafe { zeroed() };
            create.PartitionStyle = PARTITION_STYLE_GPT;
            create.Anonymous.Gpt = CREATE_DISK_GPT {
                DiskId: random_guid()?,
                MaxPartitionCount: 128,
            };
            control(&self.disk, IOCTL_DISK_CREATE_DISK, &create)?;
            query(
                &self.disk,
                IOCTL_DISK_UPDATE_PROPERTIES,
                &[],
                &reporter,
                CacheStep::UpdateProperties,
            )?;

            // Read back the usable range Windows chose for the new GPT, then
            // write one Basic Data partition covering it.
            let bytes = query(
                &self.disk,
                IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
                &[],
                &reporter,
                CacheStep::DiskLayout,
            )?;
            let style: u32 = value(
                &bytes,
                offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionStyle),
            )?;
            if style != PARTITION_STYLE_GPT as u32 {
                return Err(invalid("created cache disk is not GPT"));
            }
            let gpt: DRIVE_LAYOUT_INFORMATION_GPT =
                value(&bytes, offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous))?;
            let (start, length) = cache_partition(gpt.StartingUsableOffset, gpt.UsableLength)?;
            let mut layout: DRIVE_LAYOUT_INFORMATION_EX = unsafe { zeroed() };
            layout.PartitionStyle = PARTITION_STYLE_GPT as u32;
            layout.PartitionCount = 1;
            layout.Anonymous.Gpt = gpt;
            let partition = &mut layout.PartitionEntry[0];
            partition.PartitionStyle = PARTITION_STYLE_GPT;
            partition.StartingOffset = start;
            partition.PartitionLength = length;
            partition.PartitionNumber = 1;
            partition.RewritePartition = true;
            let mut name = [0u16; 36];
            for (slot, unit) in name.iter_mut().zip("motor cache".encode_utf16()) {
                *slot = unit;
            }
            partition.Anonymous.Gpt = PARTITION_INFORMATION_GPT {
                PartitionType: GUID::from_u128(BASIC_DATA_PARTITION),
                PartitionId: random_guid()?,
                // Keep Windows from giving the cache a drive letter; it is only
                // reachable through its mount folder.
                Attributes: GPT_BASIC_DATA_ATTRIBUTE_NO_DRIVE_LETTER,
                Name: name,
            };
            control(&self.disk, IOCTL_DISK_SET_DRIVE_LAYOUT_EX, &layout)?;
            query(
                &self.disk,
                IOCTL_DISK_UPDATE_PROPERTIES,
                &[],
                &reporter,
                CacheStep::UpdateProperties,
            )?;

            let volume = self.new_volume()?;
            motor_winagent::cache_format::run(&volume, deadline)
        })
    }

    fn volumes(&mut self) -> io::Result<Vec<CacheVolume>> {
        self.reporter.step(CacheStep::VolumeDiscovery, || {
            // PnP may publish the volume after the disk has come online. Never retry
            // a malformed/ambiguous candidate, only an absent or not-yet-ready volume.
            motor_winagent::cache::wait_for_volumes(
                Instant::now() + Duration::from_secs(5),
                &|| self.scan_volumes(),
            )
        })
    }
}
