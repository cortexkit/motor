//! Diagnostic-only AppContainer network matrix for trusted preparation. It uses
//! the self-created NtCreateLowBoxToken/object-directory launch path with Less
//! Privileged AppContainer (LPAC), plain AppContainer, and LPAC plus
//! privateNetworkClientServer tokens. It records name resolution, raw TCP,
//! Schannel TLS, a separate Local Security Authority (LSA) probe, and filesystem-write-check evidence.
//! If a probe child starts, Cargo uses the first token passing DNS/TCP/TLS, or
//! the started token with the most transport progress if none passes; if no child
//! starts, Cargo is skipped.
//!
//! Grants: GrantPrep adds the package SID with the production roles (Stage on
//! the job directory, Read on its inputs, Modify on fetch-temp, Read on the
//! persistent Cargo inputs and toolchain). The staging grants go with the
//! staging directory. RevokePrep -IncludePersistentPackage attempts to remove the
//! two persistent grants, and the report compares each persistent discretionary access
//! control list (DACL) before and after the attempt.
use super::*;
use serde_json::{json, Value};
use std::{
    ffi::c_void,
    io::ErrorKind,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    time::Duration,
};

type HInternet = *mut c_void;
#[link(name = "winhttp", kind = "raw-dylib")]
unsafe extern "system" {
    fn WinHttpOpen(
        user_agent: *const u16,
        access_type: u32,
        proxy_name: *const u16,
        proxy_bypass: *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpConnect(
        session: HInternet,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> HInternet;
    fn WinHttpOpenRequest(
        connection: HInternet,
        verb: *const u16,
        object_name: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpSetTimeouts(
        handle: HInternet,
        resolve_timeout: i32,
        connect_timeout: i32,
        send_timeout: i32,
        receive_timeout: i32,
    ) -> i32;
    fn WinHttpSendRequest(
        request: HInternet,
        headers: *const u16,
        headers_length: u32,
        optional: *const c_void,
        optional_length: u32,
        total_length: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: HInternet, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: HInternet,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_length: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: HInternet) -> i32;
}

struct InternetHandle(HInternet);
impl Drop for InternetHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

const FIXTURE_MANIFEST: &str = "[package]\nname = \"motor-lowbox-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncfg-if = \"=1.0.4\"\n";
/// cfg-if 1.0.4 has no dependencies; its checksum is the one crates.io serves.
const FIXTURE_LOCK: &str = "version = 4\n\n[[package]]\nname = \"cfg-if\"\nversion = \"1.0.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"9330f8b2ff13f34540b44e946ef35111825727b38d33286ef986142615121801\"\n\n[[package]]\nname = \"motor-lowbox-fixture\"\nversion = \"0.1.0\"\ndependencies = [\n \"cfg-if\",\n]\n";

fn failure_details(report: &Value) -> Value {
    let error = report["error"]
        .as_str()
        .or_else(|| report["broker_stderr"].as_str())
        .unwrap_or("");
    let text = error.split("trusted fetch ").nth(1).unwrap_or(error).trim();
    let call = text.split(':').next().filter(|part| !part.is_empty());
    let win32 = text
        .split("Win32 ")
        .nth(1)
        .and_then(|tail| {
            tail.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        })
        .or_else(|| {
            text.split("HRESULT 0x")
                .nth(1)
                .and_then(|tail| {
                    u32::from_str_radix(tail.chars().take(8).collect::<String>().as_str(), 16).ok()
                })
                .filter(|hresult| hresult >> 16 == 0x8007)
                .map(|hresult| hresult & 0xffff)
        })
        .or_else(|| {
            text.split("os error ").nth(1).and_then(|tail| {
                tail.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .ok()
            })
        });
    json!({"call":call,"win32_error":win32,"raw":error})
}

fn probe_leftover_paths(proof: &Value) -> Vec<String> {
    let Some(writes) = proof["write_attempts"].as_object() else {
        return Vec::new();
    };
    writes
        .values()
        .filter_map(|result| {
            let file_remains = result["write"] == "allowed" && result["removed"] != json!(true);
            let registry_key_remains = result["cleanup"]["key_removed"] == json!(false);
            if file_remains || registry_key_remains {
                result["path"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .collect()
}

fn resolve_host_tcp_target() -> (Value, Option<SocketAddr>) {
    match ("index.crates.io", 443).to_socket_addrs() {
        Ok(addresses) => {
            let addresses = addresses.collect::<Vec<_>>();
            let target = addresses
                .iter()
                .find(|address| address.is_ipv4())
                .or_else(|| addresses.first())
                .copied();
            (
                json!({
                    "api":"getaddrinfo",
                    "resolved":!addresses.is_empty(),
                    "addresses":addresses.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "tcp_target":target.map(|address| address.to_string())
                }),
                target,
            )
        }
        Err(error) => (
            json!({"api":"getaddrinfo","resolved":false,"error":error.to_string()}),
            None,
        ),
    }
}

fn schannel_tls_probe() -> Value {
    const WINHTTP_ACCESS_TYPE_NO_PROXY: u32 = 1;
    const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
    const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
    const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

    fn failed(call: &str) -> Value {
        let error = std::io::Error::last_os_error();
        json!({
            "api":"WinHTTP/Schannel",
            "handshake_completed":false,
            "failed_call":call,
            "win32_error":error.raw_os_error(),
            "error":error.to_string()
        })
    }

    let session = unsafe {
        WinHttpOpen(
            wide("MotorRunner lowbox diagnostic").as_ptr(),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            null(),
            null(),
            0,
        )
    };
    if session.is_null() {
        return failed("WinHttpOpen");
    }
    let session = InternetHandle(session);
    if unsafe { WinHttpSetTimeouts(session.0, 5000, 5000, 10000, 10000) } == 0 {
        return failed("WinHttpSetTimeouts");
    }
    let connection = unsafe { WinHttpConnect(session.0, wide("index.crates.io").as_ptr(), 443, 0) };
    if connection.is_null() {
        return failed("WinHttpConnect");
    }
    let connection = InternetHandle(connection);
    let request = unsafe {
        WinHttpOpenRequest(
            connection.0,
            wide("GET").as_ptr(),
            wide("/config.json").as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    if request.is_null() {
        return failed("WinHttpOpenRequest");
    }
    let request = InternetHandle(request);
    // WinHTTP secure requests perform the TLS handshake through Schannel.
    if unsafe { WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) } == 0 {
        return failed("WinHttpSendRequest");
    }
    if unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0 {
        return failed("WinHttpReceiveResponse");
    }
    let mut status_code = 0u32;
    let mut status_length = std::mem::size_of::<u32>() as u32;
    let status_read = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status_code as *mut u32).cast(),
            &mut status_length,
            null_mut(),
        )
    } != 0;
    json!({
        "api":"WinHTTP/Schannel",
        "url":"https://index.crates.io/config.json",
        "tls_provider":"Schannel",
        "handshake_completed":true,
        "http_status":if status_read {Some(status_code)} else {None},
        "status_query_error":if status_read {None} else {Some(std::io::Error::last_os_error().to_string())}
    })
}

fn transport_steps_pass(proof: &Value) -> bool {
    proof["system_resolved_tcp_target"]
        .as_str()
        .is_some_and(|target| !target.is_empty())
        && proof["network"]["dns_getaddrinfo"]["resolved"] == json!(true)
        && proof["network"]["raw_tcp_443"]["connected"] == json!(true)
        && proof["network"]["schannel_tls"]["handshake_completed"] == json!(true)
}

fn transport_progress(proof: &Value) -> u8 {
    if proof["network"]["schannel_tls"]["handshake_completed"] == json!(true) {
        3
    } else if proof["network"]["raw_tcp_443"]["connected"] == json!(true) {
        2
    } else if proof["network"]["dns_getaddrinfo"]["resolved"] == json!(true) {
        1
    } else {
        0
    }
}

struct CargoFetchContext<'a> {
    config: &'a Config,
    cargo: &'a Path,
    rustc: &'a Path,
    fixture: &'a Path,
    stage: &'a Path,
    tcp_target: &'a str,
    report_path: &'a Path,
}

fn run_real_cargo_fetch(
    context: &CargoFetchContext<'_>,
    cargo_home: &Path,
    policy: launch::DiagnosticPolicy,
) -> Value {
    let mut command = Command::new(context.cargo);
    command
        .env_clear()
        .env("SYSTEMROOT", r"C:\Windows")
        .env("CARGO_HOME", cargo_home)
        .env("RUSTC", context.rustc)
        .env("MOTOR_LOWBOX_TCP_TARGET", context.tcp_target)
        .args([
            "fetch",
            "--locked",
            "--target",
            "x86_64-pc-windows-msvc",
            "--manifest-path",
        ])
        .arg(context.fixture.join("Cargo.toml"));
    let mut fetch_report =
        match launch::diagnostic(context.config, &command, context.stage, false, policy) {
            Ok(report) => report,
            Err(error) => json!({"phase":"diagnostic-launch-error","error":error.to_string()}),
        };
    let crate_downloaded = fs::read_dir(cargo_home.join("registry/cache"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("cfg-if-1.0.4.crate").is_file())
        })
        .unwrap_or(false);
    fetch_report["crate_downloaded"] = json!(crate_downloaded);
    fetch_report["cargo_home"] = json!(cargo_home);
    let fetch_artifacts = ["stdout_path", "stderr_path"]
        .iter()
        .filter_map(|key| fetch_report[*key].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    fetch_report["staging_artifacts"] = json!(fetch_artifacts);
    fetch_report["staging_artifacts_retained_until_stage_cleanup"] = json!(true);
    let mut fetch_evidence = vec![context.report_path.to_string_lossy().into_owned()];
    if let Some(path) = fetch_report["launch_report"].as_str() {
        fetch_evidence.push(path.to_owned());
    }
    fetch_report["evidence_artifact_paths"] = json!(fetch_evidence);
    fetch_report["failure_details"] = failure_details(&fetch_report);
    fetch_report
}

/// Child side: every outcome is recorded rather than asserted, so one denial
/// never hides the rest. The proof is printed to stdout (always captured by
/// the broker) and also written to `output` when fetch-temp is writable.
pub(super) fn probe(credential: &Path, outside: &Path, output: &Path) -> io::Result<()> {
    fn attempt(path: &Path) -> Value {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => {
                drop(file);
                json!({"path": path, "write": "allowed", "removed": fs::remove_file(path).is_ok()})
            }
            Err(error) => json!({"path": path,
                "write": if error.kind() == ErrorKind::PermissionDenied { "denied" } else { "error" },
                "error": error.to_string()}),
        }
    }
    let id = Uuid::now_v7();
    let root = credential
        .parent()
        .ok_or_else(|| invalid("lowbox probe credential root missing"))?;
    let write_root = std::env::var_os("MOTOR_PREP_WRITE_ROOT").map(PathBuf::from);
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut writes = serde_json::Map::new();
    writes.insert("windows_temp".into(), attempt(outside));
    let mut directories = vec![
        ("runner_root", root.to_owned()),
        ("runner_state", root.join("state")),
        ("persistent_prep_cache", root.join("state/ally/prep-cargo")),
        ("prep_toolchain_bin", root.join("prep-toolchain/bin")),
        ("service_home", root.join("service-home")),
        ("staging_parent", root.join("staging")),
    ];
    if let Some(temp) = &write_root {
        directories.push(("fetch_temp", temp.clone()));
        if let Some(stage) = temp.parent() {
            directories.push(("job_directory", stage.to_owned()));
            directories.push(("fetch_manifests", stage.join("fetch-manifests")));
        }
    }
    if let Some(profile) = &profile {
        directories.push(("profile", profile.clone()));
        directories.push(("profile_local_temp", profile.join("AppData/Local/Temp")));
    }
    for (name, directory) in directories {
        writes.insert(
            name.into(),
            attempt(&directory.join(format!("motor-lowbox-{id}.txt"))),
        );
    }
    let package_sid = launch::current_appcontainer_package_sid();
    let mut profile_paths = json!({});
    if let Ok(package_sid) = &package_sid {
        match launch::appcontainer_profile_folder(package_sid) {
            Ok(path) => {
                let path = PathBuf::from(path);
                writes.insert(
                    "appcontainer_profile_folder".into(),
                    attempt(&path.join(format!("motor-lowbox-{id}.txt"))),
                );
                profile_paths["folder"] = json!(path);
            }
            Err(error) => {
                writes.insert(
                    "appcontainer_profile_folder".into(),
                    json!({"write":"unavailable","error":error.to_string()}),
                );
            }
        }
        let registry_leaf = format!("MotorRunner.LowboxProbe.{id}");
        let registry = launch::appcontainer_registry_probe(package_sid, &registry_leaf);
        profile_paths["registry"] = json!(registry["path"]);
        writes.insert("appcontainer_registry".into(), registry);
    } else {
        writes.insert(
            "appcontainer_profile_folder".into(),
            json!({"write":"unavailable","error":package_sid.err().map(|error| error.to_string())}),
        );
        writes.insert(
            "appcontainer_registry".into(),
            json!({"write":"unavailable","error":"AppContainer package SID unavailable"}),
        );
    }
    let credential_read = match fs::File::open(credential) {
        Ok(_) => json!("allowed"),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => json!("denied"),
        Err(error) => json!({"error": error.to_string()}),
    };
    let named_object = {
        use windows_sys::Win32::System::Threading::CreateEventW;
        let name = wide(&format!("Local\\motor-lowbox-probe-{id}"));
        let handle = unsafe { CreateEventW(null(), 1, 0, name.as_ptr()) };
        if handle.is_null() {
            json!({"created": false, "error": io::Error::last_os_error().to_string()})
        } else {
            drop(Handle(handle));
            json!({"created": true})
        }
    };
    let lsa = {
        use windows_sys::Win32::Security::Authentication::Identity::{
            LsaConnectUntrusted, LsaDeregisterLogonProcess,
        };
        let mut handle = null_mut();
        let status = unsafe { LsaConnectUntrusted(&mut handle) };
        if status >= 0 {
            unsafe { LsaDeregisterLogonProcess(handle) };
        }
        json!({"lsa_connect_untrusted_ntstatus": format!("0x{:08X}", status as u32), "connected": status >= 0})
    };
    let mut rng = serde_json::Map::new();
    for mode in ["rng-process-prng", "rng-bcrypt-gen-random"] {
        let file = output.with_extension(format!("{mode}.json"));
        let result = launch::rng_probe(mode, &file).map(|()| {
            fs::read(&file)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        });
        rng.insert(
            mode.into(),
            match result {
                Ok(value) => json!({"completed": true, "result": value}),
                Err(error) => json!({"completed": false, "error": error.to_string()}),
            },
        );
    }
    let dns_getaddrinfo = match ("index.crates.io", 443).to_socket_addrs() {
        Ok(addresses) => {
            let addresses = addresses.collect::<Vec<_>>();
            json!({
                "api":"getaddrinfo",
                "resolved":!addresses.is_empty(),
                "addresses":addresses.iter().map(ToString::to_string).collect::<Vec<_>>()
            })
        }
        Err(error) => json!({"api":"getaddrinfo","resolved":false,"error":error.to_string()}),
    };
    let raw_tcp_443 = match std::env::var("MOTOR_LOWBOX_TCP_TARGET")
        .ok()
        .and_then(|text| text.parse::<SocketAddr>().ok())
    {
        Some(address) => match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(_) => {
                json!({"connected":true,"address":address.to_string(),"target_source":"SYSTEM getaddrinfo precheck"})
            }
            Err(error) => {
                json!({"connected":false,"address":address.to_string(),"target_source":"SYSTEM getaddrinfo precheck","error":error.to_string()})
            }
        },
        None => {
            json!({"connected":false,"error":"SYSTEM did not resolve index.crates.io to a TCP target"})
        }
    };
    let network = json!({
        "dns_getaddrinfo":dns_getaddrinfo,
        "raw_tcp_443":raw_tcp_443,
        "schannel_tls":schannel_tls_probe()
    });
    let proof = json!({"mode": "lowbox", "token": launch::appcontainer_context(),
        "process_context": launch::probe_context().map_err(|error| error.to_string()),
        "write_attempts": writes, "appcontainer_profile_paths":profile_paths, "credential_read": credential_read,
        "named_object_local_namespace": named_object, "lsa": lsa, "rng": rng, "network": network,
        "system_resolved_tcp_target":std::env::var("MOTOR_LOWBOX_TCP_TARGET").ok(),
        "temp": std::env::var_os("TEMP").map(PathBuf::from), "write_root": write_root});
    let text = serde_json::to_string(&proof).map_err(invalid)?;
    println!("{text}");
    if let Err(error) = fs::write(output, &text) {
        eprintln!("lowbox probe output not written to fetch-temp: {error}");
    }
    Ok(())
}

/// SYSTEM side: record no-launch prechecks, compare three self-created-token
/// network variants, and, if any probe child starts, run Cargo on the first
/// DNS/TCP/TLS-ready token or the furthest-reaching launched token. LSA is separate.
pub(super) fn entry(config: Config) -> io::Result<()> {
    use fs2::FileExt;
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    launch::require_system()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(io::Error::other)?;
    let status = manager
        .open_service("MotorRunner", ServiceAccess::QUERY_STATUS)
        .and_then(|service| service.query_status())
        .map_err(io::Error::other)?;
    if status.current_state != ServiceState::Stopped {
        return Err(invalid(
            "Stop MotorRunner before the lowbox launch diagnostic",
        ));
    }
    let runner = crate::Runner::open(config)?;
    let config = runner.config();
    let vm = config.windows.as_ref().unwrap();
    let root = &vm.layout_root;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("prep.lock"))?;
    lock.lock_exclusive()?;
    // Record the existing session state without using it as a pass/fail gate.
    let prechecks = launch::lowbox_prechecks(config)?;
    let (host_tcp_target, tcp_target) = resolve_host_tcp_target();
    let tcp_target = tcp_target
        .map(|target| target.to_string())
        .unwrap_or_default();
    let id = Uuid::now_v7();
    let stage = root.join("staging").join(id.to_string());
    fs::create_dir_all(stage.join("fetch-manifests"))?;
    fs::create_dir_all(stage.join("fetch-temp"))?;
    let probe = stage.join("fetch-manifests/probe.exe");
    fs::copy(std::env::current_exe()?, &probe)?;
    let fixture = stage.join("fetch-manifests/lowbox-fixture");
    fs::create_dir_all(fixture.join("src"))?;
    fs::write(fixture.join("Cargo.toml"), FIXTURE_MANIFEST)?;
    fs::write(fixture.join("Cargo.lock"), FIXTURE_LOCK)?;
    fs::write(fixture.join("src/lib.rs"), "")?;
    let cargo = root.join("prep-toolchain/bin/cargo.exe");
    let rustc = root.join("prep-toolchain/bin/rustc.exe");
    let package = launch::appcontainer_package_sid()?;
    let persistent = [
        root.join("prep-toolchain"),
        root.join("state/ally/prep-cargo"),
    ];
    let dacl = |path: &Path| launch::path_dacl_sddl(path).map_err(|error| error.to_string());
    let before = persistent.iter().map(|path| dacl(path)).collect::<Vec<_>>();
    let report_path = root
        .join("evidence")
        .join(format!("prep-lowbox-network-{id}.json"));
    let mut report = json!({"diagnostic_only": true, "production_policy_unchanged": true,
        "prechecks":prechecks,
        "prechecks_cell":{"cell":0,"name":"pre-checks","launched":false},
        "host_resolved_tcp_target":host_tcp_target,
        "self_token_native_apis": ["NtOpenDirectoryObject", "NtCreateDirectoryObjectEx", "NtCreateLowBoxToken", "NtClose"],
        "self_token_directory_dacl":format!("O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;{package})"),
        "lsa_is_observational":true,
        "network_fetch_gate":"getaddrinfo, raw TCP to the SYSTEM-resolved address, and Schannel TLS; LSA is reported separately",
        "package_sid": package, "staging": stage, "fixture_dependency": "cfg-if 1.0.4 from crates.io",
        "grants": {"code_path": "hyperv.ps1 GrantPrep -PackageSid (Grant-RunnerPrepPath, the production prep grant builder)",
            "staging": {"job_directory": "Stage", "fetch-manifests": "Read", "fetch-temp": "Modify (the only package-SID write grant)"},
            "persistent": {"prep-toolchain": "Read", "state/ally/prep-cargo": "Read"},
            "lifetime": "staging grants are removed with the staging directory; this cell reverts the persistent grants (RevokePrep -IncludePersistentPackage). A production design would keep the persistent Read grants, as it keeps the account's own."},
        "persistent_dacl_before": before, "cells": [], "real_cargo_fetch_attempted": false});
    let mut outside_files = Vec::new();
    let mut deferred_cleanup = false;
    let result = (|| {
        super::super::vm::checked(super::super::vm::control(
            config,
            "GrantPrep",
            None,
            &[
                "-Staging".into(),
                stage.to_string_lossy().into(),
                "-PackageSid".into(),
                package.clone(),
            ],
        )?)?;
        report["persistent_dacl_granted"] =
            json!(persistent.iter().map(|path| dacl(path)).collect::<Vec<_>>());
        let cargo_fetch_context = CargoFetchContext {
            config,
            cargo: &cargo,
            rustc: &rustc,
            fixture: &fixture,
            stage: &stage,
            tcp_target: &tcp_target,
            report_path: &report_path,
        };
        use launch::DiagnosticPolicy::{
            LowboxSelfToken, LowboxSelfTokenPlain, LowboxSelfTokenPrivateNetwork,
        };
        let mut cells = Vec::new();
        let cargo_home = stage.join("fetch-temp/cargo-network");
        let mut furthest_candidate: Option<(
            usize,
            &'static str,
            launch::DiagnosticPolicy,
            PathBuf,
            u8,
        )> = None;
        let mut first_transport_success: Option<(
            usize,
            &'static str,
            launch::DiagnosticPolicy,
            PathBuf,
            u8,
        )> = None;
        for (index, name, policy) in [
            (1, "self-token-lpac", LowboxSelfToken),
            (2, "self-token-plain-appcontainer", LowboxSelfTokenPlain),
            (
                3,
                "self-token-lpac-private-network",
                LowboxSelfTokenPrivateNetwork,
            ),
        ] {
            let output = stage.join(format!("fetch-temp/lowbox-network-probe-{index}.json"));
            let outside = PathBuf::from(r"C:\Windows\Temp")
                .join(format!("motor-prep-lowbox-network-{id}-{index}.txt"));
            let mut command = Command::new(&probe);
            command
                .env_clear()
                .env("SYSTEMROOT", r"C:\Windows")
                .env("CARGO_HOME", &cargo_home)
                .env("RUSTC", &rustc)
                .env("MOTOR_LOWBOX_TCP_TARGET", &tcp_target)
                .args(["prep-probe", "lowbox"])
                .arg(root.join("prep-secret.dpapi"))
                .arg(&outside)
                .arg(&output);
            outside_files.push(outside);
            let mut cell = match launch::diagnostic(config, &command, &stage, false, policy) {
                Ok(report) => report,
                Err(error) => json!({"phase":"diagnostic-launch-error","error":error.to_string()}),
            };
            cell["probe_proof"] = cell["stdout"]
                .as_str()
                .and_then(|text| {
                    text.lines()
                        .find_map(|line| serde_json::from_str::<Value>(line).ok())
                })
                .unwrap_or(Value::Null);
            cell["probe_leftover_paths"] = json!(probe_leftover_paths(&cell["probe_proof"]));
            let mut staged_artifacts = vec![
                output.to_string_lossy().into_owned(),
                cargo_home.to_string_lossy().into_owned(),
                output
                    .with_extension("rng-process-prng.json")
                    .to_string_lossy()
                    .into_owned(),
                output
                    .with_extension("rng-bcrypt-gen-random.json")
                    .to_string_lossy()
                    .into_owned(),
            ];
            for key in ["stdout_path", "stderr_path"] {
                if let Some(path) = cell[key].as_str() {
                    staged_artifacts.push(path.to_owned());
                }
            }
            cell["staging_artifacts"] = json!(staged_artifacts);
            cell["staging_artifacts_retained_until_stage_cleanup"] = json!(true);
            let mut evidence_artifacts = vec![report_path.to_string_lossy().into_owned()];
            if let Some(path) = cell["launch_report"].as_str() {
                evidence_artifacts.push(path.to_owned());
            }
            cell["evidence_artifact_paths"] = json!(evidence_artifacts);
            cell["failure_details"] = failure_details(&cell);
            let process_created = cell["process_created"] == json!(true);
            let child_started = cell["child_started"] == json!(true);
            let transport_passed = transport_steps_pass(&cell["probe_proof"]);
            let progress = transport_progress(&cell["probe_proof"]);
            cell["transport_steps_passed"] = json!(transport_passed);
            cell["furthest_transport_step"] = json!(match progress {
                3 => "Schannel TLS",
                2 => "raw TCP",
                1 => "token getaddrinfo",
                _ => "none",
            });
            cell["lsa_observation"] = cell["probe_proof"]["lsa"].clone();
            if child_started {
                let candidate = (index, name, policy, cargo_home.clone(), progress);
                if furthest_candidate
                    .as_ref()
                    .is_none_or(|current| candidate.4 > current.4)
                {
                    furthest_candidate = Some(candidate.clone());
                }
                if transport_passed && first_transport_success.is_none() {
                    first_transport_success = Some(candidate);
                }
            }
            let deferred = cell.get("cleanup_deferred").is_some();
            eprintln!(
                "DIAG lowbox-network cell={index} name={name} created={process_created} started={child_started} transport_passed={transport_passed} lsa_connected={} exit={}",
                cell["lsa_observation"]["connected"],
                cell["exit_status"].as_str().unwrap_or("not-created")
            );
            cells.push(json!({"cell":index,"name":name,"diagnostic_policy":policy,
                "process_created":process_created,"child_started":child_started,"report":cell}));
            report["cells"] = json!(cells);
            launch::write_diagnostic_update(&report_path, &report)?;
            if deferred {
                deferred_cleanup = true;
                return Err(invalid(
                    "lowbox cells stopped after an unreaped child; preserve launch reports",
                ));
            }
        }
        let (selection, selection_reason) = if let Some(candidate) = first_transport_success {
            (
                Some(candidate),
                "first token variant to pass getaddrinfo, raw TCP, and Schannel TLS",
            )
        } else {
            (
                furthest_candidate,
                "no token variant passed all three transport checks; use the furthest probe",
            )
        };
        if let Some((index, name, policy, cargo_home, progress)) = selection {
            let fetch_report = run_real_cargo_fetch(&cargo_fetch_context, &cargo_home, policy);
            let deferred = fetch_report.get("cleanup_deferred").is_some();
            report["real_cargo_fetch_attempted"] = json!(true);
            report["cell_4"] = json!({
                "cell":4,
                "name":"real-cargo-fetch",
                "trigger_cell":index,
                "trigger_variant":name,
                "furthest_transport_progress":progress,
                "furthest_transport_step":match progress {3=>"Schannel TLS",2=>"raw TCP",1=>"token getaddrinfo",_=>"none"},
                "selection_reason":selection_reason,
                "report":fetch_report
            });
            launch::write_diagnostic_update(&report_path, &report)?;
            if deferred {
                deferred_cleanup = true;
                return Err(invalid(
                    "real Cargo fetch stopped with an unreaped child; preserve launch reports",
                ));
            }
        } else {
            let reason = "no token variant resumed a probe child; Cargo could not be launched";
            report["real_cargo_fetch_skipped"] = json!(reason);
            report["cell_4"] = json!({"cell":4,"name":"real-cargo-fetch","launched":false,"skipped_reason":reason});
            report["real_cargo_fetch_attempted"] = json!(false);
        }
        Ok(())
    })();
    let revoke = super::super::vm::control(
        config,
        "RevokePrep",
        None,
        &[
            "-Staging".into(),
            stage.to_string_lossy().into(),
            "-PackageSid".into(),
            package.clone(),
            "-IncludePersistentPackage".into(),
        ],
    )
    .and_then(super::super::vm::checked);
    let after = persistent.iter().map(|path| dacl(path)).collect::<Vec<_>>();
    report["persistent_dacl_after_revert"] = json!(after);
    report["persistent_grants_reverted_exactly"] =
        json!(before.iter().all(Result::is_ok) && before == after);
    report["revoke_error"] = json!(revoke.as_ref().err().map(ToString::to_string));
    let mut outside_leftovers = Vec::new();
    for outside in &outside_files {
        if outside.exists() {
            let _ = fs::remove_file(outside);
        }
        if outside.exists() {
            outside_leftovers.push(outside.to_string_lossy().into_owned());
        }
    }
    let cleanup = if deferred_cleanup {
        Ok(())
    } else {
        remove_staging(config, &stage)
    };
    report["staging_removed"] = json!(cleanup.is_ok() && !stage.exists());
    report["staging_path"] = json!(stage);
    report["staging_leftover_path"] = if stage.exists() {
        json!(stage.to_string_lossy())
    } else {
        Value::Null
    };
    report["outside_file_leftovers"] = json!(outside_leftovers);
    if let Err(error) = &result {
        report["error"] = json!(error.to_string());
    }
    launch::write_diagnostic_update(&report_path, &report)?;
    println!(
        "DIAG lowbox-network report={} persistent_grants_reverted_exactly={}; this is not a qualification pass",
        report_path.display(),
        report["persistent_grants_reverted_exactly"]
    );
    result?;
    revoke?;
    cleanup
}
