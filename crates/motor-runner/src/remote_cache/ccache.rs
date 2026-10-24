//! ccache 4.12.3 (Ubuntu 26.04's 4.12.3-1), the intended pin for every Linux
//! role in `script/runner/toolchain.json`. Only the build server is provisioned;
//! Boat and tenant activation stays disabled until their images install it.
//! Its HTTP backend supports `read-only`,
//! `remote_only` and `CCACHE_RECACHE`; no shared local ccache tier is used.
//!
//! Activation is explicit: a repository invokes `ccache cc` / `ccache c++` in
//! its build and declares trusted fill commands in `.motor/cache.toml` with
//! `tool = "ccache"`. The generic fill planner hashes the declaration, matched
//! descriptor inputs and compiler/tool versions and refills on their change.
//! We do not set CC, CXX, a compiler masquerade directory or a prefix command:
//! cc-rs compiles already routed through sccache must not be wrapped again.
//!
//! `base_dir` is the unit's checkout root, not the fill's host path. Commands
//! must use relative source/include paths and, for debug builds, matching
//! `-fdebug-prefix-map=$PWD=.` flags in fill and reader. Disabling `hash_dir`
//! then permits hits across their different checkout paths without embedding
//! the fill directory in debug information. `compiler_check=content` avoids
//! relying on installation mtimes; different native compilers miss cleanly.
//!
//! Counter names below are from this release's `--print-stats`, not the
//! human-readable `--show-stats`. Remote errors and timeouts veto publication
//! even when no PUT reached the gateway. Qualification requires a reader run
//! with remote hits > 0 and both failure counters = 0; local hits do not satisfy
//! that check. Ordinary cold readers may miss and compile normally.
use std::collections::BTreeMap;
use std::io;

use super::provisioning::HostRole;
use super::transport::Forwarder;

pub(crate) const VERSION: &str = "4.12.3";
pub(crate) const STATS_FILE: &str = "ccache-stats";
pub(crate) const REMOTE_HIT: &str = "remote_storage_hit";
pub(crate) const REMOTE_ERROR: &str = "remote_storage_error";
pub(crate) const REMOTE_TIMEOUT: &str = "remote_storage_timeout";

/// Images without the pinned ccache are not eligible for activation. Boat and
/// tenant remain disabled until a source installer provisions ccache 4.12.3
/// and wires it into both images.
pub(crate) fn usable(role: HostRole, version: &str) -> bool {
    role == HostRole::BuildServer
        && version.lines().next() == Some(&format!("ccache version {VERSION}"))
}

pub(crate) fn installed(role: HostRole) -> bool {
    if role != HostRole::BuildServer {
        return false;
    }
    std::process::Command::new("ccache")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && usable(role, &String::from_utf8_lossy(&output.stdout))
        })
}

pub(crate) fn environment(
    forwarder: &Forwarder,
    fill: bool,
    workspace: &str,
) -> BTreeMap<String, String> {
    let remote = if fill {
        forwarder.url()
    } else {
        format!("{}|read-only", forwarder.url())
    };
    let mut env = BTreeMap::from([
        ("CCACHE_REMOTE_STORAGE".into(), remote),
        ("CCACHE_REMOTE_ONLY".into(), "true".into()),
        // /tmp is a fresh private bind mount for each job and each fill.
        ("CCACHE_DIR".into(), "/tmp/motor-ccache".into()),
        // Ignore both host configuration and repository-supplied ccache.conf.
        ("CCACHE_CONFIGPATH".into(), "/dev/null".into()),
        ("CCACHE_COMPILERCHECK".into(), "content".into()),
        ("CCACHE_NOHASHDIR".into(), "true".into()),
        ("CCACHE_BASEDIR".into(), workspace.into()),
    ]);
    if fill {
        env.insert("CCACHE_RECACHE".into(), "true".into());
    }
    env
}

/// Parse machine statistics strictly: missing, duplicated or malformed remote
/// counters cannot turn an incomplete statistics file into a publication.
pub(crate) fn statistics(bytes: &[u8]) -> io::Result<BTreeMap<String, u64>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| io::Error::other("ccache statistics are not text"))?;
    let mut stats = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 {
            return Err(io::Error::other("invalid ccache statistics line"));
        }
        let count = fields[1]
            .parse::<u64>()
            .map_err(|_| io::Error::other(format!("invalid ccache counter {}", fields[0])))?;
        if stats.insert(fields[0].to_string(), count).is_some() {
            return Err(io::Error::other("duplicate ccache counter"));
        }
    }
    for name in [REMOTE_HIT, REMOTE_ERROR, REMOTE_TIMEOUT] {
        if !stats.contains_key(name) {
            return Err(io::Error::other(format!("missing ccache counter {name}")));
        }
    }
    Ok(stats)
}

#[cfg(test)]
#[path = "ccache_tests.rs"]
mod tests;
