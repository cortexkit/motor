use super::*;

#[test]
fn an_unattainable_hard_floor_is_a_configuration_error() {
    let volume = Path::new("/runner/state");
    let total = 64 << 30;
    for floor in [total, 128 << 30] {
        let error = check_floor(volume, floor, |path| {
            assert_eq!(path, volume);
            Ok(total)
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("--free-space-floor-bytes"), "{error}");
        assert!(error.contains(&format!("{floor} bytes")), "{error}");
        assert!(
            error.contains(&format!("total volume size of {total} bytes")),
            "{error}"
        );
    }
    check_floor(volume, total - 1, |_| Ok(total)).unwrap();
    check_floor(volume, 0, |_| panic!("disabled floor needs no space probe")).unwrap();
}

#[test]
fn cargo_gc_rejects_noatime_on_the_visible_mount() {
    let mounts = "10 0 8:1 / / rw,relatime - xfs /dev/root rw\n\
        11 10 8:1 /data /work\\040trees rw,relatime - xfs /dev/root rw\n\
        12 10 8:1 /other /work\\040trees rw,noatime - xfs /dev/root rw\n\
        13 10 8:1 /strict /strict rw - xfs /dev/root rw\n\
        14 10 0:50 / /dataset rw - zfs pool/dataset rw,noatime\n";
    let targets = [(PathBuf::from("/work trees/repo/target"), 12)];
    let error = crate::caller_path::startup::check_gc_mounts(mounts, targets)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Cargo GC needs relatime or strictatime"),
        "{error}"
    );
    assert!(
        error.contains("/work trees/repo/target is on the noatime mount /work trees"),
        "{error}"
    );
    assert!(crate::caller_path::startup::check_gc_mounts(
        mounts,
        [(PathBuf::from("/dataset"), 14)]
    )
    .is_err());
    crate::caller_path::startup::check_gc_mounts(
        mounts,
        [
            (PathBuf::from("/work trees/repo/target"), 11),
            (PathBuf::from("/strict/target"), 13),
        ],
    )
    .unwrap();
}

#[test]
fn missing_reflink_support_names_the_state_volume() {
    let state = Path::new("/runner/state");
    let error = check_reflink(state, |path| {
        assert_eq!(path, state);
        Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
    })
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("requires FICLONE reflinks in /runner/state"),
        "{error}"
    );
    check_reflink(state, |_| Ok(())).unwrap();
}
