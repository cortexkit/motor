use std::{collections::HashSet, io, net::IpAddr, path::Path};

const HOSTS_PATH: &str = ".motor/hosts";
const MAX_BYTES: usize = 4096;
const MAX_NAMES: usize = 64;

fn refusal(line: usize, reason: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{HOSTS_PATH}: line {line}: {reason}"),
    )
}

fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Validate the entire declaration before producing any job-visible entries.
fn parse(blob: &[u8]) -> io::Result<Vec<String>> {
    if blob.len() > MAX_BYTES {
        return Err(refusal(1, "exceeds the 4096-byte limit"));
    }
    let text = std::str::from_utf8(blob).map_err(|error| {
        let line = blob[..error.valid_up_to()]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        refusal(line, "content is not valid UTF-8")
    })?;
    let mut names = HashSet::new();
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let mut words = line.split('#').next().unwrap_or("").split_whitespace();
        let Some(address) = words.next() else {
            continue;
        };
        let address: IpAddr = address
            .parse()
            .map_err(|_| refusal(line_number, "invalid IP address"))?;
        let loopback = match address {
            IpAddr::V4(ip) => ip.is_loopback(),
            IpAddr::V6(ip) => ip == std::net::Ipv6Addr::LOCALHOST,
        };
        if !loopback {
            return Err(refusal(
                line_number,
                "address must be in 127.0.0.0/8 or ::1",
            ));
        }
        let mut aliases = Vec::new();
        for word in words {
            let name = word.to_ascii_lowercase();
            if !valid_hostname(&name) {
                return Err(refusal(
                    line_number,
                    format!("invalid DNS hostname {word:?}"),
                ));
            }
            if matches!(
                name.as_str(),
                "localhost" | "ip6-localhost" | "ip6-loopback"
            ) {
                return Err(refusal(
                    line_number,
                    format!("protected default name {name:?}"),
                ));
            }
            if !names.insert(name.clone()) {
                return Err(refusal(line_number, format!("duplicate hostname {name:?}")));
            }
            if names.len() > MAX_NAMES {
                return Err(refusal(line_number, "exceeds the 64-name limit"));
            }
            aliases.push(name);
        }
        if aliases.is_empty() {
            return Err(refusal(
                line_number,
                "address must have at least one hostname",
            ));
        }
        entries.push(format!("{address}\t{}\n", aliases.join("\t")));
    }
    Ok(entries)
}

pub(super) fn from_tree(workspace: &Path, tree: &str) -> io::Result<Vec<String>> {
    let git = |args: &[&std::ffi::OsStr]| {
        super::javascript_git(workspace, args)
            .map_err(|error| refusal(1, format!("cannot read job snapshot tree: {error}")))
    };
    let entry = git(&[
        "ls-tree".as_ref(),
        "-z".as_ref(),
        tree.as_ref(),
        "--".as_ref(),
        HOSTS_PATH.as_ref(),
    ])?;
    if entry.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = entry.split(|&b| b == b'\t').next().unwrap_or(&[]);
    let metadata =
        std::str::from_utf8(metadata).map_err(|_| refusal(1, "invalid Git tree entry metadata"))?;
    let mut fields = metadata.split_whitespace();
    let mode = fields.next();
    let kind = fields.next();
    if !matches!(mode, Some("100644" | "100755")) || kind != Some("blob") {
        return Err(refusal(
            1,
            "tree entry must be a regular blob (100644 or 100755)",
        ));
    }
    let object = format!("{tree}:{HOSTS_PATH}");
    let size = git(&["cat-file".as_ref(), "-s".as_ref(), object.as_ref()])?;
    let size = std::str::from_utf8(&size)
        .ok()
        .and_then(|size| size.trim().parse::<usize>().ok())
        .ok_or_else(|| refusal(1, "invalid Git blob size"))?;
    if size > MAX_BYTES {
        return Err(refusal(1, "exceeds the 4096-byte limit"));
    }
    let blob = git(&["cat-file".as_ref(), "blob".as_ref(), object.as_ref()])?;
    parse(&blob)
}

#[cfg(test)]
#[path = "hosts_tests.rs"]
mod tests;
