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
