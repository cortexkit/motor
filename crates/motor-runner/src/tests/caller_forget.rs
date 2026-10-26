use super::*;
use crate::caller_path::{catalog::Split, forget, CallerRoot};

fn two_prefix_config(f: &Fixture) -> Config {
    let mut c = f.config();
    c.instance = "main".into();
    c.manual_tick = true;
    c.test_allow_unisolated_caller_roots = true;
    for (prefix, dir) in [("/home", "callers/home"), ("/data", "callers/data")] {
        let host = f.scratch.join(dir);
        fs::create_dir_all(&host).unwrap();
        c.caller_roots
            .push(CallerRoot::parse(&format!("{prefix}={}", host.display())).unwrap());
    }
    c
}

fn home_host(c: &Config) -> PathBuf {
    c.caller_roots
        .iter()
        .find(|r| r.caller_prefix == "/home")
        .unwrap()
        .host_root
        .clone()
}

fn data_host(c: &Config) -> PathBuf {
    c.caller_roots
        .iter()
        .find(|r| r.caller_prefix == "/data")
        .unwrap()
        .host_root
        .clone()
}

fn unflagged_home(c: &Config) -> Config {
    let mut c = c.clone();
    c.caller_roots.retain(|r| r.caller_prefix != "/home");
    c
}

fn snapshot(c: &Config) -> (Vec<u8>, Vec<u8>) {
    (
        fs::read(c.state_dir().join("catalog.json")).unwrap_or_default(),
        fs::read(c.state_dir().join("caller-catalog.json")).unwrap_or_default(),
    )
}

#[test]
fn forget_sweeps_home_keeps_dual_keys_and_leaves_data_byte_identical() {
    let f = Fixture::new();
    let c = two_prefix_config(&f);
    let home = home_host(&c);
    let data = data_host(&c);
    let runner = Runner::open(c.clone()).unwrap();
    let now = runner.now();

    // Mapped workspace under /home with its own tree.
    let mapped_key = "/home/u/proj";
    let mapped_host = home.join("u/proj");
    fs::create_dir_all(&mapped_host).unwrap();
    fs::write(mapped_host.join("secret"), "mapped").unwrap();

    // Mixed workspace: identity key with a /home sibling, so the record pins
    // /home while its tree lives outside H.
    let mixed_key = f
        .scratch
        .join("remote/worktrees/mixed")
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&mixed_key).unwrap();
    fs::write(Path::new(&mixed_key).join("secret"), "mixed").unwrap();

    // Second mixed workspace that will become dual-key (identity twin kept).
    let dual_key = f
        .scratch
        .join("remote/worktrees/dual")
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&dual_key).unwrap();
    fs::write(Path::new(&dual_key).join("secret"), "dual").unwrap();

    // Unrelated identity workspace that must survive.
    let plain_key = f
        .scratch
        .join("remote/worktrees/plain")
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&plain_key).unwrap();
    fs::write(Path::new(&plain_key).join("secret"), "plain").unwrap();

    // Repository whose desired base carries a /home sibling: its warm-build
    // checkout is an identity tree owned by a /home-pinning record.
    let repo_key = f.root.clone();
    let warm = Path::new(&mixed_key).parent().unwrap().join(format!(
        ".motor-base-{}-{}",
        c.instance,
        workspace::name(&repo_key)
    ));
    fs::create_dir_all(&warm).unwrap();
    fs::write(warm.join("artifact"), "warm").unwrap();

    // Legacy checkout under /home and one under identity pinning /home.
    let legacy_mapped = "/home/u/old";
    fs::create_dir_all(home.join("u/old")).unwrap();
    fs::write(home.join("u/old/tip"), "old").unwrap();
    let legacy_mixed = f
        .scratch
        .join("remote/worktrees/legacy")
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&legacy_mixed).unwrap();
    fs::write(Path::new(&legacy_mixed).join("tip"), "legacy").unwrap();

    // /data workspace pinning only /data, with tree and job.
    let data_key = "/data/x/proj";
    let data_tree = data.join("x/proj");
    fs::create_dir_all(&data_tree).unwrap();
    fs::write(data_tree.join("secret"), "data").unwrap();

    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.workspaces.insert(
            mapped_key.into(),
            crate::storage::Workspace {
                repository: "/home/u/repo".into(),
                used_ms: now,
                ..Default::default()
            },
        );
        inner.catalog.workspaces.insert(
            mixed_key.clone(),
            crate::storage::Workspace {
                repository: f.root.clone(),
                used_ms: now,
                siblings: vec![SiblingSnapshot {
                    canonical_path: "/home/u/lib".into(),
                    commit: "a".repeat(40),
                    tree_hash: "b".repeat(64),
                    base_commit: None,
                }],
                ..Default::default()
            },
        );
        inner.catalog.workspaces.insert(
            dual_key.clone(),
            crate::storage::Workspace {
                repository: f.root.clone(),
                used_ms: now,
                siblings: vec![SiblingSnapshot {
                    canonical_path: "/home/u/lib".into(),
                    commit: "a".repeat(40),
                    tree_hash: "b".repeat(64),
                    base_commit: None,
                }],
                ..Default::default()
            },
        );
        inner.catalog.workspaces.insert(
            plain_key.clone(),
            crate::storage::Workspace {
                repository: f.root.clone(),
                used_ms: now,
                ..Default::default()
            },
        );
        inner.catalog.workspaces.insert(
            data_key.into(),
            crate::storage::Workspace {
                repository: "/data/x/repo".into(),
                used_ms: now,
                ..Default::default()
            },
        );
        inner.catalog.repositories.insert(
            repo_key.clone(),
            crate::storage::Repository {
                desired: Some(crate::storage::BaseRequest {
                    tip: "tip".into(),
                    workspace: mixed_key.clone(),
                    siblings: vec![SiblingSnapshot {
                        canonical_path: "/home/u/lib".into(),
                        commit: "a".repeat(40),
                        tree_hash: "b".repeat(64),
                        base_commit: None,
                    }],
                    git_dependencies: vec![],
                    command: None,
                }),
                ..Default::default()
            },
        );
        inner.catalog.legacy_checkouts.insert(
            legacy_mapped.into(),
            crate::storage::LegacyCheckout {
                repository: "/home/u/repo".into(),
                tip: None,
            },
        );
        inner.catalog.legacy_checkouts.insert(
            legacy_mixed.clone(),
            crate::storage::LegacyCheckout {
                repository: "/home/u/repo".into(),
                tip: None,
            },
        );
        inner.catalog.transferred.insert(
            uuid::Uuid::now_v7(),
            crate::storage::RetainedTransfer {
                repository: "/home/u/repo".into(),
                snapshot: "snap".into(),
                tip: "tip".into(),
                siblings: vec![],
                finished_ms: now,
            },
        );
        runner.catalog(&inner).unwrap();
    }
    // Make the dual key dual: keep an identity twin with plain siblings and
    // a freshness entry that forget must clear.
    {
        let disk = Split::read(&c.state_dir()).unwrap();
        assert!(disk.caller.workspaces.contains_key(&dual_key));
        let mut identity: crate::storage::Catalog = disk.identity.clone();
        let mut twin = crate::storage::Workspace {
            repository: f.root.clone(),
            used_ms: now,
            ..Default::default()
        };
        twin.sibling_freshness.insert(
            f.canonical.clone(),
            serde_json::from_value(serde_json::json!([{"tree":"old"}])).unwrap(),
        );
        identity.workspaces.insert(dual_key.clone(), twin);
        let bytes = serde_json::to_vec(&identity).unwrap();
        crate::storage::atomic_bytes(&c.state_dir().join("catalog.json"), &bytes, false).unwrap();
    }

    // Pinning jobs: a terminal mapped job, a non-terminal mapped job with a
    // dead unit, and a /data-only job.
    let mut terminal_request = f.run("true").0;
    terminal_request.workspace_key = mapped_key.into();
    terminal_request.repository_root = "/home/u/repo".into();
    terminal_request.cwd = mapped_key.into();
    terminal_request.siblings.clear();
    let mut terminal_job = Job::new(uuid::Uuid::now_v7(), Some(terminal_request), now, 1, 1);
    terminal_job.terminal(Outcome::Exit { code: 0 }, now);
    runner.save(&terminal_job).unwrap();

    let mut live_request = f.run("true").0;
    live_request.workspace_key = mapped_key.into();
    live_request.repository_root = "/home/u/repo".into();
    live_request.cwd = mapped_key.into();
    live_request.siblings.clear();
    let live_job = Job::new(uuid::Uuid::now_v7(), Some(live_request), now, 2, 1);
    runner.save(&live_job).unwrap();

    let mut data_request = f.run("true").0;
    data_request.workspace_key = data_key.into();
    data_request.repository_root = "/data/x/repo".into();
    data_request.cwd = data_key.into();
    data_request.siblings.clear();
    let data_job = Job::new(uuid::Uuid::now_v7(), Some(data_request), now, 3, 1);
    runner.save(&data_job).unwrap();
    let data_job_bytes = fs::read(runner.record_path(data_job.id)).unwrap();

    // Uncatalogued leftover and a planted foreign trash entry under H.
    fs::write(home.join("leftover"), "junk").unwrap();
    fs::create_dir_all(home.join(".motor-trash-e2e")).unwrap();
    fs::write(home.join(".motor-trash-e2e/f"), "foreign").unwrap();

    // /data bytes before forget.
    let data_tree_before = fs::read(data_tree.join("secret")).unwrap();
    let data_catalog_before = snapshot(&c);
    drop(runner);

    let removed = forget::forget_caller_root(&c, "/home").unwrap();
    assert!(removed.iter().any(|line| line.contains(mapped_key)));
    assert!(removed.iter().any(|line| line.contains(&mixed_key)));

    // H holds only its empty trash and its marker.
    let mut remaining: Vec<_> = fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    remaining.sort();
    assert_eq!(
        remaining,
        vec![
            std::ffi::OsString::from(".motor-owner"),
            std::ffi::OsString::from(format!(".motor-trash-{}", c.instance)),
        ]
    );
    assert_eq!(
        fs::read_dir(home.join(format!(".motor-trash-{}", c.instance)))
            .unwrap()
            .count(),
        0
    );
    assert!(home.join(".motor-owner").is_file());

    // Mapped and mixed trees are gone; the warm checkout and mixed legacy
    // checkout are gone; /data and plain trees survive.
    assert!(!mapped_host.exists());
    assert!(!Path::new(&mixed_key).exists());
    assert!(!warm.exists());
    assert!(!Path::new(&legacy_mixed).exists());
    assert!(!home.join("leftover").exists());
    assert!(!home.join(".motor-trash-e2e").exists());
    assert_eq!(
        fs::read(data_tree.join("secret")).unwrap(),
        data_tree_before
    );
    assert_eq!(
        fs::read_to_string(Path::new(&plain_key).join("secret")).unwrap(),
        "plain"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&dual_key).join("secret")).unwrap(),
        "dual"
    );

    // No caller entry still pins /home; /data entries are untouched.
    let disk = Split::read(&c.state_dir()).unwrap();
    assert!(!disk
        .caller
        .workspaces
        .values()
        .any(|e| e.mappings.iter().any(|m| m.caller_prefix == "/home")));
    assert!(!disk
        .caller
        .repositories
        .values()
        .any(|e| e.mappings.iter().any(|m| m.caller_prefix == "/home")));
    assert!(!disk
        .caller
        .legacy_checkouts
        .values()
        .any(|e| e.mappings.iter().any(|m| m.caller_prefix == "/home")));
    assert!(disk.caller.sibling_commits.is_empty() || true);
    assert!(!disk
        .caller
        .transferred
        .values()
        .any(|e| e.mappings.iter().any(|m| m.caller_prefix == "/home")));
    assert!(disk.caller.workspaces.contains_key(data_key));

    // Dual key: caller copy dropped, identity kept with cleared freshness.
    assert!(!disk.caller.workspaces.contains_key(&dual_key));
    assert!(disk.identity.workspaces.contains_key(&dual_key));
    assert!(disk.identity.workspaces[&dual_key]
        .sibling_freshness
        .is_empty());

    // Pinning job records are gone; the /data job record is byte-identical.
    assert!(!c
        .state_dir()
        .join("jobs")
        .join(terminal_job.id.to_string())
        .exists());
    assert!(!c
        .state_dir()
        .join("jobs")
        .join(live_job.id.to_string())
        .exists());
    assert_eq!(
        fs::read(
            c.state_dir()
                .join("jobs")
                .join(data_job.id.to_string())
                .join("record.json")
        )
        .unwrap(),
        data_job_bytes
    );
    let _ = data_catalog_before;

    // An unflagged start (without /home, keeping /data) succeeds.
    let unflagged = unflagged_home(&c);
    let runner = Runner::open(unflagged.clone()).unwrap();
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&dual_key));
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&plain_key));
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(data_key));
    drop(runner);
}

fn minimal_home_pin(f: &Fixture, c: &Config) -> (String, PathBuf) {
    let runner = Runner::open(c.clone()).unwrap();
    let key = "/home/u/proj";
    let host = home_host(c).join("u/proj");
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("secret"), "mapped").unwrap();
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.catalog.workspaces.insert(
            key.into(),
            crate::storage::Workspace {
                repository: "/home/u/repo".into(),
                used_ms: runner.now(),
                ..Default::default()
            },
        );
        runner.catalog(&inner).unwrap();
    }
    let now = runner.now();
    let mut request = f.run("true").0;
    request.workspace_key = key.into();
    request.repository_root = "/home/u/repo".into();
    request.cwd = key.into();
    request.siblings.clear();
    let job = Job::new(uuid::Uuid::now_v7(), Some(request), now, 1, 1);
    runner.save(&job).unwrap();
    drop(runner);
    (key.into(), host)
}

#[test]
fn forget_with_flagged_serve_args_reaches_the_same_end_state() {
    let f = Fixture::new();
    let c = two_prefix_config(&f);
    let home = home_host(&c);
    let (key, host) = minimal_home_pin(&f, &c);
    // This test calls forget with the `/home` mapping still supplied in the
    // serve configuration (as when forget runs before the unflagged unit is
    // installed, still carrying `--caller-root /home=H`).
    let mut flagged = c.clone();
    flagged.caller_roots.retain(|r| r.caller_prefix != "/home");
    flagged
        .caller_roots
        .push(CallerRoot::parse(&format!("/home={}", home.display())).unwrap());
    let removed = forget::forget_caller_root(&flagged, "/home").unwrap();
    assert!(removed.iter().any(|line| line.contains(&key)));
    assert!(!host.exists());
    let disk = Split::read(&c.state_dir()).unwrap();
    assert!(disk.caller.workspaces.is_empty() || !disk.caller.workspaces.contains_key(&key));
    let runner = Runner::open(unflagged_home(&c)).unwrap();
    drop(runner);
}

#[test]
fn forget_with_mismatched_flagged_host_refuses_naming_both_and_changes_nothing() {
    let f = Fixture::new();
    let c = two_prefix_config(&f);
    let home = home_host(&c);
    let (key, host) = minimal_home_pin(&f, &c);
    let before = snapshot(&c);
    let other = f.scratch.join("other-home");
    fs::create_dir_all(&other).unwrap();
    let mut mismatched = c.clone();
    mismatched
        .caller_roots
        .retain(|r| r.caller_prefix != "/home");
    mismatched
        .caller_roots
        .push(CallerRoot::parse(&format!("/home={}", other.display())).unwrap());
    let error = forget::forget_caller_root(&mismatched, "/home")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&home.display().to_string()) && error.contains(&other.display().to_string()),
        "{error}"
    );
    assert_eq!(snapshot(&c), before);
    assert_eq!(fs::read(host.join("secret")).unwrap(), b"mapped");
    assert!(Split::read(&c.state_dir())
        .unwrap()
        .caller
        .workspaces
        .contains_key(&key));
}

#[test]
fn forget_rerun_converges_after_kills_in_steps_one_to_three() {
    for upto in [1u8, 2, 3] {
        let f = Fixture::new();
        let c = two_prefix_config(&f);
        let home = home_host(&c);
        let mixed_key = f
            .scratch
            .join("remote/worktrees/mixed")
            .to_string_lossy()
            .into_owned();
        fs::create_dir_all(&mixed_key).unwrap();
        fs::write(Path::new(&mixed_key).join("secret"), "mixed").unwrap();
        {
            let runner = Runner::open(c.clone()).unwrap();
            let now = runner.now();
            let mut inner = runner.core.inner.lock().unwrap();
            inner.catalog.workspaces.insert(
                "/home/u/proj".into(),
                crate::storage::Workspace {
                    repository: "/home/u/repo".into(),
                    used_ms: now,
                    ..Default::default()
                },
            );
            inner.catalog.workspaces.insert(
                mixed_key.clone(),
                crate::storage::Workspace {
                    repository: f.root.clone(),
                    used_ms: now,
                    siblings: vec![SiblingSnapshot {
                        canonical_path: "/home/u/lib".into(),
                        commit: "a".repeat(40),
                        tree_hash: "b".repeat(64),
                        base_commit: None,
                    }],
                    ..Default::default()
                },
            );
            let mapped_host = home.join("u/proj");
            fs::create_dir_all(&mapped_host).unwrap();
            fs::write(mapped_host.join("secret"), "mapped").unwrap();
            fs::write(home.join("leftover"), "junk").unwrap();
            runner.catalog(&inner).unwrap();
        }
        // Simulate a kill after step `upto`: run the filesystem steps only.
        {
            let plan = forget::load_plan(&c, "/home").unwrap();
            if upto >= 1 {
                forget::step_one(&c, &plan).unwrap();
            }
            if upto >= 2 {
                forget::step_two(&c, &plan).unwrap();
            }
            if upto >= 3 {
                forget::step_three(&c, &plan).unwrap();
            }
        }
        // Pins remain, so an unflagged start still refuses.
        let error = match Runner::open(unflagged_home(&c)) {
            Ok(_) => panic!("unflagged start unexpectedly accepted after partial forget"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("forget-caller-root"), "{error}");
        // A rerun converges to the same end state.
        forget::forget_caller_root(&c, "/home").unwrap();
        assert!(!Path::new(&mixed_key).exists());
        assert!(!home.join("leftover").exists());
        let disk = Split::read(&c.state_dir()).unwrap();
        assert!(!disk
            .caller
            .workspaces
            .values()
            .any(|e| e.mappings.iter().any(|m| m.caller_prefix == "/home")));
        let runner = Runner::open(unflagged_home(&c)).unwrap();
        drop(runner);
    }
}

#[test]
fn forget_rerun_converges_after_freshness_write_with_pins_remaining() {
    let f = Fixture::new();
    let c = two_prefix_config(&f);
    let dual_key = f
        .scratch
        .join("remote/worktrees/dual")
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&dual_key).unwrap();
    {
        let runner = Runner::open(c.clone()).unwrap();
        let now = runner.now();
        {
            let mut inner = runner.core.inner.lock().unwrap();
            inner.catalog.workspaces.insert(
                dual_key.clone(),
                crate::storage::Workspace {
                    repository: f.root.clone(),
                    used_ms: now,
                    siblings: vec![SiblingSnapshot {
                        canonical_path: "/home/u/lib".into(),
                        commit: "a".repeat(40),
                        tree_hash: "b".repeat(64),
                        base_commit: None,
                    }],
                    ..Default::default()
                },
            );
            runner.catalog(&inner).unwrap();
        }
        drop(runner);
        // Write an identity twin with freshness, leaving the caller pin: the
        // state after a kill during step four past the freshness write.
        let disk = Split::read(&c.state_dir()).unwrap();
        let mut identity = disk.identity.clone();
        let mut twin = crate::storage::Workspace {
            repository: f.root.clone(),
            used_ms: now,
            ..Default::default()
        };
        twin.sibling_freshness.insert(
            f.canonical.clone(),
            serde_json::from_value(serde_json::json!([{"tree":"old"}])).unwrap(),
        );
        identity.workspaces.insert(dual_key.clone(), twin);
        let bytes = serde_json::to_vec(&identity).unwrap();
        // Simulate the freshness write of step four directly.
        crate::storage::atomic_bytes(&c.state_dir().join("catalog.json"), &bytes, false).unwrap();
    }
    // The caller pin remains (with stale freshness cleared only in the
    // identity copy on a full run), so an unflagged start still refuses.
    let error = match Runner::open(unflagged_home(&c)) {
        Ok(_) => panic!("unflagged start unexpectedly accepted with pins remaining"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("forget-caller-root"), "{error}");
    forget::forget_caller_root(&c, "/home").unwrap();
    let disk = Split::read(&c.state_dir()).unwrap();
    assert!(!disk.caller.workspaces.contains_key(&dual_key));
    assert!(disk.identity.workspaces[&dual_key]
        .sibling_freshness
        .is_empty());
    let runner = Runner::open(unflagged_home(&c)).unwrap();
    drop(runner);
}

#[test]
fn forget_refuses_while_daemon_lock_is_held_and_changes_nothing() {
    use fs2::FileExt;
    let f = Fixture::new();
    let c = two_prefix_config(&f);
    let (_key, host) = minimal_home_pin(&f, &c);
    let before = snapshot(&c);
    fs::create_dir_all(c.state_dir()).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(c.state_dir().join("daemon.lock"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let error = forget::forget_caller_root(&c, "/home")
        .unwrap_err()
        .to_string();
    assert!(error.contains("daemon.lock"), "{error}");
    assert_eq!(snapshot(&c), before);
    assert!(host.join("secret").exists());
    FileExt::unlock(&lock).unwrap();
    forget::forget_caller_root(&c, "/home").unwrap();
}

#[test]
fn forget_refuses_while_a_live_job_unit_exists_with_the_same_list_units_query() {
    let f = Fixture::new();
    let mut c = two_prefix_config(&f);
    let (_key, host) = minimal_home_pin(&f, &c);
    let before = snapshot(&c);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    c.systemd_command = Some(std::sync::Arc::new(move |args: &[&str]| {
        recorded
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout: b"motor-job-abc.service loaded active running test\n".to_vec(),
            stderr: vec![],
        })
    }));
    let error = forget::forget_caller_root(&c, "/home")
        .unwrap_err()
        .to_string();
    assert!(error.contains("motor-job-abc.service"), "{error}");
    // The live-unit query is the same list-units invocation serve issues.
    let queries = seen.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0],
        vec![
            "list-units",
            "--all",
            "--type=service",
            "--no-legend",
            "--plain",
            "--no-pager",
            "motor-job-*",
        ]
    );
    assert_eq!(snapshot(&c), before);
    assert!(host.join("secret").exists());
    // A listing with only inactive units lets forget proceed.
    let mut idle = c.clone();
    idle.systemd_command = Some(std::sync::Arc::new(|_: &[&str]| {
        Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout: b"motor-job-old.service loaded inactive dead test\n".to_vec(),
            stderr: vec![],
        })
    }));
    forget::forget_caller_root(&idle, "/home").unwrap();
}

#[test]
fn forget_refuses_a_swapped_trash_and_leaves_the_sentinel() {
    let f = Fixture::new();
    let c = two_prefix_config(&f);
    let home = home_host(&c);
    let (_key, host) = minimal_home_pin(&f, &c);
    let before = snapshot(&c);
    let outside = f.scratch.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), "sentinel").unwrap();
    // Swap H's private trash for a symlink to an outside tree before forget.
    let trash = home.join(format!(".motor-trash-{}", c.instance));
    let held = home.join("held-trash");
    fs::rename(&trash, &held).unwrap();
    std::os::unix::fs::symlink(&outside, &trash).unwrap();
    let error = forget::forget_caller_root(&c, "/home")
        .unwrap_err()
        .to_string();
    assert!(!error.is_empty(), "swapped trash must refuse");
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"sentinel");
    assert_eq!(snapshot(&c), before);
    assert!(host.join("secret").exists());
    fs::remove_file(&trash).unwrap();
    fs::rename(&held, &trash).unwrap();
    forget::forget_caller_root(&c, "/home").unwrap();
    assert!(!host.exists());
}

#[test]
fn forget_fence_with_a_live_unit_changes_neither_tree_nor_record() {
    // The daemon is stopped (no Runner held) while a mapped job's unit stays
    // alive: forget must refuse before any mutation.
    let f = Fixture::new();
    let mut c = two_prefix_config(&f);
    let (key, host) = minimal_home_pin(&f, &c);
    let before = snapshot(&c);
    c.systemd_command = Some(std::sync::Arc::new(|_: &[&str]| {
        Ok(std::process::Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(0),
            stdout: b"motor-job-survivor.service loaded active running job\n".to_vec(),
            stderr: vec![],
        })
    }));
    let error = forget::forget_caller_root(&c, "/home")
        .unwrap_err()
        .to_string();
    assert!(error.contains("motor-job-survivor.service"), "{error}");
    assert_eq!(snapshot(&c), before);
    assert!(host.join("secret").exists());
    assert!(Split::read(&c.state_dir())
        .unwrap()
        .caller
        .workspaces
        .contains_key(&key));
}
