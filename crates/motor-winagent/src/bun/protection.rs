//! Validate ownership and allow entries before publishing package data or reading secrets.
use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{addr_of, null_mut},
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER,
        Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
        CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        OWNER_SECURITY_INFORMATION, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
};

/// `other_access` is zero for host secrets and read/execute for the guest seed.
pub fn check(path: &Path, other_access: u32) -> io::Result<()> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut owner, mut dacl, mut descriptor) = (null_mut(), null_mut(), null_mut());
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    struct Descriptor(*mut std::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    let _descriptor = Descriptor(descriptor);
    let (mut system, mut administrators) = ([0u64; 9], [0u64; 9]);
    for (kind, buffer) in [
        (WinLocalSystemSid, &mut system),
        (WinBuiltinAdministratorsSid, &mut administrators),
    ] {
        let mut length = std::mem::size_of_val(buffer) as u32;
        if unsafe { CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr().cast(), &mut length) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let trusted = |sid| unsafe {
        EqualSid(sid, system.as_ptr().cast_mut().cast()) != 0
            || EqualSid(sid, administrators.as_ptr().cast_mut().cast()) != 0
    };
    if owner.is_null() || !trusted(owner) || dacl.is_null() {
        return Err(io::Error::other(
            "Bun host path has untrusted ownership or a null DACL",
        ));
    }
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let kind = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
        match kind {
            0 => {
                let sid = unsafe { addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) }
                    .cast_mut()
                    .cast();
                let mask = unsafe { (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask };
                if !trusted(sid) && mask & !other_access != 0 {
                    return Err(io::Error::other(
                        "Bun host path grants an untrusted identity access",
                    ));
                }
            }
            1 => (), // Deny entries cannot disclose bytes or permit modification.
            _ => return Err(io::Error::other("unsupported Bun host path access entry")),
        }
    }
    Ok(())
}
