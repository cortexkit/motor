//! Read-only, bounded evidence for trusted Cargo cache contention.
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::Path,
    process::Command,
};

const PROC_LIMIT: u64 = 256 * 1024;
const MAX_HOLDERS: usize = 4;

pub(super) fn diagnose(
    command: &mut Command,
    run: impl FnOnce(&mut Command) -> io::Result<()>,
) -> io::Result<()> {
    let result = run(command);
    let Err(error) = result else { return Ok(()) };
    let detail = error.to_string();
    if error.kind() != io::ErrorKind::TimedOut
        && !crate::sibling_snapshots::helper::UnsettledHelper::is(&error)
        && !(detail.contains("Blocking waiting for file lock") && detail.contains("package cache"))
    {
        return Err(error);
    }
    let cargo = command
        .get_envs()
        .find_map(|(key, value)| (key == "CARGO_HOME").then_some(value).flatten());
    let summary = cargo
        .map(Path::new)
        .map(|cargo| snapshot(cargo, Path::new("/proc")))
        .unwrap_or_else(|| "package cache lock holder unknown (no Cargo home)".into());
    tracing::warn!(%summary, "trusted Cargo fetch failed");
    // Keep the fatal cleanup class through diagnostic enrichment and fallback.
    if let Some(unsettled) = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<crate::sibling_snapshots::helper::UnsettledHelper>())
    {
        return Err(io::Error::other(
            crate::sibling_snapshots::helper::UnsettledHelper {
                unit: unsettled.unit.clone(),
                detail: format!("{summary}; {}", unsettled.detail),
            },
        ));
    }
    Err(io::Error::new(error.kind(), format!("{summary}; {error}")))
}

fn read(path: &Path, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Lock {
    major: u64,
    minor: u64,
    inode: u64,
}

fn owners(table: &str, lock: Lock) -> Vec<u32> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().take(9).collect();
            // Rows marked '->' are blocked contenders, not owners. Open-file-
            // description locks have PID -1, so cannot identify an owning process.
            if fields.len() < 6 || fields[1] == "->" {
                return None;
            }
            let mut identity = fields[5].split(':');
            let found = Lock {
                major: u64::from_str_radix(identity.next()?, 16).ok()?,
                minor: u64::from_str_radix(identity.next()?, 16).ok()?,
                inode: identity.next()?.parse().ok()?,
            };
            (found == lock)
                .then(|| fields[4].parse::<u32>().ok())
                .flatten()
                .filter(|pid| *pid > 0)
        })
        .take(MAX_HOLDERS)
        .collect()
}

fn start_ticks(stat: &str) -> Option<u64> {
    // The process-name field in /proc/[pid]/stat can contain spaces and ')';
    // fields after its last ')' begin with the process state.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn safe_text(text: &str, limit: usize) -> String {
    let mut output = String::new();
    for word in text
        .split(|c: char| c.is_whitespace() || c == '\0')
        .filter(|s| !s.is_empty())
    {
        let mut word = word.to_string();
        if let Some(scheme) = word.find("://") {
            // Redact the whole URL, including a userinfo or query cut off by
            // the read limit before its delimiter could be observed.
            word.replace_range(scheme + 3.., "[redacted]");
        }
        if !output.is_empty() {
            output.push(' ');
        }
        output.extend(word.chars().filter(|c| !c.is_control()));
    }
    if output.chars().count() > limit {
        output = output.chars().take(limit).collect();
        output.push('…');
    }
    output
}

fn process(
    pid: u32,
    ticks_per_second: u64,
    uptime: u64,
    boot: Option<u64>,
    mut read: impl FnMut(&str) -> io::Result<String>,
) -> Option<String> {
    let start = start_ticks(&read("stat").ok()?)?;
    let cmdline = safe_text(&read("cmdline").ok()?, 240);
    let cgroup = safe_text(&read("cgroup").ok()?, 160);
    // Never attribute a stale /proc/locks PID to a replacement process.
    if start_ticks(&read("stat").ok()?)? != start {
        return None;
    }
    let start_s = start.checked_div(ticks_per_second)?;
    let age = uptime.saturating_sub(start_s);
    let timestamp = boot
        .map(|boot| format!(", started_unix_s={}", boot.saturating_add(start_s)))
        .unwrap_or_default();
    Some(format!(
        "pid {pid} ({cmdline}) in {cgroup}, process age {age} s (start_ticks={start}{timestamp})"
    ))
}

fn snapshot(cargo: &Path, proc: &Path) -> String {
    let unknown = "package cache lock holder unknown";
    let Ok(table) = read(&proc.join("locks"), PROC_LIMIT) else {
        return unknown.into();
    };
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        return unknown.into();
    }
    let uptime = read(&proc.join("uptime"), 256)
        .ok()
        .and_then(|s| s.split('.').next()?.parse().ok())
        .unwrap_or(0);
    let boot = read(&proc.join("stat"), PROC_LIMIT).ok().and_then(|s| {
        s.lines()
            .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
    });
    let mut holders = Vec::new();
    for name in [".package-cache", ".package-cache-mutate"] {
        let Ok(meta) = fs::metadata(cargo.join(name)) else {
            continue;
        };
        let lock = Lock {
            major: libc::major(meta.dev()) as u64,
            minor: libc::minor(meta.dev()) as u64,
            inode: meta.ino(),
        };
        for pid in owners(&table, lock) {
            if holders.len() == MAX_HOLDERS {
                break;
            }
            if let Some(detail) = process(pid, ticks as u64, uptime, boot, |file| {
                read(&proc.join(pid.to_string()).join(file), 4096)
            }) {
                // Recheck ownership after reading identity, not only before it.
                if read(&proc.join("locks"), PROC_LIMIT)
                    .is_ok_and(|now| owners(&now, lock).contains(&pid))
                {
                    holders.push(format!(
                        "{name} dev={:x}:{:x} inode={}: {detail}",
                        lock.major, lock.minor, lock.inode
                    ));
                }
            }
        }
    }
    if holders.is_empty() {
        unknown.into()
    } else {
        format!("package cache lock held by {}", holders.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn real_lock_owner_matches_inode_through_an_alias() {
        let root = cortexkit_test_support::ScratchDir::new("cargo-lock-owner");
        let home = root.join("cargo");
        fs::create_dir(&home).unwrap();
        let file = fs::File::create(root.join("actual-lock")).unwrap();
        fs::hard_link(root.join("actual-lock"), home.join(".package-cache")).unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let summary = snapshot(&home, Path::new("/proc"));
        assert!(
            summary.contains(&format!("pid {} (", std::process::id())),
            "{summary}"
        );
        assert!(
            summary.contains(&format!("inode={}", file.metadata().unwrap().ino())),
            "{summary}"
        );
        assert!(summary.contains("start_ticks="), "{summary}");
        assert!(summary.contains("started_unix_s="), "{summary}");
        assert!(summary.contains("process age"), "{summary}");
    }

    #[test]
    fn lock_table_excludes_waiters_other_inodes_and_unknown_ofd_owners() {
        let lock = Lock {
            major: 8,
            minor: 17,
            inode: 99,
        };
        let table = "1: FLOCK ADVISORY WRITE 123 08:11:99 0 EOF\n2: -> FLOCK ADVISORY WRITE 456 08:11:99 0 EOF\n3: POSIX ADVISORY READ 789 08:11:99 0 EOF\n4: OFDLCK ADVISORY WRITE -1 08:11:99 0 EOF\n5: FLOCK ADVISORY WRITE 321 08:11:100 0 EOF\n6: FLOCK ADVISORY WRITE 654 09:11:99 0 EOF\n";
        assert_eq!(owners(table, lock), [123, 789]);
    }

    fn stat(ticks: u64) -> String {
        format!(
            "123 (cargo (fetch)) S {} {ticks} 0",
            vec!["0"; 18].join(" ")
        )
    }

    #[test]
    fn identity_recheck_rejects_pid_reuse_or_exit() {
        for second in [Ok(stat(201)), Err(io::Error::from(io::ErrorKind::NotFound))] {
            let mut stats = vec![Ok(stat(200)), second].into_iter();
            assert!(process(123, 100, 10, Some(1000), |file| match file {
                "stat" => stats.next().unwrap(),
                "cmdline" => Ok("cargo\0fetch".into()),
                "cgroup" => Ok("0::/motor-fill-example.service".into()),
                _ => unreachable!(),
            })
            .is_none());
        }
        let detail = process(123, 100, 10, Some(1000), |file| {
            Ok(match file {
                "stat" => stat(200),
                "cmdline" => "cargo\0fetch".into(),
                "cgroup" => "0::/motor-fill-example.service".into(),
                _ => unreachable!(),
            })
        })
        .unwrap();
        assert!(detail.contains("motor-fill-example.service"));
        assert!(detail.contains("process age 8 s"));
        assert!(detail.contains("started_unix_s=1002"));
    }

    #[test]
    fn credentials_are_redacted_before_truncation_even_for_partial_urls() {
        let command =
            "cargo\0fetch\0https://user:secret@host/repo?token=secret\0https://user:partial";
        let redacted = safe_text(command, 240);
        assert!(!redacted.contains("secret"), "{redacted}");
        assert!(!redacted.contains("user"), "{redacted}");
        assert!(!redacted.contains("partial"), "{redacted}");
        let long = safe_text(&"界".repeat(1000), 240);
        assert_eq!(long.chars().count(), 241);
        assert!(!safe_text(command, 30).contains("secret"));
    }

    #[test]
    fn diagnostics_are_best_effort_and_only_enrich_stalls_or_lock_failures() {
        let root = cortexkit_test_support::ScratchDir::new("cargo-lock-unavailable");
        assert_eq!(
            snapshot(&root, &root.join("absent-proc")),
            "package cache lock holder unknown"
        );
        let mut command = Command::new("cargo");
        command.env("CARGO_HOME", &*root);
        for (kind, detail) in [
            (io::ErrorKind::TimedOut, "deadline"),
            (
                io::ErrorKind::Other,
                "Blocking waiting for file lock on package cache",
            ),
        ] {
            let error = diagnose(&mut command, |_| Err(io::Error::new(kind, detail))).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(error
                .to_string()
                .starts_with("package cache lock holder unknown;"));
        }
        assert_eq!(
            diagnose(&mut command, |_| Err(io::Error::other("fetch miss")))
                .unwrap_err()
                .to_string(),
            "fetch miss"
        );
        assert!(diagnose(&mut command, |_| Ok(())).is_ok());
        let error = diagnose(&mut command, |_| {
            Err(io::Error::other(
                crate::sibling_snapshots::helper::UnsettledHelper {
                    unit: "motor-snaphelper-unsettled.service".into(),
                    detail: "still live".into(),
                },
            ))
        })
        .unwrap_err();
        assert!(crate::sibling_snapshots::helper::UnsettledHelper::is(
            &error
        ));
        assert!(error
            .to_string()
            .contains("package cache lock holder unknown"));
    }
}
