use serde_json::Value;
use std::{
    ffi::c_void,
    io::{self, Write},
    os::windows::io::{FromRawHandle, RawHandle},
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn token_user(token: HANDLE) -> io::Result<Vec<u64>> {
    let mut length = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0u64; (length as usize).div_ceil(8)];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}

fn sid_text(sid: *mut c_void) -> io::Result<String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let text = Local(text.cast());
    let mut length = 0;
    unsafe {
        while *text.0.cast::<u16>().add(length) != 0 {
            length += 1;
            if length > 1024 {
                return Err(io::Error::other("SYSTEM token SID is too long"));
            }
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text.0.cast::<u16>(),
            length,
        )))
    }
}

pub(super) fn require_system() -> io::Result<()> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let user = token_user(token.0)?;
    let user = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if sid_text(user.cast())? != "S-1-5-18" {
        return Err(io::Error::other(
            "SYSTEM is required for this Windows operation",
        ));
    }
    Ok(())
}

pub(super) fn write_diagnostic_update(path: &Path, value: &Value) -> io::Result<()> {
    let descriptor = wide("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)");
    let mut security_descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut security_descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let security_descriptor = Local(security_descriptor.cast());
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor.0,
        bInheritHandle: 0,
    };
    let raw = unsafe {
        CreateFileW(
            wide(&path.to_string_lossy()).as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attributes,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(raw as RawHandle) };
    file.write_all(&serde_json::to_vec_pretty(value).map_err(io::Error::other)?)?;
    file.sync_all()
}
