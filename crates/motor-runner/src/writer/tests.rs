use super::*;
use cortexkit_test_support::ScratchDir;
use std::os::unix::fs::symlink;

#[test]
fn canonical_future_resolves_existing_ancestors_and_refuses_dangling_links() {
    let scratch = ScratchDir::new("writer-paths");
    let real = scratch.join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, scratch.join("alias")).unwrap();
    assert_eq!(
        canonical_future(&scratch.join("alias/absent/child")).unwrap(),
        fs::canonicalize(&real).unwrap().join("absent/child")
    );
    symlink(scratch.join("absent"), scratch.join("dangling")).unwrap();
    let dangling = scratch.join("dangling/child");
    for (error, step) in [
        (
            canonical_future(&dangling).unwrap_err(),
            "resolve writer path",
        ),
        (
            workspace::destination(&dangling).unwrap_err(),
            "resolve destination",
        ),
    ] {
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let detail = error.to_string();
        assert!(detail.contains(step), "{detail}");
        assert!(detail.contains(dangling.to_str().unwrap()), "{detail}");
        assert!(detail.contains("No such file or directory"), "{detail}");
    }
    assert!(canonical_future(Path::new("relative")).is_err());
    assert!(canonical_future(&real.join("../escape")).is_err());
}

#[test]
fn validation_survives_a_sibling_checkout_retiring_between_metadata_and_listing() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    };
    use std::time::Duration;

    let scratch = ScratchDir::new("writer-retiring-sibling");
    let workspace = scratch.join("absent-workspace");
    let sibling = scratch.join("sibling");
    let retired = scratch.join("retired");
    fs::create_dir_all(sibling.join(".motor-base-main-fixture")).unwrap();
    let siblings = [SiblingSnapshot {
        canonical_path: sibling.to_str().unwrap().to_owned(),
        commit: String::new(),
        tree_hash: String::new(),
        base_commit: None,
    }];
    let (observed, metadata_read) = mpsc::sync_channel(0);
    let (resume, retired_path) = mpsc::sync_channel(0);
    let retired_path = Mutex::new(retired_path);
    let first = AtomicBool::new(true);
    let mut config = Config::new(scratch.join("state"), "main");
    config.long_work = Some(Arc::new(move |label| {
        if label == "writer_base_checkout_metadata" && first.swap(false, Ordering::SeqCst) {
            observed.send(()).unwrap();
            retired_path
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }));

    std::thread::scope(|scope| {
        let validation = scope.spawn(|| {
            validate_paths(
                &config,
                crate::caller_path::host(&workspace),
                None,
                &siblings,
                &[],
            )
        });
        metadata_read.recv_timeout(Duration::from_secs(10)).unwrap();
        // Discard retires a checkout by renaming it before background deletion.
        // Order that rename after metadata and before the directory is opened.
        let renamed = fs::rename(&sibling, &retired);
        resume.send(()).unwrap();
        renamed.unwrap();
        validation.join().unwrap().unwrap();
    });

    let error =
        validate_paths(&config, crate::caller_path::host(&retired), None, &[], &[]).unwrap_err();
    assert!(error
        .to_string()
        .contains("caller path overlaps writer-owned storage"));
    assert!(error.to_string().contains(retired.to_str().unwrap()));
}

#[test]
fn writer_validation_errors_identify_paths_in_every_role() {
    let scratch = ScratchDir::new("writer-path-errors");
    let dangling = scratch.join("dangling");
    symlink(scratch.join("absent"), &dangling).unwrap();
    let workspace = scratch.join("workspace");
    let config = Config::new(scratch.join("state"), "main");
    let siblings = [SiblingSnapshot {
        canonical_path: dangling.to_str().unwrap().to_owned(),
        commit: String::new(),
        tree_hash: String::new(),
        base_commit: None,
    }];
    for error in [
        validate_paths(&config, crate::caller_path::host(&dangling), None, &[], &[]).unwrap_err(),
        validate_paths(
            &config,
            crate::caller_path::host(&workspace),
            Some(crate::caller_path::host(&dangling)),
            &[],
            &[],
        )
        .unwrap_err(),
        validate_paths(
            &config,
            crate::caller_path::host(&workspace),
            None,
            &siblings,
            &[],
        )
        .unwrap_err(),
        validate_paths(
            &config,
            crate::caller_path::host(&workspace),
            None,
            &[],
            std::slice::from_ref(&dangling),
        )
        .unwrap_err(),
    ] {
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let detail = error.to_string();
        assert!(detail.contains("resolve writer path"), "{detail}");
        assert!(detail.contains(dangling.to_str().unwrap()), "{detail}");
    }
    fs::create_dir_all(config.state_dir()).unwrap();
    let cache = config.state_dir().join("sccache");
    symlink(scratch.join("absent"), &cache).unwrap();
    let detail = validate_paths(
        &config,
        crate::caller_path::host(&workspace),
        None,
        &[],
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(detail.contains("resolve writer path"), "{detail}");
    assert!(detail.contains(cache.to_str().unwrap()), "{detail}");
}

#[test]
fn cache_roots_and_scratch_are_instance_specific() {
    let scratch = ScratchDir::new("writer-roots");
    let main = Config::new(scratch.path(), "main");
    let e2e = Config::new(scratch.path(), "e2e");
    assert_eq!(main.sccache_dir(), scratch.join("main/sccache/fleet"));
    assert_eq!(
        main.writer_scratch_dir(),
        scratch.join("main/writer-scratch")
    );
    assert_ne!(main.sccache_dir(), e2e.sccache_dir());
    assert_eq!(main.sccache_size_gib, 200);
}

#[test]
fn path_validation_does_not_descend_into_private_retained_targets() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = ScratchDir::new("writer-private-target");
    let workspace = scratch.join("workspace");
    let private = workspace.join("target/private");
    fs::create_dir_all(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
    let result = validate_paths(
        &Config::new(scratch.join("state"), "main"),
        crate::caller_path::host_str(workspace.to_str().unwrap()),
        None,
        &[],
        &[],
    );
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    validate_paths(
        &Config::new("unused-writer-state", "main"),
        crate::caller_path::host_str(workspace.to_str().unwrap()),
        None,
        &[],
        &[],
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn unavailable_writer_account_disables_fills_and_cache_tracking() {
    let scratch = ScratchDir::new("writer-disabled");
    let mut config = Config::new(scratch.path(), "main");
    config.isolate_jobs = true;
    assert!(!config.writer_enabled());
    assert_eq!(
        CacheFill::begin(&config).err().unwrap().to_string(),
        "dependency writer account is unavailable"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn writer_server_addresses_work_with_long_state_roots_and_separate_fills_and_instances() {
    use motor_protocol::Uuid;
    use std::io::{Read, Write};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

    let config = Config::new(format!("/{}", "long-state-root/".repeat(40)), "main");
    assert!(config.state_dir().as_os_str().len() > 107);
    let fill = Uuid::now_v7();
    let address = config.writer_sccache_server_uds(fill);
    assert_eq!(
        address,
        Config::new("/short", "main").writer_sccache_server_uds(fill)
    );
    let alternatives = [
        config.writer_sccache_server_uds(Uuid::now_v7()),
        Config::new(&config.state_root, "e2e").writer_sccache_server_uds(fill),
        Config::new(&config.state_root, "instance".repeat(100)).writer_sccache_server_uds(fill),
    ];
    let socket = |value: &str| {
        let name = value
            .strip_prefix(r"\x00")
            .expect("sccache's literal abstract-socket prefix");
        assert!(name.len() <= 107);
        SocketAddr::from_abstract_name(name.as_bytes()).unwrap()
    };
    let addr = socket(&address);
    let listener = UnixListener::bind_addr(&addr).unwrap();
    let _other_listeners: Vec<_> = alternatives
        .iter()
        .map(|other| {
            assert_ne!(other, &address);
            UnixListener::bind_addr(&socket(other)).unwrap()
        })
        .collect();
    let mut client = UnixStream::connect_addr(&addr).unwrap();
    let mut server = listener.accept().unwrap().0;
    client.write_all(b"working server").unwrap();
    let mut bytes = [0; 14];
    server.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"working server");
}
