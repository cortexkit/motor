//! Best-effort runner journal tails before a failed job channel is torn down.
use crate::helper::Control;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub(super) fn collect(control: Arc<dyn Control>, job: &str, budget: Duration) {
    let (send, receive) = std::sync::mpsc::channel();
    let query_job = job.to_owned();
    // The control exchange has its own transport deadline. Do not let a stuck
    // guest consume more than the existing teardown grace period here.
    if std::thread::Builder::new()
        .name("job-runner-diagnostics".into())
        .spawn(move || {
            let _ = send.send(control.job_admin(&query_job, &json!({"op":"diagnostics"})));
        })
        .is_err()
    {
        tracing::warn!(
            jti = job,
            reason = "query_spawn_failed",
            "job_runner_diagnostics_failed"
        );
        return;
    }
    match receive.recv_timeout(budget.min(Duration::from_secs(5))) {
        Ok(Ok(reply)) => log_reply(job, &reply),
        Ok(Err(_)) => {
            // Control errors can quote guest or peer data; never log them raw.
            tracing::warn!(
                jti = job,
                reason = "query_failed",
                "job_runner_diagnostics_failed"
            );
        }
        Err(_) => {
            tracing::warn!(
                jti = job,
                reason = "query_timed_out",
                "job_runner_diagnostics_failed"
            );
        }
    }
}

fn log_reply(job: &str, reply: &Value) {
    let journal = &reply["journal"];
    if reply["ok"] != true
        || journal["unavailable"] == true
        || journal["unrecognized_messages_redacted"] != true
    {
        tracing::warn!(
            jti = job,
            reason = "redacted_journal_unavailable",
            "job_runner_diagnostics_failed"
        );
        return;
    }
    let Some(entries) = journal["entries"].as_array() else {
        tracing::warn!(
            jti = job,
            reason = "journal_entries_missing",
            "job_runner_diagnostics_failed"
        );
        return;
    };
    // Keep the last lines, not the first: the close reason is near the end.
    for entry in entries.iter().skip(entries.len().saturating_sub(20)) {
        if let Some(message) = entry["message"].as_str() {
            let line: String = message
                .chars()
                .take(300)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            tracing::info!(jti = job, unix_us = entry["unix_us"].as_u64(), %line, "job_runner_log");
        }
    }
    tracing::info!(
        jti = job,
        entries = entries.len().min(20),
        truncated = entries.len() > 20 || journal["truncated"] == true,
        "job_runner_diagnostics_finished"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_tail_keeps_last_redacted_lines_with_unicode_and_output_bounds() {
        let entries: Vec<_> = (0..25)
            .map(|index| json!({"message":format!("line{index} {}SHOULD_NOT_APPEAR", "é\n".repeat(300))}))
            .collect();
        let logs = crate::test_logs::capture(|| {
            log_reply(
                "job",
                &json!({"ok":true,"journal":{
                    "entries":entries,"unrecognized_messages_redacted":true
                }}),
            );
        });
        let lines: Vec<_> = logs
            .iter()
            .filter(|s| s.contains("message=job_runner_log "))
            .collect();
        assert_eq!(lines.len(), 20, "{logs:?}");
        assert!(lines[0].contains("line5 "));
        assert!(lines[19].contains("line24 "));
        assert!(lines
            .iter()
            .all(|line| !line.contains("SHOULD_NOT_APPEAR") && !line.contains("\\n")));
        assert!(logs.iter().any(|line| line.contains("truncated=true")));
    }

    #[test]
    fn runner_tail_never_logs_raw_diagnostics_or_failure_details() {
        for reply in [
            json!({"ok":false,"error":"SECRET"}),
            json!({"ok":true,"journal":{"unavailable":true,"error":"SECRET"}}),
            json!({"ok":true,"journal":{"entries":[{"message":"SECRET"}]}}),
        ] {
            let logs = crate::test_logs::capture(|| log_reply("job", &reply));
            assert!(logs
                .iter()
                .any(|line| line.contains("job_runner_diagnostics_failed")));
            assert!(logs.iter().all(|line| !line.contains("SECRET")));
        }
    }

    #[test]
    fn runner_tail_accepts_old_guest_and_exited_runner_reply_shapes() {
        let mut old_guest = json!({"ok":true,"runner_pid":123,"processes":[],"flocks":[],
        "network":{"unavailable":true},"journal":{
            "entries":[{"message":"runner_channel_closed reason=lease_expired"}],
            "unrecognized_messages_redacted":true
        }});
        for exited in [false, true] {
            if exited {
                for field in ["runner_pid", "processes", "flocks"] {
                    old_guest[field] = json!({"unavailable":true});
                }
            }
            let logs = crate::test_logs::capture(|| log_reply("job", &old_guest));
            assert!(logs
                .iter()
                .any(|line| line.contains("runner_channel_closed reason=lease_expired")));
            assert!(logs
                .iter()
                .any(|line| line.contains("job_runner_diagnostics_finished")));
            assert!(logs
                .iter()
                .all(|line| !line.contains("job_runner_diagnostics_failed")));
        }
    }
}
