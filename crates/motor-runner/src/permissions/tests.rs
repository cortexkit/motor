use super::*;
use cortexkit_test_support::ScratchDir;
use std::os::unix::{
    fs::{symlink, PermissionsExt},
    net::UnixListener,
};

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
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
    let _listener = UnixListener::bind(&socket).unwrap();
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
