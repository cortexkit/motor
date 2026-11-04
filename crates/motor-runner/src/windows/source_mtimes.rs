//! Source timestamps belong to the persistent workspace disk, not to a job VM.
use super::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::BTreeMap, io::Read};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILES: usize = 100_000;
const CANDIDATE: &str = "source-mtimes.json";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DiskIdentity {
    device: u64,
    file: u64,
    created: Option<SystemTime>,
}

fn identity(disk: &Path) -> Option<DiskIdentity> {
    let metadata = fs::symlink_metadata(disk).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let (device, file, _) = crate::filesystem::identity(disk).ok()?;
    Some(DiskIdentity {
        device,
        file,
        created: metadata.created().ok(),
    })
}

#[derive(Serialize, Deserialize)]
struct Entry {
    blob: String,
    modified_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct Record {
    disk: Option<DiskIdentity>,
    files: BTreeMap<String, Entry>,
}

fn load(path: &Path) -> Option<Record> {
    let file = crate::filesystem::read_nofollow(path).ok()?;
    if !file.metadata().ok()?.is_file() || file.metadata().ok()?.len() > MAX_BYTES {
        return None;
    }
    let record: Record = serde_json::from_reader(file.take(MAX_BYTES + 1)).ok()?;
    (record.files.len() <= MAX_FILES).then_some(record)
}

pub(super) fn prepare(
    snapshots: &[super::prep::Snapshot],
    staging: &Path,
    disk: &Path,
    record_path: &Path,
    now: SystemTime,
) -> io::Result<()> {
    let disk = identity(disk);
    let previous = load(record_path).filter(|r| disk.is_some() && r.disk == disk);
    let now_ms = u64::try_from(now.duration_since(UNIX_EPOCH).map_err(invalid)?.as_millis())
        .map_err(invalid)?;
    let mut record = Record {
        disk,
        files: BTreeMap::new(),
    };
    let mut bytes = 1024;
    let mut bounded = true;
    for snapshot in snapshots {
        let output = super::prep::git(
            snapshot,
            &["ls-tree", "-r", "-z", "--full-tree", &snapshot.commit],
        )?;
        for line in output
            .stdout
            .split(|b| *b == 0)
            .filter(|line| !line.is_empty())
        {
            let line = std::str::from_utf8(line).map_err(invalid)?;
            let (header, name) = line
                .split_once('\t')
                .ok_or_else(|| invalid("invalid git tree entry"))?;
            let mut fields = header.split_whitespace();
            let _mode = fields.next();
            if fields.next() != Some("blob") {
                continue;
            }
            let blob = fields
                .next()
                .ok_or_else(|| invalid("missing git blob id"))?;
            let path = super::prep::mapped(&snapshot.destination)?.join(name);
            let key = path.to_string_lossy().replace('\\', "/");
            motor_winagent::validate_archive_path(&key)?;
            let path = staging.join(path);
            // export-ignore paths are absent from git archive. Do not create them.
            if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                continue;
            }
            let modified_ms = previous
                .as_ref()
                .and_then(|r| r.files.get(&key))
                .filter(|entry| entry.blob == blob && entry.modified_ms <= now_ms)
                .map_or(now_ms, |entry| entry.modified_ms);
            let modified = UNIX_EPOCH + Duration::from_millis(modified_ms);
            fs::OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_times(fs::FileTimes::new().set_modified(modified))?;
            if bounded {
                let entry = Entry {
                    blob: blob.into(),
                    modified_ms,
                };
                bytes += serde_json::to_vec(&(&key, &entry)).map_err(invalid)?.len() + 2;
                if bytes as u64 > MAX_BYTES || record.files.len() == MAX_FILES {
                    record.files.clear();
                    bounded = false;
                } else {
                    record.files.insert(key, entry);
                }
            }
        }
    }
    // An empty candidate also replaces any oversized previous record at commit.
    atomic_json(&staging.join(CANDIDATE), &record)
}

/// Boot creates a missing cache disk after preparation. Only bind the successful
/// staging to that disk once it exists; failed preparation never replaces history.
pub(super) fn commit(staging: &Path, disk: &Path, record_path: &Path) -> io::Result<()> {
    let Some(mut record) = load(&staging.join(CANDIDATE)) else {
        return forget(record_path);
    };
    let current = identity(disk);
    if current.is_none() || (record.disk.is_some() && record.disk != current) {
        return forget(record_path);
    }
    record.disk = current;
    atomic_json(record_path, &record)
}

fn forget(path: &Path) -> io::Result<()> {
    // A job may build a different snapshot even if its history cannot be saved.
    // Keeping older history could later make a revert look unchanged to Cargo.
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        snapshots: Vec<super::super::prep::Snapshot>,
        disk: PathBuf,
        record: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = cortexkit_test_support::scratch_root()
                .join(format!("source-mtimes-{}", Uuid::now_v7()));
            fs::create_dir_all(&root).unwrap();
            let mut snapshots = Vec::new();
            for name in ["main", "sibling"] {
                let repo = root.join(name);
                fs::create_dir_all(&repo).unwrap();
                git(&repo, &["init", "-q"]);
                git(&repo, &["config", "user.name", "Fixture"]);
                git(&repo, &["config", "user.email", "fixture@example.invalid"]);
                fs::write(repo.join("unchanged.rs"), "unchanged").unwrap();
                fs::write(repo.join("changed.rs"), "version one").unwrap();
                git(&repo, &["add", "."]);
                git(&repo, &["commit", "-qm", "first"]);
                snapshots.push(super::super::prep::Snapshot {
                    mirror: repo.join(".git"),
                    commit: git(&repo, &["rev-parse", "HEAD"]),
                    destination: format!("/work/{name}"),
                });
            }
            let disk = root.join("cache.qcow2");
            fs::write(&disk, "cache disk").unwrap();
            let record = root.join("cache.source-mtimes.json");
            Self {
                root,
                snapshots,
                disk,
                record,
            }
        }
        fn stage(&self, label: &str, seconds: u64) -> PathBuf {
            let staging = self.root.join(label);
            fs::create_dir_all(&staging).unwrap();
            for snapshot in &self.snapshots {
                let output = super::super::prep::git(
                    snapshot,
                    &[
                        "archive",
                        "--format=tar",
                        &format!("{}^{{tree}}", snapshot.commit),
                    ],
                )
                .unwrap();
                let root = staging.join(super::super::prep::mapped(&snapshot.destination).unwrap());
                fs::create_dir_all(&root).unwrap();
                tar::Archive::new(io::Cursor::new(output.stdout))
                    .unpack(root)
                    .unwrap();
            }
            prepare(
                &self.snapshots,
                &staging,
                &self.disk,
                &self.record,
                UNIX_EPOCH + Duration::from_secs(seconds),
            )
            .unwrap();
            commit(&staging, &self.disk, &self.record).unwrap();
            staging
        }
        fn change(&mut self, contents: &str) {
            for snapshot in &mut self.snapshots {
                let repo = snapshot.mirror.parent().unwrap();
                fs::write(repo.join("changed.rs"), contents).unwrap();
                fs::write(repo.join("new.rs"), contents).unwrap();
                git(repo, &["add", "."]);
                git(repo, &["commit", "-qm", "change"]);
                snapshot.commit = git(repo, &["rev-parse", "HEAD"]);
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn mtime(staging: &Path, repository: &str, file: &str) -> u64 {
        fs::metadata(staging.join(format!("m/work/{repository}/{file}")))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn unchanged_files_keep_mtime_and_changes_advance_for_main_and_sibling() {
        let mut f = Fixture::new();
        let first = f.stage("first", 1_800_000_000);
        f.change("version two");
        let second = f.stage("second", 1_800_000_010);
        for repo in ["main", "sibling"] {
            assert_eq!(mtime(&first, repo, "unchanged.rs"), 1_800_000_000);
            assert_eq!(mtime(&second, repo, "unchanged.rs"), 1_800_000_000);
            assert_eq!(mtime(&second, repo, "changed.rs"), 1_800_000_010);
            assert_eq!(mtime(&second, repo, "new.rs"), 1_800_000_010);
        }
        f.change("version one");
        let third = f.stage("third", 1_800_000_020);
        for repo in ["main", "sibling"] {
            assert_eq!(
                mtime(&third, repo, "changed.rs"),
                1_800_000_020,
                "reverting content is still a change"
            );
        }
    }

    #[test]
    fn absent_corrupt_oversized_and_recreated_disk_records_stamp_every_file_now() {
        let f = Fixture::new();
        f.stage("first", 1_800_000_000);
        for (index, case) in ["missing", "unreadable", "corrupt", "oversized", "mismatch"]
            .iter()
            .enumerate()
        {
            match *case {
                "missing" => fs::remove_file(&f.record).unwrap(),
                "unreadable" => {
                    fs::remove_file(&f.record).unwrap();
                    fs::create_dir(&f.record).unwrap();
                }
                "corrupt" => fs::write(&f.record, "not json").unwrap(),
                "oversized" => fs::OpenOptions::new()
                    .write(true)
                    .open(&f.record)
                    .unwrap()
                    .set_len(MAX_BYTES + 1)
                    .unwrap(),
                _ => {
                    fs::rename(&f.disk, f.root.join("old-disk")).unwrap();
                    fs::write(&f.disk, "replacement disk").unwrap();
                }
            }
            let staging = f.root.join(format!("fallback-{index}"));
            // Restore the path after observing the unreadable-record fallback,
            // so committing the successful staging can replace its history.
            let mut snapshots = Vec::new();
            for snapshot in &f.snapshots {
                snapshots.push(super::super::prep::Snapshot {
                    mirror: snapshot.mirror.clone(),
                    commit: snapshot.commit.clone(),
                    destination: snapshot.destination.clone(),
                });
            }
            fs::create_dir_all(&staging).unwrap();
            for snapshot in &snapshots {
                let root = staging.join(super::super::prep::mapped(&snapshot.destination).unwrap());
                fs::create_dir_all(&root).unwrap();
                fs::write(root.join("unchanged.rs"), "unchanged").unwrap();
                fs::write(root.join("changed.rs"), "version one").unwrap();
            }
            let now = 1_800_000_100 + index as u64;
            prepare(
                &snapshots,
                &staging,
                &f.disk,
                &f.record,
                UNIX_EPOCH + Duration::from_secs(now),
            )
            .unwrap();
            for repo in ["main", "sibling"] {
                for file in ["unchanged.rs", "changed.rs"] {
                    assert_eq!(mtime(&staging, repo, file), now, "{case}");
                }
            }
            if *case == "unreadable" {
                fs::remove_dir(&f.record).unwrap();
            }
            commit(&staging, &f.disk, &f.record).unwrap();
        }
    }

    #[test]
    fn fractional_staging_times_survive_transfer_and_guest_unpack() {
        let f = Fixture::new();
        let staging = f.stage("fractional", 1_800_000_000);
        fs::remove_file(&f.record).unwrap();
        let now = UNIX_EPOCH + Duration::from_millis(1_800_000_010_123);
        prepare(&f.snapshots, &staging, &f.disk, &f.record, now).unwrap();
        super::super::prep::build_tar(&staging).unwrap();
        let guest = f.root.join("guest");
        let cargo = f.root.join("cargo-home");
        fs::create_dir_all(&guest).unwrap();
        fs::create_dir_all(&cargo).unwrap();
        motor_winagent::unpack_tree(
            &mut fs::File::open(staging.join("tree.tar")).unwrap(),
            &guest,
            &cargo,
        )
        .unwrap();
        for repo in ["main", "sibling"] {
            for file in ["unchanged.rs", "changed.rs"] {
                assert_eq!(
                    fs::metadata(guest.join(format!("work/{repo}/{file}")))
                        .unwrap()
                        .modified()
                        .unwrap(),
                    now
                );
            }
        }
    }

    #[test]
    fn unidentifiable_disk_or_missing_candidate_discards_older_history() {
        let f = Fixture::new();
        let staging = f.stage("first", 1_800_000_000);
        fs::rename(&f.disk, f.root.join("old-disk")).unwrap();
        commit(&staging, &f.disk, &f.record).unwrap();
        assert!(!f.record.exists());
        fs::write(&f.disk, "new disk").unwrap();
        let staging = f.stage("second", 1_800_000_010);
        fs::remove_file(staging.join(CANDIDATE)).unwrap();
        commit(&staging, &f.disk, &f.record).unwrap();
        assert!(!f.record.exists());
    }

    #[test]
    fn failed_staging_does_not_replace_history() {
        let mut f = Fixture::new();
        f.stage("first", 1_800_000_000);
        let original = fs::read(&f.record).unwrap();
        f.snapshots[1].commit = "not-a-commit".into();
        let staging = f.root.join("failed");
        fs::create_dir_all(&staging).unwrap();
        assert!(prepare(
            &f.snapshots,
            &staging,
            &f.disk,
            &f.record,
            UNIX_EPOCH + Duration::from_secs(1_800_000_010)
        )
        .is_err());
        assert_eq!(fs::read(&f.record).unwrap(), original);
        assert!(!staging.join(CANDIDATE).exists());
    }
}
