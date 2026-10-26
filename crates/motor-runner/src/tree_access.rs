use std::{
    cell::Cell,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, OnceLock},
    thread::ThreadId,
};

#[derive(Default)]
struct State {
    next: u64,
    active: Vec<(u64, PathBuf, ThreadId)>,
}
static ACCESS: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();

/// Recursive filesystem walks and deletion exclude overlapping trees, while
/// unrelated checkouts remain independent. Nested calls on one thread are safe.
pub(crate) struct TreeAccess {
    id: u64,
    _thread_bound: PhantomData<*const Cell<()>>,
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn acquire(path: crate::caller_path::HostPath<'_>, wait: bool) -> Option<TreeAccess> {
    let path = path
        .as_path()
        .canonicalize()
        .unwrap_or_else(|_| path.as_path().to_path_buf());
    let owner = std::thread::current().id();
    let (lock, wake) = ACCESS.get_or_init(|| (Mutex::new(State::default()), Condvar::new()));
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    while state
        .active
        .iter()
        .any(|(_, active, thread)| *thread != owner && overlaps(active, &path))
    {
        if !wait {
            return None;
        }
        state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
    }
    state.next += 1;
    let id = state.next;
    state.active.push((id, path, owner));
    Some(TreeAccess {
        id,
        _thread_bound: PhantomData,
    })
}
pub(crate) fn lock(path: crate::caller_path::HostPath<'_>) -> TreeAccess {
    crate::setup_log::lock_wait("tree_access", || acquire(path, true).unwrap())
}
pub(crate) fn try_lock(path: crate::caller_path::HostPath<'_>) -> Option<TreeAccess> {
    acquire(path, false)
}
impl Drop for TreeAccess {
    fn drop(&mut self) {
        let (lock, wake) = ACCESS.get().unwrap();
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .retain(|(id, _, _)| *id != self.id);
        wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_walk_waits_for_descendant_delete_but_unrelated_tree_remains_available() {
        let root = cortexkit_test_support::ScratchDir::new("tree-access");
        let child = root.join("retired/child");
        std::fs::create_dir_all(&child).unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn({
            let child = child.clone();
            move || {
                let _access = lock(crate::caller_path::host(&child));
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                std::fs::remove_dir_all(child).unwrap();
            }
        });
        held_rx.recv().unwrap();
        assert!(try_lock(crate::caller_path::host(&root.join("retired"))).is_none());
        assert!(try_lock(crate::caller_path::host(&root.join("unrelated"))).is_some());
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        let _access = lock(crate::caller_path::host(&root.join("retired")));
        assert!(!child.exists());
    }
    #[test]
    fn same_thread_nested_helpers_do_not_deadlock() {
        let root = cortexkit_test_support::ScratchDir::new("tree-access-nested");
        let _outer = lock(crate::caller_path::host(&root));
        let _inner = lock(crate::caller_path::host(&root));
        assert!(try_lock(crate::caller_path::host(&root.join("child"))).is_some());
    }
}
