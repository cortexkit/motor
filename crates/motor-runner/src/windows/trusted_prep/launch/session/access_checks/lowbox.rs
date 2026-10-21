//! Read-only AppContainer (lowbox) screen for the prep fetch. A lowbox token is
//! built in memory from the prep account's batch token and only ever handed to
//! AccessCheck: no process is launched, and no ACL or system setting changes.
//!
//! Less Privileged AppContainer (LPAC) mode is modelled rather than launched:
//! an LPAC process ignores ACEs for ALL APPLICATION PACKAGES (S-1-15-2-1), so
//! its decisions are the plain AppContainer decisions on descriptors with those
//! ACEs removed in memory.
use super::*;
use crate::windows::sddl_model::Sddl;

const PACKAGE_NAME: &str = "MotorRunner.Prep";
/// Well-known capability SID for outbound internet access.
const INTERNET_CLIENT: &str = "S-1-15-3-1";
/// FILE_GENERIC_READ | FILE_EXECUTE: enough to load an image or list a directory.
const IMAGE_READ: u32 = 0x0012_00A9;
/// KEY_READ.
const KEY_READ_MASK: u32 = 0x0002_0019;
/// Read-only device open, beside the read/write open bcrypt was seen to use.
const DEVICE_READ_ONLY: u32 = 0x0010_0001;
const SE_GROUP_ENABLED_ATTRIBUTE: u32 = 4;
const SE_PRIVILEGE_ENABLED_ATTRIBUTE: u32 = 2;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateLowBoxToken(
        token: *mut HANDLE,
        existing: HANDLE,
        access: u32,
        attributes: *mut Attributes,
        package: PSID,
        capability_count: u32,
        capabilities: *mut SID_AND_ATTRIBUTES,
        handle_count: u32,
        handles: *mut HANDLE,
    ) -> i32;
    fn NtOpenKey(handle: *mut HANDLE, access: u32, attributes: *mut Attributes) -> i32;
}
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

fn registry_mapping() -> GENERIC_MAPPING {
    GENERIC_MAPPING {
        GenericRead: 0x0002_0019,
        GenericWrite: 0x0002_0006,
        GenericExecute: 0x0002_0019,
        GenericAll: 0x000F_003F,
    }
}

/// DeriveAppContainerSidFromAppContainerName's SID must be released with FreeSid.
struct DerivedSid(PSID);
impl Drop for DerivedSid {
    fn drop(&mut self) {
        unsafe {
            FreeSid(self.0);
        }
    }
}
fn package_sid() -> io::Result<(DerivedSid, String)> {
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
fn capability_sids(name: &str) -> io::Result<Vec<String>> {
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
fn lowbox_token(base: HANDLE, package: PSID, capabilities: &[String]) -> io::Result<Handle> {
    // Same privilege filtering as production, but no restricting SIDs. With a
    // write-restricted token, opening KsecDD for write also has to pass a
    // second check against the restricting SIDs alone, and the logon SID alone
    // fails it; the AppContainer check replaces that boundary here.
    let mut filtered = null_mut();
    check("lowbox-base-token", unsafe {
        CreateRestrictedToken(
            base,
            DISABLE_MAX_PRIVILEGE,
            0,
            null(),
            0,
            null(),
            0,
            null(),
            &mut filtered,
        )
    })?;
    let filtered = Handle(filtered);
    let owned = capabilities
        .iter()
        .map(|text| {
            let mut sid = null_mut();
            check("capability-sid", unsafe {
                ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid)
            })?;
            Ok(Local(sid))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut entries = owned
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.0,
            Attributes: SE_GROUP_ENABLED_ATTRIBUTE,
        })
        .collect::<Vec<_>>();
    let mut attributes = Attributes {
        length: size_of::<Attributes>() as u32,
        root: null_mut(),
        name: null_mut(),
        attributes: 0,
        security: null_mut(),
        qos: null_mut(),
    };
    let mut token = null_mut();
    let status = unsafe {
        NtCreateLowBoxToken(
            &mut token,
            filtered.0,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut attributes,
            package,
            entries.len() as u32,
            if entries.is_empty() {
                null_mut()
            } else {
                entries.as_mut_ptr()
            },
            0,
            null_mut(),
        )
    };
    if status < 0 {
        return Err(step(
            "create-lowbox-token",
            format!("NTSTATUS 0x{:08X}", status as u32),
        ));
    }
    Ok(Handle(token))
}

fn token_facts(token: HANDLE) -> Value {
    let app_container =
        token_info(token, TokenIsAppContainer).map(|bytes| bytes[0] & 0xFFFF_FFFF != 0);
    let mut luid: LUID = unsafe { zeroed() };
    let change_notify = if unsafe {
        LookupPrivilegeValueW(null(), wide("SeChangeNotifyPrivilege").as_ptr(), &mut luid)
    } == 0
    {
        json!({"error": io::Error::last_os_error().to_string()})
    } else {
        match token_info(token, TokenPrivileges) {
            Ok(bytes) => {
                let header = unsafe { &*bytes.as_ptr().cast::<TOKEN_PRIVILEGES>() };
                let entries = unsafe {
                    std::slice::from_raw_parts(
                        header.Privileges.as_ptr(),
                        header.PrivilegeCount as usize,
                    )
                };
                let found = entries.iter().find(|entry| {
                    entry.Luid.LowPart == luid.LowPart && entry.Luid.HighPart == luid.HighPart
                });
                json!({"present": found.is_some(),
                    "enabled": found.is_some_and(|entry| entry.Attributes & SE_PRIVILEGE_ENABLED_ATTRIBUTE != 0)})
            }
            Err(error) => json!({"error": error.to_string()}),
        }
    };
    json!({"is_app_container": app_container.as_ref().ok(),
        "is_app_container_error": app_container.as_ref().err().map(ToString::to_string),
        "change_notify_privilege": change_notify,
        "details": token_details(token, DISABLE_MAX_PRIVILEGE).unwrap_or_else(|error| json!({"error": error.to_string()}))})
}

/// Builds an in-memory descriptor: optionally the LPAC view, plus hypothetical
/// grants and label. None means the SDDL could not be modelled.
fn model(sddl: &str, lpac: bool, grants: &[String], label: Option<&str>) -> Option<Local> {
    let parsed = Sddl::parse(sddl)?;
    let parsed = if lpac {
        parsed.without_all_application_packages()
    } else {
        parsed
    };
    descriptor(&parsed.with_grants(grants, label).render()).ok()
}
fn decide(
    sd: Option<PSECURITY_DESCRIPTOR>,
    token: HANDLE,
    mask: u32,
    generic: &GENERIC_MAPPING,
) -> Value {
    match sd {
        Some(sd) => access_mapped(sd, token, mask, generic),
        None => json!({"decision": null, "error": "descriptor unavailable or not modelled"}),
    }
}
fn registry_descriptor(path: &str) -> io::Result<Descriptor> {
    let mut name = wide(path);
    let mut unicode = Unicode {
        length: ((name.len() - 1) * 2) as u16,
        maximum: (name.len() * 2) as u16,
        buffer: name.as_mut_ptr(),
    };
    let mut attributes = Attributes {
        length: size_of::<Attributes>() as u32,
        root: null_mut(),
        name: &mut unicode,
        attributes: 0x40,
        security: null_mut(),
        qos: null_mut(),
    };
    let mut handle = null_mut();
    let status = unsafe { NtOpenKey(&mut handle, READ_CONTROL, &mut attributes) };
    if status < 0 {
        return Err(step(
            "open-registry-descriptor",
            format!("{path}: NTSTATUS 0x{:08X}", status as u32),
        ));
    }
    let handle = Handle(handle);
    Descriptor::read(handle.0)
}

/// Something the fetch must be able to read. `grantable` targets are owned by
/// the runner, so the screen also models adding a package-SID read grant.
struct ReadTarget {
    group: &'static str,
    name: String,
    path: String,
    registry: bool,
    grantable: bool,
    directory: bool,
    descriptor: Option<Descriptor>,
    sddl: Option<String>,
    error: Option<String>,
}
impl ReadTarget {
    fn file(group: &'static str, grantable: bool, path: PathBuf) -> Self {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let row = Protected::read(&name, path.clone());
        let sddl = row.description["security"]["sddl"]
            .as_str()
            .map(str::to_owned);
        let error = row.description["error"].as_str().map(str::to_owned);
        Self {
            group,
            name,
            path: path.display().to_string(),
            registry: false,
            grantable,
            directory: row.directory,
            descriptor: row.descriptor,
            sddl,
            error,
        }
    }
    fn key(group: &'static str, path: &str) -> Self {
        let (descriptor, sddl, error) = match registry_descriptor(path).and_then(|sd| {
            let description = sd.description()?;
            Ok((sd, description))
        }) {
            Ok((sd, description)) => (
                Some(sd),
                description["sddl"].as_str().map(str::to_owned),
                None,
            ),
            Err(error) => (None, None, Some(error.to_string())),
        };
        Self {
            group,
            name: path.rsplit('\\').next().unwrap_or(path).to_owned(),
            path: path.to_owned(),
            registry: true,
            grantable: false,
            directory: false,
            descriptor,
            sddl,
            error,
        }
    }
}
fn bounded_children(directory: &Path, filter: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
        .map(|entry| entry.path())
        .filter(|path| filter(path))
        .take(50)
        .collect()
}
fn read_targets(vm: &VmConfig, fixture: &Path) -> Vec<ReadTarget> {
    let root = &vm.layout_root;
    let toolchain = root.join("prep-toolchain");
    let cargo_inputs = root.join("state/ally/prep-cargo");
    let mut targets = Vec::new();
    for path in [
        toolchain.clone(),
        toolchain.join("bin"),
        toolchain.join("bin/cargo.exe"),
        toolchain.join("bin/rustc.exe"),
        toolchain.join("lib"),
    ] {
        targets.push(ReadTarget::file("toolchain", true, path));
    }
    for path in bounded_children(&toolchain.join("bin"), |path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
    }) {
        targets.push(ReadTarget::file("toolchain", true, path));
    }
    targets.push(ReadTarget::file(
        "cargo_home_inputs",
        true,
        cargo_inputs.clone(),
    ));
    for path in bounded_children(&cargo_inputs, |_| true) {
        targets.push(ReadTarget::file("cargo_home_inputs", true, path));
    }
    for path in [
        fixture.join("fetch-manifests"),
        fixture.join("fetch-manifests/Cargo.toml"),
    ] {
        targets.push(ReadTarget::file("fixture_manifests", true, path));
    }
    let system = PathBuf::from(r"C:\Windows\System32");
    targets.push(ReadTarget::file(
        "system",
        false,
        PathBuf::from(r"C:\Windows"),
    ));
    targets.push(ReadTarget::file("system", false, system.clone()));
    // Images Cargo, rustc, git and their networking/TLS stack load.
    for name in [
        "ntdll.dll",
        "kernel32.dll",
        "kernelbase.dll",
        "advapi32.dll",
        "bcrypt.dll",
        "bcryptprimitives.dll",
        "crypt32.dll",
        "ncrypt.dll",
        "secur32.dll",
        "sspicli.dll",
        "schannel.dll",
        "ws2_32.dll",
        "mswsock.dll",
        "dnsapi.dll",
        "iphlpapi.dll",
        "winhttp.dll",
        "userenv.dll",
        "ole32.dll",
        "oleaut32.dll",
        "shell32.dll",
        "user32.dll",
        "gdi32.dll",
        "ucrtbase.dll",
        "vcruntime140.dll",
        "msvcp140.dll",
    ] {
        targets.push(ReadTarget::file("system_dlls", false, system.join(name)));
    }
    for path in [
        r"C:\Program Files\Git",
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files\Git\mingw64\bin\git.exe",
    ] {
        targets.push(ReadTarget::file("git", false, PathBuf::from(path)));
    }
    // Machine certificate stores and the TLS/socket configuration schannel and
    // Winsock read. The user store needs the prep profile hive, which is not loaded.
    for path in [
        r"\Registry\Machine\SOFTWARE\Microsoft\SystemCertificates\ROOT",
        r"\Registry\Machine\SOFTWARE\Microsoft\SystemCertificates\AuthRoot",
        r"\Registry\Machine\SOFTWARE\Microsoft\SystemCertificates\CA",
        r"\Registry\Machine\SOFTWARE\Microsoft\SystemCertificates\Disallowed",
        r"\Registry\Machine\SOFTWARE\Policies\Microsoft\SystemCertificates",
        r"\Registry\Machine\SOFTWARE\Microsoft\Cryptography",
        r"\Registry\Machine\SYSTEM\CurrentControlSet\Control\SecurityProviders\SCHANNEL",
        r"\Registry\Machine\SYSTEM\CurrentControlSet\Services\WinSock2\Parameters",
        r"\Registry\Machine\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters",
    ] {
        targets.push(ReadTarget::key("certificate_and_network_registry", path));
    }
    targets
}

type Capabilities = Vec<(String, Vec<String>)>;
fn pointer(model: &Option<Local>) -> Option<PSECURITY_DESCRIPTOR> {
    model.as_ref().map(|local| local.0)
}
fn all_false(decisions: &[(&str, Option<bool>)]) -> bool {
    decisions
        .iter()
        .all(|(_, decision)| *decision == Some(false))
}

/// What a staging-only grant would give, on in-memory copies of the fixture's
/// real descriptors: traverse on the job directory, read on manifests, and
/// write on fetch-temp only when it also carries a Low integrity label
/// (lowbox tokens run at Low integrity, and unlabelled objects are Medium
/// no-write-up).
fn staging_hypothesis(
    stage: &str,
    manifests: &str,
    lpac: bool,
    package: &str,
    user: &str,
    token: HANDLE,
) -> Value {
    let file = mapping();
    let writes = |model: &Option<Local>| {
        WRITE_RIGHTS
            .into_iter()
            .chain([("delete_child", 0x40)])
            .map(|(right, mask)| {
                (
                    right,
                    decide(pointer(model), token, mask, &file)["decision"].as_bool(),
                )
            })
            .collect::<Vec<_>>()
    };
    let traverse = model(stage, lpac, &[format!("(A;;0x20;;;{package})")], None);
    let read = model(
        manifests,
        lpac,
        &[format!("(A;OICI;0x1200a9;;;{package})")],
        None,
    );
    let modify = [
        format!("(A;OICI;0x1301bf;;;{user})"),
        format!("(A;OICI;0x1301bf;;;{package})"),
    ];
    let labelled = model(stage, lpac, &modify, Some("(ML;OICI;NW;;;LW)"));
    let unlabelled = model(stage, lpac, &modify, None);
    let stage_traverse = decide(pointer(&traverse), token, 0x20, &file)["decision"].as_bool();
    let stage_writes = writes(&traverse);
    let manifests_read = decide(pointer(&read), token, IMAGE_READ, &file)["decision"].as_bool();
    let manifests_writes = writes(&read);
    let temp_writes = [
        ("add_file", 2),
        ("add_subdirectory", 4),
        ("delete_child", 0x40),
    ]
    .map(|(right, mask)| {
        (
            right,
            decide(pointer(&labelled), token, mask, &file)["decision"].as_bool(),
        )
    });
    let unlabelled_add_file = decide(pointer(&unlabelled), token, 2, &file)["decision"].as_bool();
    let ok = stage_traverse == Some(true)
        && all_false(&stage_writes)
        && manifests_read == Some(true)
        && all_false(&manifests_writes)
        && temp_writes
            .iter()
            .all(|(_, decision)| *decision == Some(true));
    json!({"kind": "hypothetical in-memory descriptors, not live paths",
        "job_directory_traverse": stage_traverse, "job_directory_writes": stage_writes,
        "manifests_read": manifests_read, "manifests_writes": manifests_writes,
        "fetch_temp_writes_with_low_label": temp_writes,
        "fetch_temp_add_file_without_low_label": unlabelled_add_file,
        "ok": ok})
}

/// `device` is the KsecDD descriptor and its decoded description (with SDDL).
pub(super) fn screen(
    base: HANDLE,
    user: &str,
    vm: &VmConfig,
    fixture: &Path,
    (device, device_description): (&Descriptor, &Value),
    paths: &[Protected],
    inputs_complete: bool,
) -> Value {
    let mut report = json!({"package_name": PACKAGE_NAME,
        "method": "lowbox tokens from the motor-prep batch token (DISABLE_MAX_PRIVILEGE, no restricting SIDs) via NtCreateLowBoxToken, used only for AccessCheck",
        "lpac_model": "LPAC decisions use in-memory descriptors without ALL APPLICATION PACKAGES (S-1-15-2-1) ACEs",
        "not_screened": ["kernel object directory \\Sessions\\N\\AppContainerNamedObjects creation without a profile (launch cell)",
            "RPC/ALPC endpoints such as the DNS client (Dnscache) and LSA, used by name resolution and schannel",
            "Windows Filtering Platform network isolation, enforced at connect time",
            "user certificate store and HKCU (the prep profile hive is not loaded)",
            "package-SID access to the private window station and desktop",
            "LPAC is modelled, not launched"],
        "variants": [], "passing_variants": []});
    let (package, package_text) = match package_sid() {
        Ok(value) => value,
        Err(error) => {
            report["error"] = json!(error.to_string());
            report["recommendation"] =
                json!("Lowbox screen incomplete: the package SID could not be derived");
            return report;
        }
    };
    report["package_sid"] = json!(package_text);
    let targets = read_targets(vm, fixture);
    let sddl_of = |name: &str| {
        paths
            .iter()
            .find(|path| path.name == name)
            .and_then(|path| path.description["security"]["sddl"].as_str())
    };
    let (stage, manifests) = (sddl_of("fixture_stage_root"), sddl_of("fixture_manifests"));
    let device_sddl = device_description["sddl"].as_str();
    let internet = (
        "internetClient".to_owned(),
        vec![INTERNET_CLIENT.to_owned()],
    );
    let crypto = ["registryRead", "lpacCryptoServices"]
        .into_iter()
        .map(|name| capability_sids(name).map(|sids| (name.to_owned(), sids)))
        .collect::<io::Result<Capabilities>>()
        .map(|mut list| {
            list.insert(0, internet.clone());
            list
        });
    let variants: Vec<(&str, bool, io::Result<Capabilities>)> = vec![
        (
            "appcontainer_internetClient",
            false,
            Ok(vec![internet.clone()]),
        ),
        ("lpac_internetClient", true, Ok(vec![internet])),
        (
            "lpac_internetClient_registryRead_lpacCryptoServices",
            true,
            crypto,
        ),
    ];
    let file = mapping();
    let mut rows = Vec::new();
    let mut passing = Vec::new();
    let mut clean_reads = Vec::new();
    let mut roundtrip_ok = false;
    for (name, lpac, capabilities) in variants {
        let built = capabilities.and_then(|capabilities| {
            let sids = capabilities
                .iter()
                .flat_map(|(_, sids)| sids.clone())
                .collect::<Vec<_>>();
            let token = lowbox_token(base, package.0, &sids)?;
            let check = impersonation(token.0)?;
            Ok((capabilities, token, check))
        });
        let (capabilities, token, check) = match built {
            Ok(value) => value,
            Err(error) => {
                rows.push(json!({"name": name, "lpac_modelled": lpac, "error": error.to_string(), "passes": false}));
                continue;
            }
        };
        let facts = token_facts(token.0);
        let check = check.0;
        let model_for = |sddl: Option<&str>| {
            if lpac {
                sddl.and_then(|sddl| model(sddl, true, &[], None))
            } else {
                None
            }
        };
        let device_model = model_for(device_sddl);
        let device_pointer = if lpac {
            pointer(&device_model)
        } else {
            Some(device.pointer())
        };
        let device_read_write = decide(device_pointer, check, DEVICE_ACCESS, &file);
        let device_read_only = decide(device_pointer, check, DEVICE_READ_ONLY, &file);
        if !lpac {
            // The LPAC model rebuilds descriptors from SDDL; it is trusted only
            // if the unmodified SDDL round trip decides exactly like the live
            // descriptor for every protected path and the device.
            let mut mismatches = Vec::new();
            let device_roundtrip = device_sddl.and_then(|sddl| model(sddl, false, &[], None));
            if decide(pointer(&device_roundtrip), check, DEVICE_ACCESS, &file)["decision"]
                != device_read_write["decision"]
            {
                mismatches.push("device".to_owned());
            }
            for path in paths {
                let Some(live) = &path.descriptor else {
                    continue;
                };
                let roundtrip = path.description["security"]["sddl"]
                    .as_str()
                    .and_then(|sddl| model(sddl, false, &[], None));
                for (right, mask) in WRITE_RIGHTS.into_iter().chain([("read", IMAGE_READ)]) {
                    let live_decision = access(live.pointer(), check, mask)["decision"].as_bool();
                    let model_decision =
                        decide(pointer(&roundtrip), check, mask, &file)["decision"].as_bool();
                    if live_decision != model_decision {
                        mismatches.push(format!(
                            "{} {right}: live={live_decision:?} model={model_decision:?}",
                            path.name
                        ));
                    }
                }
            }
            roundtrip_ok = mismatches.is_empty();
            report["model_roundtrip_mismatches"] = json!(mismatches);
        }
        let mut protected_rows = Vec::new();
        let mut decisions = Vec::new();
        let mut granted = Vec::new();
        for path in paths {
            let modelled = model_for(path.description["security"]["sddl"].as_str());
            let sd = if lpac {
                pointer(&modelled)
            } else {
                path.descriptor.as_ref().map(Descriptor::pointer)
            };
            let mut checks = Vec::new();
            if sd.is_none() {
                decisions.push(None);
                granted.push(format!(
                    "{} {}: descriptor unavailable",
                    path.name,
                    path.path.display()
                ));
            }
            for (right, mask) in WRITE_RIGHTS
                .into_iter()
                .chain(path.directory.then_some(("delete_child", 0x40)))
            {
                let result = decide(sd, check, mask, &file);
                let decision = result["decision"].as_bool();
                if sd.is_some() && decision != Some(false) {
                    granted.push(format!(
                        "{} {}: {right}={decision:?}",
                        path.name,
                        path.path.display()
                    ));
                }
                decisions.push(decision);
                checks.push(json!({"right": right, "result": result}));
            }
            protected_rows.push(json!({"name": path.name, "path": path.path, "checks": checks}));
        }
        let mut read_rows = Vec::new();
        let (mut needs, mut insufficient, mut system_denied, mut unavailable) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for target in &targets {
            let generic = if target.registry {
                registry_mapping()
            } else {
                mapping()
            };
            let mask = if target.registry {
                KEY_READ_MASK
            } else {
                IMAGE_READ
            };
            let modelled = model_for(target.sddl.as_deref());
            let sd = if lpac {
                pointer(&modelled)
            } else {
                target.descriptor.as_ref().map(Descriptor::pointer)
            };
            let actual = decide(sd, check, mask, &generic);
            let with_grant = target.grantable.then(|| {
                let flags = if target.directory { "OICI" } else { "" };
                let grants = [
                    format!("(A;{flags};0x1200a9;;;{package_text})"),
                    format!("(A;{flags};0x1200a9;;;{user})"),
                ];
                let granted = target
                    .sddl
                    .as_deref()
                    .and_then(|sddl| model(sddl, lpac, &grants, None));
                decide(pointer(&granted), check, mask, &generic)
            });
            let actual_decision = actual["decision"].as_bool();
            let grant_decision = with_grant
                .as_ref()
                .and_then(|value| value["decision"].as_bool());
            let label = format!("{} {}", target.group, target.path);
            if let Some(error) = &target.error {
                unavailable.push(format!("{label}: {error}"));
            } else if target.grantable {
                if actual_decision != Some(true) {
                    if grant_decision == Some(true) {
                        needs.push(label)
                    } else {
                        insufficient.push(label)
                    }
                }
            } else if actual_decision != Some(true) {
                system_denied.push(label);
            }
            read_rows.push(json!({"group": target.group, "name": target.name, "path": target.path, "registry": target.registry,
                "error": target.error, "actual": actual, "with_package_and_user_read_grant": with_grant}));
        }
        let staging = match (stage, manifests) {
            (Some(stage), Some(manifests)) => {
                staging_hypothesis(stage, manifests, lpac, &package_text, user, check)
            }
            _ => json!({"ok": false, "error": "fixture stage or manifests descriptor unavailable"}),
        };
        let passes = facts["is_app_container"] == true
            && device_read_write["decision"] == true
            && !decisions.is_empty()
            && decisions.iter().all(|decision| *decision == Some(false))
            && staging["ok"] == true
            && insufficient.is_empty()
            && inputs_complete
            && roundtrip_ok;
        if passes {
            passing.push(name.to_owned());
            if system_denied.is_empty() {
                clean_reads.push(name.to_owned());
            }
        }
        rows.push(json!({"name": name, "lpac_modelled": lpac,
            "capabilities": capabilities.iter().map(|(name, sids)| json!({"name": name, "sids": sids})).collect::<Vec<_>>(),
            "token": facts, "device_read_write": device_read_write, "device_read_only": device_read_only,
            "protected_write_granted": granted, "protected_checks": protected_rows,
            "package_grants_needed": needs, "package_grant_insufficient": insufficient,
            "system_reads_denied": system_denied, "unavailable_read_targets": unavailable, "read_targets": read_rows,
            "staging_hypothesis": staging, "passes": passes}));
    }
    report["variants"] = json!(rows);
    report["passing_variants"] = json!(passing);
    report["passing_variants_with_all_system_reads"] = json!(clean_reads);
    let preferred = clean_reads
        .iter()
        .find(|name| name.starts_with("lpac"))
        .or_else(|| clean_reads.first());
    report["recommendation"] = json!(match preferred {
        Some(name) => format!("{name} passes the descriptor screen with every listed system read allowed; build the real-launch cell for it next (package-SID read grants on toolchain and CARGO_HOME inputs, Low-labelled fetch-temp)"),
        None if !passing.is_empty() => "Variants pass the write boundary but some system reads are denied; see system_reads_denied before choosing capabilities".to_owned(),
        None => "No AppContainer variant passed; see device_read_write, protected_write_granted, package_grant_insufficient and staging_hypothesis per variant".to_owned(),
    });
    report
}
