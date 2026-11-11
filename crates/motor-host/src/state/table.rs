//! A keyed map of durable records whose copies are cheap.
//!
//! The host changes its state by copying the current [`super::Snapshot`],
//! editing the copy and saving it, so a failed write leaves the state the host
//! acts on untouched. A per-job host keeps a day of finished jobs in that
//! snapshot. With plain maps every save copied, compared and freed all of that
//! history, so one save cost time in proportion to the retained records, and a
//! loop that saved once per record cost their square.
//!
//! A [`Table`] keeps its records in two parts: a shared, read-only base, and the
//! records changed since that base. Copying a table copies only the changes
//! and shares the base. Once a save succeeds, [`Table::commit`] folds the
//! changes into the base, in place while the saved table is its only owner,
//! so the next copy starts with no changes again. The journal writes only the
//! changed keys (see [`crate::store`]); each base carries a version number so
//! the journal can tell whether the base it compares against is the one whose
//! contents it last stored.
//!
//! Reading a key looks at the changes first and then the base, and iteration
//! merges both in key order, so a table reads exactly like the map it
//! replaces and serializes to the same JSON object.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{btree_map, BTreeMap};
use std::iter::Peekable;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Every base gets a version no other base has had, so equal versions mean
/// the same contents.
static VERSIONS: AtomicU64 = AtomicU64::new(1);

fn next_version() -> u64 {
    VERSIONS.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
struct Base<V> {
    version: u64,
    entries: BTreeMap<String, V>,
}

pub struct Table<V> {
    base: Arc<Base<V>>,
    /// Keys changed since `base`: `Some` holds the current value, `None`
    /// marks a key the base holds and this table has removed.
    changes: BTreeMap<String, Option<V>>,
    len: usize,
}

impl<V> Default for Table<V> {
    fn default() -> Self {
        Self::from(BTreeMap::new())
    }
}

impl<V> From<BTreeMap<String, V>> for Table<V> {
    fn from(entries: BTreeMap<String, V>) -> Self {
        Self {
            len: entries.len(),
            base: Arc::new(Base {
                version: next_version(),
                entries,
            }),
            changes: BTreeMap::new(),
        }
    }
}

impl<V: Clone> Clone for Table<V> {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            changes: self.changes.clone(),
            len: self.len,
        }
    }
}

impl<V> Table<V> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, key: &str) -> Option<&V> {
        match self.changes.get(key) {
            Some(change) => change.as_ref(),
            None => self.base.entries.get(key),
        }
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    /// Every record in key order, as `(key, value)`.
    pub fn iter(&self) -> Iter<'_, V> {
        Iter {
            base: self.base.entries.iter().peekable(),
            changes: self.changes.iter().peekable(),
            remaining: self.len,
        }
    }
    pub fn keys(&self) -> impl Iterator<Item = &String> + '_ {
        self.iter().map(|(key, _)| key)
    }
    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, value)| value)
    }

    /// The version of the base this table's changes are relative to.
    pub(crate) fn version(&self) -> u64 {
        self.base.version
    }
    /// The base's value for `key`, ignoring this table's changes.
    pub(crate) fn base_get(&self, key: &str) -> Option<&V> {
        self.base.entries.get(key)
    }
    /// The keys changed since the base, in order. A changed key may hold the
    /// same value as the base, for example after `get_mut` without a write.
    pub(crate) fn changed_keys(&self) -> impl Iterator<Item = &String> + '_ {
        self.changes.keys()
    }
}

impl<V: Clone> Table<V> {
    /// A mutable reference to `key`'s value. The first one copies that one
    /// record out of the shared base.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        if !self.changes.contains_key(key) {
            let value = self.base.entries.get(key)?.clone();
            self.changes.insert(key.to_owned(), Some(value));
        }
        self.changes.get_mut(key).and_then(Option::as_mut)
    }
    pub fn insert(&mut self, key: String, value: V) -> Option<V> {
        let base = self.base.entries.get(&key);
        let previous = match self.changes.insert(key, Some(value)) {
            Some(change) => change,
            None => base.cloned(),
        };
        if previous.is_none() {
            self.len += 1;
        }
        previous
    }
    pub fn remove(&mut self, key: &str) -> Option<V> {
        let previous = if self.base.entries.contains_key(key) {
            match self.changes.insert(key.to_owned(), None) {
                Some(change) => change,
                None => self.base.entries.get(key).cloned(),
            }
        } else {
            self.changes.remove(key).flatten()
        };
        if previous.is_some() {
            self.len -= 1;
        }
        previous
    }
    pub fn entry(&mut self, key: String) -> Entry<'_, V> {
        Entry { table: self, key }
    }
    /// Keeps only the records `keep` accepts. Unlike `BTreeMap::retain`,
    /// `keep` sees each value read-only, so the records it keeps are not
    /// copied out of the base.
    pub fn retain(&mut self, mut keep: impl FnMut(&String, &V) -> bool) {
        let removed: Vec<String> = self
            .iter()
            .filter(|(key, value)| !keep(key, value))
            .map(|(key, _)| key.clone())
            .collect();
        for key in removed {
            self.remove(&key);
        }
    }
    pub fn clear(&mut self) {
        let keys: Vec<String> = self.keys().cloned().collect();
        for key in keys {
            self.remove(&key);
        }
    }
    /// Every record, mutably, in key order. This copies every record out of
    /// the base, so it costs as much as the whole table: use it only on small
    /// tables, and `get_mut` on the keys that need a change otherwise.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut V)> + '_ {
        let missing: Vec<(String, V)> = self
            .base
            .entries
            .iter()
            .filter(|(key, _)| !self.changes.contains_key(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for (key, value) in missing {
            self.changes.insert(key, Some(value));
        }
        self.changes
            .iter_mut()
            .filter_map(|(key, value)| Some((key, value.as_mut()?)))
    }
    /// Every value, mutably; copies every record, as [`Table::iter_mut`] does.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> + '_ {
        self.iter_mut().map(|(_, value)| value)
    }

    /// Folds the changes into the base after a successful save. In place when
    /// this table is the base's only owner; otherwise the base is copied
    /// first, which costs the whole table, so a caller keeping an older copy
    /// of the snapshot across saves pays for it here.
    pub(crate) fn commit(&mut self) {
        if self.changes.is_empty() {
            return;
        }
        let base = Arc::make_mut(&mut self.base);
        for (key, change) in std::mem::take(&mut self.changes) {
            match change {
                Some(value) => drop(base.entries.insert(key, value)),
                None => drop(base.entries.remove(&key)),
            }
        }
        base.version = next_version();
    }
}

pub struct Entry<'a, V> {
    table: &'a mut Table<V>,
    key: String,
}

impl<'a, V: Clone> Entry<'a, V> {
    pub fn or_insert_with(self, value: impl FnOnce() -> V) -> &'a mut V {
        let Entry { table, key } = self;
        if !table.contains_key(&key) {
            table.insert(key.clone(), value());
        }
        table.get_mut(&key).expect("entry present")
    }
    pub fn or_insert(self, value: V) -> &'a mut V {
        self.or_insert_with(|| value)
    }
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}

/// The merged records of a [`Table`] in key order.
pub struct Iter<'a, V> {
    base: Peekable<btree_map::Iter<'a, String, V>>,
    changes: Peekable<btree_map::Iter<'a, String, Option<V>>>,
    remaining: usize,
}

impl<'a, V> Iterator for Iter<'a, V> {
    type Item = (&'a String, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let base = self.base.peek().map(|(key, _)| *key);
            let change = self.changes.peek().map(|(key, _)| *key);
            let from_change = match (base, change) {
                (None, None) => return None,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some(base), Some(change)) => {
                    if base == change {
                        // The change replaces or removes the base record.
                        self.base.next();
                    }
                    change <= base
                }
            };
            let item = if from_change {
                let (key, change) = self.changes.next().expect("peeked");
                change.as_ref().map(|value| (key, value))
            } else {
                self.base.next()
            };
            if let Some(item) = item {
                self.remaining = self.remaining.saturating_sub(1);
                return Some(item);
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<V> ExactSizeIterator for Iter<'_, V> {}

impl<'a, V> IntoIterator for &'a Table<V> {
    type Item = (&'a String, &'a V);
    type IntoIter = Iter<'a, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<V> std::ops::Index<&str> for Table<V> {
    type Output = V;
    fn index(&self, key: &str) -> &V {
        self.get(key).expect("no entry found for key")
    }
}

impl<V> std::ops::Index<&String> for Table<V> {
    type Output = V;
    fn index(&self, key: &String) -> &V {
        &self[key.as_str()]
    }
}

impl<V: Clone> FromIterator<(String, V)> for Table<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(entries: I) -> Self {
        Self::from(entries.into_iter().collect::<BTreeMap<_, _>>())
    }
}

impl<V: Clone> Extend<(String, V)> for Table<V> {
    fn extend<I: IntoIterator<Item = (String, V)>>(&mut self, entries: I) {
        for (key, value) in entries {
            self.insert(key, value);
        }
    }
}

impl<V: PartialEq> PartialEq for Table<V> {
    /// Two copies of one base differ only in their changed keys, so only
    /// those are compared.
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        if self.base.version == other.base.version {
            return self
                .changes
                .keys()
                .chain(other.changes.keys())
                .all(|key| self.get(key) == other.get(key));
        }
        self.iter().eq(other.iter())
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for Table<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<V: Serialize> Serialize for Table<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Table<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(&str, u32)]) -> Table<u32> {
        entries.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    fn pairs(table: &Table<u32>) -> Vec<(String, u32)> {
        table.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    #[test]
    fn a_copy_reads_like_a_map_and_leaves_the_original_untouched() {
        let original = table(&[("a", 1), ("c", 3), ("e", 5)]);
        let mut copy = original.clone();
        copy.insert("b".into(), 2);
        *copy.get_mut("c").unwrap() = 30;
        copy.remove("e");
        copy.insert("f".into(), 6);
        assert_eq!(
            pairs(&copy),
            [("a", 1), ("b", 2), ("c", 30), ("f", 6)].map(|(k, v)| (k.to_owned(), v))
        );
        assert_eq!(copy.len(), 4);
        assert_eq!(copy.iter().len(), 4);
        assert_eq!(copy.get("e"), None);
        assert_eq!(copy["c"], 30);
        assert_eq!(
            pairs(&original),
            pairs(&table(&[("a", 1), ("c", 3), ("e", 5)]))
        );
        assert_eq!(original.len(), 3);
        assert_ne!(copy, original);
        let expected = table(&[("a", 1), ("b", 2), ("c", 30), ("f", 6)]);
        assert_eq!(copy, expected);
        assert_eq!(
            serde_json::to_string(&copy).unwrap(),
            r#"{"a":1,"b":2,"c":30,"f":6}"#
        );
    }

    #[test]
    fn remove_and_reinsert_and_entry_track_length() {
        let mut t = table(&[("a", 1)]);
        assert_eq!(t.remove("a"), Some(1));
        assert_eq!(t.remove("a"), None);
        assert!(t.is_empty());
        assert_eq!(t.insert("a".into(), 2), None);
        assert_eq!(t.insert("a".into(), 3), Some(2));
        assert_eq!(t.remove("x"), None);
        *t.entry("b".into()).or_default() += 4;
        *t.entry("b".into()).or_default() += 4;
        assert_eq!(pairs(&t), [("a".to_owned(), 3), ("b".to_owned(), 8)]);
        assert_eq!(t.len(), 2);
        t.retain(|_, v| *v > 5);
        assert_eq!(pairs(&t), [("b".to_owned(), 8)]);
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.iter().count(), 0);
    }

    #[test]
    fn commit_folds_changes_in_place_and_gives_a_new_version() {
        let mut t = table(&[("a", 1), ("b", 2)]);
        let version = t.version();
        let base = Arc::as_ptr(&t.base);
        t.insert("c".into(), 3);
        t.remove("a");
        t.commit();
        assert!(t.changes.is_empty());
        assert_ne!(t.version(), version);
        assert_eq!(
            Arc::as_ptr(&t.base),
            base,
            "an unshared base is updated in place"
        );
        assert_eq!(pairs(&t), [("b".to_owned(), 2), ("c".to_owned(), 3)]);

        let kept = t.clone();
        t.insert("d".into(), 4);
        t.commit();
        assert_eq!(pairs(&kept), [("b".to_owned(), 2), ("c".to_owned(), 3)]);
        assert_eq!(t.len(), 3);
        assert_ne!(t.version(), kept.version());
    }

    #[test]
    fn iter_mut_sees_every_record() {
        let mut t = table(&[("a", 1), ("b", 2)]);
        t.insert("c".into(), 3);
        for value in t.values_mut() {
            *value *= 10;
        }
        assert_eq!(
            pairs(&t),
            [("a", 10), ("b", 20), ("c", 30)].map(|(k, v)| (k.to_owned(), v))
        );
    }
}
