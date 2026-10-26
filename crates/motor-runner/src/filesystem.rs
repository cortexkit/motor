//! Host filesystem operations whose Unix and Windows durability APIs differ.
use std::fs;
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

pub(crate) fn read_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if file.metadata()?.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::other("reparse point refused"));
        }
    }
    Ok(file)
}
pub(crate) fn identity(path: &Path) -> io::Result<(u64, u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        Ok((
            metadata.dev(),
            metadata.ino(),
            metadata.blocks().saturating_mul(512),
        ))
    }
    #[cfg(windows)]
    {
        use std::{
            mem::zeroed,
            os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        };
        use windows_sys::Win32::Storage::FileSystem::*;
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other(
                "reparse point refused during disk accounting",
            ));
        }
        let inode = u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow);
        let mut standard: FILE_STANDARD_INFO = unsafe { zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&raw mut standard).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let allocated = u64::try_from(standard.AllocationSize)
            .map_err(|_| io::Error::other("negative host file allocation size"))?;
        Ok((info.dwVolumeSerialNumber.into(), inode, allocated))
    }
}
pub(crate) fn device(path: &Path) -> io::Result<u64> {
    Ok(identity(path)?.0)
}

/// Updating a directory's timestamp requires a directory handle on Windows;
/// File::open uses ordinary file semantics there and returns access denied.
pub(crate) fn set_directory_modified(
    path: &Path,
    modified: std::time::SystemTime,
) -> io::Result<()> {
    let context = |operation: &str, error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("{operation} {}: {error}", path.display()),
        )
    };
    #[cfg(unix)]
    let directory =
        File::open(path).map_err(|error| context("open directory for timestamp", error))?;
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
        };
        OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| context("open directory for timestamp", error))?
    };
    let metadata = directory
        .metadata()
        .map_err(|error| context("read directory timestamp metadata", error))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reparse directory refused during timestamp update: {}",
                    path.display()
                ),
            ));
        }
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "not a directory during timestamp update: {}",
                path.display()
            ),
        ));
    }
    directory
        .set_times(fs::FileTimes::new().set_modified(modified))
        .map_err(|error| context("set directory timestamp", error))
}

#[cfg(test)]
mod directory_timestamp_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn directory_timestamp_update_preserves_files_and_reports_the_failed_path() {
        let scratch = ScratchDir::new("directory-timestamp");
        let directory = scratch.join("mirror");
        fs::create_dir(&directory).unwrap();
        let head = directory.join("HEAD");
        fs::write(&head, b"ref: refs/heads/main\n").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        set_directory_modified(&directory, modified).unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(fs::read(&head).unwrap(), b"ref: refs/heads/main\n");
        let missing = scratch.join("absent-mirror");
        let error = set_directory_modified(&missing, modified).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains("open directory for timestamp"),
            "{error}"
        );
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "{error}"
        );
    }
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        // Windows has no directory fsync. Publication uses MoveFileExW's
        // WRITE_THROUGH below; native cache reconciliation discards stale disks.
        let _ = path;
        Ok(())
    }
}
pub(crate) fn rename(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let dest: Vec<_> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                dest.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root = cortexkit_test_support::scratch_root()
                .join(format!("motor-host-fs-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn nofollow_reads_the_actual_regular_file() {
        use std::io::Read;
        let scratch = Scratch::new();
        let path = scratch.0.join("file");
        fs::write(&path, b"host data").unwrap();
        let mut bytes = Vec::new();
        read_nofollow(&path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"host data");
        assert_eq!(device(&path).unwrap(), device(&scratch.0).unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn nofollow_refuses_a_symlink_to_a_readable_outside_file() {
        let scratch = Scratch::new();
        let outside = scratch.0.join("private-key");
        let link = scratch.0.join("claimed-output");
        fs::write(&outside, b"private").unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(read_nofollow(&link).is_err());
        fs::remove_file(&link).unwrap();
        fs::write(&link, b"regular").unwrap();
        assert!(read_nofollow(&link).is_ok());
    }
    #[test]
    fn disk_identity_deduplicates_real_hard_links() {
        let scratch = Scratch::new();
        let file = scratch.0.join("file");
        let link = scratch.0.join("hardlink");
        fs::write(&file, b"allocated").unwrap();
        fs::hard_link(&file, &link).unwrap();
        assert_eq!(identity(&file).unwrap(), identity(&link).unwrap());
    }
}
