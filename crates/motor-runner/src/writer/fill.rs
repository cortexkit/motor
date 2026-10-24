//! Cache-fill identity for the local default-branch commit selected by a capable
//! client. Trusted daemon preparation fetches that tip's locked sources before
//! the writer launches. Offline preflight prevents resolving new sources or compiling a
//! sibling path graph; submitted branch commits and working files are not inputs.
//!
//! A repository without a root `Cargo.toml` has no Cargo fill, so it declares
//! its own in `.motor/cache.toml` (see [`declaration`]). Everything here that
//! answers "what does this tip's fill run, and what does it depend on" takes
//! that declaration into account.
use crate::workspace::{self, Trees};
use crate::writer::declaration::{self, Compilers, Declaration};
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

pub(crate) fn descriptor(
    mirror: &Path,
    tip: &str,
    toolchain: &str,
    command: &str,
    declaration: Option<&Declaration>,
    compilers: &Compilers,
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
    // A declared fill depends on what it declared and on the files those
    // patterns name, so it is hashed beside the Cargo inputs rather than
    // instead of them: one tree can declare one fill and build another.
    if let Some(declaration) = declaration.filter(|declaration| !declaration.fills.is_empty()) {
        for step in &declaration.fills {
            declaration::step_hash(&mut hash, step);
        }
        compilers.hash(&mut hash);
        let paths: Vec<&str> = tree
            .split('\0')
            .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path))
            .collect();
        for path in declaration::matched(&paths, &declared_inputs(declaration)) {
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(&workspace::blob(mirror, &format!("{tip}:{path}"))?);
            hash.update([0]);
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Every pattern any declared fill lists, in declaration order.
fn declared_inputs(declaration: &Declaration) -> Vec<String> {
    declaration
        .fills
        .iter()
        .flat_map(|step| step.descriptor_inputs.iter().cloned())
        .collect()
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
///
/// Only tracked lockfiles are checked. A fill with no Cargo manifest, such as a
/// declared C compilation, has no lockfile at all; that is not a lockfile that
/// changed.
pub(crate) fn lockfiles(
    mirror: &Path,
    tip: &str,
    path: &Path,
) -> io::Result<Vec<(String, Vec<u8>)>> {
    let tree = workspace::git(mirror, &["ls-tree", "-r", "--name-only", "-z", tip])?;
    tree.split('\0')
        .filter(|p| p.rsplit('/').next() == Some("Cargo.lock"))
        .map(|p| {
            let bytes = match fs::read(path.join(p)) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(io::Error::other(format!("{p}: {error}"))),
            };
            Ok((p.into(), bytes))
        })
        .collect()
}

// The script is installed in the immutable tool mount. Metadata resolution and
// the warm command execute in the same tracked, network-isolated writer unit.
#[cfg(any(target_os = "linux", test))]
pub(crate) const PREFLIGHT: &str = include_str!("preflight.py");

/// The writer shell's gate on Cargo's offline preflight. It looks for the root
/// manifest in the tree it is about to compile, because the shell does not
/// carry the tip's file list; a tree without one has nothing for Cargo to
/// check, and its declared fill runs alone.
#[cfg(any(target_os = "linux", test))]
pub(crate) const CARGO_CHECK: &str = include_str!("cargo-check.sh");

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
    use std::io::Read;
    let path = state
        .join("sandboxes")
        .join(id.to_string())
        .join("output/cache-stats");
    let file = crate::filesystem::read_nofollow(&path)
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

/// The biggest output file the runner reads back from a sandbox.
const MAX_STATISTICS_BYTES: u64 = 64 * 1024;

fn read_output(state: &Path, id: motor_protocol::Uuid, name: &str) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let path = state
        .join("sandboxes")
        .join(id.to_string())
        .join("output")
        .join(name);
    let file = crate::filesystem::read_nofollow(&path)
        .map_err(|e| io::Error::other(format!("cannot read {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_STATISTICS_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(io::Error::other(format!(
            "empty statistics file: {}",
            path.display()
        )));
    }
    if bytes.len() as u64 > MAX_STATISTICS_BYTES {
        return Err(io::Error::other("statistics exceed 64 KiB"));
    }
    Ok(bytes)
}

/// Whether sccache reported a failed remote write. `cache_write_errors` counts
/// puts the store refused or the transport lost; a fill that sent `all` as its
/// multi-level write error policy also fails its own command on one, so this
/// normally agrees with the fill's exit status.
pub(crate) fn sccache_write_failures(state: &Path, id: motor_protocol::Uuid) -> io::Result<u64> {
    let value = statistics(state, id)?;
    let stats = value.get("stats").unwrap_or(&value);
    let errors = ["cache_write_errors", "cache_errors"]
        .iter()
        .filter_map(|counter| stats.get(*counter))
        .try_fold(0u64, |total, value| -> io::Result<u64> {
            let count = count_in(value).ok_or_else(|| {
                io::Error::other("statistics report a write-error count that is not a number")
            })?;
            Ok(total.saturating_add(count))
        })?;
    Ok(errors)
}

/// A counter's value, whether it is a bare number or sccache's
/// `{"counts": {...}, "adv_counts": {...}}` shape.
fn count_in(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .get("counts")
            .and_then(|counts| counts.as_object())
            .map(|counts| counts.values().filter_map(serde_json::Value::as_u64).sum())
    })
}

/// Whether ccache's `--print-stats` output reports a remote-storage error or
/// timeout. ccache prints one `key<TAB>value` line per counter, and the
/// remote-storage counters are named `remote_storage_error` and
/// `remote_storage_timeout`. A file that does not parse as that shape fails
/// the publication: statistics that cannot be read say nothing about the
/// writes that happened.
pub(crate) fn ccache_write_failures(state: &Path, id: motor_protocol::Uuid) -> io::Result<u64> {
    let bytes = read_output(state, id, "ccache-stats")?;
    let text =
        String::from_utf8(bytes).map_err(|_| io::Error::other("ccache statistics are not text"))?;
    let mut total = 0u64;
    let mut named = 0usize;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !matches!(name, "remote_storage_error" | "remote_storage_timeout") {
            continue;
        }
        let count = value
            .parse::<u64>()
            .map_err(|_| io::Error::other(format!("ccache counter {name} is not a number")))?;
        total = total.saturating_add(count);
        named += 1;
    }
    if named == 0 {
        return Err(io::Error::other(
            "ccache statistics carry no remote-storage counter",
        ));
    }
    Ok(total)
}

/// The writer compiles repository-local packages incrementally, which sccache
/// cannot store. Graphs containing only those packages need no cache writer.
/// Registry/git sources and paths outside the repository still require the
/// writer's offline metadata validation before compiling.
///
/// A declared fill is always run: its commands are explicit, so the shortcut
/// that records a fill without a writer never applies to it.
pub(crate) fn dependency_free(
    mirror: &Path,
    tip: &str,
    declaration: Option<&Declaration>,
) -> io::Result<bool> {
    if declaration.is_some_and(|declaration| !declaration.fills.is_empty()) {
        return Ok(false);
    }
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
