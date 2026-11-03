use super::{Plan, invalid};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Threading::*;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn checked(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn token_user(token: HANDLE) -> io::Result<Vec<u8>> {
    let mut size = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
    }
    let mut data = vec![0u8; size as usize];
    unsafe {
        checked(GetTokenInformation(
            token,
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        ))?;
    }
    Ok(data)
}

fn sid_text(token: HANDLE) -> io::Result<String> {
    let data = token_user(token)?;
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    unsafe {
        checked(ConvertSidToStringSidW(user.User.Sid, &mut text))?;
    }
    let _allocation = Local(text.cast());
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(text, length))
            .map_err(|_| invalid("invalid token SID"))
    }
}

fn require_system() -> io::Result<()> {
    let mut thread = null_mut();
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut thread) } != 0 {
        unsafe {
            CloseHandle(thread);
        }
        return Err(invalid("source sealing must not impersonate"));
    }
    if unsafe { GetLastError() } != ERROR_NO_TOKEN {
        return Err(io::Error::last_os_error());
    }
    let mut token = null_mut();
    unsafe {
        checked(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
    }
    let token = unsafe { File::from_raw_handle(token) };
    let data = token_user(token.as_raw_handle())?;
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    if unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) } == 0 {
        return Err(invalid("source sealing requires LocalSystem"));
    }
    Ok(())
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = unsafe { std::mem::zeroed() };
    unsafe {
        checked(GetFileInformationByHandle(file.as_raw_handle(), &mut info))?;
    }
    Ok(info)
}

fn open(path: &Path, directory: bool, access: u32) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | access)
        // Retaining ancestors without delete sharing prevents path replacement.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let info = information(&file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(invalid(
            "source seal path has unexpected type or reparse point",
        ));
    }
    Ok(file)
}

fn directories(root: &Path, plan: &Plan, access: u32) -> io::Result<Vec<File>> {
    let mut handles = Vec::new();
    for directory in plan.directories() {
        let handle = open(&root.join(directory), true, access)?;
        if let Some(first) = handles.first()
            && information(first)?.dwVolumeSerialNumber
                != information(&handle)?.dwVolumeSerialNumber
        {
            return Err(invalid("source directory crosses volume"));
        }
        handles.push(handle);
    }
    Ok(handles)
}

/// Break preparation-time hardlinks and restore bytes from the retained upload,
/// using the job token before entering the privileged descriptor-write phase.
pub fn refresh(root: &Path, archive: &mut File, plan: &Plan) -> io::Result<()> {
    let _parents = directories(root, plan, 0)?;
    let mut remaining: BTreeSet<_> = plan.files.iter().cloned().collect();
    archive.seek(SeekFrom::Start(0))?;
    for entry in tar::Archive::new(archive).entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().replace('\\', "/");
        let Some(relative) = path.strip_prefix("m/") else {
            continue;
        };
        if !remaining.remove(relative) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(invalid("selected source is not an uploaded regular file"));
        }
        let path = root.join(relative);
        let original = open(&path, false, 0)?;
        drop(original);
        // Never truncate a preparation alias: unlink only the source name and
        // materialize a new object while all of its ancestors remain pinned.
        std::fs::remove_file(&path)?;
        let mut fresh = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        io::copy(&mut entry, &mut fresh)?;
        let modified = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(entry.header().mtime()?))
            .ok_or_else(|| invalid("source timestamp out of range"))?;
        fresh.set_times(std::fs::FileTimes::new().set_modified(modified))?;
    }
    if !remaining.is_empty() {
        return Err(invalid("selected source missing from trusted upload"));
    }
    Ok(())
}

fn descriptor(sid: &str, directory: bool, surrogate: bool) -> String {
    // Denies are deliberately specific: denying GENERIC_WRITE also denies
    // SYNCHRONIZE, which ordinary readers require.
    let (deny, allow, inheritance) = if directory {
        ("0x000c0040", "0x001301bf", "OICI")
    } else {
        ("0x000d0116", "0x001200a9", "")
    };
    let owner = if surrogate {
        String::new()
    } else {
        "O:SY".into()
    };
    let owner_rights = if surrogate { "(A;;RC;;;OW)" } else { "" };
    format!(
        "{owner}D:P(D;;{deny};;;{sid}){owner_rights}(A;{inheritance};FA;;;SY)(A;{inheritance};FA;;;BA)(A;{inheritance};{allow};;;{sid})"
    )
}

fn set_descriptor(file: &File, sddl: &str, owner: bool) -> io::Result<()> {
    let mut sd = null_mut();
    unsafe {
        checked(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(sddl).as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        ))?;
    }
    let _allocation = Local(sd);
    // SetSecurityInfo propagates inheritable ACEs into existing descendants.
    // The kernel-object setter changes only this retained object: cache
    // junctions and existing output subtrees must never be normalized.
    unsafe {
        checked(SetKernelObjectSecurity(
            file.as_raw_handle(),
            DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION
                | if owner { OWNER_SECURITY_INFORMATION } else { 0 },
            sd,
        ))?;
    }
    Ok(())
}

fn apply_descriptors(root: &Path, plan: &Plan, sid: &str, surrogate: bool) -> io::Result<()> {
    let access = READ_CONTROL | WRITE_DAC | if surrogate { 0 } else { WRITE_OWNER };
    let parents = directories(root, plan, access)?;
    let mut files = Vec::new();
    for path in &plan.files {
        let file = open(&root.join(path), false, access)?;
        let info = information(&file)?;
        if info.nNumberOfLinks != 1
            || info.dwVolumeSerialNumber != information(&parents[0])?.dwVolumeSerialNumber
        {
            return Err(invalid("source file is aliased or crosses volume"));
        }
        files.push(file);
    }
    for parent in &parents {
        set_descriptor(parent, &descriptor(sid, true, surrogate), !surrogate)?;
    }
    for file in &files {
        set_descriptor(file, &descriptor(sid, false, surrogate), !surrogate)?;
    }
    Ok(())
}

/// Called only after all preparation and return exclusions, with no job alive.
pub fn apply(root: &Path, plan: &Plan, token: HANDLE) -> io::Result<()> {
    require_system()?;
    if root != Path::new("C:\\m") {
        return Err(invalid(
            "source sealing is limited to the disposable source root",
        ));
    }
    let source = open(root, true, 0)?;
    let os = open(Path::new("C:\\"), true, 0)?;
    // C:\t and T: alias the persistent cache. Read its identity, never its ACL.
    let cache = File::open(crate::CACHE_MOUNT).or_else(|_| {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(crate::CACHE_MOUNT)
    })?;
    let volume = information(&source)?.dwVolumeSerialNumber;
    if volume != information(&os)?.dwVolumeSerialNumber
        || volume == information(&cache)?.dwVolumeSerialNumber
    {
        return Err(invalid("source seal volume is not disposable"));
    }
    apply_descriptors(root, plan, &sid_text(token)?, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Cleanup {
        root: std::path::PathBuf,
        handles: Vec<File>,
        sid: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for handle in &self.handles {
                let _ = set_descriptor(handle, &format!("D:P(A;OICI;FA;;;{})", self.sid), false);
            }
            self.handles.clear();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn cleanup_handle(path: &Path) -> File {
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | WRITE_DAC)
            // Allow the directory itself to be renamed while retained for
            // teardown. Open descendant files would still block its rename.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap()
    }

    fn denied(result: io::Result<impl std::fmt::Debug>, operation: &str) {
        let error = result.expect_err(operation);
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32),
            "{operation}: {error}"
        );
    }

    fn change_security(path: &Path, information: u32, sddl: &str) -> io::Result<()> {
        let mut sd = null_mut();
        unsafe {
            checked(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            ))?;
        }
        let _allocation = Local(sd);
        checked(unsafe { SetFileSecurityW(wide(path.to_str().unwrap()).as_ptr(), information, sd) })
    }

    #[test]
    fn source_operation_matrix() {
        let mut token = null_mut();
        unsafe {
            checked(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY,
                &mut token,
            ))
            .unwrap();
        }
        let token = unsafe { File::from_raw_handle(token) };
        let sid = sid_text(token.as_raw_handle()).unwrap();
        println!(
            "job-side source ACL matrix SID={sid}; SYSTEM ownership requires the service qualification lane"
        );
        let root = cortexkit_test_support::scratch_root().join(format!(
            "source-seal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let names = [
            "write",
            "append",
            "truncate",
            "replace",
            "delete",
            "attribute",
            "time",
            "dacl",
            "owner",
        ];
        let plan = Plan::new(names.map(|name| format!("src/{name}")).into()).unwrap();
        for path in &plan.files {
            std::fs::write(root.join(path), b"original").unwrap();
        }
        std::fs::write(root.join("src/return"), b"original").unwrap();
        // Retain only directory WRITE_DAC handles for teardown. Open child-file
        // handles prevent directory renames even with delete sharing. Restoring
        // parent full control after assertions lets teardown delete sealed files
        // without reopening their denied WRITE_DAC handles.
        let _cleanup = Cleanup {
            root: root.clone(),
            handles: plan
                .directories()
                .iter()
                .map(|path| cleanup_handle(&root.join(path)))
                .collect(),
            sid: sid.clone(),
        };
        // An explicit full-parent control makes removal of the parent policy
        // observable even on images whose inherited scratch ACL is only Modify.
        for parent in plan.directories() {
            set_descriptor(
                &cleanup_handle(&root.join(parent)),
                &format!("D:P(A;OICI;FA;;;{sid})"),
                false,
            )
            .unwrap();
        }
        // Jobs cannot assign SYSTEM ownership. OWNER RIGHTS suppresses their
        // implicit owner WRITE_DAC for this job-side access-check matrix only.
        apply_descriptors(&root, &plan, &sid, true).unwrap();
        denied(std::fs::write(root.join("src/write"), b"changed"), "write");
        denied(
            OpenOptions::new()
                .append(true)
                .open(root.join("src/append"))
                .and_then(|mut f| f.write_all(b"changed")),
            "append",
        );
        denied(
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(root.join("src/truncate")),
            "truncate",
        );
        let from = root.join("src/replacement");
        std::fs::write(&from, b"changed").unwrap();
        denied(
            checked(unsafe {
                MoveFileExW(
                    wide(from.to_str().unwrap()).as_ptr(),
                    wide(root.join("src/replace").to_str().unwrap()).as_ptr(),
                    MOVEFILE_REPLACE_EXISTING,
                )
            }),
            "rename-over",
        );
        denied(
            checked(unsafe {
                DeleteFileW(wide(root.join("src/delete").to_str().unwrap()).as_ptr())
            }),
            "delete",
        );
        denied(
            checked(unsafe {
                SetFileAttributesW(
                    wide(root.join("src/attribute").to_str().unwrap()).as_ptr(),
                    FILE_ATTRIBUTE_READONLY,
                )
            }),
            "attribute",
        );
        denied(
            OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .open(root.join("src/time"))
                .and_then(|file| {
                    checked(unsafe {
                        SetFileTime(
                            file.as_raw_handle(),
                            std::ptr::null(),
                            std::ptr::null(),
                            &FILETIME {
                                dwLowDateTime: 0,
                                dwHighDateTime: 0,
                            },
                        )
                    })
                }),
            "time",
        );
        denied(
            change_security(
                &root.join("src/dacl"),
                DACL_SECURITY_INFORMATION,
                &format!("D:P(A;;FA;;;{sid})"),
            ),
            "dacl",
        );
        denied(
            change_security(
                &root.join("src/owner"),
                OWNER_SECURITY_INFORMATION,
                &format!("O:{sid}"),
            ),
            "owner",
        );
        denied(
            change_security(
                &root.join("src"),
                DACL_SECURITY_INFORMATION,
                &format!("D:P(A;OICI;FA;;;{sid})"),
            ),
            "parent dacl",
        );
        denied(
            change_security(
                &root.join("src"),
                OWNER_SECURITY_INFORMATION,
                &format!("O:{sid}"),
            ),
            "parent owner",
        );
        assert_eq!(std::fs::read(root.join("src/write")).unwrap(), b"original");
        std::fs::write(root.join("src/new"), b"allowed").unwrap();
        std::fs::create_dir(root.join("src/output")).unwrap();
        std::fs::write(root.join("src/output/file"), b"allowed").unwrap();
        std::fs::remove_dir_all(root.join("src/output")).unwrap();
        checked(unsafe {
            MoveFileExW(
                wide(from.to_str().unwrap()).as_ptr(),
                wide(root.join("src/return").to_str().unwrap()).as_ptr(),
                MOVEFILE_REPLACE_EXISTING,
            )
        })
        .unwrap();
        assert_eq!(std::fs::read(root.join("src/return")).unwrap(), b"changed");
        let moved = root.join("renamed-source");
        std::fs::rename(root.join("src"), &moved).unwrap();
        assert_eq!(std::fs::read(moved.join("write")).unwrap(), b"original");
        denied(
            std::fs::write(moved.join("write"), b"changed"),
            "write after folder rename",
        );
        denied(
            std::fs::remove_file(moved.join("delete")),
            "delete after folder rename",
        );
        std::fs::write(moved.join("new-after-rename"), b"allowed").unwrap();
        drop(_cleanup);
        assert!(
            !root.exists(),
            "directory-only teardown must remove the sealed fixture"
        );
    }

    #[test]
    fn refresh_breaks_writable_alias_and_rejects_reparse_ancestors() {
        let root = cortexkit_test_support::scratch_root().join(format!(
            "source-refresh-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("repo")).unwrap();
        std::fs::write(root.join("repo/source"), b"prepared alias").unwrap();
        std::fs::hard_link(root.join("repo/source"), root.join("writable-alias")).unwrap();
        let archive_path = root.join("upload.tar");
        let mut archive = tar::Builder::new(File::create(&archive_path).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(8);
        header.set_mtime(1234);
        header.set_cksum();
        archive
            .append_data(&mut header, "m/repo/source", &b"original"[..])
            .unwrap();
        archive.finish().unwrap();
        drop(archive);
        let plan = Plan::new(vec!["repo/source".into()]).unwrap();
        refresh(&root, &mut File::open(&archive_path).unwrap(), &plan).unwrap();
        assert_eq!(
            std::fs::read(root.join("repo/source")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.join("writable-alias")).unwrap(),
            b"prepared alias"
        );
        assert_eq!(
            information(&File::open(root.join("repo/source")).unwrap())
                .unwrap()
                .nNumberOfLinks,
            1
        );
        // A cache junction (and every other reparse ancestor) is refused even
        // before the privileged owner/DACL phase.
        let junction = root.join("junction");
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(root.join("repo"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan = Plan::new(vec!["junction/source".into()]).unwrap();
        assert!(directories(&root, &plan, 0).is_err());
        std::fs::remove_dir(junction).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
