//! Bounded network evidence from the tenant daemon's namespaces, not a job's.
use super::{diagnostics::bounded, runner_socket};
use serde_json::{json, Value};
use std::{
    io, net::Ipv4Addr, os::unix::fs::MetadataExt, path::Path, process::Stdio, time::Instant,
};
use tokio::io::AsyncReadExt;

pub(super) async fn collect(pid: u32, https: bool) -> io::Result<Value> {
    let mut value = summary(Path::new("/proc"), pid)?;
    if runner_socket::main_pid()? != Some(pid) {
        return Err(io::Error::other(
            "runner changed before network diagnostics",
        ));
    }
    value["resolve"] = probe(probe_command(pid, false), std::time::Duration::from_secs(4)).await;
    if https {
        value["https"] = probe(probe_command(pid, true), std::time::Duration::from_secs(6)).await;
    }
    value["runner_still_current"] = json!(runner_socket::main_pid()? == Some(pid));
    Ok(value)
}

fn probe_command(pid: u32, https: bool) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("/usr/bin/nsenter");
    command.args([
        "--target",
        &pid.to_string(),
        "--mount",
        "--net",
        "--root",
        "--wd=/",
        "--",
    ]);
    if https {
        // Fixed public read, explicitly requested by an admin. Ignore curlrc and
        // proxies; success proves only that this URL answered from the daemon's
        // context at probe time, not general or continuing network availability.
        command.args([
            "/usr/bin/curl",
            "--disable",
            "--noproxy",
            "*",
            "--fail",
            "--silent",
            "--max-time",
            "5",
            "--connect-timeout",
            "3",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "https://index.crates.io/config.json",
        ]);
    } else {
        command.args(["/usr/bin/getent", "ahostsv4", "index.crates.io"]);
    }
    command
}

async fn probe(mut command: tokio::process::Command, deadline: std::time::Duration) -> Value {
    let started = Instant::now();
    let child = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else {
        return json!({"available":false});
    };
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(deadline, async {
        child
            .stdout
            .take()
            .unwrap()
            .take(4096)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() == 4096 {
            child.kill().await?;
        }
        child.wait().await
    })
    .await;
    let (success, timed_out, code) = match result {
        Ok(Ok(status)) => (status.success(), false, status.code()),
        Ok(Err(_)) => (false, false, None),
        Err(_) => {
            let _ = child.kill().await;
            (false, true, None)
        }
    };
    // Return parsed IPv4 answers only; no arbitrary diagnostics or command
    // stderr are returned, even when nsenter or the resolver reports an error.
    let addresses: Vec<_> = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse::<Ipv4Addr>().ok())
        .map(|address| address.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .take(16)
        .collect();
    json!({"host":"index.crates.io","success":success,"timed_out":timed_out,"exit_code":code,
        "elapsed_ms":started.elapsed().as_millis() as u64,"addresses":addresses})
}

fn resolvers(path: &Path) -> io::Result<Vec<String>> {
    let bytes = bounded(path, 4096)?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if words.next()? != "nameserver" {
                return None;
            }
            words
                .next()?
                .parse::<Ipv4Addr>()
                .ok()
                .map(|address| address.to_string())
        })
        .take(3)
        .collect())
}

fn summary(proc: &Path, pid: u32) -> io::Result<Value> {
    let root = proc.join(pid.to_string());
    let resolver = root.join("root/etc/resolv.conf");
    let metadata = std::fs::metadata(&resolver)?;
    let routes = bounded(&root.join("net/route"), 16 * 1024)?;
    let address = |value: &str| {
        u32::from_str_radix(value, 16)
            .ok()
            .map(|n| Ipv4Addr::from(n.to_le_bytes()).to_string())
    };
    let routes: Vec<_> = String::from_utf8_lossy(&routes)
        .lines()
        .skip(1)
        .take(64)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let interface: String = fields
                .first()?
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
                .take(32)
                .collect();
            Some(
                json!({"interface":interface,"destination":address(fields.get(1)?)?,
            "gateway":address(fields.get(2)?)?,"mask":address(fields.get(7)?)?}),
            )
        })
        .collect();
    let ns = |path: &Path| {
        std::fs::read_link(path)
            .ok()
            .map(|path| path.to_string_lossy().chars().take(64).collect::<String>())
    };
    let cache = root.join("root/tenant/cargo-home/registry/cache");
    let mut archive_count = 0;
    let mut scanned = 0;
    let mut archives_truncated = false;
    let archives_available = match std::fs::read_dir(cache) {
        Ok(registries) => {
            let mut available = true;
            for (index, registry) in registries.take(17).enumerate() {
                if index == 16 {
                    archives_truncated = true;
                    break;
                }
                let Ok(registry) = registry else {
                    available = false;
                    break;
                };
                let Ok(entries) = std::fs::read_dir(registry.path()) else {
                    available = false;
                    break;
                };
                for entry in entries {
                    scanned += 1;
                    if scanned > 4096 {
                        archives_truncated = true;
                        break;
                    }
                    let Ok(entry) = entry else {
                        available = false;
                        break;
                    };
                    match entry.file_type() {
                        Ok(kind)
                            if kind.is_file()
                                && entry.file_name().to_string_lossy().ends_with(".crate") =>
                        {
                            archive_count += 1
                        }
                        Err(_) => {
                            available = false;
                            break;
                        }
                        _ => {}
                    }
                }
                if archives_truncated || !available {
                    break;
                }
            }
            available
        }
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    };
    Ok(
        json!({"routes":routes,"daemon_resolvers":resolvers(&resolver)?,
        "resolver_uid":metadata.uid(),"resolver_mode":format!("{:o}",metadata.mode() & 0o777),
        "global_resolvers":resolvers(&proc.join("1/root/etc/resolv.conf")).unwrap_or_default(),
        "daemon_mount_namespace":ns(&root.join("ns/mnt")),"daemon_network_namespace":ns(&root.join("ns/net")),
            "init_mount_namespace":ns(&proc.join("1/ns/mnt")),"init_network_namespace":ns(&proc.join("1/ns/net")),
            "cargo_cache":{"archive_count":archive_count,"truncated":archives_truncated,"available":archives_available}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_probe_enters_daemon_mount_and_network_namespaces_with_fixed_targets() {
        for https in [false, true] {
            let command = probe_command(245, https);
            let words: Vec<_> = command
                .as_std()
                .get_args()
                .map(|s| s.to_string_lossy())
                .collect();
            assert_eq!(
                &words[..7],
                ["--target", "245", "--mount", "--net", "--root", "--wd=/", "--"]
            );
            assert!(words.iter().any(|s| s.contains("index.crates.io")));
            if https {
                assert!(words.iter().any(|s| s == "--noproxy"));
            }
        }
    }

    #[test]
    fn network_summary_reads_daemon_resolver_and_routes_not_job_or_agent_files() {
        let root = cortexkit_test_support::ScratchDir::new("tenant-network-summary");
        for path in ["245/root/etc", "245/net", "1/root/etc"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::write(
            root.join("245/root/etc/resolv.conf"),
            "nameserver 1.1.1.1\nsecret TOKEN\n",
        )
        .unwrap();
        std::fs::write(root.join("1/root/etc/resolv.conf"), "").unwrap();
        let cache = root.join("245/root/tenant/cargo-home/registry/cache/registry-id");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("private-name.crate"), "archive").unwrap();
        std::fs::write(root.join("245/net/route"), "Iface Destination Gateway Flags RefCnt Use Metric Mask\neth0 00000000 0101C80A 0003 0 0 0 00000000\n").unwrap();
        let value = summary(&root, 245).unwrap();
        assert_eq!(value["daemon_resolvers"], json!(["1.1.1.1"]));
        assert_eq!(value["global_resolvers"], json!([]));
        assert_eq!(value["routes"][0]["gateway"], "10.200.1.1");
        assert_eq!(value["cargo_cache"]["archive_count"], 1);
        assert!(!value.to_string().contains("private-name"));
        assert!(!value.to_string().contains("TOKEN"));
    }

    #[tokio::test]
    async fn timed_network_probe_stops_a_stalled_tool() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let result = probe(command, std::time::Duration::from_millis(30)).await;
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["success"], false);
        assert!(result["elapsed_ms"].as_u64().unwrap() < 1000);
    }
}
