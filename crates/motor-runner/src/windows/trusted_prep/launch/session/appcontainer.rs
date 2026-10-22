//! AppContainer identity for the prep fetch: the package SID, capability SIDs,
//! and the process attributes that make CreateProcessAsUserW start the child in
//! a lowbox token. Used by the read-only access screen and the diagnostic
//! launch cells; production launches do not use it yet.
use super::*;
type Hkey = HANDLE;

pub(super) const PACKAGE_NAME: &str = "MotorRunner.Prep";
/// Well-known capability SID for outbound internet access.
pub(super) const INTERNET_CLIENT: &str = "S-1-15-3-1";
/// PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES.
const SECURITY_CAPABILITIES_ATTRIBUTE: usize = 0x0002_0009;
/// PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY.
const ALL_APPLICATION_PACKAGES_POLICY_ATTRIBUTE: usize = 0x0002_000F;
/// PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT: run as a Less
/// Privileged AppContainer, ignoring ALL APPLICATION PACKAGES grants.
const ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 1;
const SE_GROUP_ENABLED_ATTRIBUTE: u32 = 4;

#[link(name = "userenv", kind = "raw-dylib")]
unsafe extern "system" {
    fn DeriveAppContainerSidFromAppContainerName(name: *const u16, sid: *mut PSID) -> i32;
    fn CreateAppContainerProfile(
        name: *const u16,
        display_name: *const u16,
        description: *const u16,
        capabilities: *const SID_AND_ATTRIBUTES,
        capability_count: u32,
        sid: *mut PSID,
    ) -> i32;
    fn DeleteAppContainerProfile(name: *const u16) -> i32;
    fn GetAppContainerFolderPath(sid: *const u16, path: *mut *mut u16) -> i32;
    fn GetAppContainerRegistryLocation(access: u32, key: *mut Hkey) -> i32;
}
#[link(name = "ole32", kind = "raw-dylib")]
unsafe extern "system" {
    fn CoTaskMemFree(memory: *const c_void);
}
#[link(name = "advapi32", kind = "raw-dylib")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: Hkey,
        subkey: *const u16,
        options: u32,
        access: u32,
        result: *mut Hkey,
    ) -> i32;
    fn RegCloseKey(key: Hkey) -> i32;
    fn RegCreateKeyExW(
        key: Hkey,
        subkey: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        access: u32,
        attributes: *const c_void,
        result: *mut Hkey,
        disposition: *mut u32,
    ) -> i32;
    fn RegDeleteKeyW(key: Hkey, subkey: *const u16) -> i32;
}
#[link(name = "kernelbase", kind = "raw-dylib")]
unsafe extern "system" {
    fn DeriveCapabilitySidsFromName(
        name: *const u16,
        group_sids: *mut *mut PSID,
        group_count: *mut u32,
        capability_sids: *mut *mut PSID,
        capability_count: *mut u32,
    ) -> i32;
}

/// DeriveAppContainerSidFromAppContainerName's SID must be released with FreeSid.
pub(super) struct DerivedSid(pub(super) PSID);
impl Drop for DerivedSid {
    fn drop(&mut self) {
        unsafe {
            FreeSid(self.0);
        }
    }
}
pub(super) fn package_sid() -> io::Result<(DerivedSid, String)> {
    let mut sid = null_mut();
    let result =
        unsafe { DeriveAppContainerSidFromAppContainerName(wide(PACKAGE_NAME).as_ptr(), &mut sid) };
    if result < 0 {
        return Err(step(
            "derive-package-sid",
            format!("HRESULT 0x{:08X}", result as u32),
        ));
    }
    let owned = DerivedSid(sid);
    let text = sid_string(owned.0)?;
    Ok((owned, text))
}
pub(super) fn capability_sids(name: &str) -> io::Result<Vec<String>> {
    let (mut groups, mut group_count, mut capabilities, mut count) = (null_mut(), 0, null_mut(), 0);
    check("derive-capability-sids", unsafe {
        DeriveCapabilitySidsFromName(
            wide(name).as_ptr(),
            &mut groups,
            &mut group_count,
            &mut capabilities,
            &mut count,
        )
    })?;
    let mut result = Vec::new();
    let mut failure = None;
    for index in 0..count as usize {
        let sid = unsafe { *capabilities.add(index) };
        match sid_string(sid) {
            Ok(text) => result.push(text),
            Err(error) => failure = Some(error),
        }
        unsafe { LocalFree(sid) };
    }
    for index in 0..group_count as usize {
        unsafe { LocalFree(*groups.add(index)) };
    }
    unsafe {
        LocalFree(capabilities.cast());
        LocalFree(groups.cast());
    }
    match failure {
        Some(error) => Err(error),
        None if result.is_empty() => Err(step("derive-capability-sids", "no capability SID")),
        None => Ok(result),
    }
}

/// Everything needed to give a child AppContainer identity. Attribute-based
/// launches keep these buffers alive until CreateProcessAsUserW returns; the
/// self-made-token cell uses the same SID and capability data with NtCreateLowBoxToken.
pub(crate) struct Launch {
    _package: DerivedSid,
    pub(super) package_text: String,
    _capability_sids: Vec<Local>,
    entries: Vec<SID_AND_ATTRIBUTES>,
    capabilities: Box<SECURITY_CAPABILITIES>,
    lpac_policy: Option<Box<u32>>,
    profile_name: Option<String>,
    profile_folder: Option<String>,
    profile_token: HANDLE,
    pub(super) info: Value,
}
impl Launch {
    pub(super) fn new(lpac: bool, private_network: bool) -> io::Result<Self> {
        Self::build(lpac, private_network, None)
    }
    pub(super) fn with_profile(lpac: bool, token: HANDLE) -> io::Result<Self> {
        Self::build(lpac, false, Some(token))
    }
    fn build(lpac: bool, private_network: bool, profile_token: Option<HANDLE>) -> io::Result<Self> {
        let mut names = vec![(
            "internetClient".to_owned(),
            vec![INTERNET_CLIENT.to_owned()],
        )];
        if lpac {
            for name in ["registryRead", "lpacCryptoServices"] {
                names.push((name.to_owned(), capability_sids(name)?));
            }
        }
        if private_network {
            // Test whether this documented grant changes getaddrinfo or private-network access.
            let name = "privateNetworkClientServer";
            names.push((name.to_owned(), capability_sids(name)?));
        }
        let capability_sids = names
            .iter()
            .flat_map(|(_, sids)| sids.iter())
            .map(|text| {
                let mut sid = null_mut();
                check("capability-sid", unsafe {
                    ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid)
                })?;
                Ok(Local(sid))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut entries = capability_sids
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.0,
                Attributes: SE_GROUP_ENABLED_ATTRIBUTE,
            })
            .collect::<Vec<_>>();
        let (
            package,
            profile_created,
            profile_name,
            profile_folder,
            profile_folder_error,
            profile_token,
        ) = match profile_token {
            Some(token) => {
                let (package, created, folder, folder_error) = create_profile(token, &entries)?;
                (
                    package,
                    created,
                    Some(PACKAGE_NAME.to_owned()),
                    folder,
                    folder_error,
                    token,
                )
            }
            None => {
                let (package, _) = package_sid()?;
                (package, false, None, None, None, null_mut())
            }
        };
        let package_text = sid_string(package.0)?;
        let capabilities = Box::new(SECURITY_CAPABILITIES {
            AppContainerSid: package.0,
            Capabilities: entries.as_mut_ptr(),
            CapabilityCount: entries.len() as u32,
            Reserved: 0,
        });
        let info = json!({"package_name": PACKAGE_NAME, "package_sid": package_text, "lpac": lpac,
            "private_network_client_server":private_network,
            "capabilities": names.iter().map(|(name, sids)| json!({"name": name, "sids": sids})).collect::<Vec<_>>(),
            "profile_name": profile_name, "profile_created": profile_created,
            "profile_folder": profile_folder, "profile_folder_error": profile_folder_error});
        Ok(Self {
            _package: package,
            package_text,
            _capability_sids: capability_sids,
            entries,
            capabilities,
            lpac_policy: lpac.then(|| Box::new(ALL_APPLICATION_PACKAGES_OPT_OUT)),
            profile_name,
            profile_folder,
            profile_token,
            info,
        })
    }
    /// Process attributes beside the explicit stdio handle list.
    pub(crate) fn attributes(&self) -> Vec<(usize, *const c_void, usize)> {
        let mut result = vec![(
            SECURITY_CAPABILITIES_ATTRIBUTE,
            (&raw const *self.capabilities).cast::<c_void>(),
            size_of::<SECURITY_CAPABILITIES>(),
        )];
        if let Some(policy) = &self.lpac_policy {
            result.push((
                ALL_APPLICATION_PACKAGES_POLICY_ATTRIBUTE,
                (&raw const **policy).cast::<c_void>(),
                size_of::<u32>(),
            ));
        }
        result
    }
    /// The self-created token already has its capabilities; only LPAC opt-out remains a process attribute.
    pub(super) fn policy_attribute(&self) -> Vec<(usize, *const c_void, usize)> {
        self.lpac_policy.as_ref().map_or_else(Vec::new, |policy| {
            vec![(
                ALL_APPLICATION_PACKAGES_POLICY_ATTRIBUTE,
                (&raw const **policy).cast::<c_void>(),
                size_of::<u32>(),
            )]
        })
    }
    pub(super) fn create_token(
        &self,
        base: HANDLE,
        object_directory: HANDLE,
    ) -> io::Result<Handle> {
        let create = nt_create_lowbox_token()?;
        let handles = [object_directory];
        let mut token = null_mut();
        let status = unsafe {
            create(
                &mut token,
                base,
                TOKEN_ALL_ACCESS,
                null_mut(),
                self._package.0,
                self.entries.len() as u32,
                self.entries.as_ptr(),
                handles.len() as u32,
                handles.as_ptr(),
            )
        };
        if status < 0 {
            return Err(native_error("NtCreateLowBoxToken", status));
        }
        Ok(Handle(token))
    }
    pub(super) fn delete_profile(&mut self) -> io::Result<bool> {
        let Some(name) = self.profile_name.as_ref() else {
            return Ok(false);
        };
        let name = name.clone();
        let mut impersonation = Impersonation::new(self.profile_token)?;
        let result = unsafe { DeleteAppContainerProfile(wide(&name).as_ptr()) };
        impersonation.revert()?;
        if result < 0 {
            return Err(step(
                "DeleteAppContainerProfile",
                format!("HRESULT 0x{:08X}", result as u32),
            ));
        }
        self.profile_name = None;
        Ok(true)
    }
}
impl Drop for Launch {
    fn drop(&mut self) {
        if let Some(name) = self.profile_name.clone() {
            let folder = self.profile_folder.clone();
            match self.delete_profile() {
                Ok(deleted) => eprintln!(
                    "AppContainer profile cleanup name={name} folder={folder:?} deleted={deleted}"
                ),
                Err(error) => eprintln!(
                    "AppContainer profile cleanup name={name} folder={folder:?} failed: {error}"
                ),
            }
        }
    }
}

struct Impersonation {
    active: bool,
}
impl Impersonation {
    fn new(token: HANDLE) -> io::Result<Self> {
        check("ImpersonateLoggedOnUser", unsafe {
            ImpersonateLoggedOnUser(token)
        })?;
        Ok(Self { active: true })
    }
    fn revert(&mut self) -> io::Result<()> {
        if self.active {
            check("RevertToSelf", unsafe { RevertToSelf() })?;
            self.active = false;
        }
        Ok(())
    }
}
impl Drop for Impersonation {
    fn drop(&mut self) {
        if self.active {
            unsafe { RevertToSelf() };
        }
    }
}

fn create_profile(
    token: HANDLE,
    entries: &[SID_AND_ATTRIBUTES],
) -> io::Result<(DerivedSid, bool, Option<String>, Option<String>)> {
    // AppContainer registrations belong to the impersonated user's loaded profile hive.
    let mut impersonation = Impersonation::new(token)?;
    let mut created = false;
    let operation = (|| {
        let mut sid = null_mut();
        let result = unsafe {
            CreateAppContainerProfile(
                wide(PACKAGE_NAME).as_ptr(),
                wide("MotorRunner prep diagnostic").as_ptr(),
                wide("Temporary AppContainer profile for the trusted prep launch diagnostic")
                    .as_ptr(),
                entries.as_ptr(),
                entries.len() as u32,
                &mut sid,
            )
        };
        let (package, profile_created) = if result >= 0 {
            (DerivedSid(sid), true)
        } else if result as u32 == 0x8007_00B7 {
            let (package, _) = package_sid()?;
            (package, false)
        } else {
            return Err(step(
                "CreateAppContainerProfile",
                format!("HRESULT 0x{:08X}", result as u32),
            ));
        };
        created = profile_created;
        let package_text = sid_string(package.0)?;
        let (folder, folder_error) = match profile_folder(&package_text) {
            Ok(path) => (Some(path), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Ok((package, profile_created, folder, folder_error))
    })();
    let revert = impersonation.revert();
    match operation {
        Ok((package, profile_created, folder, folder_error)) => {
            if let Err(error) = revert {
                let cleanup = if created {
                    unsafe { DeleteAppContainerProfile(wide(PACKAGE_NAME).as_ptr()) }
                } else {
                    0
                };
                return Err(step(
                    "RevertToSelf",
                    format!("{error}; profile_cleanup_hresult=0x{:08X}", cleanup as u32),
                ));
            }
            Ok((package, profile_created, folder, folder_error))
        }
        Err(error) => {
            let cleanup = if created {
                unsafe { DeleteAppContainerProfile(wide(PACKAGE_NAME).as_ptr()) }
            } else {
                0
            };
            Err(step(
                "CreateAppContainerProfile",
                format!(
                    "{error}; profile_cleanup_hresult=0x{:08X}; revert={revert:?}",
                    cleanup as u32
                ),
            ))
        }
    }
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}
type NtOpenDirectory = unsafe extern "system" fn(*mut HANDLE, u32, *mut ObjectAttributes) -> i32;
type NtCreateDirectoryEx =
    unsafe extern "system" fn(*mut HANDLE, u32, *mut ObjectAttributes, HANDLE, u32) -> i32;
type NtCloseHandle = unsafe extern "system" fn(HANDLE) -> i32;
type NtLowboxToken = unsafe extern "system" fn(
    *mut HANDLE,
    HANDLE,
    u32,
    *mut ObjectAttributes,
    PSID,
    u32,
    *const SID_AND_ATTRIBUTES,
    u32,
    *const HANDLE,
) -> i32;
type NtStatusToDosError = unsafe extern "system" fn(i32) -> u32;

fn nt_proc(name: &'static [u8]) -> io::Result<*const c_void> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    let module = unsafe { GetModuleHandleW(wide("ntdll.dll").as_ptr()) };
    if module.is_null() {
        return Err(step("GetModuleHandleW(ntdll)", io::Error::last_os_error()));
    }
    let address = unsafe { GetProcAddress(module, name.as_ptr()) }
        .ok_or_else(|| step("GetProcAddress(ntdll)", String::from_utf8_lossy(name)))?;
    Ok(address as *const () as *const c_void)
}
fn nt_open_directory() -> io::Result<NtOpenDirectory> {
    Ok(unsafe {
        std::mem::transmute::<*const c_void, NtOpenDirectory>(nt_proc(b"NtOpenDirectoryObject\0")?)
    })
}
fn nt_create_directory() -> io::Result<NtCreateDirectoryEx> {
    Ok(unsafe {
        std::mem::transmute::<*const c_void, NtCreateDirectoryEx>(nt_proc(
            b"NtCreateDirectoryObjectEx\0",
        )?)
    })
}
fn nt_close() -> io::Result<NtCloseHandle> {
    Ok(unsafe { std::mem::transmute::<*const c_void, NtCloseHandle>(nt_proc(b"NtClose\0")?) })
}
fn nt_create_lowbox_token() -> io::Result<NtLowboxToken> {
    Ok(unsafe {
        std::mem::transmute::<*const c_void, NtLowboxToken>(nt_proc(b"NtCreateLowBoxToken\0")?)
    })
}
fn nt_status_to_dos_error() -> io::Result<NtStatusToDosError> {
    Ok(unsafe {
        std::mem::transmute::<*const c_void, NtStatusToDosError>(nt_proc(
            b"RtlNtStatusToDosError\0",
        )?)
    })
}
fn native_error(call: &str, status: i32) -> io::Error {
    let code = nt_status_to_dos_error()
        .map(|convert| unsafe { convert(status) })
        .unwrap_or(0);
    step(
        call,
        format!(
            "NTSTATUS 0x{:08X}; Win32 {} (0x{:08X})",
            status as u32, code, code
        ),
    )
}

struct NativeHandle(HANDLE);
impl NativeHandle {
    fn close(&mut self) -> io::Result<()> {
        if !self.0.is_null() {
            let status = unsafe { nt_close()?(self.0) };
            if status < 0 {
                return Err(native_error("NtClose", status));
            }
            self.0 = null_mut();
        }
        Ok(())
    }
}
impl Drop for NativeHandle {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn unicode_object_attributes(
    name: &mut [u16],
    root: HANDLE,
    security_descriptor: *mut c_void,
) -> (UnicodeString, ObjectAttributes) {
    let length = (name.len().saturating_sub(1) * size_of::<u16>()) as u16;
    let maximum_length = length + size_of::<u16>() as u16;
    let mut unicode = UnicodeString {
        length,
        maximum_length,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: root,
        object_name: &mut unicode,
        attributes: 0x40,
        security_descriptor,
        security_quality_of_service: null_mut(),
    };
    (unicode, attributes)
}

fn open_directory(path: &str, access: u32) -> io::Result<HANDLE> {
    let open = nt_open_directory()?;
    let mut name = wide(path);
    let (mut unicode, mut attributes) =
        unicode_object_attributes(&mut name, null_mut(), null_mut());
    attributes.object_name = &mut unicode;
    let mut handle = null_mut();
    let status = unsafe { open(&mut handle, access, &mut attributes) };
    if status < 0 {
        Err(native_error("NtOpenDirectoryObject", status))
    } else {
        Ok(handle)
    }
}

fn directory_probe(path: &str) -> Value {
    match open_directory(path, 0x0001) {
        Ok(handle) => {
            let mut handle = NativeHandle(handle);
            let close = handle.close().map(|()| "closed");
            json!({"path":path,"exists":true,"close_status":close.map_err(|error| error.to_string())})
        }
        Err(error) => {
            let text = error.to_string();
            let missing =
                text.contains("NTSTATUS 0xC0000034") || text.contains("NTSTATUS 0xC000003A");
            let exists: Option<bool> = if missing { Some(false) } else { None };
            json!({"path":path,"exists":exists,"error":text})
        }
    }
}

pub(super) struct ObjectDirectory {
    parent: NativeHandle,
    directory: NativeHandle,
    path: String,
}
impl ObjectDirectory {
    pub(super) fn create(session_id: u32, package_sid: &str) -> io::Result<Self> {
        let path = format!(r"\Sessions\{session_id}\AppContainerNamedObjects\{package_sid}");
        let parent_path = format!(r"\Sessions\{session_id}\AppContainerNamedObjects");
        let parent = NativeHandle(open_directory(&parent_path, 0x0001 | 0x0008).map_err(
            |error| {
                step(
                    "NtOpenDirectoryObject(parent)",
                    format!("path={parent_path}; {error}"),
                )
            },
        )?);
        // Keep this temporary object namespace private to SYSTEM and the package SID.
        let security = descriptor(&format!("O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;{package_sid})"))?;
        let mut name = wide(package_sid);
        let (mut unicode, mut attributes) =
            unicode_object_attributes(&mut name, parent.0, security.0);
        attributes.object_name = &mut unicode;
        let mut directory = null_mut();
        let create = nt_create_directory().map_err(|error| {
            step(
                "NtCreateDirectoryObjectEx",
                format!("path={}; {error}", path),
            )
        })?;
        let status = unsafe { create(&mut directory, 0x000F_000F, &mut attributes, null_mut(), 0) };
        if status < 0 {
            return Err(native_error("NtCreateDirectoryObjectEx", status));
        }
        Ok(Self {
            parent,
            directory: NativeHandle(directory),
            path,
        })
    }
    pub(super) fn handle(&self) -> HANDLE {
        self.directory.0
    }
    pub(super) fn close(&mut self) -> io::Result<Value> {
        let mut errors = Vec::new();
        for result in [self.directory.close(), self.parent.close()] {
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if !errors.is_empty() {
            return Err(step(
                "object-directory-cleanup",
                format!("path={}; {}", self.path, errors.join("; ")),
            ));
        }
        let after = directory_probe(&self.path);
        let removed = after["exists"] == json!(false);
        if !removed {
            return Err(step(
                "object-directory-cleanup",
                format!("path={}; post-close-probe={after}", self.path),
            ));
        }
        Ok(
            json!({"path":self.path,"handles_closed":self.directory.0.is_null() && self.parent.0.is_null(),"directory_removed":true,"post_close_probe":after}),
        )
    }
}
impl Drop for ObjectDirectory {
    fn drop(&mut self) {
        if !self.directory.0.is_null() || !self.parent.0.is_null() {
            match self.close() {
                Ok(report) => eprintln!("AppContainer object-directory cleanup: {report}"),
                Err(error) => eprintln!("AppContainer object-directory cleanup failed: {error}"),
            }
        }
    }
}

pub(super) fn precheck_directories(session_id: u32, package_sid: &str) -> Value {
    let root = format!(r"\Sessions\{session_id}\AppContainerNamedObjects");
    let child = format!(r"{root}\{package_sid}");
    json!({"root":directory_probe(&root),"package":directory_probe(&child)})
}

pub(super) fn profile_folder(package_sid: &str) -> io::Result<String> {
    let mut path = null_mut();
    let result = unsafe { GetAppContainerFolderPath(wide(package_sid).as_ptr(), &mut path) };
    if result < 0 {
        return Err(step(
            "GetAppContainerFolderPath",
            format!("HRESULT 0x{:08X}", result as u32),
        ));
    }
    let text = unsafe { read_wide(path) };
    unsafe { CoTaskMemFree(path.cast()) };
    text
}

pub(super) fn profile_registry_probe(package_sid: &str, leaf: &str) -> Value {
    const KEY_WRITE: u32 = 0x0002_0006;
    const REG_OPTION_VOLATILE: u32 = 1;
    const REG_CREATED_NEW_KEY: u32 = 1;
    let display_path = format!(
        r"HKCU\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Storage\{package_sid}\{leaf}"
    );
    let mut root = null_mut();
    let result = unsafe { GetAppContainerRegistryLocation(KEY_WRITE, &mut root) };
    if result < 0 {
        return json!({"path":display_path,"write":"error","error":format!("GetAppContainerRegistryLocation HRESULT 0x{:08X}", result as u32)});
    }
    let name = wide(leaf);
    let mut key = null_mut();
    let mut disposition = 0;
    let status = unsafe {
        RegCreateKeyExW(
            root,
            name.as_ptr(),
            0,
            null_mut(),
            REG_OPTION_VOLATILE,
            KEY_WRITE,
            null(),
            &mut key,
            &mut disposition,
        )
    };
    let (write, cleanup) = if status == 0 {
        let closed = unsafe { RegCloseKey(key) };
        let deleted = unsafe { RegDeleteKeyW(root, name.as_ptr()) };
        (
            "allowed",
            json!({"key_closed":closed == 0,"key_removed":deleted == 0,"new_key":disposition == REG_CREATED_NEW_KEY}),
        )
    } else if status == 5 {
        ("denied", json!(null))
    } else {
        ("error", json!({"win32":status}))
    };
    let closed_root = unsafe { RegCloseKey(root) };
    json!({"path":display_path,"write":write,"cleanup":cleanup,"registry_root_closed":closed_root == 0})
}

pub(super) fn precheck_profile_registry(user_sid: &str, package_sid: &str) -> Value {
    const KEY_READ: u32 = 0x0002_0019;
    let hkey_users = (0x8000_0003u32 as i32 as isize) as Hkey;
    let hive_path = user_sid.to_owned();
    let mut hive = null_mut();
    let hive_status = unsafe {
        RegOpenKeyExW(
            hkey_users,
            wide(&hive_path).as_ptr(),
            0,
            KEY_READ,
            &mut hive,
        )
    };
    let hive_open = hive_status == 0;
    if hive_open {
        unsafe { RegCloseKey(hive) };
    }
    let mapping_path = format!(
        r"{user_sid}\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings\{package_sid}"
    );
    let mut mapping = null_mut();
    let mapping_status = unsafe {
        RegOpenKeyExW(
            hkey_users,
            wide(&mapping_path).as_ptr(),
            0,
            KEY_READ,
            &mut mapping,
        )
    };
    let mapping_exists = mapping_status == 0;
    if mapping_exists {
        unsafe { RegCloseKey(mapping) };
    }
    json!({
        "hive_path":format!(r"HKEY_USERS\{user_sid}"),
        "profile_hive_loaded":hive_open,
        "profile_hive_open_status":hive_status,
        "mapping_path":format!(r"HKEY_USERS\{mapping_path}"),
        "mapping_exists":mapping_exists,
        "mapping_open_status":mapping_status
    })
}

pub(super) fn current_package_sid() -> io::Result<String> {
    let mut token = null_mut();
    check("OpenProcessToken(AppContainer SID)", unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
    })?;
    let token = Handle(token);
    let bytes = token_info(token.0, TokenAppContainerSid)?;
    let sid =
        unsafe { (*bytes.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer };
    if sid.is_null() {
        return Err(step("TokenAppContainerSid", "child is not an AppContainer"));
    }
    sid_string(sid)
}

/// Child-side readback of the probe's own token: whether it really is a lowbox
/// token, its package SID, capabilities and integrity level.
pub(super) fn current_context() -> Value {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return json!({"error": io::Error::last_os_error().to_string()});
    }
    let token = Handle(token);
    let is_app_container = token_info(token.0, TokenIsAppContainer)
        .map(|bytes| bytes[0] & 0xFFFF_FFFF != 0)
        .map_err(|error| error.to_string());
    let package = token_info(token.0, TokenAppContainerSid)
        .and_then(|bytes| {
            let sid = unsafe {
                (*bytes.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer
            };
            if sid.is_null() {
                Ok(None)
            } else {
                sid_string(sid).map(Some)
            }
        })
        .map_err(|error| error.to_string());
    let capabilities = token_info(token.0, TokenCapabilities)
        .and_then(|bytes| {
            let header = unsafe { &*bytes.as_ptr().cast::<TOKEN_GROUPS>() };
            let groups = unsafe {
                std::slice::from_raw_parts(header.Groups.as_ptr(), header.GroupCount as usize)
            };
            groups
                .iter()
                .map(|group| sid_string(group.Sid))
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|error| error.to_string());
    let integrity = token_info(token.0, TokenIntegrityLevel)
        .and_then(|bytes| {
            sid_string(unsafe { (*bytes.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid })
        })
        .map_err(|error| error.to_string());
    json!({"is_app_container": is_app_container, "package_sid": package, "capabilities": capabilities, "integrity_sid": integrity})
}
