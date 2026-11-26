use super::*;
use crate::filesystem::set_directory_modified;
use cortexkit_test_support::ScratchDir;
use std::{path::PathBuf, time::UNIX_EPOCH};

fn guest_folder(evidence: &Path) -> PathBuf {
    evidence.join(format!("{}-guest", Uuid::now_v7()))
}

fn make_folder(path: &Path, modified: SystemTime) {
    fs::create_dir_all(path.join("logs")).unwrap();
    fs::write(path.join("logs/guest.log"), b"guest log").unwrap();
    set_directory_modified(path, modified).unwrap();
}

#[test]
fn prunes_only_old_unheld_uuid_guest_directories() {
    let root = ScratchDir::new("guest-evidence");
    let evidence = root.join("evidence");
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let cutoff = now - Duration::from_secs(72 * 60 * 60);
    let old = cutoff - Duration::from_secs(1);
    let expired = guest_folder(&evidence);
    make_folder(&expired, old);

    let kept_id = Uuid::now_v7();
    let kept = [
        (guest_folder(&evidence), now),
        (guest_folder(&evidence), cutoff),
        (evidence.join(format!("{kept_id}-guest")), old),
        (evidence.join("not-a-uuid-guest"), old),
        (
            evidence.join(format!("{}-guest-extra", Uuid::now_v7())),
            old,
        ),
        (
            evidence.join(format!("{}-guest", Uuid::now_v7().simple())),
            old,
        ),
        (guest_folder(&evidence.join("saved")), old),
    ];
    for (path, mtime) in &kept {
        make_folder(path, *mtime);
    }
    let file = guest_folder(&evidence);
    fs::write(&file, b"not a directory").unwrap();

    let removed =
        GuestEvidence::default().prune_with(&root, &BTreeSet::from([kept_id]), now, |path| {
            fs::remove_dir_all(path)
        });

    assert_eq!(removed, (1, 9));
    assert!(!expired.exists());
    for (path, _) in kept {
        assert_eq!(fs::read(path.join("logs/guest.log")).unwrap(), b"guest log");
    }
    assert_eq!(fs::read(file).unwrap(), b"not a directory");
}

#[test]
fn partial_deletion_is_retried_even_if_it_refreshed_the_folder_age() {
    let root = ScratchDir::new("guest-evidence-retry");
    let evidence = root.join("evidence");
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let old = now - Duration::from_secs(73 * 60 * 60);
    let retry = guest_folder(&evidence);
    let other = guest_folder(&evidence);
    make_folder(&retry, old);
    make_folder(&other, old);

    let pruner = GuestEvidence::default();
    let held = BTreeSet::new();
    assert_eq!(
        pruner.prune_with(&root, &held, now, |path| {
            if path == retry {
                fs::remove_dir_all(path.join("logs"))?;
                set_directory_modified(path, now)?;
                return Err(io::Error::other("simulated partial deletion failure"));
            }
            fs::remove_dir_all(path)
        }),
        (1, 9)
    );
    assert!(retry.is_dir());
    assert!(!other.exists());

    pruner.prune(&root, &held, now);

    assert!(!retry.exists());
}

#[cfg(unix)]
#[test]
fn skips_linked_evidence_and_folders_containing_links() {
    use std::os::unix::fs::symlink;

    let root = ScratchDir::new("guest-evidence-links");
    let evidence = root.join("evidence");
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let old = now - Duration::from_secs(73 * 60 * 60);
    let target = root.join("outside");
    let linked = guest_folder(&evidence);
    let contains_link = guest_folder(&evidence);
    make_folder(&target, old);
    make_folder(&contains_link, old);
    symlink(&target, &linked).unwrap();
    let nested_link = contains_link.join("logs/nested-link");
    symlink(&target, &nested_link).unwrap();

    GuestEvidence::default().prune(&root, &BTreeSet::new(), now);

    assert!(linked.is_symlink());
    assert!(nested_link.is_symlink());
    assert_eq!(
        fs::read(target.join("logs/guest.log")).unwrap(),
        b"guest log"
    );
}
