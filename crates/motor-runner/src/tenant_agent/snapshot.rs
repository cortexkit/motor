//! Suspend is a filesystem consistency boundary, not just a VMM pause. Admission
//! stays drained until thaw, including in the RAM captured by the snapshot.
use super::*;
use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

static FROZEN: AtomicBool = AtomicBool::new(false);
static DRAINED: AtomicBool = AtomicBool::new(false);

pub(super) fn frozen() -> bool {
    FROZEN.load(Ordering::Acquire)
}

/// Why the runner cannot be snapshotted now, or None when it is idle. The
/// runner reports `running_jobs` as a list of jobs, not a count; anything
/// missing or malformed counts as busy, so a snapshot never captures a job.
pub(super) fn busy(value: &Value) -> Option<String> {
    if value["ok"] != true {
        return Some(format!("runner status unavailable: {value}"));
    }
    let Some(queued) = value["queue_depth"].as_u64() else {
        return Some("runner status has no queue_depth".into());
    };
    let Some(running) = value["running_jobs"].as_array() else {
        return Some("runner status has no running_jobs list".into());
    };
    if queued == 0 && running.is_empty() {
        return None;
    }
    let running: Vec<String> = running
        .iter()
        .map(|job| {
            format!(
                "{} running in {} (weight {})",
                job["job_id"].as_str().unwrap_or("?"),
                job["workspace_key"].as_str().unwrap_or("?"),
                job["weight"]
            )
        })
        .collect();
    Some(format!(
        "queue_depth={queued}; running_jobs=[{}]",
        running.join(", ")
    ))
}
pub(super) fn mounted(path: &str) -> io::Result<bool> {
    Ok(fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .any(|line| line.split_whitespace().nth(4) == Some(path)))
}

/// None once the runner is idle and admission is drained; otherwise what is
/// still busy.
async fn check_and_drain<F, Fut>(mut control: F, drained: &AtomicBool) -> io::Result<Option<String>>
where
    F: FnMut(Frame) -> Fut,
    Fut: Future<Output = io::Result<Value>>,
{
    if let Some(reason) = busy(&control(Frame::Status).await?) {
        return Ok(Some(reason));
    }
    control(Frame::Drain {
        deadline_s: 0,
        exit_after: false,
    })
    .await?;
    drained.store(true, Ordering::Release);
    // Admission and drain share the runner lock. This second read catches a job
    // admitted between the first status and closing admission, without waiting.
    Ok(busy(&control(Frame::Status).await?))
}

async fn command(program: &str, args: &[&str]) -> io::Result<()> {
    command_with_timeout(program, args, Duration::from_secs(10)).await
}
async fn command_with_timeout(program: &str, args: &[&str], timeout: Duration) -> io::Result<()> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "snapshot filesystem command timed out",
            )
        })??;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(super) async fn prepare(kind: SnapshotKind) -> io::Result<Value> {
    let result = async {
        if kind == SnapshotKind::Base {
            if runner_socket::main_pid()?.is_some() || mounted("/tenant")? {
                return Ok(json!({"ok":false,"error":"snapshot_busy"}));
            }
            command("sync", &[]).await?;
            return Ok(json!({"ok":true}));
        }
        if let Some(reason) = check_and_drain(super::forward, &DRAINED).await? {
            return Ok(json!({"ok":false,"error":format!("snapshot_busy: {reason}")}));
        }
        if !mounted("/tenant")? {
            return Err(io::Error::other("tenant filesystem is not mounted"));
        }
        command("sync", &["-f", "/tenant"]).await?;
        // Mark before the command: a timed-out freeze may have reached the
        // kernel. The compensating thaw must run even if its reply was lost.
        FROZEN.store(true, Ordering::Release);
        command("fsfreeze", &["-f", "/tenant"]).await?;
        Ok(json!({"ok":true}))
    }
    .await;
    if !result.as_ref().is_ok_and(|value| value["ok"] == true) {
        thaw().await?;
    }
    result
}

pub(super) async fn thaw() -> io::Result<()> {
    if FROZEN.load(Ordering::Acquire) {
        tokio::task::spawn_blocking(|| {
            let file = fs::File::open("/tenant")?;
            // FITHAW is the ioctl used by fsfreeze -u. EINVAL means that a
            // previous thaw already completed, which makes retries idempotent.
            // Typed u32 first: musl's request type is a signed int, and the
            // cast must keep the bit pattern rather than overflow a literal.
            if unsafe { libc::ioctl(file.as_raw_fd(), 0xc0045878_u32 as libc::Ioctl, 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINVAL) {
                    return Err(error);
                }
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)??;
        FROZEN.store(false, Ordering::Release);
    }
    if DRAINED.load(Ordering::Acquire) {
        super::forward(Frame::Resume).await?;
        DRAINED.store(false, Ordering::Release);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn hosted_prepare_refuses_started_and_admitted_jobs_without_drain() {
        let job = json!({"job_id":"01a156a8-dae7-77d3-9612-855938acf185","workspace_key":"/Users/q","weight":1});
        for (queued, running) in [(1, vec![]), (0, vec![job.clone()]), (1, vec![job])] {
            let mut calls = vec![];
            let busy_running = !running.is_empty();
            let result = check_and_drain(
                |frame| {
                    calls.push(frame);
                    std::future::ready(Ok(
                        json!({"ok":true,"queue_depth":queued,"running_jobs":running.clone()}),
                    ))
                },
                &AtomicBool::new(false),
            )
            .await
            .unwrap()
            .expect("busy runner must refuse");
            assert!(
                result.contains(&format!("queue_depth={queued}")),
                "{result}"
            );
            assert_eq!(
                result.contains(
                    "01a156a8-dae7-77d3-9612-855938acf185 running in /Users/q (weight 1)"
                ),
                busy_running,
                "{result}"
            );
            assert_eq!(calls, vec![Frame::Status]);
        }
    }
    #[tokio::test]
    async fn hosted_prepare_drains_an_idle_runner_as_the_runner_reports_it() {
        // Build the status exactly as the agent receives it from a real idle
        // runner, through the same conversion the agent's forwarder uses.
        let status = || {
            let mut value = super::super::agent_reply_value(Frame::StatusReply(
                motor_protocol::RunnerStatusReply {
                    status: motor_protocol::StatusReply::new(0, vec![], true, vec![], "rustc"),
                    disk: None,
                    health: None,
                },
            ))
            .unwrap();
            value["ok"] = json!(true);
            value
        };
        let mut calls = vec![];
        let result = check_and_drain(
            |frame| {
                calls.push(frame);
                std::future::ready(Ok(status()))
            },
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(result, None);
        assert_eq!(calls.len(), 3);
    }
    #[tokio::test]
    async fn hosted_prepare_rechecks_admission_race_after_bounded_drain() {
        let mut calls = vec![];
        let result = check_and_drain(
            |frame| {
                calls.push(frame);
                std::future::ready(Ok(if calls.len() == 3 {
                    json!({"ok":true,"queue_depth":1,"running_jobs":[]})
                } else {
                    json!({"ok":true,"queue_depth":0,"running_jobs":[]})
                }))
            },
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert_eq!(result.as_deref(), Some("queue_depth=1; running_jobs=[]"));
        assert_eq!(
            calls,
            vec![
                Frame::Status,
                Frame::Drain {
                    deadline_s: 0,
                    exit_after: false
                },
                Frame::Status
            ]
        );
    }
    #[test]
    fn hosted_prepare_missing_or_malformed_status_fields_are_busy() {
        for value in [
            json!({"ok":true}),
            json!({"ok":false,"queue_depth":0,"running_jobs":[]}),
            // A count where the runner sends a list is not proof of idleness.
            json!({"ok":true,"queue_depth":0,"running_jobs":0}),
        ] {
            assert!(busy(&value).is_some(), "{value}");
        }
    }
    #[tokio::test]
    async fn hosted_filesystem_commands_refuse_failures_and_have_a_deadline() {
        assert!(
            command_with_timeout("/bin/sh", &["-c", "exit 23"], Duration::from_secs(1))
                .await
                .is_err()
        );
        let error = command_with_timeout(
            "/bin/sh",
            &["-c", "exec sleep 60"],
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
    #[tokio::test]
    async fn hosted_thaw_unfrozen_filesystem_is_a_noop_success() {
        assert!(!FROZEN.load(Ordering::Acquire));
        assert!(!DRAINED.load(Ordering::Acquire));
        thaw().await.unwrap();
    }
}
