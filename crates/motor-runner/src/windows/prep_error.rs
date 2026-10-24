use super::*;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "MOTOR_PREP_ERROR ";
const WARNING_PREFIX: &str = "MOTOR_PREP_WARNING ";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    step: String,
    reason: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "windows prep: {}: {}", self.step, self.reason)
    }
}
impl std::error::Error for Failure {}

pub(super) fn step<T>(
    step: &str,
    staging: &Path,
    run: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    run().map_err(|error| {
        // Only runner-authored errors and OS reasons belong in this record.
        // Subprocess stderr can contain credentials and is never passed here.
        let reason = error
            .to_string()
            .replace(&format!("{}/", staging.display()), "")
            .replace(&format!("{}\\", staging.display()), "")
            .replace(&staging.to_string_lossy().to_string(), "staging");
        let reason = reason
            .split_whitespace()
            .map(|word| {
                if (word.contains("://") && (word.contains('@') || word.contains('?')))
                    || word.starts_with('/')
                    || word.contains(":\\")
                {
                    "[redacted path or source]"
                } else {
                    word
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        #[cfg(windows)]
        let reason = reason.replace('\\', "/");
        let reason = if reason.len() > 4096 {
            "failure detail exceeds diagnostic limit".into()
        } else {
            reason
        };
        io::Error::new(
            error.kind(),
            Failure {
                step: step.into(),
                reason,
            },
        )
    })
}

pub(crate) fn error_line(error: &io::Error) -> String {
    let fallback = Failure {
        step: "read preparation plan".into(),
        reason: error.kind().to_string(),
    };
    let failure = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<Failure>())
        .unwrap_or(&fallback);
    format!("{PREFIX}{}", serde_json::to_string(failure).unwrap())
}

pub(super) fn is_reported(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|e| e.is::<Failure>())
}

pub(super) fn nested_warning(staging: &Path, lockfile: &Path, error: io::Error) -> String {
    let path = lockfile
        .strip_prefix(staging)
        .expect("tracked lockfile in staging");
    let error = step(
        &format!("fetch nested lockfile {}", path.display()),
        staging,
        || -> io::Result<()> { Err(error) },
    )
    .unwrap_err();
    error_line(&error).replacen(PREFIX, WARNING_PREFIX, 1)
}

pub(super) fn log_warnings(stderr: &[u8]) {
    for warning in String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(WARNING_PREFIX))
        .filter_map(|line| serde_json::from_str::<Failure>(line).ok())
    {
        tracing::warn!("{warning}");
    }
}

#[cfg(unix)]
pub(super) fn refused(output: &Output) -> io::Error {
    let failure = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .filter_map(|line| serde_json::from_str::<Failure>(line).ok())
        .next_back();
    let mut failure = failure.unwrap_or_else(|| Failure {
        step: "launch trusted preparation".into(),
        reason: "no structured failure detail".into(),
    });
    failure.reason.push_str(&format!(" ({})", output.status));
    io::Error::new(io::ErrorKind::InvalidData, failure)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn prep_failure_record_keeps_relative_paths_and_redacts_host_paths_and_credentials() {
        let staging = Path::new("/private/staging/job");
        let error = step("stage dependencies", staging, || -> io::Result<()> {
            Err(invalid("special file: /private/staging/job/cargo-home/git/checkouts/dep/rev/file; /private/host https://user:secret@example.invalid/repo"))
        }).unwrap_err();
        let line = error_line(&error);
        assert!(
            line.contains("cargo-home/git/checkouts/dep/rev/file"),
            "{line}"
        );
        assert!(!line.contains("/private"), "{line}");
        assert!(!line.contains("secret"), "{line}");
        let output = Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: format!("systemd status\n{line}\nexit-code\n").into_bytes(),
        };
        let detail = refused(&output).to_string();
        assert!(
            detail.starts_with("windows prep: stage dependencies: special file: cargo-home/"),
            "{detail}"
        );
        assert!(detail.ends_with("(exit status: 1)"), "{detail}");
    }

    #[test]
    fn prep_failure_without_a_record_does_not_forward_untrusted_stderr() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: b"/private/host secret\nMOTOR_PREP_ERROR {broken}\n".to_vec(),
        };
        assert_eq!(refused(&output).to_string(), "windows prep: launch trusted preparation: no structured failure detail (exit status: 1)");
    }
}
