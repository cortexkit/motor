//! AppContainer identity for the prep fetch: the package SID, capability SIDs,
//! and the process attributes that make CreateProcessAsUserW start the child in
//! a lowbox token. Used by the read-only access screen and the diagnostic
//! launch cells; production launches do not use it yet.
use super::*;

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

/// Everything CreateProcessAsUserW needs to start the child as a lowbox
/// process. The SID buffers and the SECURITY_CAPABILITIES block are heap
/// owned here and must outlive the attribute list that points at them.
pub(crate) struct Launch {
    _package: DerivedSid,
    pub(super) package_text: String,
    _capability_sids: Vec<Local>,
    _entries: Vec<SID_AND_ATTRIBUTES>,
    capabilities: Box<SECURITY_CAPABILITIES>,
    lpac_policy: Option<Box<u32>>,
    pub(super) info: Value,
}
impl Launch {
    /// `lpac` selects Less Privileged AppContainer mode, which also needs the
    /// registryRead and lpacCryptoServices capabilities for certificate and
    /// TLS configuration reads.
    pub(super) fn new(lpac: bool) -> io::Result<Self> {
        let (package, package_text) = package_sid()?;
        let mut names = vec![(
            "internetClient".to_owned(),
            vec![INTERNET_CLIENT.to_owned()],
        )];
        if lpac {
            for name in ["registryRead", "lpacCryptoServices"] {
                names.push((name.to_owned(), capability_sids(name)?));
            }
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
        let capabilities = Box::new(SECURITY_CAPABILITIES {
            AppContainerSid: package.0,
            Capabilities: entries.as_mut_ptr(),
            CapabilityCount: entries.len() as u32,
            Reserved: 0,
        });
        let info = json!({"package_name": PACKAGE_NAME, "package_sid": package_text, "lpac": lpac,
            "capabilities": names.iter().map(|(name, sids)| json!({"name": name, "sids": sids})).collect::<Vec<_>>(),
            "profile_created": false});
        Ok(Self {
            _package: package,
            package_text,
            _capability_sids: capability_sids,
            _entries: entries,
            capabilities,
            lpac_policy: lpac.then(|| Box::new(ALL_APPLICATION_PACKAGES_OPT_OUT)),
            info,
        })
    }
    /// Process attributes to add beside the handle list: (attribute, value
    /// pointer, value size).
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
