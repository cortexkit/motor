//! Source timestamps belong to the persistent workspace disk, not to a job VM.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILES: usize = 100_000;

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
    #[serde(default)]
    sealed: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    disk: Option<DiskIdentity>,
    files: BTreeMap<String, Entry>,
    #[serde(default)]
    attributes: String,
    ended_ms: Option<u64>,
}

pub(super) struct Policy<'a> {
    pub(super) exported: &'a BTreeSet<String>,
    pub(super) sealed: &'a motor_winagent::source_seal::Plan,
    pub(super) concurrent_jobs: u32,
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
    policy: Policy<'_>,
    now: SystemTime,
) -> io::Result<Record> {
    let disk = identity(disk);
    let previous = load(record_path).filter(|r| {
        disk.is_some()
            && r.disk == disk
            && r.ended_ms
                .is_some_and(|end| end.checked_add(1000).is_some())
    });
    let now_ms = u64::try_from(now.duration_since(UNIX_EPOCH).map_err(invalid)?.as_millis())
        .map_err(invalid)?;
    let fresh_ms = previous
        .as_ref()
        .and_then(|r| r.ended_ms)
        .map_or(now_ms, |end| now_ms.max(end + 1000));
    let trees = snapshots
        .iter()
        .map(|snapshot| {
            super::prep::git(
                snapshot,
                &["ls-tree", "-r", "-z", "--full-tree", &snapshot.commit],
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut attributes = Sha256::new();
    for (snapshot, tree) in snapshots.iter().zip(&trees) {
        for (name, blob) in blobs(&tree.stdout)? {
            if name.rsplit('/').next() == Some(".gitattributes") {
                attributes.update(
                    serde_json::to_vec(&(&snapshot.destination, name, blob)).map_err(invalid)?,
                );
            }
        }
    }
    let attributes = format!("{:x}", attributes.finalize());
    let previous = previous.filter(|r| r.attributes == attributes);
    let sealed: BTreeSet<_> = policy
        .sealed
        .files
        .iter()
        .map(|path| format!("m/{path}"))
        .collect();
    // Multiple slots let preparation overlap another VM's writes. Old source
    // times are reusable only when the slot serializes staging after teardown.
    let reuse = policy.concurrent_jobs == 1;
    let mut record = Record {
        disk,
        files: BTreeMap::new(),
        attributes,
        ended_ms: None,
    };
    let mut bytes = 1024;
    let mut bounded = true;
    for (snapshot, tree) in snapshots.iter().zip(&trees) {
        for (name, blob) in blobs(&tree.stdout)? {
            let path = super::prep::mapped(&snapshot.destination)?.join(name);
            let key = path.to_string_lossy().replace('\\', "/");
            motor_winagent::validate_archive_path(&key)?;
            let path = staging.join(path);
            // Filesystem existence is not membership: on NTFS an ignored name
            // can resolve to a differently cased exported file.
            if !policy.exported.contains(&key) {
                continue;
            }
            if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                continue;
            }
            let sealed = sealed.contains(&key);
            let modified_ms = previous
                .as_ref()
                .and_then(|r| r.files.get(&key))
                .filter(|entry| {
                    reuse
                        && sealed
                        && entry.sealed
                        && entry.blob == blob
                        && entry.modified_ms <= fresh_ms
                })
                .map_or(fresh_ms, |entry| entry.modified_ms);
            let modified = UNIX_EPOCH + Duration::from_millis(modified_ms);
            let mut options = fs::OpenOptions::new();
            options.write(true);
            // The preparation account can write staging, so a file checked above
            // could be swapped for a symlink before this open.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            options
                .open(&path)?
                .set_times(fs::FileTimes::new().set_modified(modified))?;
            if bounded {
                let entry = Entry {
                    blob: blob.into(),
                    modified_ms,
                    sealed,
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
    // Keep candidates in memory: staging is writable by the preparation account.
    Ok(record)
}

fn blobs(bytes: &[u8]) -> io::Result<Vec<(&str, &str)>> {
    bytes
        .split(|b| *b == 0)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parse = || {
                let line = std::str::from_utf8(line).map_err(invalid)?;
                let (header, name) = line
                    .split_once('\t')
                    .ok_or_else(|| invalid("invalid git tree entry"))?;
                let mut fields = header.split_whitespace();
                let _mode = fields.next();
                if fields.next() != Some("blob") {
                    return Ok(None);
                }
                let blob = fields
                    .next()
                    .ok_or_else(|| invalid("missing git blob id"))?;
                Ok(Some((name, blob)))
            };
            parse().transpose()
        })
        .collect()
}

/// Boot creates a missing cache disk after preparation. Only bind the successful
/// staging to that disk once it exists; failed preparation never replaces history.
pub(super) fn commit(mut record: Record, disk: &Path, record_path: &Path) -> io::Result<()> {
    let current = identity(disk);
    if current.is_none() || (record.disk.is_some() && record.disk != current) {
        return forget(record_path);
    }
    record.disk = current;
    record.ended_ms = None;
    write_record(record_path, &record)
}

/// Record the host-observed end only after the guest confirmed the job cannot
/// write or delete the selected source files and the host proved the VM stopped.
/// Missing end times, including records written before a crash, never authorize reuse.
pub(super) fn ended(disk: &Path, record_path: &Path, now_ms: u64) -> io::Result<()> {
    let Some(mut record) = load(record_path) else {
        return Ok(());
    };
    if record.disk.is_none() || record.disk != identity(disk) {
        return forget(record_path);
    }
    record.ended_ms = Some(now_ms);
    write_record(record_path, &record)
}

fn write_record(path: &Path, record: &Record) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("source timestamp record has no parent"))?;
    let temporary = parent.join(format!(".source-mtimes-{}.tmp", Uuid::now_v7()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        let bytes = serde_json::to_vec(record).map_err(invalid)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("source timestamp record exceeds size bound"));
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        crate::filesystem::rename(&temporary, path)?;
        crate::filesystem::sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
        fn extract(&self, label: &str) -> (PathBuf, BTreeSet<String>) {
            let staging = self.root.join(label);
            fs::create_dir_all(&staging).unwrap();
            let mut exported = BTreeSet::new();
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
                for entry in tar::Archive::new(io::Cursor::new(&output.stdout))
                    .entries()
                    .unwrap()
                {
                    let entry = entry.unwrap();
                    if entry.header().entry_type().is_file() {
                        let name = entry.path().unwrap().to_string_lossy().into_owned();
                        exported.insert(format!("m{}/{name}", snapshot.destination));
                    }
                }
                tar::Archive::new(io::Cursor::new(output.stdout))
                    .unpack(root)
                    .unwrap();
            }
            (staging, exported)
        }
        fn apply(
            &self,
            staging: &Path,
            exported: &BTreeSet<String>,
            sealed: &motor_winagent::source_seal::Plan,
            concurrent_jobs: u32,
            now: SystemTime,
        ) -> Record {
            prepare(
                &self.snapshots,
                staging,
                &self.disk,
                &self.record,
                Policy {
                    exported,
                    sealed,
                    concurrent_jobs,
                },
                now,
            )
            .unwrap()
        }
        fn stage(&self, label: &str, seconds: u64) -> PathBuf {
            let (staging, exported) = self.extract(label);
            let candidate = self.apply(
                &staging,
                &exported,
                &seal(&exported),
                1,
                UNIX_EPOCH + Duration::from_secs(seconds),
            );
            commit(candidate, &self.disk, &self.record).unwrap();
            ended(&self.disk, &self.record, seconds * 1000).unwrap();
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
    fn seal(exported: &BTreeSet<String>) -> motor_winagent::source_seal::Plan {
        motor_winagent::source_seal::Plan::new(
            exported
                .iter()
                .map(|path| path.strip_prefix("m/").unwrap().to_owned())
                .collect(),
        )
        .unwrap()
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
    fn reuse_requires_previous_and_current_byte_protection() {
        for (previously_sealed, currently_sealed) in [(false, true), (true, false), (false, false)]
        {
            let f = Fixture::new();
            let (first, exported) = f.extract("first");
            let plan = if previously_sealed {
                seal(&exported)
            } else {
                Default::default()
            };
            let candidate = f.apply(
                &first,
                &exported,
                &plan,
                1,
                UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            );
            commit(candidate, &f.disk, &f.record).unwrap();
            ended(&f.disk, &f.record, 1_800_000_000_000).unwrap();
            assert_eq!(
                load(&f.record).unwrap().files["m/work/main/unchanged.rs"].sealed,
                previously_sealed
            );
            let (second, exported) = f.extract("second");
            let plan = if currently_sealed {
                seal(&exported)
            } else {
                Default::default()
            };
            f.apply(
                &second,
                &exported,
                &plan,
                1,
                UNIX_EPOCH + Duration::from_secs(1_800_000_010),
            );
            for repo in ["main", "sibling"] {
                assert_eq!(mtime(&second, repo, "unchanged.rs"), 1_800_000_010);
            }
        }
    }

    #[test]
    fn attributes_in_any_snapshot_invalidate_all_recorded_times() {
        let mut f = Fixture::new();
        f.stage("first", 1_800_000_000);
        for (index, repository) in [0, 1].into_iter().enumerate() {
            let snapshot = &mut f.snapshots[repository];
            let repo = snapshot.mirror.parent().unwrap();
            fs::write(repo.join(".gitattributes"), "*.rs text eol=crlf\n").unwrap();
            git(repo, &["add", ".gitattributes"]);
            git(repo, &["commit", "-qm", "attributes only"]);
            snapshot.commit = git(repo, &["rev-parse", "HEAD"]);
            let now = 1_800_000_010 + index as u64 * 10;
            let staged = f.stage(&format!("attributes-{index}"), now);
            for repo in ["main", "sibling"] {
                assert_eq!(mtime(&staged, repo, "unchanged.rs"), now);
            }
        }
    }

    #[test]
    fn ignored_case_alias_is_not_an_exported_record_entry() {
        let mut f = Fixture::new();
        let snapshot = &mut f.snapshots[0];
        let repo = snapshot.mirror.parent().unwrap();
        git(repo, &["config", "core.ignorecase", "false"]);
        fs::write(repo.join("A.rs"), "exported").unwrap();
        fs::write(repo.join(".gitattributes"), "a.rs export-ignore\n").unwrap();
        git(repo, &["add", "."]);
        // Populate the case-distinct tree directly so the fixture also works
        // when its own checkout cannot hold both spellings as separate files.
        let ignored = git(repo, &["rev-parse", "HEAD:changed.rs"]);
        git(
            repo,
            &[
                "-c",
                "core.ignorecase=false",
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{ignored},a.rs"),
            ],
        );
        git(repo, &["commit", "-qm", "case alias fixture"]);
        snapshot.commit = git(repo, &["rev-parse", "HEAD"]);
        let (staging, exported) = f.extract("alias");
        assert!(exported.contains("m/work/main/A.rs"));
        assert!(!exported.contains("m/work/main/a.rs"));
        // A hard link models NTFS resolving the ignored spelling to the same
        // object while keeping the regression portable on case-sensitive hosts.
        if !staging.join("m/work/main/a.rs").exists() {
            fs::hard_link(
                staging.join("m/work/main/A.rs"),
                staging.join("m/work/main/a.rs"),
            )
            .unwrap();
        }
        let candidate = f.apply(
            &staging,
            &exported,
            &seal(&exported),
            1,
            UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        );
        assert!(candidate.files.contains_key("m/work/main/A.rs"));
        assert!(!candidate.files.contains_key("m/work/main/a.rs"));
    }

    #[test]
    fn multiple_slots_disable_old_time_reuse() {
        let f = Fixture::new();
        f.stage("first", 1_800_000_000);
        let (second, exported) = f.extract("second");
        f.apply(
            &second,
            &exported,
            &seal(&exported),
            2,
            UNIX_EPOCH + Duration::from_secs(1_800_000_010),
        );
        for repo in ["main", "sibling"] {
            assert_eq!(mtime(&second, repo, "unchanged.rs"), 1_800_000_010);
        }
    }

    #[test]
    fn changed_times_follow_completed_vm_end_even_if_host_clock_moves_back() {
        let mut f = Fixture::new();
        f.stage("first", 1_800_000_000);
        ended(&f.disk, &f.record, 1_800_000_010_000).unwrap();
        f.change("new bytes");
        let second = f.stage("backward", 1_799_999_000);
        assert_eq!(mtime(&second, "main", "changed.rs"), 1_800_000_011);
        let mut record = load(&f.record).unwrap();
        record.ended_ms = None;
        write_record(&f.record, &record).unwrap();
        let third = f.stage("no-end", 1_800_000_100);
        assert_eq!(mtime(&third, "main", "unchanged.rs"), 1_800_000_100);
        // Older records have no end field either, and must also rebuild.
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&f.record).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("ended_ms");
        fs::write(&f.record, serde_json::to_vec(&value).unwrap()).unwrap();
        let fourth = f.stage("old-record", 1_800_000_200);
        assert_eq!(mtime(&fourth, "main", "unchanged.rs"), 1_800_000_200);
    }

    #[test]
    #[cfg(unix)]
    fn record_never_writes_through_a_prep_owned_temporary_path() {
        let f = Fixture::new();
        let (staging, exported) = f.extract("prep-owned");
        let victim = f.root.join("runner-owned-canary");
        fs::write(&victim, "must survive").unwrap();
        std::os::unix::fs::symlink(&victim, staging.join("source-mtimes.tmp")).unwrap();
        let candidate = f.apply(
            &staging,
            &exported,
            &seal(&exported),
            1,
            UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        );
        commit(candidate, &f.disk, &f.record).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "must survive");
        assert!(!staging.join("source-mtimes.json").exists());
        assert!(staging.join("source-mtimes.tmp").is_symlink());
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
            let (staging, exported) = f.extract(&format!("fallback-{index}"));
            let now = 1_800_000_100 + index as u64;
            let candidate = f.apply(
                &staging,
                &exported,
                &seal(&exported),
                1,
                UNIX_EPOCH + Duration::from_secs(now),
            );
            for repo in ["main", "sibling"] {
                for file in ["unchanged.rs", "changed.rs"] {
                    assert_eq!(mtime(&staging, repo, file), now, "{case}");
                }
            }
            if *case == "unreadable" {
                fs::remove_dir(&f.record).unwrap();
            }
            commit(candidate, &f.disk, &f.record).unwrap();
            ended(&f.disk, &f.record, now * 1000).unwrap();
        }
    }

    #[test]
    fn fractional_staging_times_survive_transfer_and_guest_unpack() {
        let f = Fixture::new();
        let (staging, exported) = f.extract("fractional");
        let now = UNIX_EPOCH + Duration::from_millis(1_800_000_010_123);
        f.apply(&staging, &exported, &seal(&exported), 1, now);
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
    fn unidentifiable_disk_discards_older_history() {
        let f = Fixture::new();
        f.stage("first", 1_800_000_000);
        let candidate = load(&f.record).unwrap();
        fs::rename(&f.disk, f.root.join("old-disk")).unwrap();
        commit(candidate, &f.disk, &f.record).unwrap();
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
            Policy {
                exported: &BTreeSet::new(),
                sealed: &motor_winagent::source_seal::Plan::default(),
                concurrent_jobs: 1
            },
            UNIX_EPOCH + Duration::from_secs(1_800_000_010)
        )
        .is_err());
        assert_eq!(fs::read(&f.record).unwrap(), original);
        assert!(!staging.join("source-mtimes.json").exists());
    }
}
