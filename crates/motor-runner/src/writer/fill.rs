//! Cache-fill identity for the local default-branch commit selected by a capable
//! client. Trusted daemon preparation fetches that tip's locked sources before
//! the writer launches. Offline preflight prevents resolving new sources or compiling a
//! sibling path graph; submitted branch commits and working files are not inputs.
use crate::workspace::{self, Trees};
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

pub(crate) fn descriptor(
    mirror: &Path,
    tip: &str,
    toolchain: &str,
    command: &str,
) -> io::Result<String> {
    // One tree traversal includes nested workspace manifests and Cargo configs.
    let tree = workspace::git(
        mirror,
        &["ls-tree", "-r", "-z", &format!("{tip}^{{commit}}")],
    )?;
    let mut hash = Sha256::new();
    for input in [toolchain, command] {
        hash.update(input.as_bytes());
        hash.update([0]);
    }
    for entry in tree.split('\0') {
        let Some((_, path)) = entry.split_once('\t') else {
            continue;
        };
        let name = path.rsplit('/').next().unwrap();
        if name == "Cargo.toml"
            || name == "Cargo.lock"
            || name.starts_with("rust-toolchain")
            || (name.starts_with("config") && path.split('/').rev().nth(1) == Some(".cargo"))
        {
            hash.update(entry.as_bytes());
            hash.update([0]);
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn locked_git(
    trees: &Trees,
    repository: &str,
    tip: &str,
) -> io::Result<Vec<motor_protocol::GitDependency>> {
    let lock = workspace::git(
        &trees.mirror(repository),
        &["show", &format!("{tip}:Cargo.lock")],
    )
    .unwrap_or_default();
    workspace::dependencies::locked_dependencies(&lock)
}

/// The checkout remains writer-owned; the daemon reads the expected paths from
/// its mirror instead of running Git in a foreign-owned checkout.
pub(crate) fn lockfiles(
    mirror: &Path,
    tip: &str,
    path: &Path,
) -> io::Result<Vec<(String, Vec<u8>)>> {
    let tree = workspace::git(mirror, &["ls-tree", "-r", "--name-only", "-z", tip])?;
    tree.split('\0')
        .filter(|p| p.rsplit('/').next() == Some("Cargo.lock"))
        .map(|p| {
            Ok((
                p.into(),
                fs::read(path.join(p)).map_err(|e| io::Error::other(format!("{p}: {e}")))?,
            ))
        })
        .collect()
}

// The script is installed in the immutable tool mount. Metadata resolution and
// the warm command execute in the same tracked, network-isolated writer unit.
#[cfg(any(target_os = "linux", test))]
pub(crate) const PREFLIGHT: &str = include_str!("preflight.py");

pub(crate) fn reader_missed(state: &Path, id: motor_protocol::Uuid) -> bool {
    fn nonzero(value: &serde_json::Value) -> bool {
        value.as_u64().is_some_and(|n| n > 0)
            || value.as_object().is_some_and(|o| o.values().any(nonzero))
    }
    statistics(state, id)
        .ok()
        .and_then(|v| v.get("stats").unwrap_or(&v).get("cache_misses").cloned())
        .is_some_and(|v| nonzero(&v))
}

pub(crate) fn statistics(state: &Path, id: motor_protocol::Uuid) -> io::Result<serde_json::Value> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let path = state
        .join("sandboxes")
        .join(id.to_string())
        .join("output/cache-stats");
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| io::Error::other(format!("cannot read {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(io::Error::other(format!(
            "empty statistics file: {}",
            path.display()
        )));
    }
    if bytes.len() > 64 * 1024 {
        return Err(io::Error::other("statistics exceed 64 KiB"));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("invalid statistics JSON: {e}")))?;
    if value
        .get("stats")
        .unwrap_or(&value)
        .get("cache_misses")
        .is_none()
    {
        return Err(io::Error::other("statistics lack the cache_misses counter"));
    }
    Ok(value)
}

/// The writer compiles repository-local packages incrementally, which sccache
/// cannot store. Graphs containing only those packages need no cache writer.
/// Registry/git sources and paths outside the repository still require the
/// writer's offline metadata validation before compiling.
pub(crate) fn dependency_free(mirror: &Path, tip: &str) -> io::Result<bool> {
    let tree = workspace::git(mirror, &["ls-tree", "-r", "--name-only", "-z", tip])?;
    let paths: std::collections::BTreeSet<_> = tree
        .split('\0')
        .filter(|path| path.rsplit('/').next() == Some("Cargo.toml"))
        .collect();
    let manifests: std::collections::BTreeMap<_, _> = paths
        .iter()
        .map(|path| {
            let text = workspace::git(mirror, &["show", &format!("{tip}:{path}")])?;
            let manifest = toml::from_str::<toml::Value>(&text).map_err(crate::storage::invalid)?;
            Ok((*path, manifest))
        })
        .collect::<io::Result<_>>()?;
    let workspace_dependencies = manifests
        .get("Cargo.toml")
        .and_then(|root| root.get("workspace"))
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table);
    fn local_dependency(
        value: &toml::Value,
        manifest: &str,
        paths: &std::collections::BTreeSet<&str>,
    ) -> bool {
        let Some(table) = value.as_table() else {
            return false;
        };
        if table.contains_key("git") || table.contains_key("registry") {
            return false;
        }
        let Some(path) = table.get("path").and_then(toml::Value::as_str) else {
            return false;
        };
        let mut directory = std::path::PathBuf::from(manifest)
            .parent()
            .unwrap()
            .to_path_buf();
        for component in std::path::Path::new(path).components() {
            match component {
                std::path::Component::Normal(part) => directory.push(part),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir if directory.pop() => {}
                _ => return false,
            }
        }
        paths.contains(directory.join("Cargo.toml").to_str().unwrap_or(""))
    }
    fn local_graph(
        value: &toml::Value,
        manifest: &str,
        paths: &std::collections::BTreeSet<&str>,
        workspace: Option<&toml::Table>,
    ) -> bool {
        match value {
            toml::Value::Table(table) => table.iter().all(|(key, value)| {
                if matches!(
                    key.as_str(),
                    "dependencies" | "dev-dependencies" | "build-dependencies"
                ) {
                    value.as_table().is_some_and(|dependencies| {
                        dependencies.iter().all(|(name, value)| {
                            if value.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                                workspace
                                    .and_then(|dependencies| dependencies.get(name))
                                    .is_some_and(|value| {
                                        local_dependency(value, "Cargo.toml", paths)
                                    })
                            } else {
                                local_dependency(value, manifest, paths)
                            }
                        })
                    })
                } else {
                    local_graph(value, manifest, paths, workspace)
                }
            }),
            toml::Value::Array(values) => values
                .iter()
                .all(|value| local_graph(value, manifest, paths, workspace)),
            _ => true,
        }
    }
    if !manifests
        .iter()
        .all(|(path, manifest)| local_graph(manifest, path, &paths, workspace_dependencies))
    {
        return Ok(false);
    }
    // A lockfile can retain registry/git packages after their declarations have
    // been removed. Do not fast-path such a graph: offline Cargo metadata must
    // validate the lockfile without resolving new versions or fetching sources.
    if let Ok(lock) = workspace::git(mirror, &["show", &format!("{tip}:Cargo.lock")]) {
        let lock = toml::from_str::<toml::Value>(&lock).map_err(crate::storage::invalid)?;
        if lock
            .get("package")
            .and_then(toml::Value::as_array)
            .is_some_and(|packages| {
                packages
                    .iter()
                    .any(|package| package.get("source").is_some())
            })
        {
            return Ok(false);
        }
    }
    Ok(true)
}
