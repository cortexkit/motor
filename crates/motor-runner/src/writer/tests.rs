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
    assert!(canonical_future(&scratch.join("dangling/child")).is_err());
    assert!(canonical_future(Path::new("relative")).is_err());
    assert!(canonical_future(&real.join("../escape")).is_err());
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
        workspace.to_str().unwrap(),
        None,
        &[],
        &[],
    );
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    validate_paths(
        &Config::new("unused-writer-state", "main"),
        workspace.to_str().unwrap(),
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
