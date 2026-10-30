use super::*;
use std::{
    os::windows::{
        ffi::OsStrExt,
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle},
    },
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
        EqualSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
        TokenUser, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
        SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, OPEN_EXISTING,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct Security {
    descriptor: *mut std::ffi::c_void,
    sid: Vec<usize>,
}
impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor) };
    }
}

impl Security {
    fn new() -> io::Result<Self> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0;
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size) };
        let mut sid = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = unsafe {
            GetTokenInformation(token, TokenUser, sid.as_mut_ptr().cast(), size, &mut size)
        };
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = unsafe { &*sid.as_ptr().cast::<TOKEN_USER>() };
        let mut text = null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        let user_sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        unsafe { LocalFree(text.cast()) };
        let sddl: Vec<_> = format!("O:{user_sid}D:P(A;OICI;FA;;;{user_sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor, sid })
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
    fn check(&self, file: &File) -> io::Result<()> {
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        let code = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        let mut expected = null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        let ok = unsafe {
            GetSecurityDescriptorDacl(self.descriptor, &mut present, &mut expected, &mut defaulted)
        };
        let user = unsafe { &*self.sid.as_ptr().cast::<TOKEN_USER>() };
        let equal_acl = |a: *const ACL, b: *const ACL| -> bool {
            if a.is_null() || b.is_null() {
                return false;
            }
            unsafe {
                std::slice::from_raw_parts(a.cast::<u8>(), (*a).AclSize as usize)
                    == std::slice::from_raw_parts(b.cast::<u8>(), (*b).AclSize as usize)
            }
        };
        let mut control = 0;
        let mut revision = 0;
        let controlled =
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } != 0;
        let private = controlled
            && control & SE_DACL_PROTECTED != 0
            && ok != 0
            && unsafe { EqualSid(owner, user.User.Sid) } != 0
            && equal_acl(acl, expected);
        unsafe { LocalFree(descriptor) };
        if !private {
            return Err(io::Error::other(
                "monitor storage must have a protected service-identity-only DACL",
            ));
        }
        Ok(())
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub fn directory(path: &Path) -> io::Result<File> {
    let security = Security::new()?;
    let absolute = std::path::absolute(path)?;
    let mut prefix = std::path::PathBuf::new();
    for part in absolute.components() {
        prefix.push(part);
        if !matches!(part, std::path::Component::Normal(_)) {
            continue;
        }
        if unsafe { CreateDirectoryW(wide(&prefix).as_ptr(), &security.attributes()) } == 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return Err(io::Error::last_os_error());
        }
        if std::fs::symlink_metadata(&prefix)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::other(
                "monitor storage cannot traverse reparse points",
            ));
        }
    }
    let file = handle(path, false, true, &security)?;
    security.check(&file)?;
    Ok(file)
}

fn handle(path: &Path, create: bool, directory: bool, security: &Security) -> io::Result<File> {
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            GENERIC_READ | if directory { 0 } else { GENERIC_WRITE },
            FILE_SHARE_READ,
            &security.attributes(),
            if create { CREATE_NEW } else { OPEN_EXISTING },
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "monitor storage cannot be a reparse point",
        ));
    }
    security.check(&file)?;
    Ok(file)
}

pub fn open(_parent: &File, path: &Path, create: bool) -> io::Result<File> {
    let security = Security::new()?;
    let file = handle(path, create, false, &security)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "monitor key/counter must be regular files",
        ));
    }
    Ok(file)
}
