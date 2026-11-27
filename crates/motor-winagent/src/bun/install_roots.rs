//! Bound Bun's install trees to the project and its resolved workspace members.

use super::{
    ExpansionBudget, MAX_LOCK_BYTES, hex, is_node_modules, path_detail, read_bounded,
    resolution_parts, text_json,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path, PathBuf};

pub(super) struct Roots(Vec<PathBuf>);

impl Roots {
    pub(super) fn from_lock(lock: &Value) -> Result<Self, &'static str> {
        let workspaces = lock["workspaces"]
            .as_object()
            .filter(|map| map.contains_key(""))
            .ok_or("invalid bun.lock workspaces table")?;
        let mut roots = Vec::with_capacity(workspaces.len());
        for member in workspaces.keys() {
            if !member.is_empty()
                && member
                    .split('/')
                    .any(|c| crate::validate_component(c).is_err())
            {
                return Err("invalid workspace member path in bun.lock workspaces");
            }
            roots.push(PathBuf::from(member));
        }
        Ok(Self(roots))
    }

    pub(super) fn read(directory: &Path, lock_sha256: &str) -> Result<Self, &'static str> {
        let bytes = read_bounded(&directory.join("bun.lock"), MAX_LOCK_BYTES as u64)
            .map_err(|_| "read bun.lock")?;
        if !lock_sha256.is_empty() && hex(&Sha256::digest(&bytes)) != lock_sha256 {
            return Err("bun.lock digest changed after cache preparation");
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "decode bun.lock as UTF-8")?;
        let lock = text_json(text).map_err(|_| "parse bun.lock")?;
        Self::from_lock(&lock)
    }

    /// Do not walk through workspace junctions or links during verification or
    /// cleanup. An absent member has nothing to verify or remove.
    pub(super) fn existing(&self, directory: &Path) -> Result<Vec<PathBuf>, PathBuf> {
        let mut result = Vec::new();
        for relative in &self.0 {
            if let Some(path) = checked_directory(directory, relative)? {
                result.push(path);
            }
        }
        Ok(result)
    }
}

fn checked_directory(root: &Path, relative: &Path) -> Result<Option<PathBuf>, PathBuf> {
    let mut path = root.to_path_buf();
    for part in std::iter::once(None).chain(relative.iter().map(Some)) {
        if let Some(part) = part {
            path.push(part);
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !crate::is_reparse(&metadata) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            _ => return Err(path),
        }
    }
    Ok(Some(path))
}

fn local_target(source: &Path, directory: &Path, local: &str) -> Option<PathBuf> {
    if local.is_empty() || local.contains([':', '\\']) || local.starts_with('/') {
        return None;
    }
    let mut target = directory.to_path_buf();
    for part in Path::new(local).components() {
        match part {
            Component::Normal(name)
                if !is_node_modules(name)
                    && name
                        .to_str()
                        .is_some_and(|name| crate::validate_component(name).is_ok()) =>
            {
                target.push(name);
            }
            Component::CurDir => (),
            Component::ParentDir if target != source && target.pop() => (),
            _ => return None,
        }
    }
    target.starts_with(source).then_some(target)
}

pub(super) fn check_inputs(
    source: &Path,
    directory: &Path,
    lock: &Value,
    budget: &ExpansionBudget,
) -> Result<(), String> {
    for root in Roots::from_lock(lock)?
        .existing(directory)
        .map_err(|path| path_detail("resolve Bun install root", directory, &path))?
    {
        scan(directory, &root, Scope::InstallRoot, budget)?;
    }
    let mut locals = BTreeSet::new();
    for row in lock["packages"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(_, row)| row)
    {
        let Some((_, version)) = row[0].as_str().and_then(resolution_parts) else {
            continue;
        };
        let Some(local) = ["file:", "link:", "portal:"]
            .iter()
            .find_map(|prefix| version.strip_prefix(prefix))
        else {
            continue;
        };
        let target = local_target(source, directory, local)
            .ok_or("invalid local dependency path in bun.lock packages")?;
        locals.insert(target);
    }
    // Local package sources can be linked into the install or supply bundled
    // dependencies. One scan of an outer target also checks its nested targets.
    for local in &locals {
        if local
            .ancestors()
            .skip(1)
            .any(|parent| locals.contains(parent))
        {
            continue;
        }
        let report_root = if local.starts_with(directory) {
            directory
        } else {
            source
        };
        let relative = local.strip_prefix(source).expect("contained local target");
        if let Some(path) = checked_directory(source, relative)
            .map_err(|path| path_detail("resolve local dependency directory", report_root, &path))?
        {
            scan(report_root, &path, Scope::LocalTree, budget)?;
        }
    }
    scan(directory, directory, Scope::SourceTree, budget)
}

#[derive(Clone, Copy)]
enum Scope {
    InstallRoot,
    LocalTree,
    SourceTree,
}

fn scan(
    root: &Path,
    directory: &Path,
    scope: Scope,
    budget: &ExpansionBudget,
) -> Result<(), String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|_| path_detail("read source directory", root, directory))?;
    for entry in entries {
        budget.check().map_err(|_| {
            path_detail(
                "source scan exceeded the preparation budget",
                root,
                directory,
            )
        })?;
        let entry =
            entry.map_err(|_| path_detail("read source directory entry", root, directory))?;
        let path = entry.path();
        if !matches!(scope, Scope::SourceTree) && is_node_modules(&entry.file_name()) {
            return Err(path_detail("pre-existing node_modules", root, &path));
        }
        if matches!(scope, Scope::InstallRoot) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| path_detail("inspect source entry", root, &path))?;
        if crate::is_reparse(&metadata) {
            return Err(path_detail("reparse point in Bun project", root, &path));
        }
        if metadata.is_dir() {
            scan(root, &path, scope, budget)?;
        }
    }
    Ok(())
}
