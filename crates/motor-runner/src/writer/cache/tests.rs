use super::*;
use cortexkit_test_support::ScratchDir;
use std::os::unix::fs::{symlink, OpenOptionsExt};

fn account() -> WriterAccount {
    WriterAccount {
        uid: unsafe { libc::geteuid() },
    }
}

#[test]
fn cache_fill_repairs_only_changed_entries_and_never_follows_symlinks() {
    let scratch = ScratchDir::new("cache-fill");
    let fleet = scratch.join("fleet");
    fs::create_dir_all(fleet.join("a/b")).unwrap();
    let old = fleet.join("a/b/old-entry");
    fs::write(&old, "unchanged").unwrap();
    let before = fs::metadata(&old).unwrap();
    let fill = CacheFill::begin_at(&fleet, account()).unwrap();
    let temporary = fleet.join("temporary");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .unwrap();
    let entry = fleet.join("a/b/private-entry");
    fs::rename(&temporary, &entry).unwrap();
    fs::create_dir_all(fleet.join("c/d")).unwrap();
    let nested = fleet.join("c/d/new-shard-entry");
    fs::write(&nested, "new entry").unwrap();
    let outside = scratch.join("outside");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, fleet.join("c/d/link")).unwrap();
    let mut repaired = BTreeSet::new();
    fill.finish_with(|path, _| {
        repaired.insert(path.to_path_buf());
        Ok(())
    })
    .unwrap();
    assert!(repaired.contains(&entry));
    assert!(repaired.contains(&nested));
    assert!(!repaired.contains(&old));
    assert!(!repaired.contains(&outside));
    assert!(!repaired.contains(&fleet.join("c/d/link")));
    let after = fs::metadata(&old).unwrap();
    assert_eq!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec())
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
}

#[test]
fn cache_fill_tracking_and_repair_fail_closed() {
    let scratch = ScratchDir::new("cache-fill-errors");
    let fleet = scratch.join("fleet");
    fs::create_dir(&fleet).unwrap();
    let fill = CacheFill::begin_at(&fleet, account()).unwrap();
    for mask in [
        libc::IN_Q_OVERFLOW,
        libc::IN_DELETE_SELF,
        libc::IN_MOVE_SELF,
        libc::IN_IGNORED,
    ] {
        let event = libc::inotify_event {
            wd: -1,
            mask,
            cookie: 0,
            len: 0,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&event as *const libc::inotify_event).cast::<u8>(),
                std::mem::size_of_val(&event),
            )
        };
        assert!(
            fill.record_events(bytes, &mut BTreeMap::new()).is_err(),
            "lost event mask {mask}"
        );
    }
    fs::write(fleet.join("entry"), "private").unwrap();
    let error = fill
        .finish_with(|_, _| Err(io::Error::other("ACL repair failed")))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "repair cache ACL {}: ACL repair failed",
            fleet.join("entry").display()
        )
    );
    assert!(CacheFill::begin_at(
        &fleet,
        WriterAccount {
            uid: account().uid + 1
        }
    )
    .is_err());
    let mut changed_owner = CacheFill::begin_at(&fleet, account()).unwrap();
    changed_owner.owner += 1;
    fs::write(fleet.join("entry"), "changed").unwrap();
    assert!(changed_owner.finish_with(|_, _| Ok(())).is_err());
}
