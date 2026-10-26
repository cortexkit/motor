//! Offline retirement of one caller-root mapping.
//!
//! `motor-runner forget-caller-root --prefix P <serve options>` is the only
//! way to retire a mapping. It requires a stopped runner (the daemon lock)
//! and no live `motor-job-*` units, checks the owner markers of H and of
//! every other host root a record pinning P also pins, then runs four
//! idempotent steps: sweep H, delete outside-H trees owned by pinning
//! records, purge the used caller trashes, and drop the pinning catalog
//! entries and job records after clearing `sibling_freshness` on matching
//! identity-catalog workspace records.

use super::{owner, startup, trash, CallerRoot};
use crate::{storage, Config};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

fn invalid(message: impl ToString) -> io::Error {
    storage::invalid(message.to_string())
}

/// A caller path resolved through one record's own mappings, without
/// consulting the runner configuration. Identity paths come back unchanged.
fn host_for_record(mappings: &[CallerRoot], caller: &str) -> PathBuf {
    for mapping in mappings {
        if Path::new(caller).starts_with(&mapping.caller_prefix) {
            if let Ok(relative) = Path::new(caller).strip_prefix(&mapping.caller_prefix) {
                return mapping.host_root.join(relative);
            }
        }
    }
    PathBuf::from(caller)
}

fn pins_prefix(mappings: &[CallerRoot], prefix: &str) -> bool {
    mappings.iter().any(|m| m.caller_prefix == prefix)
}

pub(crate) struct Plan {
    pub(crate) prefix: String,
    pub(crate) host: PathBuf,
    pub(crate) others: BTreeMap<String, PathBuf>,
    pub(crate) split: super::catalog::Split,
}

fn read_jobs(state: &Path) -> io::Result<Vec<(uuid::Uuid, Vec<CallerRoot>)>> {
    let mut jobs = Vec::new();
    let dir = state.join("jobs");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(jobs),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let id = entry
            .file_name()
            .to_str()
            .and_then(|name| uuid::Uuid::parse_str(name).ok());
        let Some(id) = id else { continue };
        let path = entry.path().join("record.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
                if let Some(mappings) = value.get("mappings") {
                    let mappings: Vec<CallerRoot> =
                        serde_json::from_value(mappings.clone()).map_err(invalid)?;
                    jobs.push((id, mappings));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    jobs.sort_by_key(|(id, _)| *id);
    Ok(jobs)
}

pub(crate) fn load_plan(config: &Config, prefix: &str) -> io::Result<Plan> {
    if !startup::canonical_spelling(prefix) || prefix == "/" {
        return Err(invalid(format!(
            "forget-caller-root prefix {prefix:?}: expected a canonical absolute path"
        )));
    }
    let state = config.state_dir();
    let split = super::catalog::Split::read(&state)?;
    let jobs = read_jobs(&state)?;
    let mut pins: Vec<CallerRoot> = split.caller.pins();
    for (_, mappings) in &jobs {
        pins.extend(mappings.iter().cloned());
    }
    let mut hosts = BTreeSet::new();
    for pin in &pins {
        if pin.caller_prefix == prefix {
            hosts.insert(pin.host_root.clone());
        }
    }
    if hosts.is_empty() {
        return Err(invalid(format!(
            "no record pins caller prefix {prefix}; nothing to forget"
        )));
    }
    if hosts.len() > 1 {
        let names: Vec<_> = hosts.iter().map(|h| h.display().to_string()).collect();
        return Err(invalid(format!(
            "caller prefix {prefix} pins inconsistent host roots: {}",
            names.join(", ")
        )));
    }
    let host = hosts.into_iter().next().unwrap();
    if let Some(configured) = config
        .caller_roots
        .iter()
        .find(|root| root.caller_prefix == prefix)
    {
        if configured.host_root != host {
            return Err(invalid(format!(
                "forget-caller-root --prefix {prefix} configured host {} does not match recorded host {}",
                configured.host_root.display(),
                host.display()
            )));
        }
    }
    let mut others: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut note = |mappings: &[CallerRoot]| -> io::Result<()> {
        if !pins_prefix(mappings, prefix) {
            return Ok(());
        }
        for mapping in mappings {
            if mapping.caller_prefix == prefix {
                continue;
            }
            match others.get(&mapping.caller_prefix) {
                Some(known) if *known != mapping.host_root => {
                    return Err(invalid(format!(
                        "caller prefix {} pins inconsistent host roots for {}",
                        prefix, mapping.caller_prefix
                    )));
                }
                Some(_) => {}
                None => {
                    others.insert(mapping.caller_prefix.clone(), mapping.host_root.clone());
                }
            }
        }
        Ok(())
    };
    for entry in split.caller.workspaces.values() {
        note(&entry.mappings)?;
    }
    for entry in split.caller.repositories.values() {
        note(&entry.mappings)?;
    }
    for entry in split.caller.legacy_checkouts.values() {
        note(&entry.mappings)?;
    }
    for entry in split.caller.sibling_commits.values() {
        note(&entry.mappings)?;
    }
    for entry in split.caller.transferred.values() {
        note(&entry.mappings)?;
    }
    for (_, mappings) in &jobs {
        note(mappings)?;
    }
    Ok(Plan {
        prefix: prefix.to_owned(),
        host,
        others,
        split,
    })
}

fn check_markers(config: &Config, plan: &Plan) -> io::Result<()> {
    owner::check_root(
        config,
        &CallerRoot {
            caller_prefix: plan.prefix.clone(),
            host_root: plan.host.clone(),
        },
    )?;
    for (prefix, host) in &plan.others {
        owner::check_root(
            config,
            &CallerRoot {
                caller_prefix: prefix.clone(),
                host_root: host.clone(),
            },
        )?;
    }
    Ok(())
}

fn check_live_units(config: &Config) -> io::Result<()> {
    // Portable fixtures run with isolation off and no job units; only query
    // the manager when jobs could be isolated or a test stands one in.
    #[cfg(test)]
    if !config.isolate_jobs && config.systemd_command.is_none() {
        return Ok(());
    }
    #[cfg(not(test))]
    if !config.isolate_jobs {
        return Ok(());
    }
    let live = crate::backend::live_job_units(config)?;
    // Unit names carry no instance, so other runners' units share the
    // listing. Only a live unit for a job this instance's state records
    // blocks forget: the unit named by each record's own `process.unit`
    // (which can differ from the job UUID) and the unit for every job ID
    // present under this instance's jobs/ directory.
    let owned = instance_units(&config.state_dir());
    let ours: Vec<_> = live.intersection(&owned).cloned().collect();
    if !ours.is_empty() {
        return Err(invalid(format!(
            "forget-caller-root refuses while {} live job units exist: {}",
            ours.len(),
            ours.join(", ")
        )));
    }
    Ok(())
}

fn instance_units(state: &Path) -> BTreeSet<String> {
    let mut owned = BTreeSet::new();
    let Ok(entries) = fs::read_dir(state.join("jobs")) else {
        return owned;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        owned.insert(format!("motor-job-{name}.service"));
        if let Ok(bytes) = fs::read(entry.path().join("record.json")) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(unit) = value
                    .pointer("/process/unit")
                    .and_then(|unit| unit.as_str())
                {
                    owned.insert(format!("motor-job-{unit}.service"));
                }
            }
        }
    }
    owned
}

#[cfg(target_os = "linux")]
fn trash_name(config: &Config) -> String {
    format!(".motor-trash-{}", config.instance)
}

#[cfg(target_os = "linux")]
pub(crate) fn step_one(config: &Config, plan: &Plan) -> io::Result<()> {
    // The sweep reuses the mapped-deletion path: detach reclaims a job-owned
    // checkout root before moving it across parents, and the trash stays
    // pinned throughout the purge.
    trash::sweep(
        config,
        &CallerRoot {
            caller_prefix: plan.prefix.clone(),
            host_root: plan.host.clone(),
        },
    )
}

#[cfg(not(target_os = "linux"))]
fn step_one(_: &Config, _: &Plan) -> io::Result<()> {
    Err(invalid("forget-caller-root requires Linux"))
}

/// Every outside-H tree owned by a caller entry that pins the forgotten
/// prefix and has no identity twin. Trees beneath H were swept in step one.
#[cfg(target_os = "linux")]
pub(crate) fn step_two(config: &Config, plan: &Plan) -> io::Result<()> {
    let mut targets: Vec<(PathBuf, Option<CallerRoot>)> = Vec::new();
    let mut push = |caller: &str, mappings: &[CallerRoot]| {
        let host = host_for_record(mappings, caller);
        if host == plan.host || host.starts_with(&plan.host) {
            return;
        }
        if let Some((prefix, root)) = plan
            .others
            .iter()
            .find(|(_, root)| host == **root || host.starts_with(*root))
            .map(|(prefix, root)| {
                (
                    prefix.clone(),
                    CallerRoot {
                        caller_prefix: prefix.clone(),
                        host_root: root.clone(),
                    },
                )
            })
        {
            let _ = prefix;
            targets.push((host, Some(root)));
        } else {
            targets.push((host, None));
        }
    };
    for (key, entry) in &plan.split.caller.workspaces {
        if pins_prefix(&entry.mappings, &plan.prefix)
            && !plan.split.identity.workspaces.contains_key(key)
        {
            push(key, &entry.mappings);
        }
    }
    for (key, entry) in &plan.split.caller.repositories {
        if !pins_prefix(&entry.mappings, &plan.prefix)
            || plan.split.identity.repositories.contains_key(key)
        {
            continue;
        }
        if let Some(desired) = entry.record.desired.as_ref() {
            let checkout = Path::new(&desired.workspace)
                .parent()
                .map(|parent| {
                    parent.join(format!(
                        ".motor-base-{}-{}",
                        config.instance,
                        crate::workspace::name(key)
                    ))
                })
                .and_then(|path| path.to_str().map(str::to_owned));
            if let Some(checkout) = checkout {
                push(&checkout, &entry.mappings);
            }
        }
    }
    for (key, entry) in &plan.split.caller.legacy_checkouts {
        if pins_prefix(&entry.mappings, &plan.prefix)
            && !plan.split.identity.legacy_checkouts.contains_key(key)
        {
            push(key, &entry.mappings);
        }
    }
    for (host, root) in targets {
        match root {
            Some(root) => {
                if let Some(retired) =
                    trash::detach(config, &root, crate::caller_path::host(&host))?
                {
                    let _ = retired;
                }
            }
            None => discard_identity(config, &host)?,
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn step_two(_: &Config, _: &Plan) -> io::Result<()> {
    Err(invalid("forget-caller-root requires Linux"))
}

/// The identity discard path for a tree forget owns: validate it against the
/// owned roots, rename it aside, then remove the renamed tree at once. An
/// absent tree is already forgotten.
#[cfg(target_os = "linux")]
fn discard_identity(config: &Config, host: &Path) -> io::Result<()> {
    match fs::symlink_metadata(host) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    crate::workspace::validate_destinations(
        &config.owned_workspace_roots,
        crate::caller_path::host(host),
        None,
        &[],
    )?;
    let trash = config.state_dir().join("trash");
    fs::create_dir_all(&trash)?;
    let aside = trash.join(uuid::Uuid::now_v7().to_string());
    match fs::rename(host, &aside) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(()) => {}
    }
    crate::filesystem::sync_directory(&trash)?;
    if let Some(parent) = host.parent() {
        let _ = crate::filesystem::sync_directory(parent);
    }
    remove_identity_tree(config, &aside)
}

#[cfg(target_os = "linux")]
fn remove_identity_tree(config: &Config, path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    if !metadata.is_dir() {
        return fs::remove_file(path).or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        });
    }
    if config.isolate_jobs {
        let _ = crate::isolation::reclaim_runtime(path);
    }
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn step_three(config: &Config, plan: &Plan) -> io::Result<()> {
    // The purge reuses the retired-entry removal: physical-path reclaim for
    // the privileged helper, boat handling, and descriptor-pinned removal.
    trash::purge_dir(config, &plan.host.join(trash_name(config)))?;
    let mut purged = BTreeSet::new();
    for host in plan.others.values() {
        if purged.insert(host.clone()) {
            trash::purge_dir(config, &host.join(trash_name(config)))?;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn step_three(_: &Config, _: &Plan) -> io::Result<()> {
    Err(invalid("forget-caller-root requires Linux"))
}

pub(crate) fn step_four(config: &Config, plan: &Plan) -> io::Result<Vec<String>> {
    let state = config.state_dir();
    let mut removed = Vec::new();
    // Re-read so a rerun after a partial step four converges: freshness may
    // already be cleared and some pinning entries already gone.
    let disk = super::catalog::Split::read(&state)?;
    let mut identity = disk.identity;
    let mut caller = disk.caller;
    let mut freshness_cleared = false;
    for (key, entry) in &caller.workspaces {
        if pins_prefix(&entry.mappings, &plan.prefix) && identity.workspaces.contains_key(key) {
            if let Some(record) = identity.workspaces.get_mut(key) {
                if !record.sibling_freshness.is_empty() {
                    record.sibling_freshness.clear();
                    freshness_cleared = true;
                }
            }
        }
    }
    if freshness_cleared {
        let bytes = serde_json::to_vec(&identity).map_err(invalid)?;
        storage::atomic_bytes(
            &state.join("catalog.json"),
            &bytes,
            config.windows_instance(),
        )?;
    }
    let mut drop_collection = |name: &str, keys: Vec<String>| {
        for key in keys {
            removed.push(format!("{name} {key}"));
        }
    };
    let workspaces: Vec<_> = caller
        .workspaces
        .iter()
        .filter(|(_, entry)| pins_prefix(&entry.mappings, &plan.prefix))
        .map(|(key, _)| key.clone())
        .collect();
    drop_collection("workspace", workspaces.clone());
    for key in &workspaces {
        caller.workspaces.remove(key);
    }
    let repositories: Vec<_> = caller
        .repositories
        .iter()
        .filter(|(_, entry)| pins_prefix(&entry.mappings, &plan.prefix))
        .map(|(key, _)| key.clone())
        .collect();
    drop_collection("repository", repositories.clone());
    for key in &repositories {
        caller.repositories.remove(key);
    }
    let legacy: Vec<_> = caller
        .legacy_checkouts
        .iter()
        .filter(|(_, entry)| pins_prefix(&entry.mappings, &plan.prefix))
        .map(|(key, _)| key.clone())
        .collect();
    drop_collection("legacy-checkout", legacy.clone());
    for key in &legacy {
        caller.legacy_checkouts.remove(key);
    }
    let commits: Vec<_> = caller
        .sibling_commits
        .iter()
        .filter(|(_, entry)| pins_prefix(&entry.mappings, &plan.prefix))
        .map(|(key, _)| key.clone())
        .collect();
    drop_collection("sibling-commit", commits.clone());
    for key in &commits {
        caller.sibling_commits.remove(key);
    }
    let transferred: Vec<_> = caller
        .transferred
        .iter()
        .filter(|(_, entry)| pins_prefix(&entry.mappings, &plan.prefix))
        .map(|(key, _)| key.to_string())
        .collect();
    drop_collection("transferred", transferred.clone());
    for key in transferred {
        if let Ok(id) = key.parse::<uuid::Uuid>() {
            caller.transferred.remove(&id);
        }
    }
    let bytes = serde_json::to_vec(&caller).map_err(invalid)?;
    storage::atomic_bytes(
        &state.join("caller-catalog.json"),
        &bytes,
        config.windows_instance(),
    )?;
    let jobs = read_jobs(&state)?;
    for (id, mappings) in jobs {
        if pins_prefix(&mappings, &plan.prefix) {
            let directory = state.join("jobs").join(id.to_string());
            match fs::remove_dir_all(&directory) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(()) => {}
            }
            if let Some(parent) = directory.parent() {
                let _ = crate::filesystem::sync_directory(parent);
            }
            removed.push(format!("job {id}"));
        }
    }
    removed.sort();
    Ok(removed)
}

/// Run all four steps in order. Every step tolerates work already done, so a
/// rerun after a kill converges; records are removed last, so a kill before
/// step four completes leaves the fixed-mapping refusal in place.
pub fn forget_caller_root(config: &Config, prefix: &str) -> io::Result<Vec<String>> {
    use fs2::FileExt;
    let state = config.state_dir();
    let lock_path = state.join("daemon.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            invalid(format!(
                "forget-caller-root cannot open {}: {error}",
                lock_path.display()
            ))
        })?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            invalid("forget-caller-root refuses while the runner is running (daemon.lock is held)")
        } else {
            error
        }
    })?;
    let plan = load_plan(config, prefix)?;
    check_markers(config, &plan)?;
    check_live_units(config)?;
    #[cfg(target_os = "linux")]
    {
        step_one(config, &plan)?;
        step_two(config, &plan)?;
        step_three(config, &plan)?;
    }
    #[cfg(not(target_os = "linux"))]
    {
        return Err(invalid("forget-caller-root requires Linux"));
    }
    let removed = step_four(config, &plan)?;
    for line in &removed {
        println!("{line}");
    }
    let _ = lock;
    Ok(removed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn host_mapping_uses_record_mappings_not_configuration() {
        let mappings = vec![CallerRoot::parse("/home=/h").unwrap()];
        assert_eq!(
            host_for_record(&mappings, "/home/u/proj"),
            PathBuf::from("/h/u/proj")
        );
        assert_eq!(
            host_for_record(&mappings, "/Users/u/proj"),
            PathBuf::from("/Users/u/proj")
        );
        assert_eq!(host_for_record(&[], "/data/x"), PathBuf::from("/data/x"));
    }
}
