//! Conservative Cargo output discovery. Deletion is performed by disk reclaim,
//! after it has excluded workspace users and restored access to job-owned files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) const STALE_AGE: Duration = Duration::from_secs(48 * 60 * 60);

fn directories(path: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect()
}

/// Only physical directories directly under target are profiles. Also support
/// Cargo's target/<triple>/<profile> layout, without descending arbitrary trees.
fn profiles(target: &Path) -> io::Result<Vec<PathBuf>> {
    let mut profiles = Vec::new();
    for path in directories(target)? {
        if !fs::symlink_metadata(&path)?.is_dir() {
            continue;
        }
        profiles.push(path.clone());
        if path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.contains('-') && name != ".fingerprint"
        }) {
            for nested in directories(&path)? {
                if fs::symlink_metadata(&nested)?.is_dir() {
                    profiles.push(nested);
                }
            }
        }
    }
    Ok(profiles)
}

pub(crate) fn incremental(target: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(profiles(target)?
        .into_iter()
        .map(|profile| profile.join("incremental"))
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()))
        .collect())
}

fn unit_key(name: &str) -> Option<String> {
    let (name, hash) = name.rsplit_once('-')?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        || hash.len() != 16
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    // Fingerprints use package names, whereas deps uses target names, which may
    // differ completely (for example a package with several named binaries).
    // Conservatively union every name with the same full hash: an ambiguous
    // match can retain extra outputs, never delete a recently read member.
    Some(hash.to_ascii_lowercase())
}

fn dep_key(name: &str) -> Option<String> {
    let stem = match name.split_once('.') {
        Some((
            stem,
            "rlib" | "rmeta" | "so" | "dylib" | "dll" | "a" | "d" | "exe" | "pdb" | "dll.lib"
            | "dll.exp" | "dSYM",
        )) => stem,
        Some((stem, suffix))
            if suffix.ends_with(".rcgu.o")
                && suffix.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                }) =>
        {
            stem
        }
        Some(_) => return None,
        None => name,
    };
    unit_key(stem)
}

/// A maintenance read_dir changes directory atime on relatime filesystems.
/// Judge nonempty trees by their files, not by those scanner-generated reads.
/// Symlinks, special files and mount boundaries make the entire unit ineligible.
fn newest_read(path: &Path, device: u64) -> io::Result<Option<SystemTime>> {
    let metadata = fs::symlink_metadata(path)?;
    if crate::filesystem::device(path)? != device
        || metadata.file_type().is_symlink()
        || (!metadata.is_dir() && !metadata.is_file())
    {
        return Ok(None);
    }
    let accessed = metadata.accessed()?;
    if metadata.is_file() {
        return Ok(Some(accessed));
    }
    let mut newest = None;
    for child in directories(path)? {
        let Some(time) = newest_read(&child, device)? else {
            return Ok(None);
        };
        newest = Some(newest.map_or(time, |old: SystemTime| old.max(time)));
    }
    Ok(Some(newest.unwrap_or(accessed)))
}

/// Each vector is indivisible: a recent member retains every output of a unit.
/// Unknown names and symlinks are never returned as deletion candidates.
pub(crate) fn stale_units(
    target: &Path,
    now: SystemTime,
    device: u64,
) -> io::Result<Vec<Vec<PathBuf>>> {
    let mut stale = Vec::new();
    for profile in profiles(target)? {
        let mut units: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut unknown = BTreeSet::new();
        for directory in ["deps", ".fingerprint", "build"] {
            for path in directories(&profile.join(directory))? {
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let key = if directory == "deps" {
                    dep_key(name)
                } else {
                    unit_key(name)
                };
                if let Some(key) = key {
                    units.entry(key).or_default().push(path);
                } else if directory == "deps" {
                    if let Some(key) = unit_key(name.split('.').next().unwrap_or(name)) {
                        unknown.insert(key);
                    }
                }
            }
        }
        for (key, paths) in units {
            if unknown.contains(&key) {
                continue;
            }
            let mut old = true;
            for path in &paths {
                old &= newest_read(path, device)?
                    .is_some_and(|read| now.duration_since(read).is_ok_and(|age| age > STALE_AGE));
            }
            if old {
                stale.push(paths);
            }
        }
        for path in directories(&profile.join("incremental"))? {
            if fs::symlink_metadata(&path)?.is_dir()
                && newest_read(&path, device)?
                    .is_some_and(|read| now.duration_since(read).is_ok_and(|age| age > STALE_AGE))
            {
                stale.push(vec![path]);
            }
        }
    }
    Ok(stale)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn output(root: &Path, name: &str, read: SystemTime) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [7; 8192]).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(read))
            .unwrap();
        path
    }

    #[test]
    fn cargo_units_group_hashes_outputs_and_scripts_without_following_symlinks() {
        let scratch = ScratchDir::new("cargo-unit-gc");
        let target = scratch.join("target");
        let now = SystemTime::now();
        let old = now - STALE_AGE - Duration::from_secs(3600);
        let a = "0123456789abcdef";
        let b = "fedcba9876543210";
        let mut expected = vec![
            output(&target, &format!("debug/deps/libmy_crate-{a}.rlib"), old),
            output(&target, &format!("debug/deps/libmy_crate-{a}.rmeta"), old),
            output(&target, &format!("debug/deps/my_crate-{a}.d"), old),
        ];
        output(
            &target,
            &format!("debug/.fingerprint/package-other-{a}/lib-my_crate"),
            old,
        );
        expected.push(target.join(format!("debug/.fingerprint/package-other-{a}")));
        output(
            &target,
            &format!("debug/build/package-other-{a}/out/generated.rs"),
            old,
        );
        expected.push(target.join(format!("debug/build/package-other-{a}")));
        // A recent output retains every member with the same hash, including
        // a fingerprint directory whose package name differs from the target.
        output(&target, &format!("debug/deps/libmy_crate-{b}.rlib"), old);
        let recent = output(&target, &format!("debug/deps/my_crate-{b}.d"), now);
        output(
            &target,
            &format!("debug/.fingerprint/different-package-{b}/bin-my_crate"),
            old,
        );
        let binary = output(&target, &format!("release/deps/tool-{a}"), old);
        output(
            &target,
            &format!("release/build/script-{b}/build-script-build"),
            old,
        );
        let script = target.join(format!("release/build/script-{b}"));
        let unrelated = output(&target, "debug/deps/do-not-guess.o", old);
        let unknown = output(&target, "debug/deps/custom-2222222222222222.unknown", old);
        output(
            &target,
            "debug/.fingerprint/custom-2222222222222222/lib-custom",
            old,
        );
        let import_library = output(&target, "release/deps/custom-3333333333333333.dll.lib", old);
        let outside = output(&scratch, "outside", old);
        let link = target.join("debug/deps/linked-1111111111111111");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let old_incremental = target.join("debug/incremental/my_crate-session");
        output(&old_incremental, "state/query-cache.bin", old);
        let hot_incremental = output(&target, "debug/incremental/hot-session/state.bin", now);
        let device = crate::filesystem::device(&target).unwrap();
        let units = stale_units(&target, now, device).unwrap();
        assert_eq!(units.len(), 5);
        expected.sort();
        assert!(units.iter().any(|unit| {
            let mut unit = unit.clone();
            unit.sort();
            unit == expected
        }));
        assert!(units.contains(&vec![binary]));
        assert!(units.contains(&vec![script]));
        assert!(units.contains(&vec![old_incremental]));
        assert!(units.contains(&vec![import_library]));
        let selected: Vec<_> = units.into_iter().flatten().collect();
        for path in [recent, unrelated, unknown, outside, link, hot_incremental] {
            assert!(
                !selected.contains(&path),
                "unexpected candidate: {}",
                path.display()
            );
        }
    }
}
