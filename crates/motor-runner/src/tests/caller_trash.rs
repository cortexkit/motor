use super::*;

fn configured(f: &Fixture) -> Config {
    let mut config = f.config();
    config.test_allow_unisolated_caller_roots = true;
    config.manual_tick = true;
    let host = f.scratch.join("callers/home");
    fs::create_dir_all(&host).unwrap();
    config.caller_roots = vec![crate::CallerRoot {
        caller_prefix: "/home".into(),
        host_root: host,
    }];
    config
}

fn plant(runner: &Runner, key: &str) -> PathBuf {
    let host = paths::host_path(&runner.core.config, key);
    fs::create_dir_all(&host).unwrap();
    fs::write(host.join("secret"), key).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    inner.catalog.workspaces.insert(
        key.into(),
        crate::storage::Workspace {
            repository: key.into(),
            used_ms: runner.now(),
            ..Default::default()
        },
    );
    runner.catalog(&inner).unwrap();
    host
}

fn trash(config: &Config) -> PathBuf {
    config.caller_roots[0]
        .host_root
        .join(format!(".motor-trash-{}", config.instance))
}

#[tokio::test]
async fn mapped_drop_renames_into_h_trash_even_on_states_mount_then_purges() {
    let f = Fixture::new();
    let mut config = configured(&f);
    let hold = Hold::new("remove");
    let _release = Release(hold.clone());
    config.long_work = Some(hold.hook());
    let runner = Runner::open(config.clone()).unwrap();
    let key = "/home/motor-trash-fixture/drop";
    let host = plant(&runner, key);
    assert_eq!(
        crate::filesystem::device(&host).unwrap(),
        crate::filesystem::device(&config.state_dir()).unwrap()
    );
    hold.arm();
    assert!(runner.drop_workspace(key).await.unwrap().dropped);
    assert!(poll(Duration::from_secs(10), || hold.held()));
    assert!(!host.exists());
    let entries: Vec<_> = fs::read_dir(trash(&config))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(fs::read_to_string(entries[0].join("secret")).unwrap(), key);
    assert!(!config.state_dir().join("trash").exists());
    assert!(fs::read_dir(host.parent().unwrap())
        .unwrap()
        .next()
        .is_none());
    hold.release();
    runner.wait_for_background_deletes(Duration::from_secs(10));
    assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
    fs::create_dir(trash(&config).join("leftover")).unwrap();
    fs::write(trash(&config).join("leftover/f"), "old").unwrap();
    drop(runner);
    let runner = reopen(config.clone());
    assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(key));
}

#[tokio::test]
async fn mapped_deletion_crash_boundaries_keep_record_before_write_and_never_restore_after() {
    for boundary in [CatalogWrite::Starting, CatalogWrite::Renamed] {
        let f = Fixture::new();
        let mut config = configured(&f);
        let armed = Arc::new(AtomicBool::new(false));
        let fault = armed.clone();
        config.catalog_write = Some(Arc::new(move |step| {
            if step == boundary && fault.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected deletion crash"));
            }
            Ok(())
        }));
        let runner = Runner::open(config.clone()).unwrap();
        let key = "/home/motor-trash-fixture/crash";
        let host = plant(&runner, key);
        armed.store(true, Ordering::SeqCst);
        assert!(runner.drop_workspace(key).await.is_err());
        assert!(!host.exists());
        runner.wait_for_background_deletes(Duration::from_secs(10));
        drop(runner);
        config.catalog_write = None;
        let runner = reopen(config.clone());
        let survives = boundary == CatalogWrite::Starting;
        assert_eq!(
            runner
                .core
                .inner
                .lock()
                .unwrap()
                .catalog
                .workspaces
                .contains_key(key),
            survives,
            "{boundary:?}"
        );
        let split = paths::catalog::Split::read(&config.state_dir()).unwrap();
        assert!(!split.identity.workspaces.contains_key(key));
        assert_eq!(split.caller.workspaces.contains_key(key), survives);
        assert!(!host.exists());
    }
}

#[test]
fn mapped_expiry_and_pressure_use_host_paths_and_private_trash() {
    for pressure in [false, true] {
        let f = Fixture::new();
        let mut config = configured(&f);
        let free = Arc::new(AtomicU64::new(u64::MAX));
        let probe = free.clone();
        let key = "/home/motor-trash-fixture/idle";
        let host = paths::host_path(&config, key);
        let observed = host.clone();
        config.free_space_floor_bytes = 1;
        config.free_space_eviction_bytes = Some(2);
        config.free_space_probe = Arc::new(move |_| {
            Ok(if observed.exists() {
                probe.load(Ordering::SeqCst)
            } else {
                u64::MAX
            })
        });
        let hold = Hold::new("remove");
        let _release = Release(hold.clone());
        config.long_work = Some(hold.hook());
        let runner = Runner::open(config.clone()).unwrap();
        let host = plant(&runner, key);
        hold.arm();
        if pressure {
            free.store(0, Ordering::SeqCst);
            let _ = runner.ensure_disk_space().unwrap();
        } else {
            f.clock.fetch_add(RETENTION_MS + 1, Ordering::SeqCst);
            runner.maintenance().unwrap();
        }
        assert!(
            poll(Duration::from_secs(10), || hold.held()),
            "pressure={pressure}"
        );
        assert!(!host.exists());
        assert!(!runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .contains_key(key));
        assert!(fs::read_dir(trash(&config)).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .join("secret")
            .exists()));
        assert!(!config.state_dir().join("trash").exists());
        hold.release();
        runner.wait_for_background_deletes(Duration::from_secs(10));
        assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
    }
}

#[test]
fn mapped_repository_retirement_detaches_head_and_derived_fill_checkout() {
    let f = Fixture::new();
    let mut config = configured(&f);
    config.disable_warm_builds = true;
    let runner = Runner::open(config.clone()).unwrap();
    let key = "/home/motor-trash-fixture/repo";
    let host = plant(&runner, key);
    let fill = host.parent().unwrap().join(format!(
        ".motor-base-{}-{}",
        config.instance,
        workspace::name(key)
    ));
    fs::create_dir(&fill).unwrap();
    fs::write(fill.join("warm"), "artifact").unwrap();
    let mirror = runner.core.trees.ensure_mirror(key).unwrap();
    let old = runner.now() - 31 * 24 * 60 * 60 * 1000;
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let head = inner.catalog.workspaces.get_mut(key).unwrap();
        head.kind = WorkspaceKind::Head;
        head.used_ms = old;
        inner.catalog.repositories.insert(
            key.into(),
            Repository {
                finished_ms: Some(old),
                result: Some(RebuildResult::Ok),
                desired: Some(BaseRequest {
                    tip: "tip".into(),
                    workspace: key.into(),
                    siblings: vec![],
                    git_dependencies: vec![],
                    command: None,
                }),
                ..Default::default()
            },
        );
        inner
            .catalog
            .mirror_used_ms
            .insert(workspace::name(key), old);
        runner.catalog(&inner).unwrap();
    }
    runner.retire_unused_repositories().unwrap();
    assert!(!host.exists());
    assert!(!fill.exists());
    assert!(!mirror.exists());
    let inner = runner.core.inner.lock().unwrap();
    assert!(!inner.catalog.workspaces.contains_key(key));
    assert!(!inner.catalog.repositories.contains_key(key));
    drop(inner);
    runner.wait_for_background_deletes(Duration::from_secs(10));
    assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
}

#[test]
fn mapped_pressure_protects_active_host_claims_and_collects_fresh_trash_garbage() {
    let f = Fixture::new();
    let mut config = configured(&f);
    let pressure = Arc::new(AtomicBool::new(false));
    let probe = pressure.clone();
    config.free_space_floor_bytes = 1;
    config.free_space_probe = Arc::new(move |_| {
        Ok(if probe.load(Ordering::SeqCst) {
            0
        } else {
            u64::MAX
        })
    });
    let runner = Runner::open(config.clone()).unwrap();
    let key = "/home/motor-trash-fixture/protected";
    let host = plant(&runner, key);
    let (mut request, _) = f.run("true");
    request.workspace_key = key.into();
    request.repository_root = key.into();
    request.cwd = key.into();
    request.siblings.clear();
    let garbage = trash(&config).join("fresh-garbage");
    fs::create_dir(&garbage).unwrap();
    fs::write(garbage.join("f"), "garbage").unwrap();
    let link = trash(&config).join("outside-link");
    std::os::unix::fs::symlink(&host, &link).unwrap();
    fs::write(trash(&config).join("file"), "garbage").unwrap();
    let _admission = runner.protect_admission(&mut runner.core.inner.lock().unwrap(), &request);
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_some());
    assert_eq!(fs::read_to_string(host.join("secret")).unwrap(), key);
    assert!(!garbage.exists());
    assert!(fs::symlink_metadata(link).is_err());
    assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
    assert!(
        crate::writer::validate_paths(&config, paths::host(&trash(&config)), None, &[], &[],)
            .is_err()
    );
}

#[test]
fn mapped_detach_refuses_swapped_ancestor_and_keeps_purge_anchored_to_trash_fd() {
    let f = Fixture::new();
    let config = configured(&f);
    let runner = Runner::open(config.clone()).unwrap();
    let key = "/home/motor-trash-fixture/work";
    let host = plant(&runner, key);
    let root = &config.caller_roots[0];
    let outside = f.scratch.join("outside");
    fs::create_dir_all(outside.join("work")).unwrap();
    fs::write(outside.join("work/secret"), "sentinel").unwrap();
    let parent = host.parent().unwrap();
    let held = root.host_root.join("held");
    fs::rename(parent, &held).unwrap();
    std::os::unix::fs::symlink(&outside, parent).unwrap();
    assert!(paths::trash::detach(&config, root, paths::host(&host)).is_err());
    assert_eq!(fs::read(outside.join("work/secret")).unwrap(), b"sentinel");
    fs::remove_file(parent).unwrap();
    fs::rename(held, parent).unwrap();
    let retired = paths::trash::detach(&config, root, paths::host(&host))
        .unwrap()
        .unwrap();
    let old_trash = root.host_root.join("old-trash");
    fs::rename(trash(&config), &old_trash).unwrap();
    std::os::unix::fs::symlink(&outside, trash(&config)).unwrap();
    retired.remove(false, false).unwrap();
    assert!(fs::read_dir(old_trash).unwrap().next().is_none());
    assert_eq!(fs::read(outside.join("work/secret")).unwrap(), b"sentinel");
}

#[test]
fn mapped_trash_requires_runner_private_mode_and_never_falls_back() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let config = configured(&f);
    let runner = Runner::open(config.clone()).unwrap();
    assert_eq!(
        fs::metadata(trash(&config)).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let key = "/home/motor-trash-fixture/work";
    let host = plant(&runner, key);
    fs::set_permissions(trash(&config), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(runner.discard(&host).is_err());
    assert!(host.join("secret").exists());
    assert!(!config.state_dir().join("trash").exists());
    drop(runner);
    assert!(Runner::open(config).is_err());
}

async fn swapped_deletion_ancestor(isolated: bool) {
    for pressure in [false, true] {
        let f = Fixture::new();
        let mut config = configured(&f);
        config.isolate_jobs = isolated;
        config.test_allow_unisolated_caller_roots = !isolated;
        let low = Arc::new(AtomicBool::new(false));
        let probe = low.clone();
        config.free_space_floor_bytes = 1;
        config.free_space_probe = Arc::new(move |_| {
            Ok(if probe.load(Ordering::SeqCst) {
                0
            } else {
                u64::MAX
            })
        });
        let hold = Hold::new(if pressure {
            "disk_remove"
        } else {
            "mapped-detach"
        });
        let _release = Release(hold.clone());
        config.long_work = Some(hold.hook());
        let runner = Runner::open(config.clone()).unwrap();
        let key = "/home/motor-trash-fixture/work";
        let host = plant(&runner, key);
        let outside = f.scratch.join("outside");
        fs::create_dir_all(outside.join("work")).unwrap();
        fs::write(outside.join("work/secret"), "sentinel").unwrap();
        hold.arm();
        low.store(pressure, Ordering::SeqCst);
        let worker = runner.clone();
        let deletion = tokio::spawn(async move {
            if pressure {
                worker
                    .off_runtime(|runner| {
                        assert!(runner.ensure_disk_space()?.is_some());
                        Ok(())
                    })
                    .await
            } else {
                worker.drop_workspace(key).await.map(|_| ())
            }
        });
        until(|| hold.held()).await;
        let parent = host.parent().unwrap();
        let held = config.caller_roots[0].host_root.join("held");
        fs::rename(parent, &held).unwrap();
        std::os::unix::fs::symlink(&outside, parent).unwrap();
        hold.release();
        let result = deletion.await.unwrap();
        assert_eq!(result.is_ok(), pressure);
        assert_eq!(fs::read(outside.join("work/secret")).unwrap(), b"sentinel");
        assert_eq!(fs::read_to_string(held.join("work/secret")).unwrap(), key);
        assert!(runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .contains_key(key));
        assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
        fs::remove_file(parent).unwrap();
        fs::rename(held, parent).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapped_drop_and_eviction_refuse_check_use_ancestor_swaps() {
    swapped_deletion_ancestor(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_mapped_drop_and_eviction_refuse_check_use_ancestor_swaps() {
    swapped_deletion_ancestor(true).await;
}

async fn only_caller_root_lifecycle(isolated: bool) {
    for operation in ["drop", "expire", "pressure"] {
        let f = Fixture::new();
        let mut config = configured(&f);
        config.test_allow_unisolated_caller_roots = !isolated;
        config.isolate_jobs = isolated;
        config.manual_tick = false;
        let key = format!("/home/motor-trash-fixture/{operation}/work");
        let repository = format!("/home/motor-trash-fixture/{operation}/repo");
        let host = paths::host_path(&config, &key);
        let pressure = Arc::new(AtomicBool::new(false));
        let probe = pressure.clone();
        let observed = host.clone();
        config.free_space_floor_bytes = 1;
        config.free_space_eviction_bytes = Some(2);
        config.free_space_probe = Arc::new(move |_| {
            Ok(if probe.load(Ordering::SeqCst) && observed.exists() {
                0
            } else {
                u64::MAX
            })
        });
        let hold = Hold::new("remove");
        let _release = Release(hold.clone());
        config.long_work = Some(hold.hook());
        let runner = Runner::open(config.clone()).unwrap();
        let (mut request, _) = f.run("true");
        request.workspace_key = key.clone();
        request.repository_root = repository.clone();
        request.cwd = key.clone();
        request.siblings.clear();
        let (bundle, bytes) = f.bundle(&f.main, &repository, 0);
        request.bundles = vec![bundle];
        f.submit(&runner, &request, &[bytes]);
        assert!(matches!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        ));
        until(|| {
            runner
                .core
                .inner
                .lock()
                .unwrap()
                .jobs
                .values()
                .any(|job| job.base && job.terminal.is_some())
        })
        .await;
        base_done(&runner).await;
        assert!(runner.core.inner.lock().unwrap().jobs.values().any(|job| {
            job.base
                && job
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| matches!(terminal.outcome, Outcome::Exit { code: 0 }))
        }));
        assert!(host.join("source").is_file());
        assert!(runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .contains_key(&key));
        hold.arm();
        match operation {
            "drop" => {
                assert!(runner.drop_workspace(&key).await.unwrap().dropped);
            }
            "expire" => {
                f.clock.fetch_add(RETENTION_MS + 1, Ordering::SeqCst);
                runner.maintenance().unwrap();
            }
            "pressure" => {
                pressure.store(true, Ordering::SeqCst);
                let _ = runner.ensure_disk_space().unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            poll(Duration::from_secs(10), || hold.held()),
            "{operation} did not detach its workspace"
        );
        assert!(!host.exists());
        assert!(fs::read_dir(trash(&config)).unwrap().any(|entry| entry
            .unwrap()
            .path()
            .join("source")
            .exists()));
        assert!(!fs::read_dir(host.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".motor-trash-")));
        hold.release();
        runner.wait_for_background_deletes(Duration::from_secs(30));
        assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
        fs::write(trash(&config).join("leftover"), "old").unwrap();
        drop(runner);
        let runner = reopen(config.clone());
        assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
        drop(runner);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_caller_root_flag_warms_drops_expires_and_pressure_evicts() {
    only_caller_root_lifecycle(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_only_caller_root_flag_warms_drops_expires_and_pressure_evicts() {
    only_caller_root_lifecycle(true).await;
}

#[test]
fn mapped_detach_reclaims_only_the_pinned_checkout_root_before_rename() {
    use std::{
        cell::RefCell,
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            process::ExitStatusExt,
        },
        rc::Rc,
    };
    for mode in [0o555, 0o000] {
        for refuse_repair in [false, true] {
            let f = Fixture::new();
            let mut config = configured(&f);
            let runner = Runner::open(config.clone()).unwrap();
            let key = "/home/motor-trash-fixture/permissions";
            let host = plant(&runner, key);
            let inode = fs::metadata(&host).unwrap().ino();
            fs::set_permissions(&host, fs::Permissions::from_mode(mode)).unwrap();
            config.isolate_jobs = true;
            let observed = Rc::new(RefCell::new(Vec::new()));
            let recorded = observed.clone();
            let checkout = host.clone();
            let result = crate::systemd::with_process_hook(
                Box::new(move |command| {
                    let words: Vec<_> = std::iter::once(command.get_program())
                        .chain(command.get_args())
                        .map(|word| word.to_string_lossy().into_owned())
                        .collect();
                    let operation = words
                        .iter()
                        .find(|word| matches!(word.as_str(), "/usr/bin/chown" | "/usr/bin/chmod"))
                        .unwrap()
                        .clone();
                    assert!(!words.iter().any(|word| word == "-R"));
                    let alias = words.last().unwrap();
                    assert!(alias.starts_with(&format!("/proc/{}/fd/", std::process::id())));
                    let pin = alias.strip_suffix("/.").unwrap();
                    assert_eq!(fs::metadata(pin)?.ino(), inode);
                    recorded
                        .borrow_mut()
                        .push((operation.clone(), alias.clone()));
                    if refuse_repair {
                        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                    }
                    if operation == "/usr/bin/chmod" {
                        assert!(words.iter().any(|word| word == "0755"));
                        fs::set_permissions(&checkout, fs::Permissions::from_mode(0o755))?;
                    }
                    Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: vec![],
                        stderr: vec![],
                    })
                }),
                || paths::trash::detach(&config, &config.caller_roots[0], paths::host(&host)),
            );
            let operations = observed.borrow();
            if refuse_repair {
                assert!(result.is_err());
                assert_eq!(operations.len(), 1);
                assert!(host.exists());
                assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
                fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
            } else {
                assert_eq!(operations.len(), 2);
                assert_eq!(operations[0].0, "/usr/bin/chown");
                assert_eq!(operations[1].0, "/usr/bin/chmod");
                assert_eq!(operations[0].1, operations[1].1);
                let retired = result.unwrap().unwrap();
                assert!(!host.exists());
                assert_eq!(
                    fs::read_to_string(retired.path().join("secret")).unwrap(),
                    key
                );
                retired.remove(false, false).unwrap();
            }
            assert!(!config.state_dir().join("trash").exists());
        }
    }
}

#[test]
fn mapped_detach_refuses_source_replacement_during_root_repair() {
    use std::{
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            process::ExitStatusExt,
        },
        sync::atomic::AtomicUsize,
    };
    let f = Fixture::new();
    let mut config = configured(&f);
    let runner = Runner::open(config.clone()).unwrap();
    let key = "/home/motor-trash-fixture/permissions";
    let host = plant(&runner, key);
    let inode = fs::metadata(&host).unwrap().ino();
    fs::set_permissions(&host, fs::Permissions::from_mode(0o555)).unwrap();
    let outside = f.scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), "sentinel").unwrap();
    let outside_mode = fs::metadata(&outside).unwrap().mode();
    let held = config.caller_roots[0].host_root.join("held-checkout");
    let original = host.clone();
    let replacement = outside.clone();
    let moved = held.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    config.isolate_jobs = true;
    let result = crate::systemd::with_process_hook(
        Box::new(move |command| {
            let alias = command.get_args().last().unwrap().to_str().unwrap();
            let pin = alias.strip_suffix("/.").unwrap();
            assert_eq!(fs::metadata(pin)?.ino(), inode);
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                fs::set_permissions(&original, fs::Permissions::from_mode(0o755))?;
                fs::rename(&original, &moved)?;
                std::os::unix::fs::symlink(&replacement, &original)?;
            } else {
                fs::set_permissions(&moved, fs::Permissions::from_mode(0o755))?;
            }
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: vec![],
                stderr: vec![],
            })
        }),
        || paths::trash::detach(&config, &config.caller_roots[0], paths::host(&host)),
    );
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"sentinel");
    assert_eq!(fs::metadata(&outside).unwrap().mode(), outside_mode);
    assert_eq!(fs::read_to_string(held.join("secret")).unwrap(), key);
    assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
    fs::remove_file(&host).unwrap();
    fs::rename(held, host).unwrap();
}

#[test]
fn mapped_trash_repair_uses_physical_host_paths_for_every_inventory() {
    use std::{cell::RefCell, os::unix::process::ExitStatusExt, rc::Rc};
    for inventory in ["detach", "startup", "pressure"] {
        let f = Fixture::new();
        let config = configured(&f);
        let runner = Runner::open(config.clone()).unwrap();
        let key = "/home/motor-trash-fixture/repair";
        let host = plant(&runner, key);
        let detached = paths::trash::detach(&config, &config.caller_roots[0], paths::host(&host))
            .unwrap()
            .unwrap();
        let physical = fs::read_dir(trash(&config))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let retired = match inventory {
            "detach" => detached,
            "startup" => {
                drop(detached);
                paths::trash::entries(&config).unwrap().pop().unwrap()
            }
            "pressure" => {
                drop(detached);
                paths::trash::garbage(&config, paths::host(&physical)).unwrap()
            }
            _ => unreachable!(),
        };
        let commands = Rc::new(RefCell::new(Vec::new()));
        let observed = commands.clone();
        let expected = physical.clone();
        crate::systemd::with_process_hook(
            Box::new(move |command| {
                let args: Vec<_> = command.get_args().map(|arg| arg.to_os_string()).collect();
                assert_eq!(Path::new(args.last().unwrap()), expected);
                assert!(!args.last().unwrap().to_string_lossy().starts_with("/proc/"));
                observed.borrow_mut().push(args);
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: vec![],
                    stderr: vec![],
                })
            }),
            || retired.remove(true, false).unwrap(),
        );
        assert!(commands.borrow().len() >= 2);
        if unsafe { libc::geteuid() } != 0 {
            assert!(commands
                .borrow()
                .iter()
                .any(|args| args.iter().any(|arg| arg == "physical-chmod")));
        }
        assert!(!physical.exists(), "{inventory}");
        assert!(!retired.path().exists(), "{inventory}");
        assert!(fs::read_dir(trash(&config)).unwrap().next().is_none());
    }
}

#[test]
fn mapped_trash_repair_refuses_changed_physical_trash_before_privileged_commands() {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    for symlink in [false, true] {
        let f = Fixture::new();
        let config = configured(&f);
        let runner = Runner::open(config.clone()).unwrap();
        let key = "/home/motor-trash-fixture/repair";
        let host = plant(&runner, key);
        let retired = paths::trash::detach(&config, &config.caller_roots[0], paths::host(&host))
            .unwrap()
            .unwrap();
        let name = retired.path().file_name().unwrap().to_owned();
        let outside = f.scratch.join("outside");
        fs::create_dir_all(outside.join(&name)).unwrap();
        fs::write(outside.join(&name).join("secret"), "sentinel").unwrap();
        let before = fs::metadata(outside.join(&name)).unwrap();
        let held = config.caller_roots[0].host_root.join("held-trash");
        fs::rename(trash(&config), &held).unwrap();
        if symlink {
            std::os::unix::fs::symlink(&outside, trash(&config)).unwrap();
        } else {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(trash(&config))
                .unwrap();
        }
        crate::systemd::with_process_hook(
            Box::new(|command| {
                panic!("changed trash must refuse before privilege repair: {command:?}")
            }),
            || assert!(retired.remove(true, false).is_err()),
        );
        assert_eq!(
            fs::read(outside.join(&name).join("secret")).unwrap(),
            b"sentinel"
        );
        let after = fs::metadata(outside.join(&name)).unwrap();
        assert_eq!(
            (after.uid(), after.gid(), after.mode()),
            (before.uid(), before.gid(), before.mode())
        );
        assert_eq!(
            fs::read_to_string(retired.path().join("secret")).unwrap(),
            key
        );
        retired.remove(false, false).unwrap();
        assert!(fs::read_dir(&held).unwrap().next().is_none());
        if symlink {
            fs::remove_file(trash(&config)).unwrap();
        } else {
            fs::remove_dir(trash(&config)).unwrap();
        }
        fs::rename(held, trash(&config)).unwrap();
    }
}
