//! The legacy file remains an identity-only catalog, including its global fields.
use super::{mappings, CallerRoot};
use crate::storage::{self, *};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::Path};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Entry<T> {
    pub record: T,
    pub mappings: Vec<CallerRoot>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CallerCatalog {
    pub workspaces: BTreeMap<String, Entry<Workspace>>,
    pub repositories: BTreeMap<String, Entry<Repository>>,
    pub legacy_checkouts: BTreeMap<String, Entry<LegacyCheckout>>,
    pub sibling_commits: BTreeMap<String, Entry<String>>,
    pub transferred: BTreeMap<Uuid, Entry<RetainedTransfer>>,
}

impl CallerCatalog {
    pub(crate) fn pins(&self) -> Vec<CallerRoot> {
        self.workspaces
            .values()
            .flat_map(|e| &e.mappings)
            .chain(self.repositories.values().flat_map(|e| &e.mappings))
            .chain(self.legacy_checkouts.values().flat_map(|e| &e.mappings))
            .chain(self.sibling_commits.values().flat_map(|e| &e.mappings))
            .chain(self.transferred.values().flat_map(|e| &e.mappings))
            .cloned()
            .collect()
    }
    fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
            && self.repositories.is_empty()
            && self.legacy_checkouts.is_empty()
            && self.sibling_commits.is_empty()
            && self.transferred.is_empty()
    }
}

#[derive(Clone)]
pub(crate) struct Split {
    pub identity: Catalog,
    pub caller: CallerCatalog,
}

fn partition<K: Ord + Clone, T: Clone>(
    records: &mut BTreeMap<K, T>,
    classify: impl Fn(&K, &T) -> Vec<CallerRoot>,
) -> BTreeMap<K, Entry<T>> {
    let mapped: Vec<_> = records
        .iter()
        .filter_map(|(key, record)| {
            let mappings = classify(key, record);
            (!mappings.is_empty()).then(|| {
                (
                    key.clone(),
                    Entry {
                        record: record.clone(),
                        mappings,
                    },
                )
            })
        })
        .collect();
    for (key, _) in &mapped {
        records.remove(key);
    }
    mapped.into_iter().collect()
}

impl Split {
    pub(crate) fn new(catalog: &Catalog, roots: &[CallerRoot]) -> Self {
        let mut identity = catalog.clone();
        let caller = CallerCatalog {
            workspaces: partition(&mut identity.workspaces, |key, record| {
                mappings(
                    roots,
                    [key.as_str(), record.repository.as_str()]
                        .into_iter()
                        .chain(
                            record
                                .siblings
                                .iter()
                                .map(|s| super::sibling(s).caller.text().as_str()),
                        ),
                )
            }),
            repositories: partition(&mut identity.repositories, |key, record| {
                let mut paths = vec![key.as_str()];
                if let Some(base) = &record.desired {
                    paths.push(&base.workspace);
                    paths.extend(
                        base.siblings
                            .iter()
                            .map(|s| super::sibling(s).caller.text().as_str()),
                    );
                }
                mappings(roots, paths)
            }),
            legacy_checkouts: partition(&mut identity.legacy_checkouts, |key, record| {
                mappings(roots, [key.as_str(), record.repository.as_str()])
            }),
            sibling_commits: partition(&mut identity.sibling_commits, |key, _| {
                mappings(roots, [key.as_str()])
            }),
            transferred: partition(&mut identity.transferred, |_, record| {
                mappings(
                    roots,
                    [record.repository.as_str()].into_iter().chain(
                        record
                            .siblings
                            .iter()
                            .map(|s| super::sibling(s).caller.text().as_str()),
                    ),
                )
            }),
        };
        Self { identity, caller }
    }

    pub(crate) fn read(state: &Path) -> io::Result<Self> {
        Ok(Self {
            identity: read_or_default(&state.join("catalog.json"))?,
            caller: read_or_default(&state.join("caller-catalog.json"))?,
        })
    }

    /// Resolve duplicate keys in memory, preferring catalog.json. Runner startup
    /// must check the mappings in the unmerged caller file before saving this
    /// result, because a discarded copy can be the last reference to a mapping.
    pub(crate) fn loaded(&self) -> Catalog {
        let mut catalog = self.identity.clone();
        for (key, entry) in &self.caller.workspaces {
            if let Some(kept) = catalog.workspaces.get_mut(key) {
                kept.sibling_freshness.clear();
                tracing::warn!(%key, "duplicate workspace in catalog.json and caller-catalog.json; keeping catalog.json and clearing sibling freshness");
            } else {
                catalog.workspaces.insert(key.clone(), entry.record.clone());
            }
        }
        merge(&mut catalog.repositories, &self.caller.repositories);
        merge(&mut catalog.legacy_checkouts, &self.caller.legacy_checkouts);
        merge(&mut catalog.sibling_commits, &self.caller.sibling_commits);
        merge(&mut catalog.transferred, &self.caller.transferred);
        catalog
    }

    /// The union phase publishes additions before either file can lose a key.
    pub(crate) fn save(
        &self,
        state: &Path,
        private: bool,
        mut renamed: impl FnMut(&Path, bool) -> io::Result<()>,
    ) -> io::Result<()> {
        let old = Self::read(state)?;
        let identity = state.join("catalog.json");
        let caller = state.join("caller-catalog.json");
        let new_identity = serde_json::to_vec(&self.identity).map_err(invalid)?;
        let new_caller = serde_json::to_vec(&self.caller).map_err(invalid)?;
        let changed_identity = fs::read(&identity).ok().as_deref() != Some(&new_identity);
        let changed_caller = (!self.caller.is_empty() || caller.exists())
            && fs::read(&caller).ok().as_deref() != Some(&new_caller);
        let mut writes = Vec::new();
        if changed_identity && changed_caller {
            let mut additions = self.clone();
            union(&mut additions.identity.workspaces, old.identity.workspaces);
            union(
                &mut additions.identity.repositories,
                old.identity.repositories,
            );
            union(
                &mut additions.identity.legacy_checkouts,
                old.identity.legacy_checkouts,
            );
            union(
                &mut additions.identity.sibling_commits,
                old.identity.sibling_commits,
            );
            union(
                &mut additions.identity.transferred,
                old.identity.transferred,
            );
            union(&mut additions.caller.workspaces, old.caller.workspaces);
            union(&mut additions.caller.repositories, old.caller.repositories);
            union(
                &mut additions.caller.legacy_checkouts,
                old.caller.legacy_checkouts,
            );
            union(
                &mut additions.caller.sibling_commits,
                old.caller.sibling_commits,
            );
            union(&mut additions.caller.transferred, old.caller.transferred);
            let phase_identity = serde_json::to_vec(&additions.identity).map_err(invalid)?;
            let phase_caller = serde_json::to_vec(&additions.caller).map_err(invalid)?;
            writes.push((&identity, phase_identity.clone()));
            writes.push((&caller, phase_caller.clone()));
            if phase_identity != new_identity {
                writes.push((&identity, new_identity));
            }
            if phase_caller != new_caller {
                writes.push((&caller, new_caller));
            }
        } else {
            if changed_identity {
                writes.push((&identity, new_identity));
            }
            if changed_caller {
                writes.push((&caller, new_caller));
            }
        }
        let count = writes.len();
        if count == 0 {
            return crate::filesystem::sync_directory(state);
        }
        for (index, (path, bytes)) in writes.into_iter().enumerate() {
            storage::atomic_bytes_and(path, &bytes, private, || renamed(path, index + 1 == count))?;
        }
        Ok(())
    }
}

fn merge<K: Ord + Clone + std::fmt::Display, T: Clone>(
    target: &mut BTreeMap<K, T>,
    source: &BTreeMap<K, Entry<T>>,
) {
    for (key, entry) in source {
        if target.contains_key(key) {
            tracing::warn!(%key, "duplicate record in catalog.json and caller-catalog.json; keeping catalog.json");
        } else {
            target.insert(key.clone(), entry.record.clone());
        }
    }
}

fn union<K: Ord, T>(new: &mut BTreeMap<K, T>, old: BTreeMap<K, T>) {
    for (key, record) in old {
        new.entry(key).or_insert(record);
    }
}

fn read_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    match storage::read_json(path) {
        Ok(value) => Ok(value),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(invalid(format!(
            "cannot read workspace catalog {}: {e}",
            path.display()
        ))),
    }
}

pub(crate) fn fixed(roots: &[CallerRoot], pins: &[CallerRoot]) -> io::Result<()> {
    for pin in pins {
        if !roots.iter().any(|root| {
            root.caller_prefix == pin.caller_prefix
                && root.host_root.as_os_str() == pin.host_root.as_os_str()
        }) {
            return Err(invalid(format!("fixed caller mapping {}={} is missing or changed; restore --caller-root {}={}, or run motor-runner forget-caller-root --prefix {} with the runner stopped",
                pin.caller_prefix, pin.host_root.display(), pin.caller_prefix, pin.host_root.display(), pin.caller_prefix)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
