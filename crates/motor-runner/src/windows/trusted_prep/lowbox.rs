//! Diagnostic-only AppContainer launch cells for the trusted prep fetch. They
//! run four setup variants and a real online `cargo fetch` after the first
//! successful launch. This reports object-directory, profile-hive, LSA, network,
//! TLS and live filesystem/registry write behavior without changing production.
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
        "write_attempts": writes, "appcontainer_profile_paths":profile_paths, "credential_read": credential_read,
        "named_object_local_namespace": named_object, "lsa": lsa, "rng": rng, "network": network,
        "temp": std::env::var_os("TEMP").map(PathBuf::from), "write_root": write_root});
    let text = serde_json::to_string(&proof).map_err(invalid)?;
    println!("{text}");
    if let Err(error) = fs::write(output, &text) {
        eprintln!("lowbox probe output not written to fetch-temp: {error}");
    }
    Ok(())
}

/// SYSTEM side: record the no-launch prechecks, run four launch variants and one
/// real fetch after the first resumed probe, then revoke grants and remove staging
/// unless an unreaped child defers staging removal. This gathers diagnostic evidence only;
/// it never marks the runner as qualified.
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
        "prechecks":prechecks,
        "prechecks_cell":{"cell":0,"name":"pre-checks","launched":false},
        "cell_2_native_apis": ["NtOpenDirectoryObject", "NtCreateDirectoryObjectEx", "NtCreateLowBoxToken", "NtClose"],
        "cell_2_directory_dacl":format!("O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;{package})"),
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
        use launch::DiagnosticPolicy::{
            LowboxLpac, LowboxProfileAppContainer, LowboxProfileLpac, LowboxSelfToken,
        };
        let mut cells = Vec::new();
        let mut fetch_attempted = false;
        for (index, name, policy) in [
            (1, "baseline-attribute-lpac", LowboxLpac),
            (2, "self-made-token-no-profile", LowboxSelfToken),
            (3, "profile-attribute-lpac", LowboxProfileLpac),
            (
                4,
                "profile-attribute-appcontainer",
                LowboxProfileAppContainer,
            ),
        ] {
            let cargo_home = stage.join(format!("fetch-temp/cargo-{index}"));
            let output = stage.join(format!("fetch-temp/lowbox-probe-{index}.json"));
            let outside = PathBuf::from(r"C:\Windows\Temp")
                .join(format!("motor-prep-lowbox-{id}-{index}.txt"));
            let mut command = Command::new(&probe);
            command
                .env_clear()
                .env("SYSTEMROOT", r"C:\Windows")
                .env("CARGO_HOME", &cargo_home)
                .env("RUSTC", &rustc)
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
            if child_started && !fetch_attempted {
                fetch_attempted = true;
                let mut fetch = Command::new(&cargo);
                fetch
                    .env_clear()
                    .env("SYSTEMROOT", r"C:\Windows")
                    .env("CARGO_HOME", &cargo_home)
                    .env("RUSTC", &rustc)
                    .args([
                        "fetch",
                        "--locked",
                        "--target",
                        "x86_64-pc-windows-msvc",
                        "--manifest-path",
                    ])
                    .arg(fixture.join("Cargo.toml"));
                let mut fetch_report =
                    match launch::diagnostic(config, &fetch, &stage, false, policy) {
                        Ok(report) => report,
                        Err(error) => {
                            json!({"phase":"diagnostic-launch-error","error":error.to_string()})
                        }
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
                let mut fetch_evidence = vec![report_path.to_string_lossy().into_owned()];
                if let Some(path) = fetch_report["launch_report"].as_str() {
                    fetch_evidence.push(path.to_owned());
                }
                fetch_report["evidence_artifact_paths"] = json!(fetch_evidence);
                fetch_report["failure_details"] = failure_details(&fetch_report);
                cell["real_cargo_fetch"] = fetch_report.clone();
                report["cell_5"] = json!({"cell":5,"name":"real-cargo-fetch","trigger_cell":index,"report":fetch_report});
            }
            let deferred = cell.get("cleanup_deferred").is_some()
                || cell["real_cargo_fetch"].get("cleanup_deferred").is_some();
            eprintln!(
                "DIAG lowbox-launch cell={index} name={name} created={process_created} started={child_started} exit={}",
                cell["exit_status"].as_str().unwrap_or("not-created")
            );
            cells.push(json!({"cell":index,"name":name,"diagnostic_policy":policy,
                "process_created":process_created,"child_started":child_started,"report":cell}));
            report["cells"] = json!(cells);
            report["real_cargo_fetch_attempted"] = json!(fetch_attempted);
            launch::write_diagnostic_update(&report_path, &report)?;
            if deferred {
                deferred_cleanup = true;
                return Err(invalid(
                    "lowbox cells stopped after an unreaped child; preserve launch reports",
                ));
            }
        }
        if !fetch_attempted {
            let reason = "none of the four launch cells reached a resumed child";
            report["real_cargo_fetch_skipped"] = json!(reason);
            report["cell_5"] = json!({"cell":5,"name":"real-cargo-fetch","launched":false,"skipped_reason":reason});
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
        "DIAG lowbox-launch report={} persistent_grants_reverted_exactly={}; this is not a qualification pass",
        report_path.display(),
        report["persistent_grants_reverted_exactly"]
    );
    result?;
    revoke?;
    cleanup
}
