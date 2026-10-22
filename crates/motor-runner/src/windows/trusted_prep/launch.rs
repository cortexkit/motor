//! One-shot SYSTEM broker. No service thread changes its window station.
use super::{checked, token, wide, Handle};
use crate::{engine::Config, windows::process_status};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

mod process;
mod rng;
mod session;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DiagnosticPolicy {
    #[default]
    None,
    Everyone,
    ExplicitDesktopWrites,
    DefaultDacl,
    RestrictedCode,
    DefaultDaclRestrictedCode,
    DefaultDaclProbeFirst,
    DefaultDaclCargoTwice,
    DefaultDaclSafeCwdPair,
    DefaultDaclProcessPrng,
    DefaultDaclBCryptRng,
}
impl DiagnosticPolicy {
    pub(super) fn changes_default_dacl(self) -> bool {
        matches!(
            self,
            Self::DefaultDacl
                | Self::DefaultDaclRestrictedCode
                | Self::DefaultDaclProbeFirst
                | Self::DefaultDaclCargoTwice
                | Self::DefaultDaclSafeCwdPair
                | Self::DefaultDaclProcessPrng
                | Self::DefaultDaclBCryptRng
        )
    }
    pub(super) fn adds_restricted_code(self) -> bool {
        matches!(self, Self::RestrictedCode | Self::DefaultDaclRestrictedCode)
    }
    fn checks_writes(self) -> bool {
        self.changes_default_dacl() || self.adds_restricted_code()
    }
    fn rng_mode(self) -> Option<&'static str> {
        match self {
            Self::DefaultDaclProcessPrng => Some("rng-process-prng"),
            Self::DefaultDaclBCryptRng => Some("rng-bcrypt-gen-random"),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    root: PathBuf,
    cwd: PathBuf,
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, Option<String>>,
    restricted: bool,
    report: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
    #[serde(default)]
    diagnostic_policy: DiagnosticPolicy,
    #[serde(default)]
    diagnostic_loader_trace: bool,
    #[serde(default)]
    diagnostic_safe_cwd: bool,
    #[serde(default)]
    diagnostic_native_calls: bool,
}

fn step(name: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("trusted fetch {name}: {error}"))
}
fn check(name: &str, result: i32) -> io::Result<()> {
    checked(result).map_err(|error| {
        step(
            name,
            format!(
                "{error} (Win32 0x{:08X})",
                error.raw_os_error().unwrap_or(0) as u32
            ),
        )
    })
}
fn bounded_text(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(64 * 1024)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
struct PlanFile(PathBuf);
impl Drop for PlanFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn run(
    config: &Config,
    command: &Command,
    cwd: &Path,
    restricted: bool,
) -> io::Result<Value> {
    run_plan(
        config,
        command,
        cwd,
        restricted,
        DiagnosticPolicy::None,
        false,
    )
}

pub(super) fn diagnostic(
    config: &Config,
    command: &Command,
    cwd: &Path,
    restricted: bool,
    policy: DiagnosticPolicy,
) -> io::Result<Value> {
    run_plan(config, command, cwd, restricted, policy, true)
}

fn run_plan(
    config: &Config,
    command: &Command,
    cwd: &Path,
    restricted: bool,
    policy: DiagnosticPolicy,
    retain_failure: bool,
) -> io::Result<Value> {
    let root = &config.windows.as_ref().unwrap().layout_root;
    let id = uuid::Uuid::now_v7();
    let directory = root.join("state/prep-launch");
    session::private_directory(root, &directory)?;
    fs::create_dir_all(cwd.join("fetch-temp"))?;
    let plan = Plan {
        root: root.clone(),
        cwd: cwd.to_owned(),
        program: command
            .get_program()
            .to_str()
            .ok_or_else(|| step("plan", "non-UTF8 program"))?
            .into(),
        args: command
            .get_args()
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| step("plan", "non-UTF8 argument"))
            })
            .collect::<io::Result<_>>()?,
        env: command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect(),
        restricted,
        diagnostic_policy: policy,
        diagnostic_loader_trace: retain_failure,
        diagnostic_safe_cwd: policy.rng_mode().is_some(),
        diagnostic_native_calls: retain_failure
            && matches!(
                policy,
                DiagnosticPolicy::DefaultDaclSafeCwdPair
                    | DiagnosticPolicy::DefaultDaclProcessPrng
                    | DiagnosticPolicy::DefaultDaclBCryptRng
            ),
        report: root.join("evidence").join(format!("prep-launch-{id}.json")),
        stdout: cwd.join("fetch-temp").join(format!("{id}.stdout")),
        stderr: cwd.join("fetch-temp").join(format!("{id}.stderr")),
    };
    let path = directory.join(format!("{id}.json"));
    session::write_private(
        &path,
        &serde_json::to_vec(&plan).map_err(io::Error::other)?,
        true,
    )?;
    let _plan_file = PlanFile(path.clone());
    let mut broker = Command::new(std::env::current_exe()?);
    broker
        .arg("prep-launch")
        .arg(&path)
        .current_dir(root)
        // Keep the trusted service's Windows initialization environment for this
        // SYSTEM helper. The fetch child gets a separate non-inherited user block.
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "");
    let output =
        crate::native_process::output_with_timeout(&mut broker, Duration::from_secs(720), || {
            false
        })?;
    if retain_failure {
        let mut report: Value = fs::read(&plan.report)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| json!({"phase":"broker-failed-before-report"}));
        report["launch_report"] = json!(plan.report);
        report["broker_exit"] = json!(output
            .status
            .code()
            .map(|code| process_status::describe(code as u32)));
        report["broker_stderr"] = json!(String::from_utf8_lossy(&output.stderr));
        return Ok(report);
    }
    if !output.status.success() {
        return Err(step(
            "broker",
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let report: Value =
        serde_json::from_slice(&fs::read(&plan.report)?).map_err(io::Error::other)?;
    if report["phase"] != "complete" || report["exit_code"] != 0 {
        return Err(step("broker-report", "missing successful child completion"));
    }
    Ok(report)
}

pub(super) fn entry(arguments: &[String]) -> io::Result<()> {
    session::require_system()?;
    if arguments.len() != 1 {
        return Err(step(
            "arguments",
            "prep-launch requires one private plan path",
        ));
    }
    let plan: Plan = serde_json::from_slice(&session::read_private_plan(Path::new(&arguments[0]))?)
        .map_err(|error| step("plan", error))?;
    if plan.cwd.parent() != Some(plan.root.join("staging").as_path())
        || plan
            .cwd
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| uuid::Uuid::parse_str(name).ok())
            .is_none()
        || Path::new(&arguments[0]).parent() != Some(plan.root.join("state/prep-launch").as_path())
        || plan.report.parent() != Some(plan.root.join("evidence").as_path())
        || plan.stdout.parent() != Some(plan.cwd.join("fetch-temp").as_path())
        || plan.stderr.parent() != Some(plan.cwd.join("fetch-temp").as_path())
    {
        return Err(step(
            "plan",
            "paths do not belong to the protected runner launch",
        ));
    }
    session::no_reparse(&plan.root, &plan.cwd)?;
    // Diagnostic token controls accept only restricted Cargo `--version` or the
    // fixed, hash-verified RNG probe below; they never authorize arbitrary commands.
    let rng_mode = plan.diagnostic_policy.rng_mode();
    if let Some(mode) = rng_mode {
        if !plan.restricted
            || Path::new(&plan.program) != plan.cwd.join("fetch-manifests/probe.exe")
            || plan.args.len() != 5
            || plan.args[0] != "prep-probe"
            || plan.args[1] != mode
            || Path::new(&plan.args[4]).parent() != Some(plan.cwd.join("fetch-temp").as_path())
        {
            return Err(step(
                "rng-diagnostic-policy",
                "only the fixed trusted RNG probe may use this variant",
            ));
        }
        use sha2::{Digest, Sha256};
        if Sha256::digest(fs::read(&plan.program)?)
            != Sha256::digest(fs::read(std::env::current_exe()?)?)
        {
            return Err(step(
                "rng-diagnostic-policy",
                "probe does not match trusted runner",
            ));
        }
    }
    if rng_mode.is_none()
        && plan.diagnostic_policy != DiagnosticPolicy::None
        && (Path::new(&plan.program) != plan.root.join("prep-toolchain/bin/cargo.exe")
            || plan.args != ["--version"]
            || !plan.restricted)
    {
        return Err(step(
            "diagnostic-policy",
            format!("token diagnostic controls are limited to restricted Cargo --version; actual_args={:?}", plan.args),
        ));
    }
    let mut report = json!({"phase":"session-setup", "program":plan.program, "arguments":plan.args,
        "stdout_path":plan.stdout,"stderr_path":plan.stderr,
        "restricted":plan.restricted, "cwd":plan.cwd, "profile_loaded":false,
        "inherit_handles":"explicit stdin/stdout/stderr allow-list only", "active_process_limit":32,
        "creation_flags":format!("0x{:08X}", process::CREATION_FLAGS | if plan.diagnostic_loader_trace { windows_sys::Win32::System::Threading::DEBUG_ONLY_THIS_PROCESS } else { 0 }),
        "diagnostic_loader_trace":plan.diagnostic_loader_trace});
    let save = |report: &Value| -> io::Result<()> {
        session::write_private(
            &plan.report,
            &serde_json::to_vec_pretty(report).map_err(io::Error::other)?,
            false,
        )
    };
    save(&report)?;
    let result = (|| {
        let base = token(&plan.root).map_err(|error| step("batch-logon", error))?;
        let mut context = session::Session::open(base, &plan)?;
        report["launch"] = context.info.clone();
        report["profile_loaded"] = json!(true);
        report["phase"] = json!("create-process");
        save(&report)?;
        if plan.diagnostic_policy == DiagnosticPolicy::DefaultDaclProbeFirst {
            let mut preflight_plan = plan.clone();
            preflight_plan.stdout = plan.stdout.with_extension("preflight.stdout");
            let mut released = true;
            let preflight = diagnostic_write_probe(&preflight_plan, &context, &mut released);
            if !released {
                std::mem::forget(context);
                report["cleanup_deferred"] = json!("unreaped preflight probe");
                return Err(step("preflight-probe", "child not reaped"));
            }
            report["preflight_probe"] = match preflight {
                Ok(proof) => proof,
                Err(error) => json!({"error":error.to_string()}),
            };
            save(&report)?;
        }
        let mut released = true;
        let mut process_created = false;
        let mut child_started = false;
        let child = process::run(
            &plan,
            &context,
            &mut released,
            &mut process_created,
            &mut child_started,
        );
        report["process_created"] = json!(process_created);
        report["child_started"] = json!(child_started);
        save(&report)?;
        if !released {
            // Never unload a hive or close UI objects while a child is alive.
            // Broker exit and the outer kill-on-close job quarantine this
            // exceptional failure; no cache results are published.
            std::mem::forget(context);
            report["cleanup_deferred"] =
                json!("unreaped child; retained until broker/outer job exit");
            return Err(step(
                "unreaped-child",
                child
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_default(),
            ));
        }
        let code = match child {
            Ok(code) => code,
            Err(error) => {
                let cleanup = context.finish();
                report["session_cleanup"] = context.cleanup_report().clone();
                if let Err(cleanup_error) = cleanup {
                    report["cleanup_error"] = json!(cleanup_error.to_string());
                }
                save(&report)?;
                return Err(error);
            }
        };
        report["exit_code"] = json!(code);
        report["exit_status"] = json!(process_status::describe(code));
        report["stdout"] = json!(bounded_text(&plan.stdout)?);
        report["stderr"] = json!(bounded_text(&plan.stderr)?);
        if rng_mode.is_some() {
            report["rng_probe"] = json!(fs::read(&plan.args[4])
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()));
        }
        if plan.diagnostic_loader_trace {
            report["process_diagnostics"] = diagnostic_process_report(&plan);
        }
        if plan.diagnostic_policy == DiagnosticPolicy::DefaultDaclCargoTwice {
            let mut retry = plan.clone();
            retry.stdout = plan.stdout.with_extension("retry.stdout");
            retry.stderr = plan.stderr.with_extension("retry.stderr");
            let mut released = true;
            let mut retry_created = false;
            let mut retry_started = false;
            let outcome = process::run(
                &retry,
                &context,
                &mut released,
                &mut retry_created,
                &mut retry_started,
            );
            if !released {
                std::mem::forget(context);
                report["cleanup_deferred"] = json!("unreaped second Cargo");
                return Err(step("retry-cargo", "child not reaped"));
            }
            report["second_cargo"] = match outcome {
                Ok(exit) => {
                    json!({"exit_code":exit,"exit_status":process_status::describe(exit),"process_diagnostics":diagnostic_process_report(&retry),"stdout":bounded_text(&retry.stdout)?,"stderr":bounded_text(&retry.stderr)?})
                }
                Err(error) => json!({"error":error.to_string()}),
            };
        }
        if plan.diagnostic_policy == DiagnosticPolicy::DefaultDaclSafeCwdPair {
            let mut safe = plan.clone();
            safe.diagnostic_safe_cwd = true;
            safe.stdout = plan.stdout.with_extension("safe-cwd.stdout");
            safe.stderr = plan.stderr.with_extension("safe-cwd.stderr");
            let mut released = true;
            let mut safe_created = false;
            let mut safe_started = false;
            let outcome = process::run(
                &safe,
                &context,
                &mut released,
                &mut safe_created,
                &mut safe_started,
            );
            if !released {
                std::mem::forget(context);
                report["cleanup_deferred"] = json!("unreaped safe-cwd Cargo");
                return Err(step("safe-cwd", "child not reaped"));
            }
            report["safe_cwd_cargo"] = match outcome {
                Ok(exit) => {
                    json!({"same_token_as_primary":true,"exit_code":exit,"exit_status":process_status::describe(exit),"process_diagnostics":diagnostic_process_report(&safe),"stdout":bounded_text(&safe.stdout)?,"stderr":bounded_text(&safe.stderr)?})
                }
                Err(error) => json!({"error":error.to_string()}),
            };
            let mut rng_calls = Vec::new();
            for mode in ["rng-process-prng", "rng-bcrypt-gen-random"] {
                let mut released = true;
                let result = diagnostic_rng_call(&plan, &context, &mut released, mode);
                if !released {
                    std::mem::forget(context);
                    report["cleanup_deferred"] = json!("unreaped same-token RNG probe");
                    return Err(step("same-token-rng", "child not reaped"));
                }
                rng_calls.push(match result {
                    Ok(value) => value,
                    Err(error) => json!({"mode":mode,"error":error.to_string()}),
                });
            }
            report["safe_cwd_rng_comparisons"] = json!(rng_calls);
        }
        if plan.diagnostic_policy.checks_writes() {
            // Reuse the exact created token, profile and desktop: relogging the
            // account here would invalidate the write-boundary comparison.
            let mut probe_released = true;
            let write_probe = diagnostic_write_probe(&plan, &context, &mut probe_released);
            if !probe_released {
                std::mem::forget(context);
                report["cleanup_deferred"] = json!("unreaped diagnostic write probe");
                return Err(step(
                    "unreaped-write-probe",
                    "preserve matrix and launch reports",
                ));
            }
            report["write_probe"] = match write_probe {
                Ok(proof) => proof,
                Err(error) => json!({"error":error.to_string()}),
            };
        }
        report["phase"] = json!("cleanup");
        save(&report)?;
        let cleanup = context.finish();
        report["session_cleanup"] = context.cleanup_report().clone();
        if code != 0 {
            if let Err(error) = cleanup {
                report["cleanup_error"] = json!(error.to_string());
            }
            save(&report)?;
            return Err(step(
                "child-exit",
                format!(
                    "Windows trusted cargo fetch exited {}; diagnostics: {}; token={} desktop={}",
                    process_status::describe(code),
                    report["stderr"]
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                        .unwrap_or("<no child stderr>"),
                    report["launch"]["token"],
                    report["launch"]["lp_desktop"]
                ),
            ));
        }
        if let Err(error) = cleanup {
            report["cleanup_error"] = json!(error.to_string());
            save(&report)?;
            return Err(error);
        }
        report["phase"] = json!("complete");
        Ok(())
    })();
    if let Err(error) = &result {
        report["phase"] = json!("failed");
        report["error"] = json!(error.to_string());
    }
    if let Err(error) = save(&report) {
        if result.is_ok() {
            return Err(error);
        }
        eprintln!("trusted fetch report write failed: {error}");
    }
    result
}

pub(super) fn probe_context() -> io::Result<Value> {
    session::probe_context()
}
pub(super) fn check_path(root: &Path, path: &Path) -> io::Result<()> {
    session::no_reparse(root, path)
}
pub(super) fn check_target(root: &Path, path: &Path) -> io::Result<()> {
    let mut existing = path;
    while fs::symlink_metadata(existing).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        existing = existing
            .parent()
            .ok_or_else(|| step("cache-target", "missing ancestor"))?;
    }
    session::no_reparse(root, existing)
}
pub(super) fn write_diagnostic_update(path: &Path, value: &Value) -> io::Result<()> {
    session::write_private(
        path,
        &serde_json::to_vec_pretty(value).map_err(io::Error::other)?,
        false,
    )
}
pub(super) fn require_system() -> io::Result<()> {
    session::require_system()
}
pub(super) fn access_check_entry(arguments: &[String]) -> io::Result<()> {
    session::require_system()?;
    if arguments.len() != 3 {
        return Err(step(
            "access-check-arguments",
            "requires root, owned fixture path and fresh report path",
        ));
    }
    let root = PathBuf::from(&arguments[0]);
    let fixture = PathBuf::from(&arguments[1]);
    let report = PathBuf::from(&arguments[2]);
    if root != Path::new(r"C:\ProgramData\MotorRunner")
        || fixture.parent() != Some(root.join("staging").as_path())
        || fixture
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| uuid::Uuid::parse_str(name).ok())
            .is_none()
        || report.file_name().and_then(|name| name.to_str()) != Some("ksecdd-access-matrix.json")
        || report.parent().and_then(Path::parent) != Some(Path::new(r"C:\motor-runner-evidence"))
    {
        return Err(step(
            "access-check-arguments",
            "paths are not the owned diagnostic roots",
        ));
    }
    session::no_reparse(&root, &fixture)?;
    session::no_reparse(
        Path::new(r"C:\motor-runner-evidence"),
        report.parent().unwrap(),
    )?;
    let vm = super::super::VmConfig::load(&root.join("vm.toml"))?;
    session::access_check_run(&vm, &fixture, &report)
}

fn diagnostic_write_probe(
    plan: &Plan,
    context: &session::Session,
    released: &mut bool,
) -> io::Result<Value> {
    use sha2::{Digest, Sha256};
    let program = plan.cwd.join("fetch-manifests/probe.exe");
    session::no_reparse(&plan.root, &program)?;
    if Sha256::digest(fs::read(&program)?) != Sha256::digest(fs::read(std::env::current_exe()?)?) {
        return Err(step(
            "diagnostic-write-probe",
            "probe does not match the trusted runner",
        ));
    }
    let outside = PathBuf::from(r"C:\Windows\Temp")
        .join(format!("motor-prep-diag-{}.txt", uuid::Uuid::now_v7()));
    let output = plan.stdout.with_extension("write-proof.json");
    let probe = Plan {
        root: plan.root.clone(),
        cwd: plan.cwd.clone(),
        program: program.to_string_lossy().into_owned(),
        args: vec![
            "prep-probe".into(),
            "diagnostic-writes".into(),
            plan.root
                .join("prep-secret.dpapi")
                .to_string_lossy()
                .into_owned(),
            outside.to_string_lossy().into_owned(),
            output.to_string_lossy().into_owned(),
        ],
        env: BTreeMap::new(),
        restricted: true,
        report: plan.report.clone(),
        stdout: plan.stdout.with_extension("write-probe.stdout"),
        stderr: plan.stderr.with_extension("write-probe.stderr"),
        diagnostic_policy: DiagnosticPolicy::None,
        diagnostic_loader_trace: plan.diagnostic_loader_trace,
        diagnostic_safe_cwd: plan.diagnostic_safe_cwd,
        diagnostic_native_calls: plan.diagnostic_native_calls,
    };
    let mut probe_created = false;
    let mut probe_started = false;
    let result = process::run(
        &probe,
        context,
        released,
        &mut probe_created,
        &mut probe_started,
    );
    let code = result?;
    let proof = if output.exists() {
        Some(serde_json::from_slice::<Value>(&fs::read(&output)?).map_err(io::Error::other)?)
    } else {
        None
    };
    if outside.exists() {
        fs::remove_file(&outside)?;
    }
    Ok(
        json!({"same_token_as_primary":true,"exit_code":code,"exit_status":process_status::describe(code),
        "stdout":bounded_text(&probe.stdout)?,"stderr":bounded_text(&probe.stderr)?,"proof":proof,
        "process_diagnostics":if plan.diagnostic_loader_trace { Some(diagnostic_process_report(&probe)) } else { None }}),
    )
}

fn diagnostic_process_report(plan: &Plan) -> Value {
    let path = plan.stdout.with_extension("process.json");
    fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({"error":"missing process diagnostics","path":path}))
}

fn save_process_report(plan: &Plan, value: &Value) -> io::Result<()> {
    session::write_private(
        &plan.stdout.with_extension("process.json"),
        &serde_json::to_vec_pretty(value).map_err(io::Error::other)?,
        false,
    )
}
pub(super) fn rng_probe(mode: &str, output: &Path) -> io::Result<()> {
    rng::probe(mode, output)
}

fn diagnostic_rng_call(
    plan: &Plan,
    context: &session::Session,
    released: &mut bool,
    mode: &str,
) -> io::Result<Value> {
    use sha2::{Digest, Sha256};
    let program = plan.cwd.join("fetch-manifests/probe.exe");
    session::no_reparse(&plan.root, &program)?;
    if Sha256::digest(fs::read(&program)?) != Sha256::digest(fs::read(std::env::current_exe()?)?) {
        return Err(step("same-token-rng", "probe differs from trusted runner"));
    }
    let mut rng = plan.clone();
    rng.program = program.to_string_lossy().into_owned();
    rng.diagnostic_safe_cwd = true;
    let output = plan.stdout.with_extension(format!("{mode}.json"));
    rng.stdout = plan.stdout.with_extension(format!("{mode}.stdout"));
    rng.stderr = plan.stderr.with_extension(format!("{mode}.stderr"));
    rng.args = vec![
        "prep-probe".into(),
        mode.into(),
        "unused".into(),
        "unused".into(),
        output.to_string_lossy().into_owned(),
    ];
    let mut rng_created = false;
    let mut rng_started = false;
    let code = process::run(&rng, context, released, &mut rng_created, &mut rng_started)?;
    let proof = fs::read(&output)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    Ok(
        json!({"same_token_as_safe_cwd_cargo":true,"mode":mode,"exit_status":process_status::describe(code),"rng_probe":proof,
        "process_diagnostics":diagnostic_process_report(&rng),"stdout":bounded_text(&rng.stdout)?,"stderr":bounded_text(&rng.stderr)?}),
    )
}
