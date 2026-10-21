//! Diagnostic-only AppContainer launch cells for the trusted prep fetch. They
//! run the trusted probe and a real online `cargo fetch` as lowbox processes
//! (LPAC first, plain AppContainer as the fallback) and report what the
//! read-only descriptor screen could not: named-object directory creation
//! without a profile, LSA and DNS reachability, network isolation, TLS through
//! a real fetch, and live write attempts inside and outside staging.
//!
//! Grants: GrantPrep adds the package SID with the production roles (Stage on
//! the job directory, Read on its inputs, Modify on fetch-temp, Read on the
//! persistent Cargo inputs and toolchain). The staging grants go with the
//! staging directory. The two persistent grants are removed again by
//! RevokePrep -IncludePersistentPackage, and the report compares each
//! persistent DACL before the grant and after the revert.
use super::*;
use serde_json::{json, Value};
use std::io::ErrorKind;

const FIXTURE_MANIFEST: &str = "[package]\nname = \"motor-lowbox-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncfg-if = \"=1.0.4\"\n";
/// cfg-if 1.0.4 has no dependencies; its checksum is the one crates.io serves.
const FIXTURE_LOCK: &str = "version = 4\n\n[[package]]\nname = \"cfg-if\"\nversion = \"1.0.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"9330f8b2ff13f34540b44e946ef35111825727b38d33286ef986142615121801\"\n\n[[package]]\nname = \"motor-lowbox-fixture\"\nversion = \"0.1.0\"\ndependencies = [\n \"cfg-if\",\n]\n";

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
    let mut network = serde_json::Map::new();
    for host in ["index.crates.io", "static.crates.io"] {
        use std::net::{TcpStream, ToSocketAddrs};
        network.insert(host.into(), match (host, 443).to_socket_addrs() {
            Ok(addresses) => {
                let addresses = addresses.collect::<Vec<_>>();
                let connect = addresses.first().map(|address| {
                    match TcpStream::connect_timeout(address, std::time::Duration::from_secs(5)) {
                        Ok(_) => json!({"connected": true, "address": address.to_string()}),
                        Err(error) => json!({"connected": false, "address": address.to_string(), "error": error.to_string()}),
                    }
                });
                json!({"dns": "resolved", "addresses": addresses.iter().map(ToString::to_string).collect::<Vec<_>>(), "tcp_443": connect})
            }
            Err(error) => json!({"dns": "failed", "error": error.to_string()}),
        });
    }
    let proof = json!({"mode": "lowbox", "token": launch::appcontainer_context(),
        "process_context": launch::probe_context().map_err(|error| error.to_string()),
        "write_attempts": writes, "credential_read": credential_read,
        "named_object_local_namespace": named_object, "lsa": lsa, "rng": rng, "network": network,
        "temp": std::env::var_os("TEMP").map(PathBuf::from), "write_root": write_root});
    let text = serde_json::to_string(&proof).map_err(invalid)?;
    println!("{text}");
    if let Err(error) = fs::write(output, &text) {
        eprintln!("lowbox probe output not written to fetch-temp: {error}");
    }
    Ok(())
}

/// SYSTEM side: one fresh staging job, the package-SID grants, four cells, then
/// revocation, an exact before/after comparison of the persistent grants, and
/// staging removal. This gathers diagnostic evidence only; it never marks the
/// runner as qualified.
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
        .join(format!("prep-lowbox-launch-{id}.json"));
    let mut report = json!({"diagnostic_only": true, "production_policy_unchanged": true,
        "package_sid": package, "staging": stage, "fixture_dependency": "cfg-if 1.0.4 from crates.io",
        "grants": {"code_path": "hyperv.ps1 GrantPrep -PackageSid (Grant-RunnerPrepPath, the production prep grant builder)",
            "staging": {"job_directory": "Stage", "fetch-manifests": "Read", "fetch-temp": "Modify (the only package-SID write grant)"},
            "persistent": {"prep-toolchain": "Read", "state/ally/prep-cargo": "Read"},
            "lifetime": "staging grants are removed with the staging directory; this cell reverts the persistent grants (RevokePrep -IncludePersistentPackage). A production design would keep the persistent Read grants, as it keeps the account's own."},
        "persistent_dacl_before": before, "cells": []});
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
        use launch::DiagnosticPolicy::{LowboxAppContainer, LowboxLpac};
        let mut cells = Vec::new();
        for (index, name, program, policy) in [
            (1, "probe-lpac", &probe, LowboxLpac),
            (2, "cargo-fetch-lpac", &cargo, LowboxLpac),
            (3, "probe-appcontainer", &probe, LowboxAppContainer),
            (4, "cargo-fetch-appcontainer", &cargo, LowboxAppContainer),
        ] {
            let cargo_home = stage.join(format!("fetch-temp/cargo-{index}"));
            let mut command = Command::new(program);
            command
                .env_clear()
                .env("SYSTEMROOT", r"C:\Windows")
                .env("CARGO_HOME", &cargo_home)
                .env("RUSTC", &rustc);
            let output = stage.join(format!("fetch-temp/lowbox-probe-{index}.json"));
            if program == &probe {
                let outside = PathBuf::from(r"C:\Windows\Temp")
                    .join(format!("motor-prep-lowbox-{id}-{index}.txt"));
                command
                    .args(["prep-probe", "lowbox"])
                    .arg(root.join("prep-secret.dpapi"))
                    .arg(&outside)
                    .arg(&output);
                outside_files.push(outside);
            } else {
                command
                    .args([
                        "fetch",
                        "--locked",
                        "--target",
                        "x86_64-pc-windows-msvc",
                        "--manifest-path",
                    ])
                    .arg(fixture.join("Cargo.toml"));
            }
            let mut cell = match launch::diagnostic(config, &command, &stage, false, policy) {
                Ok(report) => report,
                Err(error) => {
                    json!({"phase": "diagnostic-launch-error", "error": error.to_string()})
                }
            };
            let deferred = cell.get("cleanup_deferred").is_some();
            let crate_downloaded = fs::read_dir(cargo_home.join("registry/cache"))
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .any(|entry| entry.path().join("cfg-if-1.0.4.crate").is_file())
                })
                .unwrap_or(false);
            if program == &probe {
                cell["probe_proof"] = cell["stdout"]
                    .as_str()
                    .and_then(|text| {
                        text.lines()
                            .find_map(|line| serde_json::from_str::<Value>(line).ok())
                    })
                    .unwrap_or(Value::Null);
            } else {
                cell["crate_downloaded"] = json!(crate_downloaded);
            }
            eprintln!(
                "DIAG lowbox-launch cell={index} name={name} exit={} crate_downloaded={crate_downloaded}",
                cell["exit_status"].as_str().unwrap_or("not-created")
            );
            cells.push(
                json!({"cell": index, "name": name, "diagnostic_policy": policy, "report": cell}),
            );
            report["cells"] = json!(cells);
            launch::write_diagnostic_update(&report_path, &report)?;
            if deferred {
                deferred_cleanup = true;
                return Err(invalid(
                    "lowbox cells stopped after an unreaped child; preserve launch reports",
                ));
            }
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
    for outside in &outside_files {
        if outside.exists() {
            let _ = fs::remove_file(outside);
        }
    }
    let cleanup = if deferred_cleanup {
        Ok(())
    } else {
        remove_staging(config, &stage)
    };
    report["staging_removed"] = json!(cleanup.is_ok() && !stage.exists());
    if let Err(error) = &result {
        report["error"] = json!(error.to_string());
    }
    launch::write_diagnostic_update(&report_path, &report)?;
    println!(
        "DIAG lowbox-launch report={} persistent_grants_reverted_exactly={}; this is not a qualification pass",
        report_path.display(),
        report["persistent_grants_reverted_exactly"]
    );
    result?;
    revoke?;
    cleanup
}
