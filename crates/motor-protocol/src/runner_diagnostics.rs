//! Allowlisted runner diagnostics shared by guest and host.

/// A separate console namespace, never interpreted as kernel or helper output.
pub const CONSOLE_TAG: &str = "MOTOR_RUNNER_DIAGNOSTIC ";
pub const CONSOLE_MAX_LINES: usize = 28;
pub const CONSOLE_MAX_BYTES: usize = 8400;
pub const CONSOLE_LINE_CHARS: usize = 300;

/// Redact raw runner messages or already-redacted console messages. The host
/// repeats this operation because even tagged guest console bytes are untrusted.
/// Unlike the historical journal redactor, this also identifies Rust panics.
pub fn console_message(message: &str) -> String {
    if matches!(
        message,
        "immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/registry"
            | "immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/git"
    ) {
        return message.to_owned();
    }
    let location = message.strip_prefix("runner panic location=").or_else(|| {
        message
            .strip_prefix("thread '")?
            .split_once("' panicked at ")?
            .1
            .strip_suffix(':')
    });
    if let Some(location) = location {
        return format!("runner panic location={}", panic_location(location));
    }
    for (prefix, category) in [
        (
            "called `Result::unwrap()` on an `Err` value",
            "result_unwrap",
        ),
        (
            "called `Option::unwrap()` on a `None` value",
            "option_unwrap",
        ),
        ("index out of bounds", "index_out_of_bounds"),
        ("assertion `left == right` failed", "assert_eq"),
        ("assertion `left != right` failed", "assert_ne"),
        ("assertion failed:", "assert"),
        ("attempt to ", "arithmetic"),
    ] {
        if message.starts_with(prefix) || message == format!("runner panic cause={category}") {
            return format!("runner panic cause={category}");
        }
    }
    if message == "runner panic cause=[message omitted]" {
        return message.to_owned();
    }
    // Canonical event messages have no tracing target. Put it back only for
    // allowlist parsing, not for publication, so redaction remains idempotent.
    let raw = if message.contains("motor_runner::") || message.starts_with("Error: ") {
        message.to_owned()
    } else {
        format!("motor_runner::console: {message}")
    };
    journal_message(&raw)
}

fn panic_location(location: &str) -> String {
    let location = location
        .rsplit_once("/crates/")
        .map_or_else(|| location.to_owned(), |(_, rest)| format!("crates/{rest}"));
    let valid = (|| {
        let (path, column) = location.rsplit_once(':')?;
        let (path, line) = path.rsplit_once(':')?;
        line.parse::<u32>().ok().filter(|n| *n > 0)?;
        column.parse::<u32>().ok().filter(|n| *n > 0)?;
        let rest = path.strip_prefix("crates/")?;
        let (krate, source) = rest.split_once("/src/")?;
        if !matches!(krate, "motor-runner" | "motor-protocol" | "motor-lease")
            || !source.ends_with(".rs")
            || path.len() > 200
            || !source.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            })
        {
            return None;
        }
        Some(())
    })()
    .is_some();
    if valid {
        location
    } else {
        "[location omitted]".into()
    }
}

fn writer_outcome(value: &str) -> Option<String> {
    if matches!(
        value,
        "None" | "Some(Cancelled)" | "Some(OutcomeUnknown)" | "Some(HistoryExpired)"
    ) {
        return Some(value.into());
    }
    if let Some(reason) = value
        .strip_prefix("Some(RefusedBeforeStart { reason: ")
        .and_then(|v| v.strip_suffix(" })"))
    {
        if matches!(
            reason,
            "Unreachable"
                | "RunnerVersionMismatch"
                | "SnapshotFailed"
                | "WorkspaceKeyRejected"
                | "RunnerFull"
                | "RunnerDraining"
                | "RunnerDiskFull"
                | "QueueWaitExceeded"
                | "TransferInterrupted"
                | "TreeHashMismatch"
                | "BundleRejected"
                | "WorkspaceSetupFailed"
                | "NetworkUnsupported"
                | "PlatformUnsupported"
                | "LostToRestart"
                | "RequestRejected"
                | "RunnerAuthenticationFailed"
                | "SubmissionNeverAccepted"
        ) {
            return Some(value.into());
        }
    }
    for (prefix, signed) in [
        ("Some(Exit { code: ", true),
        ("Some(WindowsExit { code: ", false),
        ("Some(Signal { signal: ", true),
    ] {
        if let Some(number) = value
            .strip_prefix(prefix)
            .and_then(|v| v.strip_suffix(" })"))
        {
            if if signed {
                number.parse::<i32>().is_ok()
            } else {
                number.parse::<u32>().is_ok()
            } {
                return Some(value.into());
            }
        }
    }
    None
}

fn writer_kill(value: &str) -> bool {
    value == "None"
        || ["Deadline", "Cancel"].iter().any(|reason| {
            value
                .strip_prefix(&format!("Some(({reason}, "))
                .and_then(|v| v.strip_suffix("))"))
                .is_some_and(|number| number.parse::<u64>().is_ok())
        })
}

pub fn journal_message(message: &str) -> String {
    // Journal entries and subprocess errors can quote uploaded text. Only the
    // runner's fixed setup messages and numeric/allowlisted fields cross vsock.
    let startup = message.strip_prefix("Error: ").unwrap_or(message);
    if let Some(rest) = startup.strip_prefix("immutable Cargo source ACL check failed: ") {
        if let Some((path, _)) = rest.split_once(';') {
            if matches!(
                path,
                "/usr/local/lib/motor/cargo/registry" | "/usr/local/lib/motor/cargo/git"
            ) {
                return format!("immutable Cargo source ACL check failed: {path}");
            }
        }
    }
    let Some((_, rest)) = message.split_once("motor_runner::") else {
        return "[message omitted]".into();
    };
    let Some((_, rest)) = rest.split_once(": ") else {
        return "[message omitted]".into();
    };
    let names = [
        "materialize started",
        "materialize finished",
        "setup lock wait started",
        "setup lock wait finished",
        "handing materialized job to execution",
        "job Started recorded",
        "bundle upload complete",
        "bundles fetched; job queued",
        "placed job CPUs",
        "runner deadline signal was late",
        "runner maintenance pass exceeded ten seconds",
        "writer lifecycle",
        "build base seed refused: record mismatch",
        "build base seed failed; building without seed",
        "build base seed environment changed during sandbox preparation; building without seed",
        "build base seed succeeded",
        "build base seed skipped",
        "build base published",
        "Cargo build-folder maintenance",
        "runner admission cleared its own build outputs",
        "runner build base eviction",
        // Job-stream lifecycle on the tenant boundary: why a connection from
        // motor-host closed, and on which side. Their fields are the runner's
        // own labels, counters and job ids, never frame contents.
        "runner hello received",
        "runner hello written",
        "runner job stream attached",
        "runner job stream refused frame",
        "runner cancel frame received",
        "runner cancel handled",
        "runner terminal written to client",
        "runner terminal memory wait",
        "runner frame write failed",
        "runner_channel_closed",
        "operation_refused",
        "tenant frame read failed",
        "tenant_job_stream_ended",
    ];
    let Some(name) = names.iter().find(|name| {
        rest.strip_prefix(**name).is_some_and(|suffix| {
            suffix.is_empty()
                || suffix.strip_prefix(' ').is_some_and(|fields| {
                    !name.starts_with("build base ")
                        || fields
                            .split_whitespace()
                            .next()
                            .is_some_and(|t| t.contains('='))
                })
        })
    }) else {
        return "[message omitted]".into();
    };
    let mut output = name.to_string();
    let fields = &rest[name.len()..];
    let (fields, writer_debug) = if *name == "writer lifecycle" {
        fields
            .split_once(" outcome=")
            .map_or((fields, None), |(before, after)| (before, Some(after)))
    } else {
        (fields, None)
    };
    let mut stopped = false;
    for token in fields.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        // These events contain unquoted paths or Debug values with spaces. Once
        // one begins, later whitespace-delimited text is not a reliable field.
        if (name.starts_with("build base ") || *name == "writer lifecycle")
            && matches!(
                key,
                "repository" | "workspace" | "outcome" | "kill" | "error" | "cleanup" | "inputs"
            )
        {
            stopped = true;
            break;
        }
        let valid = match key {
            _ if matches!(
                *name,
                "Cargo build-folder maintenance"
                    | "runner admission cleared its own build outputs"
                    | "runner build base eviction"
            ) =>
            {
                match key {
                    "job_id" => uuid::Uuid::parse_str(value).is_ok(),
                    "success" => matches!(value, "true" | "false"),
                    "legacy_candidates" | "foreign_candidates" | "bytes" | "free_bytes"
                    | "floor_bytes" | "goal_bytes" | "last_used_ms" | "bytes_freed"
                    | "allocated_bytes" | "pass" => value.parse::<u64>().is_ok(),
                    _ => false,
                }
            }
            "job_id" => uuid::Uuid::parse_str(value).is_ok(),
            "cache_fill" | "success" => matches!(value, "true" | "false"),
            "reason" if *name == "build base seed skipped" => matches!(
                value,
                "no_base"
                    | "unreadable_record"
                    | "siblings"
                    | "base_job"
                    | "not_fixed_path"
                    | "not_isolated"
                    | "target_not_empty"
                    | "workspace_not_tracked"
                    | "record_mismatch"
                    | "symlink_change"
                    | "seed_failed"
            ),
            "timed_out" if *name == "runner terminal memory wait" => {
                matches!(value, "true" | "false")
            }
            "inputs_match" | "times_valid" | "toolchain_match" | "environment_match"
            | "fixed_path_match" | "device_match" | "format_match"
                if *name == "build base seed refused: record mismatch" =>
            {
                matches!(value, "true" | "false")
            }
            "tip" if *name == "build base published" => {
                value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
            }
            "event" if *name == "writer lifecycle" => matches!(
                value,
                "started" | "kill_requested" | "runtime_expired" | "finished"
            ),
            "kind" if *name == "writer lifecycle" => {
                matches!(value, "base_build" | "compile_cache_fill")
            }
            "started_ms" | "age_ms" if *name == "writer lifecycle" => {
                value == "None"
                    || value.parse::<u64>().is_ok()
                    || value
                        .strip_prefix("Some(")
                        .and_then(|v| v.strip_suffix(')'))
                        .is_some_and(|v| v.parse::<u64>().is_ok())
            }
            "unit" if *name == "writer lifecycle" => {
                value == "not_started"
                    || value
                        .strip_prefix("motor-job-")
                        .and_then(|v| v.strip_suffix(".service"))
                        .is_some_and(|v| uuid::Uuid::parse_str(v).is_ok())
            }
            "lock" => matches!(
                value.trim_matches('"'),
                "tree_access" | "dependency_mirror" | "build_wrapper"
            ),
            "elapsed_ms"
            | "wait_ms"
            | "bytes"
            | "requested_cpus"
            | "from_seq"
            | "frames"
            | "lateness_ms"
            | "slice_budget_ms"
            | "source_unseals_ms"
            | "runtime_cleanup_ms"
            | "state_ms"
            | "expired_jobs_ms"
            | "returned_files_ms"
            | "return_reservations_ms"
            | "build_folders_ms"
            | "sibling_snapshots_ms"
            | "prune_refs_ms"
            | "retire_repositories_ms" => value.parse::<u64>().is_ok(),
            "jti" => uuid::Uuid::parse_str(
                value
                    .strip_prefix("Some(")
                    .and_then(|v| v.strip_suffix(')'))
                    .unwrap_or(value),
            )
            .is_ok(),
            "seq" => {
                value == "None"
                    || value
                        .strip_prefix("Some(")
                        .and_then(|v| v.strip_suffix(')'))
                        .is_some_and(|v| v.parse::<u64>().is_ok())
            }
            // Code labels: a direction, close reason, frame type tag or I/O
            // error kind, possibly in an Option's Debug form.
            "direction" | "reason" | "frame_type" | "frame" | "kind" | "error_kind"
            | "supports_started" => {
                value.len() <= 48
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'(' | b')'))
            }
            _ => false,
        };
        if *name == "writer lifecycle" && key == "unit" && !valid {
            stopped = true;
            break;
        }
        if valid {
            output.push(' ');
            output.push_str(token);
        }
    }
    if let Some((outcome, rest)) = writer_debug
        .filter(|_| !stopped)
        .and_then(|fields| fields.split_once(" kill="))
    {
        if let Some(outcome) = writer_outcome(outcome) {
            output.push_str(" outcome=");
            output.push_str(&outcome);
            if let Some((kill, _)) = rest.split_once(" repository=") {
                if writer_kill(kill) {
                    output.push_str(" kill=");
                    output.push_str(kill);
                }
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_redaction_is_idempotent_and_omits_free_text() {
        for (raw, expected) in [
            ("[motor-runner] ERROR motor_runner::server: runner_channel_closed reason=eof error=SECRET", "runner_channel_closed reason=eof"),
            ("Error: immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/git; SECRET", "immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/git"),
            ("thread 'SECRET' panicked at /build/SECRET/crates/motor-runner/src/tenant.rs:12:3:", "runner panic location=crates/motor-runner/src/tenant.rs:12:3"),
            ("called `Result::unwrap()` on an `Err` value: SECRET", "runner panic cause=result_unwrap"),
            ("assertion failed: SECRET", "runner panic cause=assert"),
            ("SECRET stdout", "[message omitted]"),
        ] {
            assert_eq!(console_message(raw), expected);
            assert_eq!(console_message(expected), expected);
            assert!(!console_message(raw).contains("SECRET"));
        }
    }

    #[test]
    fn panic_locations_are_only_motor_source_paths() {
        for location in [
            "/tenant/SECRET.rs:1:2",
            "crates/SECRET/src/lib.rs:1:2",
            "crates/motor-runner/src/../../SECRET.rs:1:2",
            "crates/motor-runner/src/lib.rs:SECRET:2",
            "crates/motor-runner/src/lib.rs:0:2",
            "crates/motor-runner/tests/SECRET.rs:1:2",
            "crates/motor-runner/src/SECRET\n.rs:1:2",
        ] {
            assert_eq!(
                console_message(&format!("thread 'main' panicked at {location}:")),
                "runner panic location=[location omitted]"
            );
        }
    }

    #[test]
    fn build_folder_maintenance_exposes_only_bounded_decision_fields() {
        let raw = "[motor-runner] INFO motor_runner::cargo_build_dir: Cargo build-folder maintenance legacy_candidates=4 foreign_candidates=2 success=true path=/SECRET reason=SECRET";
        let expected =
            "Cargo build-folder maintenance legacy_candidates=4 foreign_candidates=2 success=true";
        assert_eq!(console_message(raw), expected);
        assert_eq!(console_message(expected), expected);
    }

    #[test]
    fn terminal_memory_wait_exposes_only_its_job_duration_and_timeout_flag() {
        let raw = "[motor-runner] INFO motor_runner::server: runner terminal memory wait job_id=01a1d8c8-f1ac-7333-b943-a32753fd8936 wait_ms=12 timed_out=false path=/SECRET";
        let expected = "runner terminal memory wait job_id=01a1d8c8-f1ac-7333-b943-a32753fd8936 wait_ms=12 timed_out=false";
        assert_eq!(console_message(raw), expected);
        assert_eq!(console_message(expected), expected);
        assert_eq!(
            console_message("runner terminal memory wait timed_out=maybe wait_ms=x"),
            "runner terminal memory wait"
        );
    }

    #[test]
    fn admitting_target_cleanup_exposes_only_numeric_pressure_fields() {
        let raw = "[motor-runner] INFO motor_runner::disk: runner admission cleared its own build outputs path=/SECRET bytes=400 free_bytes=500 floor_bytes=100 goal_bytes=350 reason=SECRET";
        let expected = "runner admission cleared its own build outputs bytes=400 free_bytes=500 floor_bytes=100 goal_bytes=350";
        assert_eq!(console_message(raw), expected);
        assert_eq!(console_message(expected), expected);
    }

    #[test]
    fn base_eviction_exposes_only_numeric_reclaim_fields() {
        let raw = "[motor-runner] INFO motor_runner::disk: runner build base eviction repository=/SECRET generation=current last_used_ms=123 bytes_freed=400 allocated_bytes=500 pass=4 reason=SECRET";
        let expected = "runner build base eviction last_used_ms=123 bytes_freed=400 allocated_bytes=500 pass=4";
        assert_eq!(console_message(raw), expected);
        assert_eq!(console_message(expected), expected);
    }

    #[test]
    fn seed_refusal_separates_compatibility_dimensions_without_inputs() {
        let raw = "[motor-runner] INFO motor_runner::seed: build base seed refused: record mismatch inputs_match=false times_valid=true toolchain_match=true environment_match=false fixed_path_match=true device_match=true format_match=true workspace=/SECRET";
        let expected = "build base seed refused: record mismatch inputs_match=false times_valid=true toolchain_match=true environment_match=false fixed_path_match=true device_match=true format_match=true";
        assert_eq!(console_message(raw), expected);
        assert_eq!(console_message(expected), expected);
    }
}
