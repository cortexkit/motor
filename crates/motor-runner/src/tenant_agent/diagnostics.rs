//! Fixed, read-only diagnostics for the host-only tenant administration socket.
use super::runner_socket;
use motor_protocol::runner_diagnostics::journal_message;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::Stdio,
};
use tokio::io::AsyncReadExt;

const MAX_PROCESSES: usize = 128;
const MAX_PROC_ENTRIES: usize = 8192;
const MAX_STATE_ENTRIES: usize = 4096;
const MAX_JOURNAL_BYTES: u64 = 32 * 1024;
const MAX_JOURNAL_STDERR_BYTES: usize = 2048;
/// Raw journalctl output read for a time-window query. Entries are filtered
/// and redacted afterwards, so this bounds the guest's work, not the reply.
const MAX_WINDOW_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
/// Entries returned for a time-window query, which bounds the reply size.
const MAX_WINDOW_ENTRIES: usize = 1000;

/// Which part of the tenant runner's journal a diagnostics request returns.
/// Without `since_unix_s` it is the last 64 entries, as before.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct JournalQuery {
    pub since_unix_s: Option<u64>,
    pub until_unix_s: Option<u64>,
    pub jti: Option<uuid::Uuid>,
}

impl JournalQuery {
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--unit=motor-tenant-runner.service".to_owned(),
            "--output=json".to_owned(),
            "--no-pager".to_owned(),
            // Attaching a tenant replaces /etc/machine-id after the base VM boots.
            // Journald still writes under its cached boot ID; the default local
            // journal query would skip that persistent journal directory.
            "--merge".to_owned(),
        ];
        match self.since_unix_s {
            // journalctl reads "@<seconds>" as a Unix time.
            Some(since) => {
                args.push(format!("--since=@{since}"));
                if let Some(until) = self.until_unix_s {
                    args.push(format!("--until=@{until}"));
                }
            }
            None => args.push("--lines=64".to_owned()),
        }
        args
    }
    fn byte_limit(&self) -> u64 {
        if self.since_unix_s.is_some() {
            MAX_WINDOW_JOURNAL_BYTES
        } else {
            MAX_JOURNAL_BYTES
        }
    }
}

pub(super) async fn collect(https: bool, query: JournalQuery) -> io::Result<Value> {
    collect_with_pid(runner_socket::main_pid()?, https, journal(query)).await
}

async fn collect_with_pid(
    pid: Option<u32>,
    https: bool,
    read_journal: impl std::future::Future<Output = io::Result<Value>>,
) -> io::Result<Value> {
    let mut result = if let Some(pid) = pid {
        tokio::task::spawn_blocking(move || {
            snapshot(Path::new("/proc"), Path::new("/tenant/state"), pid)
        })
        .await
        .map_err(io::Error::other)??
    } else {
        // The runner service's journal survives runner exit. Do not make missing live
        // process evidence look like a healthy runner with no descendants.
        json!({"runner_pid":{"unavailable":true},"processes":{"unavailable":true},
            "flocks":{"unavailable":true}})
    };
    result["journal"] = read_journal
        .await
        .unwrap_or_else(|error| json!({"unavailable":true,"error":{
            "type":"journal_read_failed", "detail":error.to_string().chars().take(300).collect::<String>()
        }}));
    if pid.is_none()
        && (result["journal"]["unavailable"] == true
            || !result["journal"]["entries"].is_array()
            || result["journal"]["unrecognized_messages_redacted"] != true)
    {
        return Err(io::Error::other("runner exited and journal is unavailable"));
    }
    result["network"] = if let Some(pid) = pid {
        super::network::collect(pid, https)
            .await
            .unwrap_or_else(|_| json!({"unavailable":true}))
    } else {
        json!({"unavailable":true})
    };
    result["ok"] = json!(true);
    Ok(result)
}

pub(super) async fn attach_result(result: io::Result<()>) -> io::Result<()> {
    attach_result_with(result, journal(JournalQuery::default())).await
}

async fn attach_result_with(
    result: io::Result<()>,
    read_journal: impl std::future::Future<Output = io::Result<Value>>,
) -> io::Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    let message = error.to_string();
    if !message.starts_with("tenant_runner_exited:") {
        return Err(error);
    }
    // A dead runner has no MainPID to inspect. Its unit journal is still
    // available, and must pass the same redactor as normal diagnostics.
    let journal = read_journal.await.ok();
    let tail = journal
        .as_ref()
        .and_then(|value| value["entries"].as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["message"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "[journal unavailable or empty]".into());
    let mut end = message.len().min(512);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = format!("{}\nrunner journal (redacted):\n", &message[..end]);
    let mut start = tail.len().saturating_sub(2048 - prefix.len());
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    Err(io::Error::new(
        error.kind(),
        format!("{prefix}{}", &tail[start..]),
    ))
}

pub(super) fn bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(super) struct Process {
    pid: u32,
    ppid: u32,
    pub(super) state: String,
    start_ticks: u64,
}

pub(super) fn process(pid: u32, text: &str) -> Option<Process> {
    // comm may contain spaces and parentheses; fields after its final ')' have
    // fixed positions in proc_pid_stat, including starttime at field 22.
    let fields: Vec<_> = text.rsplit_once(')')?.1.split_whitespace().collect();
    Some(Process {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        state: fields.first()?.to_string(),
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

fn cmdline(bytes: &[u8]) -> String {
    let parts: Vec<_> = bytes.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
    let executable = parts
        .first()
        .and_then(|p| std::str::from_utf8(p).ok())
        .and_then(|p| Path::new(p).file_name())
        .and_then(|p| p.to_str())
        .unwrap_or("");
    // Even argv[0] is peer-controlled for a running job. Preserve only known
    // program/operation names, never arbitrary arguments, shell scripts or URLs.
    let executable = match executable {
        "motor-runner" | "git" | "cargo" | "rustc" | "rustup" | "setfacl" | "chown" | "chmod"
        | "cp" | "sh" | "bash" | "systemd-run" | "systemctl" | "bun" | "npm" | "node" | "bwrap" => {
            executable
        }
        _ => "[executable redacted]",
    };
    let safe = [
        "fetch",
        "clone",
        "checkout",
        "repack",
        "read-tree",
        "write-tree",
        "rev-parse",
        "cat-file",
        "bundle",
        "list-heads",
        "update-ref",
        "diff-tree",
        "clean",
        "show",
        "config",
        "install",
        "--locked",
        "--offline",
        "--no-optional-locks",
        "--edition=2021",
        "-O",
        "-a",
        "-d",
    ];
    let mut output = executable.to_owned();
    for part in parts.iter().skip(1) {
        output.push(' ');
        match std::str::from_utf8(part).ok().filter(|p| safe.contains(p)) {
            Some(part) => output.push_str(part),
            None => output.push_str("[redacted]"),
        }
        if output.len() >= 200 {
            output.truncate(200);
            break;
        }
    }
    output
}

fn snapshot(proc: &Path, state: &Path, pid: u32) -> io::Result<Value> {
    let uptime: f64 = String::from_utf8_lossy(&bounded(&proc.join("uptime"), 128)?)
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0.0);
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let mut processes = BTreeMap::new();
    let mut scanned = 0;
    for entry in fs::read_dir(proc)?.take(MAX_PROC_ENTRIES + 1).flatten() {
        scanned += 1;
        if scanned > MAX_PROC_ENTRIES {
            break;
        }
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(bytes) = bounded(&entry.path().join("stat"), 4096) {
            if let Some(p) = process(id, &String::from_utf8_lossy(&bytes)) {
                processes.insert(id, p);
            }
        }
    }
    let mut descendants = BTreeSet::from([pid]);
    loop {
        let before = descendants.len();
        for p in processes.values() {
            if descendants.contains(&p.ppid) && descendants.len() <= MAX_PROCESSES {
                descendants.insert(p.pid);
            }
        }
        if descendants.len() == before || descendants.len() > MAX_PROCESSES {
            break;
        }
    }
    let entries: Vec<_> = descendants.iter().take(MAX_PROCESSES).filter_map(|id| {
        let p = processes.get(id)?;
        let directory = proc.join(id.to_string());
        let command = bounded(&directory.join("cmdline"), 4096).unwrap_or_default();
        let wait = bounded(&directory.join("wchan"), 128).unwrap_or_default();
        let wait: String = String::from_utf8_lossy(&wait).chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(80).collect();
        Some(json!({"pid":id,"ppid":p.ppid,"state":p.state,"wchan":wait,
            "cmdline":cmdline(&command),"elapsed_s":(uptime - p.start_ticks as f64 / ticks).max(0.0) as u64}))
    }).collect();
    let (flocks, state_truncated) = flocks(proc, state)?;
    Ok(json!({"runner_pid":pid,"processes":entries,
        "processes_truncated":scanned > MAX_PROC_ENTRIES || descendants.len() > MAX_PROCESSES,
        "flocks":flocks,"flocks_truncated":state_truncated}))
}

fn flocks(proc: &Path, state: &Path) -> io::Result<(Vec<Value>, bool)> {
    let mut pending = vec![(state.to_path_buf(), 0)];
    let mut inodes = BTreeMap::new();
    let mut scanned = 0;
    let mut truncated = false;
    while let Some((path, depth)) = pending.pop() {
        if depth > 16 || scanned >= MAX_STATE_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            scanned += 1;
            if scanned > MAX_STATE_ENTRIES {
                truncated = true;
                break;
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() {
                inodes.insert(
                    (
                        libc::major(metadata.dev()),
                        libc::minor(metadata.dev()),
                        metadata.ino(),
                    ),
                    path,
                );
            }
        }
    }
    let text = bounded(&proc.join("locks"), 128 * 1024)?;
    truncated |= text.len() == 128 * 1024;
    let mut locks = Vec::new();
    for line in String::from_utf8_lossy(&text).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        // A blocked lock has an extra '->' column and is not a held flock.
        if fields.get(1) != Some(&"FLOCK") {
            continue;
        }
        let Some(inode) = fields.get(5) else { continue };
        let parts: Vec<_> = inode.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let key = (
            u32::from_str_radix(parts[0], 16),
            u32::from_str_radix(parts[1], 16),
            parts[2].parse::<u64>(),
        );
        let (Ok(major), Ok(minor), Ok(inode)) = key else {
            continue;
        };
        if let Some(path) = inodes.get(&(major, minor, inode)) {
            // Only paths found below the configured state root are reported;
            // file contents are never read.
            let path = path.strip_prefix(state).unwrap_or(path).to_string_lossy();
            locks.push(
                json!({"pid":fields.get(4).and_then(|v| v.parse::<i32>().ok()),
                "mode":if fields.get(3) == Some(&"WRITE") {"write"} else {"read"},
                "path":path.chars().take(200).collect::<String>()}),
            );
            if locks.len() >= 128 {
                truncated = true;
                break;
            }
        }
    }
    Ok((locks, truncated))
}

async fn journal(query: JournalQuery) -> io::Result<Value> {
    let mut command = tokio::process::Command::new("journalctl");
    command.args(query.args());
    let mut result = journal_command(command, query).await?;
    let service = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::process::Command::new("systemctl")
            .args(["is-active", "systemd-journald.service"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let state = service
        .ok()
        .and_then(Result::ok)
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|state| {
            matches!(
                state.as_str(),
                "active"
                    | "inactive"
                    | "failed"
                    | "activating"
                    | "deactivating"
                    | "reloading"
                    | "unknown"
            )
        });
    result["source"] = journal_source(Path::new("/"), state.as_deref());
    Ok(result)
}

fn journal_source(root: &Path, service_state: Option<&str>) -> Value {
    let machine_id = fs::read_to_string(root.join("etc/machine-id"))
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| valid_machine_id(id));
    let directories: Vec<_> = ["run/log/journal", "var/log/journal"]
        .into_iter()
        .map(|relative| {
            let path = root.join(relative);
            let ids = fs::read_dir(&path).map(|entries| {
                let mut ids: Vec<_> = entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .filter(|id| valid_machine_id(id))
                    .take(64)
                    .collect();
                ids.sort();
                ids
            });
            json!({"path":format!("/{relative}"), "exists":path.is_dir(), "machine_ids":ids.ok()})
        })
        .collect();
    json!({"machine_id":machine_id, "directories":directories, "journald_state":service_state})
}

fn valid_machine_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn journal_command(
    mut command: tokio::process::Command,
    query: JournalQuery,
) -> io::Result<Value> {
    let limit = query.byte_limit();
    let mut child = command
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut bytes = Vec::new();
    let mut stdout = child.stdout.take().unwrap().take(limit);
    let mut stderr = child.stderr.take().unwrap();
    let (status, tail) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        // Drain both pipes together: a warning must not deadlock the stdout read.
        let (_, tail) = tokio::try_join!(
            async {
                stdout.read_to_end(&mut bytes).await?;
                // Stop at the raw byte cap without waiting for stderr's EOF.
                if bytes.len() == limit as usize {
                    child.start_kill()?;
                }
                Ok::<_, io::Error>(())
            },
            async {
                let mut tail = Vec::new();
                let mut chunk = [0; 1024];
                loop {
                    let size = stderr.read(&mut chunk).await?;
                    if size == 0 {
                        break;
                    }
                    tail.extend_from_slice(&chunk[..size]);
                    if tail.len() > MAX_JOURNAL_STDERR_BYTES {
                        tail.drain(..tail.len() - MAX_JOURNAL_STDERR_BYTES);
                    }
                }
                Ok::<_, io::Error>(tail)
            }
        )?;
        Ok::<_, io::Error>((child.wait().await?, tail))
    })
    .await
    .map_err(io::Error::other)??;
    // A warning can mean the caller sees only part of the journal even when
    // journalctl exits successfully. Never report that as a clean empty query.
    // Reaching our own byte cap deliberately kills the process, not a read failure.
    let killed_at_limit = bytes.len() == limit as usize && status.signal() == Some(libc::SIGKILL);
    if !tail.is_empty() || (!status.success() && !killed_at_limit) {
        return Ok(json!({"unavailable":true,"error":{
            "type":"journal_command_failed", "exit_status":status.code(),
            "stderr_tail":String::from_utf8_lossy(&tail)
        }}));
    }
    let mut result = journal_entries(&bytes, limit, &query);
    result["command"] = json!({"exit_status":status.code(), "stderr_tail":""});
    Ok(result)
}

/// Turns journalctl's JSON lines into redacted entries. A job filter matches
/// the job id anywhere in the raw message, so lines that name the job in any
/// field are kept; only the redacted form leaves the guest.
fn journal_entries(bytes: &[u8], limit: u64, query: &JournalQuery) -> Value {
    let jti = query.jti.map(|jti| jti.to_string());
    let mut more = false;
    let mut entries = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(timestamp) = value["__REALTIME_TIMESTAMP"]
            .as_str()
            .and_then(|t| t.parse::<u64>().ok())
        else {
            continue;
        };
        let message = value["MESSAGE"].as_str().unwrap_or("");
        if jti.as_deref().is_some_and(|jti| !message.contains(jti)) {
            continue;
        }
        if query.since_unix_s.is_some() && entries.len() == MAX_WINDOW_ENTRIES {
            more = true;
            break;
        }
        entries.push(json!({"unix_us":timestamp,"message":journal_message(message)}));
    }
    json!({"entries":entries,"truncated":more || bytes.len() == limit as usize,
        "unrecognized_messages_redacted":true})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_redactor_preserves_early_close_journal_output() {
        let messages = [
            ("[motor-runner] WARN motor_runner::server: runner_channel_closed reason=eof bytes=42 error=SECRET", "runner_channel_closed reason=eof bytes=42"),
            ("Error: immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/git; SECRET", "immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/git"),
            ("thread 'main' panicked at crates/motor-runner/src/tenant.rs:1:2:", "[message omitted]"),
            ("called `Result::unwrap()` on an `Err` value: SECRET", "[message omitted]"),
            ("job stdout SECRET", "[message omitted]"),
        ];
        let bytes = messages
            .iter()
            .enumerate()
            .map(|(index, (message, _))| {
                json!({"__REALTIME_TIMESTAMP":index.to_string(),"MESSAGE":message}).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reply = journal_entries(
            bytes.as_bytes(),
            MAX_JOURNAL_BYTES,
            &JournalQuery::default(),
        );
        let actual: Vec<_> = reply["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["message"].as_str().unwrap())
            .collect();
        assert_eq!(actual, messages.map(|(_, expected)| expected));
    }

    #[test]
    fn journal_query_reads_the_boot_machine_id_after_tenant_attachment() {
        for query in [
            JournalQuery::default(),
            JournalQuery {
                since_unix_s: Some(1),
                ..Default::default()
            },
        ] {
            let args = query.args();
            assert!(args.iter().any(|arg| arg == "--merge"), "{args:?}");
            assert_eq!(args[0], "--unit=motor-tenant-runner.service");
        }
    }

    #[test]
    fn journal_source_reports_split_machine_ids_without_arbitrary_filenames() {
        let root = cortexkit_test_support::ScratchDir::new("journal-machine-ids");
        let boot_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let tenant_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/machine-id"), format!("{tenant_id}\n")).unwrap();
        fs::create_dir_all(root.join(format!("var/log/journal/{boot_id}"))).unwrap();
        fs::create_dir_all(root.join("var/log/journal/TOP_SECRET")).unwrap();
        let source = journal_source(&root, Some("active"));
        assert_eq!(source["machine_id"], tenant_id);
        assert_eq!(source["journald_state"], "active");
        assert_eq!(source["directories"][0]["exists"], false);
        assert_eq!(source["directories"][1]["machine_ids"], json!([boot_id]));
        assert!(!source.to_string().contains("TOP_SECRET"));
    }

    #[tokio::test]
    async fn exited_runner_diagnostics_returns_journal_not_fake_live_evidence() {
        let raw = json!({"__REALTIME_TIMESTAMP":"1800000000000000", "MESSAGE":
            "motor_runner::server: runner_channel_closed direction=read reason=lease_expired error=SECRET"})
            .to_string();
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf '%s\\n' \"$1\"", "journal-fixture", &raw]);
        let reply = collect_with_pid(
            None,
            false,
            journal_command(command, JournalQuery::default()),
        )
        .await
        .unwrap();
        assert_eq!(reply["ok"], true);
        for field in ["runner_pid", "processes", "flocks", "network"] {
            assert_eq!(reply[field], json!({"unavailable":true}), "{field}");
        }
        assert_eq!(reply["journal"]["unrecognized_messages_redacted"], true);
        assert_eq!(
            reply["journal"]["entries"][0]["message"],
            "runner_channel_closed direction=read reason=lease_expired"
        );
        assert!(!reply.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn exited_runner_diagnostics_does_not_claim_success_without_a_journal() {
        for read in [
            Err(io::Error::other("SECRET")),
            Ok(json!({"unavailable":true,"error":{"stderr_tail":"SECRET"}})),
        ] {
            let error = collect_with_pid(None, false, async { read })
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "runner exited and journal is unavailable"
            );
        }
    }

    #[tokio::test]
    async fn attach_exit_returns_bounded_redacted_journal_without_a_live_pid() {
        let line = |message: &str| {
            json!({"__REALTIME_TIMESTAMP":"1800000000000000", "MESSAGE":message}).to_string()
        };
        let mut lines = vec![line("motor_runner::setup_log: materialize started")];
        lines.extend((0..60).map(|_| line("motor_runner::server: runner_channel_closed direction=read reason=eof kind=ConnectionReset bytes=12345")));
        lines.push(line("untrusted command TOP_SECRET"));
        lines.push(line("Error: immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/registry; require ACLs TOP_SECRET"));
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "printf '%s\\n' \"$1\"",
            "journal-fixture",
            &lines.join("\n"),
        ]);
        let result = attach_result_with(
            Err(io::Error::other(
                "tenant_runner_exited: no MainPID after start",
            )),
            journal_command(command, JournalQuery::default()),
        )
        .await;
        let reply = super::super::agent_result(result.map(|()| json!({"ok":true})));
        let error = reply["error"].as_str().unwrap();
        assert_eq!(reply["ok"], false);
        assert!(error.starts_with("tenant_runner_exited:"));
        assert!(error.len() <= 2048);
        assert!(error.ends_with(
            "immutable Cargo source ACL check failed: /usr/local/lib/motor/cargo/registry"
        ));
        assert!(!error.contains("materialize started"));
        assert!(!error.contains("TOP_SECRET"));
        assert_eq!(
            journal_message(
                "Error: immutable Cargo source ACL check failed: /tenant/TOP_SECRET; bad"
            ),
            "[message omitted]"
        );
    }

    #[tokio::test]
    async fn attach_journal_failure_does_not_expose_subprocess_stderr() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf TOP_SECRET >&2; exit 7"]);
        let result = attach_result_with(
            Err(io::Error::other("tenant_runner_exited: runner stopped")),
            journal_command(command, JournalQuery::default()),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(result.contains("journal unavailable or empty"));
        assert!(!result.contains("TOP_SECRET"));
    }

    #[tokio::test]
    async fn attach_only_queries_the_journal_for_runner_exit_errors() {
        for result in [Ok(()), Err(io::Error::other("workspace mount failed"))] {
            let read = std::cell::Cell::new(false);
            let result = attach_result_with(result, async {
                read.set(true);
                Ok(json!({"entries":[]}))
            })
            .await;
            assert!(!read.get());
            if let Err(error) = result {
                assert_eq!(error.to_string(), "workspace mount failed");
            }
        }
    }

    #[tokio::test]
    async fn journal_command_failure_reports_status_and_bounded_stderr_not_empty_entries() {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "printf 'Permission denied reading journal\\n' >&2; exit 7",
        ]);
        let reply = journal_command(command, JournalQuery::default())
            .await
            .unwrap();
        assert_eq!(reply["unavailable"], true, "{reply}");
        assert_eq!(reply["error"]["type"], "journal_command_failed");
        assert_eq!(reply["error"]["exit_status"], 7);
        assert!(reply["error"]["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("Permission denied"));
        assert!(reply.get("entries").is_none());
    }

    #[tokio::test]
    async fn journal_success_with_unreadable_warning_is_not_a_clean_empty_journal() {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "printf 'No journal files were opened due to insufficient permissions.\\n' >&2",
        ]);
        let reply = journal_command(command, JournalQuery::default())
            .await
            .unwrap();
        assert_eq!(reply["unavailable"], true, "{reply}");
        assert_eq!(reply["error"]["exit_status"], 0);
        assert!(reply.get("entries").is_none());
    }

    #[tokio::test]
    async fn journal_stderr_tail_is_bounded_and_retains_the_end() {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "printf '%5000s' x >&2; printf 'journal access denied' >&2; exit 9",
        ]);
        let reply = journal_command(command, JournalQuery::default())
            .await
            .unwrap();
        let tail = reply["error"]["stderr_tail"].as_str().unwrap();
        assert_eq!(reply["error"]["exit_status"], 9);
        assert_eq!(tail.len(), MAX_JOURNAL_STDERR_BYTES);
        assert!(tail.ends_with("journal access denied"));
    }

    #[tokio::test]
    async fn journal_raw_byte_cap_remains_truncation_not_command_failure() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf '%40000s' x"]);
        let reply = journal_command(command, JournalQuery::default())
            .await
            .unwrap();
        assert_eq!(reply["truncated"], true, "{reply}");
        assert!(reply.get("unavailable").is_none());
    }

    #[tokio::test]
    async fn journal_command_keeps_recognised_job_lines_and_still_omits_unrecognised_text() {
        let job = uuid::Uuid::parse_str("018d0d3b-a338-7181-8fcc-88e901ef438c").unwrap();
        let known = json!({"__REALTIME_TIMESTAMP":"1800000000000000", "MESSAGE":format!(
            "[motor-runner] INFO motor_runner::server: tenant_job_stream_ended jti={job} reason=error error=TOP_SECRET")});
        let unknown = json!({"__REALTIME_TIMESTAMP":"1800000000000001", "MESSAGE":format!(
            "[motor-runner] INFO motor_runner::server: unapproved message jti={job} TOP_SECRET")});
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "printf '%s\\n' \"$1\" \"$2\"",
            "journal-fixture",
            &known.to_string(),
            &unknown.to_string(),
        ]);
        let reply = journal_command(
            command,
            JournalQuery {
                jti: Some(job),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            reply["entries"][0]["message"],
            format!("tenant_job_stream_ended jti={job} reason=error")
        );
        assert_eq!(reply["entries"][1]["message"], "[message omitted]");
        assert_eq!(reply["command"], json!({"exit_status":0,"stderr_tail":""}));
        assert!(!reply.to_string().contains("TOP_SECRET"));
    }

    #[test]
    fn diagnostics_redact_command_arguments_and_untrusted_journal_text() {
        let command = cmdline(b"/usr/bin/cargo\0fetch\0--locked\0--token\0TOP_SECRET\0");
        assert!(command.starts_with("cargo fetch --locked"));
        assert!(!command.contains("TOP_SECRET"));
        assert!(!cmdline(b"/secret/TOP_SECRET\0").contains("TOP_SECRET"));
        assert_eq!(journal_message("password=TOP_SECRET"), "[message omitted]");
        let message = journal_message("[runner] INFO motor_runner::setup_log: setup lock wait started job_id=018d0d3b-a338-7181-8fcc-88e901ef438c lock=build_wrapper secret=TOP_SECRET");
        assert!(message.contains("lock=build_wrapper"));
        assert!(!message.contains("TOP_SECRET"));
        assert!(cmdline(&b"cargo\0fetch\0".repeat(200)).len() <= 200);
    }

    #[test]
    fn journal_keeps_deadline_and_maintenance_timings_without_free_text() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        assert_eq!(
            journal_message(&format!("[runner] WARN motor_runner::engine: runner deadline signal was late job_id={job} lateness_ms=101000 error=TOP_SECRET")),
            format!("runner deadline signal was late job_id={job} lateness_ms=101000")
        );
        let fields = "elapsed_ms=101000 slice_budget_ms=1 source_unseals_ms=2 runtime_cleanup_ms=3 state_ms=4 expired_jobs_ms=99000 returned_files_ms=5 return_reservations_ms=6 build_folders_ms=7 sibling_snapshots_ms=8 prune_refs_ms=9 retire_repositories_ms=10";
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::engine: runner maintenance pass exceeded ten seconds {fields} secret=TOP_SECRET")),
            format!("runner maintenance pass exceeded ten seconds {fields}")
        );
        assert_eq!(
            journal_message("[runner] WARN motor_runner::engine: runner deadline signal was late job_id=TOP_SECRET lateness_ms=-1"),
            "runner deadline signal was late"
        );
        for key in fields
            .split_whitespace()
            .map(|field| field.split_once('=').unwrap().0)
        {
            assert_eq!(
                journal_message(&format!("[runner] INFO motor_runner::engine: runner maintenance pass exceeded ten seconds {key}=TOP_SECRET")),
                "runner maintenance pass exceeded ten seconds"
            );
        }
    }

    /// The lines that say why a hosted job stream closed keep their labels,
    /// counters and job ids; a field that is not a known label is dropped.
    #[test]
    fn journal_keeps_job_stream_close_reasons_without_free_text() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        let closed = journal_message(&format!("[motor-runner] INFO motor_runner::server: runner_channel_closed jti=Some({job}) direction=read reason=read_error error_kind=Some(InvalidData) frames=12 frame_type=heartbeat seq=None elapsed_ms=4100"));
        assert_eq!(closed, format!("runner_channel_closed jti=Some({job}) direction=read reason=read_error error_kind=Some(InvalidData) frames=12 frame_type=heartbeat seq=None elapsed_ms=4100"));
        let refused = journal_message(&format!("[motor-runner] WARN motor_runner::server: runner job stream refused frame job_id={job} frame=bundle_chunk"));
        assert_eq!(
            refused,
            format!("runner job stream refused frame job_id={job} frame=bundle_chunk")
        );
        let ended = journal_message(&format!("[motor-runner] INFO motor_runner::server: tenant_job_stream_ended jti={job} reason=error error_kind=Some(InvalidData)"));
        assert!(
            ended.ends_with("reason=error error_kind=Some(InvalidData)"),
            "{ended}"
        );
        let quoted = journal_message("[motor-runner] WARN motor_runner::server: operation_refused jti=x frame=TOP-SECRET! reason=TOP_SECRET_LABEL_THAT_IS_FAR_TOO_LONG_TO_BE_A_LABEL");
        assert_eq!(quoted, "operation_refused");
    }

    #[test]
    fn journal_writer_lifecycle_stops_at_path_and_debug_values() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::engine: writer lifecycle job_id={job} unit=motor-job-{job}.service kind=base_build event=finished started_ms=123 age_ms=456 outcome=Some(Exit {{ code: 0 }}) kill=Some((Deadline, 789)) repository=/secret elapsed_ms=123")),
            format!("writer lifecycle job_id={job} unit=motor-job-{job}.service kind=base_build event=finished started_ms=123 age_ms=456 outcome=Some(Exit {{ code: 0 }}) kill=Some((Deadline, 789))")
        );
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::engine: writer lifecycle job_id={job} repository=/secret elapsed_ms=123 unit=other kind=base_build outcome=Some(Exit {{ code: 0 }}) kill=None")),
            format!("writer lifecycle job_id={job}")
        );
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::engine: writer lifecycle job_id={job} outcome=Some(Exit {{ elapsed_ms=123 }}) kill=Some(Deadline)")),
            format!("writer lifecycle job_id={job}")
        );
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::engine: writer lifecycle job_id={job} outcome=Some(Unknown {{ kind: \"secret elapsed_ms=999 kill=None repository=/secret\" }}) kill=None repository=/secret")),
            format!("writer lifecycle job_id={job}")
        );
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::engine: writer lifecycle job_id={job} unit=/secret elapsed_ms=999 outcome=None kill=None repository=/secret")),
            format!("writer lifecycle job_id={job}")
        );
    }

    #[test]
    fn journal_seed_record_mismatch_keeps_booleans_not_workspace() {
        assert_eq!(
            journal_message("[runner] INFO motor_runner::seed: build base seed refused: record mismatch inputs_match=false times_valid=true workspace=/secret elapsed_ms=123"),
            "build base seed refused: record mismatch inputs_match=false times_valid=true"
        );
    }

    #[test]
    fn journal_seed_failure_stops_at_error() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        assert_eq!(
            journal_message(&format!("[runner] WARN motor_runner::seed: build base seed failed; building without seed job_id={job} error=/secret elapsed_ms=123")),
            format!("build base seed failed; building without seed job_id={job}")
        );
    }

    #[test]
    fn journal_seed_environment_change_stops_at_inputs() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        assert_eq!(
            journal_message(&format!("[runner] WARN motor_runner::seed: build base seed environment changed during sandbox preparation; building without seed job_id={job} inputs=[\"/secret elapsed_ms=123\"]")),
            format!("build base seed environment changed during sandbox preparation; building without seed job_id={job}")
        );
    }

    #[test]
    fn journal_seed_success_keeps_job_and_duration_not_path() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::seed: build base seed succeeded job_id={job} elapsed_ms=42 workspace=/secret elapsed_ms=123")),
            format!("build base seed succeeded job_id={job} elapsed_ms=42")
        );
    }

    #[test]
    fn journal_seed_skip_keeps_only_closed_reasons() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        for reason in [
            "no_base",
            "unreadable_record",
            "siblings",
            "base_job",
            "not_fixed_path",
            "not_isolated",
            "target_not_empty",
            "workspace_not_tracked",
            "record_mismatch",
            "symlink_change",
            "seed_failed",
        ] {
            assert_eq!(journal_message(&format!("[runner] INFO motor_runner::seed: build base seed skipped job_id={job} reason={reason} workspace=/secret elapsed_ms=123")), format!("build base seed skipped job_id={job} reason={reason}"));
        }
        assert_eq!(journal_message(&format!("[runner] INFO motor_runner::seed: build base seed skipped job_id={job} reason=SECRET_LABEL workspace=/secret")), format!("build base seed skipped job_id={job}"));
    }

    #[test]
    fn journal_base_publication_keeps_tip_and_duration_not_path() {
        let job = "018d0d3b-a338-7181-8fcc-88e901ef438c";
        let tip = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            journal_message(&format!("[runner] INFO motor_runner::build_base: build base published job_id={job} elapsed_ms=42 tip={tip} repository=/secret elapsed_ms=123")),
            format!("build base published job_id={job} elapsed_ms=42 tip={tip}")
        );
    }

    #[test]
    fn journal_omits_unlisted_build_base_variant() {
        assert_eq!(
            journal_message("[runner] INFO motor_runner::seed: build base seed refused: another reason workspace=/secret"),
            "[message omitted]"
        );
        assert_eq!(
            journal_message("[runner] INFO motor_runner::seed: build base seed succeeded_unexpected workspace=/secret"),
            "[message omitted]"
        );
        assert_eq!(
            journal_message("[runner] INFO motor_runner::seed: build base seed succeeded unexpectedly workspace=/secret"),
            "[message omitted]"
        );
    }

    #[test]
    fn journal_window_query_selects_by_time_and_job_and_bounds_entries() {
        let job = uuid::Uuid::parse_str("018d0d3b-a338-7181-8fcc-88e901ef438c").unwrap();
        let window = JournalQuery {
            since_unix_s: Some(1_800_000_000),
            until_unix_s: Some(1_800_000_600),
            jti: Some(job),
        };
        assert_eq!(
            window.args()[4..],
            ["--since=@1800000000", "--until=@1800000600"]
        );
        assert_eq!(JournalQuery::default().args()[4], "--lines=64");
        let line = |message: String| {
            json!({"__REALTIME_TIMESTAMP":"1800000001000000","MESSAGE":message}).to_string()
        };
        let mut raw = vec![
            line(format!("[motor-runner] INFO motor_runner::server: runner_channel_closed jti=Some({job}) direction=read reason=eof")),
            line("[motor-runner] INFO motor_runner::server: runner_channel_closed jti=Some(00000000-0000-7000-8000-000000000001) direction=read reason=eof".into()),
            "not json".into(),
        ];
        let value = journal_entries(raw.join("\n").as_bytes(), window.byte_limit(), &window);
        let entries = value["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["unix_us"], 1_800_000_001_000_000u64);
        assert_eq!(value["truncated"], false);
        raw = (0..MAX_WINDOW_ENTRIES + 1)
            .map(|_| {
                line(format!(
                    "motor_runner::server: runner_channel_closed jti={job}"
                ))
            })
            .collect();
        let value = journal_entries(raw.join("\n").as_bytes(), window.byte_limit(), &window);
        assert_eq!(
            value["entries"].as_array().unwrap().len(),
            MAX_WINDOW_ENTRIES
        );
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn diagnostics_select_runner_descendants_and_state_flocks_with_bounds() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-diagnostics");
        let proc = root.join("proc");
        let state = root.join("state");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&proc).unwrap();
        fs::write(proc.join("uptime"), "1000.0 1000.0").unwrap();
        for (pid, ppid, name) in [
            (100, 1, "motor-runner"),
            (101, 100, "cargo"),
            (102, 101, "git"),
            (103, 1, "bash"),
        ] {
            let directory = proc.join(pid.to_string());
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("stat"),
                format!("{pid} (name (with) spaces) S {ppid} {} 0", "0 ".repeat(17)),
            )
            .unwrap();
            fs::write(directory.join("cmdline"), format!("{name}\0SECRET\0")).unwrap();
            fs::write(directory.join("wchan"), "locks_lock_inode_wait").unwrap();
        }
        let lockfile = state.join("configure.lock");
        fs::write(&lockfile, "secret contents").unwrap();
        let m = fs::metadata(&lockfile).unwrap();
        fs::write(proc.join("locks"), format!("1: FLOCK ADVISORY WRITE 101 {:02x}:{:02x}:{} 0 EOF\n2: -> FLOCK ADVISORY WRITE 102 00:00:0 0 EOF\n3: FLOCK ADVISORY WRITE 103 00:00:1 0 EOF\n", libc::major(m.dev()), libc::minor(m.dev()), m.ino())).unwrap();
        let value = snapshot(&proc, &state, 100).unwrap();
        let pids: Vec<_> = value["processes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["pid"].as_u64().unwrap())
            .collect();
        assert_eq!(pids, [100, 101, 102]);
        assert_eq!(value["flocks"].as_array().unwrap().len(), 1);
        assert_eq!(value["flocks"][0]["pid"], 101);
        assert_eq!(value["flocks"][0]["path"], "configure.lock");
        assert!(!value.to_string().contains("SECRET"));
        assert_eq!(value["processes_truncated"], false);
    }
}
