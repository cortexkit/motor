use super::*;
use crate::caller_path::{
    catalog::{CallerCatalog, Entry, Split},
    CallerRoot,
};

fn configured(f: &Fixture) -> Config {
    let mut c = f.config();
    c.manual_tick = true;
    c.test_allow_unisolated_caller_roots = true;
    let host = f.scratch.join("callers/home");
    fs::create_dir_all(&host).unwrap();
    c.caller_roots
        .push(CallerRoot::parse(&format!("/home={}", host.display())).unwrap());
    c
}

fn refusal(c: Config) -> String {
    match Runner::open(c) {
        Ok(_) => panic!("startup unexpectedly accepted"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn same_mapping_restart_preserves_catalog_and_retained_job_pins() {
    let f = Fixture::new();
    let c = configured(&f);
    let runner = Runner::open(c.clone()).unwrap();
    let mut catalog = Catalog::default();
    catalog.workspaces.insert(
        "/home/u/proj".into(),
        crate::storage::Workspace {
            repository: "/home/u/repo".into(),
            used_ms: runner.now(),
            ..Default::default()
        },
    );
    catalog.workspaces.insert(
        f.key.clone(),
        crate::storage::Workspace {
            repository: f.root.clone(),
            used_ms: runner.now(),
            siblings: vec![SiblingSnapshot {
                canonical_path: "/home/u/lib".into(),
                commit: "a".repeat(40),
                tree_hash: "b".repeat(64),
                base_commit: None,
            }],
            ..Default::default()
        },
    );
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog = catalog;
        runner.catalog(&inner).unwrap();
    }
    let (mut request, _) = f.run("true");
    request.workspace_key = "/home/u/proj".into();
    request.repository_root = "/home/u/repo".into();
    request.cwd = "/home/u/proj".into();
    request.siblings.clear();
    let mut job = Job::new(Uuid::now_v7(), Some(request), runner.now(), 1, 1);
    job.terminal(Outcome::Exit { code: 0 }, runner.now());
    runner.save(&job).unwrap();
    let record_path = runner.record_path(job.id);
    assert_eq!(
        read_json::<Job>(&record_path).unwrap().mappings,
        c.caller_roots
    );
    let sentinel = c.state_dir().join("caller-roots-ever-enabled");
    assert!(sentinel.is_file());
    drop(runner);
    let runner = reopen(c.clone());
    assert_eq!(
        runner.core.inner.lock().unwrap().catalog.workspaces.len(),
        2
    );
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&job.id].mappings,
        c.caller_roots
    );
    assert!(Split::read(&c.state_dir())
        .unwrap()
        .identity
        .workspaces
        .is_empty());
    drop(runner);
    let files = ["catalog.json", "caller-catalog.json"];
    let saved: Vec<_> = files
        .iter()
        .map(|p| fs::read(c.state_dir().join(p)).unwrap())
        .collect();
    let job_bytes = fs::read(&record_path).unwrap();
    for mode in 0..3 {
        let mut changed = c.clone();
        match mode {
            0 => changed.caller_roots.clear(),
            1 => {
                let other = f.scratch.join("other-home");
                fs::create_dir_all(&other).unwrap();
                changed.caller_roots[0].host_root = other;
            }
            2 => changed.caller_roots[0].caller_prefix = "/data".into(),
            _ => unreachable!(),
        }
        let error = refusal(changed);
        assert!(
            error.contains("/home")
                && error.contains("restore --caller-root")
                && error.contains("forget-caller-root"),
            "{error}"
        );
        for (path, bytes) in files.iter().zip(&saved) {
            assert_eq!(&fs::read(c.state_dir().join(path)).unwrap(), bytes);
        }
        assert_eq!(fs::read(&record_path).unwrap(), job_bytes);
    }
    drop(reopen(c));
}

#[test]
fn sole_dual_key_pin_refuses_before_cleanup_in_both_move_directions() {
    for toward_mapped in [false, true] {
        let f = Fixture::new();
        let c = configured(&f);
        let runner = Runner::open(c.clone()).unwrap();
        drop(runner);
        let mut identity = Catalog::default();
        let mut record = crate::storage::Workspace {
            repository: f.root.clone(),
            used_ms: (c.clock)(),
            ..Default::default()
        };
        record.sibling_freshness.insert(
            f.canonical.clone(),
            serde_json::from_value(serde_json::json!([{"tree":"stale"}])).unwrap(),
        );
        identity.workspaces.insert(f.key.clone(), record.clone());
        record.siblings.push(SiblingSnapshot {
            canonical_path: "/home/u/lib".into(),
            commit: "a".repeat(40),
            tree_hash: "b".repeat(64),
            base_commit: None,
        });
        let mut mixed = identity.clone();
        mixed.workspaces.insert(f.key.clone(), record);
        let (before, after) = if toward_mapped {
            (&identity, &mixed)
        } else {
            (&mixed, &identity)
        };
        Split::new(before, &c.caller_roots)
            .save(&c.state_dir(), false, |_, _| Ok(()))
            .unwrap();
        let mut writes = 0;
        assert!(Split::new(after, &c.caller_roots)
            .save(&c.state_dir(), false, |_, _| {
                writes += 1;
                if writes == 2 {
                    Err(io::Error::other("simulated death after additions"))
                } else {
                    Ok(())
                }
            })
            .is_err());
        let disk = Split::read(&c.state_dir()).unwrap();
        assert!(disk.identity.workspaces.contains_key(&f.key));
        assert!(disk.caller.workspaces.contains_key(&f.key));
        let a = fs::read(c.state_dir().join("catalog.json")).unwrap();
        let b = fs::read(c.state_dir().join("caller-catalog.json")).unwrap();
        let mut unflagged = c.clone();
        unflagged.caller_roots.clear();
        assert!(refusal(unflagged).contains("forget-caller-root"));
        assert_eq!(fs::read(c.state_dir().join("catalog.json")).unwrap(), a);
        assert_eq!(
            fs::read(c.state_dir().join("caller-catalog.json")).unwrap(),
            b
        );
        let runner = reopen(c.clone());
        let inner = runner.core.inner.lock().unwrap();
        assert!(inner.catalog.workspaces[&f.key].siblings.is_empty());
        assert!(inner.catalog.workspaces[&f.key]
            .sibling_freshness
            .is_empty());
        assert!(Split::read(&c.state_dir())
            .unwrap()
            .caller
            .workspaces
            .is_empty());
    }
}

#[test]
fn probe_failure_with_dual_key_preserves_catalog_trash_and_absent_sentinel() {
    for failure in ["st_dev", "FICLONE"] {
        let f = Fixture::new();
        let mut c = configured(&f);
        crate::caller_path::owner::claim(&c).unwrap();
        let state = c.state_dir();
        fs::create_dir_all(&state).unwrap();
        let mut identity = Catalog::default();
        let record = crate::storage::Workspace {
            repository: f.root.clone(),
            used_ms: (c.clock)(),
            ..Default::default()
        };
        identity.workspaces.insert(f.key.clone(), record.clone());
        let mut caller = CallerCatalog::default();
        caller.workspaces.insert(
            f.key.clone(),
            Entry {
                record,
                mappings: c.caller_roots.clone(),
            },
        );
        atomic_json(&state.join("catalog.json"), &identity).unwrap();
        atomic_json(&state.join("caller-catalog.json"), &caller).unwrap();
        let trash = c.caller_roots[0]
            .host_root
            .join(".motor-trash-e2e/leftover");
        fs::write(&trash, "unchanged").unwrap();
        let a = fs::read(state.join("catalog.json")).unwrap();
        let b = fs::read(state.join("caller-catalog.json")).unwrap();
        let marker = fs::read(c.caller_roots[0].host_root.join(".motor-owner")).unwrap();
        c.caller_root_probe = Some(Arc::new(move |_| Err(io::Error::other(failure))));
        assert!(refusal(c.clone()).contains(failure));
        assert_eq!(fs::read(state.join("catalog.json")).unwrap(), a);
        assert_eq!(fs::read(state.join("caller-catalog.json")).unwrap(), b);
        assert_eq!(fs::read(&trash).unwrap(), b"unchanged");
        assert_eq!(
            fs::read(c.caller_roots[0].host_root.join(".motor-owner")).unwrap(),
            marker
        );
        assert!(!state.join("caller-roots-ever-enabled").exists());
        c.caller_root_probe = None;
        let runner = reopen(c);
        runner.wait_for_background_deletes(Duration::from_secs(10));
        assert!(!trash.exists());
        assert!(state.join("caller-roots-ever-enabled").is_file());
    }
}

#[test]
fn marker_mismatch_start_does_not_create_state_directory() {
    let f = Fixture::new();
    let c = configured(&f);
    crate::caller_path::owner::claim(&c).unwrap();
    let mut other = c.clone();
    other.state_root = f.scratch.join("attempted-state");
    let error = refusal(other.clone());
    assert!(
        error.contains(".motor-owner") && error.contains("found instance"),
        "{error}"
    );
    assert!(!other.state_root.exists());
}

#[test]
fn partial_two_file_save_finishes_newer_snapshot_before_older_snapshot() {
    let f = Fixture::new();
    let mut c = configured(&f);
    let armed = Arc::new(AtomicBool::new(false));
    let panic_before_write = Arc::new(AtomicBool::new(false));
    let panic_hook = panic_before_write.clone();
    c.long_work = Some(Arc::new(move |work| {
        if work == "catalog_save" && panic_hook.swap(false, Ordering::SeqCst) {
            panic!("injected panic before retrying a partial save");
        }
    }));
    let fault = armed.clone();
    c.catalog_write = Some(Arc::new(move |step| {
        if step == CatalogWrite::PhaseRenamed && fault.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected partial save failure"));
        }
        Ok(())
    }));
    let runner = Runner::open(c.clone()).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    let initial = crate::storage::Workspace {
        repository: f.root.clone(),
        used_ms: runner.now(),
        ..Default::default()
    };
    inner.catalog.workspaces.insert(f.key.clone(), initial);
    runner.catalog(&inner).unwrap();
    let older = runner.catalog_snapshot(&inner).unwrap();
    inner
        .catalog
        .workspaces
        .get_mut(&f.key)
        .unwrap()
        .siblings
        .push(SiblingSnapshot {
            canonical_path: "/home/u/lib".into(),
            commit: "a".repeat(40),
            tree_hash: "b".repeat(64),
            base_commit: None,
        });
    let newer = runner.catalog_snapshot(&inner).unwrap();
    drop(inner);
    armed.store(true, Ordering::SeqCst);
    assert!(runner.write_catalog(newer).is_err());
    panic_before_write.store(true, Ordering::SeqCst);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || runner.write_catalog(older.clone())
    ))
    .is_err());
    runner.write_catalog(older).unwrap();
    let disk = Split::read(&c.state_dir()).unwrap();
    assert!(!disk.identity.workspaces.contains_key(&f.key));
    assert_eq!(
        disk.caller.workspaces[&f.key].record.siblings[0].canonical_path,
        "/home/u/lib"
    );
}
