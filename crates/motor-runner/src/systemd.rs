use std::{io, process::Output};

#[cfg(test)]
pub(crate) type CommandHook = std::sync::Arc<dyn Fn(&[&str]) -> io::Result<Output> + Send + Sync>;

pub(crate) fn failed_job_units(
    pattern: &str,
    run: impl Fn(&[&str]) -> io::Result<Output>,
) -> io::Result<u64> {
    let output = run(&[
        "list-units",
        "--type=service",
        "--state=failed",
        "--no-legend",
        "--plain",
        "--no-pager",
        pattern,
    ])?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "list failed {pattern} units: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(io::Error::other)?;
    let prefix = pattern
        .strip_suffix('*')
        .ok_or_else(|| io::Error::other("invalid unit pattern"))?;
    Ok(text
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|unit| unit.starts_with(prefix) && unit.ends_with(".service"))
        })
        .count() as u64)
}

pub(crate) fn alive(unit: &str, run: impl Fn(&[&str]) -> io::Result<Output>) -> io::Result<bool> {
    let output = run(&[
        "show",
        "--property=LoadState",
        "--property=ActiveState",
        "--property=SubState",
        unit,
    ])?;
    let text = String::from_utf8_lossy(&output.stdout);
    if text.lines().any(|line| {
        matches!(
            line,
            "LoadState=not-found" | "ActiveState=inactive" | "ActiveState=failed"
        )
    }) {
        return Ok(false);
    }
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "inspect {unit}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if text.lines().any(|line| {
        matches!(
            line,
            "ActiveState=active" | "ActiveState=activating" | "ActiveState=deactivating"
        )
    }) {
        Ok(true)
    } else {
        Err(io::Error::other(format!(
            "inspect {unit}: missing unit state"
        )))
    }
}

pub(crate) fn oom_killed(
    unit: &str,
    run: impl Fn(&[&str]) -> io::Result<Output>,
) -> io::Result<bool> {
    let output = run(&["show", "--property=Result", unit])?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "inspect {unit} result: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let result = std::str::from_utf8(&output.stdout)
        .map_err(io::Error::other)?
        .lines()
        .find_map(|line| line.strip_prefix("Result="))
        .ok_or_else(|| io::Error::other(format!("inspect {unit}: missing result")))?;
    Ok(result == "oom-kill")
}

pub(crate) fn exit_status(
    unit: &str,
    run: impl Fn(&[&str]) -> io::Result<Output>,
) -> io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let output = run(&[
        "show",
        "--property=ExecMainCode",
        "--property=ExecMainStatus",
        unit,
    ])?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "inspect {unit} exit status: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(io::Error::other)?;
    let property = |name: &str| -> io::Result<i32> {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| io::Error::other(format!("inspect {unit}: missing {name}")))?
            .parse()
            .map_err(io::Error::other)
    };
    let code = property("ExecMainCode=")?;
    let status = property("ExecMainStatus=")?;
    // ExecMainCode uses waitid's CLD_* values, not a shell's 128+signal code.
    // Zero is systemd's default for a unit with no retained main-process status.
    let raw = match code {
        libc::CLD_EXITED if (0..=255).contains(&status) => status << 8,
        libc::CLD_KILLED if (1..=127).contains(&status) => status,
        libc::CLD_DUMPED if (1..=127).contains(&status) => status | 0x80,
        _ => {
            return Err(io::Error::other(format!(
                "inspect {unit}: no valid retained exit status (ExecMainCode={code}, ExecMainStatus={status})"
            )));
        }
    };
    Ok(std::process::ExitStatus::from_raw(raw))
}

pub(crate) fn signal(
    unit: &str,
    hard: bool,
    run: impl Fn(&[&str]) -> io::Result<Output>,
) -> io::Result<()> {
    for args in [
        Some(vec!["stop", "--no-block", unit]),
        hard.then(|| vec!["kill", "--kill-whom=all", "--signal=KILL", unit]),
    ]
    .into_iter()
    .flatten()
    {
        let output = run(&args)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "signal {unit}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "systemd_tests.rs"]
mod tests;
