//! Cache disk identity and the bounded storage-management script.

use std::io;

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

pub fn mount_script(number: u32) -> String {
    format!(
        r#"$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new();
$watch=[Diagnostics.Stopwatch]::StartNew();
$d=Get-Disk -Number {number};
if($d.IsBoot -or $d.IsSystem) {{ throw 'cache disk is a boot or system disk' }};
# SAN policy can mark a previously initialized data disk offline and read-only.
$d | Set-Disk -IsReadOnly $false;
$d | Set-Disk -IsOffline $false;
$d=Get-Disk -Number {number};
if($d.IsOffline -or $d.IsReadOnly) {{ throw 'cache disk is not online and writable' }};
$setDiskMs=$watch.ElapsedMilliseconds; $watch.Restart();
if($d.PartitionStyle -eq 'RAW') {{ $d | Initialize-Disk -PartitionStyle GPT -PassThru | New-Partition -UseMaximumSize | Format-Volume -FileSystem NTFS -NewFileSystemLabel '{label}' -Confirm:$false | Out-Null }};
$v=@(Get-Partition -DiskNumber {number} | Get-Volume | Where-Object {{ $_.FileSystem -eq 'NTFS' -and $_.FileSystemLabel -eq '{label}' }});
if($v.Count -ne 1) {{ throw 'cache is not the expected NTFS volume' }};
@{{volume=$v[0].Path;set_disk_ms=$setDiskMs;volume_ms=$watch.ElapsedMilliseconds}} | ConvertTo-Json -Compress"#,
        label = crate::CACHE_LABEL,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(script.contains("if($d.PartitionStyle -eq 'RAW')"));
        assert!(script.contains("$_.FileSystem -eq 'NTFS'"));
        assert!(script.contains(crate::CACHE_LABEL));
        assert!(script.find("$d.IsSystem").unwrap() < script.find("Set-Disk").unwrap());
    }
}
