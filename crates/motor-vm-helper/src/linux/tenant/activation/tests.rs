use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

const VECTOR: &[u8] = include_bytes!("../../../../tests/vectors/runner-args-staged.json");

#[test]
fn staged_file_security_requires_root_0600_regular_and_no_follow() {
    check_metadata(0, 0o100600, true).unwrap();
    for (uid, mode, regular) in [
        (1, 0o600, true),
        (0, 0o644, true),
        (0, 0o660, true),
        (0, 0o4600, true),
        (0, 0o600, false),
    ] {
        assert!(check_metadata(uid, mode, regular).is_err());
    }
    let scratch = cortexkit_test_support::ScratchDir::new("staged-file");
    let path = scratch.join("runner-args.staged");
    assert!(read_staged(&path).unwrap().is_none());
    std::fs::write(&path, VECTOR).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_staged(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(read_staged(&path).is_err());
    }
    let link = scratch.join("linked");
    symlink(&path, &link).unwrap();
    assert!(read_staged(&link).is_err());
    std::fs::remove_file(&link).unwrap();
    symlink(scratch.join("missing"), &link).unwrap();
    assert!(read_staged(&link).is_err());
    assert!(available(scratch.join("missing").as_path()).is_err());
    assert!(available(&path).is_ok());
}

#[test]
fn failed_completion_directory_barrier_restores_staged_intent() {
    let scratch = cortexkit_test_support::ScratchDir::new("staged-completion");
    let path = scratch.join("runner-args.staged");
    std::fs::write(&path, VECTOR).unwrap();
    let activation = ResizeActivation::parse(VECTOR).unwrap();
    assert!(complete_with(&path, &activation, || Err(Error(
        "directory fsync failed".into()
    )))
    .is_err());
    let restored = ResizeActivation::parse(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(restored.runner_args, activation.runner_args);
    assert!(restored.quota_grew);
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    complete(&path, &activation).unwrap();
    assert!(!path.exists());
}
