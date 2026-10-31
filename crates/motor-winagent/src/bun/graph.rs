//! Bound graph work independently of lockfile size: aliases can multiply edges.
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_WORKSPACES: usize = 128;
const MAX_EDGES: usize = 100_000;
const MAX_QUEUE: usize = 32_768;
const MAX_WORK: usize = 400_000;
const MAX_GRAPH_BYTES: usize = 16 * 1024 * 1024;

fn edges(metadata: &Value) -> impl Iterator<Item = &str> {
    [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .into_iter()
    .flat_map(|group| {
        metadata
            .get(group)
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|map| map.keys().map(String::as_str))
    })
}
#[derive(Default)]
struct Work {
    queue: VecDeque<(String, String)>,
    seen: BTreeSet<(String, String)>,
    bytes: usize,
    work: usize,
}
impl Work {
    fn tick(&mut self) -> Result<(), ()> {
        self.work += 1;
        if self.work > MAX_WORK {
            return Err(());
        }
        Ok(())
    }
    fn enqueue(&mut self, parent: &str, name: &str) -> Result<(), ()> {
        self.tick()?;
        let size = parent.len().checked_add(name.len()).ok_or(())?;
        if self.bytes.saturating_add(size) > MAX_GRAPH_BYTES {
            return Err(());
        }
        let edge = (parent.to_owned(), name.to_owned());
        if self.seen.contains(&edge) {
            return Ok(());
        }
        if self.seen.len() >= MAX_EDGES || self.queue.len() >= MAX_QUEUE {
            return Err(());
        }
        self.bytes += size;
        self.seen.insert(edge.clone());
        self.queue.push_back(edge);
        Ok(())
    }
}

pub(super) fn reachable(lock: &Value, rows: &Map<String, Value>) -> Result<BTreeSet<String>, ()> {
    let workspaces = lock
        .get("workspaces")
        .and_then(Value::as_object)
        .filter(|map| map.contains_key(""))
        .ok_or(())?;
    if workspaces.len() > MAX_WORKSPACES {
        return Err(());
    }
    let mut placements = BTreeMap::<&str, Vec<&str>>::new();
    for (placement, row) in rows {
        placements
            .entry(row[0].as_str().ok_or(())?)
            .or_default()
            .push(placement);
    }
    let mut work = Work::default();
    for (path, workspace) in workspaces {
        if !workspace.is_object() {
            return Err(());
        }
        let name = workspace.get("name").and_then(Value::as_str).unwrap_or("");
        let resolution = format!("{name}@workspace:{path}");
        let parents = if path.is_empty() {
            vec![""]
        } else {
            placements
                .get(resolution.as_str())
                .cloned()
                .unwrap_or_else(|| vec![name])
        };
        for parent in parents {
            for edge in edges(workspace) {
                work.enqueue(parent, edge)?;
            }
        }
    }
    let mut reachable = BTreeSet::new();
    while let Some((mut parent, name)) = work.queue.pop_front() {
        let placement = loop {
            work.tick()?;
            let candidate = if parent.is_empty() {
                name.clone()
            } else {
                format!("{parent}/{name}")
            };
            if rows.contains_key(&candidate) {
                break Some(candidate);
            }
            if parent.is_empty() {
                break None;
            }
            parent = parent.rsplit_once('/').map_or("", |p| p.0).into();
        };
        let Some(placement) = placement else { continue };
        if !reachable.insert(placement.clone()) {
            continue;
        }
        let row = &rows[&placement];
        let metadata = if row[1].is_object() { &row[1] } else { &row[2] };
        if !super::platform(metadata, "os", "win32") || !super::platform(metadata, "cpu", "x64") {
            continue;
        }
        for edge in edges(metadata) {
            work.enqueue(&placement, edge)?;
        }
    }
    Ok(reachable)
}
