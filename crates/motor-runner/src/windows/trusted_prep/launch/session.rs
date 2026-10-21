use super::{check, step, wide, DiagnosticPolicy, Handle, Plan};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    fs, io,
    mem::{size_of, zeroed},
    os::windows::{
        fs::MetadataExt,
        io::{FromRawHandle, RawHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree, SetLastError, HANDLE, INVALID_HANDLE_VALUE},
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{
        Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
        StationsAndDesktops::*,
        Threading::*,
    },
    UI::{
        Shell::{LoadUserProfileW, UnloadUserProfile, PROFILEINFOW},
        WindowsAndMessaging::{WINSTA_ALL_ACCESS, WSF_VISIBLE},
    },
};

struct Local(*mut c_void);
const FETCH_TOKEN_FLAGS: u32 = DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED;
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn win32_code(name: &str, code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(step(
            name,
            format!(
                "{} (Win32 0x{code:08X})",
                io::Error::from_raw_os_error(code as i32)
            ),
        ))
    }
}
fn descriptor(sddl: &str) -> io::Result<Local> {
    let mut result = null_mut();
    check("security-descriptor", unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(sddl).as_ptr(),
            1,
            &mut result,
            null_mut(),
        )
    })?;
    Ok(Local(result))
}
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    check("sid-string", unsafe {
        ConvertSidToStringSidW(sid, &mut text)
    })?;
    let _owned = Local(text.cast());
    unsafe { read_wide(text) }
}
unsafe fn read_wide(text: *const u16) -> io::Result<String> {
    let mut length = 0;
    while length < 32768 && *text.add(length) != 0 {
        length += 1;
    }
    if length == 32768 {
        return Err(step("wide-string", "unterminated Windows value"));
    }
    String::from_utf16(std::slice::from_raw_parts(text, length))
        .map_err(|error| step("wide-string", error))
}
fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut length = 0;
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(step("token-information", io::Error::last_os_error()));
    }
    let mut bytes = vec![0u64; (length as usize).div_ceil(8)];
    check("token-information", unsafe {
        GetTokenInformation(token, class, bytes.as_mut_ptr().cast(), length, &mut length)
    })?;
    Ok(bytes)
}
fn identity(token: HANDLE) -> io::Result<(String, String)> {
    let user = token_info(token, TokenUser)?;
    let user_sid = sid_string(unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid })?;
    let groups = token_info(token, TokenGroups)?;
    let groups = unsafe { &*groups.as_ptr().cast::<TOKEN_GROUPS>() };
    let groups =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    let mut logon = None;
    for group in groups {
        let sid = sid_string(group.Sid)?;
        if sid == "S-1-5-32-544" {
            return Err(step(
                "fetch-identity",
                "motor-prep must not be an administrator",
            ));
        }
        // SE_GROUP_LOGON_ID is the pair of high attribute bits on the logon SID.
        if group.Attributes & 0xc000_0000 == 0xc000_0000 {
            logon = Some(sid);
        }
    }
    if user_sid == "S-1-5-18" {
        return Err(step("fetch-identity", "Cargo must not run as SYSTEM"));
    }
    let logon = logon.ok_or_else(|| step("fetch-identity", "batch token has no logon SID"))?;
    if logon == user_sid || !logon.starts_with("S-1-5-5-") {
        return Err(step(
            "fetch-identity",
            "restriction requires a distinct logon SID",
        ));
    }
    Ok((user_sid, logon))
}
pub(super) fn require_system() -> io::Result<()> {
    let mut token = null_mut();
    check("broker-auth", unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
    })?;
    let token = Handle(token);
    let user = token_info(token.0, TokenUser)?;
    let sid = sid_string(unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid })?;
    if sid != "S-1-5-18" {
        return Err(step(
            "broker-auth",
            "prep-launch requires SYSTEM; fetch still runs as motor-prep",
        ));
    }
    Ok(())
}
fn enable_profile_privileges() -> io::Result<()> {
    let mut token = null_mut();
    check("profile-privileges", unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    })?;
    let token = Handle(token);
    for name in ["SeBackupPrivilege", "SeRestorePrivilege"] {
        let mut privileges: TOKEN_PRIVILEGES = unsafe { zeroed() };
        privileges.PrivilegeCount = 1;
        check("profile-privileges", unsafe {
            LookupPrivilegeValueW(
                null(),
                wide(name).as_ptr(),
                &mut privileges.Privileges[0].Luid,
            )
        })?;
        privileges.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        unsafe {
            SetLastError(0);
        }
        check("profile-privileges", unsafe {
            AdjustTokenPrivileges(token.0, 0, &privileges, 0, null_mut(), null_mut())
        })?;
        win32_code("profile-privileges", unsafe { GetLastError() })?;
    }
    Ok(())
}

struct Profile {
    token: Handle,
    handle: HANDLE,
}
impl Profile {
    fn load(token: Handle) -> io::Result<Self> {
        enable_profile_privileges()?;
        let mut username = wide("motor-prep");
        let mut info: PROFILEINFOW = unsafe { zeroed() };
        info.dwSize = size_of::<PROFILEINFOW>() as u32;
        info.dwFlags = 1; // PI_NOUI: a batch fetch never prompts on the host.
        info.lpUserName = username.as_mut_ptr();
        check("LoadUserProfile", unsafe {
            LoadUserProfileW(token.0, &mut info)
        })?;
        Ok(Self {
            token,
            handle: info.hProfile,
        })
    }
    fn close(&mut self) -> io::Result<()> {
        if !self.handle.is_null() {
            check("UnloadUserProfile", unsafe {
                UnloadUserProfile(self.token.0, self.handle)
            })?;
            self.handle = null_mut();
        }
        Ok(())
    }
    fn environment(&self) -> io::Result<BTreeMap<String, String>> {
        struct Block(*mut c_void);
        impl Drop for Block {
            fn drop(&mut self) {
                unsafe {
                    DestroyEnvironmentBlock(self.0);
                }
            }
        }
        let mut pointer = null_mut();
        check("CreateEnvironmentBlock", unsafe {
            CreateEnvironmentBlock(&mut pointer, self.token.0, 0)
        })?;
        let owned = Block(pointer);
        let pointer = owned.0.cast::<u16>();
        let mut length = 0;
        unsafe {
            while length < 1024 * 1024 {
                if *pointer.add(length) == 0 && *pointer.add(length + 1) == 0 {
                    return motor_winagent::environment::parse_block(std::slice::from_raw_parts(
                        pointer,
                        length + 2,
                    ));
                }
                length += 1;
            }
        }
        Err(step(
            "CreateEnvironmentBlock",
            "unterminated user environment",
        ))
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("{error}");
        }
    }
}

pub(super) fn write_private(path: &Path, bytes: &[u8], create_new: bool) -> io::Result<()> {
    use std::io::Write;
    let security = descriptor("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)")?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    let raw = unsafe {
        CreateFileW(
            wide(&path.to_string_lossy()).as_ptr(),
            0x40000000,
            FILE_SHARE_READ,
            &attributes,
            if create_new {
                CREATE_NEW
            } else {
                CREATE_ALWAYS
            },
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(step("private-launch-file", io::Error::last_os_error()));
    }
    let mut file = unsafe { fs::File::from_raw_handle(raw as RawHandle) };
    file.write_all(bytes)?;
    file.sync_all()
}

pub(super) fn no_reparse(root: &Path, path: &Path) -> io::Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| step("staging-path", error))?;
    let mut current = root.to_owned();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            current.push(component);
        }
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(step("staging-path", "reparse points are forbidden"));
        }
    }
    Ok(())
}

struct StagingGrant {
    handle: Handle,
    _descriptor: Local,
    old: *mut ACL,
    active: bool,
}
impl StagingGrant {
    fn new(path: &Path, sid: PSID) -> io::Result<Self> {
        let raw = unsafe {
            CreateFileW(
                wide(&path.to_string_lossy()).as_ptr(),
                0x0006_0080,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(step("staging-logon-grant", io::Error::last_os_error()));
        }
        let handle = Handle(raw);
        let mut old = null_mut();
        let mut security = null_mut();
        win32_code("staging-logon-grant", unsafe {
            GetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut old,
                null_mut(),
                &mut security,
            )
        })?;
        let descriptor = Local(security);
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: 0x0013_01bf, // Modify, without WRITE_DAC/WRITE_OWNER.
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_GROUP,
                ptstrName: sid.cast(),
            },
        };
        let mut acl = null_mut();
        win32_code("staging-logon-grant", unsafe {
            SetEntriesInAclW(1, &access, old, &mut acl)
        })?;
        let acl = Local(acl.cast());
        win32_code("staging-logon-grant", unsafe {
            SetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl.0.cast(),
                null(),
            )
        })?;
        Ok(Self {
            handle,
            _descriptor: descriptor,
            old,
            active: true,
        })
    }
    fn close(&mut self) -> io::Result<()> {
        if self.active {
            // Use the original no-follow handle, not a child-controlled path.
            win32_code("staging-logon-revoke", unsafe {
                SetSecurityInfo(
                    self.handle.0,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    self.old,
                    null(),
                )
            })?;
            self.active = false;
        }
        Ok(())
    }
}
impl Drop for StagingGrant {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("{error}");
        }
    }
}

struct Desktop {
    station: HWINSTA,
    desktop: HDESK,
    name: String,
}
impl Desktop {
    fn new(logon_sid: &str, explicit_writes: bool) -> io::Result<Self> {
        let name = format!("MotorPrep-{}", uuid::Uuid::now_v7().simple());
        let station_name = wide(&name);
        let desktop_name = wide("Fetch");
        let initial = descriptor("O:SYG:SYD:P(A;;GA;;;SY)")?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: initial.0,
            bInheritHandle: 0,
        };
        unsafe {
            SetLastError(0);
        }
        let station = unsafe {
            CreateWindowStationW(
                station_name.as_ptr(),
                0,
                WINSTA_ALL_ACCESS as u32 | 0x000f_0000,
                &attributes,
            )
        };
        let existed = unsafe { GetLastError() } == 183;
        if station.is_null() {
            return Err(step("CreateWindowStation", io::Error::last_os_error()));
        }
        let mut result = Self {
            station,
            desktop: null_mut(),
            name: format!("{name}\\Fetch"),
        };
        if existed {
            return Err(step("CreateWindowStation", "private name already exists"));
        }
        struct Restore(HWINSTA);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    SetProcessWindowStation(self.0);
                }
            }
        }
        let original = unsafe { GetProcessWindowStation() };
        if original.is_null() {
            return Err(step("GetProcessWindowStation", io::Error::last_os_error()));
        }
        check("SetProcessWindowStation-private", unsafe {
            SetProcessWindowStation(station)
        })?;
        let restore = Restore(original);
        result.desktop = unsafe {
            CreateDesktopExW(
                desktop_name.as_ptr(),
                null(),
                null(),
                0,
                0x000f_01ff,
                &attributes,
                4096,
                null(),
            )
        };
        let creation_error = io::Error::last_os_error();
        check("SetProcessWindowStation-restore", unsafe {
            SetProcessWindowStation(original)
        })?;
        drop(restore);
        if result.desktop.is_null() {
            return Err(step("CreateDesktopEx", creation_error));
        }
        // The creator retains its management handles; only this logon session
        // receives access by name. Never grant access to the service's desktop.
        for (object, mask) in [
            (result.station, 0x000f_037fu32),
            (result.desktop, 0x000f_01ffu32),
        ] {
            let extra = if explicit_writes {
                format!("(A;;0x{mask:08X};;;{logon_sid})")
            } else {
                String::new()
            };
            let final_acl = descriptor(&format!("O:SYG:SYD:P(A;;GA;;;{logon_sid}){extra}"))?;
            check("private-desktop-dacl", unsafe {
                SetUserObjectSecurity(object, &DACL_SECURITY_INFORMATION, final_acl.0)
            })?;
            // Do not depend on the SYSTEM creator's default mandatory label.
            // Label-only updates use WRITE_OWNER, not an extra security privilege.
            let label = descriptor("S:(ML;;NW;;;ME)")?;
            let mut present = 0;
            let mut defaulted = 0;
            let mut sacl = null_mut();
            check("private-desktop-integrity", unsafe {
                GetSecurityDescriptorSacl(label.0, &mut present, &mut sacl, &mut defaulted)
            })?;
            if present == 0 || sacl.is_null() {
                return Err(step("private-desktop-integrity", "missing medium label"));
            }
            win32_code("private-desktop-integrity", unsafe {
                SetSecurityInfo(
                    object,
                    SE_WINDOW_OBJECT,
                    LABEL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null(),
                    sacl,
                )
            })?;
        }
        if station_visible(result.station)? {
            return Err(step(
                "private-desktop",
                "window station must be non-interactive",
            ));
        }
        Ok(result)
    }
    fn close(&mut self) -> io::Result<()> {
        if !self.desktop.is_null() {
            check("CloseDesktop", unsafe { CloseDesktop(self.desktop) })?;
            self.desktop = null_mut();
        }
        if !self.station.is_null() {
            check("CloseWindowStation", unsafe {
                CloseWindowStation(self.station)
            })?;
            self.station = null_mut();
        }
        Ok(())
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("{error}");
        }
    }
}
fn station_visible(station: HWINSTA) -> io::Result<bool> {
    #[repr(C)]
    struct Flags {
        inherit: i32,
        reserved: i32,
        flags: u32,
    }
    let mut flags: Flags = unsafe { zeroed() };
    check("window-station-flags", unsafe {
        GetUserObjectInformationW(
            station,
            UOI_FLAGS,
            (&raw mut flags).cast(),
            size_of::<Flags>() as u32,
            null_mut(),
        )
    })?;
    Ok(flags.flags & WSF_VISIBLE as u32 != 0)
}
fn object_name(object: HANDLE) -> io::Result<String> {
    let mut length = 0;
    unsafe {
        GetUserObjectInformationW(object, UOI_NAME, null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(step("user-object-name", io::Error::last_os_error()));
    }
    let mut text = vec![0u16; (length as usize).div_ceil(2)];
    check("user-object-name", unsafe {
        GetUserObjectInformationW(
            object,
            UOI_NAME,
            text.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    })?;
    unsafe { read_wide(text.as_ptr()) }
}

pub(super) struct Session {
    grant: StagingGrant,
    desktop: Desktop,
    restricted: Option<Handle>,
    _diagnostic_default_dacl: Option<Local>,
    profile: Profile,
    pub(super) environment: Vec<u16>,
    pub(super) info: Value,
}
impl Session {
    pub(super) fn open(base: Handle, plan: &Plan) -> io::Result<Self> {
        let (user_sid, logon_sid) = identity(base.0)?;
        let profile = Profile::load(base)?;
        let mut sid = null_mut();
        check("logon-sid", unsafe {
            ConvertStringSidToSidW(wide(&logon_sid).as_ptr(), &mut sid)
        })?;
        let sid = Local(sid);
        let mut everyone = null_mut();
        if plan.diagnostic_policy == DiagnosticPolicy::Everyone {
            check("diagnostic-everyone-sid", unsafe {
                ConvertStringSidToSidW(wide("S-1-1-0").as_ptr(), &mut everyone)
            })?;
        }
        let everyone = Local(everyone);
        let mut restricted_code = null_mut();
        if plan.diagnostic_policy.adds_restricted_code() {
            check("diagnostic-restricted-code-sid", unsafe {
                ConvertStringSidToSidW(wide("S-1-5-12").as_ptr(), &mut restricted_code)
            })?;
        }
        let restricted_code = Local(restricted_code);
        let restricted = if plan.restricted {
            let mut restrictions = vec![SID_AND_ATTRIBUTES {
                Sid: sid.0,
                Attributes: 0,
            }];
            if !everyone.0.is_null() {
                restrictions.push(SID_AND_ATTRIBUTES {
                    Sid: everyone.0,
                    Attributes: 0,
                });
            }
            if !restricted_code.0.is_null() {
                restrictions.push(SID_AND_ATTRIBUTES {
                    Sid: restricted_code.0,
                    Attributes: 0,
                });
            }
            let mut result = null_mut();
            // Unlike the User SID, this logon SID has no write grants on the
            // profile or shared host temp. Only the staging grant below enables it.
            check("CreateRestrictedToken", unsafe {
                CreateRestrictedToken(
                    profile.token.0,
                    FETCH_TOKEN_FLAGS,
                    0,
                    null(),
                    0,
                    null(),
                    restrictions.len() as u32,
                    restrictions.as_ptr(),
                    &mut result,
                )
            })?;
            Some(Handle(result))
        } else {
            None
        };
        let actual_token = restricted.as_ref().map_or(profile.token.0, |token| token.0);
        let default_dacl_before = token_default_dacl(actual_token)?;
        let diagnostic_default_dacl = if plan.diagnostic_policy.changes_default_dacl() {
            Some(set_diagnostic_default_dacl(
                actual_token,
                &user_sid,
                &logon_sid,
            )?)
        } else {
            None
        };
        let desktop = Desktop::new(
            &logon_sid,
            plan.diagnostic_policy == DiagnosticPolicy::ExplicitDesktopWrites,
        )?;
        let temp = plan.cwd.join("fetch-temp");
        no_reparse(&plan.root, &temp)?;
        let grant = StagingGrant::new(&temp, sid.0)?;
        let mut env = profile.environment()?;
        // Keep Windows/profile essentials, not account-level Cargo/Git tokens,
        // rewrites or credential-agent settings. Trusted fetch variables follow.
        env.retain(|key, _| {
            matches!(
                key.as_str(),
                "SYSTEMROOT"
                    | "SYSTEMDRIVE"
                    | "WINDIR"
                    | "COMSPEC"
                    | "OS"
                    | "PATH"
                    | "PATHEXT"
                    | "PROGRAMFILES"
                    | "PROGRAMFILES(X86)"
                    | "PROGRAMW6432"
                    | "COMMONPROGRAMFILES"
                    | "COMMONPROGRAMFILES(X86)"
                    | "COMMONPROGRAMW6432"
                    | "ALLUSERSPROFILE"
                    | "PUBLIC"
                    | "USERPROFILE"
                    | "USERNAME"
                    | "USERDOMAIN"
                    | "USERDNSDOMAIN"
                    | "HOMEDRIVE"
                    | "HOMEPATH"
                    | "APPDATA"
                    | "LOCALAPPDATA"
                    | "TEMP"
                    | "TMP"
                    | "NUMBER_OF_PROCESSORS"
                    | "PROCESSOR_ARCHITECTURE"
                    | "PROCESSOR_IDENTIFIER"
            )
        });
        for key in ["USERPROFILE", "APPDATA", "LOCALAPPDATA"] {
            if env.get(key).is_none_or(String::is_empty) {
                return Err(step("CreateEnvironmentBlock", format!("missing {key}")));
            }
        }
        for (key, value) in &plan.env {
            let key = key.to_uppercase();
            if let Some(value) = value {
                env.insert(key, value.clone());
            } else {
                env.remove(&key);
            }
        }
        if env.get("USERPROFILE").is_none_or(String::is_empty) {
            return Err(step(
                "fetch-environment",
                "explicit variables removed USERPROFILE",
            ));
        }
        for (key, path) in [
            ("TEMP", temp.clone()),
            ("TMP", temp.clone()),
            ("APPDATA", temp.join("app-data/roaming")),
            ("LOCALAPPDATA", temp.join("app-data/local")),
        ] {
            fs::create_dir_all(&path)?;
            env.insert(key.into(), path.to_string_lossy().into_owned());
        }
        env.insert("MOTOR_PREP_DESKTOP".into(), desktop.name.clone());
        env.insert(
            "MOTOR_PREP_WRITE_ROOT".into(),
            temp.to_string_lossy().into_owned(),
        );
        env.insert("RUSTC_WRAPPER".into(), String::new());
        env.insert("CARGO_BUILD_RUSTC_WRAPPER".into(), String::new());
        let actual_token = restricted.as_ref().map_or(profile.token.0, |token| token.0);
        let info = json!({"user_sid":user_sid, "logon_sid":logon_sid, "restricting_sid":if plan.restricted { Some(logon_sid.clone()) } else { None },
            "token_default_dacl_before":default_dacl_before,
            "token":token_details(actual_token, if plan.restricted { FETCH_TOKEN_FLAGS } else { 0 })?,
            "window_station_readback":{"name":object_name(desktop.station)?, "dacl":object_dacl(desktop.station)?},
            "desktop_readback":{"name":object_name(desktop.desktop)?, "dacl":object_dacl(desktop.desktop)?},
            "diagnostic_policy":plan.diagnostic_policy,
            "lp_desktop":desktop.name, "window_station_visible":false, "desktop_heap_kib":4096, "desktop_integrity":"medium",
            "desktop_dacl":format!("D:P(A;;GA;;;{logon_sid})"), "profile_loaded":true, "user_environment_block":true,
            "environment_keys":env.keys().collect::<Vec<_>>(), "temp":env["TEMP"], "tmp":env["TMP"], "userprofile":env["USERPROFILE"]});
        let environment = motor_winagent::environment::encode_block(&env)?;
        Ok(Self {
            grant,
            desktop,
            restricted,
            _diagnostic_default_dacl: diagnostic_default_dacl,
            profile,
            environment,
            info,
        })
    }
    pub(super) fn token(&self) -> HANDLE {
        self.restricted
            .as_ref()
            .map_or(self.profile.token.0, |token| token.0)
    }
    pub(super) fn desktop(&self) -> Vec<u16> {
        wide(&self.desktop.name)
    }
    pub(super) fn finish(&mut self) -> io::Result<()> {
        let mut errors = Vec::new();
        for result in [
            self.grant.close(),
            self.desktop.close(),
            self.profile.close(),
        ] {
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(step("cleanup", errors.join("; ")))
        }
    }
}

pub(super) fn probe_context() -> io::Result<Value> {
    let mut token = null_mut();
    check("probe-token", unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
    })?;
    let token = Handle(token);
    let (user_sid, logon_sid) = identity(token.0)?;
    let restrictions = token_info(token.0, TokenRestrictedSids)?;
    let count = unsafe { *restrictions.as_ptr().cast::<u32>() } as usize;
    let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
    if count > 0 && offset + count * size_of::<SID_AND_ATTRIBUTES>() > restrictions.len() * 8 {
        return Err(step("probe-token", "invalid restricted SID array"));
    }
    let mut restricting_sids = Vec::new();
    for index in 0..count {
        let sid = unsafe {
            (*restrictions
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<SID_AND_ATTRIBUTES>()
                .add(index))
            .Sid
        };
        restricting_sids.push(sid_string(sid)?);
    }
    let station = unsafe { GetProcessWindowStation() };
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let full = format!("{}\\{}", object_name(station)?, object_name(desktop)?);
    let expected =
        std::env::var("MOTOR_PREP_DESKTOP").map_err(|error| step("probe-desktop", error))?;
    if full != expected || station_visible(station)? {
        return Err(step(
            "probe-desktop",
            "child did not receive its private non-interactive desktop",
        ));
    }
    for key in ["USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP"] {
        if std::env::var_os(key).is_none_or(|value| value.is_empty()) {
            return Err(step("probe-environment", format!("missing {key}")));
        }
    }
    Ok(
        json!({"user_sid":user_sid, "logon_sid":logon_sid, "restricting_sids":restricting_sids,
        "lp_desktop":full, "window_station_visible":false, "profile_environment":true}),
    )
}

fn private_handle(path: &Path, directory: bool) -> io::Result<fs::File> {
    let raw = unsafe {
        CreateFileW(
            wide(&path.to_string_lossy()).as_ptr(),
            0x8002_0080,
            FILE_SHARE_READ,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(step("private-plan-open", io::Error::last_os_error()));
    }
    let file = unsafe { fs::File::from_raw_handle(raw as RawHandle) };
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    check("private-plan-identity", unsafe {
        GetFileInformationByHandle(raw, &mut information)
    })?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (!directory && information.nNumberOfLinks != 1)
    {
        return Err(step(
            "private-plan-identity",
            "reparse points and linked plans are forbidden",
        ));
    }
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut security = null_mut();
    win32_code("private-plan-acl", unsafe {
        GetSecurityInfo(
            raw,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut security,
        )
    })?;
    let _security = Local(security);
    let allowed = |sid: &str| matches!(sid, "S-1-5-18" | "S-1-5-32-544");
    if owner.is_null() || !allowed(&sid_string(owner)?) || dacl.is_null() {
        return Err(step(
            "private-plan-acl",
            "plan owner and DACL must be SYSTEM/Administrators only",
        ));
    }
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut pointer = null_mut();
        check("private-plan-acl", unsafe {
            GetAce(dacl, index as u32, &mut pointer)
        })?;
        let ace = unsafe { &*pointer.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Header.AceType != 0
            || !allowed(&sid_string((&raw const ace.SidStart).cast_mut().cast())?)
        {
            return Err(step(
                "private-plan-acl",
                "plan access includes an untrusted principal",
            ));
        }
    }
    Ok(file)
}

pub(super) fn private_directory(root: &Path, path: &Path) -> io::Result<()> {
    no_reparse(
        root,
        path.parent()
            .ok_or_else(|| step("private-plan-directory", "missing state directory"))?,
    )?;
    let security = descriptor("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)")?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide(&path.to_string_lossy()).as_ptr(), &attributes) } == 0
        && unsafe { GetLastError() } != 183
    {
        return Err(step("private-plan-directory", io::Error::last_os_error()));
    }
    drop(private_handle(path, true)?);
    Ok(())
}

pub(super) fn read_private_plan(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let directory = path
        .parent()
        .ok_or_else(|| step("private-plan-path", "missing directory"))?;
    let state = directory
        .parent()
        .ok_or_else(|| step("private-plan-path", "missing state"))?;
    let root = state
        .parent()
        .ok_or_else(|| step("private-plan-path", "missing root"))?;
    if !path.is_absolute()
        || directory.file_name().and_then(|name| name.to_str()) != Some("prep-launch")
        || state.file_name().and_then(|name| name.to_str()) != Some("state")
    {
        return Err(step(
            "private-plan-path",
            "plan must be under runner state/prep-launch",
        ));
    }
    no_reparse(root, path)?;
    let _directory = private_handle(directory, true)?;
    let file = private_handle(path, false)?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(step("private-plan-path", "launch plan exceeds size limit"));
    }
    Ok(bytes)
}

fn token_details(token: HANDLE, requested_flags: u32) -> io::Result<Value> {
    let restrictions = token_info(token, TokenRestrictedSids)?;
    let count = unsafe { *restrictions.as_ptr().cast::<u32>() } as usize;
    let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
    if count > 0 && offset + count * size_of::<SID_AND_ATTRIBUTES>() > restrictions.len() * 8 {
        return Err(step("diagnostic-token", "invalid restricted SID array"));
    }
    let mut sids = Vec::new();
    let mut entries = Vec::new();
    for index in 0..count {
        let group = unsafe {
            &*restrictions
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<SID_AND_ATTRIBUTES>()
                .add(index)
        };
        let sid = sid_string(group.Sid)?;
        entries.push(json!({"sid":sid, "attributes_hex":format!("0x{:08X}", group.Attributes)}));
        sids.push(sid);
    }
    let integrity = token_info(token, TokenIntegrityLevel)?;
    let integrity = sid_string(unsafe {
        (*(integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()))
            .Label
            .Sid
    })?;
    let rid = integrity
        .rsplit('-')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let name = match rid {
        0 => "untrusted",
        4096 => "low",
        8192 => "medium",
        12288 => "high",
        16384 => "system",
        _ => "other",
    };
    let session = token_info(token, TokenSessionId)?;
    Ok(
        json!({"requested_flags":requested_flags, "requested_flags_hex":format!("0x{requested_flags:08X}"),
        "write_restricted_requested":requested_flags & WRITE_RESTRICTED != 0,
        "is_token_restricted":unsafe { IsTokenRestricted(token) } != 0,
        "restricting_sids":sids, "restricted_sid_entries":entries, "default_dacl":token_default_dacl(token)?,
        "integrity_sid":integrity, "integrity_level":name, "session_id":unsafe { *session.as_ptr().cast::<u32>() }}),
    )
}

fn object_dacl(object: HANDLE) -> io::Result<Value> {
    let mut dacl = null_mut();
    let mut security = null_mut();
    win32_code("diagnostic-ui-dacl", unsafe {
        GetSecurityInfo(
            object,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut security,
        )
    })?;
    let _security = Local(security);
    acl_details(dacl)
}

fn acl_details(dacl: *mut ACL) -> io::Result<Value> {
    if dacl.is_null() {
        return Ok(json!({"null_dacl":true}));
    }
    let mut entries = Vec::new();
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut pointer = null_mut();
        check("diagnostic-ui-dacl", unsafe {
            GetAce(dacl, index as u32, &mut pointer)
        })?;
        let ace = unsafe { &*pointer.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Header.AceType <= 1 {
            entries.push(json!({"ace_type":ace.Header.AceType,
                "sid":sid_string((&raw const ace.SidStart).cast_mut().cast())?,
                "rights_hex":format!("0x{:08X}", ace.Mask), "generic_all":ace.Mask & 0x1000_0000 != 0}));
        } else {
            entries.push(json!({"ace_type":ace.Header.AceType, "unsupported_ace":true}));
        }
    }
    Ok(json!({"null_dacl":false, "entries":entries}))
}

fn token_default_dacl(token: HANDLE) -> io::Result<Value> {
    let bytes = token_info(token, TokenDefaultDacl)?;
    let dacl = unsafe { (*(bytes.as_ptr().cast::<TOKEN_DEFAULT_DACL>())).DefaultDacl };
    acl_details(dacl)
}

fn set_diagnostic_default_dacl(
    token: HANDLE,
    user_sid: &str,
    logon_sid: &str,
) -> io::Result<Local> {
    // Only the diagnostic token is changed. Existing filesystem/registry/UI
    // object DACLs are untouched. New objects created using this token use this
    // default when no explicit or inherited DACL overrides it.
    let security = descriptor(&format!(
        "D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)(A;;GA;;;{logon_sid})"
    ))?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = null_mut();
    check("diagnostic-default-dacl", unsafe {
        GetSecurityDescriptorDacl(security.0, &mut present, &mut dacl, &mut defaulted)
    })?;
    if present == 0 || dacl.is_null() {
        return Err(step(
            "diagnostic-default-dacl",
            "missing explicit token default DACL",
        ));
    }
    let info = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
    check("SetTokenInformation-TokenDefaultDacl", unsafe {
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            (&raw const info).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    })?;
    Ok(security)
}
