//! Native queries for the sealed VM's RAW or GPT cache disk.

use motor_winagent::cache::{
    CACHE_DISK_SERIAL, CacheStorage, CacheVolume, DiskLayout, PartitionKind, mount_script,
    storage_serial,
};
use motor_winagent::cache_steps::{CacheReporter, CacheStep};
use std::{
    io,
    mem::{offset_of, size_of, zeroed},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{IO::DeviceIoControl, Ioctl::*, SystemInformation::GetWindowsDirectoryW},
};

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
fn extents(volume: &str, reporter: &CacheReporter) -> io::Result<Vec<u32>> {
    let handle = Handle::open(
        volume.trim_end_matches('\\'),
        0,
        reporter,
        CacheStep::VolumeOpen,
    )?;
    let bytes = query(
        &handle,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        reporter,
        CacheStep::VolumeExtents,
    )?;
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
        .collect()
}

pub struct WindowsStorage {
    disk: Handle,
    number: u32,
    reporter: CacheReporter,
}
impl WindowsStorage {
    pub fn open(number: u32, reporter: &CacheReporter) -> io::Result<Self> {
        Ok(Self {
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
    fn scan_volumes(&self) -> io::Result<Vec<CacheVolume>> {
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
            let disks = extents(&path, &self.reporter)?;
            if disks.contains(&self.number) {
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
}

impl CacheStorage for WindowsStorage {
    fn serial(&mut self) -> io::Result<String> {
        disk_serial(&self.disk, &self.reporter)
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
        extents(&text(&volume)?, &self.reporter)
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
            PARTITION_STYLE_MBR => Ok(DiskLayout::Mbr),
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
                    let kind = match (guid.data1, guid.data2, guid.data3, guid.data4) {
                        (
                            0xebd0a0a2,
                            0xb9e5,
                            0x4433,
                            [0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7],
                        ) => PartitionKind::BasicData,
                        (
                            0xe3c9e316,
                            0x0b5c,
                            0x4db8,
                            [0x81, 0x7d, 0xf9, 0x2d, 0xf0, 0x02, 0x15, 0xae],
                        ) => PartitionKind::Reserved,
                        _ => PartitionKind::Other,
                    };
                    parts.push(kind);
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
        if disk_serial(&self.disk, &self.reporter)? != CACHE_DISK_SERIAL {
            return Err(invalid("cache disk serial changed"));
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
        self.reporter.step(CacheStep::InitializeRaw, || {
            // The script rechecks serial, boot/system status and RAW before formatting.
            // Discard output to keep both process memory and the wait bounded.
            let mut child =
                Command::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &mount_script(self.number),
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => return Ok(()),
                    Ok(Some(status)) => {
                        return Err(io::Error::other(format!(
                            "cache initialization failed: {status}"
                        )));
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    result => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(result.err().unwrap_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::TimedOut,
                                "cache initialization timed out",
                            )
                        }));
                    }
                }
            }
        })
    }

    fn volumes(&mut self) -> io::Result<Vec<CacheVolume>> {
        self.reporter.step(CacheStep::VolumeDiscovery, || {
            // PnP may publish the volume after the disk has come online. Never retry
            // a malformed/ambiguous candidate, only an absent or not-yet-ready volume.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.scan_volumes() {
                    Ok(volumes) if !volumes.is_empty() => return Ok(volumes),
                    Err(error) if error.raw_os_error() != Some(ERROR_NOT_READY as i32) => {
                        return Err(error);
                    }
                    _ if Instant::now() >= deadline => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "cache volume did not appear",
                        ));
                    }
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }
        })
    }
}
