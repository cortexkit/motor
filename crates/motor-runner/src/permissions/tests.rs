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

fn fixture_owners() -> StagingOwners {
    let uid = unsafe { libc::geteuid() };
    StagingOwners {
        runner: uid,
        writer: uid,
    }
}

fn fixture_staging_apply(
    action: &str,
    state: &Path,
    repository: &str,
    fill: motor_protocol::Uuid,
) -> io::Result<()> {
    staging_apply(action, state, repository, fill, fixture_owners())
}

fn set_fixture_owner(path: &Path, uid: u32) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::lchown(path.as_ptr(), uid, u32::MAX) }, 0);
}

fn staging_fixture(scratch: &Path, fill: motor_protocol::Uuid) -> std::path::PathBuf {
    let generation = scratch
        .join("build-bases")
        .join(REPOSITORY)
        .join(format!("staging-{fill}"));
    fs::create_dir_all(generation.join("target/nested/deeper")).unwrap();
    for path in [
        &generation,
        &generation.join("target"),
        &generation.join("target/nested"),
        &generation.join("target/nested/deeper"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
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
    fixture_staging_apply("check-staging-links", &scratch, REPOSITORY, fill).unwrap();
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
        let error =
            fixture_staging_apply("check-staging-links", &scratch, REPOSITORY, fill).unwrap_err();
        assert!(error.to_string().contains("linked from outside"), "{error}");
        fs::remove_file(&outside).unwrap();
    }
    fixture_staging_apply("check-staging-links", &scratch, REPOSITORY, fill).unwrap();
}

#[test]
fn staging_removal_accepts_internal_hard_links() {
    let scratch = ScratchDir::new("staging-removal-internal-links");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let file = generation.join("target/artifact");
    fs::write(&file, b"artifact").unwrap();
    fs::hard_link(&file, generation.join("target/nested/uplift")).unwrap();
    fs::hard_link(&file, generation.join("target/nested/deeper/uplift")).unwrap();
    fixture_staging_apply("remove-staging", &scratch, REPOSITORY, fill).unwrap();
    assert!(!generation.exists());
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
    fixture_staging_apply("remove-staging", &scratch, REPOSITORY, fill).unwrap();
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
        assert!(fixture_staging_apply(action, &scratch, REPOSITORY, fill).is_err());
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
        assert!(fixture_staging_apply(action, &scratch, REPOSITORY, fill).is_err());
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
            || staging_execute(action, &scratch, REPOSITORY, fill, fixture_owners()).unwrap(),
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

#[test]
fn staging_security_refuses_wrong_owner_without_removing_any_entry() {
    let scratch = ScratchDir::new("staging-owner-refusal");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    fs::write(generation.join("target/keep"), b"keep").unwrap();
    let uid = unsafe { libc::geteuid() };
    let owners = StagingOwners {
        runner: uid + 2,
        writer: uid + 1,
    };
    for action in ["check-staging-links", "remove-staging"] {
        // Exercise the inode preflight directly: the store-directory owner
        // checks are covered separately and would reject this fixture first.
        let root = physical_root(&generation).unwrap();
        let error = if action == "check-staging-links" {
            inspect_staging(root, owners, true)
        } else {
            remove_staging(
                root,
                physical_root(generation.parent().unwrap()).unwrap(),
                generation.file_name().unwrap(),
                owners,
            )
        }
        .unwrap_err();
        assert!(error.to_string().contains("fill writer"), "{error}");
        assert_eq!(fs::read(generation.join("target/keep")).unwrap(), b"keep");
    }
}

#[test]
fn staging_security_preflights_all_directory_modes_before_removal() {
    let scratch = ScratchDir::new("staging-mode-refusal");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let file = generation.join("target/keep");
    fs::write(&file, b"keep").unwrap();
    for path in [&generation, &generation.join("target/nested/deeper")] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o770)).unwrap();
        for action in ["check-staging-links", "remove-staging"] {
            let error = fixture_staging_apply(action, &scratch, REPOSITORY, fill).unwrap_err();
            assert!(
                error.to_string().contains("group/other-writable"),
                "{error}"
            );
            assert_eq!(fs::read(&file).unwrap(), b"keep");
            assert_eq!(mode(path), 0o770);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o750)).unwrap();
    }
}

#[test]
fn staging_security_rejects_untrusted_store_owners_and_directory_modes() {
    let scratch = ScratchDir::new("staging-store-refusal");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let file = generation.join("target/keep");
    fs::write(&file, b"keep").unwrap();
    for path in [
        scratch.join("build-bases"),
        scratch.join("build-bases").join(REPOSITORY),
    ] {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        for action in ["check-staging-links", "remove-staging"] {
            let error = fixture_staging_apply(action, &scratch, REPOSITORY, fill).unwrap_err();
            assert!(error.to_string().contains("store directories"), "{error}");
            assert_eq!(fs::read(&file).unwrap(), b"keep");
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let owners = StagingOwners {
            runner: metadata.uid() + 1,
            writer: metadata.uid(),
        };
        if metadata.uid() != 0 {
            assert!(owners.store_directory(&metadata).is_err());
        }
    }
}

#[test]
fn staging_security_refuses_duplicate_directory_visits() {
    let scratch = ScratchDir::new("staging-duplicate-visits");
    fs::write(scratch.join("file"), b"one name").unwrap();
    let root = physical_root(&scratch).unwrap();
    let mut walk = StagingWalk::new(&root, fixture_owners()).unwrap();
    walk.visit(&root, 0).unwrap();
    assert!(walk
        .visit(&root, 1)
        .unwrap_err()
        .to_string()
        .contains("visited more than once"));
}

#[test]
fn staging_security_refuses_excess_inode_visits() {
    let scratch = ScratchDir::new("staging-excess-visits");
    fs::write(scratch.join("file"), b"one name").unwrap();
    let root = physical_root(&scratch).unwrap();
    let mut walk = StagingWalk::new(&root, fixture_owners()).unwrap();
    let file = open_at(root.as_raw_fd(), std::ffi::OsStr::new("file"), false).unwrap();
    walk.visit(&file, 1).unwrap();
    assert!(walk
        .visit(&file, 1)
        .unwrap_err()
        .to_string()
        .contains("visit count"));
}

#[test]
fn staging_security_refuses_a_different_device_or_mount_before_entering() {
    let scratch = ScratchDir::new("staging-device-refusal");
    let root = physical_root(&scratch).unwrap();
    let other = physical_root(Path::new("/proc")).unwrap();
    let owners = StagingOwners {
        runner: unsafe { libc::geteuid() },
        writer: fs::metadata("/proc").unwrap().uid(),
    };
    let mut walk = StagingWalk::new(&root, owners).unwrap();
    let error = walk.visit(&other, 1).unwrap_err();
    assert!(
        error.to_string().contains("device or mount crossings"),
        "{error}"
    );
}

#[test]
fn staging_security_bounds_depth_before_any_removal() {
    let scratch = ScratchDir::new("staging-depth-refusal");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let file = generation.join("keep");
    fs::write(&file, b"keep").unwrap();
    let mut deep = generation.clone();
    for _ in 0..=MAX_STAGING_DEPTH {
        deep.push("d");
        fs::create_dir(&deep).unwrap();
    }
    for action in ["check-staging-links", "remove-staging"] {
        let error = fixture_staging_apply(action, &scratch, REPOSITORY, fill).unwrap_err();
        assert!(error.to_string().contains("maximum depth"), "{error}");
        assert_eq!(fs::read(&file).unwrap(), b"keep");
        assert!(deep.is_dir());
    }
}

#[test]
fn helper_digest_covers_dispatch_and_account_validation_sources() {
    let mut expected = sha2::Sha256::new();
    for source in [
        include_bytes!("../permissions.rs").as_slice(),
        include_bytes!("../source_seal.rs").as_slice(),
        include_bytes!("../source_seal/authority.rs").as_slice(),
        include_bytes!("../main.rs").as_slice(),
        include_bytes!("../lib.rs").as_slice(),
        include_bytes!("../isolation.rs").as_slice(),
    ] {
        expected.update(source);
    }
    assert_eq!(digest(), format!("{:x}", expected.finalize()));
}

#[test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts"]
fn isolated_staging_security_checks_every_owner_before_removing_anything() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let scratch = ScratchDir::new("staging-foreign-owner");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    // Model a delegated non-root runner even though this fixture needs root
    // to plant each foreign owner. Root is then foreign to both identities.
    let owners = StagingOwners {
        runner: 65533,
        ..StagingOwners::current().unwrap()
    };
    let job = crate::isolation::job_identity().unwrap().uid;
    let keep = generation.join("target/keep");
    let foreign = generation.join("target/nested/deeper/foreign");
    let link = generation.join("target/nested/deeper/link");
    fs::write(&keep, b"keep").unwrap();
    fs::write(&foreign, b"foreign").unwrap();
    symlink("missing", &link).unwrap();
    fn owner(path: &Path, uid: u32) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::lchown(path.as_ptr(), uid, u32::MAX) }, 0);
    }
    for path in [
        &generation,
        &generation.join("target"),
        &generation.join("target/nested"),
        &generation.join("target/nested/deeper"),
        &keep,
        &foreign,
        &link,
    ] {
        owner(path, owners.writer);
    }
    for path in [
        &generation,
        &generation.join("target/nested/deeper"),
        &foreign,
        &link,
    ] {
        for uid in [0, job] {
            owner(path, uid);
            for action in ["check-staging-links", "remove-staging"] {
                let error = staging_apply(action, &scratch, REPOSITORY, fill, owners).unwrap_err();
                assert!(error.to_string().contains("fill writer"), "{error}");
                assert_eq!(fs::read(&keep).unwrap(), b"keep");
                assert_eq!(fs::read(&foreign).unwrap(), b"foreign");
                assert_eq!(fs::symlink_metadata(path).unwrap().uid(), uid);
            }
            owner(path, owners.writer);
        }
    }
    staging_execute("check-staging-links", &scratch, REPOSITORY, fill, owners).unwrap();
    staging_execute("remove-staging", &scratch, REPOSITORY, fill, owners).unwrap();
    assert!(!generation.exists());
}

#[test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts"]
fn isolated_staging_removal_accepts_partial_runner_chown_but_link_check_does_not() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let scratch = ScratchDir::new("staging-partial-chown");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let owners = StagingOwners {
        runner: 65533,
        ..StagingOwners::current().unwrap()
    };
    let artifact = generation.join("target/nested/deeper/artifact");
    fs::write(&artifact, b"artifact").unwrap();
    for path in [
        &generation,
        &generation.join("target"),
        &generation.join("target/nested"),
        &generation.join("target/nested/deeper"),
        &artifact,
    ] {
        set_fixture_owner(path, owners.writer);
    }
    set_fixture_owner(&generation, owners.runner);
    set_fixture_owner(&artifact, owners.runner);
    assert!(staging_apply("check-staging-links", &scratch, REPOSITORY, fill, owners).is_err());
    assert!(artifact.exists());
    staging_apply("remove-staging", &scratch, REPOSITORY, fill, owners).unwrap();
    assert!(!generation.exists());
}

#[test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts; checks in-process authority"]
fn isolated_staging_security_requires_authority_for_in_process_root() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let scratch = ScratchDir::new("staging-root-authority");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    fs::write(generation.join("keep"), b"keep").unwrap();
    let owners = StagingOwners::current().unwrap();
    for path in [
        &generation,
        &generation.join("target"),
        &generation.join("target/nested"),
        &generation.join("target/nested/deeper"),
        &generation.join("keep"),
    ] {
        set_fixture_owner(path, owners.writer);
    }
    for action in ["check-staging-links", "remove-staging"] {
        let error = staging_operation(action, &scratch, REPOSITORY, fill).unwrap_err();
        assert!(
            error.to_string().contains("source seal authority")
                || error.kind() == io::ErrorKind::NotFound,
            "{error}"
        );
        assert_eq!(fs::read(generation.join("keep")).unwrap(), b"keep");
    }
}

#[test]
#[ignore = "requires root, mount namespaces and provisioned motor-cache/motor-job accounts"]
fn isolated_staging_security_refuses_bind_and_device_mounts_before_removal() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    assert_eq!(
        unsafe { libc::unshare(libc::CLONE_NEWNS) },
        0,
        "{}",
        io::Error::last_os_error()
    );
    assert_eq!(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        },
        0
    );
    let scratch = ScratchDir::new("staging-mount-refusal");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let owners = StagingOwners::current().unwrap();
    let outside = scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    let mountpoint = generation.join("target/nested/deeper");
    let keep = generation.join("keep");
    fs::write(&keep, b"keep").unwrap();
    let set_owner = |path: &Path| {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::lchown(path.as_ptr(), owners.writer, u32::MAX) },
            0
        );
    };
    for path in [
        &generation,
        &generation.join("target"),
        &generation.join("target/nested"),
        &mountpoint,
        &keep,
        &outside,
    ] {
        set_owner(path);
    }
    struct Mounted(CString);
    impl Drop for Mounted {
        fn drop(&mut self) {
            assert_eq!(
                unsafe { libc::umount2(self.0.as_ptr(), libc::MNT_DETACH) },
                0
            );
        }
    }
    for bind in [true, false] {
        let target = CString::new(mountpoint.as_os_str().as_bytes()).unwrap();
        let source = CString::new(outside.as_os_str().as_bytes()).unwrap();
        let result = if bind {
            unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            }
        } else {
            unsafe {
                libc::mount(
                    c"tmpfs".as_ptr(),
                    target.as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    std::ptr::null(),
                )
            }
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        let mounted = Mounted(target);
        set_owner(&mountpoint);
        fs::set_permissions(&mountpoint, fs::Permissions::from_mode(0o755)).unwrap();
        let valuable = mountpoint.join("valuable");
        fs::write(&valuable, b"mounted data").unwrap();
        set_owner(&valuable);
        let root = physical_root(&generation).unwrap();
        let child = physical_root(&mountpoint).unwrap();
        assert_ne!(mount_id(&root).unwrap(), mount_id(&child).unwrap());
        assert_eq!(
            fs::metadata(&generation).unwrap().dev() == fs::metadata(&mountpoint).unwrap().dev(),
            bind
        );
        for action in ["check-staging-links", "remove-staging"] {
            let error = staging_apply(action, &scratch, REPOSITORY, fill, owners).unwrap_err();
            assert!(
                error.to_string().contains("device or mount crossings"),
                "{error}"
            );
            assert_eq!(fs::read(&valuable).unwrap(), b"mounted data");
            assert_eq!(fs::read(&keep).unwrap(), b"keep");
        }
        drop(mounted);
    }
}

#[test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts"]
fn isolated_staging_security_refuses_foreign_owned_store_directories() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let scratch = ScratchDir::new("staging-store-owner");
    let fill = motor_protocol::Uuid::nil();
    let generation = staging_fixture(&scratch, fill);
    let owners = StagingOwners::current().unwrap();
    let job = crate::isolation::job_identity().unwrap().uid;
    let keep = generation.join("keep");
    fs::write(&keep, b"keep").unwrap();
    for path in [
        &generation,
        &generation.join("target"),
        &generation.join("target/nested"),
        &generation.join("target/nested/deeper"),
        &keep,
    ] {
        set_fixture_owner(path, owners.writer);
    }
    for path in [
        scratch.join("build-bases"),
        scratch.join("build-bases").join(REPOSITORY),
    ] {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        for uid in [job, owners.writer] {
            assert_eq!(unsafe { libc::lchown(name.as_ptr(), uid, u32::MAX) }, 0);
            for action in ["check-staging-links", "remove-staging"] {
                let error = staging_apply(action, &scratch, REPOSITORY, fill, owners).unwrap_err();
                assert!(error.to_string().contains("store directories"), "{error}");
                assert_eq!(fs::read(&keep).unwrap(), b"keep");
            }
        }
        assert_eq!(unsafe { libc::lchown(name.as_ptr(), 0, u32::MAX) }, 0);
    }
}

#[test]
#[ignore = "requires the trusted non-root operator and installed matching helper/sudoers"]
fn operator_staging_security_rejects_malformed_fixed_helper_forms() {
    assert_ne!(unsafe { libc::geteuid() }, 0);
    for action in ["check-staging-links", "remove-staging", "seal", "unseal"] {
        let grant = Command::new("/usr/bin/sudo")
            .args(["-n", "-l", HELPER, action, "x"])
            .output()
            .unwrap();
        assert!(
            grant.status.success(),
            "missing grant for {action}: {}",
            String::from_utf8_lossy(&grant.stderr)
        );
        for joined in [false, true] {
            let mut command = Command::new("/usr/bin/sudo");
            command.args(["-n", HELPER]).env("LC_ALL", "C");
            if joined {
                command.arg(format!("{action} x"));
            } else {
                command.args([action, "x"]);
            }
            let output = command.output().unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "malformed form succeeded: {command:?}"
            );
            // sudo-rs may refuse a joined argv before exec; classic sudo may
            // admit it. Neither may reach normal runner initialization.
            assert!(
                error.contains("invalid fixed permissions helper invocation")
                    || (joined && error.contains("not allowed")),
                "{command:?}: {error}"
            );
        }
    }
}

#[test]
fn mount_id_matches_the_kernels_fdinfo_mount_id() {
    // mount_id reads raw statx bytes at fixed offsets; /proc's fdinfo reports
    // the same mount id, so a wrong offset or mask shows up here on any libc.
    let scratch = ScratchDir::new("mount-id");
    for path in [Path::new("/"), scratch.path(), Path::new("/proc")] {
        let fd: OwnedFd = fs::File::open(path).unwrap().into();
        let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).unwrap();
        let expected: u64 = fdinfo
            .lines()
            .find_map(|line| line.strip_prefix("mnt_id:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(mount_id(&fd).unwrap(), expected, "{}", path.display());
    }
}

#[test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts"]
fn isolated_staging_in_process_ignores_inherited_sudo_uid() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    const CHILD: &str = "MOTOR_STAGING_IDENTITY_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert_ne!(std::env::var("SUDO_UID").unwrap(), "0");
        assert_eq!(StagingOwners::current().unwrap().runner, 0);
        return;
    }
    // Use a child process so this identity test never mutates another test's
    // environment. A real non-root account makes the delegated path distinguishable.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "permissions::tests::isolated_staging_in_process_ignores_inherited_sudo_uid",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env(
            "SUDO_UID",
            crate::isolation::job_identity().unwrap().uid.to_string(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
