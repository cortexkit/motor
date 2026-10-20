//! Fixed, read-only diagnostics for the host-only tenant administration socket.
use super::runner_socket;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::Path,
    process::Stdio,
};
use tokio::io::AsyncReadExt;

const MAX_PROCESSES: usize = 128;
const MAX_PROC_ENTRIES: usize = 8192;
const MAX_STATE_ENTRIES: usize = 4096;
const MAX_JOURNAL_BYTES: u64 = 32 * 1024;

pub(super) async fn collect() -> io::Result<Value> {
    let pid = runner_socket::main_pid()?
        .ok_or_else(|| io::Error::other("tenant runner has no current MainPID"))?;
    let mut result = tokio::task::spawn_blocking(move || {
        snapshot(Path::new("/proc"), Path::new("/tenant/state"), pid)
    })
    .await
    .map_err(io::Error::other)??;
    result["journal"] = journal()
        .await
        .unwrap_or_else(|_| json!({"unavailable":true}));
    result["ok"] = json!(true);
    Ok(result)
}

fn bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

struct Process {
    pid: u32,
    ppid: u32,
    state: String,
    start_ticks: u64,
}

fn process(pid: u32, text: &str) -> Option<Process> {
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

fn journal_message(message: &str) -> String {
    // Journal entries and subprocess errors can quote uploaded text. Only the
    // runner's fixed setup messages and numeric/allowlisted fields cross vsock.
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
    ];
    let Some(name) = names.iter().find(|name| rest.starts_with(**name)) else {
        return "[message omitted]".into();
    };
    let mut output = name.to_string();
    for token in rest[name.len()..].split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        let valid = match key {
            "job_id" => uuid::Uuid::parse_str(value).is_ok(),
            "cache_fill" | "success" => matches!(value, "true" | "false"),
            "lock" => matches!(
                value.trim_matches('"'),
                "tree_access" | "dependency_mirror" | "build_wrapper"
            ),
            "elapsed_ms" | "wait_ms" | "bytes" | "requested_cpus" => value.parse::<u64>().is_ok(),
            _ => false,
        };
        if valid {
            output.push(' ');
            output.push_str(token);
        }
    }
    output
}

async fn journal() -> io::Result<Value> {
    let mut child = tokio::process::Command::new("journalctl")
        .args([
            "--unit=motor-tenant-runner.service",
            "--lines=64",
            "--output=json",
            "--no-pager",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut bytes = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        child
            .stdout
            .take()
            .unwrap()
            .take(MAX_JOURNAL_BYTES)
            .read_to_end(&mut bytes)
            .await?;
        // A full buffer may have stopped in the middle of a journal entry.
        if bytes.len() == MAX_JOURNAL_BYTES as usize {
            child.kill().await?;
        }
        child.wait().await
    })
    .await
    .map_err(io::Error::other)??;
    let entries: Vec<_> = bytes.split(|b| *b == b'\n').filter_map(|line| {
        let value: Value = serde_json::from_slice(line).ok()?;
        let timestamp = value["__REALTIME_TIMESTAMP"].as_str()?.parse::<u64>().ok()?;
        Some(json!({"unix_us":timestamp,"message":journal_message(value["MESSAGE"].as_str().unwrap_or(""))}))
    }).collect();
    Ok(
        json!({"entries":entries,"truncated":bytes.len() == MAX_JOURNAL_BYTES as usize,
        "unrecognized_messages_redacted":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
