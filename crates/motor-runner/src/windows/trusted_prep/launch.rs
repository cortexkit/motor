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
mod session;

#[derive(Serialize, Deserialize)]
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
    let mut report = json!({"phase":"session-setup", "program":plan.program, "arguments":plan.args,
        "restricted":plan.restricted, "cwd":plan.cwd, "profile_loaded":false,
        "inherit_handles":"explicit stdin/stdout/stderr allow-list only", "active_process_limit":32,
        "creation_flags":format!("0x{:08X}", process::CREATION_FLAGS)});
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
        let mut released = true;
        let child = process::run(&plan, &context, &mut released);
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
        let code = child?;
        report["exit_code"] = json!(code);
        report["exit_status"] = json!(process_status::describe(code));
        report["stdout"] = json!(bounded_text(&plan.stdout)?);
        report["stderr"] = json!(bounded_text(&plan.stderr)?);
        report["phase"] = json!("cleanup");
        save(&report)?;
        let cleanup = context.finish();
        if code != 0 {
            if let Err(error) = cleanup {
                report["cleanup_error"] = json!(error.to_string());
            }
            return Err(step(
                "child-exit",
                format!(
                    "Windows trusted cargo fetch exited {}; diagnostics: {}",
                    process_status::describe(code),
                    report["stderr"].as_str().unwrap_or("")
                ),
            ));
        }
        cleanup?;
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
