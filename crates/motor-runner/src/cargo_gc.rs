//! Conservative Cargo output discovery. Deletion is performed by disk reclaim,
//! after it has excluded workspace users and restored access to job-owned files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) const STALE_AGE: Duration = Duration::from_secs(48 * 60 * 60);

#[cfg(test)]
#[path = "../tests/fixtures/cargo-layout-v2/fixture.rs"]
pub(crate) mod fixture;

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

fn hex_component(path: &Path, length: usize) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    (name.len() == length && name.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| name.to_ascii_lowercase())
}

fn stale_profiles(target: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let mut found = profiles(target)?;
    // The runner's per-checkout build roots are not ordinary Cargo profiles.
    // Descend only the two exact hash components, never arbitrary job trees.
    for prefix in directories(&target.join("bd"))? {
        if hex_component(&prefix, 2).is_none() {
            continue;
        }
        for build in directories(&prefix)? {
            if hex_component(&build, 14).is_some() {
                found.extend(profiles(&build)?);
            }
        }
    }
    Ok(found)
}

pub(crate) fn incremental(target: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(stale_profiles(target)?
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
/// Empty directories use mtime: Cargo creating or emptying them is recent work,
/// but a maintenance scan must not keep an empty output directory warm forever.
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
    Ok(Some(newest.unwrap_or(metadata.modified()?)))
}

fn v2_units(
    profile: &Path,
    units: &mut BTreeMap<String, Vec<PathBuf>>,
    unknown: &mut BTreeSet<String>,
) -> io::Result<()> {
    for package in directories(&profile.join("build"))? {
        let Some(name) = package.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // A name-hash entry already belongs to the v1 grouping. Keeping it as
        // one tree also safely handles a v2 package with that unusual name.
        if unit_key(name).is_some()
            || name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            continue;
        }
        for unit in directories(&package)? {
            let Some(hash) = hex_component(&unit, 16) else {
                continue;
            };
            let children = directories(&unit)?;
            if !children
                .iter()
                .any(|path| path.file_name().is_some_and(|n| n == "out"))
                || children.iter().any(|path| {
                    !path
                        .file_name()
                        .is_some_and(|n| n == "out" || n == "fingerprint" || n == "run")
                        || !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
                })
            {
                unknown.insert(hash);
                continue;
            }
            // Fingerprints, outputs and build-script state are one indivisible
            // build step in Cargo v2. Union hashes with v1 and other packages so
            // any recent member retains the complete unit in a mixed tree.
            units.entry(hash).or_default().push(unit);
        }
    }
    Ok(())
}

/// Each vector is indivisible: a recent member retains every output of a unit.
/// Unknown names and symlinks are never returned as deletion candidates.
pub(crate) fn stale_units(
    target: &Path,
    now: SystemTime,
    device: u64,
) -> io::Result<Vec<Vec<PathBuf>>> {
    let mut stale = Vec::new();
    for profile in stale_profiles(target)? {
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
        v2_units(&profile, &mut units, &mut unknown)?;
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

    fn age_tree(path: &Path, read: SystemTime) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                age_tree(&path, read);
            } else {
                fs::File::open(path)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_accessed(read))
                    .unwrap();
            }
        }
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(read).set_modified(read))
            .unwrap();
    }

    #[test]
    fn captured_v2_units_are_indivisible_and_recent_members_keep_live_outputs() {
        let scratch = ScratchDir::new("cargo-v2-unit-gc");
        let target = scratch.join("target");
        fixture::materialize(&target, fixture::V2);
        let now = SystemTime::now();
        let old = now - STALE_AGE - Duration::from_secs(3600);
        age_tree(&target, old);
        let hot = "debug/build/layout-app/210fc4115dcd38ca";
        output(
            &target,
            &format!("{hot}/fingerprint/invoked.timestamp"),
            now,
        );
        // The same hash under another package, and even a v1 name, must remain
        // live when the v2 fingerprint has been read recently.
        output(
            &target,
            "debug/build/another/210fc4115dcd38ca/out/libanother.rlib",
            old,
        );
        output(&target, "debug/deps/renamed-210fc4115dcd38ca.d", old);
        let unknown = output(
            &target,
            "debug/build/unknown/1111111111111111/mystery/data",
            old,
        );
        let unknown_hash = output(&target, "debug/build/unknown/not-a-hash/out/data", old);
        let unknown_package = output(
            &target,
            "debug/build/bad.name/2222222222222222/out/data",
            old,
        );
        let outside = output(&scratch, "outside", old);
        let linked = target.join("debug/build/linked/3333333333333333");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(
            target.join("debug/build/layout-app/42cfe3b998a78e33"),
            &linked,
        )
        .unwrap();
        let nested_link = target.join("debug/build/nested/4444444444444444/out/link");
        fs::create_dir_all(nested_link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &nested_link).unwrap();
        let device = crate::filesystem::device(&target).unwrap();
        let units = stale_units(&target, now, device).unwrap();
        let cold = target.join("debug/build/layout-app/42cfe3b998a78e33");
        let script = target.join("debug/build/layout-dep/a0a8c0feb630c617");
        assert!(units.contains(&vec![cold]));
        assert!(units.contains(&vec![script]));
        for candidate in units.iter().flatten() {
            assert_eq!(
                candidate.parent().unwrap().parent().unwrap(),
                target.join("debug/build")
            );
            for kept in [
                &target.join(hot),
                &unknown,
                &unknown_hash,
                &unknown_package,
                &linked,
                &nested_link,
                &outside,
            ] {
                assert!(
                    !kept.starts_with(candidate),
                    "candidate includes kept member: {candidate:?}"
                );
            }
        }
    }

    #[test]
    fn captured_layouts_are_swept_inside_only_physical_workspace_hash_roots() {
        let scratch = ScratchDir::new("cargo-build-root-gc");
        let target = scratch.join("target");
        let now = SystemTime::now();
        let old = now - STALE_AGE - Duration::from_secs(3600);
        for (hash, tree) in [
            ("00000000000001", fixture::V1),
            ("00000000000002", fixture::V2),
        ] {
            let build = target.join("bd/ab").join(hash);
            // Exercise the target-triple level as well as the profile level.
            fixture::materialize(&build.join("x86_64-unknown-linux-gnu"), tree);
            age_tree(&build, old);
        }
        let live = target.join("bd/ab/00000000000003");
        fixture::materialize(&live, fixture::V2);
        let unknown = target.join("bd/ab/not-a-workspace");
        fixture::materialize(&unknown, fixture::V2);
        age_tree(&unknown, old);
        let outside = scratch.join("outside");
        fixture::materialize(&outside, fixture::V2);
        age_tree(&outside, old);
        std::os::unix::fs::symlink(&outside, target.join("bd/ab/00000000000004")).unwrap();
        std::os::unix::fs::symlink(target.join("bd/ab"), target.join("bd/cd")).unwrap();
        let device = crate::filesystem::device(&target).unwrap();
        let units = stale_units(&target, now, device).unwrap();
        for hash in ["00000000000001", "00000000000002"] {
            assert!(units
                .iter()
                .flatten()
                .any(|path| path.starts_with(target.join("bd/ab").join(hash))));
        }
        for path in units.into_iter().flatten() {
            assert!(!path.starts_with(&live));
            assert!(!path.starts_with(&unknown));
            assert!(!path.starts_with(&outside));
            assert!(!path.starts_with(target.join("bd/ab/00000000000004")));
            assert!(!path.starts_with(target.join("bd/cd")));
        }
        assert!(fixture::files(&live).iter().all(|path| path.is_file()));
    }

    #[test]
    fn v2_empty_out_stays_stale_across_both_deletion_scans() {
        let scratch = ScratchDir::new("cargo-v2-two-scans");
        let target = scratch.join("target");
        fixture::materialize(&target, fixture::V2);
        let now = SystemTime::now();
        let old = now - STALE_AGE - Duration::from_secs(3600);
        age_tree(&target, old);
        let script = target.join("debug/build/layout-dep/a0a8c0feb630c617");
        assert!(fs::read_dir(script.join("out")).unwrap().next().is_none());
        // Leave atime old for the first scan. The second scan must still select
        // the unit after maintenance has read its otherwise-empty output tree.
        fs::File::open(script.join("out"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_accessed(old))
            .unwrap();
        let device = crate::filesystem::device(&target).unwrap();
        assert!(stale_units(&target, now, device)
            .unwrap()
            .contains(&vec![script.clone()]));
        assert!(stale_units(&target, now, device)
            .unwrap()
            .contains(&vec![script.clone()]));
        // Cargo emptying or recreating out/ is real recent work, even when no
        // files remain; that modification must retain the complete unit.
        fs::File::open(script.join("out"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now))
            .unwrap();
        assert!(!stale_units(&target, now, device)
            .unwrap()
            .iter()
            .flatten()
            .any(|path| path == &script));
    }

    #[test]
    fn incremental_visits_both_layouts_only_inside_physical_workspace_hash_roots() {
        let scratch = ScratchDir::new("cargo-incremental-build-roots");
        let target = scratch.join("target");
        let now = SystemTime::now();
        let mut expected = Vec::new();
        for (hash, tree, prefix) in [
            ("00000000000001", fixture::V1, ""),
            ("00000000000002", fixture::V2, "x86_64-unknown-linux-gnu"),
        ] {
            let build = target.join("bd/ab").join(hash).join(prefix);
            fixture::materialize(&build, tree);
            output(&build, "debug/incremental/local-session/state.bin", now);
            expected.push(build.join("debug/incremental"));
        }
        output(&target, "release/incremental/legacy-session/state.bin", now);
        expected.push(target.join("release/incremental"));
        for root in [
            "bd/a/00000000000001",
            "bd/abc/00000000000001",
            "bd/gh/00000000000001",
            "bd/ab/0000000000001",
            "bd/ab/000000000000001",
            "bd/ab/not-a-workspace",
        ] {
            output(&target.join(root), "debug/incremental/keep/state.bin", now);
        }
        let outside = scratch.join("outside");
        output(&outside, "debug/incremental/keep/state.bin", now);
        std::os::unix::fs::symlink(&outside, target.join("bd/ab/00000000000003")).unwrap();
        std::os::unix::fs::symlink(target.join("bd/ab"), target.join("bd/cd")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("debug"),
            target.join("bd/ab/00000000000001/custom"),
        )
        .unwrap();
        fs::create_dir_all(target.join("bd/ab/00000000000004/debug")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("debug/incremental"),
            target.join("bd/ab/00000000000004/debug/incremental"),
        )
        .unwrap();
        let mut actual = incremental(&target).unwrap();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        let linked_target = scratch.join("linked-target");
        std::os::unix::fs::symlink(&target, &linked_target).unwrap();
        assert!(incremental(&linked_target).unwrap().is_empty());
        assert!(outside.join("debug/incremental/keep/state.bin").is_file());
    }
}
