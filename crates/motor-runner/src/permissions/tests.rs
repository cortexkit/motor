use super::*;
use cortexkit_test_support::ScratchDir;
use std::os::unix::{
    fs::{symlink, FileTypeExt, PermissionsExt},
    net::UnixListener,
};

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn physical_permission_repair_accepts_canonical_runner_state_for_all_profiles() {
    use crate::{Config, Runner};

    let scratch = ScratchDir::new("motor-symlink-state");
    let real = scratch.join("real");
    fs::create_dir(&real).unwrap();
    let alias = scratch.join("state");
    symlink(&real, &alias).unwrap();
    for profile in ["main", "boat", "tenant"] {
        let mut config = Config::new(&alias, profile);
        config.free_space_floor_bytes = 0;
        config.reserve_daemon_core = false;
        config.tenant_vm = profile != "main";
        if profile == "boat" {
            config.boat_profile = true;
            config.boat_memory_max_bytes = Some(14 * (1 << 30));
        }
        let runner = Runner::open(config).unwrap();
        let home = runner.config().state_dir().join("sandboxes/job/home");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        // The same walk is used by the installed helper and direct-root runners.
        if unsafe { libc::geteuid() } == 0 {
            repair_tree(Policy::Private, &[&home]).unwrap();
        } else {
            apply(Policy::Private, &[&home]).unwrap();
        }
        assert_eq!(mode(&home), 0o700);
        assert_eq!(runner.config().state_root, real.canonicalize().unwrap());

        let outside = scratch.join(format!("outside-{profile}"));
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o750)).unwrap();
        let planted = runner.config().state_dir().join("sandboxes/planted");
        symlink(&outside, &planted).unwrap();
        assert!(apply(Policy::Private, &[&planted]).is_err());
        assert!(apply(Policy::Private, &[&planted.join("child")]).is_err());
        assert_eq!(mode(&outside), 0o750);
    }
    let outside = scratch.join("outside-instance");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, real.join("planted-instance")).unwrap();
    assert!(Runner::open(Config::new(&alias, "planted-instance"))
        .err()
        .unwrap()
        .to_string()
        .contains("not a symlink"));
}

#[test]
fn physical_permission_walk_never_follows_planted_descendant_symlinks() {
    let scratch = ScratchDir::new("motor-physical-permissions");
    let tree = scratch.join("tree");
    let outside = scratch.join("outside");
    fs::create_dir_all(tree.join("nested")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let secret = outside.join("file");
    fs::write(&secret, b"outside").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o440)).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o750)).unwrap();
    symlink(&outside, tree.join("nested/directory-link")).unwrap();
    symlink(&secret, tree.join("file-link")).unwrap();
    symlink(scratch.join("missing"), tree.join("dangling")).unwrap();
    for policy in [Policy::OwnerWrite, Policy::Private] {
        apply(policy, &[&tree]).unwrap();
        assert_eq!(mode(&outside), 0o750);
        assert_eq!(mode(&secret), 0o440);
    }
}

#[test]
fn physical_permission_walk_rejects_top_level_and_ancestor_symlinks_before_changes() {
    let scratch = ScratchDir::new("motor-physical-permission-roots");
    let good = scratch.join("good");
    let outside = scratch.join("outside");
    fs::create_dir(&good).unwrap();
    fs::create_dir_all(outside.join("nested")).unwrap();
    fs::set_permissions(&good, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o750)).unwrap();
    let link = scratch.join("link");
    symlink(&outside, &link).unwrap();
    assert!(apply(Policy::Private, &[&good, &link]).is_err());
    assert_eq!(mode(&good), 0o755);
    assert_eq!(mode(&outside), 0o750);
    assert!(apply(Policy::Private, &[&link.join("nested")]).is_err());
    assert!(apply(Policy::Private, &[Path::new("/")]).is_err());
    assert!(apply(Policy::Private, &[Path::new("/.")]).is_err());
}

#[test]
fn physical_permission_walk_pins_inodes_across_a_symlink_replacement() {
    let scratch = ScratchDir::new("motor-physical-permission-race");
    let tree = scratch.join("tree");
    let outside = scratch.join("outside");
    fs::create_dir(&tree).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o750)).unwrap();
    let entry = tree.join("entry");
    fs::write(&entry, b"inside").unwrap();
    let secret = outside.join("file");
    fs::write(&secret, b"outside").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o440)).unwrap();
    let parent = physical_root(&tree).unwrap();
    let pinned = open_at(parent.as_raw_fd(), std::ffi::OsStr::new("entry"), false).unwrap();
    fs::rename(&entry, tree.join("original")).unwrap();
    symlink(&secret, &entry).unwrap();
    repair_node(pinned, Policy::Private).unwrap();
    assert_eq!(mode(&secret), 0o440);
    assert_eq!(mode(&tree.join("original")), 0o600);
    let pinned_tree = physical_root(&tree).unwrap();
    fs::rename(&tree, scratch.join("moved")).unwrap();
    symlink(&outside, &tree).unwrap();
    repair(pinned_tree, Policy::Private).unwrap();
    assert_eq!(mode(&secret), 0o440);
    assert_eq!(mode(&outside), 0o750);
}

#[test]
fn physical_permission_walk_repairs_a_regular_file_root_and_refuses_a_symlink_root() {
    let scratch = ScratchDir::new("motor-physical-permission-file-root");
    let output = scratch.join("output");
    fs::create_dir(&output).unwrap();
    let stdout = output.join("stdout");
    let exit = output.join("exit");
    fs::write(&stdout, b"job output").unwrap();
    fs::write(&exit, b"0").unwrap();
    fs::set_permissions(&stdout, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&exit, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(&output, fs::Permissions::from_mode(0o755)).unwrap();
    apply(Policy::OwnerWrite, &[&stdout]).unwrap();
    assert_eq!(mode(&stdout), 0o644);
    apply(Policy::Private, &[&stdout, &exit]).unwrap();
    assert_eq!(mode(&stdout), 0o600);
    assert_eq!(mode(&exit), 0o700);
    // Only the named file's inode changes, never its parent directory.
    assert_eq!(mode(&output), 0o755);

    let outside = scratch.join("outside");
    fs::write(&outside, b"outside").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o440)).unwrap();
    let link = output.join("stderr");
    symlink(&outside, &link).unwrap();
    let dangling = output.join("dangling");
    symlink(scratch.join("missing"), &dangling).unwrap();
    let fifo = output.join("fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o666) }, 0);
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o666)).unwrap();
    fs::set_permissions(&stdout, fs::Permissions::from_mode(0o444)).unwrap();
    for refused in [&link, &dangling, &fifo] {
        // Roots are validated before any mode changes, so the good file is untouched.
        assert!(apply(Policy::Private, &[&stdout, refused]).is_err());
        assert_eq!(mode(&stdout), 0o444);
    }
    assert_eq!(mode(&outside), 0o440);
    assert_eq!(mode(&fifo), 0o666);
    // A regular file is not a directory ancestor.
    assert!(apply(Policy::Private, &[&stdout.join("child")]).is_err());
}

#[test]
fn physical_permission_modes_preserve_conditional_execute_and_support_special_inodes() {
    let scratch = ScratchDir::new("motor-physical-permission-modes");
    let tree = scratch.join("tree");
    fs::create_dir(&tree).unwrap();
    let plain = tree.join("plain");
    let executable = tree.join("executable");
    fs::write(&plain, b"plain").unwrap();
    fs::write(&executable, b"executable").unwrap();
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o451)).unwrap();
    let fifo = tree.join("fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o666) }, 0);
    let socket = tree.join("socket");
    // Gate scratch paths can exceed the 108-byte sun_path limit, so bind through
    // a short /proc/self/fd path naming the open tree directory instead.
    let directory = fs::File::open(&tree).unwrap();
    let short = format!("/proc/self/fd/{}/socket", directory.as_raw_fd());
    let _listener = UnixListener::bind(&short).unwrap();
    assert!(fs::symlink_metadata(&socket)
        .unwrap()
        .file_type()
        .is_socket());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(mode(&socket), 0o666);
    apply(Policy::OwnerWrite, &[&tree]).unwrap();
    assert_eq!(mode(&plain), 0o644);
    assert_eq!(mode(&executable), 0o751);
    apply(Policy::Private, &[&tree]).unwrap();
    assert_eq!(mode(&tree), 0o700);
    assert_eq!(mode(&plain), 0o600);
    assert_eq!(mode(&executable), 0o700);
    assert_eq!(mode(&fifo), 0o600);
    assert_eq!(mode(&socket), 0o600);
}

const REPOSITORY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn staging_fixture(scratch: &Path, fill: motor_protocol::Uuid) -> std::path::PathBuf {
    let generation = scratch
        .join("build-bases")
        .join(REPOSITORY)
        .join(format!("staging-{fill}"));
    fs::create_dir_all(generation.join("target/nested/deeper")).unwrap();
    generation
}

#[test]
fn staging_link_check_accepts_nested_directories_and_internal_hard_links() {
    let scratch = ScratchDir::new("motor-staging-internal-links");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let file = generation.join("target/artifact");
    fs::write(&file, b"artifact").unwrap();
    fs::hard_link(&file, generation.join("target/nested/uplift")).unwrap();
    fs::hard_link(&file, generation.join("target/nested/deeper/uplift")).unwrap();
    symlink("nested", generation.join("target/directory-link")).unwrap();
    symlink("missing", generation.join("target/dangling")).unwrap();
    staging_apply("check-staging-links", &scratch, REPOSITORY, fill).unwrap();
}

#[test]
fn staging_link_check_refuses_external_links_for_every_non_directory_type() {
    let scratch = ScratchDir::new("motor-staging-external-links");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let file = generation.join("target/artifact");
    fs::write(&file, b"artifact").unwrap();
    let symlink_path = generation.join("target/link");
    symlink("missing", &symlink_path).unwrap();
    let fifo = generation.join("target/fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    for entry in [&file, &symlink_path, &fifo] {
        let outside = scratch.join("outside-link");
        fs::hard_link(entry, &outside).unwrap();
        let error = staging_apply("check-staging-links", &scratch, REPOSITORY, fill).unwrap_err();
        assert!(error.to_string().contains("linked from outside"), "{error}");
        fs::remove_file(&outside).unwrap();
    }
    staging_apply("check-staging-links", &scratch, REPOSITORY, fill).unwrap();
}

#[test]
fn staging_removal_preserves_external_inode_owner_mode_and_acl() {
    let scratch = ScratchDir::new("motor-staging-removal");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let outside = scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    let file = outside.join("artifact");
    fs::write(&file, b"outside").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    let output = Command::new("/usr/bin/setfacl")
        .args(["-m", "u:12345:r--", "--"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let acl = || {
        let output = Command::new("/usr/bin/getfacl")
            .args(["-cpn", "--"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let before = fs::symlink_metadata(&file).unwrap();
    let before_acl = acl();
    assert!(String::from_utf8_lossy(&before_acl).contains("user:12345:r--"));
    fs::hard_link(&file, generation.join("target/nested/artifact")).unwrap();
    symlink(&outside, generation.join("target/directory-link")).unwrap();
    symlink(&file, generation.join("target/file-link")).unwrap();
    symlink(scratch.join("missing"), generation.join("target/dangling")).unwrap();
    staging_apply("remove-staging", &scratch, REPOSITORY, fill).unwrap();
    assert!(!generation.exists());
    let after = fs::symlink_metadata(&file).unwrap();
    assert_eq!(
        (after.uid(), after.gid(), after.mode()),
        (before.uid(), before.gid(), before.mode())
    );
    assert_eq!(acl(), before_acl);
    assert_eq!(fs::read(&file).unwrap(), b"outside");
}

#[test]
fn staging_operations_reject_nonphysical_roots_and_invalid_forms() {
    let scratch = ScratchDir::new("motor-staging-invalid");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let outside = scratch.join("outside");
    fs::rename(&generation, &outside).unwrap();
    symlink(&outside, &generation).unwrap();
    for action in ["check-staging-links", "remove-staging"] {
        assert!(staging_apply(action, &scratch, REPOSITORY, fill).is_err());
        assert!(outside.join("target/nested/deeper").is_dir());
        for state in [
            Path::new("/"),
            Path::new("relative"),
            Path::new("/state/../other"),
            Path::new("/state/./other"),
        ] {
            assert!(staging_command(action, state, REPOSITORY, fill).is_err());
        }
        for repository in ["..", ".", "other", "../escape", &"A".repeat(64)] {
            assert!(staging_command(action, &scratch, repository, fill).is_err());
        }
        assert!(staging_helper(action, &[]).is_err());
        assert!(staging_helper(
            action,
            &[
                REPOSITORY.into(),
                fill.to_string(),
                scratch.to_string_lossy().into(),
                "extra".into()
            ]
        )
        .is_err());
    }
    assert!(staging_command("remove-current", &scratch, REPOSITORY, fill).is_err());
    fs::remove_file(&generation).unwrap();
    fs::rename(&outside, &generation).unwrap();
    let bases = scratch.join("build-bases");
    fs::rename(&bases, scratch.join("moved-bases")).unwrap();
    symlink(scratch.join("moved-bases"), &bases).unwrap();
    for action in ["check-staging-links", "remove-staging"] {
        assert!(staging_apply(action, &scratch, REPOSITORY, fill).is_err());
    }
    assert!(scratch
        .join("moved-bases")
        .join(REPOSITORY)
        .join(format!("staging-{fill}/target"))
        .exists());
}

fn helper_output(success: bool, stdout: Vec<u8>) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt;
    std::process::Output {
        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
        stdout,
        stderr: Vec::new(),
    }
}

#[test]
fn staging_runner_uses_fixed_sudo_forms_or_walks_in_process_as_root() {
    use std::{cell::Cell, rc::Rc};
    let scratch = ScratchDir::new("motor-staging-runner");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let root = unsafe { libc::geteuid() } == 0;
    for action in ["check-staging-links", "remove-staging"] {
        let command = staging_command(action, &scratch, REPOSITORY, fill).unwrap();
        assert_eq!(command.get_program(), "/usr/bin/sudo");
        let expected = vec![
            "-n".into(),
            HELPER.into(),
            action.into(),
            REPOSITORY.into(),
            fill.to_string(),
            scratch.to_string_lossy().into_owned(),
        ];
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            expected
        );
        let called = Rc::new(Cell::new(false));
        let observed = called.clone();
        crate::systemd::with_process_hook(
            Box::new(move |command| {
                assert!(!root, "root runner must not invoke the installed helper");
                assert_eq!(command.get_program(), "/usr/bin/sudo");
                assert_eq!(
                    command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                    expected
                );
                observed.set(true);
                Ok(helper_output(true, Vec::new()))
            }),
            || staging_operation(action, &scratch, REPOSITORY, fill).unwrap(),
        );
        assert_eq!(called.get(), !root);
    }
    assert_eq!(generation.exists(), !root);
}

#[test]
fn install_contract_requires_matching_helper_and_both_staging_grants() {
    use std::{cell::RefCell, rc::Rc};
    for missing in [
        None,
        Some("check-staging-links"),
        Some("remove-staging"),
        Some("digest"),
    ] {
        let actions = Rc::new(RefCell::new(Vec::new()));
        let observed = actions.clone();
        let result = crate::systemd::with_process_hook(
            Box::new(move |command| {
                if command.get_program() == HELPER {
                    return Ok(helper_output(
                        true,
                        if missing == Some("digest") {
                            b"old-helper".to_vec()
                        } else {
                            digest().into_bytes()
                        },
                    ));
                }
                assert_eq!(command.get_program(), "/usr/bin/sudo");
                let args = command
                    .get_args()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                assert_eq!(&args[..3], &["-n", "-l", HELPER]);
                let action = args[3].clone();
                observed.borrow_mut().push(action.clone());
                Ok(helper_output(missing != Some(action.as_str()), Vec::new()))
            }),
            check_install_commands,
        );
        assert_eq!(
            result.is_ok(),
            missing.is_none(),
            "missing {missing:?}: {result:?}"
        );
        if missing.is_none() {
            assert_eq!(
                &*actions.borrow(),
                &[
                    "physical-chmod",
                    "physical-chmod",
                    "seal",
                    "unseal",
                    "check-staging-links",
                    "remove-staging"
                ]
            );
        } else if missing == Some("digest") {
            assert!(actions.borrow().is_empty());
        }
    }
}
