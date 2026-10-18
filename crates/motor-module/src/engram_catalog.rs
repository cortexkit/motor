use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Serialize)]
struct Catalog {
    schema_version: u8,
    module_id: &'static str,
    entries: Vec<Entry>,
}

#[derive(Serialize)]
struct Entry {
    entry_id: String,
    class: &'static str,
    reason: &'static str,
    mechanism: &'static str,
    path: String,
}

const FILETREE: &str = "filetree";
const CONFIG_REASON: &str = "Operator routing settings can be reused on another machine.";
const STORAGE_REASON: &str = "Not backed up on purpose: jobs from the last 24 hours, their runner bindings and pending cancels, plus per-start runtime sockets and the lease; a restored copy would be stale and could replay a cancel.";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_best_effort(data_dir: io::Result<PathBuf>, storage_root: &Path) {
    let result = data_dir.and_then(|data_dir| write_catalog(&data_dir, storage_root));
    if let Err(error) = result {
        tracing::warn!(%error, "could not write Engram backup catalog");
    }
}

fn write_catalog(data_dir: &Path, storage_root: &Path) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let bytes = catalog_bytes(data_dir, storage_root)?;
    atomic_write(&data_dir.join("engram-catalog.json"), &bytes)
}

fn catalog_bytes(data_dir: &Path, storage_root: &Path) -> io::Result<Vec<u8>> {
    let data_root = fs::canonicalize(data_dir)?;
    let mut entries = Vec::new();

    let config = data_dir.join("config.jsonc");
    match fs::symlink_metadata(&config) {
        Ok(metadata) if metadata.file_type().is_file() => entries.push(Entry {
            entry_id: "ck-motor/config".into(),
            class: "portable",
            reason: CONFIG_REASON,
            mechanism: FILETREE,
            path: "config.jsonc".into(),
        }),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let storage = match fs::canonicalize(storage_root) {
        Ok(path) if path.is_dir() => path,
        Ok(_) => return catalog_json(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return catalog_json(entries),
        Err(error) => return Err(error),
    };
    if let Ok(relative) = storage.strip_prefix(&data_root) {
        if let Some(path) = canonical_relative_path(relative) {
            entries.push(Entry {
                entry_id: "ck-motor/storage".into(),
                class: "device-local",
                reason: STORAGE_REASON,
                mechanism: FILETREE,
                path,
            });
        }
    }

    catalog_json(entries)
}

fn catalog_json(mut entries: Vec<Entry>) -> io::Result<Vec<u8>> {
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    serde_json::to_vec(&Catalog {
        schema_version: 1,
        module_id: "ck-motor",
        entries,
    })
    .map_err(io::Error::other)
}

fn canonical_relative_path(path: &Path) -> Option<String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value.to_str()?),
            _ => return None,
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => match fs::read(path) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    atomic_write_with(path, bytes, |_| Ok(()))
}

fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("catalog path has no parent directory"))?;
    let (temporary_path, mut temporary_file) = create_temporary_file(parent)?;
    let result = (|| {
        temporary_file.write_all(bytes)?;
        temporary_file.sync_all()?;
        drop(temporary_file);
        before_rename(&temporary_path)?;
        fs::rename(&temporary_path, path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".engram-catalog.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique Engram catalog temporary file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn fixture() -> (cortexkit_test_support::ScratchDir, PathBuf, PathBuf) {
        let scratch = cortexkit_test_support::ScratchDir::new("engram-catalog");
        let data_dir = scratch.join("cortexkit/ck-motor");
        let storage_root = data_dir.join("store.db.ck-motor");
        fs::create_dir_all(&storage_root).unwrap();
        fs::write(data_dir.join("config.jsonc"), "{}\n").unwrap();
        (scratch, data_dir, storage_root)
    }

    #[test]
    fn catalog_has_deterministic_exact_engram_v1_bytes() {
        let (_scratch, data_dir, storage_root) = fixture();
        let bytes = catalog_bytes(&data_dir, &storage_root).unwrap();
        assert_eq!(
            bytes,
            br#"{"schema_version":1,"module_id":"ck-motor","entries":[{"entry_id":"ck-motor/config","class":"portable","reason":"Operator routing settings can be reused on another machine.","mechanism":"filetree","path":"config.jsonc"},{"entry_id":"ck-motor/storage","class":"device-local","reason":"Not backed up on purpose: jobs from the last 24 hours, their runner bindings and pending cancels, plus per-start runtime sockets and the lease; a restored copy would be stale and could replay a cancel.","mechanism":"filetree","path":"store.db.ck-motor"}]}"#
        );
    }

    #[test]
    fn storage_outside_module_data_directory_is_not_listed() {
        let scratch = cortexkit_test_support::ScratchDir::new("engram-catalog-outside");
        let data_dir = scratch.join("data");
        let storage_root = scratch.join("elsewhere/store.db.ck-motor");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&storage_root).unwrap();
        fs::write(data_dir.join("config.jsonc"), "{}\n").unwrap();
        let json = String::from_utf8(catalog_bytes(&data_dir, &storage_root).unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"schema_version":1,"module_id":"ck-motor","entries":[{"entry_id":"ck-motor/config","class":"portable","reason":"Operator routing settings can be reused on another machine.","mechanism":"filetree","path":"config.jsonc"}]}"#
        );
    }

    #[test]
    fn identical_catalog_keeps_its_modification_time() {
        let (_scratch, data_dir, storage_root) = fixture();
        write_catalog(&data_dir, &storage_root).unwrap();
        let path = data_dir.join("engram-catalog.json");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        #[cfg(unix)]
        let before_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).unwrap().ino()
        };
        std::thread::sleep(Duration::from_millis(20));
        write_catalog(&data_dir, &storage_root).unwrap();
        let after_metadata = fs::metadata(&path).unwrap();
        assert_eq!(after_metadata.modified().unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(after_metadata.ino(), before_inode);
        }
    }

    #[test]
    fn catalog_is_written_from_a_synced_private_temporary_file_then_renamed() {
        let scratch = cortexkit_test_support::ScratchDir::new("engram-catalog-atomic");
        let path = scratch.join("engram-catalog.json");
        fs::write(&path, b"old bytes").unwrap();
        atomic_write_with(&path, b"complete new bytes", |temporary| {
            assert_eq!(temporary.parent(), path.parent());
            assert_eq!(fs::read(&path)?, b"old bytes");
            assert_eq!(fs::read(temporary)?, b"complete new bytes");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(fs::metadata(temporary)?.permissions().mode() & 0o777, 0o600);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"complete new bytes");
        let leftovers = fs::read_dir(scratch.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn failed_catalog_write_emits_one_warning_and_does_not_panic() {
        let scratch = cortexkit_test_support::ScratchDir::new("engram-catalog-warning");
        let not_a_directory = scratch.join("file");
        fs::write(&not_a_directory, "not a directory").unwrap();
        let captured = Captured::default();
        let _guard = crate::logging::scoped(captured.clone());
        write_best_effort(Ok(not_a_directory), &scratch.join("unused-root"));
        let lines: Vec<_> = captured.text().lines().map(str::to_owned).collect();
        assert_eq!(lines.len(), 1, "unexpected log output: {lines:?}");
        assert!(lines[0].contains("WARN"), "{lines:?}");
        assert!(
            lines[0].contains("could not write Engram backup catalog"),
            "{lines:?}"
        );
    }
}
